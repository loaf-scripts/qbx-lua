use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;
use qbx_fivem_data::Side;
use rustc_hash::FxHashMap;
use serde::Deserialize;

use crate::diagnostic::Severity;
use crate::project::split_import;
use crate::{lua_ls_config, rules};

pub const CONFIG_FILE_NAMES: &[&str] = &["qbxlint.toml", ".qbxlint.toml"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Off,
    Hint,
    Info,
    #[serde(alias = "warn")]
    Warning,
    Error,
}

impl Level {
    fn severity(self) -> Option<Severity> {
        match self {
            Level::Off => None,
            Level::Hint => Some(Severity::Hint),
            Level::Info => Some(Severity::Info),
            Level::Warning => Some(Severity::Warning),
            Level::Error => Some(Severity::Error),
        }
    }
}

/// The database server whose reserved words the `sql/` rules read queries for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Database {
    /// MariaDB, which oxmysql recommends.
    #[default]
    MariaDb,
    MySql,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawConfig {
    root: bool,
    exclude: Vec<String>,
    ignore_diagnostics: Vec<String>,
    globals: Vec<String>,
    ignore_unused_prefix: Option<String>,
    strict_classes: Option<bool>,
    strict: Option<bool>,
    database: Option<Database>,
    rules: BTreeMap<String, Level>,
    overrides: Vec<RawOverride>,
    imports: Imports,
    #[serde(deserialize_with = "format_table")]
    format: toml::Table,
}

/// The `[format]` table as written, so that a nearer file can change some of its keys, checked
/// against the options it may set.
fn format_table<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<toml::Table, D::Error> {
    let table = toml::Table::deserialize(deserializer)?;
    table.clone().try_into::<qbx_lua_fmt::FormatOptions>().map_err(serde::de::Error::custom)?;
    Ok(table)
}

/// Files a resource runs without an fxmanifest.lua entry, for example through
/// `load(LoadResourceFile(...))`, as `@resource/path` patterns grouped by the side they run on.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Imports {
    shared: Vec<String>,
    client: Vec<String>,
    server: Vec<String>,
}

impl Imports {
    fn entries(&self) -> impl Iterator<Item = (&str, Side)> {
        [(&self.shared, Side::Shared), (&self.client, Side::Client), (&self.server, Side::Server)]
            .into_iter()
            .flat_map(|(patterns, side)| patterns.iter().map(move |p| (p.as_str(), side)))
    }
}

/// The side an override gives the scripts of a resource that its fxmanifest.lua does not list.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ScriptSide {
    Shared,
    Client,
    Server,
}

