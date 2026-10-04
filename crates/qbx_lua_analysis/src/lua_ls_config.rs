//! Reads the part of a LuaLS (`.luarc.json`) or EmmyLua (`.emmyrc.json`) configuration that has a
//! qbx-lint equivalent, as a fallback for projects that are set up for either server but have no
//! `qbxlint.toml`.

use qbx_fivem_data::{is_hash_native_name, native, KNOWN_IMPORTS};
use serde_json::Value;

use crate::config::Level;
use crate::env::builtins;

/// Files read when no `qbxlint.toml` is found. All of them present in one directory are merged.
pub const FILE_NAMES: &[&str] = &[".luarc.json", ".luarc.jsonc", ".emmyrc.json"];

/// EmmyLua codes that cover several qbx-lint rules.
const ALIASES: &[(&str, &[&str])] =
    &[("unused", &["unused-local", "unused-function", "unused-argument", "unused-loop-variable", "unused-vararg"])];

/// Codes that check the same thing in both tools. `deprecated` is left out: projects often turn off
/// the LuaLS check, which must not turn off the deprecated runtime functions reported here.
const EQUIVALENT_CODES: &[&str] = &[
    "undefined-global",
    "lowercase-global",
    "unused-local",
    "unused-function",
    "unused-label",
    "unused-vararg",
    "redefined-local",
    "unreachable-code",
    "empty-block",
    "unbalanced-assignments",
    "duplicate-index",
    "duplicate-set-field",
    "count-down-loop",
    "missing-parameter",
    "redundant-parameter",
    "undefined-doc-name",
    "undefined-doc-param",
    "duplicate-doc-alias",
    "duplicate-doc-field",
    "missing-global-doc",
    "missing-local-export-doc",
    "incomplete-signature-doc",
    "missing-fields",
    "assign-type-mismatch",
    "invisible",
    "param-type-mismatch",
    "return-type-mismatch",
    "missing-return",
    "redundant-return-value",
    "discard-returns",
    "cast-type-mismatch",
    "cast-local-type",
    "no-unknown",
    "need-check-nil",
    "inject-field",
    "undefined-field",
    "newline-call",
    "newfield-call",
    "trailing-space",
    "redundant-return",
    "duplicate-doc-param",
    "doc-field-no-class",
    "unknown-operator",
    "unknown-cast-variable",
    "unknown-diag-code",
    "close-non-object",
    "circle-doc-class",
];

/// The diagnostic codes of lua-language-server 3.19.1, including those that have no rule here.
pub const LUA_LS_CODES: &[&str] = &[
    "unused-local",
    "unused-function",
    "unused-label",
    "unused-vararg",
    "trailing-space",
    "redundant-return",
    "empty-block",
    "code-after-break",
    "unreachable-code",
    "redundant-value",
    "unbalanced-assignments",
    "redundant-parameter",
    "missing-parameter",
    "missing-return-value",
    "redundant-return-value",
    "missing-return",
    "missing-fields",
    "need-check-nil",
    "undefined-field",
    "cast-local-type",
    "assign-type-mismatch",
    "param-type-mismatch",
    "cast-type-mismatch",
    "return-type-mismatch",
    "inject-field",
    "unnecessary-assert",
    "duplicate-doc-alias",
    "undefined-doc-class",
    "undefined-doc-name",
    "circle-doc-class",
    "undefined-doc-param",
    "duplicate-doc-param",
    "doc-field-no-class",
    "duplicate-doc-field",
    "unknown-diag-code",
    "unknown-cast-variable",
    "unknown-operator",
    "incomplete-signature-doc",
    "missing-global-doc",
    "missing-local-export-doc",
    "codestyle-check",
    "spell-check",
    "name-style-check",
    "newline-call",
    "newfield-call",
    "ambiguity-1",
    "count-down-loop",
    "different-requires",
    "await-in-sync",
    "not-yieldable",
    "no-unknown",
    "redefined-local",
    "undefined-global",
    "global-in-nil-env",
    "lowercase-global",
    "undefined-env-child",
    "global-element",
    "duplicate-index",
    "duplicate-set-field",
    "close-non-object",
    "deprecated",
    "discard-returns",
    "invisible",
];

