use std::path::{Path, PathBuf};
use std::sync::Arc;

use qbx_fivem_data::{known_import, Side};
use qbx_lua_syntax::ast::Chunk;
use qbx_lua_syntax::{parse, SmolStr};
use qbx_luacats::types::{FunType, Type};
use rustc_hash::{FxHashMap, FxHashSet};
use walkdir::{DirEntry, WalkDir};

use crate::config::Config;
use crate::glob::{is_glob, manifest_glob_match};
use crate::manifest::{Manifest, MANIFEST_FILE_NAMES};
use crate::scope::{resolve, Resolution};
use crate::summary::{summarize, FileSummary};

const GENERATED_LINE_BYTES: usize = 4096;

/// Whether a `.lua` file holds something other than hand-written Lua source: a FiveM escrow (asset
/// protection) payload, precompiled bytecode, obfuscated or minified code, or any other binary blob.
pub fn is_not_source(bytes: &[u8]) -> bool {
    bytes.starts_with(b"FXAP")
        || bytes.starts_with(b"\x1bLua")
        || bytes.iter().take(1024).any(|b| *b == 0)
        || is_generated_code(bytes)
}

/// Obfuscators emit whole programs on one line; long data lines without functions stay linted.
fn is_generated_code(bytes: &[u8]) -> bool {
    bytes.split(|b| *b == b'\n').any(|line| line.len() >= GENERATED_LINE_BYTES && has_function_keyword(line))
}

fn has_function_keyword(line: &[u8]) -> bool {
    let is_word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    line.windows(8).enumerate().any(|(i, window)| {
        window == b"function" && !is_word(i.checked_sub(1).and_then(|p| line.get(p))) && !is_word(line.get(i + 8))
    })
}

/// Reads Lua source. Encrypted, binary or obfuscated files are an error, so every caller skips
/// them the same way it skips unreadable files. Invalid UTF-8 is decoded lossily for read-only
/// analysis; use `read_source_for_edit` before persisting edits.
pub fn read_source(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    if is_not_source(&bytes) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "encrypted, binary or obfuscated file"));
    }
    Ok(match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
    })
}

/// Reads editable Lua source without replacing invalid UTF-8 bytes. `None` means an encrypted,
/// binary or obfuscated file, which should be skipped rather than treated as a source encoding error.
pub fn read_source_for_edit(path: &Path) -> std::io::Result<Option<String>> {
    let bytes = std::fs::read(path)?;
    if is_not_source(&bytes) {
        return Ok(None);
    }
    String::from_utf8(bytes).map(Some).map_err(|err| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("source is not valid UTF-8: {err}"))
    })
}

pub fn find_manifest_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() { Some(start) } else { start.parent() };
    let mut root = None;

    while let Some(current) = dir {
        if MANIFEST_FILE_NAMES.iter().any(|name| current.join(name).is_file()) {
            // FiveM does not discover resources nested inside another resource.
            root = Some(current.to_path_buf());
        }

        dir = current.parent();
    }

    root
}

pub fn manifest_path(resource_root: &Path) -> Option<PathBuf> {
    if find_manifest_dir(resource_root).as_deref() != Some(resource_root) {
        return None;
    }

    manifest_in(resource_root)
}

pub fn manifest_in(dir: &Path) -> Option<PathBuf> {
    MANIFEST_FILE_NAMES.iter().map(|name| dir.join(name)).find(|p| p.is_file())
}

pub fn is_manifest_file(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| MANIFEST_FILE_NAMES.contains(&n))
}

pub fn relative_slash_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// The real path of `entry` when it is a folder that a walk reached through a symbolic link.
fn linked_folder(entry: &DirEntry) -> Option<PathBuf> {
    let linked = entry.depth() > 0 && entry.path_is_symlink() && entry.file_type().is_dir();
    linked.then(|| entry.path().canonicalize().ok()).flatten()
}

/// Whether a folder at `target` that a link leads to is somewhere else than the folders a walk from
/// `root`, by its real path, covers anyway, or one above them, which it would walk again.
fn leads_elsewhere(root: Option<&Path>, target: &Path) -> bool {
    root.is_some_and(|root| !target.starts_with(root) && !root.starts_with(target))
}