impl From<ScriptSide> for Side {
    fn from(side: ScriptSide) -> Self {
        match side {
            ScriptSide::Shared => Side::Shared,
            ScriptSide::Client => Side::Client,
            ScriptSide::Server => Side::Server,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawOverride {
    files: Vec<String>,
    globals: Vec<String>,
    strict: Option<bool>,
    rules: BTreeMap<String, Level>,
    imports: Imports,
    side: Option<ScriptSide>,
}

#[derive(Debug)]
struct Override {
    files: GlobSet,
    globals: Vec<String>,
    strict: Option<bool>,
    rules: BTreeMap<String, Level>,
    imports: Imports,
    side: Option<Side>,
}

/// The settings of one `qbxlint.toml`, or of the LuaLS or EmmyLua files of one folder. Its patterns
/// are relative to `root`, the folder it is in.
#[derive(Debug)]
struct Layer {
    root: PathBuf,
    /// Whether it says `root = true`, so that the files above it do not apply.
    stops_search: bool,
    exclude: GlobSet,
    ignore_diagnostics: Gitignore,
    globals: Vec<String>,
    ignore_unused_prefix: Option<String>,
    strict_classes: Option<bool>,
    strict: Option<bool>,
    database: Option<Database>,
    /// The `[format]` table of a `qbxlint.toml`, empty when it has none; `None` for LuaLS or
    /// EmmyLua settings, which leave formatting to the editor.
    format: Option<toml::Table>,
    rules: BTreeMap<String, Level>,
    overrides: Vec<Override>,
    imports: Imports,
    /// Which LuaLS or EmmyLua files discovery fell back to, and what it skipped in them.
    notes: Vec<String>,
}

/// The layers that apply to a path, outermost first, so that nearer ones take precedence.
type Chain = Arc<[Arc<Layer>]>;

/// The configuration of a run: one file that applies to every path, as `--config` gives, or, once
/// discovered, the configuration files of each path's folder and of the folders above it.
#[derive(Debug, Default)]
pub struct Config {
    /// What applies to every path when nothing is discovered: no file, or the one read.
    fixed: Chain,
    discovery: Option<Discovery>,
    /// Levels from the command line, which take the place of those the `[rules]` tables give, and
    /// apply where none gives one.
    rules: BTreeMap<String, Level>,
}

/// The configuration files found in the folders looked up so far.
#[derive(Debug, Default)]
struct Discovery {
    folders: RwLock<FxHashMap<PathBuf, Folder>>,
    /// The files that could not be read, by path, with why.
    errors: Mutex<BTreeMap<PathBuf, String>>,
}

#[derive(Clone, Debug, Default)]
struct Folder {
    /// What applies to the files in the folder: its `qbxlint.toml` and those above it, up to one
    /// that says `root = true`, or else the LuaLS or EmmyLua settings of the nearest folder with any.
    layers: Chain,
    /// Whether `layers` holds LuaLS or EmmyLua settings.
    fallback: bool,
}

const DEFAULT_EXCLUDES: &[&str] = &["**/node_modules/**", "**/.git/**", "**/[[]builders[]]/**"];

fn default_excludes() -> &'static GlobSet {
    static EXCLUDES: OnceLock<GlobSet> = OnceLock::new();
    EXCLUDES.get_or_init(|| build_globset(DEFAULT_EXCLUDES.iter().copied()).expect("default exclusions are valid"))
}

impl Config {
    /// A configuration in which `text`, a `qbxlint.toml` in the folder `root`, applies to every path.
    pub fn parse(text: &str, root: PathBuf) -> Result<Self, String> {
        Layer::parse(text, root).map(Self::fixed)
    }

    /// Loads a `qbxlint.toml`, or a LuaLS or EmmyLua JSON configuration by its extension, which then
    /// applies to every path.
    pub fn load(path: &Path) -> Result<Self, String> {
        Layer::load(path).map(Self::fixed)
    }

    fn fixed(layer: Layer) -> Self {
        Self { fixed: Arc::from([Arc::new(layer)]), ..Self::default() }
    }

    /// A configuration that gives each path the `qbxlint.toml` files of its folder and of the folders
    /// above it, combined so that nearer files take precedence, up to one that says `root = true`.
    /// Only where there is none do the LuaLS or EmmyLua settings of the nearest folder with any
    /// supply globals, rule levels and exclusions. Folders are looked up as paths in them are asked
    /// about, those of `start` right away, so `errors` and `notes` tell about them before anything
    /// is analyzed.
    pub fn discover(start: &Path) -> Self {
        let discovery = Discovery::default();
        let folder = if start.is_dir() { Some(start) } else { start.parent() };
        if let Some(folder) = folder {
            discovery.folder(folder);
        }
        Self { discovery: Some(discovery), ..Self::default() }
    }

    /// The configuration files that discovery could not read so far, and went on without.
    pub fn errors(&self) -> Vec<String> {
        let Some(discovery) = &self.discovery else { return Vec::new() };
        discovery.errors.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
    }

    /// Which LuaLS or EmmyLua files discovery fell back to so far, and what it skipped in them.
    pub fn notes(&self) -> Vec<String> {
        let layers = match &self.discovery {
            Some(discovery) => discovery.fallbacks(),
            None => self.fixed.to_vec(),
        };
        layers.iter().flat_map(|layer| layer.notes.iter().cloned()).collect()
    }

    /// The layers that apply to `path`, outermost first.
    fn layers(&self, path: &Path) -> Chain {
        self.layers_in(path.parent())
    }

    /// The layers that apply to the files in `folder`, outermost first.
    fn layers_in(&self, folder: Option<&Path>) -> Chain {
        match (&self.discovery, folder) {
            (Some(discovery), Some(folder)) => discovery.folder(folder).layers,
            _ => self.fixed.clone(),
        }
    }

    pub fn set_rule(&mut self, code: &str, level: Level) {
        self.rules.insert(code.to_string(), level);
    }

    pub fn is_excluded(&self, path: &Path) -> bool {
        let excluded =
            |relative: &Path, set: &GlobSet| relative.ancestors().any(|p| !p.as_os_str().is_empty() && set.is_match(p));
        let layers = self.layers(path);
        excluded(outermost_relative(&layers, path), default_excludes())
            || layers.iter().any(|layer| excluded(layer.relative(path), &layer.exclude))
    }

    /// A directory walk has already checked the entry's parent directories, so only the entry is matched.
    pub(crate) fn excludes_entry(&self, path: &Path) -> bool {
        let layers = self.layers(path);
        default_excludes().is_match(outermost_relative(&layers, path))
            || layers.iter().any(|layer| layer.exclude.is_match(layer.relative(path)))
    }

    /// Paths that are still analyzed, so their globals and exports count, but whose own findings are
    /// not reported. As with nested `.gitignore` files, the nearest file whose patterns match decides.
    pub fn ignores_diagnostics(&self, path: &Path) -> bool {
        for layer in self.layers(path).iter().rev() {
            let relative = layer.relative(path);
            // The matcher panics on paths outside its root.
            if relative.has_root() {
                continue;
            }
            match layer.ignore_diagnostics.matched_path_or_any_parents(relative, false) {
                Match::None => {}
                found => return found.is_ignore(),
            }
        }
        false
    }

    /// The rule levels and globals for the file at `path`. Each configuration file applies its own
    /// settings and then its matching overrides, outermost file first, so the settings of a nearer
    /// file take precedence over the overrides of those above it.
    pub fn for_file(&self, path: &Path) -> FileConfig {
        let layers = self.layers(path);
        let mut rules = self.rules.clone();
        let mut globals = Vec::new();
        let (mut prefix, mut database) = (None, None);
        for layer in layers.iter() {
            for (code, level) in &layer.rules {
                rules.insert(code.clone(), self.rules.get(code).copied().unwrap_or(*level));
            }
            globals.extend(layer.globals.iter().cloned());
            prefix = layer.ignore_unused_prefix.as_deref().or(prefix);
            database = layer.database.or(database);
            for entry in layer.matching(path) {
                rules.extend(entry.rules.iter().map(|(k, v)| (k.clone(), *v)));
                globals.extend(entry.globals.iter().cloned());
            }
        }
        FileConfig {
            rules,
            globals,
            ignore_unused_prefix: prefix.unwrap_or("_").to_string(),
            strict: strict_in(&layers, path),
            database: database.unwrap_or_default(),
        }
    }

    /// What the configuration sets `strict` to for the file at `path`, as the last setting or
    /// override that matches it and sets it does, and `None` where it leaves it to the editor.
    pub fn strict_at(&self, path: &Path) -> Option<bool> {
        strict_in(&self.layers(path), path)
    }

    /// Whether a `---@class` without `(strict)` or `(loose)` declared in the file at `path` is strict.
    /// Only classes declared in files whose diagnostics are reported follow it; qbx-lua-ls applies it.
    pub fn strict_classes_at(&self, path: &Path) -> bool {
        self.layers(path).iter().rev().find_map(|layer| layer.strict_classes).unwrap_or(false)
    }

    /// Whether `strict_classes` is on in a configuration file read so far.
    pub fn strict_classes_anywhere(&self) -> bool {
        let on = |layer: &Arc<Layer>| layer.strict_classes == Some(true);
        match &self.discovery {
            Some(discovery) => {
                let folders = discovery.folders.read().unwrap_or_else(|e| e.into_inner());
                folders.values().any(|folder| folder.layers.iter().any(on))
            }
            None => self.fixed.iter().any(on),
        }
    }

    /// The `[format]` options for the file at `path`, a nearer file's keys replacing those of the files
    /// above it, or `None` when no `qbxlint.toml` applies to it, so that editors keep their own.
    pub fn format_for(&self, path: &Path) -> Option<qbx_lua_fmt::FormatOptions> {
        merged_format(&self.layers(path))
    }

    /// The `[format]` options for the files in `folder`, as `format_for` gives them.
    pub fn format_in(&self, folder: &Path) -> Option<qbx_lua_fmt::FormatOptions> {
        merged_format(&self.layers_in(Some(folder)))
    }

    /// The configured `imports` of the resource whose manifest is `manifest_path`. The scripts of a
    /// resource share their globals, so an override adds its imports to every resource whose
    /// manifest its `files` patterns match.
    pub fn imports_for(&self, manifest_path: &Path) -> Vec<(String, Side)> {
        let layers = self.layers(manifest_path);
        let imports = layers
            .iter()
            .flat_map(|layer| std::iter::once(&layer.imports).chain(layer.matching(manifest_path).map(|o| &o.imports)));
        imports.flat_map(Imports::entries).map(|(pattern, side)| (pattern.to_string(), side)).collect()
    }

    /// The side the last matching override with a `side` gives the script at `path`. It only counts
    /// for scripts the manifest does not list, such as files a loader runs at runtime.
    pub fn side_for(&self, path: &Path) -> Option<Side> {
        let layers = self.layers(path);
        layers.iter().rev().find_map(|layer| layer.matching(path).rev().find_map(|o| o.side))
    }
}

/// `path` relative to the outermost of `layers`, against which the default exclusions are matched.
fn outermost_relative<'a>(layers: &[Arc<Layer>], path: &'a Path) -> &'a Path {
    layers.first().map_or(path, |layer| layer.relative(path))
}

