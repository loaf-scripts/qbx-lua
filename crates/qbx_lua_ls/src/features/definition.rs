use std::cell::OnceCell;

use lsp_types::{GotoDefinitionResponse, Location, Position, Range};
use qbx_lua_syntax::ast::{ExprKind, StmtKind};
use qbx_lua_syntax::SmolStr;

use super::class_tables::Classes;
use super::hover::{event_string_context, target_at, Target};
use super::member_refs::member_target;
use super::with_infer;
use crate::document::Document;
use crate::index::{EventFamily, EventKind, FileId, FileOrigin};
use crate::infer::{Decl, Infer};
use crate::locate::locate;
use crate::luacats::applies_on;
use crate::types::Type;
use crate::workspace::Workspace;

const REQUIRE_CALLS: &[&str] = &["require", "lib.require", "lib.load"];

fn location(ws: &Workspace, file: FileId, range: Range) -> Option<Location> {
    let entry = ws.index.file(file).filter(|f| f.origin != FileOrigin::Stub)?;
    Some(Location::new(entry.uri.clone(), range))
}

fn string_definition(ws: &Workspace, infer: &Infer, doc: &Document, offset: u32) -> Vec<Location> {
    let located = locate(&doc.chunk, offset);
    let Some((string, call)) = located.string else { return Vec::new() };
    let ExprKind::String(value) = &string.kind else { return Vec::new() };

    let callee = call.and_then(|(call, _)| match &call.kind {
        ExprKind::Call { callee, .. } => callee.dotted_path(),
        _ => None,
    });
    if callee.as_deref() == Some("locale") {
        let locale = ws.index.resource_of(doc.file).and_then(|r| qbx_lua_analysis::locale::LocaleFile::load(&r.root));
        let Some(locale) = locale else { return Vec::new() };
        let lines = qbx_lua_syntax::LineIndex::new(&locale.source);
        return locale
            .keys
            .iter()
            .filter(|(key, ..)| key == value.as_str())
            .map(|(_, span, _)| {
                let range = crate::indexer::span_to_range(&locale.source, &lines, *span);
                Location::new(crate::workspace::path_to_uri(&locale.path), range)
            })
            .collect();
    }
    if callee.as_deref().is_some_and(|path| REQUIRE_CALLS.contains(&path)) {
        return ws
            .index
            .resolve_require(value, doc.file)
            .and_then(|file| location(ws, file, Range::default()))
            .into_iter()
            .collect();
    }
    // A registration is its own definition. Editors such as VS Code then show its references
    // instead, which list the calls that trigger it.
    let range = doc.range(string.span);
    let own_events = ws.index.file(doc.file).map(|entry| entry.index.events.as_slice()).unwrap_or_default();
    if own_events.iter().any(|event| event.kind != EventKind::Trigger && event.range == range) {
        return vec![Location::new(doc.uri.clone(), range)];
    }
    let context = event_string_context(infer, call);
    ws.index
        .events()
        .filter(|(_, e)| e.name == *value && e.kind != EventKind::Trigger)
        .filter(|(_, event)| match &context {
            Some(context) => context.accepts_registration(event),
            None => matches!(event.family, EventFamily::Native | EventFamily::OxLib),
        })
        .filter_map(|(file, event)| location(ws, file, event.range))
        .collect()
}

/// Every registration and trigger of the event or callback name under the cursor, within its family:
/// `RegisterNetEvent('shop:buy')` finds each `TriggerServerEvent('shop:buy', ...)`, and the reverse.
/// `None` when the cursor is not on such a name.
pub fn event_references(
    ws: &Workspace,
    doc: &Document,
    offset: u32,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    with_infer(ws, doc, |infer| {
        let (string, call) = locate(&doc.chunk, offset).string?;
        let ExprKind::String(value) = &string.kind else { return None };
        let family = event_string_context(infer, call)?.family;
        let found = ws
            .index
            .events()
            .filter(|(_, event)| event.name == *value && event.family == family)
            .filter(|(_, event)| include_declaration || event.kind == EventKind::Trigger)
            .filter_map(|(file, event)| location(ws, file, event.range));
        Some(found.collect())
    })
}

