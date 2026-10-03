//! `inject-field`: fields that an assignment or a `function value:name()` statement sets through a
//! value whose type does not have them, as TypeScript reports a property its type does not
//! declare. The type is a class, a table type such as the `{ label: string }` a table constructor
//! gives a local, or a table that another name declares, like the rows of another file's table
//! that a loop goes through. What is set through the names that own their tables declares the
//! field instead, so it is never reported: a global and the paths from it, the table a `---@class`
//! annotation declares, `self` in a method, a local declared with a table at the top of the file,
//! and the instance a constructor makes in a local. An empty table such as `local result = {}`
//! takes any field, and so do values of unknown type, `table` and `any`. Only names are checked,
//! not keys held in variables, and strict classes are left to `undeclared-field`.

use qbx_lua_analysis::scope::Resolved;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::Span;

use super::class_tables::{accesses, Classes, Key};
use crate::index::FileId;
use crate::infer::{Decl, Infer};
use crate::types::Type;

/// The longest expression a message names for a table whose type has no name of its own.
const MAX_HOLDER_LEN: usize = 40;

/// Each field that an assignment or a `function value:name()` statement sets through a value whose
/// type does not have it, with the message naming the type. `by_default` tells whether
/// `strict_classes` applies to a class declared in a file.
pub fn injected_fields(infer: &Infer, chunk: &Chunk, by_default: impl Fn(FileId) -> bool) -> Vec<(Span, String)> {
    let classes = Classes::new(infer);
    let mut out = Vec::new();
    for access in accesses(infer, chunk, false).into_iter().filter(|access| !access.on_class_table) {
        let Key::Name(name) = access.key else { continue };
        if through_self(infer, access.holder) {
            continue;
        }
        let parts = match access.owner.without_nil() {
            Type::Union(types) => types.iter().filter(|ty| !matches!(ty, Type::Nil)).cloned().collect(),
            ty => vec![ty],
        };
        let lacks = |ty: &Type| has(infer, &classes, ty, &access.key, name, &by_default) == Some(false);
        if !parts.is_empty() && parts.iter().all(lacks) {
            out.push((access.span, message(infer, name, &access.owner, access.holder)));
        }
    }
    out
}

/// Whether the value written at `holder` is `self` in a method, or a path from it, whose fields the
/// method declares.
fn through_self(infer: &Infer, holder: Span) -> bool {
    let Some(Resolved::Local(id)) = infer.ctx.resolution.resolve_at(holder.start) else { return false };
    matches!(infer.ctx.decl(infer.ctx.resolution.local(id).decl.start), Some(Decl::SelfParam { .. }))
}

/// Whether a value of type `ty` has the field `name`, which `key` is: a field of its class or
/// table type, or one that code sets through the names that own its table. `None` when the type
/// does not tell which fields a value has, or is a strict class, whose fields `undeclared-field`
/// checks.
fn has(
    infer: &Infer,
    classes: &Classes,
    ty: &Type,
    key: &Key,
    name: &str,
    by_default: impl Fn(FileId) -> bool,
) -> Option<bool> {
    let file = classes.file();
    if let Some((class, _, view)) = classes.class_of(ty, file) {
        if classes.is_strict(&class, view, by_default) {
            return None;
        }
        return Some(classes.declares(&class, view, key) || infer.member(ty, name).is_some());
    }
    match classes.table_type_of(ty, file) {
        Some(Type::Shape(shape)) if shape.fields.is_empty() && shape.array.is_none() && shape.indices.is_empty() => {
            None
        }
        Some(table) => Some(classes.table_field_type(&table, file, key).is_some()),
        None => matches!(ty, Type::GlobalTable(_)).then(|| infer.member(ty, name).is_some()),
    }
}

/// The message for the field `name` set through the value of type `owner` written at `holder`.
/// A table that a local declares has no name of its own, so the expression that holds it names it.
fn message(infer: &Infer, name: &str, owner: &Type, holder: Span) -> String {
    let shown = owner.without_nil().to_string();
    if !shown.is_empty() && shown != "table" {
        return format!("Field `{name}` is not declared in `{shown}`");
    }
    let text = &infer.ctx.source[holder.start as usize..holder.end as usize];
    if text.len() > MAX_HOLDER_LEN || text.contains('\n') {
        return format!("Field `{name}` is not declared in its table");
    }
    format!("Field `{name}` is not declared in the table `{text}` holds")
}