/// The names of lua-language-server's syntax errors, which `---@diagnostic` comments may name too.
const LUA_LS_SYNTAX_ERRORS: &[&str] = &[
    "action-after-return",
    "ambiguous-syntax",
    "args-after-dots",
    "assign-const-global",
    "block-after-else",
    "break-outside",
    "declare-const",
    "env-is-global",
    "err-assign-as-eq",
    "err-c-long-comment",
    "err-comment-prefix",
    "err-do-as-then",
    "err-eq-as-assign",
    "err-esc",
    "err-nonstandard-symbol",
    "err-then-as-do",
    "exp-in-action",
    "global-close-attribute",
    "index-in-func-name",
    "jump-local-scope",
    "keyword",
    "local-limit",
    "lua-doc-miss-sign",
    "luadoc-error-diag-mode",
    "luadoc-miss-alias-extends",
    "luadoc-miss-alias-name",
    "luadoc-miss-arg-name",
    "luadoc-miss-cate-name",
    "luadoc-miss-class-extends-name",
    "luadoc-miss-class-name",
    "luadoc-miss-diag-mode",
    "luadoc-miss-diag-name",
    "luadoc-miss-field-extends",
    "luadoc-miss-field-name",
    "luadoc-miss-fun-after-overload",
    "luadoc-miss-generic-name",
    "luadoc-miss-local-name",
    "luadoc-miss-module-name",
    "luadoc-miss-operator-name",
    "luadoc-miss-param-extends",
    "luadoc-miss-param-name",
    "luadoc-miss-see-name",
    "luadoc-miss-sign-name",
    "luadoc-miss-symbol",
    "luadoc-miss-type-name",
    "luadoc-miss-vararg-type",
    "luadoc-miss-version",
    "malformed-number",
    "miss-end",
    "miss-esc-x",
    "miss-exp",
    "miss-exponent",
    "miss-field",
    "miss-loop-max",
    "miss-loop-min",
    "miss-method",
    "miss-name",
    "miss-sep-in-table",
    "miss-space-between",
    "miss-symbol",
    "multi-close",
    "need-paren",
    "nesting-long-mark",
    "no-visible-label",
    "redefined-label",
    "set-const",
    "unexpect-dots",
    "unexpect-efunc-name",
    "unexpect-gfunc-name",
    "unexpect-lfunc-name",
    "unexpect-symbol",
    "unicode-name",
    "unknown-attribute",
    "unknown-symbol",
    "unsupport-named-vararg",
    "unsupport-symbol",
    "variable-not-declared",
];

/// Whether lua-language-server knows `code` as one of its diagnostics or syntax errors.
pub fn is_lua_ls_code(code: &str) -> bool {
    LUA_LS_CODES.contains(&code) || LUA_LS_SYNTAX_ERRORS.contains(&code)
}

#[derive(Debug, Default)]
pub(crate) struct Settings {
    pub globals: Vec<String>,
    /// Rule levels in application order.
    pub rules: Vec<(String, Level)>,
    pub exclude: Vec<String>,
}

/// LuaLS projects list runtime, native and import globals such as `lib` because LuaLS does not read
/// the manifest. Configuring them here would hide the import and client/server checks for them.
fn is_known_global(name: &str) -> bool {
    builtins().get(name).is_some()
        || native(name).is_some()
        || is_hash_native_name(name)
        || KNOWN_IMPORTS.iter().any(|import| import.globals.contains(&name))
}