fn merged_format(layers: &[Arc<Layer>]) -> Option<qbx_lua_fmt::FormatOptions> {
    let mut merged: Option<toml::Table> = None;
    for format in layers.iter().filter_map(|layer| layer.format.as_ref()) {
        merged.get_or_insert_with(toml::Table::new).extend(format.clone());
    }
    merged.map(|table| table.try_into().expect("each [format] table was checked when it was read"))
}

fn strict_in(layers: &[Arc<Layer>], path: &Path) -> Option<bool> {
    layers.iter().fold(None, |strict, layer| {
        layer.matching(path).fold(layer.strict.or(strict), |strict, entry| entry.strict.or(strict))
    })
}

impl Discovery {
    /// What applies in the folder `dir`, which is looked up once.
    fn folder(&self, dir: &Path) -> Folder {
        let known = self.folders.read().unwrap_or_else(|e| e.into_inner()).get(dir).cloned();
        if let Some(folder) = known {
            return folder;
        }
        let folder = self.look_up(dir);
        let mut folders = self.folders.write().unwrap_or_else(|e| e.into_inner());
        folders.entry(dir.to_path_buf()).or_insert(folder).clone()
    }

    fn look_up(&self, dir: &Path) -> Folder {
        let parent = dir.parent().map(|parent| self.folder(parent)).unwrap_or_default();
        let inherited: &[Arc<Layer>] = if parent.fallback { &[] } else { &parent.layers };
        let file = CONFIG_FILE_NAMES.iter().map(|name| dir.join(name)).find(|path| path.is_file());
        if let Some(layer) = file.and_then(|path| self.read(&path, Layer::load(&path))) {
            let layers: Vec<_> =
                if layer.stops_search { vec![layer] } else { inherited.iter().cloned().chain([layer]).collect() };
            return Folder { layers: layers.into(), fallback: false };
        }
        if !inherited.is_empty() {
            return parent;
        }
        let found: Vec<PathBuf> =
            lua_ls_config::FILE_NAMES.iter().map(|name| dir.join(name)).filter(|p| p.is_file()).collect();
        let fallback = found.first().and_then(|first| {
            let layer = Layer::load_lua_ls(&found, true).map_err(|e| format!("{}: {e}", first.display()));
            self.read(first, layer)
        });
        match fallback {
            Some(layer) => Folder { layers: Arc::from([layer]), fallback: true },
            None => parent,
        }
    }

