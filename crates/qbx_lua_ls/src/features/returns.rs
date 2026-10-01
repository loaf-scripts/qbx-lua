//! `return-type-mismatch` and `missing-return`: a function documented with `@return` has to return
//! values of those types. A value of a different kind, or a literal the type does not list, is a
//! mismatch; a `return` with fewer values than the required ones, or a body that can run past its
//! end, is missing one. A value is required unless its type allows `nil`. An empty body is missing
//! its values too, except in a `---@meta` file, whose functions only declare their signatures.
//! Returned tables typed as a class are checked like other class tables, by `missing-fields`,
//! `assign-type-mismatch` and `undeclared-field`.
//!
//! A function that lists sets of values, as `@return false | (string, string)` does, has to return
//! one of them: each `return` is compared with the set it comes closest to.

use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;

use super::class_tables::Classes;
use crate::indexer::is_meta_file;
use crate::infer::{always_exits, return_stmts, Infer};
use crate::types::Type;

/// Each returned value that its function's `@return` does not take, with the message naming both
/// types.
pub fn mismatched_returns(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let classes = Classes::new(infer);
    let mut out = Vec::new();
    for function in documented(infer, chunk) {
        for (_, exprs) in return_stmts(&function.func.body) {
            let values = classes.values(exprs);
            out.extend(mismatches(&classes, function.closest(&classes, exprs, &values), &values));
        }
    }
    out
}

/// Each of `values` that the `@return` type at its position does not take.
fn mismatches(classes: &Classes, returns: &[Type], values: &[(Type, Span)]) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    for (i, (given, span)) in values.iter().enumerate() {
        let Some(expected) = expected_at(returns, i) else { break };
        if classes.rejects(expected, classes.file(), given) {
            let shown =
                if classes.literal_mismatch(expected, classes.file(), given) { given.clone() } else { given.widen() };
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
        let Documented { func, returns, sets } = &function;
        let required = required_values(&classes, returns);
        if (required == 0 && sets.is_empty()) || (is_meta && func.body.stmts.is_empty()) {
            continue;
        }
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
                    format!("`@return` requires {}, but this returns {}", values(required), values(exprs.len()));
                out.push((stmt.span, message));
            }
        }
        if required > 0 && !always_exits(&func.body) {
            let types: Vec<String> = returns[..required].iter().map(code).collect();
            let message = format!(
                "The function can reach its end without returning, but `@return` requires {}",
                types.join(", ")
            );
            out.push((func.end_span, message));
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

/// A function documented with `@return`.
struct Documented<'c> {
    func: &'c FuncBody,
    /// The type of each value it returns.
    returns: Vec<Type>,
    /// The sets of values of `@return false | (string, string)`, which `returns` then merges.
    sets: Vec<Vec<Type>>,
}

impl Documented<'_> {
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

/// The functions of `chunk` documented with `@return`.
fn documented<'c>(infer: &Infer, chunk: &'c Chunk) -> Vec<Documented<'c>> {
    let mut finder = Finder { infer, out: Vec::new() };
    finder.visit_block(&chunk.block);
    finder.out
}

struct Finder<'a, 'b, 'c> {
    infer: &'a Infer<'b>,
    out: Vec<Documented<'c>>,
}

impl<'c> Visitor<'c> for Finder<'_, '_, 'c> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        for (doc, functions) in self.infer.ctx.function_docs(stmt) {
            if !doc.returns.is_empty() {
                let documented = |func| {
                    let ty = |ty: &Type| self.infer.doc_type_for(stmt, func, ty);
                    let returns = doc.returns.iter().map(|r| ty(&r.ty)).collect();
                    let sets = doc.return_sets.iter().map(|set| set.iter().map(ty).collect()).collect();
                    Documented { func, returns, sets }
                };
                self.out.extend(functions.into_iter().map(documented));
            }
        }
        visit::walk_stmt(self, stmt);
    }
}
