use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use qbx_lua_analysis::lint::FileReport;
use qbx_lua_analysis::{rules, Diagnostic, Severity};
use qbx_lua_syntax::LineIndex;
use serde_json::json;

use crate::Format;

pub struct Options {
    pub color: bool,
    pub fixed: usize,
    pub elapsed: Duration,
}

pub fn render(reports: &[FileReport], format: Format, options: &Options) -> String {
    match format {
        Format::Pretty => pretty(reports, options),
        Format::Compact => compact(reports),
        Format::Json => json_report(reports),
        Format::Junit => junit(reports),
        Format::Github => github(reports),
        Format::Sarif => sarif(reports),
    }
}

fn display_path(path: &Path) -> String {
    let relative = std::env::current_dir().ok().and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf));
    relative.unwrap_or_else(|| path.to_path_buf()).to_string_lossy().replace('\\', "/")
}

struct Located<'a> {
    diagnostic: &'a Diagnostic,
    line: u32,
    col: u32,
    end_line: u32,
    end_col: u32,
}

fn locate<'a>(report: &'a FileReport, index: &LineIndex) -> Vec<Located<'a>> {
    report
        .diagnostics
        .iter()
        .map(|diagnostic| {
            let start = index.line_col(&report.source, diagnostic.span.start);
            let end = index.line_col(&report.source, diagnostic.span.end.max(diagnostic.span.start));
            Located {
                diagnostic,
                line: start.line + 1,
                col: start.col + 1,
                end_line: end.line + 1,
                end_col: end.col + 1,
            }
        })
        .collect()
}

struct Palette {
    color: bool,
}

impl Palette {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn severity(&self, severity: Severity) -> String {
        let code = match severity {
            Severity::Error => "1;31",
            Severity::Warning => "1;33",
            Severity::Info => "1;36",
            Severity::Hint => "1;90",
        };
        self.paint(code, severity.label())
    }
}

fn pretty(reports: &[FileReport], options: &Options) -> String {
    let palette = Palette { color: options.color };
    let mut out = String::new();
    let mut counts = [0usize; 4];
    let mut fixable = 0usize;
    for report in reports.iter().filter(|r| !r.diagnostics.is_empty()) {
        let index = LineIndex::new(&report.source);
        let path = display_path(&report.path);
        for located in locate(report, &index) {
            let d = located.diagnostic;
            counts[d.severity as usize] += 1;
            fixable += usize::from(d.fix.as_ref().is_some_and(|fix| !fix.suggestion));
            let location = palette.paint("1", &format!("{path}:{}:{}", located.line, located.col));
            let code = palette.paint("2", &format!("[{}]", d.code));
            writeln!(out, "{location}: {}{code}: {}", palette.severity(d.severity), d.message).unwrap();

            let line_span = index.line_span(located.line - 1);
            let text = line_span.text(&report.source).trim_end_matches(['\r', '\n']);
            if text.len() <= 240 {
                let gutter = format!("{:>5} | ", located.line);
                writeln!(out, "{}{}", palette.paint("2", &gutter), text.replace('\t', " ")).unwrap();
                let width = if located.end_line == located.line {
                    (located.end_col - located.col).max(1) as usize
                } else {
                    text.chars().count().saturating_sub(located.col as usize - 1).max(1)
                };
                let marker = format!("{}{}", " ".repeat(located.col as usize - 1), "^".repeat(width));
                let marker_color = if d.severity == Severity::Error { "31" } else { "33" };
                writeln!(out, "{}{}", palette.paint("2", "      | "), palette.paint(marker_color, &marker)).unwrap();
            }
        }
    }

    let total: usize = counts.iter().sum();
    let files = reports.len();
    let millis = options.elapsed.as_secs_f64() * 1000.0;
    if options.fixed > 0 {
        writeln!(out, "{}", palette.paint("32", &format!("fixed {} problem(s)", options.fixed))).unwrap();
    }
    if total == 0 {
        writeln!(out, "{}", palette.paint("32", &format!("no problems found in {files} file(s) ({millis:.0} ms)")))
            .unwrap();
        return out;
    }
    let mut parts = Vec::new();
    for severity in [Severity::Error, Severity::Warning, Severity::Info, Severity::Hint] {
        let n = counts[severity as usize];
        if n > 0 {
            parts.push(format!("{n} {}{}", severity.label(), if n == 1 { "" } else { "s" }));
        }
    }
    writeln!(out, "{total} problem(s) ({}) in {files} file(s) ({millis:.0} ms)", parts.join(", ")).unwrap();
    if fixable > 0 {
        writeln!(out, "{fixable} of them can be fixed automatically with --fix").unwrap();
    }
    out
}