    /// The layer read from `path`, or `None` with its error kept when it could not be read.
    fn read(&self, path: &Path, layer: Result<Layer, String>) -> Option<Arc<Layer>> {
        match layer {
            Ok(layer) => Some(Arc::new(layer)),
            Err(error) => {
                self.errors.lock().unwrap_or_else(|e| e.into_inner()).insert(path.to_path_buf(), error);
                None
            }
        }
    }

    /// The LuaLS or EmmyLua settings that apply in a folder looked up so far, in the order of their
    /// folders.
    fn fallbacks(&self) -> Vec<Arc<Layer>> {
        let folders = self.folders.read().unwrap_or_else(|e| e.into_inner());
        let layers = folders.values().filter(|folder| folder.fallback).flat_map(|folder| folder.layers.first());
        let by_root: BTreeMap<&Path, &Arc<Layer>> = layers.map(|layer| (layer.root.as_path(), layer)).collect();
        by_root.into_values().cloned().collect()
    }
}

impl Layer {
    fn parse(text: &str, root: PathBuf) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        Self::from_raw(raw, root)
    }

    /// Reads a `qbxlint.toml`, or a LuaLS or EmmyLua JSON configuration by its extension.
    fn load(path: &Path) -> Result<Self, String> {
        let path = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if path.extension().is_some_and(|e| e == "json" || e == "jsonc") {
            return Self::load_lua_ls(std::slice::from_ref(&path), false)
                .map_err(|e| format!("{}: {e}", path.display()));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Self::parse(&text, root).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Merges LuaLS or EmmyLua settings files from one directory. Settings and rule codes without
    /// a qbx-lint equivalent are ignored. When `lenient`, as during discovery, a file or pattern
    /// that cannot be read is skipped with a note, so another tool's configuration never stops a
    /// lint run.
    fn load_lua_ls(paths: &[PathBuf], lenient: bool) -> Result<Self, String> {
        let mut settings = lua_ls_config::Settings::default();
        let mut loaded = Vec::new();
        let mut notes = Vec::new();
        for path in paths {
            let emmylua = path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".emmyrc"));
            let result = std::fs::read_to_string(path)
                .map_err(|e| e.to_string())
                .and_then(|text| lua_ls_config::parse(&text, emmylua, &mut settings));
            match result {
                Ok(()) => loaded.push(path.display().to_string()),
                Err(e) if lenient => notes.push(format!("skipped {}: {e}", path.display())),
                Err(e) => return Err(e),
            }
        }
        let mut exclude = settings.exclude;
        if lenient {
            exclude.retain(|pattern| match Glob::new(pattern) {
                Ok(_) => true,
                Err(e) => {
                    notes.push(format!("skipped the ignore pattern '{pattern}': {e}"));
                    false
                }
            });
            if !loaded.is_empty() {
                let files = loaded.join(", ");
                notes.insert(0, format!("no qbxlint.toml found; falling back to the supported settings in {files}"));
            }
        }
        let raw = RawConfig {
            exclude,
            globals: settings.globals,
            rules: settings.rules.into_iter().collect(),
            ..RawConfig::default()
        };
        let root = paths.first().and_then(|path| path.parent()).map(Path::to_path_buf).unwrap_or_default();
        let mut layer = Self::from_raw(raw, root)?;
        layer.format = None;
        layer.notes = notes;
        Ok(layer)
    }

    fn from_raw(raw: RawConfig, root: PathBuf) -> Result<Self, String> {
        for code in raw.rules.keys().chain(raw.overrides.iter().flat_map(|o| o.rules.keys())) {
            if rules::find(code).is_none() {
                return Err(format!("unknown rule '{code}'"));
            }
        }
        let imports = std::iter::once(&raw.imports).chain(raw.overrides.iter().map(|o| &o.imports));
        for (pattern, _) in imports.flat_map(Imports::entries) {
            let lua = pattern.ends_with(".lua") || pattern.ends_with('*');
            if split_import(pattern).is_none() || !lua {
                return Err(format!(
                    "import '{pattern}' must name Lua files as '@resource/path', such as '@lib/shared/**.lua'"
                ));
            }
        }
        let exclude = build_globset(raw.exclude.iter().map(String::as_str))?;
        let ignore_diagnostics = build_gitignore(&root, &raw.ignore_diagnostics)?;
        let overrides = raw
            .overrides
            .into_iter()
            .map(|o| {
                Ok(Override {
                    files: build_globset(o.files.iter().map(String::as_str))?,
                    globals: o.globals,
                    strict: o.strict,
                    rules: o.rules,
                    imports: o.imports,
                    side: o.side.map(Side::from),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            root,
            stops_search: raw.root,
            exclude,
            ignore_diagnostics,
            globals: raw.globals,
            ignore_unused_prefix: raw.ignore_unused_prefix,
            strict_classes: raw.strict_classes,
            strict: raw.strict,
            database: raw.database,
            format: Some(raw.format),
            rules: raw.rules,
            overrides,
            imports: raw.imports,
            notes: Vec::new(),
        })
    }

    fn relative<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.root).unwrap_or(path)
    }

    /// The overrides whose `files` patterns match `path`, in the order they are written.
    fn matching<'a>(&'a self, path: &'a Path) -> impl DoubleEndedIterator<Item = &'a Override> + 'a {
        let relative = self.relative(path);
        self.overrides.iter().filter(move |o| o.files.is_match(relative))
    }
}

