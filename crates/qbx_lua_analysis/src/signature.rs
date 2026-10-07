//! Function signatures from LuaCATS doc comments, and how many arguments a call has to pass.

use std::sync::Arc;

use qbx_lua_syntax::ast::{Expr, ExprKind, FuncBody, Name};
use qbx_lua_syntax::{Comment, SmolStr};
use qbx_luacats::luacats::{parse_doc_lines, DocGroup};
use qbx_luacats::types::{FunType, Param, Type};

use crate::env::leading_doc_lines;

const MAX_ALIAS_DEPTH: u8 = 8;

/// The signature the doc comment above `stmt_start` gives the function `func`, or `None` when the
/// comment says nothing about its parameters.
pub fn documented(
    source: &str,
    comments: &[Comment],
    stmt_start: u32,
    func: &FuncBody,
    is_method: bool,
) -> Option<Arc<FunType>> {
    let lines = leading_doc_lines(source, comments, stmt_start);
    if lines.is_empty() {
        return None;
    }
    let doc = parse_doc_lines(&lines);
    if let Some(Type::Fun(fun)) = &doc.ty {
        return Some(fun.clone());
    }
    if doc.params.is_empty() && doc.overloads.is_empty() {
        return None;
    }
    let names: Vec<SmolStr> = func.params.iter().map(|p| p.text.clone()).collect();
    Some(Arc::new(doc.fun_type(&names, func.vararg.is_some(), is_method)))
}

/// The signature of the function `func`: the one the doc comment above `stmt_start` gives, or else
/// its parameters, which take any value.
pub fn defined(source: &str, comments: &[Comment], stmt_start: u32, func: &FuncBody, is_method: bool) -> Arc<FunType> {
    documented(source, comments, stmt_start, func, is_method).unwrap_or_else(|| undocumented(func, is_method))
}

/// The parameters of `func`, which take any value, as the signature of a function without a doc
/// comment.
pub fn undocumented(func: &FuncBody, is_method: bool) -> Arc<FunType> {
    let names: Vec<SmolStr> = func.params.iter().map(|p| p.text.clone()).collect();
    Arc::new(DocGroup::default().fun_type(&names, func.vararg.is_some(), is_method))
}

/// The variable an `a.b.c` or `a['b']` chain starts from, and the field names after it.
pub fn member_path(expr: &Expr) -> Option<(&Name, Vec<&str>)> {
    let (base, field) = match &expr.kind {
        ExprKind::Name(name) => return Some((name, Vec::new())),
        ExprKind::Field { base, name, .. } => (base, name.text.as_str()),
        ExprKind::Index { base, index, .. } => (base, index.as_string()?.as_str()),
        _ => return None,
    };
    let (root, mut fields) = member_path(base)?;
    fields.push(field);
    Some((root, fields))
}

/// The dotted path a global or a field of a global table is summarized under; `_G.Notify` is `Notify`.
pub fn global_key(root: &str, fields: &[&str]) -> Option<SmolStr> {
    let parts = match root {
        "_G" | "_ENV" => fields.to_vec(),
        _ => std::iter::once(root).chain(fields.iter().copied()).collect(),
    };
    (!parts.is_empty()).then(|| SmolStr::new(parts.join(".")))
}

/// The `@alias` declarations of every `---` comment block in a file, with their types. Like LuaLS,
/// other comments inside a block are passed over; a blank line or code ends it.
pub fn doc_aliases(source: &str, comments: &[Comment]) -> Vec<(SmolStr, Type)> {
    let mut aliases = Vec::new();
    doc_blocks_with(source, comments, "@alias", |doc| {
        aliases.extend(doc.aliases.into_iter().map(|alias| (alias.name, alias.ty)));
    });
    aliases
}