/// Whether a walk from `root`, by its real path, goes into `entry`. FiveM loads resources from
/// symlinked folders too, as a server's `[local]` linked to where they are developed, so walks
/// follow links, but only those that lead somewhere else.
pub fn walks_into(root: Option<&Path>, entry: &DirEntry) -> bool {
    linked_folder(entry).is_none_or(|target| leads_elsewhere(root, &target))
}

/// The Lua files under `root`, sorted. Symbolic links are followed as `walks_into` decides, and a
/// folder that several of them lead to is read once, through the first by name.
pub fn lua_files_under(root: &Path, config: &Config) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let canonical_root = root.canonicalize().ok();
    let mut linked: FxHashSet<PathBuf> = FxHashSet::default();
    let walker = WalkDir::new(root).follow_links(true).sort_by_file_name().into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        let hidden_dir = entry.file_type().is_dir() && name.starts_with('.') && entry.depth() > 0;
        if hidden_dir || name == "node_modules" || config.excludes_entry(entry.path()) {
            return false;
        }
        match linked_folder(entry) {
            Some(target) => leads_elsewhere(canonical_root.as_deref(), &target) && linked.insert(target),
            None => true,
        }
    });
    for entry in walker.flatten() {
        if entry.file_type().is_file() && entry.path().extension().is_some_and(|e| e == "lua") {
            files.push(entry.into_path());
        }
    }
    files.sort();
    files
}

/// The side a script runs on according to the manifest; `None` when it is not listed as a script
/// (for example modules loaded through `require` or `lib.load`). `Config::side_for` may still give
/// such a file a side.
pub fn side_of(manifest: &Manifest, relative_path: &str) -> Option<Side> {
    let mut side: Option<Side> = None;
    for entry in manifest.scripts.iter().filter(|s| !s.is_import()) {
        if manifest_glob_match(&entry.pattern, relative_path) {
            side = Some(match side {
                None => entry.side,
                Some(existing) if existing == entry.side => existing,
                Some(_) => Side::Shared,
            });
        }
    }
    side
}

pub struct ParsedFile {
    pub path: PathBuf,
    pub relative: String,
    pub source: String,
    pub chunk: Chunk,
    pub resolution: Resolution,
    pub summary: FileSummary,
    pub side: Option<Side>,
}

impl ParsedFile {
    pub fn new(path: PathBuf, relative: String, source: String, side: Option<Side>) -> Self {
        let chunk = parse(&source);
        let resolution = resolve(&chunk);
        let summary = summarize(&source, &chunk, &resolution);
        Self { path, relative, source, chunk, resolution, summary, side }
    }
}

#[derive(Clone, Debug)]
pub struct UnresolvedImport {
    pub path: SmolStr,
    pub side: Side,
}

/// The side of the file that assigns a function, and its signature as in `FunctionDef`.
type SidedSignature = (Option<Side>, Option<Arc<FunType>>);

/// Where a global that a definition file outside any resource declares exists at runtime.
#[derive(Clone, Debug)]
pub struct Declaration {
    pub side: Option<Side>,
    /// The resource whose import provides the global, as `ox_core` does for `player`.
    pub provider: Option<SmolStr>,
}

/// The globals that definition files outside any resource declare, such as type libraries.
pub type Declarations = FxHashMap<SmolStr, Vec<Declaration>>;

/// The globals visible to scripts of one resource, split by the side they are loaded on.
#[derive(Clone, Debug, Default)]
pub struct ResourceEnv {
    client: FxHashSet<SmolStr>,
    server: FxHashSet<SmolStr>,
    field_defs: FxHashSet<(SmolStr, SmolStr)>,
    file_scope: FxHashSet<SmolStr>,
    /// `@resource/file.lua` patterns the scripts load at runtime through `lib.load` or `require`.
    module_imports: FxHashSet<SmolStr>,
    /// Manifest and configured imports, with the side they run on.
    imports: Vec<(SmolStr, Side)>,
    /// Every assignment to a global or global table field by dotted path, with the side of its file.
    functions: FxHashMap<SmolStr, Vec<SidedSignature>>,
    aliases: FxHashMap<SmolStr, Type>,
    /// Globals that definition files outside any resource describe, such as type libraries. They
    /// exist at runtime on the side of their file, and those of a provider only with its import.
    declared: Arc<Declarations>,
    pub unresolved_imports: Vec<UnresolvedImport>,
    /// Part of the resource is encrypted or unreadable, so neither what it defines nor what it
    /// uses is known; rules that need the whole picture stay quiet.
    pub opaque: bool,
}

