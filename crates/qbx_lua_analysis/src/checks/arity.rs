use std::sync::Arc;

use qbx_fivem_data::{native, ArgKind, InvokeParam, Native, ParamKind, Side, WrapperParam};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{NumberValue, SmolStr, Span};
use qbx_luacats::luacats::{applies_on, parse_doc_lines};
use qbx_luacats::types::{FunType, Type};
use rustc_hash::FxHashMap;

use super::crossfile::{passed_count, plural};
use super::{FileInput, Sink};
use crate::diagnostic::{Fix, TextEdit};
use crate::env::{builtins, leading_doc_lines};
use crate::invoke;
use crate::project::ResourceEnv;
use crate::rules;
use crate::scope::{LocalId, LocalKind, Resolved};
use crate::side_guard::SideRegions;
use crate::signature::{
    defined, global_key, member_path, most_arguments, native_signature, runtime_definition, undocumented, Requirement,
};

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
                // What the runtime defines counts beside the definitions, so a native or a runtime
                // function keeps the signatures it has, and alone where the workspace defines nothing.
                if defs.is_empty() {
                    defs = self.runtime_defs(&path, side);
                } else {
                    defs.extend(runtime_definition(&path, side, self.input.config.strict()));
                }
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

    /// The signatures of the runtime function at `path` that the workspace does not define: those
    /// its stub declares, such as the `n` that `string.rep` requires, or else those of the native of
    /// that name, whose optional parameters a call may leave out, fewer with `strict`.
    fn runtime_defs(&self, path: &str, side: Option<Side>) -> Vec<Option<Arc<FunType>>> {
        let stubs: Vec<_> = builtins().functions(path, side).map(|fun| Some(fun.clone())).collect();
        if !stubs.is_empty() {
            return stubs;
        }
        self.runtime_native(path, side).map(|found| vec![Some(native_signature(found))]).unwrap_or_default()
    }

    /// The native that a call of the global at `path` runs, when no stub declares a runtime function
    /// of that name, read as the file's `strict` reads it.
    fn runtime_native(&self, path: &str, side: Option<Side>) -> Option<Native> {
        if builtins().functions(path, side).next().is_some() {
            return None;
        }
        let available = |found: &Native| found.side.is_available_on(side.unwrap_or(Side::Shared));
        native(path).filter(available).map(|found| found.on(side).strictly(self.input.config.strict()))
    }

    /// The native whose signature `signatures` gives a call of `root.fields`: a global that the
    /// workspace does not define.
    fn native_called(&self, call: &Expr, root: &Name, fields: &[&str]) -> Option<Native> {
        if !matches!(self.input.resolution.resolve_at(root.span.start)?, Resolved::Global(_)) || self.env().opaque {
            return None;
        }
        let side = self.regions.effective(call.span.start, self.input.side);
        let path = global_key(&root.text, fields)?;
        if self.env().function_defs(&path, side).next().is_some() {
            return None;
        }
        self.runtime_native(&path, side)
    }

    /// The native that a call of `callee`, a function of `invoke::INVOKERS` whose root is `root`, with
    /// `args` invokes by its hash, read as the file's `strict` reads it.
    fn invoked_native(&self, callee: &Expr, root: &Name, args: &[Expr]) -> Option<Native> {
        if !matches!(self.input.resolution.resolve_at(root.span.start)?, Resolved::Global(_)) {
            return None;
        }
        invoke::invoked_native(callee, args).map(|native| native.strictly(self.input.config.strict()))
    }

    /// What `arg`, passed to a native, is: as the types of the language server tell, or else as its
    /// syntax shows.
    fn arg_kind(&self, arg: &Expr) -> ArgKind {
        match self.input.native_arg {
            Some(kind) => kind(arg),
            None => written_kind(self.input, arg),
        }
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
        if let ExprKind::Call { callee, .. } = &call.kind {
            if let Some(native) = self.callees.invoked_native(callee, root, args) {
                return self.invoked(native, args, span, display);
            }
        }
        // Any definition or overload may be the one that runs.
        let Some(signatures) = self.callees.signatures(call, root, fields) else { return };
        let signatures: Vec<&FunType> = signatures.iter().map(|fun| fun.as_ref()).collect();
        let native = self.callees.native_called(call, root, fields);
        self.missing(&signatures, args, via_colon, span, display, native);
        self.redundant(&signatures, call, native, via_colon, display);
    }

    /// `missing-parameter`, for the signature that needs the fewest arguments. A call at the end
    /// passes as many values as it gives, which only the language server tells. A vector passed to
    /// a `native` fills a parameter for each of its parts where the runtime spreads it, and one
    /// with `strict`.
    fn missing(
        &mut self,
        signatures: &[&FunType],
        args: &[Expr],
        via_colon: bool,
        span: Span,
        display: &str,
        native: Option<Native>,
    ) {
        let given = |last: &Expr| match last.is_call() {
            true => self.callees.input.value_count.and_then(|count| count(last)),
            false => None,
        };
        let passed = passed_count(args, 0).or_else(|| Some(args.len() - 1 + given(args.last()?)?));
        let Some(passed) = passed else { return };
        let (filled, most) = match native.filter(|_| passed == args.len()) {
            Some(native) => self.native_filled(native, args),
            None => (passed, passed),
        };
        let env = self.callees.env();
        let alias = |name: &str| env.alias(name);
        let least = signatures
            .iter()
            .map(|fun| Requirement::of(fun, via_colon, &alias))
            .min_by_key(|requirement| requirement.arguments);
        let Some(least) = least.filter(|least| most < least.arguments) else { return };
        let missing = match least.first_missing(filled) {
            Some(param) => format!("'{}' ({})", param.name, param.ty),
            None => "'self' (call it with ':')".to_string(),
        };
        self.sink.report(
            rules::MISSING_PARAMETER,
            span,
            match filled == passed {
                true => format!(
                    "'{display}' is called with {passed} argument{}, but needs {}; {missing} will be nil",
                    plural(passed),
                    least.arguments
                ),
                false => format!(
                    "'{display}' is called with {passed} argument{}, which fill {filled} parameters, but needs {}; \
                     {missing} will be nil",
                    plural(passed),
                    least.arguments
                ),
            },
        );
    }

    /// How many parameters of `native` `args` fill, and how many they may fill at most: the runtime
    /// spreads a vector over a parameter for each of its parts, unless the fxv2 natives read the
    /// arguments or `strict` reads them as the fxv2 natives do, and an argument of no known kind for
    /// a number parameter may be a vector of up to four parts.
    fn native_filled(&self, native: Native, args: &[Expr]) -> (usize, usize) {
        let fxv2 = self.callees.input.resource.is_some_and(|resource| resource.fxv2_natives);
        if fxv2 || self.callees.input.config.strict() {
            return (args.len(), args.len());
        }
        let kinds: Vec<ArgKind> = args.iter().map(|arg| self.callees.arg_kind(arg)).collect();
        let spread = native.spread(&kinds);
        let taken: Vec<WrapperParam> =
            native.wrapper_params().filter(|param| param.kind != ParamKind::Filled).collect();
        let widest: Vec<ArgKind> = kinds
            .iter()
            .zip(&spread.targets)
            .map(|(kind, &target)| match kind {
                ArgKind::Unknown if taken.get(target).is_some_and(WrapperParam::takes_parts) => ArgKind::Vector(4),
                kind => *kind,
            })
            .collect();
        (spread.filled, native.spread(&widest).filled)
    }

    /// `missing-parameter` and `redundant-parameter` for a call of `Citizen.InvokeNative` that
    /// invokes `native`, whose arguments after the hash fill the parameters that
    /// `Native::invoke_params` lists: a result marker fills none, and a vector one for each of its
    /// parts, as `Citizen.InvokeNative` spreads it whatever the manifest and `strict` say. A value of
    /// no known kind for a number parameter may be a vector of up to four parts.
    fn invoked(&mut self, native: Native, args: &[Expr], span: Span, display: &str) {
        let params: Vec<InvokeParam> = native.invoke_params().collect();
        // A table or a function takes a form that the Lua wrapper would make of it.
        if params.iter().any(|param| matches!(param.kind, ParamKind::Object | ParamKind::Function)) {
            return;
        }
        let passed = args.get(1..).unwrap_or_default();
        let (mut filled, mut most, mut open, mut beyond) = (0, 0, false, None);
        for (i, arg) in passed.iter().enumerate() {
            let (least, widest) = match invoke::marker(arg) {
                Some(marker) => (marker.slots(), marker.slots()),
                None if i + 1 == passed.len() && arg.is_multi_value() => {
                    let count = arg.is_call().then(|| self.callees.input.value_count.and_then(|count| count(arg)));
                    let count = count.flatten();
                    open = count.is_none();
                    (count.unwrap_or(1), count.unwrap_or(1))
                }
                None => match self.callees.arg_kind(arg) {
                    ArgKind::Vector(parts) => (usize::from(parts), usize::from(parts)),
                    ArgKind::Unknown if params.get(filled).is_some_and(|param| param.ty == "number") => (1, 4),
                    _ => (1, 1),
                },
            };
            if least > 0 && filled >= params.len() && beyond.is_none() {
                beyond = Some(i);
            }
            filled += least;
            most += widest;
        }
        let name = native.name;
        let required = params.iter().rposition(|param| !param.optional).map_or(0, |last| last + 1);
        if let Some(param) = params[filled.min(required)..required].iter().find(|param| !param.optional) {
            if !open && most < required {
                let unset = if param.is_pointer() { "a null pointer" } else { "0" };
                self.sink.report(
                    rules::MISSING_PARAMETER,
                    span,
                    format!(
                        "'{display}' passes {filled} value{} to '{name}', which needs {required}; '{}' ({}) will be {unset}",
                        plural(filled),
                        param.name,
                        param.ty
                    ),
                );
            }
        }
        // Pointer markers after a last parameter that is a pointer fill the struct it points to, as
        // FiveM keeps the values of pointer markers next to each other: `GetPedHeadBlendData` takes
        // one for each field.
        let fields = params.last().is_some_and(InvokeParam::is_pointer)
            && beyond.is_some_and(|i| passed[i..].iter().all(|arg| invoke::marker(arg).is_some()));
        if let (Some(first), Some(last), false) = (beyond.map(|i| &passed[i]), passed.last(), fields) {
            self.sink.report(
                rules::REDUNDANT_PARAMETER,
                first.span.to(last.span),
                format!(
                    "'{display}' passes {filled} value{} to '{name}', which takes at most {}",
                    plural(filled),
                    params.len()
                ),
            );
        }
    }

    /// `redundant-parameter`, for the signature that takes the most arguments, at the arguments
    /// after them. A call or `...` among those counts as one argument.
    /// For a call of a `native`, the arguments after them may go to the parameters that only the
    /// game native takes, which the Lua wrapper drops but `Citizen.InvokeNative` passes, and the
    /// call is offered as a call of it.
    fn redundant(
        &mut self,
        signatures: &[&FunType],
        call: &Expr,
        native: Option<Native>,
        via_colon: bool,
        display: &str,
    ) {
        let (ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. }) = &call.kind else { return };
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
        let mut message =
            format!("'{display}' is called with {} argument{}, but {takes}", args.len(), plural(args.len()));
        let extras: Vec<InvokeParam> = native.iter().flat_map(Native::extra_params).collect();
        let passed = extras.get(..args.len() - most).filter(|passed| !passed.is_empty());
        let fix = match (native, passed) {
            (Some(native), Some(passed)) => {
                let named: Vec<String> =
                    passed.iter().map(|param| format!("`{}` ({})", param.name, param.ty)).collect();
                message.push_str(&format!(
                    ": FiveM's Lua function drops the rest, though the game native also takes {}, which \
                     `Citizen.InvokeNative` passes",
                    listed(&named)
                ));
                self.invoke_fix(call, native, passed)
            }
            _ => None,
        };
        self.sink.report_with(rules::REDUNDANT_PARAMETER, first.span.to(last.span), message, None, fix);
    }

    /// The fix that calls `native` through `Citizen.InvokeNative`, which passes its arguments to the
    /// `extras` that only the game native takes as well, and returns what the Lua wrapper returns,
    /// as the result marker the wrapper passes tells: none for a `BOOL`, which the default gives.
    /// The wrapper converts no argument of such a call: it fills no pointer, and hashes no string
    /// for a `Hash` and turns no other value into text for a `string`.
    fn invoke_fix(&self, call: &Expr, native: Native, extras: &[InvokeParam]) -> Option<Fix> {
        let ExprKind::Call { callee, args, style: CallStyle::Paren, .. } = &call.kind else { return None };
        if extras.iter().any(InvokeParam::is_pointer) {
            return None;
        }
        for (param, arg) in native.wrapper_params().zip(args) {
            let kind = self.callees.arg_kind(arg);
            let unchanged = match param.kind {
                ParamKind::Value => true,
                ParamKind::Hash => kind != ArgKind::String,
                ParamKind::String => matches!(kind, ArgKind::String | ArgKind::Zero),
                ParamKind::Object | ParamKind::Function | ParamKind::Pointer | ParamKind::Filled => false,
            };
            if !unchanged {
                return None;
            }
        }
        let marker = match native.returns_result().then(|| native.returns().next()).flatten() {
            None | Some("boolean" | "any") => None,
            Some("number") => Some("Citizen.ResultAsFloat()"),
            Some("string") => Some("Citizen.ResultAsString()"),
            Some("vector3") => Some("Citizen.ResultAsVector()"),
            Some("table") => return None,
            Some(_) => Some("Citizen.ResultAsInteger()"),
        };
        let mut edits = vec![
            TextEdit { span: callee.span, new_text: "Citizen.InvokeNative".into() },
            TextEdit { span: Span::empty(args.first()?.span.start), new_text: format!("{}, ", native.hash) },
        ];
        if let Some(marker) = marker {
            edits.push(TextEdit { span: Span::empty(args.last()?.span.end), new_text: format!(", {marker}") });
        }
        Some(Fix { title: "Call through Citizen.InvokeNative".into(), edits, suggestion: true })
    }
}

