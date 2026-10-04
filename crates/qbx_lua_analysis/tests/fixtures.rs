use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use qbx_lua_analysis::lint::lint_paths;
use qbx_lua_analysis::project::{find_manifest_dir, manifest_path, ResourceLocator};
use qbx_lua_analysis::{apply_fixes, Config, Level};
use qbx_lua_syntax::LineIndex;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn config_with_everything_enabled() -> Config {
    let mut config = Config::default();
    for rule in qbx_lua_analysis::rules::RULES.iter().filter(|r| r.default.is_none()) {
        if rule.code != qbx_lua_analysis::rules::SHADOWED_LOCAL {
            config.set_rule(rule.code, Level::Warning);
        }
    }
    config
}

fn render(root: &Path, config: &Config) -> String {
    let mut out = String::new();
    for report in lint_paths(&[root.to_path_buf()], config) {
        let index = LineIndex::new(&report.source);
        let relative = report.path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        for diagnostic in &report.diagnostics {
            let pos = index.line_col(&report.source, diagnostic.span.start);
            writeln!(
                out,
                "{relative}:{}:{} {} {}",
                pos.line + 1,
                pos.col + 1,
                diagnostic.severity.label(),
                diagnostic.code
            )
            .unwrap();
        }
    }
    out
}

/// Run with `QBX_BLESS=1` to rewrite the expectation after an intentional change.
fn assert_snapshot(name: &str, actual: &str) {
    let path = fixtures().join(format!("{name}.expected"));
    if std::env::var_os("QBX_BLESS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
    assert_eq!(actual, expected, "snapshot {name} changed; rerun with QBX_BLESS=1 if this is intended");
}

#[test]
fn bad_resource_reports_every_rule() {
    let actual = render(&fixtures().join("bad_resource"), &Config::default());
    assert_snapshot("bad_resource", &actual);
    for rule in qbx_lua_analysis::rules::RULES.iter().filter(|r| r.default.is_some()) {
        let exercised = actual.contains(&format!(" {}\n", rule.code));
        let covered_elsewhere = matches!(
            rule.code,
            "syntax-error"
                | "unused-loop-variable"
                | "unused-label"
                | "redefined-local"
                | "self-comparison"
                | "builtin-overwrite"
                | "qbox/prefer-cache"
                | "manifest/missing-field"
                | "fivem/event-argument-count"
                | "fivem/event-missing-arguments"
                | "fivem/event-wrong-side"
                | "fivem/export-argument-count"
                | "fivem/unknown-export"
                | "security/client-supplied-source"
                | "security/unvalidated-event-argument"
                | "security/sql-concatenation"
                | "qbox/unknown-locale-key"
                | "qbox/unused-locale-key"
                | "fivem/resource-not-found"
                // Reported by qbx-lua-ls, which has the LuaCATS types the linter does not.
                | "undefined-doc-name"
                | "missing-fields"
                | "assign-type-mismatch"
                | "param-type-mismatch"
                | "undeclared-field"
                | "inject-field"
                | "invisible"
                | "return-type-mismatch"
                | "missing-return"
                | "redundant-return-value"
                | "discard-returns"
                | "cast-type-mismatch"
                | "cast-local-type"
                | "no-unknown"
                | "impossible-comparison"
                | "need-check-nil"
                | "circle-doc-class"
                | "close-non-object"
        );
        assert!(exercised || covered_elsewhere, "rule {} is not exercised by the bad_resource fixture", rule.code);
    }
}

#[test]
fn server_cfg_decides_order_and_what_is_installed() {
    qbx_lua_analysis::startup::clear_cache();
    let actual = render(&fixtures().join("server/resources"), &Config::default());
    let expected = "[bridges]/bridge/server.lua:9:1 info fivem/resource-not-found\n\
                    [bridges]/bridge/server.lua:12:20 info fivem/unknown-export\n";
    assert_eq!(
        actual, expected,
        "core starts earlier, the ghost framework is optional, ghost_inventory is missing, core has no 'Missing' \
         export, and the escrowed vault may export anything"
    );
}

#[test]
fn good_resource_is_clean_even_with_optional_rules() {
    let actual = render(&fixtures().join("good_resource"), &config_with_everything_enabled());
    assert_eq!(actual, "", "the idiomatic fixture must not produce findings");
}

#[test]
fn nested_manifests_keep_globals_in_the_parent_resource() {
    let root = fixtures().join("nested");
    let library = root.join("lib");
    let mut locator = ResourceLocator::default();

    assert_eq!(find_manifest_dir(&library.join("client/client.lua")), Some(root.clone()));
    assert!(manifest_path(&library).is_none());
    assert!(locator.locate(&root, "lib").is_none());
    assert_eq!(locator.locate(&root, "nested"), Some(root.clone()));

    let reports = lint_paths(&[root.join("client/client.lua")], &Config::default());

    assert_eq!(reports.len(), 1);
    assert!(reports[0].diagnostics.is_empty(), "{:?}", reports[0].diagnostics);
}

#[test]
fn escrowed_resources_are_not_second_guessed() {
    let actual = render(&fixtures().join("escrowed_resource"), &config_with_everything_enabled());
    assert!(!actual.contains("escrowed.lua"), "encrypted files are never parsed: {actual}");
    assert!(!actual.contains("undefined-global"), "globals may be defined by the encrypted part: {actual}");
    assert!(!actual.contains("implicit-global"), "and declared at file scope there: {actual}");
    assert!(!actual.contains("unused-locale-key"), "keys may be used by the encrypted part: {actual}");
}

#[test]
fn fixes_are_applied_and_converge() {
    let root = fixtures().join("bad_resource");
    let mut config = Config::default();
    config.set_rule(qbx_lua_analysis::rules::MANIFEST_LUA54, Level::Warning);
    let reports = lint_paths(&[root.join("client/main.lua"), root.join("fxmanifest.lua")], &config);

    let client = reports.iter().find(|r| r.path.ends_with("client/main.lua")).unwrap();
    let (fixed, applied) = apply_fixes(&client.source, &client.diagnostics);
    assert_eq!(applied, 5);
    assert!(fixed.contains("\nCreateThread(function()\n    while true do\n        local ped = PlayerPedId()"));
    assert!(fixed.contains("local model = `adder`"));
    assert!(fixed.contains("        Wait(0)"));
    assert!(!fixed.contains("Citizen."));
    assert!(fixed.contains("for i = 3, 1, -1 do"));

    let manifest = reports.iter().find(|r| r.path.ends_with("fxmanifest.lua")).unwrap();
    let (fixed, applied) = apply_fixes(&manifest.source, &manifest.diagnostics);
    assert_eq!(applied, 1);
    assert!(fixed.starts_with("fx_version 'cerulean'\nlua54 'yes'\ngame 'gta5'"));
}