/// Whether what a file on side `def` defines is there for code on `side`.
fn is_visible_on(def: Option<Side>, side: Option<Side>) -> bool {
    match side {
        Some(Side::Client) => def != Some(Side::Server),
        Some(Side::Server) => def != Some(Side::Client),
        _ => true,
    }
}

/// The FiveM asset escrow leaves a `.fxap` file in the root of every resource it protects.
pub fn is_escrowed_resource(resource_root: &Path) -> bool {
    resource_root.join(".fxap").is_file()
}

impl ResourceEnv {
    pub fn add_global(&mut self, name: &SmolStr, side: Option<Side>) {
        if side != Some(Side::Server) {
            self.client.insert(name.clone());
        }
        if side != Some(Side::Client) {
            self.server.insert(name.clone());
        }
    }

    pub fn add_summary(&mut self, summary: &FileSummary, side: Option<Side>) {
        for def in &summary.global_defs {
            self.add_global(&def.name, side);
            if def.at_file_scope {
                self.file_scope.insert(def.name.clone());
            }
        }
        self.field_defs.extend(summary.global_field_defs.iter().cloned());
        self.module_imports.extend(summary.module_imports.iter().cloned());
        for def in &summary.functions {
            self.functions.entry(def.path.clone()).or_default().push((side, def.signature.clone()));
        }
        for (name, ty) in &summary.aliases {
            let merged = match self.aliases.remove(name) {
                Some(existing) => Type::union([existing, ty.clone()]),
                None => ty.clone(),
            };
            self.aliases.insert(name.clone(), merged);
        }
    }

    /// The signatures of the assignments to the global or global table field at `path` that code on
    /// `side` can see. `None` stands for a value whose parameters are unknown.
    pub fn function_defs(&self, path: &str, side: Option<Side>) -> impl Iterator<Item = Option<&Arc<FunType>>> {
        self.functions
            .get(path)
            .into_iter()
            .flatten()
            .filter(move |(def, _)| is_visible_on(*def, side))
            .map(|(_, sig)| sig.as_ref())
    }

    pub fn alias(&self, name: &str) -> Option<&Type> {
        self.aliases.get(name)
    }

    /// Whether a script of the resource loads `@resource/file.lua` itself at runtime, so the
    /// globals that file defines need no manifest entry.
    pub fn loads_module(&self, pattern: &str) -> bool {
        self.module_imports.contains(pattern)
    }

    /// Whether a manifest or configured import loads `path` on `side`.
    pub fn imports_path(&self, path: &str, side: Side) -> bool {
        self.imports
            .iter()
            .any(|(pattern, imported_side)| pattern.eq_ignore_ascii_case(path) && imported_side.is_available_on(side))
    }

    pub fn defines(&self, name: &str, side: Option<Side>) -> bool {
        match side {
            Some(Side::Client) => self.client.contains(name),
            Some(Side::Server) => self.server.contains(name),
            Some(Side::Shared) => self.client.contains(name) || self.server.contains(name),
            None => self.client.contains(name) || self.server.contains(name),
        }
    }

    pub fn set_declarations(&mut self, declared: Arc<Declarations>) {
        self.declared = declared;
    }

    /// Whether a definition file declares `name` for scripts on `side` of this resource.
    pub fn declares(&self, name: &str, side: Option<Side>) -> bool {
        let Some(declarations) = self.declared.get(name) else { return false };
        declarations.iter().any(|declaration| {
            is_visible_on(declaration.side, side)
                && declaration.provider.as_deref().is_none_or(|provider| self.imports_from(provider, side))
        })
    }

    /// Whether a manifest, configured or runtime import loads a file of `resource` for `side`.
    fn imports_from(&self, resource: &str, side: Option<Side>) -> bool {
        let names = |pattern: &str| split_import(pattern).is_some_and(|(name, _)| name.eq_ignore_ascii_case(resource));
        self.imports.iter().any(|(pattern, imported)| names(pattern) && is_visible_on(Some(*imported), side))
            || self.module_imports.iter().any(|pattern| names(pattern))
    }

    pub fn declared_at_file_scope(&self, name: &str) -> bool {
        self.file_scope.contains(name)
    }

