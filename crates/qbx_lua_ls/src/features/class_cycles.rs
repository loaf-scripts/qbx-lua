//! `circle-doc-class`: a `---@class` that inherits from itself, directly as `---@class A : A` does, or
//! through its parents, as `---@class A : B` with `---@class B : A` does. Parents declared in other
//! files count, as the index has them, so each declaration on the cycle is reported in its own file.

use qbx_lua_analysis::env::doc_blocks;
use qbx_lua_syntax::ast::Chunk;
use qbx_lua_syntax::{SmolStr, Span};
use rustc_hash::FxHashSet;

use super::class_tables::Classes;
use crate::index::FileId;
use crate::infer::Infer;
use crate::luacats::{declared_name, parse_doc_lines};
use crate::types::Type;

/// How many classes deep a chain of parents is followed.
const MAX_DEPTH: u32 = 32;

/// Each `---@class` line of `chunk` whose class inherits from itself, with the message naming the
/// classes in between.
pub fn circular_classes(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let source = infer.ctx.source;
    if !source.contains("@class") {
        return Vec::new();
    }
    let classes = Classes::new(infer);
    let mut out = Vec::new();
    for block in doc_blocks(source, &chunk.comments) {
        let lines: Vec<&str> = block.iter().map(|comment| &comment.span.text(source)[3..]).collect();
        let doc = parse_doc_lines(&lines);
        for class in &doc.classes {
            let mut visited = FxHashSet::default();
            let Some(path) = class
                .parent_types
                .iter()
                .find_map(|parent| path_to(&classes, parent, classes.file(), &class.name, &mut visited, 0))
            else {
                continue;
            };
            let Some((offset, name)) = declared_name(lines[class.line]) else { continue };
            let start = block[class.line].span.start + 3 + offset as u32;
            let message = match path.split_last() {
                None => format!("Class `{name}` inherits from itself"),
                Some((last, [])) => format!("Class `{name}` inherits from itself through `{last}`"),
                Some((last, before)) => {
                    let before: Vec<String> = before.iter().map(|class| format!("`{class}`")).collect();
                    format!("Class `{name}` inherits from itself through {} and `{last}`", before.join(", "))
                }
            };
            out.push((Span::new(start, start + name.len() as u32), message));
        }
    }
    out
}

/// The classes through which the class `parent`, as `from` sees it, inherits from `target`, starting
/// with `parent` itself and empty when it is `target`, or `None` when it does not inherit from it.
fn path_to(
    classes: &Classes,
    parent: &Type,
    from: FileId,
    target: &str,
    visited: &mut FxHashSet<SmolStr>,
    depth: u32,
) -> Option<Vec<SmolStr>> {
    let (name, _, view) = classes.class_of(parent, from)?;
    if name == target {
        return Some(Vec::new());
    }
    if depth > MAX_DEPTH || !visited.insert(name.clone()) {
        return None;
    }
    for (file, def) in classes.class_defs(&name, view) {
        for grandparent in &def.parent_types {
            if let Some(mut path) = path_to(classes, grandparent, file, target, visited, depth + 1) {
                path.insert(0, name);
                return Some(path);
            }
        }
    }
    None
}
