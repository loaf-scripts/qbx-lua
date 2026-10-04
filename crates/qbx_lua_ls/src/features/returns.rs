//! `return-type-mismatch`, `missing-return` and `redundant-return-value`: a function documented with
//! `@return` has to return values of those types. A value of a different kind, or a literal the type
//! does not list, is a mismatch, for each type a union lists, `nil` included, though not the `nil`
//! that the type of a field read allows, as lua-language-server reads fields, unless `strict` asks
//! for what TypeScript reports; a `return` with fewer
//! values than the required ones, or a body that can run past its end, is missing one; a `return`
//! with more values than declared returns values nobody expects. A value is required unless its
//! type allows `nil`. An empty body is missing its values too, except in a `---@meta` file, whose
//! functions only declare their signatures. Returned tables typed as a class are checked like other
//! class tables, by `missing-fields`, `assign-type-mismatch` and `undeclared-field`.
//!
//! A function that lists sets of values, as `@return false | (string, string)` does, has to return
//! one of them: each `return` is compared with the set it comes closest to.
//!
//! A function literal without `@return` gets the same checks from the function type it is written
//! as, as lua-language-server gives them: the `fun(n: integer): string` of the parameter it is
//! passed for, of the `---@type` of the statement it is the value of, or of the field of a typed
//! table it is written in.

use qbx_lua_analysis::env::is_meta_file;
use qbx_lua_analysis::scope::Resolved;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;
use rustc_hash::FxHashSet;

use super::class_tables::{checked_value, Classes};
use super::unknown_types::{given_values, is_typed, value_at};
use crate::infer::{always_exits, return_stmts, Infer};
use crate::types::Type;

/// Each returned value that its function's `@return` does not take, with the message naming both
/// types.
pub fn mismatched_returns(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let classes = Classes::new(infer);
    let mut out = Vec::new();
    for function in documented(infer, chunk) {
        for (_, exprs) in return_stmts(&function.func.body) {
            let values = classes.values(exprs).into_iter().enumerate();
            let values: Vec<(Type, Span)> = values
                .map(|(index, (given, span))| (checked_value(infer, value_at(exprs, index), given), span))
                .collect();
            out.extend(mismatches(&classes, function.closest(&classes, exprs, &values), &values));
        }
    }
    out
}

/// Each returned value of no known type, of those `pick` picks, whose function's `@return` declares
/// its type, with the message naming the position.
pub fn unknown_returns(infer: &Infer, chunk: &Chunk, pick: &dyn Fn(&Expr) -> bool) -> Vec<(Span, String)> {
    let classes = Classes::new(infer);
    let mut out = Vec::new();
    for function in documented(infer, chunk) {
        for (_, exprs) in return_stmts(&function.func.body) {
            let values = given_values(infer, exprs, classes.values(exprs));
            let returns = function.closest(&classes, exprs, &values);
            for (index, (given, span)) in values.iter().enumerate() {
                let Some(expected) = expected_at(returns, index) else { break };
                if given.is_unknown() && is_typed(expected) && value_at(exprs, index).is_some_and(pick) {
                    let message = format!("The type of return value #{} of type `{expected}` is unknown", index + 1);
                    out.push((*span, message));
                }
            }
        }
    }
    out
}

/// Each of `values` that the `@return` type at its position does not take.
fn mismatches(classes: &Classes, returns: &[Type], values: &[(Type, Span)]) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    for (i, (given, span)) in values.iter().enumerate() {
        let Some(expected) = expected_at(returns, i) else { break };
        if let Some(part) = classes.rejected_part(expected, classes.file(), given) {
            let shown = classes.shown(expected, classes.file(), given, &part);
            out.push((*span, format!("Cannot return `{shown}` as return value #{} of type `{expected}`", i + 1)));
        }
    }
    out
}

/// How many values `returns` requires: up to the last whose type does not allow `nil`.
fn required_values(classes: &Classes, returns: &[Type]) -> usize {
    let is_required = |ty: &Type| !matches!(ty, Type::Variadic(_)) && !classes.admits_nil(ty, classes.file());
    returns.iter().rposition(is_required).map_or(0, |last| last + 1)
}

