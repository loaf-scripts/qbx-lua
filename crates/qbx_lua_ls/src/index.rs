use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use lsp_types::{Range, Url};
use qbx_fivem_data::Side;
use qbx_lua_analysis::manifest::Manifest;
use qbx_lua_analysis::project::{split_import, Declaration, Declarations};
use qbx_lua_analysis::summary::FileSummary;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

use crate::luacats::{applies_on, DocIndexField};
use crate::types::{DescribedValue, FunType, Type};

pub type FileId = u32;
pub type ResourceId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Variable,
    Table,
    Field,
    Class,
    Alias,
    Export,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: SmolStr,
    pub kind: SymbolKind,
    pub ty: Type,
    pub doc: Option<Arc<str>>,
    pub deprecated: bool,
    /// Source text of a short literal value, shown in hovers as `name: type = value`.
    pub literal: Option<SmolStr>,
    pub range: Range,
}

impl Symbol {
    /// A global that a `---@class` annotation declares, like `Test` in `---@class Test` `Test = {}`,
    /// rather than one typed as the class with `---@type`.
    pub fn is_class_table(&self) -> bool {
        self.kind == SymbolKind::Table && matches!(self.ty, Type::Named(..))
    }
}

#[derive(Clone, Debug)]
pub struct Member {
    pub owner: SmolStr,
    pub symbol: Symbol,
    /// Set through a value typed as the class rather than on the table its `---@class` declares,
    /// like `abc.x = 1` below `---@type Test` `local abc`. Strict classes do not count it as declared.
    pub injected: bool,
}

/// The entries of a table constructor that have no name: its array part (`key` is `None`), or its
/// other `[key] = value` pairs.
#[derive(Clone, Debug)]
pub struct Element {
    pub owner: SmolStr,
    pub key: Option<Type>,
    pub value: Type,
}

#[derive(Clone, Debug)]
pub struct ClassDef {
    pub name: SmolStr,
    pub parents: Vec<SmolStr>,
    pub fields: Vec<Symbol>,
    /// The side each of `fields` is scoped to by `@field (server) name type`.
    pub field_sides: Vec<Option<Side>>,
    /// The values that `---|` lines list under each of `fields`, with their descriptions.
    pub field_values: Vec<Vec<DescribedValue>>,
    pub indices: Vec<DocIndexField>,
    /// `---@field [1] number` and `---@field [true] string`: fields keyed by an integer or boolean
    /// literal, with their values, in declaration order.
    pub literal_fields: Vec<DocIndexField>,
    pub call: Option<Arc<FunType>>,
    pub doc: Option<Arc<str>>,
    pub range: Range,
    /// The side of `@class (server) Name`.
    pub side: Option<Side>,
    /// `Some(true)` for `@class (strict) Name` or `(exact)`, `Some(false)` for `(loose)`, and `None`
    /// when `strict_classes` in qbxlint.toml decides.
    pub strict: Option<bool>,
}

impl ClassDef {
    /// The general indices declared for this side, as `[string]` and `[integer]`. Of indices with
    /// the same key, the last declared counts, as for an unscoped index before side filtering.
    pub fn indices(&self, side: Option<Side>) -> Vec<(&Type, &Type)> {
        let mut out: Vec<(&Type, &Type)> = Vec::new();
        for field in self.indices.iter().filter(|field| applies_on(field.side, side)) {
            match out.iter_mut().find(|(key, _)| **key == field.key) {
                Some(index) => *index = (&field.key, &field.ty),
                None => out.push((&field.key, &field.ty)),
            }
        }
        out
    }

    pub fn literal_fields(&self, side: Option<Side>) -> impl Iterator<Item = (&Type, &Type)> {
        self.literal_fields
            .iter()
            .filter(move |field| applies_on(field.side, side))
            .map(|field| (&field.key, &field.ty))
    }
}

