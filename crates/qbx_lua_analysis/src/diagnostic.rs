use qbx_lua_syntax::Span;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Hint,
    Info,
    #[serde(alias = "warn")]
    Warning,
    Error,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Hint => "hint",
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Unnecessary,
    Deprecated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub span: Span,
    pub new_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fix {
    pub title: String,
    pub edits: Vec<TextEdit>,
    /// Whether only the editor offers it, as it changes what the code does: `apply_fixes`, and so
    /// `qbx-lint --fix`, leaves it out.
    pub suggestion: bool,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub span: Span,
    pub message: String,
    pub tag: Option<Tag>,
    pub fix: Option<Fix>,
}

/// Applies non-overlapping fixes to `source`; fixes that overlap an earlier one are skipped, and so
/// are suggestions.
pub fn apply_fixes(source: &str, diagnostics: &[Diagnostic]) -> (String, usize) {
    let mut fixes: Vec<&Fix> = diagnostics.iter().filter_map(|d| d.fix.as_ref()).filter(|f| !f.suggestion).collect();
    fixes.sort_by_key(|f| f.edits.iter().map(|e| e.span.start).min().unwrap_or(0));

    let mut edits: Vec<&TextEdit> = Vec::new();
    let mut applied = 0;
    let mut last_end = 0u32;
    for fix in fixes {
        let mut sorted: Vec<&TextEdit> = fix.edits.iter().collect();
        sorted.sort_by_key(|e| e.span.start);
        let start = sorted.first().map_or(0, |e| e.span.start);
        let nested_overlap = sorted.windows(2).any(|w| w[0].span.end > w[1].span.start);
        if (start < last_end && applied > 0) || nested_overlap || sorted.is_empty() {
            continue;
        }
        last_end = sorted.last().map_or(last_end, |e| e.span.end);
        edits.extend(sorted);
        applied += 1;
    }

    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for edit in edits {
        out.push_str(&source[cursor..edit.span.start as usize]);
        out.push_str(&edit.new_text);
        cursor = edit.span.end as usize;
    }
    out.push_str(&source[cursor..]);
    (out, applied)
}