fn compact(reports: &[FileReport]) -> String {
    let mut out = String::new();
    for report in reports {
        let index = LineIndex::new(&report.source);
        let path = display_path(&report.path);
        for l in locate(report, &index) {
            let d = l.diagnostic;
            writeln!(out, "{path}:{}:{}: {} [{}] {}", l.line, l.col, d.severity.label(), d.code, d.message).unwrap();
        }
    }
    out
}

fn json_report(reports: &[FileReport]) -> String {
    let files: Vec<_> = reports
        .iter()
        .map(|report| {
            let index = LineIndex::new(&report.source);
            let diagnostics: Vec<_> = locate(report, &index)
                .iter()
                .map(|l| {
                    json!({
                        "code": l.diagnostic.code,
                        "severity": l.diagnostic.severity.label(),
                        "message": l.diagnostic.message,
                        "line": l.line,
                        "column": l.col,
                        "endLine": l.end_line,
                        "endColumn": l.end_col,
                        "fixable": l.diagnostic.fix.as_ref().is_some_and(|fix| !fix.suggestion),
                    })
                })
                .collect();
            json!({ "path": display_path(&report.path), "diagnostics": diagnostics })
        })
        .collect();
    let mut text = serde_json::to_string_pretty(&json!({ "files": files })).unwrap_or_default();
    text.push('\n');
    text
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn junit(reports: &[FileReport]) -> String {
    let total: usize = reports.iter().map(|r| r.diagnostics.len().max(1)).sum();
    let failures: usize = reports.iter().map(|r| r.diagnostics.len()).sum();
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    writeln!(out, "<testsuites name=\"qbx-lint\" tests=\"{total}\" failures=\"{failures}\">").unwrap();
    for report in reports {
        let index = LineIndex::new(&report.source);
        let path = xml_escape(&display_path(&report.path));
        let count = report.diagnostics.len();
        writeln!(out, "  <testsuite name=\"{path}\" tests=\"{}\" failures=\"{count}\">", count.max(1)).unwrap();
        if count == 0 {
            writeln!(out, "    <testcase name=\"{path}\" classname=\"{path}\" file=\"{path}\"/>").unwrap();
        }
        for l in locate(report, &index) {
            let d = l.diagnostic;
            let name = xml_escape(&format!("{path}:{}:{}: {}", l.line, l.col, d.code));
            let message = xml_escape(&d.message);
            writeln!(out, "    <testcase name=\"{name}\" classname=\"{path}\" file=\"{path}\" line=\"{}\">", l.line)
                .unwrap();
            writeln!(
                out,
                "      <failure type=\"{}\" message=\"{message}\">{path}:{}:{}: {message}</failure>",
                d.code, l.line, l.col
            )
            .unwrap();
            writeln!(out, "    </testcase>").unwrap();
        }
        writeln!(out, "  </testsuite>").unwrap();
    }
    out.push_str("</testsuites>\n");
    out
}

fn github_escape(text: &str, property: bool) -> String {
    let mut out = text.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A");
    if property {
        out = out.replace(':', "%3A").replace(',', "%2C");
    }
    out
}

fn github(reports: &[FileReport]) -> String {
    let mut out = String::new();
    for report in reports {
        let index = LineIndex::new(&report.source);
        let path = display_path(&report.path);
        for l in locate(report, &index) {
            let d = l.diagnostic;
            let command = match d.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Info | Severity::Hint => "notice",
            };
            let mut props = format!("file={},line={},col={}", github_escape(&path, true), l.line, l.col);
            if l.end_line == l.line {
                write!(props, ",endColumn={}", l.end_col).unwrap();
            }
            writeln!(
                out,
                "::{command} {props},title={}::{}",
                github_escape(d.code, true),
                github_escape(&d.message, false)
            )
            .unwrap();
        }
    }
    out
}

fn sarif(reports: &[FileReport]) -> String {
    let rule_list: Vec<_> = rules::RULES
        .iter()
        .map(|r| json!({ "id": r.code, "shortDescription": { "text": r.summary }, "properties": { "category": r.category.label() } }))
        .collect();
    let mut results = Vec::new();
    for report in reports {
        let index = LineIndex::new(&report.source);
        let path = display_path(&report.path);
        for l in locate(report, &index) {
            let level = match l.diagnostic.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Info | Severity::Hint => "note",
            };
            results.push(json!({
                "ruleId": l.diagnostic.code,
                "level": level,
                "message": { "text": l.diagnostic.message },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": path },
                        "region": { "startLine": l.line, "startColumn": l.col, "endLine": l.end_line, "endColumn": l.end_col }
                    }
                }]
            }));
        }
    }
    let document = json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": { "name": "qbx-lint", "version": env!("CARGO_PKG_VERSION"), "rules": rule_list } },
            "results": results
        }]
    });
    let mut text = serde_json::to_string_pretty(&document).unwrap_or_default();
    text.push('\n');
    text
}