    pub fn defines_field(&self, table: &str, field: &str) -> bool {
        self.field_defs.contains(&(SmolStr::new(table), SmolStr::new(field)))
    }

    pub fn has_unresolved_import_for(&self, side: Option<Side>) -> Option<&UnresolvedImport> {
        self.unresolved_imports.iter().find(|import| side.is_none_or(|s| import.side.is_available_on(s)))
    }

    /// The environment of a file that `loaders`, other resources, also run: what they define and
    /// import is there too, though this resource's own aliases win. It is opaque when this resource
    /// is, or when every loader is.
    pub fn with_loaders<'a>(&self, loaders: impl IntoIterator<Item = &'a ResourceEnv>) -> ResourceEnv {
        let mut env = self.clone();
        let mut loaded = false;
        let mut all_opaque = true;
        for loader in loaders {
            loaded = true;
            all_opaque &= loader.opaque;
            env.client.extend(loader.client.iter().cloned());
            env.server.extend(loader.server.iter().cloned());
            env.field_defs.extend(loader.field_defs.iter().cloned());
            env.file_scope.extend(loader.file_scope.iter().cloned());
            env.module_imports.extend(loader.module_imports.iter().cloned());
            env.imports.extend(loader.imports.iter().cloned());
            for (path, defs) in &loader.functions {
                env.functions.entry(path.clone()).or_default().extend(defs.iter().cloned());
            }
            for (name, ty) in &loader.aliases {
                env.aliases.entry(name.clone()).or_insert_with(|| ty.clone());
            }
            env.unresolved_imports.extend(loader.unresolved_imports.iter().cloned());
        }
        env.opaque |= loaded && all_opaque;
        env
    }

    /// Adds what an `@resource/path` import provides on `side`: the globals of the files it names,
    /// and the usual globals of well-known imports such as `@ox_lib/init.lua`, which also cover
    /// imports whose resource is not installed. Any other import that names no file is recorded as
    /// unresolved.
    pub fn add_import<'a>(&mut self, pattern: &str, side: Side, files: impl IntoIterator<Item = &'a FileSummary>) {
        self.imports.push((pattern.into(), side));
        let mut resolved = false;
        for summary in files {
            self.add_summary(summary, Some(side));
            resolved = true;
        }
        match known_import(pattern) {
            Some(known) => known.globals.iter().for_each(|g| self.add_global(&SmolStr::new(g), Some(side))),
            None if !resolved => self.unresolved_imports.push(UnresolvedImport { path: pattern.into(), side }),
            None => {}
        }
    }
}

/// Every Lua import of a resource with the side it runs on: the `@resource/path` entries of its
/// manifest, then the `imports` the configuration adds for files it loads at runtime.
pub fn resource_imports<'a>(manifest: &'a Manifest, manifest_path: &Path, config: &'a Config) -> Vec<(&'a str, Side)> {
    let own = manifest.imports().filter(|s| s.is_lua()).map(|s| (s.pattern.as_str(), s.side));
    own.chain(config.imports_for(manifest_path)).collect()
}

/// Finds sibling resources by name so `@resource/file.lua` imports can be followed, and keeps
/// what it read for them, since many resources often import the same files.
#[derive(Default)]
pub struct ResourceLocator {
    roots: FxHashMap<PathBuf, FxHashMap<String, PathBuf>>,
    lua_files: FxHashMap<PathBuf, Vec<PathBuf>>,
    summaries: FxHashMap<PathBuf, Option<FileSummary>>,
    manifests: FxHashMap<PathBuf, Option<(PathBuf, Manifest)>>,
    envs: FxHashMap<PathBuf, ResourceEnv>,
}

impl ResourceLocator {
    fn resources_dir(resource_root: &Path) -> Option<PathBuf> {
        let mut dir = resource_root.parent()?;
        while dir.file_name().is_some_and(|n| {
            let n = n.to_string_lossy();
            n.starts_with('[') && n.ends_with(']')
        }) {
            dir = dir.parent()?;
        }
        Some(dir.to_path_buf())
    }

    pub fn locate(&mut self, from_resource: &Path, name: &str) -> Option<PathBuf> {
        let dir = Self::resources_dir(from_resource)?;
        self.resource_roots(dir).get(&name.to_lowercase()).cloned()
    }

