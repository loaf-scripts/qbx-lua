use std::path::{Path, PathBuf};

use lsp_types::Url;
use qbx_fivem_data::{Side, KNOWN_IMPORTS, STUBS};
use qbx_lua_analysis::glob::{is_glob, manifest_glob_match};
use qbx_lua_analysis::manifest::Manifest;
use qbx_lua_analysis::project::{
    find_manifest_dir, is_manifest_file, lua_files_under, manifest_path, read_source, relative_slash_path,
    resource_imports, side_of, split_import, ResourceEnv, ResourceLocator,
};
use qbx_lua_analysis::scope::resolve;
use qbx_lua_analysis::Config;
use qbx_lua_syntax::{parse, SmolStr};
use rustc_hash::FxHashSet;

use crate::index::{
    normalize_path, Changes, DefinitionScope, FileEntry, FileId, FileIndex, FileOrigin, Index, Read, ResourceEntry,
    ResourceId,
};
use crate::indexer::index_file;

const MAX_INDEXED_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// How often one change may index the same files again, so that globals typed from each other
/// cannot keep it going.
pub const MAX_INDEX_PASSES: usize = 4;

/// A file just indexed, with the entry the index held for it before.
type Indexed = (FileId, Option<FileEntry>);

/// The files that `refresh_readers` reached, open ones included.
#[derive(Default)]
pub struct Refreshed {
    pub reached: FxHashSet<FileId>,
    /// Those of them that now declare something differently.
    pub changed: FxHashSet<FileId>,
}

#[derive(Default)]
pub struct Workspace {
    pub index: Index,
    pub roots: Vec<PathBuf>,
    pub library: Vec<PathBuf>,
    pub lint_config: Config,
    /// Convars assigned with `set`, `setr` or `sets` in the workspace's .cfg files.
    pub cfg_convars: Vec<SmolStr>,
    locator: ResourceLocator,
}

fn cfg_convars(roots: &[PathBuf]) -> Vec<SmolStr> {
    let mut names: Vec<SmolStr> = Vec::new();
    for root in roots {
        let files = walkdir::WalkDir::new(root).max_depth(2).into_iter().flatten();
        for entry in files.filter(|e| e.path().extension().is_some_and(|ext| ext == "cfg")) {
            let Ok(text) = read_source(entry.path()) else { continue };
            for line in text.lines() {
                let mut words = line.split_whitespace();
                if let (Some("set" | "setr" | "sets"), Some(name)) = (words.next(), words.next()) {
                    let name = name.trim_matches(['"', '\'']);
                    if !name.is_empty() && !names.iter().any(|n| n == name) {
                        names.push(SmolStr::new(name));
                    }
                }
            }
        }
    }
    names
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub files: usize,
    pub resources: usize,
    pub millis: u128,
}

pub fn path_to_uri(path: &Path) -> Url {
    Url::from_file_path(path).unwrap_or_else(|_| Url::parse("file:///invalid").expect("static url"))
}

pub fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

impl Workspace {
    pub fn load_stubs(&mut self) {
        for stub in STUBS {
            let path = PathBuf::from(format!("/qbx-lua-ls/stubs/{}", stub.name));
            let id = self.index.allocate(&path);
            let chunk = parse(stub.source);
            let resolution = resolve(&chunk);
            let uri = Url::parse(&format!("qbx-stub:///{}", stub.name)).expect("static url");
            let side = (stub.side != Side::Shared).then_some(stub.side);
            let entry = |index| FileEntry {
                path: path.clone(),
                uri: uri.clone(),
                origin: FileOrigin::Stub,
                resource: None,
                side,
                index,
            };
            // Globals are only visible from files the index knows. A stub needs its entry before it is
            // indexed to see the stubs loaded before it, so that `function os.nanotime()` of the CfxLua
            // stub lands on the `os` that the Lua stub declares.
            self.index.set_file(id, entry(FileIndex::default()));
            let index = index_file(id, stub.source, &chunk, &resolution, &self.index, side);
            self.index.set_file(id, entry(index));
        }
    }

