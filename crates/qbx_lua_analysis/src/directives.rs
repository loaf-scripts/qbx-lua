use qbx_lua_syntax::{Comment, LineIndex};

#[derive(Debug)]
struct LineRule {
    line: u32,
    codes: Vec<String>,
}

#[derive(Debug)]
struct RangeRule {
    from_line: u32,
    to_line: u32,
    codes: Vec<String>,
}

/// Inline suppressions from `-- qbx-lint: disable-next-line code` and LuaLS style
/// `---@diagnostic disable-next-line: code` comments.
#[derive(Debug, Default)]
pub struct Suppressions {
    lines: Vec<LineRule>,
    ranges: Vec<RangeRule>,
}

enum Action {
    Disable,
    Enable,
    DisableLine,
    DisableNextLine,
}

fn parse_directive(text: &str, own_line: bool) -> Option<(Action, Vec<String>)> {
    let text = text.trim_start_matches('-').trim();
    if let Some(luacheck) = text.strip_prefix("luacheck:") {
        let ignores = luacheck.split_whitespace().next() == Some("ignore");
        let action = if own_line { Action::Disable } else { Action::DisableLine };
        return ignores.then_some((action, Vec::new()));
    }
    let rest = text
        .strip_prefix("qbx-lint:")
        .or_else(|| text.strip_prefix("qbxlint:"))
        .or_else(|| text.strip_prefix("@diagnostic"))?
        .trim();
    // Any whitespace separates, so a non-breaking space pasted in where a space was meant still works.
    let (action, codes) = rest.split_once(|c: char| c == ':' || c.is_whitespace()).unwrap_or((rest, ""));
    let action = match action {
        "disable" => Action::Disable,
        "enable" => Action::Enable,
        "disable-line" => Action::DisableLine,
        "disable-next-line" => Action::DisableNextLine,
        _ => return None,
    };
    let codes = codes
        .trim_start_matches(':')
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|c| !c.is_empty())
        .take_while(|c| !c.starts_with("--"))
        .map(str::to_string)
        .collect();
    Some((action, codes))
}

impl Suppressions {
    pub fn parse(source: &str, comments: &[Comment], line_index: &LineIndex) -> Self {
        let mut out = Self::default();
        let mut open: Vec<(u32, Vec<String>)> = Vec::new();
        for comment in comments {
            let line = line_index.line_of(comment.span.start);
            let before = &source[line_index.line_start(line) as usize..comment.span.start as usize];
            let Some((action, codes)) = parse_directive(comment.span.text(source), before.trim().is_empty()) else {
                continue;
            };
            match action {
                Action::DisableLine => out.lines.push(LineRule { line, codes }),
                Action::DisableNextLine => out.lines.push(LineRule { line: line + 1, codes }),
                Action::Disable => open.push((line, codes)),
                Action::Enable => {
                    let (closed, still_open): (Vec<_>, Vec<_>) = open
                        .into_iter()
                        .partition(|(_, open_codes)| codes.is_empty() || open_codes.iter().any(|c| codes.contains(c)));
                    open = still_open;
                    out.ranges.extend(closed.into_iter().map(|(from_line, codes)| RangeRule {
                        from_line,
                        to_line: line,
                        codes,
                    }));
                }
            }
        }
        out.ranges.extend(open.into_iter().map(|(from_line, codes)| RangeRule { from_line, to_line: u32::MAX, codes }));
        out
    }

    pub fn is_suppressed(&self, code: &str, line: u32) -> bool {
        let matches = |codes: &[String]| codes.is_empty() || codes.iter().any(|c| c == code);
        self.lines.iter().any(|rule| rule.line == line && matches(&rule.codes))
            || self.ranges.iter().any(|rule| rule.from_line <= line && line <= rule.to_line && matches(&rule.codes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qbx_lua_syntax::parse;

    fn suppressions(source: &str) -> Suppressions {
        let chunk = parse(source);
        Suppressions::parse(source, &chunk.comments, &LineIndex::new(source))
    }

    #[test]
    fn line_directives() {
        let s = suppressions("-- qbx-lint: disable-next-line unused-local\nlocal a\nlocal b ---@diagnostic disable-line: unused-local, empty-block\nlocal c -- qbx-lint: disable-line");
        assert!(s.is_suppressed("unused-local", 1));
        assert!(!s.is_suppressed("empty-block", 1));
        assert!(s.is_suppressed("empty-block", 2));
        assert!(s.is_suppressed("anything", 3));
        assert!(!s.is_suppressed("unused-local", 0));
    }

    #[test]
    fn luacheck_comments_are_honoured() {
        let s = suppressions(
            "function A() -- luacheck: ignore
end
-- luacheck: ignore
function B() end",
        );
        assert!(s.is_suppressed("builtin-overwrite", 0));
        assert!(!s.is_suppressed("builtin-overwrite", 1));
        assert!(s.is_suppressed("builtin-overwrite", 3));
    }

    #[test]
    fn range_directives() {
        let s = suppressions("---@diagnostic disable: undefined-global\nx()\n---@diagnostic enable: undefined-global\ny()\n-- qbx-lint: disable\nz()");
        assert!(s.is_suppressed("undefined-global", 1));
        assert!(!s.is_suppressed("undefined-global", 3));
        assert!(s.is_suppressed("unused-local", 5));
    }

    #[test]
    fn unicode_whitespace_separates() {
        let s = suppressions("-- qbx-lint: disable-next-line\u{a0}undefined-global\u{3000}unused-local\nprint(foo)\nlocal a ---@diagnostic disable-line:\u{a0}empty-block,\u{a0}unused-local\n-- qbx-lint: disable\u{3000}lowercase-global\nx = 1");
        assert!(s.is_suppressed("undefined-global", 1));
        assert!(s.is_suppressed("unused-local", 1));
        assert!(!s.is_suppressed("empty-block", 1));
        assert!(s.is_suppressed("empty-block", 2));
        assert!(s.is_suppressed("unused-local", 2));
        assert!(s.is_suppressed("lowercase-global", 4));
        assert!(!s.is_suppressed("undefined-global", 4));
    }
}
