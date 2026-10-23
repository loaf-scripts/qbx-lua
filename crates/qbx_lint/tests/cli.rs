use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let path = root.join(format!("cli-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn read(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.0.join(path)).unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_qbx-lint")).current_dir(&self.0).args(args).output().unwrap()
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn edit_commands_reject_non_utf8_without_changing_bytes() {
    let fixture = Fixture::new();
    let source = b"local text='caf\xe9'\nCitizen.Wait(0)\nprint(text)\n";
    fixture.write("main.lua", source);
    for args in [vec!["fmt", "main.lua"], vec!["fmt", "--check", "main.lua"], vec!["--fix", "main.lua"]] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("not valid UTF-8"), "{output:?}");
        assert_eq!(fixture.read("main.lua"), source, "{args:?}");
    }

    let output = fixture.run(&["--format", "json", "main.lua"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("fivem/citizen-prefix"));
    assert_eq!(fixture.read("main.lua"), source, "read-only analysis preserves the original encoding");
}

#[test]
fn edit_commands_continue_to_skip_binary_escrow_and_obfuscated_files() {
    let fixture = Fixture::new();
    let obfuscated = format!("Citizen.Wait(0)\n{}\n", "local a=function(z,z)return z end;".repeat(200));
    for source in [b"FXAP\xffCitizen.Wait(0)".as_slice(), b"\x1bLua\xff", b"binary\0\xff", obfuscated.as_bytes()] {
        fixture.write("encrypted.lua", source);
        for args in [vec!["fmt", "encrypted.lua"], vec!["--fix", "encrypted.lua"]] {
            assert_success(&fixture.run(&args));
            assert_eq!(fixture.read("encrypted.lua"), source);
        }
    }
}

#[test]
fn obfuscated_files_are_skipped_and_make_their_resource_opaque() {
    let fixture = Fixture::new();
    fixture.write("fxmanifest.lua", "fx_version 'cerulean'\ngame 'gta5'\nlua54 'yes'\nclient_script 'client/*.lua'\n");
    fixture.write(
        "client/protected.lua",
        format!("Citizen.Wait(0)\n{}\n", "Hidden=function(z,z)return z end;".repeat(200)),
    );
    fixture.write("client/open.lua", "Hidden(1)\n");
    fixture.write("client/data.lua", format!("local t={{{}}}\nCitizen.Wait(t[1])\n", "1, ".repeat(2000)));
    let stdout = String::from_utf8(fixture.run(&["--format", "json", "."]).stdout).unwrap();
    assert!(!stdout.contains("protected.lua") && !stdout.contains("undefined-global"), "{stdout}");
    assert!(stdout.contains("data.lua") && stdout.contains("fivem/citizen-prefix"), "{stdout}");
}

#[test]
fn valid_utf8_including_replacement_characters_remains_editable() {
    let fixture = Fixture::new();
    fixture.write("main.lua", "local text='caf�'\nCitizen.Wait(0)\nprint(text)\n");
    assert_success(&fixture.run(&["--fix", "main.lua"]));
    assert_success(&fixture.run(&["fmt", "main.lua"]));
    let source = String::from_utf8(fixture.read("main.lua")).unwrap();
    assert!(source.contains("caf�"));
    assert!(source.contains("Wait(0)"));
    assert!(!source.contains("Citizen."));
}

#[test]
fn hash_fixes_leave_standalone_calls_valid() {
    let fixture = Fixture::new();
    fixture.write("main.lua", "GetHashKey('adder')\nprint(GetHashKey('adder'))\n");
    assert_success(&fixture.run(&["--fix", "main.lua"]));
    assert_eq!(fixture.read("main.lua"), b"GetHashKey('adder')\nprint(`adder`)\n");
}