#[derive(Clone, Debug)]
pub struct AliasDef {
    pub name: SmolStr,
    pub ty: Type,
    pub doc: Option<Arc<str>>,
    pub range: Range,
    /// The side of `@alias (server) Name` or `@enum (server) Name`.
    pub side: Option<Side>,
    /// The values that the `---|` lines under `@alias` list, with their descriptions.
    pub values: Vec<DescribedValue>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    NetEvent,
    Handler,
    Callback,
    Trigger,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EventFamily {
    Native,
    OxLib,
    QbCore,
    Esx,
    /// Wrappers tagged `---@callback`, by the family name of their tag.
    Custom(SmolStr),
}

#[derive(Clone, Debug)]
pub struct EventDef {
    pub name: SmolStr,
    pub kind: EventKind,
    pub family: EventFamily,
    /// The manifest side narrowed by the guard around this registration or trigger.
    pub side: Option<Side>,
    pub handler: Option<Arc<FunType>>,
    pub range: Range,
}

#[derive(Clone, Debug)]
pub struct NuiCallbackDef {
    pub name: SmolStr,
    /// The exact registration global, retained to check current cross-file replacements.
    pub registration: SmolStr,
    pub range: Range,
}

#[derive(Clone, Debug, Default)]
pub struct FileIndex {
    pub globals: Vec<Symbol>,
    pub members: Vec<Member>,
    pub elements: Vec<Element>,
    pub classes: Vec<ClassDef>,
    pub aliases: Vec<AliasDef>,
    pub exports: Vec<Symbol>,
    pub events: Vec<EventDef>,
    /// NUI registrations are not network events and must not enter event completion.
    pub nui_callbacks: Vec<NuiCallbackDef>,
    pub module_return: Option<Type>,
    pub convars: Vec<SmolStr>,
    pub state_keys: Vec<SmolStr>,
    /// The file registers exports under names computed at runtime, so the listed ones are not all.
    pub dynamic_exports: bool,
    /// A `---@meta` line above the first statement marks a definition file.
    pub meta: bool,
    /// The `members` set on `exports` below a `---@type` or `---@class`, by index, as
    /// `exports.phone = {}` below `---@type PhoneExports`, rather than registering an export.
    pub typed_exports: Vec<u32>,
    /// Where the file applies when it is a definition file outside any resource.
    pub definition_scope: DefinitionScope,
    pub summary: FileSummary,
}

/// The scripts a definition file outside any resource is written for.
#[derive(Clone, Debug, Default)]
pub struct DefinitionScope {
    /// The side its path names, as `server_vehicle.lua` or a `client` folder do.
    pub side: Option<Side>,
    /// The resource it sits in a folder named after, whose import provides what it declares.
    pub provider: Option<SmolStr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOrigin {
    Stub,
    Workspace,
    Library,
}

#[derive(Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub uri: Url,
    pub origin: FileOrigin,
    pub resource: Option<ResourceId>,
    pub side: Option<Side>,
    pub index: FileIndex,
}

impl FileEntry {
    /// A definition file outside any resource: one from a `library` folder, as LuaLS reads its
    /// library, or a workspace file marked `---@meta`. Resources see its globals on the side of its
    /// `definition_scope`, and only when they import from its provider. Built-in stubs are visible
    /// anyway and their globals are known to the rules.
    pub fn defines_for_all(&self) -> bool {
        match self.origin {
            FileOrigin::Stub => false,
            FileOrigin::Library => self.resource.is_none(),
            FileOrigin::Workspace => self.resource.is_none() && self.index.meta,
        }
    }

    /// Whether the member at `index`, set on `exports`, declares the type of a resource's exports:
    /// one with a `---@type` or `---@class` above it, or any in a definition file. Elsewhere
    /// `exports.Name = fn` registers an export of the file's own resource, and tests mock exports so.
    fn declares_export_type(&self, index: u32) -> bool {
        self.index.meta || self.defines_for_all() || self.index.typed_exports.contains(&index)
    }

