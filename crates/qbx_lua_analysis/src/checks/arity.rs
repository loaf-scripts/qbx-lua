use std::sync::Arc;

use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{SmolStr, Span};
use qbx_luacats::luacats::{applies_on, parse_doc_lines};
use qbx_luacats::types::{FunType, Type};
use rustc_hash::FxHashMap;

use super::crossfile::{passed_count, plural};
use super::{FileInput, Sink};
use crate::env::leading_doc_lines;
use crate::project::ResourceEnv;
use crate::rules;
use crate::scope::{LocalId, LocalKind, Resolved};
use crate::side_guard::SideRegions;
use crate::signature::{defined, global_key, member_path, most_arguments, undocumented, Requirement};

/// Every value assigned to a local or to a field of a local table, by local and dotted field path.
type LocalDefs = FxHashMap<(LocalId, SmolStr), Vec<Option<Arc<FunType>>>>;

/// The function type that a `---@type` gives a local or a field of a local table, by local and
/// dotted field path. It holds whatever value is assigned, as in LuaLS.
type DeclaredDefs = FxHashMap<(LocalId, SmolStr), Arc<FunType>>;

const MAX_ALIAS_DEPTH: u8 = 8;

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    if sink.enabled(rules::MISSING_PARAMETER).is_none() && sink.enabled(rules::REDUNDANT_PARAMETER).is_none() {
        return;
    }
    let callees = Callees::new(input);
    Calls { callees: &callees, sink }.visit_block(&input.chunk.block);
}

/// The functions that the calls of a file may run, as far as the file and the summaries of its
/// resource tell: the documented global functions, and the local functions and fields of local
/// tables of the file.
pub(super) struct Callees<'i, 'a> {
    input: &'i FileInput<'a>,
    /// The file's own summary, for a file outside any resource.
    own_env: ResourceEnv,
    locals: LocalDefs,
    declared: DeclaredDefs,
    regions: SideRegions,
}

impl<'i, 'a> Callees<'i, 'a> {
    pub(super) fn new(input: &'i FileInput<'a>) -> Self {
        let mut own_env = ResourceEnv::default();
        if input.resource.is_none() {
            own_env.add_summary(input.summary, None);
        }
        let env = input.resource.map_or(&own_env, |resource| resource.env);
        let mut collector =
            LocalCollector { input, env, defs: LocalDefs::default(), declared: DeclaredDefs::default() };
        collector.visit_block(&input.chunk.block);
        let (locals, declared) = (collector.defs, collector.declared);
        Self { input, own_env, locals, declared, regions: SideRegions::of(input.source, input.chunk) }
    }

    fn env(&self) -> &ResourceEnv {
        self.input.resource.map_or(&self.own_env, |resource| resource.env)
    }

    /// The signatures a call of `root.fields` may run: every definition, and its overloads unless
    /// they are scoped to the other side. `None` when the call may run a function the file does not
    /// know, such as one held by a parameter.
    fn signatures(&self, call: &Expr, root: &Name, fields: &[&str]) -> Option<Vec<Arc<FunType>>> {
        let side = self.regions.effective(call.span.start, self.input.side);
        let defs: Vec<Option<Arc<FunType>>> = match self.input.resolution.resolve_at(root.span.start)? {
            Resolved::Local(id) => {
                // Parameters and loop variables start out with a value this file does not know.
                let kind = self.input.resolution.local(id).kind;
                if !matches!(kind, LocalKind::Local | LocalKind::LocalFunction) {
                    return None;
                }
                let key = (id, SmolStr::new(fields.join(".")));
                match (self.declared.get(&key), self.locals.get(&key)) {
                    (Some(declared), _) => vec![Some(declared.clone())],
                    (None, Some(defs)) => defs.clone(),
                    (None, None) => return None,
                }
            }
            // An encrypted script of the resource may define the global differently.
            Resolved::Global(_) if self.env().opaque => return None,
            Resolved::Global(_) => {
                let path = global_key(&root.text, fields)?;
                let mut defs: Vec<_> = self.env().function_defs(&path, side).map(|def| def.cloned()).collect();
                // What `---@extend` lines add counts beside the definitions, so a native or a
                // function the resource does not define keeps the signatures it has.
                if !defs.is_empty() {
                    defs.extend(self.env().extension_defs(&path, side).map(|fun| Some(fun.clone())));
                }
                defs
            }
        };
        // A value other than a function literal may be any function.
        let defs = defs.into_iter().collect::<Option<Vec<_>>>()?;
        let signatures: Vec<Arc<FunType>> = defs
            .iter()
            .flat_map(|fun| {
                let overloads = fun.overloads.iter().filter(|overload| applies_on(overload.side, side));
                std::iter::once(fun).chain(overloads).cloned()
            })
            .collect();
        (!signatures.is_empty()).then_some(signatures)
    }

