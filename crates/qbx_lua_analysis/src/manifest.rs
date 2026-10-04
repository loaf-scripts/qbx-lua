use qbx_fivem_data::Side;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::{SmolStr, Span};

pub const MANIFEST_FILE_NAMES: &[&str] = &["fxmanifest.lua", "__resource.lua"];

pub const KNOWN_DIRECTIVES: &[&str] = &[
    "fx_version",
    "game",
    "games",
    "lua54",
    "author",
    "description",
    "version",
    "name",
    "repository",
    "license",
    "client_script",
    "client_scripts",
    "server_script",
    "server_scripts",
    "shared_script",
    "shared_scripts",
    "file",
    "files",
    "dependency",
    "dependencies",
    "ui_page",
    "loadscreen",
    "loadscreen_manual_shutdown",
    "loadscreen_cursor",
    "data_file",
    "this_is_a_map",
    "server_only",
    "provide",
    "provides",
    "export",
    "exports",
    "server_export",
    "server_exports",
    "use_experimental_fxv2_oal",
    "node_version",
    "rdr3_warning",
    "clr_disable_task_scheduler",
    "before_level_meta",
    "after_level_meta",
    "replace_level_meta",
    "convar_category",
    "resource_manifest_version",
    "resource_type",
    "map",
    "my_data",
    "ox_lib",
    "ox_libs",
    "escrow_ignore",
];

#[derive(Clone, Debug)]
pub struct Entry {
    pub value: SmolStr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ScriptEntry {
    pub pattern: SmolStr,
    pub span: Span,
    pub side: Side,
}

impl ScriptEntry {
    pub fn is_import(&self) -> bool {
        self.pattern.starts_with('@')
    }

    pub fn is_lua(&self) -> bool {
        self.pattern.ends_with(".lua") || self.pattern.ends_with('*')
    }
}

#[derive(Clone, Debug)]
pub struct Directive {
    pub name: SmolStr,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub struct Manifest {
    pub fx_version: Option<Entry>,
    pub games: Vec<Entry>,
    pub lua54: Option<Entry>,
    pub scripts: Vec<ScriptEntry>,
    pub files: Vec<Entry>,
    /// The `data_file` entries among `files`: game data that is found by asset name, not by path.
    pub data_files: Vec<Entry>,
    pub dependencies: Vec<Entry>,
    pub ui_page: Option<Entry>,
    /// Files named by `map '...'`; mapmanager runs them in its own environment (`spawnpoint` etc.).
    pub maps: Vec<Entry>,
    pub directives: Vec<Directive>,
}

impl Manifest {
    pub fn from_chunk(chunk: &Chunk) -> Self {
        let mut manifest = Manifest::default();
        for stmt in &chunk.block.stmts {
            if let StmtKind::Expr(expr) = &stmt.kind {
                manifest.directive(expr);
            }
        }
        manifest
    }

    /// JavaScript and C# scripts register exports and event handlers this tooling cannot read.
    pub fn has_non_lua_scripts(&self) -> bool {
        self.scripts.iter().filter(|s| !s.is_import()).any(|s| {
            let pattern = s.pattern.to_ascii_lowercase();
            [".js", ".ts", ".mjs", ".cjs", ".dll"].iter().any(|ext| pattern.ends_with(ext))
        })
    }

    pub fn is_map_file(&self, relative_path: &str) -> bool {
        self.maps.iter().any(|map| crate::glob::manifest_glob_match(&map.value, relative_path))
    }

    pub fn lua54_enabled(&self) -> bool {
        self.lua54.as_ref().is_some_and(|e| matches!(e.value.as_str(), "yes" | "true"))
    }

    pub fn imports(&self) -> impl Iterator<Item = &ScriptEntry> {
        self.scripts.iter().filter(|s| s.is_import())
    }

    pub fn imports_path(&self, path: &str, side: Side) -> bool {
        self.imports().any(|s| s.pattern.eq_ignore_ascii_case(path) && s.side.is_available_on(side))
    }

