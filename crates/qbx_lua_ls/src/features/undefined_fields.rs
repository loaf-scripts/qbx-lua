//! `undefined-field`: fields that code reads from a value whose type does not have them, as
//! TypeScript reports a property its type does not declare. The linter reports the same code for
//! the standard library tables, like `string.nope`; this module checks the values whose type the
//! language server knows. A type has the fields that `inject-field` lets code set: those of a class
//! or table type, also the `{ label: string }` a table constructor gives, and those set through the
//! names that own its table, such as a global path, the table a `---@class` annotation declares,
//! `self` in a method or a local declared with a table at the top of a file. Reads such as
//! `value.name`, `value['name']` and `value:name()` are checked, also in conditions like
//! `if value.name then`, and a union lacks a field when none of its parts has it. A local that is
//! assigned again has the types of all the values that may reach the read. A value that is surely
//! `nil`, as what a call of a function that returns nothing gives, has no fields at all, as in LuaLS.
//!
//! Some values may have any field, so reads from them are left alone:
//!
//! - values of unknown type, `table` and `any`, empty tables such as `local result = {}`, and
//!   classes with an index that takes the name or a parent that is no class, like `table`;
//! - global tables and the paths from them, such as `Config.debug` or `ESX.PlayerData`, whose
//!   fields other files and resources set, and the instances made from them, unless an annotation
//!   types them, and the exports of a resource that no type is declared for;
//! - `self` in a method of a table that is no class, whose instances get their fields elsewhere,
//!   and tables whose metatable has an `__index` that is no table, such as a function;
//! - numbers, booleans and functions whose type is not declared, as `point - origin` is inferred to
//!   be a `number` for values of unknown type;
//! - strict classes, whose fields `undeclared-field` checks.

use qbx_lua_analysis::env::builtins;
use qbx_lua_analysis::scope::Resolved;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::{SmolStr, Span};

use super::class_tables::{accesses, Classes, Key};
use super::comparisons::Declared;
use super::injected_fields::{message, through_self};
use crate::index::{instance_class, is_local_table, FileId};
use crate::infer::{Decl, Infer};
use crate::narrow::Origin;
use crate::types::Type;

/// Each field that code reads from a value whose type does not have it, with the message naming
/// the type. `by_default` tells whether `strict_classes` applies to a class declared in a file.
pub fn undefined_fields(infer: &Infer, chunk: &Chunk, by_default: impl Fn(FileId) -> bool) -> Vec<(Span, String)> {
    let classes = Classes::new(infer);
    let declared = Declared::new(infer);
    let mut out = Vec::new();
    for access in accesses(infer, chunk, true) {
        let (Some(base), Key::Name(name)) = (access.read, &access.key) else { continue };
        let owner = match base.unparen().kind {
            ExprKind::Name(_) => classes.value_type(base),
            _ => access.owner,
        };
        // `nil` has no fields, as what a function that returns nothing gives.
        if owner == Type::Nil {
            if surely_nil(infer, base) {
                out.push((access.span, message(infer, name, &owner, access.holder)));
            }
            continue;
        }
        let parts: Vec<&Type> = match &owner {
            Type::Union(types) => types.iter().filter(|ty| !matches!(ty, Type::Nil)).collect(),
            ty => vec![ty],
        };
        let is_class = |ty: &&Type| classes.class_of(ty, classes.file()).is_some();
        if through_self(infer, access.holder) && !parts.iter().any(is_class) {
            continue;
        }
        let lacks = |ty: &&Type| lacks(infer, &classes, &declared, base, ty, &access.key, &by_default);
        if parts.is_empty() || !parts.iter().all(lacks) {
            continue;
        }
        // The exports of a resource are named by the type declared for them.
        let owner = match &owner {
            Type::Exports(Some(resource)) => infer.declared_export_type(resource).unwrap_or(owner),
            _ => owner,
        };
        if let Some(path) = global_path(infer, base) {
            // The linter checks the standard library tables, and a global path has the fields set on
            // it, like the `function math.clamp()` of a library that extends `math`.
            if builtins().closed_table(&path).is_some()
                || infer.member(&Type::GlobalTable(SmolStr::new(path)), name).is_some()
            {
                continue;
            }
        }
        out.push((access.span, message(infer, name, &owner, access.holder)));
    }
    out
}