    pub fn scan(&mut self) -> ScanStats {
        let started = std::time::Instant::now();
        let mut stats = ScanStats::default();
        self.index.clear_workspace();
        self.locator = ResourceLocator::default();
        self.lint_config =
            self.roots.first().and_then(|root| Config::discover(root).ok().flatten()).unwrap_or_default();
        let roots: Vec<(PathBuf, FileOrigin)> = self
            .roots
            .iter()
            .map(|r| (r.clone(), FileOrigin::Workspace))
            .chain(self.library.iter().map(|r| (r.clone(), FileOrigin::Library)))
            .collect();
        for (root, origin) in roots {
            for path in lua_files_under(&root, &self.lint_config) {
                if is_manifest_file(&path) {
                    if let Some(root) = path.parent() {
                        self.ensure_resource(root);
                    }
                } else if self.index_path(&path, origin, None) {
                    stats.files += 1;
                }
            }
        }
        stats.files += self.index_dependencies();
        self.link_imports();
        self.reindex_all();
        self.cfg_convars = cfg_convars(&self.roots);
        stats.resources = self.index.resources.len();
        stats.millis = started.elapsed().as_millis();
        stats
    }

    /// Symbol types are inferred while indexing and may refer to files that were not indexed yet
    /// (exports, imported globals), so a second pass settles them once every file is known.
    fn reindex_all(&mut self) {
        let files: Vec<(PathBuf, FileOrigin)> = self
            .index
            .files()
            .filter(|(_, f)| f.origin != FileOrigin::Stub)
            .map(|(_, f)| (f.path.clone(), f.origin))
            .collect();
        for (path, origin) in files {
            self.index_path(&path, origin, None);
        }
    }

    /// Indexes resources that workspace manifests refer to but that live outside the workspace,
    /// so opening a single resource folder still resolves `@ox_lib`, `@qbx_core` and friends.
    fn index_dependencies(&mut self) -> usize {
        let mut wanted: Vec<(PathBuf, SmolStr)> = Vec::new();
        for resource in &self.index.resources {
            let configured = self.lint_config.imports_for(&resource.manifest_path).into_iter().map(|(p, _)| p);
            let patterns = resource.manifest.imports().map(|s| s.pattern.as_str()).chain(configured);
            let imports = patterns.filter_map(split_import).map(|(name, _)| name);
            let dependencies = resource.manifest.dependencies.iter().map(|d| d.value.as_str());
            for name in imports.chain(dependencies) {
                wanted.push((resource.root.clone(), SmolStr::new(name.trim_start_matches('/'))));
            }
        }
        let mut seen = FxHashSet::default();
        let mut indexed = 0;
        for (from, name) in wanted {
            if !seen.insert(name.clone()) || self.index.resource_by_name(&name).is_some() {
                continue;
            }
            let Some(root) = self.locator.locate(&from, &name) else { continue };
            self.ensure_resource(&root);
            for path in lua_files_under(&root, &self.lint_config) {
                if !is_manifest_file(&path) && self.index_path(&path, FileOrigin::Library, None) {
                    indexed += 1;
                }
            }
        }
        indexed
    }

    fn ensure_resource(&mut self, root: &Path) -> Option<ResourceId> {
        if let Some(id) = self.index.resources.iter().position(|r| r.root == root) {
            return Some(id as ResourceId);
        }
        let manifest_path = manifest_path(root)?;
        let source = read_source(&manifest_path).ok()?;
        let manifest = Manifest::from_chunk(&parse(&source));
        let name = SmolStr::new(root.file_name()?.to_string_lossy());
        self.index.resources.push(ResourceEntry {
            name,
            root: root.to_path_buf(),
            manifest_path,
            manifest,
            files: Vec::new(),
            imports: Vec::new(),
            escrowed: qbx_lua_analysis::project::is_escrowed_resource(root),
        });
        Some(self.index.resources.len() as ResourceId - 1)
    }

    pub fn reload_manifest(&mut self, manifest_file: &Path) {
        let Some(root) = manifest_file.parent() else { return };
        let Some(id) = self.index.resources.iter().position(|r| r.root == root) else {
            self.ensure_resource(root);
            return;
        };
        let Ok(source) = read_source(manifest_file) else { return };
        self.index.resources[id].manifest = Manifest::from_chunk(&parse(&source));
        let files = self.index.resources[id].files.clone();
        for file in files {
            if let Some(path) = self.index.file(file).map(|f| f.path.clone()) {
                self.index_path(&path, FileOrigin::Workspace, None);
            }
        }
        self.link_imports();
    }

    /// The resource of `path` and the side its manifest, or else a `side` override, runs it on.
    pub fn side_and_resource(&mut self, path: &Path) -> (Option<ResourceId>, Option<Side>) {
        let Some(root) = find_manifest_dir(path) else { return (None, None) };
        let Some(id) = self.ensure_resource(&root) else { return (None, None) };
        let relative = relative_slash_path(&root, path);
        let side = side_of(&self.index.resources[id as usize].manifest, &relative);
        (Some(id), side.or_else(|| self.lint_config.side_for(path)))
    }

