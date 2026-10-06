use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let path = root.join(format!("check-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_qbx-lua-ls")).current_dir(&self.0).arg("--check").args(args).output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let root = std::fs::canonicalize(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let path = std::fs::canonicalize(&self.0).unwrap();
        assert_eq!(path.parent(), Some(root.as_path()));
        std::fs::remove_dir_all(path).unwrap();
    }
}

fn lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout).lines().map(str::to_string).collect()
}

#[test]
fn check_reports_what_the_editor_does_with_the_type_rules() {
    let fixture = Fixture::new();
    fixture.write("fxmanifest.lua", "fx_version 'cerulean'\ngame 'gta5'\nclient_script 'client.lua'\n");
    fixture.write(
        "client.lua",
        "\
---@param text string
local function show(text) print(text) end

---@return string?
local function find() end

show(5)
local found = find()
print(found .. '!')
",
    );
    let relaxed = fixture.run(&[]);
    assert_eq!(
        lines(&relaxed),
        ["client.lua:7:6: warning [param-type-mismatch] Cannot assign `integer` to parameter `text` of type `string`"],
        "a type rule that qbx-lint does not run is reported as the editor reports it"
    );
    assert_eq!(relaxed.status.code(), Some(1), "a warning fails the check");
    assert!(String::from_utf8_lossy(&relaxed.stderr).contains("2 files checked: 0 errors, 1 warning"));

    let strict = fixture.run(&["--strict"]);
    assert_eq!(
        lines(&strict),
        [
            "client.lua:7:6: warning [param-type-mismatch] Cannot assign `integer` to parameter `text` of type `string`",
            "client.lua:9:7: warning [need-check-nil] `found` may be nil: its type here is `string?`",
        ],
        "`--strict` checks as the `strict` setting does"
    );

    let allowed = fixture.run(&["--strict", "--max-warnings", "2"]);
    assert_eq!(allowed.status.code(), Some(0), "`--max-warnings` allows that many warnings");
    let silenced = fixture.run(&["--rule", "param-type-mismatch=off"]);
    assert!(lines(&silenced).is_empty() && silenced.status.success(), "`--rule` sets the level of a rule");

    fixture.write("qbxlint.toml", "[rules]\n\"param-type-mismatch\" = \"error\"\n");
    let configured = fixture.run(&[]);
    assert_eq!(
        lines(&configured),
        ["client.lua:7:6: error [param-type-mismatch] Cannot assign `integer` to parameter `text` of type `string`"],
        "qbxlint.toml applies as in the editor"
    );
    let unknown = fixture.run(&["--rule", "nothing=warning"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown rule 'nothing'"));
}