/// Parses one settings file. EmmyLua resolves `workspace.ignoreDir` from the project root, while
/// LuaLS treats each entry as a gitignore-style pattern.
pub(crate) fn parse(text: &str, emmylua: bool, settings: &mut Settings) -> Result<(), String> {
    let json: Value = serde_json::from_str(&strip_jsonc(text)).map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    flatten("", &json, &mut entries);

    let mut severities = Vec::new();
    let mut disabled = Vec::new();
    let mut turned_on = Vec::new();
    let mut turned_off = Vec::new();
    for (key, value) in entries {
        // LuaLS also accepts the settings under the client's `Lua.` prefix.
        let key = key.strip_prefix("Lua.").unwrap_or(&key);
        match key {
            "diagnostics.globals" => {
                settings.globals.extend(strings(value).into_iter().filter(|name| !is_known_global(name)))
            }
            "diagnostics.disable" => disabled.extend(strings(value)),
            "workspace.ignoreGlobs" => settings.exclude.extend(strings(value)),
            "workspace.ignoreDir" => settings.exclude.extend(strings(value).iter().flat_map(|dir| {
                if emmylua {
                    root_dir_globs(dir)
                } else {
                    gitignore_globs(dir)
                }
            })),
            _ => {
                if let (Some(code), Some(level)) = (key.strip_prefix("diagnostics.severity."), value.as_str()) {
                    if let Some(level) = level_from_severity(level) {
                        severities.push((code.to_string(), level));
                    }
                } else if let (Some(code), Some(status)) =
                    (key.strip_prefix("diagnostics.neededFileStatus."), value.as_str())
                {
                    match status.trim_end_matches('!').to_ascii_lowercase().as_str() {
                        "any" | "opened" => turned_on.push(code.to_string()),
                        "none" => turned_off.push(code.to_string()),
                        _ => {}
                    }
                }
            }
        }
    }
    // A LuaLS file status turns a check on at the check's own severity, a warning for each check
    // that is off here by default, unless `diagnostics.severity` gives one, and `None` turns it off
    // whatever its severity. Both servers let `diagnostics.disable` win over everything.
    let off_by_default = |code: &String| crate::rules::find(code).is_some_and(|rule| rule.default.is_none());
    let turned_on = turned_on.into_iter().filter(off_by_default).map(|code| (code, Level::Warning));
    let turned_off = turned_off.into_iter().chain(disabled).map(|code| (code, Level::Off));
    for (code, level) in turned_on.chain(severities).chain(turned_off) {
        match ALIASES.iter().find(|(alias, _)| *alias == code) {
            Some((_, codes)) => settings.rules.extend(codes.iter().map(|c| (c.to_string(), level))),
            None if EQUIVALENT_CODES.contains(&code.as_str()) => settings.rules.push((code, level)),
            None => {}
        }
    }
    Ok(())
}

/// Collects `a.b.c` keys for nested objects, the form EmmyLua writes, alongside the dotted keys
/// LuaLS writes. Arrays are values.
fn flatten<'a>(prefix: &str, value: &'a Value, out: &mut Vec<(String, &'a Value)>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, value) in map {
                let key = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                flatten(&key, value, out);
            }
        }
        _ => out.push((prefix.to_string(), value)),
    }
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

fn level_from_severity(severity: &str) -> Option<Level> {
    // LuaLS marks a forced severity with a trailing `!`.
    match severity.trim_end_matches('!').to_ascii_lowercase().as_str() {
        "error" => Some(Level::Error),
        "warning" => Some(Level::Warning),
        "information" | "info" => Some(Level::Info),
        "hint" => Some(Level::Hint),
        _ => None,
    }
}