    /// Re-indexes the files whose side a changed configuration moves, since the side decides the
    /// globals and `(server)` or `(client)` annotations a file sees.
    pub fn resync_sides(&mut self) {
        let files: Vec<(PathBuf, FileOrigin, Option<Side>)> = self
            .index
            .files()
            .filter(|(_, f)| f.origin != FileOrigin::Stub && f.resource.is_some())
            .map(|(_, f)| (f.path.clone(), f.origin, f.side))
            .collect();
        for (path, origin, side) in files {
            if self.side_and_resource(&path).1 != side {
                self.index_path(&path, origin, None);
            }
        }
    }

    /// Indexes `path`, reading it from disk unless `text` (an open document) is given.
    pub fn index_path(&mut self, path: &Path, origin: FileOrigin, text: Option<&str>) -> bool {
        self.read_and_index(path, origin, text).is_some()
    }

    /// Indexes `path` like `index_path`, returning its file and the entry the index held for it.
    fn read_and_index(&mut self, path: &Path, origin: FileOrigin, text: Option<&str>) -> Option<Indexed> {
        if self.lint_config.is_excluded(path) {
            return None;
        }
        let owned;
        let source = match text {
            Some(text) => text,
            None => {
                let too_large = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_INDEXED_FILE_BYTES);
                if too_large {
                    return None;
                }
                match read_source(path) {
                    Ok(text) => {
                        owned = text;
                        &owned
                    }
                    Err(_) => {
                        self.mark_escrowed(path);
                        return None;
                    }
                }
            }
        };
        let chunk = parse(source);
        let resolution = resolve(&chunk);
        Some(self.index_entry(path, origin, source, &chunk, &resolution))
    }

    /// Indexes `path` from disk like `index_path`, adding what it now declares differently to
    /// `changes`.
    pub fn index_path_tracked(&mut self, path: &Path, origin: FileOrigin, changes: &mut Changes) -> bool {
        let Some((id, replaced)) = self.read_and_index(path, origin, None) else { return false };
        record(changes, id, self.changed(id, replaced.as_ref()));
        true
    }

    /// What the file `id` declares differently than `replaced`, the entry the index held for it
    /// before.
    fn changed(&self, id: FileId, replaced: Option<&FileEntry>) -> FxHashSet<Read> {
        self.index.changes(replaced, self.index.file(id))
    }

    /// Indexes the closed files in `stale` again, and then the files that read what they now
    /// declare differently, themselves included, until nothing changes or after `passes` passes,
    /// so that the types those files inferred from those declarations follow them. Files in `open`
    /// are left to their documents.
    pub fn refresh_readers(&mut self, stale: FxHashSet<FileId>, open: &FxHashSet<FileId>, passes: usize) -> Refreshed {
        let mut refreshed = Refreshed::default();
        let mut stale: Vec<FileId> = stale.into_iter().collect();
        for _ in 0..passes {
            stale.sort_unstable();
            let mut changed = Changes::default();
            let mut indexed = FxHashSet::default();
            for id in stale.drain(..) {
                let Some(file) = self.index.file(id).filter(|file| file.origin != FileOrigin::Stub) else { continue };
                refreshed.reached.insert(id);
                if !open.contains(&id) {
                    let (path, origin) = (file.path.clone(), file.origin);
                    if self.index_path_tracked(&path, origin, &mut changed) {
                        indexed.insert(id);
                    }
                }
            }
            refreshed.changed.extend(changed.keys());
            stale.extend(self.index.readers_of(&changed, &indexed));
            if stale.is_empty() {
                break;
            }
        }
        refreshed
    }

    fn mark_escrowed(&mut self, unreadable: &Path) {
        if !unreadable.is_file() {
            return;
        }
        let resource = find_manifest_dir(unreadable).and_then(|root| self.ensure_resource(&root));
        if let Some(id) = resource {
            self.index.resources[id as usize].escrowed = true;
        }
    }

    pub fn index_parsed(
        &mut self,
        path: &Path,
        origin: FileOrigin,
        source: &str,
        chunk: &qbx_lua_syntax::ast::Chunk,
        resolution: &qbx_lua_analysis::scope::Resolution,
    ) -> FileId {
        self.index_entry(path, origin, source, chunk, resolution).0
    }

    /// Indexes a parsed file like `index_parsed`, returning its file and the entry the index held
    /// for it.
    fn index_entry(
        &mut self,
        path: &Path,
        origin: FileOrigin,
        source: &str,
        chunk: &qbx_lua_syntax::ast::Chunk,
        resolution: &qbx_lua_analysis::scope::Resolution,
    ) -> Indexed {
        let id = self.index.allocate(path);
        let (resource, side) = self.side_and_resource(path);
        let origin = self.index.file(id).map_or(origin, |f| f.origin);
        let entry =
            |index| FileEntry { path: path.to_path_buf(), uri: path_to_uri(path), origin, resource, side, index };
        // Globals are only visible from files the index knows, so a file indexed for the first time,
        // such as a new one opened in the editor, needs its entry before its calls to a `---@callback`
        // wrapper of another file can be recognized.
        if self.index.file(id).is_none() {
            self.index.set_file(id, entry(FileIndex::default()));
        }
        let mut file = entry(index_file(id, source, chunk, resolution, &self.index, side));
        if file.defines_for_all() {
            file.index.definition_scope = self.definition_scope(path);
        }
        (id, self.index.set_file(id, file))
    }

    /// Indexes a document open in the editor. Its symbols are inferred with what the index held for
    /// the file, so when it is indexed for the first time, or declares other classes or aliases now,
    /// and it read what it declares differently, a second pass reads that, as the scan's second
    /// pass does for every file: the methods of `---@class Name` `Name = {}` see the class, and
    /// `function Shop.count() return Shop.items.apple end` the items. What it now declares
    /// differently is added to `changes`.
    pub fn index_document(
        &mut self,
        path: &Path,
        source: &str,
        chunk: &qbx_lua_syntax::ast::Chunk,
        resolution: &qbx_lua_analysis::scope::Resolution,
        changes: &mut Changes,
    ) -> FileId {
        let id = self.index.allocate(path);
        let first = self.index.file(id).is_none();
        let declared = |ws: &Self| ws.index.file(id).map(|file| declared_types(&file.index)).unwrap_or_default();
        let before = declared(self);
        let (_, replaced) = self.index_entry(path, FileOrigin::Workspace, source, chunk, resolution);
        let changed = self.changed(id, replaced.as_ref());
        let again = (first || declared(self) != before) && self.index.read_any(id, &changed);
        record(changes, id, changed);
        if again {
            let (_, replaced) = self.index_entry(path, FileOrigin::Workspace, source, chunk, resolution);
            record(changes, id, self.changed(id, replaced.as_ref()));
        }
        id
    }

    /// Where a definition file outside any resource applies: on the side a `side` override, or else
    /// the name of the file or of its nearest folder that names one, gives it, and for the resource
    /// that the nearest folder named after one stands for. Folders above the workspace or library
    /// root that holds the file do not count.
    fn definition_scope(&self, path: &Path) -> DefinitionScope {
        let normalized = normalize_path(path);
        let root = self.roots.iter().chain(&self.library).map(|root| normalize_path(root));
        let depth = root.filter(|root| normalized.starts_with(root)).map(|root| root.components().count()).max();
        let parent = path.parent().unwrap_or(path);
        let depth = depth.unwrap_or_else(|| parent.components().count().saturating_sub(1));
        let mut folders: Vec<String> =
            parent.components().skip(depth).map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        folders.reverse();
        let stem = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
        let named = std::iter::once(&stem).chain(&folders).find_map(|name| side_named_by(name));
        DefinitionScope {
            side: self.lint_config.side_for(path).or(named),
            provider: folders.into_iter().find(|name| self.is_resource_name(name)).map(SmolStr::from),
        }
    }

    /// Whether `name` is a resource that the workspace has, imports from, or that a well-known
    /// import such as `@ox_core/lib/init.lua` names.
    fn is_resource_name(&self, name: &str) -> bool {
        let names =
            |pattern: &str| split_import(pattern).is_some_and(|(resource, _)| resource.eq_ignore_ascii_case(name));
        self.index.resource_by_name(name).is_some()
            || KNOWN_IMPORTS.iter().any(|import| names(import.path))
            || self.index.resources.iter().any(|r| r.manifest.imports().any(|script| names(&script.pattern)))
    }

    pub fn link_imports(&mut self) {
        for id in 0..self.index.resources.len() {
            let entry = &self.index.resources[id];
            let imports = resource_imports(&entry.manifest, &entry.manifest_path, &self.lint_config)
                .into_iter()
                .flat_map(|(pattern, side)| self.import_files(pattern).into_iter().map(move |file| (file, side)))
                .collect();
            self.index.resources[id].imports = imports;
        }
    }

    /// The indexed files an `@resource/path` import names, where the path may be a manifest glob.
    fn import_files(&self, pattern: &str) -> Vec<FileId> {
        let Some((name, file)) = split_import(pattern) else { return Vec::new() };
        let Some((_, target)) = self.index.resource_by_name(name) else { return Vec::new() };
        if !is_glob(file) {
            let id = self.index.file_id(&target.root.join(file));
            return id.filter(|id| self.index.file(*id).is_some()).into_iter().collect();
        }
        // Open documents keep the path spelling the editor sent, so compare normalized paths.
        let root = normalize_path(&target.root);
        let matches = |path: &Path| manifest_glob_match(file, &relative_slash_path(&root, &normalize_path(path)));
        target.files.iter().copied().filter(|id| self.index.file(*id).is_some_and(|f| matches(&f.path))).collect()
    }

    /// The lint environment of a resource, assembled from the per-file summaries in the index.
    pub fn resource_env(&self, resource: ResourceId) -> ResourceEnv {
        let mut env = ResourceEnv::default();
        let Some(entry) = self.index.resource(resource) else { return env };
        env.opaque = entry.escrowed;
        for file in entry.files.iter().filter_map(|id| self.index.file(*id)) {
            env.add_summary(&file.index.summary, file.side);
        }
        for (pattern, side) in resource_imports(&entry.manifest, &entry.manifest_path, &self.lint_config) {
            let files = self.import_files(pattern).into_iter().filter_map(|id| self.index.file(id));
            env.add_import(pattern, side, files.map(|file| &file.index.summary));
        }
        env.set_declarations(self.index.declarations());
        env
    }
}