/// Whether a value of type `ty`, read from `base`, lacks the field that `key` names. Values that
/// may have any field lack none.
fn lacks(
    infer: &Infer,
    classes: &Classes,
    declared: &Declared,
    base: &Expr,
    ty: &Type,
    key: &Key,
    by_default: impl Fn(FileId) -> bool,
) -> bool {
    let Key::Name(name) = key else { return false };
    let file = classes.file();
    if let Some((class, _, view)) = classes.class_of(ty, file) {
        return !classes.is_strict(&class, view, by_default)
            && !classes.declares(&class, view, key)
            && infer.member(ty, name).is_none();
    }
    match classes.table_type_of(ty, file) {
        Some(Type::Shape(shape)) if shape.fields.is_empty() && shape.array.is_none() && shape.indices.is_empty() => {
            return false;
        }
        Some(table) => return classes.table_field_type(&table, file, key).is_none(),
        None => {}
    }
    match infer.resolve_alias(ty) {
        Type::GlobalTable(owner) => {
            let instance = instance_class(&owner);
            // Other files and resources set the fields of global tables, and so of their instances.
            if !is_local_table(instance.unwrap_or(&owner)) || infer.indexes_at_runtime(&owner) {
                return false;
            }
            // A table that nothing gives a field may have any, as `{}` does.
            (instance.is_some() || infer.index.has_members(&owner)) && infer.member(ty, name).is_none()
        }
        Type::String | Type::StringLit(_) => infer.member(ty, name).is_none(),
        // A type declared for the exports of a resource tells what they hold, with what the files
        // that describe them set and what the resource registers.
        Type::Exports(Some(resource)) => match infer.declared_export_type(&resource) {
            Some(export_type) => {
                lacks(infer, classes, declared, base, &export_type, key, by_default) && infer.member(ty, name).is_none()
            }
            None => false,
        },
        ty if is_scalar(&ty) => {
            let declared = declared.of(base);
            let mut parts = match &declared {
                Type::Union(types) => types.iter().collect(),
                ty => vec![ty],
            };
            parts.retain(|ty| !matches!(ty, Type::Nil));
            !parts.is_empty() && parts.iter().all(|ty| is_scalar(&infer.resolve_alias(ty)))
        }
        _ => false,
    }
}

/// Whether `base` surely holds `nil`: it calls a function that returns nothing, or names a local
/// that is never assigned again after a declaration that gives it such a call, `nil` or no value,
/// or one that each value reaching it there gives such a call. Other values that read as `nil` may
/// hold what code of unknown type stores in them, and a local declared without a value may be
/// assigned before a function that reads it runs.
fn surely_nil(infer: &Infer, base: &Expr) -> bool {
    let base = base.unparen();
    if base.is_call() {
        return infer.returns_nothing(base);
    }
    let ExprKind::Name(name) = &base.kind else { return false };
    let Some(Resolved::Local(id)) = infer.ctx.resolution.resolve_at(name.span.start) else { return false };
    let local = infer.ctx.resolution.local(id);
    let Some(Decl::Local { stmt, index }) = infer.ctx.decl(local.decl.start) else { return false };
    let StmtKind::Local { exprs, in_unpack: false, .. } = &stmt.kind else { return false };
    if local.refs.iter().any(|r| r.write) {
        let versions = infer.ctx.flow().at(id, name.span.start);
        return !versions.is_empty()
            && versions.iter().all(|version| gives_nothing(infer, exprs, *index, version.origin));
    }
    match (exprs.get(*index).map(Expr::unparen), exprs.last()) {
        (Some(value), _) if value.is_call() => infer.returns_nothing(value),
        (Some(value), _) => matches!(value.kind, ExprKind::Nil),
        (None, Some(last)) if last.is_call() => infer.returns_nothing(last),
        (None, last) => last.is_none_or(|last| !last.is_multi_value()),
    }
}

/// Whether the value that `origin` gives a local declared with value `index` of `declared` is what a
/// call of a function that returns nothing gives.
fn gives_nothing(infer: &Infer, declared: &[Expr], index: usize, origin: u32) -> bool {
    let flow = infer.ctx.flow();
    let (exprs, index) = match flow.origin(origin) {
        Origin::Declaration => (declared, index),
        Origin::Assignment { stmt: Stmt { kind: StmtKind::Assign { exprs, .. }, .. }, index } => (&exprs[..], index),
        Origin::Others(set) => {
            return flow.others(set).iter().all(|other| gives_nothing(infer, declared, index, *other))
        }
        _ => return false,
    };
    let call = match exprs.get(index) {
        Some(value) => Some(value.unparen()),
        None => exprs.last(),
    };
    call.is_some_and(|call| call.is_call() && infer.returns_nothing(call))
}

/// Whether a value of type `ty` is a number, a boolean or a function, which has no fields.
fn is_scalar(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Number
            | Type::Integer
            | Type::IntLit(_)
            | Type::Handle(_)
            | Type::Boolean
            | Type::BooleanLit(_)
            | Type::Function
            | Type::Fun(_)
    )
}

/// The dotted path of `expr` when it starts from a global, like `math` or `Config.Sound`.
fn global_path(infer: &Infer, expr: &Expr) -> Option<String> {
    let mut root = expr.unparen();
    while let ExprKind::Field { base, .. } | ExprKind::Index { base, .. } = &root.kind {
        root = base.unparen();
    }
    let ExprKind::Name(name) = &root.kind else { return None };
    let global = matches!(infer.ctx.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)));
    global.then(|| expr.unparen().dotted_path()).flatten()
}