    /// The resources in the resources folder `dir`, by lowercase name.
    fn resource_roots(&mut self, dir: PathBuf) -> &FxHashMap<String, PathBuf> {
        self.roots.entry(dir.clone()).or_insert_with(|| {
            let mut index = FxHashMap::default();
            let root = dir.canonicalize().ok();
            let walker = WalkDir::new(&dir).max_depth(6).follow_links(true).into_iter().filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                let hidden = entry.depth() > 0 && (name.starts_with('.') || name == "node_modules");
                !hidden && walks_into(root.as_deref(), entry)
            });
            for entry in walker.flatten() {
                if entry.file_type().is_file() && is_manifest_file(entry.path()) {
                    if let Some(root) = entry.path().parent().filter(|root| manifest_path(root).is_some()) {
                        if let Some(name) = root.file_name() {
                            index.entry(name.to_string_lossy().to_lowercase()).or_insert_with(|| root.to_path_buf());
                        }
                    }
                }
            }
            index
        })
    }

    /// The summaries of the readable files an `@resource/path` import names, where the path may be
    /// a manifest glob, read from the resource of that name next to `from_resource`.
    pub fn import_summaries(&mut self, from_resource: &Path, pattern: &str, config: &Config) -> Vec<&FileSummary> {
        let Some((resource, file)) = split_import(pattern) else { return Vec::new() };
        let Some(root) = self.locate(from_resource, resource) else { return Vec::new() };
        let paths = if is_glob(file) {
            let all = self.lua_files.entry(root.clone()).or_insert_with(|| lua_files_under(&root, config));
            all.iter().filter(|path| manifest_glob_match(file, &relative_slash_path(&root, path))).cloned().collect()
        } else {
            let path = root.join(file);
            if config.is_excluded(&path) {
                return Vec::new();
            }
            vec![path]
        };
        for path in &paths {
            self.read_summary(path);
        }
        paths.iter().filter_map(|path| self.summaries.get(path)?.as_ref()).collect()
    }

    /// Reads the summary of the file at `path` once; `None` when it cannot be read as Lua source.
    fn read_summary(&mut self, path: &Path) -> Option<&FileSummary> {
        let summary = self.summaries.entry(path.to_path_buf()).or_insert_with(|| {
            let source = read_source(path).ok()?;
            let chunk = parse(&source);
            Some(summarize(&source, &chunk, &resolve(&chunk)))
        });
        summary.as_ref()
    }

    /// The resources next to `resource_root`, the resource `name`, whose manifest or configured
    /// imports name files of it, with the path of each such import inside it.
    pub fn loaders(&mut self, resource_root: &Path, name: &str, config: &Config) -> Vec<(PathBuf, String)> {
        let Some(dir) = Self::resources_dir(resource_root) else { return Vec::new() };
        let mut roots: Vec<PathBuf> = self.resource_roots(dir).values().cloned().collect();
        roots.sort();
        let mut found = Vec::new();
        for root in roots.into_iter().filter(|root| root != resource_root) {
            let Some((manifest_path, manifest)) = self.manifest_of(&root) else { continue };
            for (pattern, _) in resource_imports(manifest, manifest_path, config) {
                match split_import(pattern) {
                    Some((imported, path)) if imported.eq_ignore_ascii_case(name) => {
                        found.push((root.clone(), path.to_string()));
                    }
                    _ => {}
                }
            }
        }
        found
    }

    /// The manifest of the resource at `root` and its path, read once.
    fn manifest_of(&mut self, root: &Path) -> Option<&(PathBuf, Manifest)> {
        let manifest = self.manifests.entry(root.to_path_buf()).or_insert_with(|| {
            let path = manifest_path(root)?;
            let manifest = Manifest::from_chunk(&parse(&read_source(&path).ok()?));
            Some((path, manifest))
        });
        manifest.as_ref()
    }

    /// The globals the scripts of the resource at `root` see, for the files that it loads from
    /// other resources: those of its own files and of its imports.
    pub fn loader_env(&mut self, root: &Path, config: &Config) -> Option<ResourceEnv> {
        if let Some(env) = self.envs.get(root) {
            return Some(env.clone());
        }
        let (manifest_path, manifest) = self.manifest_of(root)?.clone();
        let mut env = ResourceEnv { opaque: is_escrowed_resource(root), ..ResourceEnv::default() };
        let paths = self.lua_files.entry(root.to_path_buf()).or_insert_with(|| lua_files_under(root, config)).clone();
        for path in paths.iter().filter(|path| !is_manifest_file(path)) {
            let side = side_of(&manifest, &relative_slash_path(root, path)).or_else(|| config.side_for(path));
            match self.read_summary(path) {
                Some(summary) => env.add_summary(summary, side),
                None => env.opaque = true,
            }
        }
        for (pattern, side) in resource_imports(&manifest, &manifest_path, config) {
            env.add_import(pattern, side, self.import_summaries(root, pattern, config));
        }
        self.envs.insert(root.to_path_buf(), env.clone());
        Some(env)
    }
}