/// A LuaLS `ignoreDir` entry follows gitignore rules: a `\` escapes the next character, and only a
/// pattern with a `/` before its end is anchored to the root.
fn gitignore_globs(pattern: &str) -> Vec<String> {
    let pattern = pattern.trim();
    if pattern.is_empty() || pattern.starts_with('!') || pattern.starts_with('#') {
        return Vec::new();
    }
    let mut glob = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(escaped) = chars.next() {
                    push_literal(&mut glob, escaped);
                }
            }
            _ => glob.push(c),
        }
    }
    let glob = glob.trim_end_matches('/');
    if glob.contains('/') {
        let glob = glob.trim_start_matches('/');
        vec![glob.to_string(), format!("{glob}/**")]
    } else {
        vec![format!("**/{glob}"), format!("**/{glob}/**")]
    }
}

/// An EmmyLua `ignoreDir` entry is a directory path from the root. Paths that EmmyLua expands from
/// the home directory or environment cannot be matched against the project and are skipped.
fn root_dir_globs(dir: &str) -> Vec<String> {
    let dir = dir.trim().trim_start_matches("./").trim_matches('/');
    if dir.is_empty() || dir.starts_with(['~', '$', '{']) || std::path::Path::new(dir).is_absolute() {
        return Vec::new();
    }
    let mut glob = String::new();
    dir.chars().for_each(|c| push_literal(&mut glob, c));
    vec![format!("{glob}/**")]
}

fn push_literal(glob: &mut String, c: char) {
    if matches!(c, '*' | '?' | '[' | ']' | '{' | '}') {
        glob.extend(['[', c, ']']);
    } else {
        glob.push(c);
    }
}

/// LuaLS reads its configuration as JSON with comments and trailing commas.
fn strip_jsonc(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let without_comments = outside_strings(text, |c, rest, out| match (c, rest.peek()) {
        ('/', Some('/')) => while rest.next_if(|&n| n != '\n').is_some() {},
        ('/', Some('*')) => {
            rest.next();
            let mut previous = '\0';
            for n in rest.by_ref() {
                if previous == '*' && n == '/' {
                    break;
                }
                previous = n;
            }
        }
        _ => out.push(c),
    });
    outside_strings(&without_comments, |c, rest, out| {
        let trailing_comma = c == ',' && matches!(rest.clone().find(|n| !n.is_whitespace()), Some('}' | ']'));
        if !trailing_comma {
            out.push(c);
        }
    })
}