pub fn definition(ws: &Workspace, doc: &Document, position: Position) -> Option<GotoDefinitionResponse> {
    let offset = doc.offset(position);
    let locations = with_infer(ws, doc, |infer| match target_at(infer, doc, offset) {
        Some(Target::Local(id, _)) => {
            let local = doc.resolution.local(id);
            vec![Location::new(doc.uri.clone(), doc.range(local.decl))]
        }
        Some(Target::Global(name, _)) => ws
            .index
            .globals_named(&name, doc.file)
            .into_iter()
            .filter_map(|(file, symbol)| location(ws, file, symbol.range))
            .collect(),
        Some(Target::Member { info, .. }) => {
            info.location.and_then(|(file, range)| location(ws, file, range)).into_iter().collect()
        }
        Some(Target::Type(name, _)) => {
            let classes = ws.index.class_defs(&name).into_iter().map(|(file, class)| (file, class.range));
            let aliases = ws.index.alias_defs(&name).into_iter().map(|(file, alias)| (file, alias.range));
            classes.chain(aliases).filter_map(|(file, range)| location(ws, file, range)).collect()
        }
        None => string_definition(ws, infer, doc, offset),
    });
    (!locations.is_empty()).then_some(GotoDefinitionResponse::Array(locations))
}

/// The declarations of the type of the value under the cursor: the `---@class`, `---@alias` or
/// `---@enum` of each class and alias it names, through `?`, unions, arrays, the keys and values of
/// `table<K, V>` and the arguments of generic classes, and the `---@enum` of an enum's own table. A
/// function is typed by what it returns, so `GetPlayer` in `GetPlayer()` leads to the class of the
/// player. Built-in types, the handles of natives such as `Vehicle`, which are integers, and tables
/// without a class have none.
pub fn type_definition(ws: &Workspace, doc: &Document, position: Position) -> Option<GotoDefinitionResponse> {
    let offset = doc.offset(position);
    let locations = with_infer(ws, doc, |infer| {
        let target = target_at(infer, doc, offset)?;
        let origin = OnceCell::new();
        let ty = match &target {
            Target::Local(id, _) => infer.local_type_at(*id, offset),
            Target::Global(name, _) => infer.global_type(name),
            Target::Member { info, .. } => info.ty.clone(),
            Target::Type(name, _) => Type::Named(name.clone(), Vec::new()),
        };
        let mut named = Vec::new();
        named_types(infer, &ty, &mut named, 0);
        let classes = Classes::new(infer);
        let mut locations: Vec<Location> = Vec::new();
        for named in named {
            let ranges: Vec<(FileId, Range)> = match named {
                Named::Type(name) => {
                    let (mut found_classes, mut found_aliases) = classes.declarations(&name, doc.file);
                    // A name the file cannot tell apart is read the way the file the value comes from
                    // sees it: qbx_core's `Player` for what its `GetPlayer` export returns.
                    if found_classes.is_empty() && found_aliases.is_empty() {
                        if let Some(origin) = *origin.get_or_init(|| origin_file(ws, infer, doc, &target, 0)) {
                            (found_classes, found_aliases) = classes.declarations(&name, origin);
                        }
                    }
                    // Declarations that neither sees, or that several resources declare, are all shown.
                    if found_classes.is_empty() && found_aliases.is_empty() {
                        found_classes = ws.index.class_defs(&name);
                        found_classes.retain(|(_, class)| applies_on(class.side, infer.side()));
                        found_aliases = ws.index.alias_defs(&name);
                        found_aliases.retain(|(_, alias)| applies_on(alias.side, infer.side()));
                    }
                    let ranges = found_classes.iter().map(|(file, class)| (*file, class.range));
                    ranges.chain(found_aliases.iter().map(|(file, alias)| (*file, alias.range))).collect()
                }
                Named::EnumTable(owner) => {
                    let mut enums = ws.index.enums_of_table(&owner);
                    enums.retain(|(_, alias)| applies_on(alias.side, infer.side()));
                    if enums.iter().any(|(file, _)| ws.index.is_visible(doc.file, *file)) {
                        enums.retain(|(file, _)| ws.index.is_visible(doc.file, *file));
                    }
                    enums.iter().map(|(file, alias)| (*file, alias.range)).collect()
                }
            };
            for found in ranges.into_iter().filter_map(|(file, range)| location(ws, file, range)) {
                if !locations.contains(&found) {
                    locations.push(found);
                }
            }
        }
        Some(locations)
    })?;
    (!locations.is_empty()).then_some(GotoDefinitionResponse::Array(locations))
}