/// Whether a `return` passes fewer values than `required`. A call or `...` at its end passes as
/// many values as it gives.
fn is_short(exprs: &[Expr], required: usize) -> bool {
    exprs.len() < required && !exprs.last().is_some_and(Expr::is_multi_value)
}

/// Each `return` that gives fewer values than its function's `@return` requires, and the `end` of
/// each such function that its body can run past.
pub fn missing_returns(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let classes = Classes::new(infer);
    let is_meta = is_meta_file(infer.ctx.source, chunk);
    let mut out = Vec::new();
    for function in documented(infer, chunk) {
        let Documented { func, returns, sets, .. } = &function;
        let required = required_values(&classes, returns);
        if (required == 0 && sets.is_empty()) || (is_meta && func.body.stmts.is_empty()) {
            continue;
        }
        let declarer = function.declarer();
        for (stmt, exprs) in return_stmts(&func.body) {
            // With several sets of values, the one this `return` comes closest to decides.
            let required = match sets.is_empty() {
                true => required,
                false => {
                    let values = classes.values(exprs);
                    required_values(&classes, function.closest(&classes, exprs, &values))
                }
            };
            if is_short(exprs, required) {
                let message =
                    format!("{declarer} requires {}, but this returns {}", values(required), values(exprs.len()));
                out.push((stmt.span, message));
            }
        }
        if required > 0 && !always_exits(&func.body) {
            let types: Vec<String> = returns[..required].iter().map(code).collect();
            let message = format!(
                "The function can reach its end without returning, but {} requires {}",
                declarer.to_lowercase(),
                types.join(", ")
            );
            out.push((func.end_span, message));
        }
    }
    out
}

/// Each `return` that passes more values than its function's `@return` declares, at the values it
/// does not declare. A trailing `...T` takes any number of values, and of several sets, or of the
/// signatures `@overload` adds, the longest decides. Only the values written out count: a call or
/// `...` at the end may give none, or more than it needs to, as `return text:gsub(...)` passes on
/// the count of replacements.
pub fn redundant_returns(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let is_open = |types: &[Type]| matches!(types.last(), Some(Type::Variadic(_)));
    let mut out = Vec::new();
    for function in documented(infer, chunk) {
        let Documented { func, returns, sets, overloads, .. } = &function;
        let declared: Vec<&Vec<Type>> = std::iter::once(returns).chain(sets).chain(overloads).collect();
        if declared.iter().any(|types| is_open(types)) {
            continue;
        }
        let most = declared.iter().map(|types| types.len()).max().unwrap_or(0);
        for (_, exprs) in return_stmts(&func.body) {
            let open_ended = exprs.last().is_some_and(Expr::is_multi_value);
            let written = exprs.len() - usize::from(open_ended);
            let (Some(first), Some(last)) = (exprs.get(most), exprs.last()) else { continue };
            if written <= most {
                continue;
            }
            let returned = if open_ended { format!("at least {}", values(written)) } else { values(written) };
            let message = match function.handler && most == 0 {
                true => format!("Event handlers' return values are discarded, but this returns {returned}"),
                false => {
                    format!("{} allows at most {}, but this returns {returned}", function.declarer(), values(most))
                }
            };
            out.push((first.span.to(last.span), message));
        }
    }
    out
}

/// `ty` written as code: in backticks, unless it is already written in them, as `` `T` `` is.
fn code(ty: &Type) -> String {
    match ty {
        Type::NameOf(_) => ty.to_string(),
        _ => format!("`{ty}`"),
    }
}

fn values(count: usize) -> String {
    match count {
        0 => "no values".to_string(),
        1 => "1 value".to_string(),
        count => format!("{count} values"),
    }
}

/// The `@return` type of the value at `index`; a trailing `...T` covers every value from there on.
fn expected_at(returns: &[Type], index: usize) -> Option<&Type> {
    match returns.get(index) {
        Some(Type::Variadic(inner)) => Some(inner),
        Some(ty) => Some(ty),
        None => match returns.last() {
            Some(Type::Variadic(inner)) => Some(inner),
            _ => None,
        },
    }
}