    /// The function types that the functions `call` may run declare for its argument at
    /// `position`, such as the `fun(source: number, ...)` of a callback wrapper.
    pub(super) fn argument_functions(&self, call: &Expr, position: usize) -> Vec<Arc<FunType>> {
        let callee = match &call.kind {
            ExprKind::Call { callee, .. } => member_path(callee).map(|(root, fields)| (root, fields, false)),
            ExprKind::MethodCall { base, method, .. } => member_path(base).map(|(root, mut fields)| {
                fields.push(&method.text);
                (root, fields, true)
            }),
            _ => None,
        };
        let Some((root, fields, via_colon)) = callee else { return Vec::new() };
        let Some(signatures) = self.signatures(call, root, &fields) else { return Vec::new() };
        let alias = |name: &str| self.env().alias(name);
        signatures
            .iter()
            .filter_map(|fun| {
                // Positions count as `Requirement::of` counts them.
                let index = (position + usize::from(via_colon)).checked_sub(usize::from(fun.is_method))?;
                let param = fun.params.get(index).or_else(|| fun.params.last().filter(|p| p.name == "..."))?;
                function_type(&param.ty, &alias, 0)
            })
            .collect()
    }
}

struct LocalCollector<'i, 'a> {
    input: &'i FileInput<'a>,
    env: &'i ResourceEnv,
    defs: LocalDefs,
    declared: DeclaredDefs,
}

impl LocalCollector<'_, '_> {
    fn local(&self, name: &Name) -> Option<LocalId> {
        match self.input.resolution.resolve_at(name.span.start)? {
            Resolved::Local(id) => Some(id),
            Resolved::Global(_) => None,
        }
    }

    fn add(&mut self, id: LocalId, fields: &[&str], signature: Option<Arc<FunType>>) {
        self.defs.entry((id, SmolStr::new(fields.join(".")))).or_default().push(signature);
    }

    /// Records the function type that the `---@type` above `stmt` gives its name at `index`, when
    /// it gives one.
    fn declare(&mut self, stmt: &Stmt, index: usize, id: LocalId, fields: &[&str]) {
        let lines = leading_doc_lines(self.input.source, &self.input.chunk.comments, stmt.span.start);
        if !lines.iter().any(|line| line.contains("@type")) {
            return;
        }
        let doc = parse_doc_lines(&lines);
        let alias = |name: &str| self.env.alias(name);
        if let Some(fun) = doc.type_at(index).and_then(|ty| function_type(ty, &alias, 0)) {
            self.declared.insert((id, SmolStr::new(fields.join("."))), fun);
        }
    }

    /// The signature of a function `value`, and `None` for other values, which may be any function.
    fn signature(&self, stmt: &Stmt, value: Option<&Expr>, single: bool) -> Option<Arc<FunType>> {
        match value.map(|e| &e.kind) {
            // A doc comment above `local a, b = ...` does not say which value it describes.
            Some(ExprKind::Function(func)) if single => {
                Some(defined(self.input.source, &self.input.chunk.comments, stmt.span.start, func, false))
            }
            Some(ExprKind::Function(func)) => Some(undocumented(func, false)),
            _ => None,
        }
    }
}

impl<'ast> Visitor<'ast> for LocalCollector<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::LocalFunction { name, func } => {
                if let Some(id) = self.local(name) {
                    let signature =
                        Some(defined(self.input.source, &self.input.chunk.comments, stmt.span.start, func, false));
                    self.add(id, &[], signature);
                }
            }
            StmtKind::Local { names, exprs, .. } => {
                for (index, name) in names.iter().enumerate() {
                    let Some(id) = self.local(&name.name) else { continue };
                    self.declare(stmt, index, id, &[]);
                    // `local f` holds nil until it is assigned, which the assignment records.
                    if index >= exprs.len() && !exprs.last().is_some_and(Expr::is_multi_value) {
                        continue;
                    }
                    let signature = self.signature(stmt, exprs.get(index), names.len() == 1);
                    self.add(id, &[], signature);
                }
            }
            StmtKind::Function { name, func } => {
                if let Some(id) = self.local(&name.base) {
                    let fields: Vec<&str> = name.path.iter().chain(&name.method).map(|n| n.text.as_str()).collect();
                    let signature = Some(defined(
                        self.input.source,
                        &self.input.chunk.comments,
                        stmt.span.start,
                        func,
                        name.method.is_some(),
                    ));
                    self.add(id, &fields, signature);
                }
            }
            StmtKind::Assign { targets, exprs } => {
                for (index, target) in targets.iter().enumerate() {
                    let Some((root, fields)) = member_path(target) else { continue };
                    if let Some(id) = self.local(root) {
                        self.declare(stmt, index, id, &fields);
                        let signature = self.signature(stmt, exprs.get(index), targets.len() == 1);
                        self.add(id, &fields, signature);
                    }
                }
            }
            StmtKind::CompoundAssign { target, .. } => {
                if let Some((root, fields)) = member_path(target) {
                    if let Some(id) = self.local(root) {
                        self.add(id, &fields, None);
                    }
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }
}

