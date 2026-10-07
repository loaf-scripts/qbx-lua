//! `cast-type-mismatch`: a `---@cast name T` that gives a local a type its declared type does not
//! take, such as `string` for an `integer`. A local that nothing annotates and that is assigned
//! again has the declared types of the values that reach the cast. As for `assign-type-mismatch`,
//! each type that `T` lists has to be of a kind the declared type allows, and a literal has to be
//! one it lists. A class also has to be one that the declared type names or extends, as
//! lua-language-server checks it. Only plain casts are checked, since `+T` and `-T` add to the type
//! and take from it.

use qbx_lua_analysis::scope::LocalId;
use qbx_lua_syntax::ast::{ExprKind, StmtKind};
use qbx_lua_syntax::Span;

use super::class_tables::Classes;
use super::comparisons::Declared;
use crate::infer::{Decl, Infer};
use crate::luacats::CastEntry;
use crate::types::Type;

/// Each type of a `---@cast` line that the type its local is declared with does not take, with the
/// message naming both.
pub fn mismatched_casts(infer: &Infer) -> Vec<(Span, String)> {
    let casts = infer.ctx.casts();
    if casts.iter().next().is_none() {
        return Vec::new();
    }
    let classes = Classes::new(infer);
    let declared = Declared::keeping_annotations(infer);
    let mut out = Vec::new();
    for cast in casts.iter() {
        let Some(local) = cast.local else { continue };
        // A local that nothing annotates and that is assigned again holds the declared types of the
        // values that reach the cast.
        let expected = match declared.declaration(local) {
            Type::Unknown => declared.at(local, cast.span.start),
            declared => declared,
        };
        // `local data = nil` makes room for a value that the cast tells the type of.
        if expected.is_unknown() || expected == Type::Nil {
            continue;
        }
        // A local declared with a table constructor holds a table any class may describe, as
        // lua-language-server reads it.
        let holds_table = declared_with_table(infer, local);
        for (entry, span) in &cast.entries {
            let CastEntry::Replace(ty) = entry else { continue };
            // A cast that widens the type, as `---@cast coords Cell | Coords` for a `Coords`, takes
            // every value the local may hold, as TypeScript takes `x as A | B` for a `B`, although
            // lua-language-server reports it.
            if classes.rejected_part(ty, classes.file(), &expected).is_none() && takes_class(&classes, ty, &expected) {
                continue;
            }
            let parts = match ty {
                Type::Union(parts) => parts.as_slice(),
                one => std::slice::from_ref(one),
            };
            let rejects = |part: &Type| {
                classes.rejects(&expected, classes.file(), part)
                    || (!holds_table && !takes_class(&classes, &expected, part))
            };
            if parts.iter().any(rejects) {
                out.push((*span, format!("Cannot convert `{expected}` to `{ty}`")));
            }
        }
    }
    out
}

/// Whether a local declared as `declared` may be cast to `cast` as far as classes go: each class
/// that `cast` names needs a type of `declared` that is that class or one it extends, or that is no
/// class but may hold it, like `table` or `any`. Type arguments are not compared.
fn takes_class(classes: &Classes, declared: &Type, cast: &Type) -> bool {
    let file = classes.file();
    let (mut casts, mut parts) = (Vec::new(), Vec::new());
    classes.flatten(cast, file, &mut casts, 0);
    classes.flatten(declared, file, &mut parts, 0);
    parts.retain(|part| *part != Type::Nil);
    casts.iter().all(|cast| {
        let Some((class, _, view)) = classes.class_of(cast, file) else { return true };
        parts.iter().any(|part| match classes.class_of(part, file) {
            Some((declared, ..)) => classes.extends(&class, view, &declared),
            None => !classes.rejects(part, file, cast),
        })
    })
}

/// Whether `local` is declared with a table constructor, as `local item = {}` is.
fn declared_with_table(infer: &Infer, local: LocalId) -> bool {
    let decl = infer.ctx.resolution.local(local).decl.start;
    let Some(Decl::Local { stmt, index }) = infer.ctx.decl(decl) else { return false };
    let StmtKind::Local { exprs, .. } = &stmt.kind else { return false };
    exprs.get(*index).is_some_and(|value| matches!(value.unparen().kind, ExprKind::Table(_)))
}
