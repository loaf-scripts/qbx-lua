//! The diagnostics of the files nobody has open: the server publishes them for the Problems panel of
//! the editor, and `qbx-lua-ls --check` prints them, so that CI and the command line see what the
//! editor shows, the type rules included.

use std::path::{Path, PathBuf};

use lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString, Url};
use qbx_lua_analysis::crossref::CrossRefs;
use qbx_lua_analysis::locale::{locale_usage, LocaleFile, LocaleUsage};

use crate::document::Document;
use crate::features::diagnostics::{self, RuleSettings};
use crate::index::{FileEntry, FileId, FileOrigin, ResourceId};
use crate::workspace::{path_to_uri, Workspace};

/// A file the workspace pass checks: a Lua file of the workspace, with the id the index has for it,
/// or the manifest of a resource.
pub struct Target {
    pub uri: Url,
    pub path: PathBuf,
    pub file: Option<FileId>,
    pub resource: Option<ResourceId>,
}

/// The Lua files of the workspace that `file_in_scope` picks, and the manifests in its folders of
/// the resources that `resource_in_scope` picks.
pub fn targets(
    ws: &Workspace,
    file_in_scope: impl Fn(FileId, &FileEntry) -> bool,
    resource_in_scope: impl Fn(ResourceId) -> bool,
) -> Vec<Target> {
    let mut targets = Vec::new();
    for (id, file) in ws.index.files().filter(|(id, f)| f.origin == FileOrigin::Workspace && file_in_scope(*id, f)) {
        let (uri, path) = (file.uri.clone(), file.path.clone());
        targets.push(Target { uri, path, file: Some(id), resource: file.resource });
    }
    let in_workspace = |path: &Path| ws.roots.iter().any(|root| path.starts_with(root));
    for (id, resource) in ws.index.resources.iter().enumerate() {
        let id = id as ResourceId;
        if in_workspace(&resource.manifest_path) && resource_in_scope(id) {
            let (uri, path) = (path_to_uri(&resource.manifest_path), resource.manifest_path.clone());
            targets.push(Target { uri, path, file: None, resource: None });
        }
    }
    targets
}

/// The diagnostics of `target`, whose source is `text`, with the locale keys it uses.
pub fn file_diagnostics(
    ws: &mut Workspace,
    target: &Target,
    text: String,
    settings: &RuleSettings,
    crossrefs: &CrossRefs,
) -> (Vec<Diagnostic>, LocaleUsage) {
    let mut doc = Document::new(target.uri.clone(), target.path.clone(), 0, text);
    doc.file = target.file.unwrap_or_else(|| ws.index.allocate(&doc.path));
    let usage = locale_usage(&doc.chunk);
    (diagnostics::diagnostics(ws, &doc, settings, crossrefs), usage)
}

/// The keys of the locale file of `resource` that none of `usages`, those of its scripts, uses,
/// with the URI of that file. `None` when the resource has no locale file, the rule is off for it,
/// or the resource is escrowed: its encrypted scripts may use any key.
pub fn unused_locale_keys(
    ws: &Workspace,
    resource: ResourceId,
    usages: Vec<LocaleUsage>,
    settings: &RuleSettings,
) -> Option<(Url, Vec<Diagnostic>)> {
    let entry = ws.index.resource(resource).filter(|entry| !entry.escrowed)?;
    let locale = LocaleFile::load(&entry.root)?;
    if diagnostics::is_silenced(ws, &locale.path) {
        return None;
    }
    let mut config = ws.lint_config.for_file(&locale.path);
    settings.apply(&mut config);
    let severity = config.severity(qbx_lua_analysis::rules::UNUSED_LOCALE_KEY)?;
    let lines = qbx_lua_syntax::LineIndex::new(&locale.source);
    let found = qbx_lua_analysis::lint::unused_locale_keys_from(&locale, usages.into_iter())
        .into_iter()
        .map(|d| Diagnostic {
            range: crate::indexer::span_to_range(&locale.source, &lines, d.span),
            severity: Some(match severity {
                qbx_lua_analysis::Severity::Error => DiagnosticSeverity::ERROR,
                qbx_lua_analysis::Severity::Warning => DiagnosticSeverity::WARNING,
                qbx_lua_analysis::Severity::Info => DiagnosticSeverity::INFORMATION,
                qbx_lua_analysis::Severity::Hint => DiagnosticSeverity::HINT,
            }),
            code: Some(NumberOrString::String(d.code.to_string())),
            source: Some(diagnostics::SOURCE.to_string()),
            message: d.message,
            tags: Some(vec![DiagnosticTag::UNNECESSARY]),
            ..Diagnostic::default()
        })
        .collect();
    Some((path_to_uri(&locale.path), found))
}

