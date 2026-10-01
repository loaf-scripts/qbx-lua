mod arity;
mod crossfile;
mod docs;
mod fivem;
mod flow;
mod globals;
mod locale;
mod locals;
pub mod manifest;
mod security;

use qbx_fivem_data::Side;
use qbx_lua_syntax::ast::Chunk;
use qbx_lua_syntax::{LineIndex, Span};

use crate::config::FileConfig;
use crate::diagnostic::{Diagnostic, Fix, Severity, Tag};
use crate::directives::Suppressions;
use crate::manifest::Manifest;
use crate::project::ResourceEnv;
use crate::rules;
use crate::scope::Resolution;
use crate::summary::FileSummary;

pub struct FileInput<'a> {
    pub source: &'a str,
    pub chunk: &'a Chunk,
    pub resolution: &'a Resolution,
    pub summary: &'a FileSummary,
    pub config: &'a FileConfig,
    /// `None` when the manifest does not list the file as a script, or there is no manifest.
    pub side: Option<Side>,
    pub resource: Option<ResourceInput<'a>>,
    /// Event handlers and exports of the other files; without it the cross-file rules stay silent.
    pub crossrefs: Option<&'a crate::crossref::CrossRefs>,
    pub locale: Option<&'a crate::locale::LocaleFile>,
    /// Slash-separated path inside the resource, empty when unknown.
    pub relative_path: &'a str,
}

#[derive(Clone, Copy)]
pub struct ResourceInput<'a> {
    pub name: &'a str,
    pub env: &'a ResourceEnv,
    pub manifest: &'a Manifest,
    /// Resources that server.cfg starts before this one, which settles load order without a
    /// `dependency` entry.
    pub started_before: Option<&'a rustc_hash::FxHashSet<qbx_lua_syntax::SmolStr>>,
    /// Every resource in the server's resources folder, including names they `provide`. Only
    /// known when a server.cfg was found, i.e. when the whole server is on disk.
    pub installed: Option<&'a rustc_hash::FxHashSet<qbx_lua_syntax::SmolStr>>,
}

pub(crate) struct Sink<'a> {
    config: &'a FileConfig,
    out: Vec<Diagnostic>,
}

impl<'a> Sink<'a> {
    pub(crate) fn new(config: &'a FileConfig) -> Self {
        Self { config, out: Vec::new() }
    }

    pub(crate) fn enabled(&self, code: &'static str) -> Option<Severity> {
        self.config.severity(code)
    }

    pub(crate) fn report(&mut self, code: &'static str, span: Span, message: impl Into<String>) {
        self.report_with(code, span, message, None, None);
    }

    pub(crate) fn report_with(
        &mut self,
        code: &'static str,
        span: Span,
        message: impl Into<String>,
        tag: Option<Tag>,
        fix: Option<Fix>,
    ) {
        if let Some(severity) = self.enabled(code) {
            self.out.push(Diagnostic { code, severity, span, message: message.into(), tag, fix });
        }
    }

    pub(crate) fn finish(self) -> Vec<Diagnostic> {
        self.out
    }
}

const MAX_SYNTAX_ERRORS: usize = 10;

pub fn check_file(input: &FileInput) -> Vec<Diagnostic> {
    let mut sink = Sink::new(input.config);
    let errors = &input.chunk.errors;
    for error in errors.iter().take(MAX_SYNTAX_ERRORS) {
        sink.report(rules::SYNTAX_ERROR, error.span, error.message.clone());
    }
    if errors.len() > MAX_SYNTAX_ERRORS {
        // Whatever this is, it is not Lua the other rules could say anything useful about.
        let more = errors.len() - MAX_SYNTAX_ERRORS;
        sink.report(
            rules::SYNTAX_ERROR,
            errors[MAX_SYNTAX_ERRORS].span,
            format!("{more} more syntax errors not shown"),
        );
        return sink.finish();
    }
    locals::check(input, &mut sink);
    flow::check(input, &mut sink);
    docs::check(input, &mut sink);
    globals::check(input, &mut sink);
    arity::check(input, &mut sink);
    fivem::check(input, &mut sink);
    crossfile::check(input, &mut sink);
    security::check(input, &mut sink);
    locale::check(input, &mut sink);

    let mut diagnostics = sink.finish();
    let line_index = LineIndex::new(input.source);
    let suppressions = Suppressions::parse(input.source, &input.chunk.comments, &line_index);
    diagnostics.retain(|d| {
        d.code == rules::SYNTAX_ERROR || !suppressions.is_suppressed(d.code, line_index.line_of(d.span.start))
    });
    diagnostics.sort_by_key(|d| (d.span.start, d.code));
    diagnostics
}