    /// Whether the file declares the exports of `resource` member by member: a definition file, or
    /// one that types `exports.<resource>` itself.
    fn describes_exports_of(&self, resource: &str) -> bool {
        let typed = |i: &u32| self.index.members.get(*i as usize).is_some_and(|m| m.symbol.name == resource);
        self.index.meta || self.defines_for_all() || self.index.typed_exports.iter().any(typed)
    }
}

#[derive(Debug)]
pub struct ResourceEntry {
    pub name: SmolStr,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
    pub files: Vec<FileId>,
    /// Files pulled in through `@resource/file.lua` manifest entries, with the side they load on.
    pub imports: Vec<(FileId, Side)>,
    /// Ships a `.fxap` marker or an encrypted file, so part of its code cannot be read.
    pub escrowed: bool,
}

type Slot = (FileId, u32);

#[derive(Debug, Default)]
pub struct Index {
    files: Vec<Option<FileEntry>>,
    by_path: FxHashMap<PathBuf, FileId>,
    pub resources: Vec<ResourceEntry>,
    globals: FxHashMap<SmolStr, Vec<Slot>>,
    members: FxHashMap<SmolStr, Vec<Slot>>,
    elements: FxHashMap<SmolStr, Vec<Slot>>,
    classes: FxHashMap<SmolStr, Vec<Slot>>,
    aliases: FxHashMap<SmolStr, Vec<Slot>>,
    /// The `members` on `exports` that declare the type of a resource's exports, by resource name.
    export_types: FxHashMap<SmolStr, Vec<Slot>>,
    /// Built on first use after the files change, since every lint of a resource needs them.
    declarations: OnceLock<Arc<Declarations>>,
}

pub fn normalize_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().replace('/', "\\").to_lowercase())
    } else {
        path.to_path_buf()
    }
}

fn remove_file_slots<'a>(
    map: &mut FxHashMap<SmolStr, Vec<Slot>>,
    keys: impl Iterator<Item = &'a SmolStr>,
    file: FileId,
) {
    for key in keys {
        if let Some(slots) = map.get_mut(key) {
            slots.retain(|(f, _)| *f != file);
            if slots.is_empty() {
                map.remove(key);
            }
        }
    }
}

impl Index {
    /// Start a fresh disk scan without discarding the built-in library.
    pub fn clear_workspace(&mut self) {
        let stubs: Vec<FileEntry> =
            self.files.iter_mut().filter_map(Option::take).filter(|file| file.origin == FileOrigin::Stub).collect();
        *self = Self::default();
        for entry in stubs {
            let id = self.allocate(&entry.path);
            self.set_file(id, entry);
        }
    }

    pub fn file_id(&self, path: &Path) -> Option<FileId> {
        self.by_path.get(&normalize_path(path)).copied()
    }

    pub fn file(&self, id: FileId) -> Option<&FileEntry> {
        self.files.get(id as usize).and_then(Option::as_ref)
    }

    pub fn files(&self) -> impl Iterator<Item = (FileId, &FileEntry)> {
        self.files.iter().enumerate().filter_map(|(id, f)| f.as_ref().map(|f| (id as FileId, f)))
    }

    pub fn file_count(&self) -> usize {
        self.files.iter().flatten().count()
    }

    pub fn resource(&self, id: ResourceId) -> Option<&ResourceEntry> {
        self.resources.get(id as usize)
    }

    pub fn resource_by_name(&self, name: &str) -> Option<(ResourceId, &ResourceEntry)> {
        self.resources
            .iter()
            .enumerate()
            .find(|(_, r)| r.name.eq_ignore_ascii_case(name))
            .map(|(id, r)| (id as ResourceId, r))
    }

    pub fn resource_of(&self, file: FileId) -> Option<&ResourceEntry> {
        self.file(file)?.resource.and_then(|id| self.resource(id))
    }

    /// Reserves an id for `path`, reusing the existing one when the file is already known.
    pub fn allocate(&mut self, path: &Path) -> FileId {
        let key = normalize_path(path);
        if let Some(id) = self.by_path.get(&key) {
            return *id;
        }
        let id = self.files.len() as FileId;
        self.files.push(None);
        self.by_path.insert(key, id);
        id
    }