fn build_globset<'a>(patterns: impl Iterator<Item = &'a str>) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(|e| e.to_string())?);
    }
    builder.build().map_err(|e| e.to_string())
}

fn build_gitignore(root: &Path, patterns: &[String]) -> Result<Gitignore, String> {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        builder.add_line(None, pattern).map_err(|e| e.to_string())?;
    }
    builder.build().map_err(|e| e.to_string())
}

#[derive(Clone, Debug)]
pub struct FileConfig {
    rules: BTreeMap<String, Level>,
    pub globals: Vec<String>,
    pub ignore_unused_prefix: String,
    strict: Option<bool>,
    /// The database server that the `sql/` rules check queries for.
    pub database: Database,
}

impl Default for FileConfig {
    fn default() -> Self {
        Config::default().for_file(Path::new(""))
    }
}

impl FileConfig {
    pub fn severity(&self, code: &str) -> Option<Severity> {
        match self.rules.get(code) {
            Some(level) => level.severity(),
            None => rules::find(code).and_then(|r| r.default),
        }
    }

    pub fn set(&mut self, code: &str, level: Level) {
        self.rules.insert(code.to_string(), level);
    }

    /// Sets the level of a rule the configuration file leaves alone, as an editor setting does.
    pub fn set_default(&mut self, code: &str, level: Level) {
        self.rules.entry(code.to_string()).or_insert(level);
    }

    /// Whether the rules report what TypeScript's strict mode does where lua-language-server
    /// reports nothing, such as `n + 1` for a `number?` in `need-check-nil`. Off unless the
    /// configuration file or the editor turns it on.
    pub fn strict(&self) -> bool {
        self.strict.unwrap_or(false)
    }

