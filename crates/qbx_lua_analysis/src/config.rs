use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use qbx_fivem_data::Side;
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawConfig {
    exclude: Vec<String>,
    ignore_diagnostics: Vec<String>,
    globals: Vec<String>,
    ignore_unused_prefix: Option<String>,
    strict_classes: bool,
    strict: Option<bool>,
    rules: BTreeMap<String, Level>,
    overrides: Vec<RawOverride>,
    imports: Imports,
    format: qbx_lua_fmt::FormatOptions,
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

#[derive(Clone, Debug)]
struct Override {
    files: GlobSet,
    globals: Vec<String>,
    strict: Option<bool>,
    rules: BTreeMap<String, Level>,
    imports: Imports,
    side: Option<Side>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub root: PathBuf,
    exclude: GlobSet,
    ignore_diagnostics: Gitignore,
    pub globals: Vec<String>,
    pub ignore_unused_prefix: String,
    /// Whether a `---@class` without `(strict)` or `(loose)` is strict. Only classes declared in
    /// files whose diagnostics are reported follow it; qbx-lua-ls applies it.
    pub strict_classes: bool,
    /// Whether the type checks of qbx-lua-ls report what TypeScript's strict mode does beyond
    /// lua-language-server. `None` leaves it to the editor's setting.
    strict: Option<bool>,
    pub format: qbx_lua_fmt::FormatOptions,
    /// Whether `format` comes from a `qbxlint.toml`. Editors keep their own indentation otherwise.
    pub format_configured: bool,
    /// Which LuaLS or EmmyLua files discovery fell back to, and what it skipped in them.
    pub notes: Vec<String>,
    rules: BTreeMap<String, Level>,
    overrides: Vec<Override>,
    imports: Imports,
}

const DEFAULT_EXCLUDES: &[&str] = &["**/node_modules/**", "**/.git/**", "**/[[]builders[]]/**"];

impl Default for Config {
    fn default() -> Self {
        Self::from_raw(RawConfig::default(), PathBuf::new()).expect("default config is valid")
    }
}

impl Config {
    pub fn parse(text: &str, root: PathBuf) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut config = Self::from_raw(raw, root)?;
        config.format_configured = true;
        Ok(config)
    }

    /// Loads a `qbxlint.toml`, or a LuaLS or EmmyLua JSON configuration by its extension.
    pub fn load(path: &Path) -> Result<Self, String> {
        let path = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if path.extension().is_some_and(|e| e == "json" || e == "jsonc") {
            return Self::load_lua_ls(std::slice::from_ref(&path), false)
                .map_err(|e| format!("{}: {e}", path.display()));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Self::parse(&text, root).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Walks up from `start` looking for a `qbxlint.toml`. Only when there is none does the nearest
    /// directory with a LuaLS or EmmyLua configuration supply globals, rule levels and exclusions.
    pub fn discover(start: &Path) -> Result<Option<Self>, String> {
        let start = if start.is_dir() { Some(start) } else { start.parent() };
        let ancestors = || std::iter::successors(start, |dir| dir.parent());
        for dir in ancestors() {
            for name in CONFIG_FILE_NAMES {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Self::load(&candidate).map(Some);
                }
            }
        }
        for dir in ancestors() {
            let found: Vec<PathBuf> =
                lua_ls_config::FILE_NAMES.iter().map(|name| dir.join(name)).filter(|p| p.is_file()).collect();
            if !found.is_empty() {
                return Self::load_lua_ls(&found, true).map(Some);
            }
        }
        Ok(None)
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
        let mut config = Self::from_raw(raw, root)?;
        config.notes = notes;
        Ok(config)
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
        let exclude = build_globset(DEFAULT_EXCLUDES.iter().copied().chain(raw.exclude.iter().map(String::as_str)))?;
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
            exclude,
            ignore_diagnostics,
            globals: raw.globals,
            ignore_unused_prefix: raw.ignore_unused_prefix.unwrap_or_else(|| "_".to_string()),
            strict_classes: raw.strict_classes,
            strict: raw.strict,
            format: raw.format,
            format_configured: false,
            notes: Vec::new(),
            rules: raw.rules,
            overrides,
            imports: raw.imports,
        })
    }

    pub fn set_rule(&mut self, code: &str, level: Level) {
        self.rules.insert(code.to_string(), level);
    }

    fn relative<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.root).unwrap_or(path)
    }

    pub fn is_excluded(&self, path: &Path) -> bool {
        self.relative(path).ancestors().any(|p| !p.as_os_str().is_empty() && self.exclude.is_match(p))
    }

    /// A directory walk has already checked the entry's parent directories, so only the entry is matched.
    pub(crate) fn excludes_entry(&self, path: &Path) -> bool {
        self.exclude.is_match(self.relative(path))
    }

    /// Paths that are still analyzed, so their globals and exports count, but whose own findings are not reported.
    pub fn ignores_diagnostics(&self, path: &Path) -> bool {
        let relative = self.relative(path);
        // The matcher panics on paths outside its root.
        !relative.has_root() && self.ignore_diagnostics.matched_path_or_any_parents(relative, false).is_ignore()
    }

    pub fn for_file(&self, path: &Path) -> FileConfig {
        let relative = self.relative(path);
        let mut rules = self.rules.clone();
        let mut globals = self.globals.clone();
        let mut strict = self.strict;
        for entry in self.overrides.iter().filter(|o| o.files.is_match(relative)) {
            rules.extend(entry.rules.iter().map(|(k, v)| (k.clone(), *v)));
            globals.extend(entry.globals.iter().cloned());
            strict = entry.strict.or(strict);
        }
        FileConfig { rules, globals, ignore_unused_prefix: self.ignore_unused_prefix.clone(), strict }
    }

    /// The configured `imports` of the resource whose manifest is `manifest_path`. The scripts of a
    /// resource share their globals, so an override adds its imports to every resource whose
    /// manifest its `files` patterns match.
    pub fn imports_for(&self, manifest_path: &Path) -> Vec<(&str, Side)> {
        let relative = self.relative(manifest_path);
        let overrides = self.overrides.iter().filter(|o| o.files.is_match(relative)).map(|o| &o.imports);
        std::iter::once(&self.imports).chain(overrides).flat_map(Imports::entries).collect()
    }

    /// The side the last matching override with a `side` gives the script at `path`. It only counts
    /// for scripts the manifest does not list, such as files a loader runs at runtime.
    pub fn side_for(&self, path: &Path) -> Option<Side> {
        let relative = self.relative(path);
        self.overrides.iter().rev().filter(|o| o.files.is_match(relative)).find_map(|o| o.side)
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
        assert!(!config.strict_classes);
        assert!(Config::parse("strict_classes = true", PathBuf::from("/repo")).unwrap().strict_classes);
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
        let everywhere = [("@lib/shared/**.lua", Side::Shared)];
        assert_eq!(config.imports_for(Path::new("/repo/resources/chat/fxmanifest.lua")), everywhere);
        assert_eq!(
            config.imports_for(Path::new("/repo/resources/[lib]/shop/fxmanifest.lua")),
            [everywhere[0], ("@lib/client/*.lua", Side::Client), ("@oxmysql/lib/MySQL.lua", Side::Server)]
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
        let config = Config::discover(&root.join("res/client/main.lua")).unwrap().unwrap();
        assert_eq!(config.root, root.join("res"));
        assert!(!config.format_configured);
        assert_eq!(config.globals, ["Config"]);
        assert_eq!(config.notes.len(), 1);
        assert!(config.notes[0].contains(".luarc.json") && config.notes[0].contains(".emmyrc.json"));
        let file = config.for_file(&root.join("res/client/main.lua"));
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
        let config = Config::discover(&root.join("res/main.lua")).unwrap().unwrap();
        assert_eq!(config.globals, ["FromToml"]);
        assert!(config.format_configured);
        assert!(config.notes.is_empty());
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
        let config = Config::discover(&root.join("res")).unwrap().unwrap();
        assert_eq!(config.globals, ["Fine"], "the readable file in the same directory still applies");
        assert_eq!(config.notes.len(), 3, "{:?}", config.notes);
        assert!(config.notes[1].starts_with("skipped") && config.notes[1].contains(".luarc.json"));
        assert!(config.notes[2].contains("[z-a]"));

        let other = Config::discover(&root.join("other")).unwrap().unwrap();
        assert!(other.globals.is_empty());
        assert_eq!(other.notes.len(), 1, "a broken file is reported, not replaced by a config further up");
        assert!(Config::load(&root.join("other/.luarc.json")).unwrap_err().contains(".luarc.json"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