/// Copies string literals unchanged and passes every other character to `handle`.
fn outside_strings(
    text: &str,
    mut handle: impl FnMut(char, &mut std::iter::Peekable<std::str::Chars>, &mut String),
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            handle(c, &mut chars, &mut out);
            continue;
        }
        out.push(c);
        while let Some(c) = chars.next() {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => break,
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_luals(text: &str) -> Settings {
        let mut settings = Settings::default();
        parse(text, false, &mut settings).unwrap();
        settings
    }

    #[test]
    fn reads_dotted_nested_and_prefixed_keys() {
        let dotted =
            parse_luals(r#"{ "diagnostics.globals": ["Config"], "diagnostics.disable": ["lowercase-global"] }"#);
        let nested = parse_luals(r#"{ "diagnostics": { "globals": ["Config"], "disable": ["lowercase-global"] } }"#);
        let prefixed = parse_luals(
            r#"{ "Lua.diagnostics.globals": ["Config"], "Lua.diagnostics.disable": ["lowercase-global"] }"#,
        );
        for settings in [dotted, nested, prefixed] {
            assert_eq!(settings.globals, ["Config"]);
            assert_eq!(settings.rules, [("lowercase-global".to_string(), Level::Off)]);
        }
    }

    #[test]
    fn drops_globals_that_are_already_known() {
        let settings = parse_luals(
            r#"{ "diagnostics.globals": ["lib", "cache", "MySQL", "Citizen", "CreateThread", "GetPlayerPed", "Config"] }"#,
        );
        assert_eq!(settings.globals, ["Config"]);
    }

    #[test]
    fn maps_only_codes_that_mean_the_same() {
        let settings = parse_luals(
            r#"{ "diagnostics.disable": ["undefined-field", "deprecated", "syntax-error", "undefined-global", "missing-parameter", "duplicate-set-field"] }"#,
        );
        assert_eq!(
            settings.rules,
            [
                ("undefined-field".to_string(), Level::Off),
                ("undefined-global".to_string(), Level::Off),
                ("missing-parameter".to_string(), Level::Off),
                ("duplicate-set-field".to_string(), Level::Off)
            ]
        );
        let targets = EQUIVALENT_CODES.iter().chain(ALIASES.iter().flat_map(|(_, codes)| codes.iter()));
        for code in targets {
            assert!(crate::rules::find(code).is_some(), "{code} is not a rule");
        }
        for code in EQUIVALENT_CODES {
            assert!(LUA_LS_CODES.contains(code), "{code} is not a lua-language-server code");
        }
    }

    #[test]
    fn disable_wins_over_severity_and_aliases_expand() {
        let settings = parse_luals(
            r#"{ "diagnostics.severity": { "unused-local": "Hint!", "undefined-global": "Error" },
                 "diagnostics.disable": ["unused-local"] }"#,
        );
        assert_eq!(settings.rules.last(), Some(&("unused-local".to_string(), Level::Off)));
        assert!(settings.rules.contains(&("undefined-global".to_string(), Level::Error)));

        let settings = parse_luals(r#"{ "diagnostics": { "disable": ["unused"] } }"#);
        assert_eq!(settings.rules.len(), 5);
        assert!(settings.rules.iter().all(|(code, level)| code.starts_with("unused-") && *level == Level::Off));
    }

    #[test]
    fn file_status_turns_checks_on_and_off() {
        let settings = parse_luals(
            r#"{ "diagnostics.neededFileStatus": { "missing-global-doc": "Any", "incomplete-signature-doc": "Opened!",
                     "missing-local-export-doc": "Any", "unused-local": "Opened", "need-check-nil": "None", "no-unknown": "Any" },
                 "diagnostics.severity": { "missing-local-export-doc": "Hint", "need-check-nil": "Error" },
                 "diagnostics.disable": ["no-unknown"] }"#,
        );
        let level = |code: &str| settings.rules.iter().rev().find(|(c, _)| c == code).map(|(_, level)| *level);
        // Checks that are off here by default turn on as warnings, unless a severity is given.
        assert_eq!(level("missing-global-doc"), Some(Level::Warning));
        assert_eq!(level("incomplete-signature-doc"), Some(Level::Warning));
        assert_eq!(level("missing-local-export-doc"), Some(Level::Hint));
        // Rules on by default keep their level.
        assert_eq!(level("unused-local"), None);
        // `None` wins over a severity, and `diagnostics.disable` over a status.
        assert_eq!(level("need-check-nil"), Some(Level::Off));
        assert_eq!(level("no-unknown"), Some(Level::Off));
    }

    #[test]
    fn accepts_comments_and_trailing_commas() {
        let settings = parse_luals(
            "\u{feff}{\n  // comment\n  \"diagnostics.globals\": [\"a//b\", \"c/*d*/\",], /* block */\n}\n",
        );
        assert_eq!(settings.globals, ["a//b", "c/*d*/"]);
    }

    #[test]
    fn converts_ignore_dir_patterns() {
        assert_eq!(gitignore_globs("node_modules"), ["**/node_modules", "**/node_modules/**"]);
        assert_eq!(gitignore_globs("web/"), ["**/web", "**/web/**"]);
        assert_eq!(gitignore_globs("res/web"), ["res/web", "res/web/**"]);
        assert_eq!(gitignore_globs(r"\[standalone\]"), ["**/[[]standalone[]]", "**/[[]standalone[]]/**"]);
        assert!(gitignore_globs("!keep").is_empty());
        assert_eq!(root_dir_globs("./res/[standalone]/"), ["res/[[]standalone[]]/**"]);
        assert!(root_dir_globs("~/lua").is_empty());
    }
}