#[test]
fn automatic_fixes_never_write_unparseable_output() {
    let fixture = Fixture::new();
    let source = b"Citizen.Wait(0)\nlocal = 1\n";
    fixture.write("main.lua", source);
    let output = fixture.run(&["--fix", "main.lua"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("automatic fixes would produce invalid Lua"));
    assert_eq!(fixture.read("main.lua"), source);
}

#[test]
fn long_expression_chains_fail_cleanly_without_modifying_source() {
    let fixture = Fixture::new();
    let source = format!("local n = {}1\nprint(n)\n", "1 + ".repeat(63_999));
    fixture.write("main.lua", &source);
    for args in [vec!["--format", "compact", "main.lua"], vec!["fmt", "--check", "main.lua"]] {
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let message = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(message.contains("nesting"), "{message}");
        assert_eq!(fixture.read("main.lua"), source.as_bytes());
    }
}

#[test]
fn ignored_diagnostics_keep_the_files_in_the_analysis() {
    let fixture = Fixture::new();
    fixture
        .write("fxmanifest.lua", "fx_version 'cerulean'\ngame 'gta5'\nclient_scripts { 'vendor/*.lua', 'main.lua' }\n");
    fixture.write("vendor/lib.lua", "VendorApi = {}\nCitizen.Wait(0)\n");
    fixture.write("main.lua", "print(VendorApi)\n");
    for (config, vendor_reported, vendor_known) in
        [("", true, true), ("ignore_diagnostics = ['vendor/']", false, true), ("exclude = ['vendor/**']", false, false)]
    {
        fixture.write("qbxlint.toml", config);
        let stdout = String::from_utf8(fixture.run(&["--format", "json", "."]).stdout).unwrap();
        assert_eq!(stdout.contains("lib.lua"), vendor_reported, "{config}: {stdout}");
        assert_eq!(stdout.contains("citizen-prefix"), vendor_reported, "{config}: {stdout}");
        assert_eq!(!stdout.contains("undefined-global"), vendor_known, "{config}: {stdout}");
    }
    fixture.write("main.lua", "print(VendorApi, locale('used'))\n");
    fixture.write("locales/en.json", r#"{ "used": "a", "unused": "b" }"#);
    for (config, reported) in [("", true), ("ignore_diagnostics = ['locales/']", false)] {
        fixture.write("qbxlint.toml", config);
        let stdout = String::from_utf8(fixture.run(&["--format", "json", "."]).stdout).unwrap();
        assert_eq!(stdout.contains("unused-locale-key"), reported, "{config}: {stdout}");
    }
}

#[test]
fn configured_imports_define_globals_by_side_for_matching_resources() {
    let fixture = Fixture::new();
    let manifest = "fx_version 'cerulean'\ngame 'gta5'\n";
    fixture.write("resources/lib/fxmanifest.lua", format!("{manifest}files {{ 'shared/**.lua', 'client/*.lua' }}\n"));
    fixture.write("resources/lib/shared/deep/api.lua", "SharedApi = {}\n");
    fixture.write("resources/lib/client/api.lua", "function ClientApi() end\n");
    for resource in ["[lib]/shop", "other"] {
        let scripts = "client_script 'client.lua'\nserver_script 'server.lua'\n";
        fixture.write(&format!("resources/{resource}/fxmanifest.lua"), format!("{manifest}{scripts}"));
        fixture.write(&format!("resources/{resource}/client.lua"), "print(SharedApi, ClientApi)\n");
        fixture.write(&format!("resources/{resource}/server.lua"), "print(SharedApi, ClientApi)\n");
    }
    fixture.write(
        "qbxlint.toml",
        "[[overrides]]\nfiles = ['resources/[[]lib[]]/**']\n\
         [overrides.imports]\nshared = ['@lib/shared/**.lua']\nclient = ['@lib/client/*.lua']\n",
    );
    let output = fixture.run(&["--format", "json", "--no-fail", "resources"]);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut undefined: Vec<String> = Vec::new();
    for file in json["files"].as_array().unwrap() {
        for diagnostic in file["diagnostics"].as_array().unwrap().iter().filter(|d| d["code"] == "undefined-global") {
            let name = diagnostic["message"].as_str().unwrap().split('\'').nth(1).unwrap();
            undefined.push(format!("{} {name}", file["path"].as_str().unwrap().replace('\\', "/")));
        }
    }
    undefined.sort();
    assert_eq!(
        undefined,
        [
            "resources/[lib]/shop/server.lua ClientApi",
            "resources/other/client.lua ClientApi",
            "resources/other/client.lua SharedApi",
            "resources/other/server.lua ClientApi",
            "resources/other/server.lua SharedApi",
        ]
    );
}

#[test]
fn overrides_give_scripts_the_manifest_does_not_list_a_side() {
    let fixture = Fixture::new();
    fixture.write(
        "fxmanifest.lua",
        "fx_version 'cerulean'\ngame 'gta5'\nfiles { 'client/**.lua', 'server/**.lua' }\n\
         shared_script 'load.lua'\nclient_script 'client/listed.lua'\n",
    );
    fixture.write("load.lua", "print('loader')\n");
    fixture.write("client/api.lua", "function ClientApi() end\nprint(GetPlayerIdentifier(1, 0))\n");
    fixture.write("client/listed.lua", "print(PlayerPedId())\n");
    fixture.write("server/main.lua", "print(ClientApi)\n");
    let findings = |config: &str| {
        fixture.write("qbxlint.toml", config);
        let output = fixture.run(&["--format", "json", "--no-fail", "."]);
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let mut found: Vec<String> = Vec::new();
        for file in json["files"].as_array().unwrap() {
            for diagnostic in file["diagnostics"].as_array().unwrap() {
                let code = diagnostic["code"].as_str().unwrap();
                if matches!(code, "undefined-global" | "fivem/native-wrong-side") {
                    found.push(format!("{} {code}", file["path"].as_str().unwrap().replace('\\', "/")));
                }
            }
        }
        found.sort();
        found
    };
    assert!(findings("").is_empty(), "files of an unknown side see both sides");
    let sides = "[[overrides]]\nfiles = ['client/**']\nside = 'client'\n\
                 [[overrides]]\nfiles = ['server/**', 'client/listed.lua']\nside = 'server'\n";
    assert_eq!(
        findings(sides),
        ["client/api.lua fivem/native-wrong-side", "server/main.lua undefined-global"],
        "the manifest keeps client/listed.lua on the client"
    );
}

#[test]
fn relative_and_absolute_config_paths_apply_identical_exclusions_and_overrides() {
    let fixture = Fixture::new();
    fixture.write(
        "qbxlint.toml",
        "exclude = ['skip/**']\n[[overrides]]\nfiles = ['main.lua']\nrules = { 'undefined-global' = 'off' }\n",
    );
    fixture.write("main.lua", "print(CustomGlobal)\n");
    fixture.write("skip/broken.lua", "local =\n");
    let absolute = fixture.0.join("qbxlint.toml").to_string_lossy().into_owned();
    let expected = fixture.run(&["--config", &absolute, "--format", "json", "."]);
    assert_success(&expected);
    let json: serde_json::Value = serde_json::from_slice(&expected.stdout).unwrap();
    assert_eq!(json["files"].as_array().unwrap().len(), 1);
    assert_eq!(json["files"][0]["diagnostics"].as_array().unwrap().len(), 0);
    for relative in ["qbxlint.toml", "./qbxlint.toml"] {
        let actual = fixture.run(&["--config", relative, "--format", "json", "."]);
        assert_success(&actual);
        assert_eq!(actual.stdout, expected.stdout);
    }

    fixture.write("skip/broken.lua", "Citizen.Wait(0)\n");
    assert_success(&fixture.run(&["--config", "qbxlint.toml", "--fix", "."]));
    assert_eq!(fixture.read("skip/broken.lua"), b"Citizen.Wait(0)\n");
    fixture.write("skip/broken.lua", "local n=1\nprint(n)\n");
    assert_success(&fixture.run(&["fmt", "--config", "qbxlint.toml", "skip/broken.lua"]));
    assert_eq!(fixture.read("skip/broken.lua"), b"local n=1\nprint(n)\n");
}

#[test]
fn configured_imports_respect_excluded_files_and_directories() {
    let fixture = Fixture::new();
    fixture.write("resources/lib/fxmanifest.lua", "fx_version 'cerulean'\ngame 'gta5'\nfiles { 'api.lua' }\n");
    fixture.write("resources/lib/api.lua", "ExcludedApi = {}\n");
    fixture.write("resources/shop/fxmanifest.lua", "fx_version 'cerulean'\ngame 'gta5'\nclient_script 'client.lua'\n");
    fixture.write("resources/shop/client.lua", "print(ExcludedApi)\n");
    for excluded in ["resources/lib/api.lua", "resources/lib"] {
        for pattern in ["@lib/*.lua", "@lib/api.lua"] {
            fixture.write("qbxlint.toml", format!("exclude = ['{excluded}']\n[imports]\nclient = ['{pattern}']\n"));
            let output = fixture.run(&["--format", "json", "--no-fail", "resources"]);
            assert_success(&output);
            let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let reported = json["files"].as_array().unwrap().iter().any(|f| {
                f["path"].as_str().unwrap().replace('\\', "/").ends_with("shop/client.lua")
                    && f["diagnostics"].as_array().unwrap().iter().any(|d| d["code"] == "undefined-global")
            });
            assert!(reported, "{pattern} leaked a global excluded by {excluded}: {json}");
        }
    }
}

#[test]
fn configured_ox_lib_imports_supply_extensions_and_cache_checks_by_side() {
    let fixture = Fixture::new();
    fixture.write(
        "shop/fxmanifest.lua",
        "fx_version 'cerulean'\ngame 'gta5'\nclient_script 'client.lua'\nserver_script 'server.lua'\n",
    );
    fixture.write("shop/client.lua", "print(table.contains({}, 1), PlayerPedId())\n");
    fixture.write("shop/server.lua", "print(table.contains({}, 1))\n");
    for side in ["shared", "client", "server"] {
        fixture.write(
            "qbxlint.toml",
            format!("[rules]\n'qbox/prefer-cache' = 'warning'\n[imports]\n{side} = ['@ox_lib/init.lua']\n"),
        );
        let output = fixture.run(&["--format", "json", "--no-fail", "shop"]);
        assert_success(&output);
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        for file in json["files"].as_array().unwrap() {
            let path = file["path"].as_str().unwrap().replace('\\', "/");
            let client = path.ends_with("/client.lua");
            if !client && !path.ends_with("/server.lua") {
                continue;
            }
            let codes: Vec<&str> =
                file["diagnostics"].as_array().unwrap().iter().map(|d| d["code"].as_str().unwrap()).collect();
            let available = side == "shared" || side == if client { "client" } else { "server" };
            assert_eq!(codes.contains(&"undefined-field"), !available, "{side}: {file}");
            assert_eq!(codes.contains(&"qbox/prefer-cache"), client && available, "{side}: {file}");
        }
    }
}

#[test]
fn lua_ls_settings_apply_when_no_qbxlint_toml_exists() {
    let fixture = Fixture::new();
    fixture.write(
        ".luarc.json",
        "{\n\t\"runtime.version\": \"Lua 5.4\",\n\t\"diagnostics.globals\": [\"Config\"],\n\t\"workspace.ignoreDir\": [\"\\\\[standalone\\\\]\"],\n\t\"diagnostics.disable\": [\"lowercase-global\", \"duplicate-doc-class\"]\n}\n",
    );
    fixture.write("main.lua", "helper = function() return Config end\nprint(helper, Missing)\n");
    fixture.write("[standalone]/broken.lua", "local =\n");
    for args in [vec!["--format", "json", "."], vec!["--config", ".luarc.json", "--format", "json", "."]] {
        let output = fixture.run(&args);
        assert_success(&output);
        let noted = String::from_utf8_lossy(&output.stderr).contains("falling back to the supported settings");
        assert_eq!(noted, args[0] != "--config", "{args:?}: {output:?}");
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{args:?}: {json}");
        let codes: Vec<&str> =
            files[0]["diagnostics"].as_array().unwrap().iter().map(|d| d["code"].as_str().unwrap()).collect();
        assert_eq!(codes, ["undefined-global"], "{args:?}: {json}");
    }
}

#[test]
fn nested_qbxlint_toml_files_combine_with_those_above_wherever_the_run_starts() {
    let fixture = Fixture::new();
    fixture.write("server/qbxlint.toml", "globals = ['ServerGlobal']\n[rules]\n'fivem/citizen-prefix' = 'off'\n");
    fixture.write("server/resource_a/qbxlint.toml", "globals = ['OnlyA']\n");
    fixture.write("server/resource_b/qbxlint.toml", "globals = ['OnlyB']\n[rules]\n'fivem/citizen-prefix' = 'error'\n");
    for resource in ["resource_a", "resource_b"] {
        let manifest = "fx_version 'cerulean'\ngame 'gta5'\nclient_script 'client.lua'\n";
        fixture.write(&format!("server/{resource}/fxmanifest.lua"), manifest);
        fixture.write(&format!("server/{resource}/client.lua"), "print(ServerGlobal, OnlyA, OnlyB)\nCitizen.Wait(0)\n");
    }
    let findings = |args: &[&str]| {
        let output = fixture.run(&[&["--format", "json", "--no-fail"], args].concat());
        assert_success(&output);
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let mut found: Vec<String> = Vec::new();
        for file in json["files"].as_array().unwrap() {
            for diagnostic in file["diagnostics"].as_array().unwrap() {
                let path = file["path"].as_str().unwrap().replace('\\', "/");
                found.push(format!("{path} {} {}", diagnostic["code"], diagnostic["severity"]));
            }
        }
        found.sort();
        found
    };
    let expected = findings(&["server"]);
    assert_eq!(
        expected,
        [
            "server/resource_a/client.lua \"undefined-global\" \"warning\"",
            "server/resource_b/client.lua \"fivem/citizen-prefix\" \"error\"",
            "server/resource_b/client.lua \"undefined-global\" \"warning\"",
        ]
    );
    assert_eq!(findings(&["server/resource_a", "server/resource_b"]), expected);
    assert_eq!(findings(&["server/resource_b/client.lua", "server/resource_a/client.lua"]), expected);
}

#[test]
fn a_nested_qbxlint_toml_that_cannot_be_read_fails_the_run() {
    let fixture = Fixture::new();
    fixture.write("server/qbxlint.toml", "globals = ['ServerGlobal']\n");
    fixture.write("server/res/qbxlint.toml", "[rules]\n'nope' = 'off'\n");
    fixture.write("server/res/main.lua", "print(ServerGlobal)\n");
    for args in [vec!["server"], vec!["fmt", "server"]] {
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr).replace('\\', "/");
        assert!(stderr.contains("server/res/qbxlint.toml") && stderr.contains("unknown rule 'nope'"), "{stderr}");
    }
}

#[test]
fn fmt_formats_each_file_with_the_options_of_its_folders() {
    let fixture = Fixture::new();
    fixture.write("qbxlint.toml", "[format]\nindent_width = 2\nquote_style = 'single'\n");
    fixture.write("res/qbxlint.toml", "[format]\nquote_style = 'double'\n");
    let source = "if a then\nprint(\"x\", 'y')\nend\n";
    fixture.write("main.lua", source);
    fixture.write("res/main.lua", source);
    assert_success(&fixture.run(&["fmt", "."]));
    assert_eq!(String::from_utf8(fixture.read("main.lua")).unwrap(), "if a then\n  print('x', 'y')\nend\n");
    assert_eq!(String::from_utf8(fixture.read("res/main.lua")).unwrap(), "if a then\n  print(\"x\", \"y\")\nend\n");
}
