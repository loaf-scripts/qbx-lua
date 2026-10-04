//! `deprecated`: code reads a global, field or method whose definitions all carry `---@deprecated`,
//! as lua-language-server reports them: `OldName()`, `Lib.old()`, `value:old()` or `value["old"]`.
//! A `---@field` declares a field without defining it, so it neither counts as a definition nor
//! keeps the field from being deprecated. Locals are left alone, as LuaLS leaves them, and so are
//! the definitions in the bundled runtime stubs, which the linter reports.

use std::sync::Arc;

use qbx_lua_analysis::scope::GlobalRefKind;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::Span;

use super::class_tables::{accesses, Key};
use crate::index::{FileId, FileOrigin};
use crate::indexer::deprecation_reason;
use crate::infer::{Infer, MemberInfo};

/// Each read of a deprecated global, field or method, with the message naming it and the reason
/// its `---@deprecated` gives.
pub fn deprecated_uses(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    for global in infer.ctx.resolution.globals.iter().filter(|global| global.kind == GlobalRefKind::Read) {
        let symbols = infer.index.globals_named(&global.name, infer.ctx.file);
        let definitions = symbols.iter().map(|(file, symbol)| (Some(*file), symbol.deprecated, &symbol.doc));
        if let Some(reason) = deprecated(infer, definitions) {
            out.push((global.span, message(&global.name, &reason)));
        }
    }
    for access in accesses(infer, chunk, true) {
        let (Some(base), Key::Name(name)) = (access.read, &access.key) else { continue };
        let members = infer.members_named(&access.owner, name);
        // Almost no field is deprecated, so most reads end here.
        if !members.iter().any(|member| member.deprecated) {
            continue;
        }
        let defined = members.iter().filter(|member| !is_declared_field(infer, member));
        let definitions = defined.map(|member| (member.location.map(|(file, _)| file), member.deprecated, &member.doc));
        if let Some(reason) = deprecated(infer, definitions) {
            let between = &infer.ctx.source[base.span.end as usize..access.span.start as usize];
            let shown = match base.unparen().dotted_path() {
                Some(path) => format!("{path}{}{name}", if between.contains(':') { ':' } else { '.' }),
                None => name.to_string(),
            };
            out.push((access.span, message(&shown, &reason)));
        }
    }
    out
}

/// The reason given when every one of `definitions`, the file, deprecation and doc of each, is
/// deprecated, an empty one when none is given, or `None` when one is not or there are none.
fn deprecated<'d>(
    infer: &Infer,
    definitions: impl Iterator<Item = (Option<FileId>, bool, &'d Option<Arc<str>>)>,
) -> Option<String> {
    let mut reason = None;
    for (file, deprecated, doc) in definitions {
        let stub = file.and_then(|file| infer.index.file(file)).is_some_and(|entry| entry.origin == FileOrigin::Stub);
        if !deprecated || stub {
            return None;
        }
        reason = reason.or_else(|| Some(doc.as_deref().and_then(deprecation_reason).unwrap_or_default().to_string()));
    }
    reason
}

/// Whether a `---@field` of a class declares `member`, rather than code defining it.
fn is_declared_field(infer: &Infer, member: &MemberInfo) -> bool {
    let Some((file, range)) = member.location else { return true };
    let Some(entry) = infer.index.file(file) else { return true };
    let fields = entry.index.classes.iter().flat_map(|class| &class.fields);
    fields.into_iter().any(|field| field.name == member.name && field.range == range)
}

fn message(name: &str, reason: &str) -> String {
    match reason {
        "" => format!("'{name}' is deprecated"),
        reason => format!("'{name}' is deprecated: {reason}"),
    }
}
