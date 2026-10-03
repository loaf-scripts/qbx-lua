//! `no-unknown`: a parameter, local or loop variable that has no type, because none is declared
//! and none can be inferred, and a value of no known type that code stores in a variable or field,
//! passes for a parameter or returns where a type is declared for it, other than `any`. A value read
//! from a local the rule reports itself is left out, as that report names what gives both a type.
//! The rule is off until a project turns it on, since most code is not annotated this far.

use qbx_lua_analysis::scope::{Local, LocalId, LocalKind, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::Span;
use rustc_hash::FxHashSet;

use super::arguments::unknown_arguments;
use super::assignments::unknown_assignments;
use super::callback_payloads::Payload;
use super::returns::unknown_returns;
use crate::infer::{Decl, Infer};
use crate::types::Type;

/// Each value of no known type that a variable, field, parameter or `@return` with a declared type
/// is given. One read from a local that `unknown_types` reports itself is left out, as that report
/// names what would give it a type.
pub fn unknown_values(infer: &Infer, chunk: &Chunk, payloads: &[Payload], ignored_prefix: &str) -> Vec<(Span, String)> {
    let reported: FxHashSet<LocalId> = untyped_locals(infer, ignored_prefix).into_iter().map(|(id, _)| id).collect();
    let pick = |expr: &Expr| !root_local(infer, expr).is_some_and(|id| reported.contains(&id));
    let mut out = unknown_assignments(infer, chunk, &pick);
    out.extend(unknown_arguments(infer, chunk, payloads, &pick));
    out.extend(unknown_returns(infer, chunk, &pick));
    out
}

/// The local that `expr` names, or that the fields, indexes and calls it makes are read from.
fn root_local(infer: &Infer, expr: &Expr) -> Option<LocalId> {
    match &expr.unparen().kind {
        ExprKind::Name(name) => match infer.ctx.resolution.resolve_at(name.span.start) {
            Some(Resolved::Local(id)) => Some(id),
            _ => None,
        },
        ExprKind::Field { base, .. } | ExprKind::Index { base, .. } | ExprKind::MethodCall { base, .. } => {
            root_local(infer, base)
        }
        ExprKind::Call { callee, .. } => root_local(infer, callee),
        _ => None,
    }
}

/// Whether `ty`, the declared type of where a value goes, says which values it takes: neither
/// `any` nor `unknown`, which take anything.
pub(super) fn is_typed(ty: &Type) -> bool {
    !matches!(ty, Type::Unknown | Type::Any)
}

/// `values`, the values that `exprs` give as `Classes::values` types them, with one of unknown type
/// for a call of no known type at their end, which gives none there.
pub(super) fn given_values(infer: &Infer, exprs: &[Expr], mut values: Vec<(Type, Span)>) -> Vec<(Type, Span)> {
    if let Some(last) = exprs.last().filter(|last| last.is_call()) {
        if values.len() < exprs.len() && infer.expr(last).is_unknown() {
            values.push((Type::Unknown, last.span));
        }
    }
    values
}

/// The expression that gives value `index` of those `exprs` give: the one at that place, or the call
/// that ends them.
pub(super) fn value_at(exprs: &[Expr], index: usize) -> Option<&Expr> {
    exprs.get(index).or_else(|| exprs.last().filter(|last| last.is_multi_value()))
}

/// The declaration of each local name whose type is unknown, with a message naming what gives it
/// one. `self`, local functions and the names that start with `ignored_prefix`, as unused locals
/// do, are left out.
pub fn unknown_types(infer: &Infer, ignored_prefix: &str) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    for (_, local) in untyped_locals(infer, ignored_prefix) {
        let name = local.name.as_str();
        let message = match local.kind {
            LocalKind::Param => format!("Parameter `{name}` has no type; add `---@param {name} <type>`"),
            LocalKind::LoopVar => {
                format!("Loop variable `{name}` has no type; give the value the loop goes through one")
            }
            _ => format!("The type of `{name}` is unknown; add `---@type <type>`"),
        };
        out.push((local.decl, message));
    }
    out
}

/// The locals whose type is unknown, as `unknown_types` reports them.
fn untyped_locals<'i>(infer: &'i Infer, ignored_prefix: &str) -> Vec<(LocalId, &'i Local)> {
    let locals = infer.ctx.resolution.locals.iter().enumerate().map(|(id, local)| (id as LocalId, local));
    let untyped = locals.filter(|(id, local)| {
        let name = local.name.as_str();
        let ignored = name == "_" || (!ignored_prefix.is_empty() && name.starts_with(ignored_prefix));
        if name.is_empty() || name == "self" || ignored {
            return false;
        }
        if matches!(local.kind, LocalKind::ImplicitSelf | LocalKind::LocalFunction) {
            return false;
        }
        match infer.local_type(*id) {
            Type::Unknown => true,
            // The handler of `RegisterNetEvent(name, function(payload) end)` takes `any` from the
            // `...` of the `fun(...)` it is passed as, which says no more about `payload` than
            // nothing does. A parameter the function type names, like `value` of
            // `fun(reason: string, value: any)`, is declared `any`.
            Type::Any if local.kind == LocalKind::Param => !has_param_line(infer, local) && from_vararg(infer, local),
            _ => false,
        }
    });
    untyped.collect()
}

/// Whether a `@param` line documents the parameter: above its function, the call its function is
/// passed to, or the table field that holds it.
pub(super) fn has_param_line(infer: &Infer, param: &Local) -> bool {
    let Some(Decl::Param { doc_anchor, .. }) = infer.ctx.decl(param.decl.start) else { return false };
    doc_anchor.is_some_and(|anchor| infer.ctx.doc_at(anchor).params.iter().any(|doc| doc.name == param.name))
}

/// Whether the parameter takes its value from the `...` of the function type its function has to be.
fn from_vararg(infer: &Infer, param: &Local) -> bool {
    let Some(Decl::Param { index, expected: Some(expected), .. }) = infer.ctx.decl(param.decl.start) else {
        return false;
    };
    infer.expected_fun(expected).is_some_and(|fun| fun.params.get(*index).is_some_and(|p| p.name == "..."))
}