/// A function documented with `@return`, or written as a value of a declared function type.
struct Documented<'c> {
    func: &'c FuncBody,
    /// The type of each value it returns.
    returns: Vec<Type>,
    /// The sets of values of `@return false | (string, string)`, which `returns` then merges.
    sets: Vec<Vec<Type>>,
    /// The values each `@overload` returns, or each set of them it lists.
    overloads: Vec<Vec<Type>>,
    /// The values come from the function type the function is written as, not from `@return`.
    typed: bool,
    /// It is the handler passed to `AddEventHandler` or `RegisterNetEvent`, whose returned values
    /// the event system discards.
    handler: bool,
}

impl Documented<'_> {
    /// What declares the values, as a message names it.
    fn declarer(&self) -> &'static str {
        if self.typed {
            "Its function type"
        } else {
            "`@return`"
        }
    }

    /// The types a `return` passing `values` is compared with: `returns`, or of several sets the
    /// one that takes the most of the values, then one the `return` is not short for, then the one
    /// nearest in length.
    fn closest(&self, classes: &Classes, exprs: &[Expr], values: &[(Type, Span)]) -> &[Type] {
        let distance = |set: &&Vec<Type>| {
            let rejected = mismatches(classes, set, values).len();
            (rejected, is_short(exprs, required_values(classes, set)), set.len().abs_diff(values.len()))
        };
        self.sets.iter().min_by_key(distance).unwrap_or(&self.returns)
    }
}

/// The functions of `chunk` documented with `@return`, and the function literals without it that a
/// declared function type describes.
fn documented<'c>(infer: &Infer, chunk: &'c Chunk) -> Vec<Documented<'c>> {
    let mut finder =
        Finder { infer, out: Vec::new(), with_returns: FxHashSet::default(), handlers: FxHashSet::default() };
    finder.visit_block(&chunk.block);
    finder.out
}

struct Finder<'a, 'b, 'c> {
    infer: &'a Infer<'b>,
    out: Vec<Documented<'c>>,
    /// The functions documented with `@return`, by the start of their parameter list.
    with_returns: FxHashSet<u32>,
    /// The function literals passed as event handlers, by the start of their parameter list.
    handlers: FxHashSet<u32>,
}

impl Finder<'_, '_, '_> {
    /// Whether `callee` is the runtime's `AddEventHandler` or `RegisterNetEvent`, not a local of
    /// that name.
    fn registers_event(&self, callee: &Expr) -> bool {
        let ExprKind::Name(name) = &callee.unparen().kind else { return false };
        matches!(name.text.as_str(), "AddEventHandler" | "RegisterNetEvent")
            && matches!(self.infer.ctx.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
    }
}

impl<'c> Visitor<'c> for Finder<'_, '_, 'c> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        for (doc, functions) in self.infer.ctx.function_docs(stmt) {
            if !doc.returns.is_empty() {
                let overloads: Vec<Vec<Type>> = doc
                    .overloads
                    .iter()
                    .flat_map(|overload| std::iter::once(&overload.returns).chain(&overload.return_sets).cloned())
                    .collect();
                let documented = |func: &'c FuncBody| {
                    let ty = |ty: &Type| self.infer.doc_type_for(stmt, func, ty);
                    let returns = doc.returns.iter().map(|r| ty(&r.ty)).collect();
                    let sets = doc.return_sets.iter().map(|set| set.iter().map(ty).collect()).collect();
                    Documented { func, returns, sets, overloads: overloads.clone(), typed: false, handler: false }
                };
                self.with_returns.extend(functions.iter().map(|func| func.params_span.start));
                self.out.extend(functions.into_iter().map(documented));
            }
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'c Expr) {
        if let ExprKind::Call { callee, args, .. } = &expr.kind {
            if self.registers_event(callee) {
                let handlers = args.iter().filter_map(|arg| match &arg.unparen().kind {
                    ExprKind::Function(func) => Some(func.params_span.start),
                    _ => None,
                });
                self.handlers.extend(handlers);
            }
        }
        if let ExprKind::Function(func) = &expr.kind {
            if !self.with_returns.contains(&func.params_span.start) {
                if let Some(fun) = self.infer.declared_fun_type(func) {
                    let (returns, sets) = (fun.returns.clone(), fun.return_sets.clone());
                    let handler = self.handlers.contains(&func.params_span.start);
                    self.out.push(Documented { func, returns, sets, overloads: Vec::new(), typed: true, handler });
                }
            }
        }
        visit::walk_expr(self, expr);
    }
}
