use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::checks::manifest::{check_manifest, ManifestInput};
use crate::checks::{check_file, FileInput, ResourceInput};
use crate::config::Config;
use crate::crossref::CrossRefs;
use crate::diagnostic::{Diagnostic, Severity};
use crate::locale::{locale_usage, LocaleFile};
use crate::project::{
    find_manifest_dir, is_manifest_file, lua_files_under, read_source, relative_slash_path, walks_into, ParsedFile,
    Resource, ResourceLocator,
};
use crate::rules;
use qbx_lua_syntax::parse;

pub struct FileReport {
    pub path: PathBuf,
    pub source: String,
    pub diagnostics: Vec<Diagnostic>,
}

const ANALYSIS_STACK_SIZE: usize = 32 * 1024 * 1024;

/// Lints every Lua file under `paths`. Files that belong to a resource are checked against the
/// globals of the whole resource, even when only some of its files were requested.
pub fn lint_paths(paths: &[PathBuf], config: &Config) -> Vec<FileReport> {
    let mut targets: Vec<PathBuf> = Vec::new();
    for path in paths {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.clone());
        if path.is_dir() {
            targets.extend(lua_files_under(&path, config));
        } else if !config.is_excluded(&path) {
            targets.push(path);
        }
    }
    targets.sort();
    targets.dedup();

    let mut by_resource: BTreeMap<Option<PathBuf>, Vec<PathBuf>> = BTreeMap::new();
    for target in targets {
        by_resource.entry(find_manifest_dir(&target)).or_default().push(target);
    }

    let locator = Mutex::new(ResourceLocator::default());
    let pool = rayon::ThreadPoolBuilder::new().stack_size(ANALYSIS_STACK_SIZE).build().expect("thread pool");
    let mut reports: Vec<FileReport> = pool.install(|| {
        // Syntax trees are not kept between the two passes: parsing twice is cheaper than holding
        // every resource of a server in memory at once.
        let crossrefs = by_resource
            .par_iter()
            .map(|(root, files)| collect_crossrefs(root.as_deref(), files, config, &locator))
            .reduce(CrossRefs::default, |mut all, part| {
                all.merge(part);
                all
            });
        by_resource
            .into_par_iter()
            .flat_map(|(root, files)| match root {
                Some(root) => lint_resource(&root, &files, config, &locator, &crossrefs),
                None => files.par_iter().filter_map(|path| lint_loose_file(path, config, &crossrefs)).collect(),
            })
            .collect()
    });
    reports.sort_by(|a, b| a.path.cmp(&b.path));
    reports
}

fn load_resource(root: &Path, config: &Config, locator: &Mutex<ResourceLocator>) -> Option<Resource> {
    let mut locator = locator.lock().unwrap_or_else(|e| e.into_inner());
    Resource::load(root, config, &mut locator)
}

fn collect_crossrefs(
    root: Option<&Path>,
    files: &[PathBuf],
    config: &Config,
    locator: &Mutex<ResourceLocator>,
) -> CrossRefs {
    let mut refs = CrossRefs::default();
    match root.and_then(|root| load_resource(root, config, locator)) {
        Some(resource) => {
            for file in &resource.files {
                refs.collect(&file.chunk, file.side, Some(&resource.name));
            }
            if resource.env.opaque || resource.manifest.has_non_lua_scripts() {
                refs.opaque_resources.insert(resource.name.as_str().into());
            }
        }
        None => {
            for source in files.iter().filter_map(|path| read_source(path).ok()) {
                refs.collect(&parse(&source), None, None);
            }
        }
    }
    refs
}

fn lint_loose_file(path: &Path, config: &Config, crossrefs: &CrossRefs) -> Option<FileReport> {
    if config.ignores_diagnostics(path) {
        return None;
    }
    let source = read_source(path).ok()?;
    let file = ParsedFile::new(path.to_path_buf(), String::new(), source, None);
    let file_config = config.for_file(path);
    let diagnostics = check_file(&FileInput {
        source: &file.source,
        chunk: &file.chunk,
        resolution: &file.resolution,
        summary: &file.summary,
        config: &file_config,
        side: None,
        resource: None,
        crossrefs: Some(crossrefs),
        locale: None,
        relative_path: "",
        value_count: None,
    });
    Some(FileReport { path: file.path, source: file.source, diagnostics })
}

/// Keys of the locale file that no `locale()` call of the resource can reach.
pub fn unused_locale_keys<'a>(
    locale: &'a LocaleFile,
    chunks: impl Iterator<Item = &'a qbx_lua_syntax::ast::Chunk>,
) -> Vec<Diagnostic> {
    unused_locale_keys_from(locale, chunks.map(locale_usage))
}