    pub fn set_file(&mut self, id: FileId, entry: FileEntry) {
        self.clear_slots(id);
        for (i, symbol) in entry.index.globals.iter().enumerate() {
            self.globals.entry(symbol.name.clone()).or_default().push((id, i as u32));
        }
        for (i, member) in entry.index.members.iter().enumerate() {
            self.members.entry(member.owner.clone()).or_default().push((id, i as u32));
        }
        for (i, element) in entry.index.elements.iter().enumerate() {
            self.elements.entry(element.owner.clone()).or_default().push((id, i as u32));
        }
        for (i, class) in entry.index.classes.iter().enumerate() {
            self.classes.entry(class.name.clone()).or_default().push((id, i as u32));
        }
        for (i, alias) in entry.index.aliases.iter().enumerate() {
            self.aliases.entry(alias.name.clone()).or_default().push((id, i as u32));
        }
        for (i, member) in entry.index.members.iter().enumerate() {
            if member.owner == "exports" && entry.declares_export_type(i as u32) {
                self.export_types.entry(member.symbol.name.clone()).or_default().push((id, i as u32));
            }
        }
        if let Some(resource) = entry.resource.and_then(|r| self.resources.get_mut(r as usize)) {
            if !resource.files.contains(&id) {
                resource.files.push(id);
            }
        }
        self.files[id as usize] = Some(entry);
    }

    fn clear_slots(&mut self, id: FileId) {
        self.declarations.take();
        let Some(old) = self.files.get_mut(id as usize).and_then(Option::take) else { return };
        remove_file_slots(&mut self.globals, old.index.globals.iter().map(|s| &s.name), id);
        remove_file_slots(&mut self.members, old.index.members.iter().map(|m| &m.owner), id);
        remove_file_slots(&mut self.elements, old.index.elements.iter().map(|e| &e.owner), id);
        remove_file_slots(&mut self.classes, old.index.classes.iter().map(|c| &c.name), id);
        remove_file_slots(&mut self.aliases, old.index.aliases.iter().map(|a| &a.name), id);
        let exports = old.index.members.iter().filter(|m| m.owner == "exports");
        remove_file_slots(&mut self.export_types, exports.map(|m| &m.symbol.name), id);
    }

    pub fn remove_file(&mut self, path: &Path) {
        let Some(id) = self.file_id(path) else { return };
        self.clear_slots(id);
        for resource in &mut self.resources {
            resource.files.retain(|f| *f != id);
            resource.imports.retain(|(f, _)| *f != id);
        }
        self.files[id as usize] = None;
    }

    /// Whether symbols of `target` are in scope for code in `from`.
    pub fn is_visible(&self, from: FileId, target: FileId) -> bool {
        if from == target {
            return true;
        }
        let (Some(source), Some(other)) = (self.file(from), self.file(target)) else { return false };
        let sides_match = match (source.side, other.side.or(other.index.definition_scope.side)) {
            (Some(a), Some(b)) => b.is_available_on(a),
            _ => true,
        };
        if !sides_match {
            return false;
        }
        if other.defines_for_all() {
            let provider = other.index.definition_scope.provider.as_deref();
            return provider.is_none_or(|provider| self.imports_from(source, provider));
        }
        if other.origin == FileOrigin::Stub {
            return true;
        }
        match (source.resource, other.resource) {
            (Some(a), Some(b)) if a == b => true,
            (Some(a), _) => self.resource(a).is_some_and(|r| {
                r.imports
                    .iter()
                    .any(|(file, side)| *file == target && source.side.is_none_or(|s| side.is_available_on(s)))
            }),
            (None, None) => true,
            (None, Some(_)) => false,
        }
    }

    /// Whether the resource of `source` loads a file of the resource `provider` on its side.
    fn imports_from(&self, source: &FileEntry, provider: &str) -> bool {
        let Some(resource) = source.resource.and_then(|id| self.resource(id)) else { return true };
        resource.manifest.imports().any(|script| {
            split_import(&script.pattern).is_some_and(|(name, _)| name.eq_ignore_ascii_case(provider))
                && source.side.is_none_or(|side| script.side.is_available_on(side))
        })
    }

