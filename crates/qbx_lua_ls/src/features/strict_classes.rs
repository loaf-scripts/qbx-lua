//! `undeclared-field`: fields and keys a strict class does not declare. A class is strict when it
//! says `---@class (strict) Name` or LuaLS's `(exact)`, or when `strict_classes` in qbxlint.toml
//! makes every class without `(loose)` strict. Table constructors typed as the class, assignments
//! through values of it and reads from them may only use its declared fields, including those
//! keyed by a literal like `---@field [1] number`, and keys of a type one of its indexes takes. The
//! table the `---@class` annotation declares may still take fields and methods, which then count
//! as declared.

use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::Span;

use super::class_tables::{accesses, class_tables, entries, Classes, Key};
use crate::index::FileId;
use crate::infer::Infer;

/// Each field or key that a table constructor, an assignment or a read uses on a strict class that
/// does not declare it, with the message naming both. `by_default` tells whether `strict_classes`
/// applies to a class declared in a file.
pub fn undeclared_fields(infer: &Infer, chunk: &Chunk, by_default: impl Fn(FileId) -> bool) -> Vec<(Span, String)> {
    let classes = Classes::new(infer);
    let mut out = Vec::new();
    for found in class_tables(infer, chunk) {
        let ExprKind::Table(fields) = &found.table.kind else { continue };
        if !classes.is_strict(&found.class, found.from, &by_default) {
            continue;
        }
        for (key, span, _) in entries(infer, fields) {
            if !classes.declares(&found.class, found.from, &key) {
                out.push((span, message(&key, &found.class)));
            }
        }
    }
    let file = classes.file();
    for access in accesses(infer, chunk, true).into_iter().filter(|access| !access.on_class_table) {
        let Some((class, _, view)) = classes.class_of(&access.owner, file) else { continue };
        if classes.is_strict(&class, view, &by_default) && !classes.declares(&class, view, &access.key) {
            out.push((access.span, message(&access.key, &class)));
        }
    }
    out
}

fn message(key: &Key, class: &str) -> String {
    match key.field() {
        Some(field) => format!("Field `{field}` is not declared in strict class `{class}`"),
        None => format!("Strict class `{class}` has no `{}` keys", key.ty()),
    }
}