/// The signatures that the `@extend` lines of a file add, by the dotted path of the function they
/// add them to, as `summary` keys its definitions: `Player.on` for `---@extend Player:on fun()`.
pub fn doc_extensions(source: &str, comments: &[Comment]) -> Vec<(SmolStr, Arc<FunType>)> {
    let mut extensions = Vec::new();
    doc_blocks_with(source, comments, "@extend", |doc| {
        for extend in doc.extends {
            let path = match &extend.owner {
                Some(owner) => {
                    let mut parts = owner.split('.');
                    let root = parts.next().unwrap_or_default();
                    global_key(root, &parts.chain([extend.name.as_str()]).collect::<Vec<_>>())
                }
                None => Some(extend.name.clone()),
            };
            extensions.extend(path.map(|path| (path, extend.fun)));
        }
    });
    extensions
}

/// Runs `found` on each `---` comment block of a file that has a line holding `needle`, parsed. Like
/// LuaLS, other comments inside a block are passed over; a blank line or code ends it.
fn doc_blocks_with(source: &str, comments: &[Comment], needle: &str, mut found: impl FnMut(DocGroup)) {
    if !source.contains(needle) {
        return;
    }
    let mut block: Vec<&str> = Vec::new();
    let mut flush = |block: &mut Vec<&str>| {
        if block.iter().any(|line| line.contains(needle)) {
            found(parse_doc_lines(block));
        }
        block.clear();
    };
    let mut previous_end: Option<u32> = None;
    for comment in comments {
        let adjacent = previous_end.is_some_and(|end| {
            let gap = &source[end as usize..comment.span.start as usize];
            gap.bytes().filter(|b| *b == b'\n').count() <= 1 && gap.trim().is_empty()
        });
        if !adjacent {
            flush(&mut block);
        }
        if let Some(line) = comment.span.text(source).strip_prefix("---") {
            block.push(line);
        }
        previous_end = Some(comment.span.end);
    }
    flush(&mut block);
}

/// Whether a parameter of this type may be left out: `nil`, `any` and `unknown` allow it, and so
/// do unions and aliases that include one of them.
pub fn may_be_nil<'t>(ty: &Type, alias: &impl Fn(&str) -> Option<&'t Type>) -> bool {
    may_be_nil_at(ty, alias, 0)
}

fn may_be_nil_at<'t>(ty: &Type, alias: &impl Fn(&str) -> Option<&'t Type>, depth: u8) -> bool {
    match ty {
        Type::Unknown | Type::Any | Type::Nil | Type::Variadic(_) => true,
        Type::Union(types) => types.iter().any(|t| may_be_nil_at(t, alias, depth)),
        Type::Named(name, args) if args.is_empty() && depth < MAX_ALIAS_DEPTH => {
            alias(name).is_some_and(|t| may_be_nil_at(t, alias, depth + 1))
        }
        _ => false,
    }
}

/// What a call must pass to one signature.
#[derive(Clone, Debug)]
pub struct Requirement<'f> {
    /// Arguments the call needs, counting a `self` it has to pass explicitly.
    pub arguments: usize,
    /// The required parameters, each with the argument position it is passed at.
    required: Vec<(usize, &'f Param)>,
    /// Whether the call has to pass `self` as its first argument.
    explicit_self: bool,
}

impl<'f> Requirement<'f> {
    /// The requirement of `fun` for a call made with `:` (`via_colon`) or `.`. A parameter is
    /// required when it is documented with a type that does not allow `nil`; every parameter before
    /// the last required one has to be passed as well.
    ///
    /// Positions are counted the way Lua passes values, whatever the parameters are named: a `:`
    /// call passes its receiver first, and a function defined with `:` receives `self` first. So
    /// `function Locale.new(_, opts)` called as `Locale:new(opts)` gets both.
    pub fn of<'t>(fun: &'f FunType, via_colon: bool, alias: &impl Fn(&str) -> Option<&'t Type>) -> Self {
        let implicit_self = usize::from(fun.is_method);
        let receiver = usize::from(via_colon);
        let required: Vec<(usize, &Param)> = fun
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| p.name != "..." && !p.optional && !may_be_nil(&p.ty, alias))
            .filter_map(|(index, param)| Some(((implicit_self + index).checked_sub(receiver)?, param)))
            .collect();
        let explicit_self = implicit_self > receiver;
        let arguments = required.last().map_or(usize::from(explicit_self), |(position, _)| position + 1);
        Self { arguments, required, explicit_self }
    }

    /// The first required parameter a call with `passed` arguments leaves out; `None` when that is
    /// the `self` a call with `.` has to pass.
    pub fn first_missing(&self, passed: usize) -> Option<&'f Param> {
        if self.explicit_self && passed == 0 {
            return None;
        }
        self.required.iter().find(|(position, _)| *position >= passed).map(|(_, param)| *param)
    }
}