    fn directive(&mut self, expr: &Expr) {
        let mut arg_groups: Vec<&[Expr]> = Vec::new();
        let mut current = expr;
        let name = loop {
            match &current.kind {
                ExprKind::Call { callee, args, .. } => {
                    arg_groups.push(args);
                    current = callee;
                }
                ExprKind::Name(name) => break name,
                _ => return,
            }
        };
        arg_groups.reverse();
        self.directives.push(Directive { name: name.text.clone(), span: name.span });

        let first = arg_groups.first().map(|args| collect_strings(args)).unwrap_or_default();
        match name.text.as_str() {
            "fx_version" => self.fx_version = first.into_iter().next(),
            "game" | "games" => self.games.extend(first),
            "lua54" => self.lua54 = first.into_iter().next(),
            "client_script" | "client_scripts" => self.add_scripts(first, Side::Client),
            "server_script" | "server_scripts" => self.add_scripts(first, Side::Server),
            "shared_script" | "shared_scripts" => self.add_scripts(first, Side::Shared),
            "file" | "files" => self.files.extend(first),
            "dependency" | "dependencies" => self.dependencies.extend(first),
            "ui_page" => self.ui_page = first.into_iter().next(),
            "map" => self.maps.extend(first),
            "loadscreen" => self.files.extend(first),
            "data_file" => {
                if let Some(paths) = arg_groups.get(1) {
                    let paths = collect_strings(paths);
                    self.data_files.extend(paths.iter().cloned());
                    self.files.extend(paths);
                }
            }
            _ => {}
        }
    }

    fn add_scripts(&mut self, entries: Vec<Entry>, side: Side) {
        self.scripts.extend(entries.into_iter().map(|e| ScriptEntry { pattern: e.value, span: e.span, side }));
    }
}

fn collect_strings(args: &[Expr]) -> Vec<Entry> {
    let mut out = Vec::new();
    for arg in args {
        match &arg.kind {
            ExprKind::String(value) => out.push(Entry { value: value.clone(), span: arg.span }),
            ExprKind::Table(fields) => {
                for field in fields {
                    if let TableField::Positional(Expr { kind: ExprKind::String(value), span }) = field {
                        out.push(Entry { value: value.clone(), span: *span });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub fn closest_directive(name: &str) -> Option<&'static str> {
    if KNOWN_DIRECTIVES.contains(&name) {
        return None;
    }
    KNOWN_DIRECTIVES
        .iter()
        .map(|known| (*known, edit_distance(name, known)))
        .filter(|(known, distance)| *distance <= 2 && known.len() > 4)
        .min_by_key(|(_, distance)| *distance)
        .map(|(known, _)| known)
}

pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            row.push((prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1));
        }
        prev = row;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use qbx_lua_syntax::parse;

    #[test]
    fn reads_directives() {
        let chunk = parse(
            r#"
            fx_version 'cerulean'
            game 'gta5'
            lua54 'yes'
            shared_scripts { '@ox_lib/init.lua', 'shared/*.lua' }
            client_script 'client/main.lua'
            server_scripts({ '@oxmysql/lib/MySQL.lua', 'server/**/*.lua' })
            files { 'locales/*.json', 'config/client.lua' }
            data_file 'DLC_ITYP_REQUEST' 'stream/props.ytyp'
            dependencies { 'ox_lib', 'qbx_core' }
            "#,
        );
        let manifest = Manifest::from_chunk(&chunk);
        assert_eq!(manifest.fx_version.as_ref().unwrap().value, "cerulean");
        assert!(manifest.lua54_enabled());
        assert_eq!(manifest.scripts.len(), 5);
        assert!(manifest.imports_path("@ox_lib/init.lua", Side::Client));
        assert!(manifest.imports_path("@oxmysql/lib/MySQL.lua", Side::Server));
        assert!(!manifest.imports_path("@oxmysql/lib/MySQL.lua", Side::Client));
        assert_eq!(manifest.files.len(), 3);
        assert_eq!(manifest.data_files.len(), 1);
        assert_eq!(manifest.dependencies.len(), 2);
    }

    #[test]
    fn suggests_typo_fixes() {
        assert_eq!(closest_directive("client_scipts"), Some("client_scripts"));
        assert_eq!(closest_directive("shared_script"), None);
        assert_eq!(closest_directive("my_custom_metadata"), None);
    }
}