    /// Whether two files can share globals at all, regardless of the side either one runs on.
    pub fn is_related(&self, a: FileId, b: FileId) -> bool {
        let (Some(first), Some(second)) = (self.file(a), self.file(b)) else { return false };
        match (first.resource, second.resource) {
            (Some(x), Some(y)) if x == y => true,
            (Some(x), Some(y)) => {
                let imports = |from: ResourceId, file: FileId| {
                    self.resource(from).is_some_and(|r| r.imports.iter().any(|(imported, _)| *imported == file))
                };
                imports(x, b) || imports(y, a)
            }
            (None, None) => true,
            _ => false,
        }
    }

    fn visible_first<'a, T>(
        &'a self,
        slots: Option<&'a Vec<Slot>>,
        from: FileId,
        get: impl Fn(&'a FileEntry, u32) -> Option<&'a T>,
    ) -> Vec<(FileId, &'a T)> {
        // Strictly what the runtime would see: a `Config` of some other resource is a different table.
        let Some(slots) = slots else { return Vec::new() };
        let resolve = |(file, i): &Slot| Some((*file, get(self.file(*file)?, *i)?));
        slots.iter().filter(|(f, _)| self.is_visible(from, *f)).filter_map(resolve).collect()
    }

    /// Whether `target` is a definition file outside any resource that code in `from` only falls back
    /// on: what the resource of `from` declares itself, imports, or gets from the stubs comes first.
    pub fn falls_back_on(&self, from: FileId, target: FileId) -> bool {
        from != target && self.file(target).is_some_and(FileEntry::defines_for_all)
    }

    /// Drops what definition files outside any resource declare when other files declare it too.
    pub fn prefer_own<T>(&self, from: FileId, found: &mut Vec<T>, file: impl Fn(&T) -> FileId) {
        if found.iter().any(|item| !self.falls_back_on(from, file(item))) {
            found.retain(|item| !self.falls_back_on(from, file(item)));
        }
    }

    pub fn globals_named(&self, name: &str, from: FileId) -> Vec<(FileId, &Symbol)> {
        let mut found = self.visible_first(self.globals.get(name), from, |f, i| f.index.globals.get(i as usize));
        self.prefer_own(from, &mut found, |(file, _)| *file);
        found
    }

    /// Members are looked up per resource rather than per file: libraries such as ox_lib load the
    /// files that extend their table lazily, so importing one file makes all of them reachable.
    pub fn members_of(&self, owner: &str, from: FileId) -> Vec<(FileId, &Symbol)> {
        self.owner_slots(self.members.get(owner), owner, from)
            .into_iter()
            .filter_map(|(file, i)| Some((file, &self.file(file)?.index.members.get(i as usize)?.symbol)))
            .collect()
    }

    /// The members of the class `owner` set on the table its `---@class` declares, like
    /// `function Test:greet()`, leaving out those set through values typed as the class.
    pub fn declared_members_of(&self, owner: &str, from: FileId) -> Vec<&Symbol> {
        self.owner_slots(self.members.get(owner), owner, from)
            .into_iter()
            .filter_map(|(file, i)| self.file(file)?.index.members.get(i as usize))
            .filter(|member| !member.injected)
            .map(|member| &member.symbol)
            .collect()
    }

    /// The array parts and `[key]` entries of the tables `owner` names, visible like its members.
    pub fn elements_of(&self, owner: &str, from: FileId) -> Vec<&Element> {
        self.owner_slots(self.elements.get(owner), owner, from)
            .into_iter()
            .filter_map(|(file, i)| self.file(file)?.index.elements.get(i as usize))
            .collect()
    }