impl Workspace {
    /// Event handlers and exports of every indexed file, in the shape the cross-file lint rules use.
    pub fn crossrefs(&self) -> qbx_lua_analysis::crossref::CrossRefs {
        use qbx_lua_analysis::crossref::{Arity, CrossRefs};

        use crate::index::EventKind;
        use crate::types::FunType;

        fn arity(fun: &FunType) -> Arity {
            let vararg = fun.params.last().is_some_and(|p| p.name == "...");
            Arity { params: fun.params.len() - usize::from(vararg), vararg }
        }

        let mut refs = CrossRefs::default();
        for resource in &self.index.resources {
            let hidden = resource.escrowed || resource.manifest.has_non_lua_scripts();
            if hidden {
                refs.opaque_resources.insert(resource.name.clone());
            }
        }
        for (_, file) in self.index.files() {
            let resource = file.resource.and_then(|id| self.index.resource(id)).map(|r| r.name.clone());
            if let (true, Some(name)) = (file.index.dynamic_exports, &resource) {
                refs.opaque_resources.insert(name.clone());
            }
            if let Some(name) = &resource {
                refs.resources.insert(name.clone());
            }
            for event in file.index.events.iter().filter(|e| matches!(e.kind, EventKind::NetEvent | EventKind::Handler))
            {
                refs.add_event(event.name.clone(), event.side, event.handler.as_deref().map(arity), resource.clone());
            }
            if let Some(resource) = resource {
                for export in &file.index.exports {
                    let known = export.ty.as_fun().map(|f| arity(f));
                    let arity = known.unwrap_or(Arity { params: 0, vararg: true });
                    refs.exports.insert((resource.clone(), export.name.clone()), arity);
                }
            }
        }
        refs
    }
}