    /// Sets `strict` unless the configuration file sets it, as an editor setting does.
    pub fn set_strict_default(&mut self, strict: bool) {
        self.strict.get_or_insert(strict);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules_and_overrides() {
        let config = Config::parse(
            r#"
            exclude = ["web/**"]
            globals = ["MyGlobal"]
            [rules]
            "unused-argument" = "off"
            "fivem/citizen-prefix" = "error"
            [[overrides]]
            files = ["tests/**"]
            rules = { "undefined-global" = "off" }
            "#,
            PathBuf::from("/repo"),
        )
        .unwrap();
        assert!(config.is_excluded(Path::new("/repo/web/app.lua")));
        assert!(config.is_excluded(Path::new("/repo/x/node_modules/y/z.lua")));
        let file = config.for_file(Path::new("/repo/client/main.lua"));
        assert_eq!(file.severity("unused-argument"), None);
        assert_eq!(file.severity("fivem/citizen-prefix"), Some(Severity::Error));
        assert_eq!(file.severity("undefined-global"), Some(Severity::Warning));
        assert_eq!(config.for_file(Path::new("/repo/tests/a.lua")).severity("undefined-global"), None);
        assert!(!config.strict_classes_at(Path::new("/repo/client/main.lua")));
        let strict_classes = Config::parse("strict_classes = true", PathBuf::from("/repo")).unwrap();
        assert!(strict_classes.strict_classes_at(Path::new("/repo/client/main.lua")));
    }

    #[test]
    fn database_defaults_to_mariadb() {
        let file =
            |text: &str| Config::parse(text, PathBuf::from("/repo")).map(|c| c.for_file(Path::new("/repo/a.lua")));
        assert_eq!(file("").unwrap().database, Database::MariaDb);
        assert_eq!(file("database = 'mysql'").unwrap().database, Database::MySql);
        assert_eq!(file("database = 'mariadb'").unwrap().database, Database::MariaDb);
        assert!(file("database = 'postgres'").is_err());
    }

    #[test]
    fn strict_comes_from_the_last_override_that_sets_it() {
        let config = Config::parse(
            r#"
            strict = true
            [[overrides]]
            files = ["vendor/**"]
            strict = false
            [[overrides]]
            files = ["vendor/ours/**"]
            strict = true
            [[overrides]]
            files = ["vendor/ours/**"]
            globals = ["Later"]
            "#,
            PathBuf::from("/repo"),
        )
        .unwrap();
        let strict = |path: &str| config.for_file(Path::new(&format!("/repo/{path}"))).strict();
        assert!(strict("client/main.lua"));
        assert!(!strict("vendor/lib.lua"));
        assert!(strict("vendor/ours/lib.lua"), "a later override without `strict` keeps it");

        let mut file = config.for_file(Path::new("/repo/vendor/lib.lua"));
        file.set_strict_default(true);
        assert!(!file.strict(), "the configuration file wins over the editor");
        let mut file = FileConfig::default();
        assert!(!file.strict());
        file.set_strict_default(true);
        assert!(file.strict());
    }

    #[test]
    fn a_file_is_excluded_when_one_of_its_directories_is() {
        let config = Config::parse("exclude = ['web']", PathBuf::from("/repo")).unwrap();
        assert!(config.is_excluded(Path::new("/repo/web/app.lua")));
        assert!(!config.is_excluded(Path::new("/repo/client/web.lua")));
    }

    #[test]
    fn ignore_diagnostics_uses_gitignore_patterns() {
        let config = Config::parse(
            r"ignore_diagnostics = ['\[standalone\]/', 'vendor', '!vendor/ours.lua', '/generated.lua']",
            PathBuf::from("/repo"),
        )
        .unwrap();
        let ignored = |path: &str| config.ignores_diagnostics(Path::new(&format!("/repo/{path}")));
        assert!(ignored("res/[standalone]/tool/main.lua"));
        assert!(!ignored("res/s/main.lua"), "escaped brackets are not a character class");
        assert!(ignored("vendor/lib.lua"));
        assert!(ignored("res/vendor/deep/lib.lua"));
        assert!(!ignored("vendor/ours.lua"));
        assert!(ignored("generated.lua"));
        assert!(!ignored("res/generated.lua"));
        assert!(!ignored("client/main.lua"));
        assert!(!config.ignores_diagnostics(Path::new("/elsewhere/vendor/lib.lua")));
        assert!(!config.is_excluded(Path::new("/repo/vendor/lib.lua")));
    }

    #[test]
    fn imports_apply_to_resources_whose_manifest_an_override_matches() {
        let config = Config::parse(
            r#"
            [imports]
            shared = ["@lib/shared/**.lua"]
            [[overrides]]
            files = ["resources/[[]lib[]]/**"]
            imports = { client = ["@lib/client/*.lua"], server = ["@oxmysql/lib/MySQL.lua"] }
            "#,
            PathBuf::from("/repo"),
        )
        .unwrap();
        let owned =
            |imports: &[(&str, Side)]| imports.iter().map(|(p, side)| (p.to_string(), *side)).collect::<Vec<_>>();
        let everywhere = [("@lib/shared/**.lua", Side::Shared)];
        assert_eq!(config.imports_for(Path::new("/repo/resources/chat/fxmanifest.lua")), owned(&everywhere));
        assert_eq!(
            config.imports_for(Path::new("/repo/resources/[lib]/shop/fxmanifest.lua")),
            owned(&[everywhere[0], ("@lib/client/*.lua", Side::Client), ("@oxmysql/lib/MySQL.lua", Side::Server)])
        );
        for pattern in ["lib/shared/a.lua", "@lib", "@lib/web/app.js"] {
            let error = Config::parse(&format!("imports = {{ shared = ['{pattern}'] }}"), PathBuf::new()).unwrap_err();
            assert!(error.contains(pattern), "{error}");
        }
    }

    #[test]
    fn overrides_give_scripts_a_side() {
        let config = Config::parse(
            r#"
            [[overrides]]
            files = ["lib/**"]
            side = "shared"
            [[overrides]]
            files = ["lib/client/**"]
            side = "client"
            [[overrides]]
            files = ["lib/client/**"]
            globals = ["Later"]
            "#,
            PathBuf::from("/repo"),
        )
        .unwrap();
        let side = |path: &str| config.side_for(Path::new(&format!("/repo/{path}")));
        assert_eq!(side("lib/client/main.lua"), Some(Side::Client), "a later override without a side keeps it");
        assert_eq!(side("lib/shared/util.lua"), Some(Side::Shared));
        assert_eq!(side("other/main.lua"), None);
        assert!(Config::parse("[[overrides]]\nfiles = ['a']\nside = 'both'", PathBuf::new()).is_err());
    }

    #[test]
    fn rejects_unknown_rules() {
        assert!(Config::parse("[rules]\n\"nope\" = \"off\"", PathBuf::new()).unwrap_err().contains("unknown rule"));
    }

    fn temp_tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("qbx-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        root
    }

    #[test]
    fn falls_back_to_lua_ls_settings_without_qbxlint_toml() {
        let root = temp_tree(
            "luarc",
            &[
                (
                    "res/.luarc.json",
                    r#"{ "diagnostics.globals": ["lib", "Config"], "diagnostics.disable": ["lowercase-global", "no-such-code"], "workspace.ignoreDir": ["\\[standalone\\]"] }"#,
                ),
                ("res/.emmyrc.json", r#"{ "workspace": { "ignoreGlobs": ["build/**"] } }"#),
                ("res/client/main.lua", ""),
            ],
        );
        let config = Config::discover(&root.join("res/client/main.lua"));
        let notes = config.notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains(".luarc.json") && notes[0].contains(".emmyrc.json"));
        assert!(config.format_for(&root.join("res/client/main.lua")).is_none());
        let file = config.for_file(&root.join("res/client/main.lua"));
        assert_eq!(file.globals, ["Config"]);
        assert_eq!(file.severity("lowercase-global"), None);
        assert_eq!(file.severity("undefined-global"), Some(Severity::Warning));
        assert!(config.is_excluded(&root.join("res/[standalone]/tool/main.lua")));
        assert!(config.is_excluded(&root.join("res/build/out.lua")));
        assert!(!config.is_excluded(&root.join("res/client/main.lua")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn qbxlint_toml_anywhere_above_wins_over_a_nearer_lua_ls_config() {
        let root = temp_tree(
            "precedence",
            &[
                ("qbxlint.toml", "globals = ['FromToml']\n"),
                ("res/.luarc.json", r#"{ "diagnostics.globals": ["FromLuarc"] }"#),
                ("res/main.lua", ""),
            ],
        );
        let config = Config::discover(&root.join("res/main.lua"));
        assert_eq!(config.for_file(&root.join("res/main.lua")).globals, ["FromToml"]);
        assert!(config.format_for(&root.join("res/main.lua")).is_some());
        assert!(config.notes().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unreadable_lua_ls_settings_are_skipped_with_a_note_during_discovery_only() {
        let root = temp_tree(
            "unreadable",
            &[
                ("res/.luarc.json", r#"{ "diagnostics.globals": ["#),
                (
                    "res/.emmyrc.json",
                    r#"{ "diagnostics": { "globals": ["Fine"] }, "workspace": { "ignoreGlobs": ["[z-a]"] } }"#,
                ),
                ("other/.luarc.json", "not json"),
            ],
        );
        let config = Config::discover(&root.join("res"));
        let globals = config.for_file(&root.join("res/main.lua")).globals;
        assert_eq!(globals, ["Fine"], "the readable file in the same directory still applies");
        let notes = config.notes();
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes[1].starts_with("skipped") && notes[1].contains(".luarc.json"));
        assert!(notes[2].contains("[z-a]"));
        assert!(config.errors().is_empty());

        let other = Config::discover(&root.join("other"));
        assert!(other.for_file(&root.join("other/main.lua")).globals.is_empty());
        let notes = other.notes();
        assert_eq!(notes.len(), 1, "a broken file is reported, not replaced by a config further up");
        assert!(Config::load(&root.join("other/.luarc.json")).unwrap_err().contains(".luarc.json"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nested_files_combine_with_the_files_above_them() {
        let root = temp_tree(
            "nested",
            &[
                (
                    "server/qbxlint.toml",
                    "globals = ['Shared']\nexclude = ['**/skip/**']\n[rules]\n'unused-argument' = 'off'\n'unused-local' = 'error'\n",
                ),
                ("server/resource_a/qbxlint.toml", "globals = ['OnlyA']\n[rules]\n'unused-local' = 'hint'\n"),
                ("server/resource_b/qbxlint.toml", "globals = ['OnlyB']\nexclude = ['generated/**']\n"),
            ],
        );
        let server = root.join("server");
        for start in [server.clone(), server.join("resource_a"), server.join("resource_a/client.lua")] {
            let config = Config::discover(&start);
            let a = config.for_file(&server.join("resource_a/client.lua"));
            assert_eq!(a.globals, ["Shared", "OnlyA"], "from {}", start.display());
            assert_eq!(a.severity("unused-argument"), None);
            assert_eq!(a.severity("unused-local"), Some(Severity::Hint));
            let b = config.for_file(&server.join("resource_b/client.lua"));
            assert_eq!(b.globals, ["Shared", "OnlyB"]);
            assert_eq!(b.severity("unused-local"), Some(Severity::Error));
            assert_eq!(config.for_file(&server.join("resource_c/client.lua")).globals, ["Shared"]);
            assert!(config.is_excluded(&server.join("resource_b/generated/a.lua")));
            assert!(!config.is_excluded(&server.join("resource_a/generated/a.lua")));
            assert!(config.is_excluded(&server.join("resource_a/skip/a.lua")));
            assert!(config.is_excluded(&server.join("resource_a/node_modules/a.lua")));
            assert!(config.errors().is_empty());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_nearer_file_wins_over_the_overrides_of_the_files_above_it() {
        let root = temp_tree(
            "nearer",
            &[
                (
                    "qbxlint.toml",
                    "strict = true\n[[overrides]]\nfiles = ['res/**']\nrules = { 'unused-local' = 'off' }\nstrict = false\nside = 'client'\n",
                ),
                (
                    "res/qbxlint.toml",
                    "strict = true\n[rules]\n'unused-local' = 'error'\n[[overrides]]\nfiles = ['lib/**']\nside = 'server'\nrules = { 'unused-local' = 'info' }\n",
                ),
            ],
        );
        let mut config = Config::discover(&root);
        let level = |config: &Config, path: &str| config.for_file(&root.join(path)).severity("unused-local");
        assert_eq!(level(&config, "res/main.lua"), Some(Severity::Error));
        assert_eq!(level(&config, "res/lib/a.lua"), Some(Severity::Info), "patterns are relative to their file");
        assert_eq!(config.strict_at(&root.join("res/main.lua")), Some(true));
        assert_eq!(config.side_for(&root.join("res/main.lua")), Some(Side::Client));
        assert_eq!(config.side_for(&root.join("res/lib/a.lua")), Some(Side::Server));
        assert_eq!(config.side_for(&root.join("main.lua")), None);

        config.set_rule("unused-local", Level::Warning);
        assert_eq!(level(&config, "res/main.lua"), Some(Severity::Warning), "--rule replaces every [rules] level");
        assert_eq!(level(&config, "res/lib/a.lua"), Some(Severity::Info), "overrides still apply after it");
        assert_eq!(level(&config, "main.lua"), Some(Severity::Warning));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn root_true_stops_the_search_for_files_above() {
        let root = temp_tree(
            "root",
            &[
                ("qbxlint.toml", "globals = ['Outer']\n"),
                ("inner/qbxlint.toml", "root = true\nglobals = ['Inner']\nstrict_classes = true\n"),
                ("inner/deeper/qbxlint.toml", "globals = ['Deeper']\n"),
            ],
        );
        let config = Config::discover(&root);
        let globals = |path: &str| config.for_file(&root.join(path)).globals;
        assert_eq!(globals("main.lua"), ["Outer"]);
        assert_eq!(globals("inner/main.lua"), ["Inner"]);
        assert_eq!(globals("inner/deeper/main.lua"), ["Inner", "Deeper"]);
        assert!(!config.strict_classes_at(&root.join("main.lua")));
        assert!(config.strict_classes_at(&root.join("inner/deeper/main.lua")));
        assert!(config.strict_classes_anywhere());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_nearest_file_whose_ignore_diagnostics_patterns_match_decides() {
        let root = temp_tree(
            "ignore",
            &[
                ("qbxlint.toml", "ignore_diagnostics = ['vendor/']\n"),
                ("res/qbxlint.toml", "ignore_diagnostics = ['!vendor/ours.lua', 'generated.lua']\n"),
            ],
        );
        let config = Config::discover(&root);
        let ignored = |path: &str| config.ignores_diagnostics(&root.join(path));
        assert!(ignored("vendor/lib.lua"));
        assert!(ignored("res/vendor/lib.lua"));
        assert!(!ignored("res/vendor/ours.lua"));
        assert!(ignored("res/generated.lua"));
        assert!(!ignored("generated.lua"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn format_options_combine_key_by_key() {
        let root = temp_tree(
            "format",
            &[
                ("qbxlint.toml", "[format]\nindent_width = 2\nquote_style = 'single'\n"),
                ("res/qbxlint.toml", "[format]\nquote_style = 'double'\n"),
            ],
        );
        let config = Config::discover(&root);
        let res = config.format_for(&root.join("res/main.lua")).unwrap();
        assert_eq!((res.indent_width, res.quote_style), (2, qbx_lua_fmt::QuoteStyle::Double));
        let top = config.format_for(&root.join("main.lua")).unwrap();
        assert_eq!((top.indent_width, top.quote_style), (2, qbx_lua_fmt::QuoteStyle::Single));
        let error = Config::parse("[format]\nindent = 2\n", PathBuf::new()).unwrap_err();
        assert!(error.contains("indent") && error.contains("line 1"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_nested_file_that_cannot_be_read_is_reported_and_skipped() {
        let root = temp_tree(
            "broken",
            &[("qbxlint.toml", "globals = ['Outer']\n"), ("res/qbxlint.toml", "[rules]\n'nope' = 'off'\n")],
        );
        let config = Config::discover(&root);
        assert!(config.errors().is_empty(), "res is not looked up before a path in it is");
        assert_eq!(config.for_file(&root.join("res/main.lua")).globals, ["Outer"]);
        let errors = config.errors();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("res") && errors[0].contains("unknown rule 'nope'"), "{errors:?}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lua_ls_settings_apply_only_where_no_qbxlint_toml_does() {
        let root = temp_tree(
            "fallback",
            &[
                (".luarc.json", r#"{ "diagnostics.globals": ["FromLuarc"] }"#),
                ("res_a/qbxlint.toml", "globals = ['FromToml']\n"),
            ],
        );
        let config = Config::discover(&root);
        assert_eq!(config.for_file(&root.join("res_a/main.lua")).globals, ["FromToml"]);
        assert_eq!(config.for_file(&root.join("res_b/main.lua")).globals, ["FromLuarc"]);
        assert!(config.format_for(&root.join("res_b/main.lua")).is_none());
        assert_eq!(config.notes().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