/// How many arguments a call made with `:` (`via_colon`) or `.` can pass to `fun`, with positions
/// counted as `Requirement::of` counts them; `None` when its `...` takes any number.
pub fn most_arguments(fun: &FunType, via_colon: bool) -> Option<usize> {
    if fun.params.last().is_some_and(|p| p.name == "...") {
        return None;
    }
    Some((usize::from(fun.is_method) + fun.params.len()).saturating_sub(usize::from(via_colon)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qbx_lua_syntax::ast::StmtKind;
    use qbx_lua_syntax::parse;

    fn signature(source: &str) -> Option<Arc<FunType>> {
        let chunk = parse(source);
        let stmt = chunk.block.stmts.last().unwrap();
        let (func, is_method) = match &stmt.kind {
            StmtKind::Function { name, func } => (func, name.method.is_some()),
            StmtKind::LocalFunction { func, .. } => (func, false),
            _ => panic!("not a function statement"),
        };
        documented(source, &chunk.comments, stmt.span.start, func, is_method)
    }

    fn required(source: &str, via_colon: bool) -> usize {
        let fun = signature(source).expect("documented");
        Requirement::of(&fun, via_colon, &|_| None).arguments
    }

    #[test]
    fn required_parameters() {
        assert_eq!(required("---@param a string\n---@param b number\nfunction f(a, b) end", false), 2);
        assert_eq!(required("---@param a string\n---@param b? number\nfunction f(a, b) end", false), 1);
        assert_eq!(required("---@param a? string\n---@param b number\nfunction f(a, b) end", false), 2);
        assert_eq!(required("---@param a string|nil\n---@param b any\nfunction f(a, b) end", false), 0);
        assert_eq!(required("---@param b number\nfunction f(a, b, c) end", false), 2);
        assert_eq!(required("---@param a string\n---@param ... number\nfunction f(a, ...) end", false), 1);
        assert_eq!(required("---@type fun(a: string, b?: number)\nlocal function f(a, b) end", false), 1);
    }

    #[test]
    fn methods_count_self_only_when_it_is_passed_explicitly() {
        let method = "---@param x number\nfunction M:m(x) end";
        assert_eq!(required(method, true), 1);
        assert_eq!(required(method, false), 2);
        let explicit = "---@param x number\nfunction M.m(self, x) end";
        assert_eq!(required(explicit, true), 1);
        assert_eq!(required(explicit, false), 2);
        // The receiver of a `:` call fills the first parameter whatever its name (qb-core's Locale).
        let unnamed = "---@param opts table\nfunction Locale.new(_, opts) end";
        assert_eq!(required(unnamed, true), 1);
        assert_eq!(required(unnamed, false), 2);
        let documented_self = "---@param self Locale\n---@param opts table\nfunction Locale.new(self, opts) end";
        assert_eq!(required(documented_self, true), 1);
    }

    #[test]
    fn names_the_first_missing_required_parameter() {
        let fun = signature("---@param x number\nfunction M:m(x) end").unwrap();
        let dot = Requirement::of(&fun, false, &|_| None);
        assert!(dot.first_missing(0).is_none(), "self");
        assert_eq!(dot.first_missing(1).unwrap().name, "x");
        assert_eq!(Requirement::of(&fun, true, &|_| None).first_missing(0).unwrap().name, "x");

        let fun = signature("---@param a? string\n---@param b number\nfunction f(a, b) end").unwrap();
        assert_eq!(Requirement::of(&fun, false, &|_| None).first_missing(0).unwrap().name, "b");
    }

    fn most(source: &str, via_colon: bool) -> Option<usize> {
        let chunk = parse(source);
        let stmt = chunk.block.stmts.last().unwrap();
        let StmtKind::Function { name, func } = &stmt.kind else { panic!("not a function statement") };
        most_arguments(&defined(source, &chunk.comments, stmt.span.start, func, name.method.is_some()), via_colon)
    }

    #[test]
    fn most_arguments_count_every_parameter() {
        assert_eq!(most("function f(a, b) end", false), Some(2), "undocumented parameters count");
        assert_eq!(most("---@param a string\nfunction f(a, b) end", false), Some(2));
        assert_eq!(most("function f(a, ...) end", false), None);
        assert_eq!(most("---@type fun(a: string, ...: any)\nfunction f(a, ...) end", false), None);
        assert_eq!(most("function M:m(a) end", true), Some(1));
        assert_eq!(most("function M:m(a) end", false), Some(2), "self is passed explicitly");
        assert_eq!(most("function M.f(a) end", true), Some(0), "the receiver fills 'a'");
    }

    #[test]
    fn undocumented_functions_have_no_signature() {
        assert!(signature("function f(a, b) end").is_none());
        assert!(signature("--- Adds two numbers.\n---@return number\nfunction f(a, b) end").is_none());
        assert!(signature("---@param a number\n\nfunction f(a) end").is_none());
    }

    #[test]
    fn aliases_that_allow_nil() {
        let source =
            "---@alias Maybe number|nil\n---@alias Mode\n---| 'a'\n---| 'b'\nlocal x = 1\n---@alias Wrapped Maybe";
        let chunk = parse(source);
        let aliases = doc_aliases(source, &chunk.comments);
        let names: Vec<&str> = aliases.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["Maybe", "Mode", "Wrapped"]);
        let alias = |name: &str| aliases.iter().find(|(n, _)| n == name).map(|(_, ty)| ty);
        let named = |name: &str| Type::Named(name.into(), Vec::new());
        assert!(may_be_nil(&named("Maybe"), &alias));
        assert!(may_be_nil(&named("Wrapped"), &alias));
        assert!(!may_be_nil(&named("Mode"), &alias));
        assert!(!may_be_nil(&named("Player"), &alias));
    }

    #[test]
    fn alias_blocks_pass_over_other_comments() {
        let source = "---@alias Mode\n-- note\n---| 'a'\n---| 'b'\n\n---@alias Wide\n--[[ note ]]\n---| string\n---| number\n\n---@alias Strict number\n\n-- note\n---| nil";
        let chunk = parse(source);
        let aliases = doc_aliases(source, &chunk.comments);
        let alias = |name: &str| aliases.iter().find(|(n, _)| n == name).map(|(_, ty)| ty);
        let named = |name: &str| Type::Named(name.into(), Vec::new());
        assert!(!may_be_nil(&named("Mode"), &alias), "a plain comment keeps the values");
        assert!(!may_be_nil(&named("Wide"), &alias), "a block comment keeps the values");
        assert!(!may_be_nil(&named("Strict"), &alias), "a blank line ends the alias");
    }

    #[test]
    fn alias_cycles_end() {
        let aliases = [("A", Type::Named("B".into(), Vec::new())), ("B", Type::Named("A".into(), Vec::new()))];
        let alias = |name: &str| aliases.iter().find(|(n, _)| *n == name).map(|(_, ty)| ty);
        assert!(!may_be_nil(&Type::Named("A".into(), Vec::new()), &alias));
    }
}
