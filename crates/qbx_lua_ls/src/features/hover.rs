use std::sync::Arc;

use lsp_types::{Hover, HoverContents, HoverParams, Position};
use qbx_fivem_data::{native, native_docs, Side};
use qbx_lua_analysis::scope::{LocalId, LocalKind, Resolved};
use qbx_lua_syntax::ast::{Expr, ExprKind};
use qbx_lua_syntax::{CommentKind, SmolStr, Span};
use serde::{Deserialize, Serialize};

use super::{lua_block, markdown, with_infer};
use crate::callback_wrappers::{source_skip, target_of, Wrapper};
use crate::document::Document;
use crate::index::{
    instance_class, AliasDef, ClassDef, EventDef, EventFamily, EventKind, FileId, FileOrigin, SymbolKind,
};
use crate::indexer::{described_values, render_doc};
use crate::infer::{Decl, Infer, MemberInfo};
use crate::locate::{locate, MemberAccess};
use crate::luacats::{applies_on, own_type, type_name_at};
use crate::types::{CallbackRole, DescribedValue, Type, TypeName};
use crate::workspace::Workspace;

pub enum Target {
    Local(LocalId, Span),
    Global(SmolStr, Span),
    Member { info: MemberInfo, owner: Type, span: Span },
    Type(SmolStr, Span),
}

impl Target {
    pub fn span(&self) -> Span {
        match self {
            Target::Local(_, span) | Target::Global(_, span) | Target::Member { span, .. } => *span,
            Target::Type(_, span) => *span,
        }
    }
}

/// A class or alias named in the doc comment under the cursor.
fn annotation_type_at(doc: &Document, offset: u32) -> Option<(SmolStr, Span)> {
    let comment = doc.chunk.comments.iter().find(|c| c.span.contains_inclusive(offset))?;
    let content = comment.content.text(&doc.text);
    // The content of a `---` line starts at its third dash. `--[[@as T]]` is the only long form.
    let line = match comment.kind {
        CommentKind::Line => content.strip_prefix('-')?,
        CommentKind::Long if content.starts_with("@as") => content,
        _ => return None,
    };
    let line_start = comment.content.end - line.len() as u32;
    let (start, name) = type_name_at(line, offset.checked_sub(line_start)? as usize)?;
    let start = line_start + start as u32;
    Some((SmolStr::new(name), Span::new(start, start + name.len() as u32)))
}

pub fn target_at(infer: &Infer, doc: &Document, offset: u32) -> Option<Target> {
    if let Some((resolved, span)) = doc.resolution.resolved_at_offset(offset) {
        return Some(match resolved {
            Resolved::Local(id) => Target::Local(id, span),
            Resolved::Global(index) => Target::Global(doc.resolution.globals[index as usize].name.clone(), span),
        });
    }
    let located = locate(&doc.chunk, offset);
    if let Some(access) = located.member {
        let owner = infer.expr(access.base());
        let name = access.name(&doc.text)?;
        let mut info = infer.member(&owner, &name.text)?;
        // What the guards around a field of a local leave of it, as `self.target` inside `if self.target then`.
        if !matches!(access, MemberAccess::Method { .. }) {
            info.ty = infer.member_narrowed(access.base(), &name.text, info.ty);
        }
        return Some(Target::Member { info, owner, span: name.span });
    }
    if let Some((func_name, segment)) = located.func_name.filter(|(_, segment)| *segment > 0) {
        let segments: Vec<_> = func_name.path.iter().chain(&func_name.method).collect();
        let mut owner = infer.func_name_owner_type(&qbx_lua_syntax::ast::FuncName {
            base: func_name.base.clone(),
            path: Vec::new(),
            method: None,
            span: func_name.base.span,
        });
        for name in &segments[..segment - 1] {
            owner = infer.member(&owner, &name.text)?.ty;
        }
        let name = segments[segment - 1];
        let info = infer.member(&owner, &name.text)?;
        return Some(Target::Member { info, owner, span: name.span });
    }
    annotation_type_at(doc, offset).map(|(name, span)| Target::Type(name, span))
}

const MAX_OVERVIEW_FIELDS: usize = 14;

/// The deepest level of a hover. Level 0 writes the type of a value alone, 1 the fields of its
/// tables and the aliases they use, and each level above lists more fields and writes out one more
/// step of the tables and classes they hold.
pub const MAX_LEVEL: u32 = 5;

/// The level a hover is written at, and what writing it finds out about the other levels: the
/// lowest one that writes the same, and whether a higher one writes more. Together they make the
/// `maxLevel` that lua-language-server answers a hover request with.
struct Detail {
    level: u32,
    same_from: u32,
    more: bool,
}

impl Detail {
    fn new(level: u32) -> Self {
        Self { level: level.min(MAX_LEVEL), same_from: 0, more: false }
    }

    /// Whether what hovers write from `level` on is written at this level.
    fn reaches(&mut self, level: u32) -> bool {
        if self.level < level {
            self.more = true;
            return false;
        }
        self.same_from = self.same_from.max(level);
        true
    }

    /// How many of `count` items a list that hovers write from level `from` on holds: `per` at that
    /// level and `per·n²` at the `n`th, as lua-language-server grows its table overviews.
    fn take(&mut self, from: u32, count: usize, per: usize) -> usize {
        let cap = |levels: u32| per * (levels as usize).pow(2);
        let held = cap(self.level + 1 - from);
        if count > held {
            self.more = true;
            self.same_from = self.level;
            return held;
        }
        let needed = (1..).find(|&levels| cap(levels) >= count).unwrap_or(1);
        self.same_from = self.same_from.max(from + needed - 1);
        count
    }

