use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use qbx_lua_syntax::SmolStr;
use rustc_hash::{FxHashMap, FxHashSet};
use walkdir::WalkDir;

use crate::project::{manifest_in, walks_into};

/// Resources the server artifact ships in `citizen/system_resources`. They are installed on
/// every server without appearing under `resources`, so a recipe may even delete its own copy.
pub const SYSTEM_RESOURCES: &[&str] = &["chat", "monitor", "webpack", "yarn"];

/// The order in which a server's cfg files start resources. Resources started by one
/// `ensure [category]` line share a group, because their relative order is not defined.
#[derive(Debug, Default)]
pub struct StartOrder {
    groups: FxHashMap<SmolStr, usize>,
    /// Every resource under the server's `resources` folder, plus the names they `provide` and
    /// the artifact's system resources.
    pub installed: FxHashSet<SmolStr>,
    /// Names a resource answers to through `provide`, so starting it also starts those.
    provided: FxHashMap<SmolStr, Vec<SmolStr>>,
}

fn provided_names(manifest_text: &str) -> impl Iterator<Item = SmolStr> + '_ {
    manifest_text
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("provide"))
        .filter_map(|line| line.split(['\'', '"']).nth(1))
        .filter(|name| !name.is_empty())
        .map(SmolStr::new)
}

fn installed_resources(resources_dir: &Path) -> (FxHashSet<SmolStr>, FxHashMap<SmolStr, Vec<SmolStr>>) {
    let mut names: FxHashSet<SmolStr> = SYSTEM_RESOURCES.iter().map(SmolStr::new).collect();
    let mut provided: FxHashMap<SmolStr, Vec<SmolStr>> = FxHashMap::default();
    // FiveM starts resources from symlinked folders too.
    let root = resources_dir.canonicalize().ok();
    let mut walker = WalkDir::new(resources_dir).max_depth(7).follow_links(true).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        let hidden = entry.depth() > 0 && (name == "node_modules" || name.starts_with('.'));
        !hidden && walks_into(root.as_deref(), entry)
    });
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_dir() {
            continue;
        }
        let Some(manifest) = manifest_in(entry.path()) else { continue };
        walker.skip_current_dir();
        let name = SmolStr::new(entry.file_name().to_string_lossy());
        names.insert(name.clone());
        // `provide 'qb-core'` lets a resource answer to another name, exports included.
        let Ok(text) = std::fs::read_to_string(&manifest) else { continue };
        for alias in provided_names(&text) {
            names.insert(alias.clone());
            provided.entry(name.clone()).or_default().push(alias);
        }
    }
    (names, provided)
}

const MAX_EXEC_DEPTH: u32 = 5;

fn cache() -> &'static Mutex<FxHashMap<PathBuf, Arc<StartOrder>>> {
    static CACHE: OnceLock<Mutex<FxHashMap<PathBuf, Arc<StartOrder>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Forgets parsed cfg files; call it when one of them changes.