    fn owner_slots(&self, slots: Option<&Vec<Slot>>, owner: &str, from: FileId) -> Vec<Slot> {
        let Some(slots) = slots else { return Vec::new() };
        let reachable = |target: FileId| {
            let sides_match = match (self.file(from).and_then(|f| f.side), self.file(target).and_then(|f| f.side)) {
                (Some(a), Some(b)) => b.is_available_on(a),
                _ => true,
            };
            sides_match
                && (self.is_visible(from, target)
                    || self.is_related(from, target)
                    || self.imports_resource_of(from, target))
        };
        // `%`-owners name one specific table of one file (a local or a module return), so whoever
        // holds a value of that type may see all of it.
        if owner.starts_with('%') {
            return slots.clone();
        }
        // A table the resource fills itself (`Config`, `Shared`) is its own; only tables that come
        // from an imported library (`lib`, `qbx`) are completed from that library's other files.
        let own_resource = self.file(from).and_then(|f| f.resource);
        let is_own = |target: FileId| {
            target == from || (own_resource.is_some() && self.file(target).and_then(|f| f.resource) == own_resource)
        };
        let mut in_scope: Vec<Slot> = slots.iter().copied().filter(|(f, _)| self.is_visible(from, *f)).collect();
        self.prefer_own(from, &mut in_scope, |(file, _)| *file);
        if in_scope.iter().any(|(f, _)| is_own(*f)) {
            return in_scope;
        }
        let visible: Vec<Slot> = slots.iter().copied().filter(|(f, _)| reachable(*f)).collect();
        // Classes travel between resources through exports and events, so their members are looked
        // up everywhere; plain tables of unrelated resources are not.
        if !visible.is_empty() || !self.classes.contains_key(owner) {
            return visible;
        }
        slots.clone()
    }

    fn imports_resource_of(&self, from: FileId, target: FileId) -> bool {
        let (Some(resource), Some(target_resource)) =
            (self.resource_of(from), self.file(target).and_then(|f| f.resource))
        else {
            return false;
        };
        resource.imports.iter().any(|(file, _)| self.file(*file).and_then(|f| f.resource) == Some(target_resource))
    }

    pub fn has_members(&self, owner: &str) -> bool {
        self.members.contains_key(owner)
    }

    fn class_slots(&self, name: &str) -> impl Iterator<Item = (FileId, &ClassDef)> {
        let slots = self.classes.get(name).into_iter().flatten();
        slots.filter_map(|(f, i)| Some((*f, self.file(*f)?.index.classes.get(*i as usize)?)))
    }

    fn alias_slots(&self, name: &str) -> impl Iterator<Item = (FileId, &AliasDef)> {
        let slots = self.aliases.get(name).into_iter().flatten();
        slots.filter_map(|(f, i)| Some((*f, self.file(*f)?.index.aliases.get(*i as usize)?)))
    }

    /// The first declaration of the class `name` that applies to code on `side`.
    pub fn class(&self, name: &str, side: Option<Side>) -> Option<(FileId, &ClassDef)> {
        self.class_slots(name).find(|(_, class)| applies_on(class.side, side))
    }

    /// Whether any class is declared `(strict)` or `(exact)`.
    pub fn has_strict_class(&self) -> bool {
        self.files().any(|(_, f)| f.index.classes.iter().any(|class| class.strict == Some(true)))
    }

    /// Every declaration of the class `name`, whatever side it is scoped to.
    pub fn class_defs(&self, name: &str) -> Vec<(FileId, &ClassDef)> {
        self.class_slots(name).collect()
    }

    /// The first declaration of the alias or enum `name` that applies to code on `side`.
    pub fn alias(&self, name: &str, side: Option<Side>) -> Option<(FileId, &AliasDef)> {
        self.alias_slots(name).find(|(_, alias)| applies_on(alias.side, side))
    }

    /// Every declaration of the alias or enum `name`, whatever side it is scoped to.
    pub fn alias_defs(&self, name: &str) -> Vec<(FileId, &AliasDef)> {
        self.alias_slots(name).collect()
    }

    pub fn class_names(&self) -> impl Iterator<Item = &SmolStr> {
        self.classes.keys().chain(self.aliases.keys())
    }

    /// Every global visible from `from`, for completion.
    pub fn visible_globals(&self, from: FileId) -> impl Iterator<Item = (FileId, &Symbol)> {
        self.files()
            .filter(move |(id, _)| self.is_visible(from, *id))
            .flat_map(|(id, file)| file.index.globals.iter().map(move |s| (id, s)))
    }