    /// How many items to collect for a list that hovers write from level `from` on: one more than
    /// this level writes, which tells `take` whether there are more.
    fn room(&self, from: u32, per: usize) -> usize {
        match (self.level + 1).checked_sub(from) {
            Some(levels) if levels > 0 => per * (levels as usize).pow(2) + 1,
            _ => 1,
        }
    }

    /// The deepest level that writes more than the one below it.
    fn max_level(&self) -> u32 {
        if self.more {
            (self.level + 1).min(MAX_LEVEL)
        } else {
            self.same_from
        }
    }
}

/// `name: type`, a function signature, or for tables an overview of the fields that are in scope
/// for this file, with the literal values the index remembered. Aliases follow on their own lines,
/// then from level 2 on the classes the hover names. The signature of a function that may yield
/// starts with `(async)`, as in lua-language-server.
fn describe_value(
    infer: &Infer,
    detail: &mut Detail,
    prefix: &str,
    name: &str,
    ty: &Type,
    literal: Option<&str>,
) -> String {
    if let (Some(fun), Type::Fun(_)) = (ty.as_fun(), ty) {
        let marker = if fun.is_async { "(async) " } else { "" };
        let mut out = format!("{marker}{prefix}{}", fun.signature(name));
        let room = detail.room(1, MAX_NESTED_ALIASES);
        let aliases = alias_lines(detail, 1, used_aliases(infer, [ty], &[], false, room));
        push_aliases(&mut out, &aliases);
        let mut listed = names(&aliases);
        let mut classes = Vec::new();
        used_classes(infer, [ty], false, &listed, &mut classes);
        push_classes(infer, detail, &mut out, &mut listed, classes, 2);
        return out;
    }
    let (mut out, shown) = value_overview(infer, detail, prefix, name, ty, literal);
    let mut aliases = alias_expansions(infer, ty);
    let skip = names(&aliases);
    if !aliases.is_empty() && !detail.reaches(1) {
        aliases.clear();
    }
    push_aliases(&mut out, &aliases);
    // Then those that its type arguments and the fields it lists use, as `Mode` of `Box<Mode>` or of
    // `Car { mode: Mode }`.
    let room = detail.room(1, MAX_NESTED_ALIASES);
    let mut nested = used_aliases(infer, [ty], &skip, true, room);
    let mut listed = [skip, names(&nested)].concat();
    nested.extend(used_aliases(infer, shown.iter().flatten(), &listed, false, room));
    let nested = alias_lines(detail, 1, nested);
    push_aliases(&mut out, &nested);
    listed.extend(names(&nested));
    // A table that the overview lists the fields of has its own class written out already.
    let mut classes = Vec::new();
    used_classes(infer, [ty], shown.is_some(), &listed, &mut classes);
    used_classes(infer, shown.iter().flatten(), false, &listed, &mut classes);
    push_classes(infer, detail, &mut out, &mut listed, classes, 2);
    out
}

fn names(aliases: &[(SmolStr, &AliasDef)]) -> Vec<SmolStr> {
    aliases.iter().map(|(name, _)| name.clone()).collect()
}

/// Writes each alias out on a line of its own after `out`.
fn push_aliases(out: &mut String, aliases: &[(SmolStr, &AliasDef)]) {
    for (name, alias) in aliases {
        out.push('\n');
        out.push_str(&alias_definition(name, alias));
    }
}

/// The aliases that hovers write out from level `from` on, as many as this level writes.
fn alias_lines<'a>(
    detail: &mut Detail,
    from: u32,
    mut aliases: Vec<(SmolStr, &'a AliasDef)>,
) -> Vec<(SmolStr, &'a AliasDef)> {
    if aliases.is_empty() || !detail.reaches(from) {
        return Vec::new();
    }
    aliases.truncate(detail.take(from, aliases.len(), MAX_NESTED_ALIASES));
    aliases
}

/// Writes out the classes a hover names from level `from` on, each once after the lines before
/// it, followed by the aliases their fields use. The classes those fields name follow one level
/// later, and so on.
fn push_classes<'a>(
    infer: &Infer<'a>,
    detail: &mut Detail,
    out: &mut String,
    listed: &mut Vec<SmolStr>,
    mut classes: Vec<(SmolStr, &'a ClassDef)>,
    mut from: u32,
) {
    while !classes.is_empty() && detail.reaches(from) {
        classes.truncate(detail.take(from, classes.len(), MAX_NESTED_ALIASES));
        listed.extend(classes.iter().map(|(name, _)| name.clone()));
        let mut shown = Vec::new();
        for (_, class) in &classes {
            let (block, types) = class_block(infer, detail, class, from);
            out.push('\n');
            out.push_str(&block);
            shown.extend(types);
        }
        let room = detail.room(from, MAX_NESTED_ALIASES);
        let aliases = alias_lines(detail, from, used_aliases(infer, &shown, listed, false, room));
        push_aliases(out, &aliases);
        listed.extend(names(&aliases));
        classes = Vec::new();
        used_classes(infer, &shown, false, listed, &mut classes);
        from += 1;
    }
}