pub fn clear_cache() {
    cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// A server.cfg only governs what lives in the `resources` folder next to it; a project that
/// merely sits somewhere below a server's data folder is not one of its resources.
fn find_server_cfg(resource_root: &Path) -> Option<PathBuf> {
    resource_root
        .ancestors()
        .skip(1)
        .filter(|dir| resource_root.starts_with(dir.join("resources")))
        .map(|dir| dir.join("server.cfg"))
        .find(|cfg| cfg.is_file())
}

fn resources_in_category(resources_dir: &Path, category: &str) -> Vec<SmolStr> {
    let mut names = Vec::new();
    let root = resources_dir.canonicalize().ok();
    let folders = WalkDir::new(resources_dir).max_depth(4).follow_links(true).into_iter();
    let folders = folders.filter_entry(|entry| walks_into(root.as_deref(), entry)).flatten();
    for folder in folders.filter(|e| e.file_type().is_dir() && e.file_name().to_string_lossy() == category) {
        let walked = folder.path().canonicalize().ok();
        let entries = WalkDir::new(folder.path()).max_depth(5).follow_links(true).into_iter();
        let mut entries = entries.filter_entry(|entry| walks_into(walked.as_deref(), entry));
        while let Some(entry) = entries.next() {
            let Ok(entry) = entry else { continue };
            if entry.file_type().is_dir() && manifest_in(entry.path()).is_some() {
                entries.skip_current_dir();
                names.push(SmolStr::new(entry.file_name().to_string_lossy()));
            }
        }
    }
    names
}

impl StartOrder {
    /// The configuration governing a resource, without reading its contents.
    pub fn configuration_path(resource_root: &Path) -> Option<PathBuf> {
        find_server_cfg(resource_root)
    }
    /// Uncached, bounded discovery for explicit assistant snapshots. The caller controls every
    /// source read (including containment and byte limits); ordinary editor discovery is unchanged.
    pub fn discover_bounded(
        resource_root: &Path,
        read: &mut impl FnMut(&Path) -> Option<String>,
        remaining_entries: &mut usize,
    ) -> (Option<Self>, bool) {
        let Some(cfg) = find_server_cfg(resource_root) else { return (None, false) };
        let Some(first) = read(&cfg) else { return (None, true) };
        let resources_dir = cfg.parent().unwrap_or(Path::new(".")).join("resources");
        let mut order = Self::default();
        order.installed.extend(SYSTEM_RESOURCES.iter().map(SmolStr::new));
        let mut resources = Vec::new();
        let mut partial = false;
        let root = resources_dir.canonicalize().ok();
        let mut walker =
            WalkDir::new(&resources_dir).max_depth(7).follow_links(true).into_iter().filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                let hidden = entry.depth() > 0 && (name == "node_modules" || name.starts_with('.'));
                !hidden && walks_into(root.as_deref(), entry)
            });
        while *remaining_entries > 0 {
            let Some(entry) = walker.next() else { break };
            *remaining_entries -= 1;
            let Ok(entry) = entry else {
                partial = true;
                continue;
            };
            if !entry.file_type().is_dir() {
                continue;
            }
            let Some(manifest) = manifest_in(entry.path()) else { continue };
            walker.skip_current_dir();
            let name = SmolStr::new(entry.file_name().to_string_lossy());
            order.installed.insert(name.clone());
            resources.push((entry.path().to_path_buf(), name.clone()));
            if let Some(text) = read(&manifest) {
                for alias in provided_names(&text) {
                    order.installed.insert(alias.clone());
                    order.provided.entry(name.clone()).or_default().push(alias);
                }
            } else {
                partial = true;
            }
        }
        if walker.next().is_some() {
            partial = true;
        }
        struct Context<'a, F> {
            read: &'a mut F,
            resources: &'a [(PathBuf, SmolStr)],
            visited: FxHashSet<PathBuf>,
            next: usize,
            partial: bool,
        }
        fn read_cfg_bounded<F: FnMut(&Path) -> Option<String>>(
            order: &mut StartOrder,
            path: &Path,
            text: String,
            depth: u32,
            context: &mut Context<'_, F>,
        ) {
            if !context.visited.insert(path.to_path_buf()) {
                return;
            }
            let dir = path.parent().unwrap_or(Path::new("."));
            for line in text.lines() {
                let mut words = line.split('#').next().unwrap_or("").split_whitespace();
                let (Some(command), Some(target)) = (words.next(), words.next()) else { continue };
                let target = target.trim_matches(['"', '\'']);
                if command == "exec" {
                    if depth >= MAX_EXEC_DEPTH {
                        context.partial = true;
                        continue;
                    }
                    let child = dir.join(target);
                    if context.visited.contains(&child) {
                        continue;
                    }
                    if let Some(text) = (context.read)(&child) {
                        read_cfg_bounded(order, &child, text, depth + 1, context);
                    } else {
                        context.partial = true;
                    }
                } else if command == "ensure" || command == "start" {
                    if target.starts_with('[') && target.ends_with(']') {
                        let category_root = dir.join("resources");
                        for (path, name) in context.resources {
                            if path.starts_with(&category_root)
                                && path.ancestors().any(|part| part.file_name().is_some_and(|name| name == target))
                            {
                                order.assign(name.clone(), context.next);
                            }
                        }
                    } else {
                        order.assign(SmolStr::new(target), context.next);
                    }
                    context.next += 1;
                }
            }
        }
        let mut context = Context { read, resources: &resources, visited: FxHashSet::default(), next: 0, partial };
        read_cfg_bounded(&mut order, &cfg, first, 0, &mut context);
        (Some(order), context.partial)
    }
    /// The start order that applies to the resource at `resource_root`, if a `server.cfg` sits
    /// above it.
    pub fn discover(resource_root: &Path) -> Option<Arc<StartOrder>> {
        let cfg = find_server_cfg(resource_root)?;
        let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(order) = cache.get(&cfg) {
            return Some(order.clone());
        }
        let resources_dir = cfg.parent().unwrap_or(Path::new(".")).join("resources");
        let (installed, provided) = installed_resources(&resources_dir);
        let mut order = StartOrder { installed, provided, ..StartOrder::default() };
        let mut next_group = 0;
        order.read_cfg(&cfg, &mut next_group, 0);
        let order = Arc::new(order);
        cache.insert(cfg, order.clone());
        Some(order)
    }

    fn read_cfg(&mut self, cfg: &Path, next_group: &mut usize, depth: u32) {
        let Ok(text) = std::fs::read_to_string(cfg) else { return };
        let dir = cfg.parent().unwrap_or(Path::new("."));
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let mut words = line.split_whitespace();
            let (Some(command), Some(target)) = (words.next(), words.next()) else { continue };
            let target = target.trim_matches(['"', '\'']);
            match command {
                "exec" if depth < MAX_EXEC_DEPTH => self.read_cfg(&dir.join(target), next_group, depth + 1),
                "ensure" | "start" => {
                    let names = if target.starts_with('[') && target.ends_with(']') {
                        resources_in_category(&dir.join("resources"), target)
                    } else {
                        vec![SmolStr::new(target)]
                    };
                    for name in names {
                        self.assign(name, *next_group);
                    }
                    *next_group += 1;
                }
                _ => {}
            }
        }
    }

    /// Starting a resource also starts every name it provides, at the same position.
    fn assign(&mut self, name: SmolStr, group: usize) {
        for alias in self.provided.get(&name).into_iter().flatten() {
            self.groups.entry(alias.clone()).or_insert(group);
        }
        self.groups.entry(name).or_insert(group);
    }

    /// Resources that are certain to be running before `resource` starts.
    pub fn started_before(&self, resource: &str) -> FxHashSet<SmolStr> {
        let Some(own) = self.groups.get(resource) else { return FxHashSet::default() };
        self.groups.iter().filter(|(_, group)| *group < own).map(|(name, _)| name.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ensure_order_categories_and_exec() {
        let root = std::env::temp_dir().join(format!("qbx-start-order-{}", std::process::id()));
        let resource = |path: &str| {
            let dir = root.join("resources").join(path);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("fxmanifest.lua"), "fx_version 'cerulean'").unwrap();
        };
        for path in ["[ox]/ox_lib", "[ox]/ox_inventory", "[qbx]/qbx_core", "[standalone]/mything", "unlisted"] {
            resource(path);
        }
        std::fs::write(root.join("server.cfg"), "# comment\nensure [ox]\nexec extra.cfg\nensure mything # last\n")
            .unwrap();
        std::fs::write(root.join("extra.cfg"), "start qbx_core\n").unwrap();
        let core_manifest = root.join("resources/[qbx]/qbx_core/fxmanifest.lua");
        std::fs::write(core_manifest, "fx_version 'cerulean'\nprovide 'qb-core'\n").unwrap();

        clear_cache();
        let order = StartOrder::discover(&root.join("resources/[standalone]/mything")).unwrap();
        let before = order.started_before("mything");
        assert!(
            before.contains("ox_inventory") && before.contains("ox_lib") && before.contains("qbx_core"),
            "{before:?}"
        );
        assert!(!order.started_before("ox_lib").contains("ox_inventory"), "one category line gives no order");
        assert!(order.started_before("qbx_core").contains("ox_lib"));
        assert!(order.started_before("unlisted").is_empty());
        assert!(order.installed.contains("unlisted") && order.installed.contains("ox_lib"));
        assert!(!order.installed.contains("not_here") && !order.installed.contains("[ox]"));
        assert!(order.installed.contains("qb-core"), "provided names count as installed");
        assert!(
            order.installed.contains("chat") && order.installed.contains("monitor"),
            "system resources are installed"
        );
        assert!(before.contains("qb-core"), "starting qbx_core also starts the name it provides: {before:?}");
        assert!(!order.started_before("qbx_core").contains("qb-core"), "an alias starts with its resource, not before");
        let (bounded, partial) = StartOrder::discover_bounded(
            &root.join("resources/[standalone]/mything"),
            &mut |path| std::fs::read_to_string(path).ok(),
            &mut 1000,
        );
        let bounded = bounded.unwrap();
        assert!(!partial);
        assert_eq!(bounded.installed, order.installed);
        for name in ["mything", "ox_lib", "qbx_core", "unlisted"] {
            assert_eq!(bounded.started_before(name), order.started_before(name));
        }
        let (_, partial) = StartOrder::discover_bounded(
            &root.join("resources/[standalone]/mything"),
            &mut |path| std::fs::read_to_string(path).ok(),
            &mut 1,
        );
        assert!(partial, "exhausted directory inspection must not claim complete installed resources");
        let (_, partial) = StartOrder::discover_bounded(
            &root.join("resources/[standalone]/mything"),
            &mut |path| {
                (path.file_name().unwrap() != "extra.cfg").then(|| std::fs::read_to_string(path).ok()).flatten()
            },
            &mut 1000,
        );
        assert!(partial, "an omitted exec file must be reported");
        std::fs::remove_dir_all(&root).ok();
    }
}