/// `names` listed as a sentence does, as "`x`, `y` and `z`".
fn listed(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
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

/// What `arg` is as its syntax shows: a literal, a function, a comparison, or the vector that a
/// constructor such as `vector3(x, y, z)` or a native such as `GetEntityCoords` returns. Anything
/// else may be a vector, as `a + b` is for vectors.
fn written_kind(input: &FileInput, arg: &Expr) -> ArgKind {
    match &arg.unparen().kind {
        ExprKind::String(_) | ExprKind::Binary { op: BinOp::Concat, .. } => ArgKind::String,
        ExprKind::Nil | ExprKind::False => ArgKind::Zero,
        ExprKind::Number(NumberValue::Int(0)) => ArgKind::Zero,
        ExprKind::Number(NumberValue::Float(value)) if *value == 0.0 => ArgKind::Zero,
        ExprKind::Number(_) | ExprKind::True | ExprKind::Table(_) => ArgKind::Value,
        ExprKind::Unary { op: UnOp::Not | UnOp::Len, .. } => ArgKind::Value,
        ExprKind::Binary { op: BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, .. } => {
            ArgKind::Value
        }
        ExprKind::Function(_) => ArgKind::Function,
        ExprKind::Call { callee, .. } => {
            let ExprKind::Name(name) = &callee.unparen().kind else { return ArgKind::Unknown };
            if !matches!(input.resolution.resolve_at(name.span.start), Some(Resolved::Global(_))) {
                return ArgKind::Unknown;
            }
            match name.text.as_str() {
                "vector2" | "vec2" => ArgKind::Vector(2),
                "vector3" | "vec3" => ArgKind::Vector(3),
                "vector4" | "vec4" | "quat" => ArgKind::Vector(4),
                name => match native(name).filter(Native::returns_result).and_then(|n| n.returns().next()) {
                    Some("vector3") => ArgKind::Vector(3),
                    Some("string") => ArgKind::String,
                    Some("any" | "table") | None => ArgKind::Unknown,
                    Some(_) => ArgKind::Value,
                },
            }
        }
        _ => ArgKind::Unknown,
    }
}
