//! `undefined-doc-name`: a LuaCATS annotation names a type that no `@class`, `@alias` or `@enum`
//! declares, or that only `(server)` or `(client)` declarations of the other side declare. Only the
//! language server indexes the declarations, so qbx-lint registers the rule and this module reports it.

use qbx_fivem_data::Side;
use qbx_lua_analysis::env::doc_blocks;
use qbx_lua_syntax::ast::Chunk;
use qbx_lua_syntax::{Comment, Span};

use crate::index::Index;
use crate::infer::NATIVE_HANDLE_TYPES;
use crate::luacats::{declared_generics, referenced_type_names};

/// Each type name in the doc comments of `chunk` that nothing declares for code on `side`, with its
/// message.
pub fn undefined_doc_names(index: &Index, source: &str, chunk: &Chunk, side: Option<Side>) -> Vec<(Span, String)> {
    let blocks = doc_blocks(source, &chunk.comments);
    let line = |comment: &Comment| (comment.span.start + 3, &comment.span.text(source)[3..]);
    // A generic class passes its parameters on to the methods declared for it elsewhere in the file.
    let class_generics: Vec<&str> = blocks
        .iter()
        .flatten()
        .map(|comment| line(comment).1)
        .filter(|text| text.trim_start().starts_with("@class"))
        .flat_map(declared_generics)
        .collect();
    let known = |name: &str| {
        name == "self"
            || NATIVE_HANDLE_TYPES.contains(&name)
            || index.class(name, side).is_some()
            || index.alias(name, side).is_some()
    };
    // The side of a declaration that exists, but not for code on `side`.
    let declared_side = |name: &str| {
        let classes = index.class_defs(name).into_iter().map(|(_, class)| class.side);
        let aliases = index.alias_defs(name).into_iter().map(|(_, alias)| alias.side);
        classes.chain(aliases).flatten().next()
    };
    let mut out = Vec::new();
    for block in &blocks {
        let lines: Vec<(u32, &str)> = block.iter().map(|comment| line(comment)).collect();
        let generics: Vec<&str> = lines.iter().flat_map(|(_, text)| declared_generics(text)).collect();
        for (start, text) in &lines {
            for (offset, name) in referenced_type_names(text) {
                if generics.contains(&name) || class_generics.contains(&name) || known(name) {
                    continue;
                }
                let from = start + offset as u32;
                let message = match (declared_side(name), side) {
                    (Some(declared), Some(side)) => format!(
                        "Type `{name}` only exists on the {}, but this is a {} script",
                        declared.label(),
                        side.label()
                    ),
                    _ => format!("Undefined type or alias `{name}`"),
                };
                out.push((Span::new(from, from + name.len() as u32), message));
            }
        }
    }
    out
}