    /// Types declared for the exports of resources, as `---@type PhoneExports` above
    /// `exports['phone'] = {}`, that code in `from` sees. Exports cross resources, so these are
    /// looked up in every file whose side fits, like registered exports are.
    pub fn declared_exports(&self, from: FileId) -> Vec<(FileId, &Symbol)> {
        self.export_type_slots(self.export_types.values().flatten(), from)
    }

    /// The types declared for the exports of `resource`, as `declared_exports` finds them.
    pub fn declared_exports_of(&self, resource: &str, from: FileId) -> Vec<(FileId, &Symbol)> {
        self.export_type_slots(self.export_types.get(resource).into_iter().flatten(), from)
    }

    /// What the files that describe the exports of `resource` set on them one by one, as
    /// `function exports.qbx_core:GetCid(source) end` does below `exports.qbx_core = {}`.
    pub fn declared_export_members(&self, resource: &str, from: FileId) -> Vec<(FileId, &Symbol)> {
        let slots = self.members.get(format!("exports.{resource}").as_str()).into_iter().flatten();
        let describes = |(file, _): &&Slot| self.file(*file).is_some_and(|entry| entry.describes_exports_of(resource));
        self.export_type_slots(slots.filter(describes), from)
    }

    fn export_type_slots<'a>(
        &'a self,
        slots: impl Iterator<Item = &'a Slot>,
        from: FileId,
    ) -> Vec<(FileId, &'a Symbol)> {
        let side = self.file(from).and_then(|f| f.side);
        slots
            .filter_map(|(file, i)| Some((*file, self.file(*file)?, i)))
            .filter(|(_, entry, _)| match (side, entry.side.or(entry.index.definition_scope.side)) {
                (Some(side), Some(declared)) => declared.is_available_on(side),
                _ => true,
            })
            .filter_map(|(id, entry, i)| Some((id, &entry.index.members.get(*i as usize)?.symbol)))
            .collect()
    }

    /// The globals that definition files outside any resource declare, with where they apply.
    pub fn declarations(&self) -> Arc<Declarations> {
        let build = || {
            let mut declarations = Declarations::default();
            for (_, file) in self.files().filter(|(_, file)| file.defines_for_all()) {
                let DefinitionScope { side, provider } = &file.index.definition_scope;
                for def in &file.index.summary.global_defs {
                    let declaration = Declaration { side: *side, provider: provider.clone() };
                    declarations.entry(def.name.clone()).or_default().push(declaration);
                }
            }
            Arc::new(declarations)
        };
        self.declarations.get_or_init(build).clone()
    }

    pub fn exports_of(&self, resource: &str) -> Vec<(FileId, &Symbol)> {
        let Some((_, entry)) = self.resource_by_name(resource) else { return Vec::new() };
        entry
            .files
            .iter()
            .filter_map(|id| Some((*id, self.file(*id)?)))
            .flat_map(|(id, file)| file.index.exports.iter().map(move |s| (id, s)))
            .collect()
    }

    pub fn events(&self) -> impl Iterator<Item = (FileId, &EventDef)> {
        self.files().flat_map(|(id, file)| file.index.events.iter().map(move |e| (id, e)))
    }

    /// Resolves a `require` argument the way ox_lib does: dotted or slashed, relative to the
    /// resource root, optionally prefixed with `@resource`.
    pub fn resolve_require(&self, module: &str, from: FileId) -> Option<FileId> {
        let (resource, module) = match module.strip_prefix('@') {
            Some(rest) => {
                let (name, path) = rest.split_once(['/', '.'])?;
                (self.resource_by_name(name)?.1, path)
            }
            None => (self.resource_of(from)?, module),
        };
        let relative = if module.contains('/') { module.to_string() } else { module.replace('.', "/") };
        let relative = relative.trim_end_matches(".lua");
        [format!("{relative}.lua"), format!("{relative}/init.lua")]
            .iter()
            .find_map(|candidate| self.file_id(&resource.root.join(candidate)))
    }
}