/// Adds what the file `id` declares differently, `changed`, to `changes`.
fn record(changes: &mut Changes, id: FileId, changed: FxHashSet<Read>) {
    if !changed.is_empty() {
        changes.entry(id).or_default().extend(changed);
    }
}

/// The classes and aliases that a file declares. Its globals and members are left out, since typing
/// a new `Config.Foo` changes them on every key.
fn declared_types(index: &FileIndex) -> Vec<(&'static str, SmolStr)> {
    let classes = index.classes.iter().map(|class| ("@class", class.name.clone()));
    let aliases = index.aliases.iter().map(|alias| ("@alias", alias.name.clone()));
    let mut names: Vec<(&'static str, SmolStr)> = classes.chain(aliases).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// The side the name of a file or folder gives, as `server_vehicle.lua`, `cl_main.lua` or a `client`
/// folder do. Names of both sides, or `shared` and `common`, give both.
fn side_named_by(name: &str) -> Option<Side> {
    let words: Vec<String> = name.split(['_', '-', '.', ' ']).map(str::to_ascii_lowercase).collect();
    let has = |options: &[&str]| words.iter().any(|word| options.contains(&word.as_str()));
    match (has(&["client", "cl"]), has(&["server", "sv"]), has(&["shared", "common"])) {
        (false, false, false) => None,
        (true, false, false) => Some(Side::Client),
        (false, true, false) => Some(Side::Server),
        _ => Some(Side::Shared),
    }
}