pub fn split_import(pattern: &str) -> Option<(&str, &str)> {
    pattern.strip_prefix('@')?.split_once('/')
}

/// Whether an import of the file `path` of a resource, as `@lb-bridge/phone/load.lua` imports
/// `phone/load.lua`, runs its file at `relative`, a Lua file that its manifest does not list as a
/// script, in the importing resource: those in the folder of the file it names and below do, since
/// such an import is usually a loader that runs them with `load(LoadResourceFile(...))`, `require` or
/// `lib.load`. A glob names the folder before its first wildcard.
pub fn import_loads(path: &str, relative: &str) -> bool {
    let fixed = path.find('*').map_or(path, |wildcard| &path[..wildcard]);
    let folder = fixed.rfind('/').map_or("", |slash| &fixed[..=slash]);
    relative.get(..folder.len()).is_some_and(|start| start.eq_ignore_ascii_case(folder))
}

pub struct Resource {
    pub name: String,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
    pub files: Vec<ParsedFile>,
    pub env: ResourceEnv,
    /// The environments of the files that other resources load, one for each set of loaders.
    loaded_envs: Vec<ResourceEnv>,
    /// For each of `files`, its environment in `loaded_envs` when other resources load it.
    file_envs: Vec<Option<usize>>,
}

impl Resource {
    /// The globals that the file at `index` of `files` sees: those of the resource, and of the
    /// resources that load it when its manifest does not list it as a script.
    pub fn env_for(&self, index: usize) -> &ResourceEnv {
        match self.file_envs.get(index).copied().flatten() {
            Some(loaded) => &self.loaded_envs[loaded],
            None => &self.env,
        }
    }