/// What `qbx-lua-ls --check` checks, and how.
#[derive(Default)]
pub struct CheckOptions {
    /// The folders to check, each a workspace root as the editor opens it.
    pub roots: Vec<PathBuf>,
    /// Folders indexed for their declarations but not checked, as the `qbxLua.library` setting.
    pub library: Vec<PathBuf>,
    pub settings: RuleSettings,
    /// How many warnings may be found before the check fails. Any error fails it.
    pub max_warnings: usize,
}

/// What a check found.
pub struct CheckOutcome {
    /// One `path:line:column: level [code] message` line per diagnostic, as `qbx-lint --format
    /// compact` writes them, sorted by path and position.
    pub lines: Vec<String>,
    pub files: usize,
    pub errors: usize,
    pub warnings: usize,
}

impl CheckOutcome {
    pub fn failed(&self, options: &CheckOptions) -> bool {
        self.errors > 0 || self.warnings > options.max_warnings
    }
}

/// Checks the folders `options` names as the editor checks the files nobody has open. Hints, which
/// the editor shows faded in open files only, are left out.
pub fn check(options: &CheckOptions) -> CheckOutcome {
    let mut ws = Workspace::default();
    ws.roots = options.roots.iter().map(|root| std::path::absolute(root).unwrap_or_else(|_| root.clone())).collect();
    ws.library = options.library.clone();
    ws.load_stubs();
    ws.scan();
    let targets = targets(&ws, |_, _| true, |_| true);
    let crossrefs = ws.crossrefs();
    let mut found: Vec<(PathBuf, Diagnostic)> = Vec::new();
    let mut usages: Vec<(ResourceId, LocaleUsage)> = Vec::new();
    for target in &targets {
        let Ok(text) = qbx_lua_analysis::project::read_source(&target.path) else { continue };
        let (diagnostics, usage) = file_diagnostics(&mut ws, target, text, &options.settings, &crossrefs);
        usages.extend(target.resource.map(|resource| (resource, usage)));
        found.extend(diagnostics.into_iter().map(|d| (target.path.clone(), d)));
    }
    let mut by_resource: Vec<(ResourceId, Vec<LocaleUsage>)> = Vec::new();
    for (resource, usage) in usages {
        match by_resource.iter_mut().find(|(id, _)| *id == resource) {
            Some((_, list)) => list.push(usage),
            None => by_resource.push((resource, vec![usage])),
        }
    }
    for (resource, usages) in by_resource {
        if let Some((uri, diagnostics)) = unused_locale_keys(&ws, resource, usages, &options.settings) {
            let path = crate::workspace::uri_to_path(&uri).unwrap_or_default();
            found.extend(diagnostics.into_iter().map(|d| (path.clone(), d)));
        }
    }
    found.retain(|(_, d)| d.severity != Some(DiagnosticSeverity::HINT));
    found.sort_by(|(a, x), (b, y)| {
        let at = |d: &Diagnostic| (d.range.start.line, d.range.start.character);
        a.cmp(b).then(at(x).cmp(&at(y)))
    });
    let count = |severity| found.iter().filter(|(_, d)| d.severity == Some(severity)).count();
    let (errors, warnings) = (count(DiagnosticSeverity::ERROR), count(DiagnosticSeverity::WARNING));
    let lines = found.iter().map(|(path, d)| line_of(path, d)).collect();
    CheckOutcome { lines, files: targets.len(), errors, warnings }
}

fn line_of(path: &Path, d: &Diagnostic) -> String {
    let level = match d.severity {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        _ => "hint",
    };
    let code = match &d.code {
        Some(NumberOrString::String(code)) => code.clone(),
        Some(NumberOrString::Number(code)) => code.to_string(),
        None => String::new(),
    };
    let (line, column) = (d.range.start.line + 1, d.range.start.character + 1);
    format!("{}:{line}:{column}: {level} [{code}] {}", display_path(path), d.message)
}

/// `path` relative to the current folder when it is inside it, with forward slashes, as qbx-lint
/// shows paths.
fn display_path(path: &Path) -> String {
    let relative = std::env::current_dir().ok().and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf));
    relative.unwrap_or_else(|| path.to_path_buf()).to_string_lossy().replace('\\', "/")
}