struct Calls<'c, 'i, 'a, 's> {
    callees: &'c Callees<'i, 'a>,
    sink: &'c mut Sink<'s>,
}

impl<'ast> Visitor<'ast> for Calls<'_, '_, '_, '_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Call { callee, args, .. } => {
                if let (Some((root, fields)), Some(display)) = (member_path(callee), callee.dotted_path()) {
                    let span = match &callee.kind {
                        ExprKind::Field { name, .. } => name.span,
                        ExprKind::Index { index, .. } => index.span,
                        _ => callee.span,
                    };
                    self.check_call(expr, root, &fields, args, false, span, &display);
                }
            }
            ExprKind::MethodCall { base, method, args, .. } => {
                if let (Some((root, mut fields)), Some(owner)) = (member_path(base), base.dotted_path()) {
                    fields.push(&method.text);
                    self.check_call(expr, root, &fields, args, true, method.span, &format!("{owner}:{}", method.text));
                }
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

impl Calls<'_, '_, '_, '_> {
    #[allow(clippy::too_many_arguments)]
    fn check_call(
        &mut self,
        call: &Expr,
        root: &Name,
        fields: &[&str],
        args: &[Expr],
        via_colon: bool,
        span: Span,
        display: &str,
    ) {
        // Any definition or overload may be the one that runs.
        let Some(signatures) = self.callees.signatures(call, root, fields) else { return };
        let signatures: Vec<&FunType> = signatures.iter().map(|fun| fun.as_ref()).collect();
        self.missing(&signatures, args, via_colon, span, display);
        self.redundant(&signatures, args, via_colon, display);
    }

    /// `missing-parameter`, for the signature that needs the fewest arguments. A call at the end
    /// passes as many values as it gives, which only the language server tells.
    fn missing(&mut self, signatures: &[&FunType], args: &[Expr], via_colon: bool, span: Span, display: &str) {
        let given = |last: &Expr| match last.is_call() {
            true => self.callees.input.value_count.and_then(|count| count(last)),
            false => None,
        };
        let passed = passed_count(args, 0).or_else(|| Some(args.len() - 1 + given(args.last()?)?));
        let Some(passed) = passed else { return };
        let env = self.callees.env();
        let alias = |name: &str| env.alias(name);
        let least = signatures
            .iter()
            .map(|fun| Requirement::of(fun, via_colon, &alias))
            .min_by_key(|requirement| requirement.arguments);
        let Some(least) = least.filter(|least| passed < least.arguments) else { return };
        let missing = match least.first_missing(passed) {
            Some(param) => format!("'{}' ({})", param.name, param.ty),
            None => "'self' (call it with ':')".to_string(),
        };
        self.sink.report(
            rules::MISSING_PARAMETER,
            span,
            format!(
                "'{display}' is called with {passed} argument{}, but needs {}; {missing} will be nil",
                plural(passed),
                least.arguments
            ),
        );
    }

    /// `redundant-parameter`, for the signature that takes the most arguments, at the arguments
    /// after them. A call or `...` among those counts as one argument.
    fn redundant(&mut self, signatures: &[&FunType], args: &[Expr], via_colon: bool, display: &str) {
        let mut most = 0;
        for fun in signatures {
            let Some(count) = most_arguments(fun, via_colon) else { return };
            most = most.max(count);
        }
        let (Some(first), Some(last)) = (args.get(most), args.last()) else { return };
        let takes = match most {
            0 => "takes none".to_string(),
            most => format!("takes at most {most}"),
        };
        self.sink.report(
            rules::REDUNDANT_PARAMETER,
            first.span.to(last.span),
            format!("'{display}' is called with {} argument{}, but {takes}", args.len(), plural(args.len())),
        );
    }
}

/// The function type `ty` names, through aliases and the `nil` of `fun()?`. A union of several
/// function types names none.
fn function_type<'t>(ty: &Type, alias: &impl Fn(&str) -> Option<&'t Type>, depth: u8) -> Option<Arc<FunType>> {
    match ty {
        Type::Fun(fun) => Some(fun.clone()),
        Type::Union(types) => match types.iter().filter(|t| !matches!(t, Type::Nil)).collect::<Vec<_>>()[..] {
            [only] => function_type(only, alias, depth),
            _ => None,
        },
        Type::Named(name, args) if args.is_empty() && depth < MAX_ALIAS_DEPTH => {
            function_type(alias(name)?, alias, depth + 1)
        }
        _ => None,
    }
}