/// `name: type`, or for tables an overview of their fields from level 1 on, with the types of the
/// fields it lists. The types are `None` for a value that is no table.
fn value_overview(
    infer: &Infer,
    detail: &mut Detail,
    prefix: &str,
    name: &str,
    ty: &Type,
    literal: Option<&str>,
) -> (String, Option<Vec<Type>>) {
    let bare = ty.without_nil();
    let owner = table_part(infer, &bare, 0);
    let members = infer.members(&owner);
    if members.is_empty() {
        let value = literal.map(|l| format!(" = {l}")).unwrap_or_default();
        return (format!("{prefix}{name}: {}{value}", shown_type(infer, ty)), None);
    }
    // The fields are those of the value when it is not nil; the `?` still says it may be.
    let optional = if *ty != bare && matches!(ty, Type::Union(types) if types.contains(&Type::Nil)) { "?" } else { "" };
    // A class with its type arguments, as `List<string>`, or the types a value of several may be,
    // as `number|ArrayLike<number>`, unless one is a table whose fields only the overview names.
    let label = match &bare {
        Type::Named(..) => format!("{bare}{optional} "),
        Type::Union(types) if !types.iter().any(is_table_value) => format!("{ty} "),
        _ => String::new(),
    };
    if !detail.reaches(1) {
        let label = if label.is_empty() { format!("table{optional}") } else { label.trim_end().to_string() };
        return (format!("{prefix}{name}: {label}"), Some(Vec::new()));
    }
    let tables = match &bare {
        Type::GlobalTable(path) => vec![path.clone()],
        _ => Vec::new(),
    };
    let out = format!("{prefix}{name}: {label}{{");
    let mut overview = Overview { infer, detail, out, shown: Vec::new(), tables };
    overview.fields(&owner, &members, 1, 1);
    let Overview { mut out, shown, .. } = overview;
    out.push_str(
        "
}",
    );
    if label.is_empty() {
        out.push_str(optional);
    }
    (out, Some(shown))
}

/// Writes out the fields of tables, collecting the types it shows. `tables` are the paths of the
/// tables it is inside of, which a field that holds one of them leaves at `table`.
struct Overview<'i, 'a> {
    infer: &'i Infer<'a>,
    detail: &'i mut Detail,
    out: String,
    shown: Vec<Type>,
    tables: Vec<SmolStr>,
}

impl Overview<'_, '_> {
    /// The fields of `owner` that hovers list from level `from` on, `indent` steps in. A table type
    /// that a field declares, as `{ [string]: Variation }`, is written out as lua-language-server
    /// writes it. A table the index only knows by its path is `table`, and its fields follow in
    /// place one level later, as do the signatures of functions.
    fn fields(&mut self, owner: &Type, members: &[MemberInfo], from: u32, indent: usize) {
        let pad = "    ".repeat(indent);
        let count = self.detail.take(from, members.len(), MAX_OVERVIEW_FIELDS);
        for member in &members[..count] {
            self.out.push_str(&format!(
                "
{pad}{}: ",
                member.name
            ));
            match &member.ty {
                Type::Fun(_) if !self.detail.reaches(from + 1) => self.out.push_str("function"),
                Type::GlobalTable(path) => self.table(path, from + 1, indent),
                // Tables that several files assign merge into a bare `table`, while reading the
                // field finds the one the index holds, as hovering it does.
                Type::Table => match self.infer.member(owner, &member.name).map(|info| info.ty) {
                    Some(Type::GlobalTable(path)) => self.table(&path, from + 1, indent),
                    _ => self.out.push_str("table"),
                },
                other => {
                    self.shown.push(other.clone());
                    self.out.push_str(&other.to_string());
                }
            }
            let value = member.literal.as_ref().map(|l| format!(" = {l}")).unwrap_or_default();
            self.out.push_str(&format!("{value},"));
        }
        if members.len() > count {
            self.out.push_str(&format!(
                "
{pad}...(+{})",
                members.len() - count
            ));
        }
    }

    /// A table that a field `indent` steps in holds, which the index knows by its `path`: `table`,
    /// then from level `from` on its fields, or the array type of a table with only integer keys.
    fn table(&mut self, path: &SmolStr, from: u32, indent: usize) {
        let ty = Type::GlobalTable(path.clone());
        let members = if self.tables.contains(path) { Vec::new() } else { self.infer.members(&ty) };
        if members.is_empty() {
            match shown_type(self.infer, &ty) {
                array @ Type::Array(_) if self.detail.reaches(from) => {
                    self.out.push_str(&array.to_string());
                    self.shown.push(array);
                }
                _ => self.out.push_str("table"),
            }
            return;
        }
        if !self.detail.reaches(from) {
            self.out.push_str("table");
            return;
        }
        self.out.push('{');
        self.tables.push(path.clone());
        self.fields(&ty, &members, from, indent + 1);
        self.tables.pop();
        self.out.push_str(&format!(
            "
{}}}",
            "    ".repeat(indent)
        ));
    }
}

/// Whether `ty` is a table that a hover shows by its fields alone, such as `{ name: string }` or
/// the table a local holds, rather than by a name.
fn is_table_value(ty: &Type) -> bool {
    matches!(ty, Type::Shape(_) | Type::GlobalTable(_) | Type::Require(_) | Type::Exports(_))
}