pub fn unused_locale_keys_from(
    locale: &LocaleFile,
    usages: impl Iterator<Item = crate::locale::LocaleUsage>,
) -> Vec<Diagnostic> {
    let mut used = Vec::new();
    let mut prefixes = Vec::new();
    for usage in usages {
        if usage.dynamic {
            return Vec::new();
        }
        used.extend(usage.keys.into_iter().map(|(key, _)| key));
        prefixes.extend(usage.prefixes);
    }
    if used.is_empty() && prefixes.is_empty() {
        return Vec::new();
    }
    locale
        .keys
        .iter()
        .filter(|(key, ..)| !used.contains(key) && !prefixes.iter().any(|p| key.starts_with(p.as_str())))
        .map(|(key, span, _)| Diagnostic {
            code: rules::UNUSED_LOCALE_KEY,
            severity: Severity::Info,
            span: *span,
            message: format!("locale key '{key}' is never used by this resource"),
            tag: None,
            fix: None,
        })
        .collect()
}

fn lint_resource(
    root: &Path,
    targets: &[PathBuf],
    config: &Config,
    locator: &Mutex<ResourceLocator>,
    crossrefs: &CrossRefs,
) -> Vec<FileReport> {
    let Some(resource) = load_resource(root, config, locator) else {
        return targets.iter().filter_map(|path| lint_loose_file(path, config, crossrefs)).collect();
    };
    let locale = LocaleFile::load(root);
    let start_order = crate::startup::StartOrder::discover(root);
    let started_before = start_order.as_ref().map(|order| order.started_before(&resource.name));

    let mut reports: Vec<FileReport> = resource
        .files
        .par_iter()
        .enumerate()
        .filter(|(_, file)| targets.contains(&file.path) && !config.ignores_diagnostics(&file.path))
        .map(|(index, file)| {
            let mut file_config = config.for_file(&file.path);
            if resource.manifest.is_map_file(&file.relative) {
                file_config.set(rules::UNDEFINED_GLOBAL, crate::config::Level::Off);
            }
            let diagnostics = check_file(&FileInput {
                source: &file.source,
                chunk: &file.chunk,
                resolution: &file.resolution,
                summary: &file.summary,
                config: &file_config,
                side: file.side,
                resource: Some(ResourceInput {
                    name: &resource.name,
                    env: resource.env_for(index),
                    manifest: &resource.manifest,
                    started_before: started_before.as_ref(),
                    installed: start_order.as_ref().map(|order| &order.installed),
                }),
                crossrefs: Some(crossrefs),
                locale: locale.as_ref(),
                relative_path: &file.relative,
                value_count: None,
            });
            FileReport { path: file.path.clone(), source: file.source.clone(), diagnostics }
        })
        .collect();

    let whole_resource = targets.iter().any(|t| is_manifest_file(t) && t.parent() == Some(root));
    if whole_resource {
        if !config.ignores_diagnostics(&resource.manifest_path) {
            if let Ok(source) = read_source(&resource.manifest_path) {
                let chunk = parse(&source);
                let resource_files = all_files(root);
                let file_config = config.for_file(&resource.manifest_path);
                let diagnostics = check_manifest(&ManifestInput {
                    source: &source,
                    chunk: &chunk,
                    manifest: &resource.manifest,
                    config: &file_config,
                    resource_files: &resource_files,
                    has_lua_scripts: !resource.files.is_empty(),
                });
                reports.push(FileReport { path: resource.manifest_path.clone(), source, diagnostics });
            }
        }
        // Encrypted scripts may use any key, so "unused" cannot be decided for such a resource.
        if let Some(locale) = locale.filter(|l| !resource.env.opaque && !config.ignores_diagnostics(&l.path)) {
            let severity = config.for_file(&locale.path).severity(rules::UNUSED_LOCALE_KEY);
            let mut diagnostics = unused_locale_keys(&locale, resource.files.iter().map(|f| &f.chunk));
            match severity {
                Some(severity) => diagnostics.iter_mut().for_each(|d| d.severity = severity),
                None => diagnostics.clear(),
            }
            if !diagnostics.is_empty() {
                reports.push(FileReport { path: locale.path, source: locale.source, diagnostics });
            }
        }
    }
    reports
}

pub fn all_files(root: &Path) -> Vec<String> {
    let walked = root.canonicalize().ok();
    WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            let skipped = e.depth() > 0 && (name == "node_modules" || name == ".git");
            !skipped && walks_into(walked.as_deref(), e)
        })
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| relative_slash_path(root, e.path()))
        .collect()
}