    pub fn load(root: &Path, config: &Config, locator: &mut ResourceLocator) -> Option<Self> {
        let manifest_path = manifest_path(root)?;
        let manifest_source = read_source(&manifest_path).ok()?;
        let manifest = Manifest::from_chunk(&parse(&manifest_source));

        let mut env = ResourceEnv { opaque: is_escrowed_resource(root), ..ResourceEnv::default() };
        let mut files = Vec::new();
        for path in lua_files_under(root, config) {
            if is_manifest_file(&path) {
                continue;
            }
            let Ok(source) = read_source(&path) else {
                env.opaque = true;
                continue;
            };
            let relative = relative_slash_path(root, &path);
            let side = side_of(&manifest, &relative).or_else(|| config.side_for(&path));
            let file = ParsedFile::new(path, relative, source, side);
            env.add_summary(&file.summary, side);
            files.push(file);
        }
        for (pattern, side) in resource_imports(&manifest, &manifest_path, config) {
            env.add_import(pattern, side, locator.import_summaries(root, pattern, config));
        }
        // What `loader_env` finds for this resource, for the files it loads from other resources.
        locator.envs.entry(root.to_path_buf()).or_insert_with(|| env.clone());
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let unlisted: Vec<Option<&str>> = files
            .iter()
            .map(|file| side_of(&manifest, &file.relative).is_none().then_some(file.relative.as_str()))
            .collect();
        let loads_any = |path: &str| unlisted.iter().flatten().any(|relative| import_loads(path, relative));
        let loaders: Vec<(String, ResourceEnv)> = locator
            .loaders(root, &name, config)
            .into_iter()
            .filter(|(_, path)| loads_any(path))
            .filter_map(|(loader, path)| Some((path, locator.loader_env(&loader, config)?)))
            .collect();
        let mut loaded_envs = Vec::new();
        let mut loader_sets: Vec<Vec<usize>> = Vec::new();
        let file_envs = unlisted
            .iter()
            .map(|relative| {
                let relative = (*relative)?;
                let loading = loaders.iter().enumerate().filter(|(_, (path, _))| import_loads(path, relative));
                let set: Vec<usize> = loading.map(|(i, _)| i).collect();
                if set.is_empty() {
                    return None;
                }
                Some(loader_sets.iter().position(|known| *known == set).unwrap_or_else(|| {
                    loaded_envs.push(env.with_loaders(set.iter().map(|i| &loaders[*i].1)));
                    loader_sets.push(set);
                    loader_sets.len() - 1
                }))
            })
            .collect();
        Some(Self { name, root: root.to_path_buf(), manifest_path, manifest, files, env, loaded_envs, file_envs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obfuscated_code_is_not_source() {
        let program = "local a=function(z,z)return z end;".repeat(200);
        assert!(is_not_source(format!("-- protected\n\n{program}\n").as_bytes()));
        assert!(is_not_source(format!("return(function(...){}end)(...)", "x=1;".repeat(1100)).as_bytes()));
    }

    #[test]
    fn long_data_lines_and_short_functions_are_source() {
        let table = format!("local t={{{}}}\nlocal f=function(z,z)end\n", "1, ".repeat(2000));
        assert!(!is_not_source(table.as_bytes()));
        assert!(!is_not_source(format!("local avatar='{}'\n", "A".repeat(8000)).as_bytes()));
        assert!(!is_not_source(format!("local t={{{}}}\n", "functions=1,_function=2,".repeat(200)).as_bytes()));
    }

    /// Links `link` to the folder `target`, unless the platform refuses to, as Windows refuses
    /// symbolic links without the right to create them; a junction needs no such right.
    fn link_folder(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link).is_ok();
        #[cfg(windows)]
        return std::os::windows::fs::symlink_dir(target, link).is_ok()
            || std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|output| output.status.success());
        #[cfg(not(any(unix, windows)))]
        false
    }

    #[test]
    fn lua_files_under_follows_linked_folders_once() {
        let root = std::env::temp_dir().join(format!("qbx-linked-folders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // `mklink` takes no forward slashes.
        let at = |relative: &str| relative.split('/').fold(root.clone(), |path, part| path.join(part));
        for file in ["elsewhere/racing/client.lua", "server/resources/[core]/core.lua", "server/cache/stray.lua"] {
            std::fs::create_dir_all(at(file).parent().unwrap()).unwrap();
            std::fs::write(at(file), "").unwrap();
        }
        let resources = at("server/resources");
        let links = [
            ("elsewhere", "server/resources/[symlink]"),
            ("elsewhere", "server/resources/[again]"),
            ("server/resources/[core]", "server/resources/[core-link]"),
            ("server/resources", "server/resources/[core]/loop"),
            ("server", "server/resources/[up]"),
        ];
        if !links.iter().all(|(target, link)| link_folder(&at(target), &at(link))) {
            eprintln!("skipped: folder links cannot be created here");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let files = lua_files_under(&resources, &Config::default());
        let relative: Vec<String> = files.iter().map(|file| relative_slash_path(&resources, file)).collect();
        let _ = std::fs::remove_dir_all(&root);
        // The folder two links lead to is read through the first, and links into the tree, or to a
        // folder above it, are left out.
        assert_eq!(relative, ["[again]/racing/client.lua", "[core]/core.lua"]);
    }

    #[test]
    fn declarations_need_their_side_and_the_import_of_their_provider() {
        let declare = |side, provider: Option<&str>| Declaration { side, provider: provider.map(SmolStr::new) };
        let mut declarations = Declarations::default();
        declarations.insert("Sql".into(), vec![declare(None, None)]);
        declarations.insert("vehicle".into(), vec![declare(Some(Side::Server), Some("ox_core"))]);
        let mut env = ResourceEnv::default();
        env.set_declarations(Arc::new(declarations));

        assert!(env.declares("Sql", Some(Side::Client)) && env.declares("Sql", None));
        assert!(!env.declares("vehicle", Some(Side::Server)), "ox_core is not imported");
        env.add_import("@ox_core/lib/init.lua", Side::Client, []);
        assert!(!env.declares("vehicle", Some(Side::Server)), "imported for the client only");
        env.add_import("@ox_core/lib/init.lua", Side::Shared, []);
        assert!(env.declares("vehicle", Some(Side::Server)) && env.declares("vehicle", None));
        assert!(!env.declares("vehicle", Some(Side::Client)), "declared for the server");
    }
}