/// A table the index holds with only integer keys, such as `local list = { 'a', 'b' }` or
/// `Config.Items = { 'a', 'b' }`, is shown as `string[]` rather than as a bare `table`.
fn shown_type(infer: &Infer, ty: &Type) -> Type {
    match ty {
        Type::GlobalTable(_) => match infer.key_value_types(ty, false) {
            (Type::Integer, value) if !value.is_unknown() => Type::Array(Box::new(value)),
            _ => ty.clone(),
        },
        _ => ty.clone(),
    }
}

/// The parts of `ty` whose members a hover lists. `"male"|"female"`, or an alias of it, is shown as
/// itself rather than as the `string` library, and `Garage|string` lists only the `Garage` fields.
fn table_part(infer: &Infer, ty: &Type, depth: u32) -> Type {
    if depth > 8 {
        return Type::Unknown;
    }
    match infer.resolve_alias(ty) {
        Type::Union(types) => Type::union(types.iter().map(|t| table_part(infer, t, depth + 1))),
        Type::GlobalTable(_) | Type::Named(..) | Type::Shape(_) | Type::Require(_) | Type::Exports(Some(_)) => {
            ty.clone()
        }
        _ => Type::Unknown,
    }
}

/// The aliases in `ty` that stand for something other than a table, such as `"male"|"female"`,
/// expanded one layer the way a class is expanded into its fields.
fn alias_expansions<'a>(infer: &Infer<'a>, ty: &Type) -> Vec<(SmolStr, &'a AliasDef)> {
    let parts = match ty {
        Type::Union(types) => types.as_slice(),
        other => std::slice::from_ref(other),
    };
    let mut out: Vec<(SmolStr, &AliasDef)> = Vec::new();
    for part in parts {
        let part = match part {
            Type::Array(inner) => &**inner,
            other => other,
        };
        let Type::Named(name, _) = part else { continue };
        if infer.index.class(name, infer.side()).is_some() || out.iter().any(|(seen, _)| seen == name) {
            continue;
        }
        let Some((_, alias)) = infer.index.alias(name, infer.side()) else { continue };
        // `Result<string>` of `---@alias Result<T> T|nil` stands for `string?`, which is no table.
        if table_part(infer, part, 0).is_unknown() {
            out.push((name.text.clone(), alias));
        }
    }
    out
}

/// How many aliases a hover, or a parameter in signature help, writes out after the types that use
/// them, so that a class with many fields stays readable. Hovers write out as many classes from
/// level 2 on, and more of both at higher levels.
const MAX_NESTED_ALIASES: usize = 8;

/// The aliases that `types` use in the parameters and returns of functions, the fields of tables and
/// type arguments, as `Mode` in `fun(mode: Mode)` or `{ mode: Mode }`, each once and leaving out those
/// in `skip`. With `top`, the aliases the types name themselves are left out too, as a hover that
/// expands them already shows them. Classes are left to their own hovers.
pub(super) fn nested_aliases<'a, 't>(
    infer: &Infer<'a>,
    types: impl IntoIterator<Item = &'t Type>,
    skip: &[SmolStr],
    top: bool,
) -> Vec<(SmolStr, &'a AliasDef)> {
    used_aliases(infer, types, skip, top, MAX_NESTED_ALIASES)
}

/// The first `limit` of the aliases that `nested_aliases` finds.
fn used_aliases<'a, 't>(
    infer: &Infer<'a>,
    types: impl IntoIterator<Item = &'t Type>,
    skip: &[SmolStr],
    top: bool,
    limit: usize,
) -> Vec<(SmolStr, &'a AliasDef)> {
    let mut out: Vec<(SmolStr, &AliasDef)> = Vec::new();
    for ty in types {
        visit_names(ty, top, 0, &mut |name, top| {
            let listed = skip.iter().chain(out.iter().map(|(seen, _)| seen)).any(|seen| *seen == name.text);
            if top || listed || out.len() >= limit || infer.index.class(name, infer.side()).is_some() {
                return;
            }
            if let Some((_, alias)) = infer.index.alias(name, infer.side()) {
                out.push((name.text.clone(), alias));
            }
        });
    }
    out
}