/// The file whose view of the type names the value under the cursor comes with: the one that
/// declares the member or global it is, or what a local is set to, like the `qbx_core` export of
/// `local player = exports.qbx_core:GetPlayer(source)`.
fn origin_file(ws: &Workspace, infer: &Infer, doc: &Document, target: &Target, depth: u32) -> Option<FileId> {
    match target {
        Target::Member { info, .. } => info.location.map(|(file, _)| file),
        Target::Global(name, _) => ws.index.globals_named(name, doc.file).first().map(|(file, _)| *file),
        Target::Local(id, _) if depth < 4 => {
            let local = infer.ctx.resolution.local(*id);
            let Some(Decl::Local { stmt, index }) = infer.ctx.decl(local.decl.start) else { return None };
            let StmtKind::Local { exprs, .. } = &stmt.kind else { return None };
            // The last value of `local a, b = f()` gives the rest of the names theirs.
            let value = exprs.get(*index).or_else(|| exprs.last())?.unparen();
            let callee = match &value.kind {
                ExprKind::Call { callee, .. } => callee.unparen(),
                _ => value,
            };
            let span = match &callee.kind {
                ExprKind::MethodCall { method, .. } => method.span,
                ExprKind::Name(name) | ExprKind::Field { name, .. } => name.span,
                _ => return None,
            };
            origin_file(ws, infer, doc, &target_at(infer, doc, span.start)?, depth + 1)
        }
        _ => None,
    }
}

/// What a type names that has a declaration.
enum Named {
    /// A class or alias.
    Type(SmolStr),
    /// The table of an `---@enum`, by the owner its fields are indexed under.
    EnumTable(SmolStr),
}

/// The classes, aliases and enum tables that `ty` names, in the order it names them.
fn named_types(infer: &Infer, ty: &Type, out: &mut Vec<Named>, depth: u32) {
    if depth > 8 {
        return;
    }
    match ty {
        Type::Named(name, args) => {
            if !out.iter().any(|known| matches!(known, Named::Type(known) if known == name)) {
                out.push(Named::Type(name.clone()));
                aliased_types(infer, name, out, depth);
            }
            args.iter().for_each(|arg| named_types(infer, arg, out, depth + 1));
        }
        Type::GlobalTable(owner) => {
            if !out.iter().any(|known| matches!(known, Named::EnumTable(known) if known == owner)) {
                out.push(Named::EnumTable(owner.clone()));
            }
        }
        Type::Union(types) | Type::Tuple(types) => {
            types.iter().for_each(|part| named_types(infer, part, out, depth + 1))
        }
        Type::Array(inner) | Type::Variadic(inner) => named_types(infer, inner, out, depth + 1),
        Type::Map(key, value) => {
            named_types(infer, key, out, depth + 1);
            named_types(infer, value, out, depth + 1);
        }
        Type::Fun(fun) => fun.returns.iter().for_each(|ty| named_types(infer, ty, out, depth + 1)),
        Type::Require(_) => {
            let resolved = infer.resolve_alias(ty);
            if resolved != *ty {
                named_types(infer, &resolved, out, depth + 1);
            }
        }
        _ => {}
    }
}

/// The classes and aliases that the alias `name` stands for: the one it names, or each that a union
/// or a `?` of it names, as `Dog` and `Animal` for `---@alias Pet Dog|Animal`. The types an array
/// or a table type of it holds are not its own, as lua-language-server reads them.
fn aliased_types(infer: &Infer, name: &str, out: &mut Vec<Named>, depth: u32) {
    if infer.index.class(name, infer.side()).is_some() {
        return;
    }
    let Some((_, alias)) = infer.index.alias(name, infer.side()) else { return };
    let parts = match &alias.ty {
        Type::Union(parts) => parts.as_slice(),
        one => std::slice::from_ref(one),
    };
    for part in parts.iter().filter(|part| matches!(part, Type::Named(..))) {
        named_types(infer, part, out, depth + 1);
    }
}

/// Where code sets the field or method under the cursor: `function Class:name()`, `Class.name =
/// value`, `self.name = value` and the keys of table constructors, without the `---@field` lines
/// that declare it, so a method declared in a definition file leads to its body. Other names, and
/// members that no code the file sees sets, like the exports a resource registers, go to their
/// definition.
pub fn implementation(ws: &Workspace, doc: &Document, position: Position) -> Option<GotoDefinitionResponse> {
    let Some(target) = member_target(ws, doc, doc.offset(position)) else { return definition(ws, doc, position) };
    let locations: Vec<Location> = target
        .implementations(ws, doc.file)
        .into_iter()
        .filter_map(|(file, range)| location(ws, file, range))
        .collect();
    if locations.is_empty() {
        return definition(ws, doc, position);
    }
    Some(GotoDefinitionResponse::Array(locations))
}