/// Adds the classes that `types` name to `out`, as `Variation` of `{ [string]: Variation }` or of
/// `fun(variation: Variation)`, each once and leaving out those in `listed`, those without fields to
/// write out and those of the built-in library, such as `vector3`. With `top`, the classes the types
/// name themselves are left out too, as a hover that lists their fields already shows them.
fn used_classes<'a, 't>(
    infer: &Infer<'a>,
    types: impl IntoIterator<Item = &'t Type>,
    top: bool,
    listed: &[SmolStr],
    out: &mut Vec<(SmolStr, &'a ClassDef)>,
) {
    for ty in types {
        visit_names(ty, top, 0, &mut |name, top| {
            if top || listed.contains(&name.text) || out.iter().any(|(seen, _)| *seen == name.text) {
                return;
            }
            let Some((file, class)) = infer.index.class(name, infer.side()) else { return };
            let library = infer.index.file(file).is_some_and(|entry| entry.origin == FileOrigin::Stub);
            if !library && has_fields(infer, class) {
                out.push((name.text.clone(), class));
            }
        });
    }
}

/// Calls `visit` with each class or alias name in `ty`, after the type arguments given to it, and
/// with `top` for the names that `ty` is itself, alone or in a union or an array.
fn visit_names(ty: &Type, top: bool, depth: u32, visit: &mut impl FnMut(&TypeName, bool)) {
    if depth > 8 {
        return;
    }
    let mut inner = |ty: &Type, top: bool| visit_names(ty, top, depth + 1, visit);
    match ty {
        Type::Named(name, args) => {
            for arg in args {
                inner(arg, false);
            }
            visit(name, top);
        }
        Type::Union(types) => types.iter().for_each(|ty| inner(ty, top)),
        Type::Array(item) | Type::Variadic(item) => inner(item, top),
        Type::Tuple(types) => types.iter().for_each(|ty| inner(ty, false)),
        Type::Map(key, value) => {
            inner(key, false);
            inner(value, false);
        }
        Type::Fun(fun) => {
            fun.params.iter().for_each(|param| inner(&param.ty, false));
            fun.returns.iter().for_each(|ty| inner(ty, false));
        }
        Type::Shape(shape) => {
            shape.fields.iter().for_each(|field| inner(&field.ty, false));
            shape.array.iter().for_each(|ty| inner(ty, false));
            shape.indices.iter().for_each(|(key, value)| {
                inner(key, false);
                inner(value, false);
            });
        }
        _ => {}
    }
}

/// An alias as written out in a hover: `type Mode = "fast"|"slow"`, or with each value on a line of
/// its own when the `---|` lines that list them describe any.
pub(super) fn alias_definition(name: &str, alias: &AliasDef) -> String {
    let name = with_params(name, &alias.generics);
    if alias.values.iter().all(|listed| listed.description.is_empty()) {
        return format!("type {name} = {}", alias.ty);
    }
    let parts = match &alias.ty {
        Type::Union(types) => types.as_slice(),
        other => std::slice::from_ref(other),
    };
    let values: Vec<DescribedValue> = parts
        .iter()
        .map(|value| {
            let listed = alias.values.iter().find(|listed| listed.value == *value);
            let description = listed.map(|listed| listed.description.clone()).unwrap_or_default();
            DescribedValue { value: value.clone(), description }
        })
        .collect();
    format!("type {name} ={}", described_values(&values))
}

/// The name of a class or alias with the type parameters it declares, as `List<T>`.
fn with_params(name: &str, generics: &[SmolStr]) -> String {
    match generics {
        [] => name.to_string(),
        generics => format!("{name}<{}>", generics.join(", ")),
    }
}

/// `offset` is where the name is written, which decides what the guards around it rule out.
fn local_hover(infer: &Infer, detail: &mut Detail, id: LocalId, offset: u32, called: Option<Type>) -> String {
    let local = infer.ctx.resolution.local(id);
    let ty = called.unwrap_or_else(|| infer.local_type_at(id, offset));
    let prefix = match local.kind {
        LocalKind::Param => "(parameter) ",
        LocalKind::ImplicitSelf => "(self) ",
        LocalKind::LoopVar => "(loop variable) ",
        LocalKind::Local | LocalKind::LocalFunction => "local ",
    };
    let mut out = lua_block(&describe_value(infer, detail, prefix, &local.name, &ty, None));
    let doc = match infer.ctx.decl(local.decl.start) {
        Some(Decl::Local { stmt, .. } | Decl::LocalFunction { stmt, .. }) => {
            render_doc(&infer.ctx.doc_at(stmt.span.start))
        }
        Some(Decl::Param { doc_anchor: Some(anchor), .. }) => {
            infer.ctx.doc_at(*anchor).param_description(&local.name).map(Into::into)
        }
        _ => None,
    };
    if let Some(doc) = doc {
        out.push_str("\n\n");
        out.push_str(&doc);
    }
    out
}

fn native_hover(name: &str, side: Option<Side>) -> Option<String> {
    let native = native(name)?.on(side);
    let mut out = lua_block(&native.signature());
    let canonical = native.alias_of.map(|target| format!(" · alias of `{target}`")).unwrap_or_default();
    out.push_str(&format!(
        "\n\n*{} native* · `{}` · `{}`{canonical}",
        native.side.label(),
        native.namespace,
        native.hash
    ));
    if let Some(docs) = native_docs(name) {
        out.push_str("\n\n");
        out.push_str(&docs);
    }
    Some(out)
}

fn global_hover(
    ws: &Workspace,
    infer: &Infer,
    detail: &mut Detail,
    name: &str,
    called: Option<Type>,
) -> Option<String> {
    let symbols = ws.index.globals_named(name, infer.ctx.file);
    let Some((file, symbol)) = infer.preferred_global(&symbols) else {
        if let Some(hover) = native_hover(name, infer.side()) {
            return Some(hover);
        }
        let ty = infer.global_type(name);
        return (!ty.is_unknown()).then(|| lua_block(&describe_value(infer, detail, "(global) ", name, &ty, None)));
    };
    // Going through `global_type` merges the table with members other files of the resource add.
    let ty = match called.unwrap_or_else(|| infer.global_type(name)) {
        Type::Unknown => symbol.ty.clone(),
        resolved => resolved,
    };
    let mut out = lua_block(&describe_value(infer, detail, "(global) ", name, &ty, symbol.literal.as_deref()));
    if let Some(doc) = &symbol.doc {
        out.push_str("\n\n");
        out.push_str(doc);
    }
    if let Some(entry) = ws.index.file(*file).filter(|f| f.origin != FileOrigin::Stub && *file != infer.ctx.file) {
        let resource = entry.resource.and_then(|r| ws.index.resource(r));
        let location = match resource {
            Some(resource) => format!(
                "{}/{}",
                resource.name,
                qbx_lua_analysis::project::relative_slash_path(&resource.root, &entry.path)
            ),
            None => entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        };
        out.push_str(&format!("\n\n*defined in* `{location}`"));
    }
    Some(out)
}

fn member_hover(infer: &Infer, detail: &mut Detail, info: &MemberInfo, owner: &Type) -> String {
    let owner_label = owner_label(owner);
    let is_method = info.ty.as_fun().is_some_and(|f| f.is_method);
    let qualified = match (owner_label.is_empty(), is_method) {
        (true, _) => info.name.to_string(),
        (false, true) => format!("{owner_label}:{}", info.name),
        (false, false) => format!("{owner_label}.{}", info.name),
    };
    let prefix = if matches!(info.kind, SymbolKind::Field) && info.ty.as_fun().is_none() { "(field) " } else { "" };
    let mut out = lua_block(&describe_value(infer, detail, prefix, &qualified, &info.ty, info.literal.as_deref()));
    if info.deprecated {
        out.push_str("\n\n**Deprecated**");
    }
    if let Some(doc) = &info.doc {
        out.push_str("\n\n");
        out.push_str(doc);
    }
    out
}

/// How a member hover names the value of type `owner` the member is read from. An instance goes by
/// its class, as what `Zone:new()` returns is `Zone`, without the table it was made from, which
/// `setmetatable({ size = 1 }, Class)` keeps. A table of one file has no name to show.
fn owner_label(owner: &Type) -> String {
    let owner = owner.without_nil();
    let parts = match &owner {
        Type::Union(types) => types.as_slice(),
        other => std::slice::from_ref(other),
    };
    let is_instance = |part: &Type| match part {
        Type::GlobalTable(path) => instance_class(path).is_some(),
        other => matches!(other, Type::Named(..)),
    };
    let label = if parts.iter().any(is_instance) {
        let classes =
            parts.iter().filter(|part| !matches!(part, Type::Shape(_) | Type::Table)).map(|part| match part {
                Type::GlobalTable(path) => Type::GlobalTable(SmolStr::new(instance_class(path).unwrap_or(path))),
                other => other.clone(),
            });
        Type::union(classes)
    } else {
        owner
    };
    match label {
        Type::GlobalTable(path) if path.starts_with('%') => String::new(),
        // `Promise:New` of any `Promise<T>`, as lua-language-server names it.
        Type::Named(class, _) => class.to_string(),
        other => other.to_string(),
    }
}

/// The fields a class declares, as a hover lists them, with their types.
fn class_fields(infer: &Infer, class: &ClassDef) -> Vec<(String, Type)> {
    let members = infer.members(&own_type(&class.name, &class.generics));
    let mut fields: Vec<(String, Type)> =
        members.iter().map(|member| (format!("{}: {}", member.name, member.ty), member.ty.clone())).collect();
    fields.extend(class.literal_fields(infer.side()).map(|(key, value)| (format!("[{key}]: {value}"), value.clone())));
    fields
}

/// Whether a class has fields for a hover to list.
fn has_fields(infer: &Infer, class: &ClassDef) -> bool {
    !class_fields(infer, class).is_empty() || !class.indices(infer.side()).is_empty()
}

/// `(class) Name<T> : Parent`, with from level `from` on the fields it declares, and the types of
/// the fields it lists.
fn class_block(infer: &Infer, detail: &mut Detail, class: &ClassDef, from: u32) -> (String, Vec<Type>) {
    let mut declaration = format!("(class) {}", with_params(&class.name, &class.generics));
    if !class.parent_types.is_empty() {
        let parents: Vec<String> = class.parent_types.iter().map(Type::to_string).collect();
        declaration.push_str(&format!(" : {}", parents.join(", ")));
    }
    let fields = class_fields(infer, class);
    let indices = class.indices(infer.side());
    let mut shown: Vec<Type> = Vec::new();
    if (!fields.is_empty() || !indices.is_empty()) && detail.reaches(from) {
        declaration.push_str(" {");
        let count = detail.take(from, fields.len(), MAX_OVERVIEW_FIELDS);
        for (field, ty) in &fields[..count] {
            declaration.push_str(&format!("\n    {field},"));
            shown.push(ty.clone());
        }
        for (key, value) in &indices {
            declaration.push_str(&format!("\n    [{key}]: {value},"));
            shown.extend([(*key).clone(), (*value).clone()]);
        }
        if fields.len() > count {
            declaration.push_str(&format!("\n    ...(+{})", fields.len() - count));
        }
        declaration.push_str("\n}");
    }
    (declaration, shown)
}

fn class_hover(infer: &Infer, detail: &mut Detail, class: &ClassDef) -> String {
    let (mut declaration, shown) = class_block(infer, detail, class, 1);
    let parents = class.parent_types.iter().filter_map(|parent| match parent {
        Type::Named(_, args) => Some(args.iter()),
        _ => None,
    });
    let used: Vec<Type> = parents.flatten().cloned().chain(shown).collect();
    let mut listed = vec![class.name.clone()];
    let room = detail.room(1, MAX_NESTED_ALIASES);
    let aliases = alias_lines(detail, 1, used_aliases(infer, &used, &listed, false, room));
    push_aliases(&mut declaration, &aliases);
    listed.extend(names(&aliases));
    let mut classes = Vec::new();
    used_classes(infer, &used, false, &listed, &mut classes);
    push_classes(infer, detail, &mut declaration, &mut listed, classes, 2);
    let mut out = lua_block(&declaration);
    if let Some(doc) = &class.doc {
        out.push_str("\n\n");
        out.push_str(doc);
    }
    out
}

/// A class or alias, preferring declarations for this file's side, then workspace declarations over
/// the built-in library, then this file's over those of other files.
fn type_hover(infer: &Infer, detail: &mut Detail, name: &str) -> Option<String> {
    let preference = |file: FileId, side: Option<Side>| {
        (
            applies_on(side, infer.side()),
            infer.index.file(file).is_some_and(|entry| entry.origin != FileOrigin::Stub),
            file == infer.ctx.file,
        )
    };
    let classes = infer.index.class_defs(name).into_iter();
    if let Some((_, class)) = classes.max_by_key(|(file, class)| preference(*file, class.side)) {
        return Some(class_hover(infer, detail, class));
    }
    let aliases = infer.index.alias_defs(name).into_iter();
    let (_, alias) = aliases.max_by_key(|(file, alias)| preference(*file, alias.side))?;
    let mut definition = alias_definition(name, alias);
    let mut listed = vec![SmolStr::new(name)];
    let room = detail.room(1, MAX_NESTED_ALIASES);
    let aliases = alias_lines(detail, 1, used_aliases(infer, [&alias.ty], &listed, false, room));
    push_aliases(&mut definition, &aliases);
    listed.extend(names(&aliases));
    let mut classes = Vec::new();
    used_classes(infer, [&alias.ty], false, &listed, &mut classes);
    push_classes(infer, detail, &mut definition, &mut listed, classes, 2);
    let mut out = lua_block(&definition);
    if let Some(doc) = &alias.doc {
        out.push_str("\n\n");
        out.push_str(doc);
    }
    Some(out)
}

/// The `@overload` picked by the call whose function name is under the cursor, so hovering
/// `OnAction` in `OnAction('keyPressed', function(key) end)` shows the signature `key` is typed from.
fn called_overload(infer: &Infer, doc: &Document, offset: u32) -> Option<Type> {
    let site = locate(&doc.chunk, offset).callee?;
    let (fun, _) = infer.callee_fun(site.base, site.method)?;
    if fun.overloads.is_empty() {
        return None;
    }
    let picked = infer.call_signature(&fun, site.args, site.method.is_some(), site.base.span.start);
    (!Arc::ptr_eq(&picked, &fun)).then_some(Type::Fun(picked))
}

pub(super) struct EventStringContext {
    pub family: EventFamily,
    pub target_side: Option<Side>,
    pub active: bool,
}

impl EventStringContext {
    pub fn framework(&self) -> bool {
        matches!(self.family, EventFamily::QbCore | EventFamily::Esx)
    }

    pub fn accepts_registration(&self, event: &EventDef) -> bool {
        self.active
            && event.family == self.family
            && event.kind != EventKind::Trigger
            && (!self.framework() || (event.kind == EventKind::Callback && event.side == Some(Side::Server)))
    }
}

/// The name argument of a call to a `---@callback` wrapper, which may sit anywhere its annotations say.
fn wrapper_string_context(infer: &Infer, call: &Expr, arg_index: usize) -> Option<EventStringContext> {
    let (base, method) = match &call.kind {
        ExprKind::Call { callee, .. } => (&**callee, None),
        ExprKind::MethodCall { base, method, .. } => (&**base, Some(method)),
        _ => return None,
    };
    let (fun, _) = infer.callee_fun(base, method)?;
    let wrapper = Wrapper::of(&fun, method.is_some())?;
    if wrapper.arg(wrapper.name) != arg_index {
        return None;
    }
    let target_side = match wrapper.tag.role {
        CallbackRole::Register => None,
        CallbackRole::Await | CallbackRole::Trigger => target_of(infer.side_at(call.span.start)),
    };
    Some(EventStringContext { family: wrapper.family(), target_side, active: true })
}

/// A literal first argument is the only place framework callback names acquire special meaning.
pub(super) fn event_string_context(infer: &Infer, call: Option<(&Expr, usize)>) -> Option<EventStringContext> {
    let (call, arg_index) = call?;
    if let Some(context) = wrapper_string_context(infer, call, arg_index) {
        return Some(context);
    }
    if arg_index != 0 {
        return None;
    }
    let ExprKind::Call { callee, .. } = &call.kind else { return None };
    let own_side = || {
        let side = infer.index.file(infer.ctx.file).and_then(|file| file.side);
        qbx_lua_analysis::side_guard::SideRegions::of(infer.ctx.source, infer.ctx.chunk)
            .effective(call.span.start, side)
    };
    let path = callee.dotted_path();
    let (family, target_side) = match path.as_deref() {
        Some("TriggerServerEvent" | "TriggerLatentServerEvent") => (EventFamily::Native, Some(Side::Server)),
        Some("TriggerClientEvent" | "TriggerLatentClientEvent") => (EventFamily::Native, Some(Side::Client)),
        Some("TriggerEvent") => (EventFamily::Native, own_side()),
        Some("AddEventHandler" | "RegisterNetEvent" | "RegisterServerEvent") => (EventFamily::Native, None),
        Some("lib.callback" | "lib.callback.await") => {
            let target = match own_side() {
                Some(Side::Client) => Some(Side::Server),
                Some(Side::Server) => Some(Side::Client),
                _ => None,
            };
            (EventFamily::OxLib, target)
        }
        Some("lib.callback.register") => (EventFamily::OxLib, None),
        _ => {
            let framework = crate::framework_callbacks::classify(infer.ctx, infer.index, callee)?;
            return Some(EventStringContext {
                active: own_side() == Some(framework.required_side()),
                family: framework.family,
                target_side: Some(Side::Server),
            });
        }
    };
    Some(EventStringContext { family, target_side, active: true })
}

pub(super) fn event_handler_signature(event: &EventDef) -> Option<String> {
    let handler = event.handler.as_deref()?;
    if let EventFamily::Custom(_) = event.family {
        // What a caller passes, and what `await` returns.
        let mut payload = handler.clone();
        payload.params.drain(..source_skip(event, handler));
        return Some(payload.signature(""));
    }
    if !matches!(event.family, EventFamily::QbCore | EventFamily::Esx) {
        return Some(handler.signature(""));
    }
    let mut payload = handler.clone();
    payload.params.drain(..payload.params.len().min(2));
    // Responses arrive through cb; returning from the server handler is not the client call result.
    payload.returns.clear();
    Some(payload.signature(""))
}

fn string_hover(ws: &Workspace, infer: &Infer, doc: &Document, offset: u32) -> Option<(String, Span)> {
    let located = locate(&doc.chunk, offset);
    let (string, call) = located.string?;
    let ExprKind::String(value) = &string.kind else { return None };
    let callee = call.and_then(|(call, _)| match &call.kind {
        ExprKind::Call { callee, .. } => callee.dotted_path(),
        _ => None,
    });
    if callee.as_deref() == Some("locale") {
        let resource = ws.index.resource_of(doc.file)?;
        let locale = qbx_lua_analysis::locale::LocaleFile::load(&resource.root)?;
        let file = locale.path.file_name()?.to_string_lossy().into_owned();
        let text = locale.text_of(value)?;
        return Some((format!("`{value}` · locales/{file}\n\n{text}"), string.span));
    }
    let context = event_string_context(infer, call);
    let registrations: Vec<_> = ws
        .index
        .events()
        .filter(|(_, event)| event.name == *value && event.kind != EventKind::Trigger)
        .filter(|(_, event)| match &context {
            Some(context) => context.accepts_registration(event),
            None => matches!(event.family, EventFamily::Native | EventFamily::OxLib),
        })
        .collect();
    if registrations.is_empty() {
        return None;
    }
    let label = match context.as_ref().map(|context| &context.family) {
        Some(EventFamily::QbCore) => "QB-Core callback",
        Some(EventFamily::Esx) => "ESX callback",
        Some(EventFamily::Custom(_)) => "callback",
        _ => "event",
    };
    let mut out = format!("{label} `{value}`");
    for (file, event) in registrations.iter().take(5) {
        let Some(entry) = ws.index.file(*file) else { continue };
        let name = entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let side = entry.side.map_or("", |s| s.label());
        let handler = event_handler_signature(event).map(|signature| format!(" `{signature}`")).unwrap_or_default();
        out.push_str(&format!("\n- {side} `{name}:{}`{handler}", event.range.start.line + 1));
    }
    if context.as_ref().is_some_and(EventStringContext::framework) {
        out.push_str("\n\nPayload parameters omit the server's `source` and response `cb`. Responses are asynchronous; Lua return values are not inferred.");
    }
    Some((out, string.span))
}

/// A hover request, with the `level` that lua-language-server's client asks for.
#[derive(Deserialize)]
pub struct LevelHoverParams {
    #[serde(flatten)]
    pub params: HoverParams,
    pub level: Option<i64>,
}

/// A hover, with the deepest level that writes more of it, as lua-language-server answers.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LevelHover {
    #[serde(flatten)]
    pub hover: Hover,
    pub max_level: u32,
}

pub fn hover(ws: &Workspace, doc: &Document, position: Position, level: u32) -> Option<LevelHover> {
    let offset = doc.offset(position);
    let mut detail = Detail::new(level);
    let (text, span) = with_infer(ws, doc, |infer| {
        let Some(target) = target_at(infer, doc, offset) else {
            return super::native_argument::hover(ws, doc, offset).or_else(|| string_hover(ws, infer, doc, offset));
        };
        let called = called_overload(infer, doc, offset);
        let detail = &mut detail;
        let text = match &target {
            Target::Local(id, span) => Some(local_hover(infer, detail, *id, span.start, called)),
            Target::Global(name, _) => global_hover(ws, infer, detail, name, called),
            Target::Member { info, owner, .. } => match called {
                Some(ty) => Some(member_hover(infer, detail, &MemberInfo { ty, ..info.clone() }, owner)),
                None => Some(member_hover(infer, detail, info, owner)),
            },
            Target::Type(name, _) => type_hover(infer, detail, name),
        };
        text.map(|t| (t, target.span()))
    })?;
    let hover = Hover { contents: HoverContents::Markup(markdown(text)), range: Some(doc.range(span)) };
    Some(LevelHover { hover, max_level: detail.max_level() })
}
