use qbx_lua_analysis::crossref::CrossRefs;
use qbx_lua_analysis::locale::LocaleFile;
use qbx_lua_analysis::scope::resolve;
use qbx_lua_analysis::summary::summarize;
use qbx_lua_analysis::{check_file, FileConfig, FileInput, Level, Side};
use qbx_lua_syntax::parse;

fn codes_with(source: &str, config: &FileConfig) -> Vec<&'static str> {
    codes_in_project(source, config, None, &[], None)
}

/// Lints `source` as a file on `side`, with `others` (side, source) providing handlers and exports
/// as the resource `other`.
fn codes_in_project(
    source: &str,
    config: &FileConfig,
    side: Option<Side>,
    others: &[(Option<Side>, &str)],
    locale: Option<&str>,
) -> Vec<&'static str> {
    let chunk = parse(source);
    let resolution = resolve(&chunk);
    let summary = summarize(source, &chunk, &resolution);
    let mut crossrefs = CrossRefs::default();
    crossrefs.collect(&chunk, side, None);
    for (other_side, other) in others {
        crossrefs.collect(&parse(other), *other_side, Some("other"));
    }
    let locale = locale.map(|json| LocaleFile::parse("locales/en.json".into(), json.to_string()));
    let input = FileInput {
        source,
        chunk: &chunk,
        resolution: &resolution,
        summary: &summary,
        config,
        side,
        resource: None,
        crossrefs: Some(&crossrefs),
        locale: locale.as_ref(),
        relative_path: "",
    };
    check_file(&input).into_iter().map(|d| d.code).collect()
}

fn project(source: &str, side: Side, others: &[(Option<Side>, &str)]) -> Vec<&'static str> {
    codes_in_project(source, &FileConfig::default(), Some(side), others, None)
}

/// Lints `source` as a `side` script of the resource `own`, with `others` (resource, side, source)
/// providing the event handlers of named resources.
fn resource_project(source: &str, side: Side, own: &str, others: &[(&str, Option<Side>, &str)]) -> Vec<&'static str> {
    let chunk = parse(source);
    let resolution = resolve(&chunk);
    let summary = summarize(source, &chunk, &resolution);
    let mut crossrefs = CrossRefs::default();
    crossrefs.collect(&chunk, Some(side), Some(own));
    for (resource, other_side, other) in others {
        crossrefs.collect(&parse(other), *other_side, Some(resource));
    }
    let env = qbx_lua_analysis::project::ResourceEnv::default();
    let manifest = qbx_lua_analysis::manifest::Manifest::default();
    let resource = qbx_lua_analysis::ResourceInput {
        name: own,
        env: &env,
        manifest: &manifest,
        started_before: None,
        installed: None,
    };
    let config = FileConfig::default();
    let input = FileInput {
        source,
        chunk: &chunk,
        resolution: &resolution,
        summary: &summary,
        config: &config,
        side: Some(side),
        resource: Some(resource),
        crossrefs: Some(&crossrefs),
        locale: None,
        relative_path: "",
    };
    check_file(&input).into_iter().map(|d| d.code).collect()
}

#[test]
fn events_are_checked_against_their_handlers() {
    let server = (Some(Side::Server), "RegisterNetEvent('shop:buy', function(item, amount) print(item, amount) end)");
    assert_eq!(project("TriggerServerEvent('shop:buy', 'water', 2)", Side::Client, &[server]), Vec::<&str>::new());
    assert_eq!(
        project("TriggerServerEvent('shop:buy', 'water', 2, 3)", Side::Client, &[server]),
        ["fivem/event-argument-count"]
    );
    assert_eq!(
        project("TriggerServerEvent('shop:buy', 'water')", Side::Client, &[server]),
        ["fivem/event-missing-arguments"]
    );
    assert_eq!(
        project("TriggerServerEvent('shop:buy', table.unpack({ 1 }))", Side::Client, &[server]),
        Vec::<&str>::new()
    );
    assert_eq!(project("TriggerServerEvent('unknown:event', 1)", Side::Client, &[server]), Vec::<&str>::new());

    let variadic = (Some(Side::Server), "RegisterNetEvent('log', function(...) print(...) end)");
    assert_eq!(project("TriggerServerEvent('log', 1, 2, 3)", Side::Client, &[variadic]), Vec::<&str>::new());

    let client = (Some(Side::Client), "RegisterNetEvent('hud:update', function(value) print(value) end)");
    assert_eq!(project("TriggerServerEvent('hud:update', 1)", Side::Client, &[client]), ["fivem/event-wrong-side"]);
    assert_eq!(project("TriggerClientEvent('hud:update', -1, 1)", Side::Server, &[client]), Vec::<&str>::new());
    assert_eq!(
        project("TriggerClientEvent('hud:update', -1, 1, 2)", Side::Server, &[client]),
        ["fivem/event-argument-count"]
    );
}

#[test]
fn event_checks_distinguish_own_handlers_from_other_resources() {
    let none = Vec::<&str>::new();
    let own_handler = ("hud", Some(Side::Client), "RegisterNetEvent('hud:update', function(value) print(value) end)");
    let other_handler = ("other", Some(Side::Client), "AddEventHandler('hud:update', function() end)");
    let source = "TriggerClientEvent('hud:update', -1, 1, 2)";
    assert_eq!(resource_project(source, Side::Server, "hud", &[own_handler]), ["fivem/event-argument-count"]);
    assert_eq!(resource_project(source, Side::Server, "hud", &[other_handler]), none, "a consumer may ignore payload");
    assert_eq!(
        resource_project(source, Side::Server, "hud", &[own_handler, other_handler]),
        ["fivem/event-argument-count"]
    );
    let wide = ("other", Some(Side::Client), "AddEventHandler('hud:update', function(value, extra, more) end)");
    assert_eq!(
        resource_project("TriggerClientEvent('hud:update', -1, 1)", Side::Server, "hud", &[wide]),
        ["fivem/event-missing-arguments"],
        "too few arguments for another resource's handler is still worth knowing"
    );

    let server_only = ("medical", Some(Side::Server), "RegisterNetEvent('base:playerDied', function(killer) end)");
    assert_eq!(
        resource_project("TriggerEvent('base:playerDied', 1)", Side::Client, "base", &[server_only]),
        none,
        "a local hook for other resources"
    );
    assert_eq!(
        resource_project(
            "TriggerEvent('base:playerDied', 1)",
            Side::Client,
            "base",
            &[("base", Some(Side::Server), "RegisterNetEvent('base:playerDied', function(killer) end)")]
        ),
        ["fivem/event-wrong-side"],
        "the resource's own handler is on the other side"
    );
    assert_eq!(
        resource_project("TriggerClientEvent('base:playerDied', -1, 1)", Side::Server, "other", &[server_only]),
        ["fivem/event-wrong-side"],
        "sending a server-handled event to clients is wrong whoever handles it"
    );
    let mirrored = "TriggerEvent('base:playerDied', 1)\nTriggerServerEvent('base:playerDied', 1)";
    let own_server = ("base", Some(Side::Server), "RegisterNetEvent('base:playerDied', function(killer) end)");
    assert_eq!(
        resource_project(mirrored, Side::Client, "base", &[own_server]),
        none,
        "a local copy of a mirrored event"
    );
    let chat_hook = ("core", Some(Side::Server), "AddEventHandler('chatMessage', function(source, name, message) end)");
    assert_eq!(
        resource_project(
            "TriggerClientEvent('chatMessage', -1, 'sys', {0, 0, 255}, 'hi')",
            Side::Server,
            "admin",
            &[chat_hook]
        ),
        none,
        "chat is a system resource"
    );
}

#[test]
fn data_files_are_found_by_streamed_asset_name() {
    use qbx_lua_analysis::{check_manifest, ManifestInput};
    let source = "fx_version 'cerulean'\ngame 'gta5'\nfiles { 'client.lua', 'missing.png' }\ndata_file 'DLC_ITYP_REQUEST' 'props.ytyp'\ndata_file 'DLC_ITYP_REQUEST' 'absent.ytyp'\ndata_file 'HANDLING_FILE' 'data/handling.meta'\n";
    let chunk = parse(source);
    let manifest = qbx_lua_analysis::manifest::Manifest::from_chunk(&chunk);
    let files = ["client.lua".to_string(), "stream/[props]/props.ytyp".to_string(), "data/handling.meta".to_string()];
    let config = FileConfig::default();
    let input = ManifestInput {
        source,
        chunk: &chunk,
        manifest: &manifest,
        config: &config,
        resource_files: &files,
        has_lua_scripts: false,
    };
    let missing: Vec<String> = check_manifest(&input)
        .into_iter()
        .filter(|d| d.code == "manifest/missing-file")
        .map(|d| source[d.span.start as usize..d.span.end as usize].to_string())
        .collect();
    assert_eq!(
        missing,
        ["'missing.png'", "'absent.ytyp'"],
        "streamed data files are matched by name, plain files by path"
    );
}

#[test]
fn modules_loaded_at_runtime_provide_their_globals() {
    fn codes(source: &str) -> Vec<&'static str> {
        let chunk = parse(source);
        let resolution = resolve(&chunk);
        let summary = summarize(source, &chunk, &resolution);
        let mut env = qbx_lua_analysis::project::ResourceEnv::default();
        env.add_summary(&summary, Some(Side::Client));
        let manifest = qbx_lua_analysis::manifest::Manifest::default();
        let resource = qbx_lua_analysis::ResourceInput {
            name: "prison",
            env: &env,
            manifest: &manifest,
            started_before: None,
            installed: None,
        };
        let mut config = FileConfig::default();
        config.globals.push("lib".into());
        let input = FileInput {
            source,
            chunk: &chunk,
            resolution: &resolution,
            summary: &summary,
            config: &config,
            side: Some(Side::Client),
            resource: Some(resource),
            crossrefs: None,
            locale: None,
            relative_path: "client.lua",
        };
        check_file(&input).into_iter().map(|d| d.code).collect()
    }
    assert_eq!(codes("qbx.playAudio({ audioName = 'cell_door' })"), ["fivem/import-not-declared"]);
    assert_eq!(
        codes("lib.load('@qbx_core.modules.lib')\nqbx.playAudio({ audioName = 'cell_door' })"),
        Vec::<&str>::new()
    );
    assert_eq!(
        codes("require '@qbx_core/modules/lib.lua'\nqbx.playAudio({ audioName = 'cell_door' })"),
        Vec::<&str>::new()
    );
    assert_eq!(
        qbx_lua_analysis::summary::module_import_pattern("@ox_lib.imports.callback.client").as_deref(),
        Some("@ox_lib/imports/callback/client.lua")
    );
    assert_eq!(qbx_lua_analysis::summary::module_import_pattern("modules.lib"), None);
}

#[test]
fn side_checks_follow_is_duplicity_version() {
    let client = |source: &str| project(source, Side::Client, &[]);
    let wrong = ["fivem/native-wrong-side"];
    let none = Vec::<&str>::new();

    assert_eq!(client("TriggerClientEvent('a', -1)"), wrong);
    assert_eq!(client("if IsDuplicityVersion() then\n    TriggerClientEvent('a', -1)\nend"), none);
    assert_eq!(
        client("if IsDuplicityVersion() then\n    print(1)\nelse\n    TriggerClientEvent('a', -1)\nend"),
        wrong,
        "the else branch is the client"
    );
    assert_eq!(client("if IsDuplicityVersion() then\n    print(PlayerPedId())\nend"), wrong, "server-only branch");
    assert_eq!(client("if not IsDuplicityVersion() then return end\nTriggerClientEvent('a', -1)"), none);
    assert_eq!(client("local isServer = IsDuplicityVersion()\nif isServer then TriggerClientEvent('a', -1) end"), none);
    assert_eq!(
        client("local isServer = IsDuplicityVersion()\nif not isServer then TriggerClientEvent('a', -1) end"),
        wrong
    );

    let with_lib = {
        let mut config = FileConfig::default();
        config.globals.push("lib".into());
        config
    };
    let source =
        "if lib.context == 'server' then\n    TriggerClientEvent('a', -1)\nelse\n    print(PlayerPedId())\nend";
    assert_eq!(codes_in_project(source, &with_lib, None, &[], None), none);

    let handler = (None, "if IsDuplicityVersion() then\n    RegisterNetEvent('sync:push', function() end)\nend");
    assert_eq!(project("TriggerClientEvent('sync:push', -1)", Side::Server, &[handler]), ["fivem/event-wrong-side"]);
}

#[test]
fn exports_are_checked_against_their_definition() {
    let other = (Some(Side::Server), "local function getPlayer(id) return id end\nexports('GetPlayer', getPlayer)");
    assert_eq!(project("print(exports.other:GetPlayer(1))", Side::Server, &[other]), Vec::<&str>::new());
    assert_eq!(
        project("print(exports.other:GetPlayer(1, 2))", Side::Server, &[other]),
        ["fivem/export-argument-count"]
    );
    assert_eq!(project("print(exports['other']:Missing())", Side::Server, &[other]), ["fivem/unknown-export"]);
    assert_eq!(project("print(exports.notIndexed:Anything(1, 2, 3))", Side::Server, &[other]), Vec::<&str>::new());
}

#[test]
fn server_handlers_must_not_trust_the_client() {
    let trusting = "RegisterNetEvent('bank:deposit', function(src, amount)\n    local player = exports.qbx_core:GetPlayer(src)\n    player.Functions.AddMoney('bank', amount)\nend)";
    assert_eq!(
        project(trusting, Side::Server, &[]),
        ["security/client-supplied-source", "security/unvalidated-event-argument"]
    );

    let checked = "RegisterNetEvent('bank:deposit', function(amount)\n    local player = exports.qbx_core:GetPlayer(source)\n    if type(amount) ~= 'number' or amount <= 0 then return end\n    player.Functions.AddMoney('bank', amount)\nend)";
    assert_eq!(project(checked, Side::Server, &[]), Vec::<&str>::new());

    let split = "RegisterNetEvent('run')\nAddEventHandler('run', function(code) load(code)() end)";
    assert_eq!(project(split, Side::Server, &[]), ["security/unvalidated-event-argument"]);

    assert_eq!(
        project(trusting, Side::Client, &[]),
        Vec::<&str>::new(),
        "client handlers receive data from the server"
    );
}

#[test]
fn sql_must_use_placeholders() {
    let config = {
        let mut config = FileConfig::default();
        config.globals.push("MySQL".into());
        config
    };
    let lint = |source: &str| codes_in_project(source, &config, Some(Side::Server), &[], None);
    assert_eq!(
        lint("local id = 1\nMySQL.query('SELECT * FROM players WHERE id = ' .. id)"),
        ["security/sql-concatenation"]
    );
    assert_eq!(
        lint("local id = 1\nMySQL.query.await(('SELECT * FROM players WHERE id = %s'):format(id))"),
        ["security/sql-concatenation"]
    );
    assert_eq!(lint("local id = 1\nMySQL.query('SELECT * FROM players WHERE id = ?', { id })"), Vec::<&str>::new());
    assert_eq!(lint("MySQL.query('SELECT * FROM ' .. 'players')"), Vec::<&str>::new());
    assert_eq!(
        lint("local where, values = 'id = ?', { 1 }\nMySQL.query('SELECT * FROM players WHERE ' .. where, values)"),
        Vec::<&str>::new()
    );
    assert_eq!(
        lint("local name = 'players'\nMySQL.query(('SHOW COLUMNS FROM `%s`'):format(name))"),
        Vec::<&str>::new()
    );
}

#[test]
fn locale_keys_must_exist() {
    let config = {
        let mut config = FileConfig::default();
        config.globals.push("locale".into());
        config
    };
    let json = r#"{ "error": { "not_online": "Offline" }, "ok": "Fine" }"#;
    let lint = |source: &str| codes_in_project(source, &config, Some(Side::Client), &[], Some(json));
    assert_eq!(lint("print(locale('error.not_online'), locale('ok'))"), Vec::<&str>::new());
    assert_eq!(lint("print(locale('error.not_onlin'))"), ["qbox/unknown-locale-key"]);
    assert_eq!(lint("local k = 'ok'\nprint(locale(k), locale('error.' .. k))"), Vec::<&str>::new());
}

fn codes(source: &str) -> Vec<&'static str> {
    codes_with(source, &FileConfig::default())
}

#[test]
fn clean_snippets_stay_clean() {
    for source in [
        "local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end\nprint(fib(10))",
        "local t = {}\nfunction t:method() return self end\nprint(t)",
        "for _, v in ipairs({ 1, 2 }) do print(v) end",
        "print(glm.normalize(vector3(1, 2, 3)), glm.pi, glm.quatLookAt(glm.forward(), glm.up()))",
        "local ok, err = pcall(error, 'x')\nprint(ok, err)",
        "local a <close> = setmetatable({}, { __close = function() end })",
        "while true do\n    local done = coroutine.yield()\n    if done then break end\nend",
        "for i = 1, 3 do\n    if i == 2 then goto continue end\n    print(i)\n    ::continue::\nend",
        "local x = 1\nlocal function get() return x end\nx = 2\nprint(get())",
        "print(N_0xdeadbeef(1), vector3(1, 2, 3).x, json.encode({}), string.strtrim(' a '))",
        // What CfxLua registers beside the vector constructors.
        "local a, q = vec3(1, 2, 3), qua(1, 0, 0, 0)\nprint(dot(a, a), cross(a, a), inv(q), slerp(q, q, 0.5), vec1(1), ivec3(1, 2, 3), bvec2(true, false))",
        "print(mat(1), mat2(1), mat2x3(1), mat3x3(1), mat4(1), mat4x4(1), scrub(1, {}), utf8.strlenutf8('a'), utf8.strcmputf8i('a', 'A'))",
        "for k, v in each({}) do print(k, v) end",
        "print(json.isarray({}), json.array(), json.object(), json.isobject({}), json.getoption('indent'))\njson.setoption('indent', true)",
        "print(msgpack.null, msgpack.new(), msgpack.next('', 1), msgpack.getoption('float'), msgpack.gettype('x'))\nmsgpack.setoption('float', true)",
        "print(Citizen.InvokeNative2(0x1), Citizen.ResultAsObject2(msgpack.unpack))",
        "Global = Global or {}\nfunction Global.helper() end",
        "local function mayWait() end\nwhile true do\n    mayWait()\nend",
        "repeat\n    local line = io.read()\nuntil line == nil",
    ] {
        assert_eq!(codes(source), Vec::<&str>::new(), "{source}");
    }
}

#[test]
fn syntax_errors_are_not_suppressible() {
    assert_eq!(codes("-- qbx-lint: disable\nlocal = 1"), ["syntax-error"]);
}

#[test]
fn unused_and_shadowing() {
    assert_eq!(codes("for k, v in pairs({}) do print(v) end"), ["unused-loop-variable"]);
    assert_eq!(codes("local a = 1\nlocal a = 2\nprint(a)"), ["unused-local", "redefined-local"]);
    assert_eq!(codes("do ::top:: end"), ["unused-label"]);
    assert_eq!(codes("local a = 1\nprint(a < a)"), ["self-comparison"]);

    let mut config = FileConfig::default();
    config.set("shadowed-local", Level::Warning);
    assert_eq!(codes_with("local a = 1\ndo local a = 2 print(a) end\nprint(a)", &config), ["shadowed-local"]);
}

#[test]
fn ignore_prefix_silences_unused() {
    assert_eq!(codes("local _ignored = 1\nlocal function cb(_a, _b) end\ncb()"), Vec::<&str>::new());
}

#[test]
fn runtime_names_are_protected() {
    assert_eq!(codes("function GetEntityCoords() end"), ["builtin-overwrite"]);
    assert_eq!(codes("print = nil"), ["builtin-overwrite"]);
}

#[test]
fn shadowed_runtime_names_do_not_trigger_fivem_rules() {
    let source = "local Citizen = { Wait = function() end }\nlocal function GetHashKey(s) return s end\nCitizen.Wait(0)\nprint(GetHashKey('adder'))";
    assert_eq!(codes(source), Vec::<&str>::new());
}

fn hash_fixes(source: &str) -> (String, usize) {
    let chunk = parse(source);
    assert!(chunk.errors.is_empty(), "invalid test input: {source}");
    let resolution = resolve(&chunk);
    let summary = summarize(source, &chunk, &resolution);
    let config = FileConfig::default();
    let diagnostics = check_file(&FileInput {
        source,
        chunk: &chunk,
        resolution: &resolution,
        summary: &summary,
        config: &config,
        side: None,
        resource: None,
        crossrefs: None,
        locale: None,
        relative_path: "",
    });
    let hashes: Vec<_> = diagnostics.into_iter().filter(|d| d.code == "fivem/hash-literal").collect();
    qbx_lua_analysis::apply_fixes(source, &hashes)
}

#[test]
fn hash_literals_are_not_suggested_for_statements_or_prefix_expressions() {
    for source in [
        "GetHashKey('adder')",
        "GetHashKey('adder')()",
        "GetHashKey('adder'):method()",
        "print(GetHashKey('adder').field)",
        "print(GetHashKey('adder')[1])",
        "GetHashKey('adder').field = 1",
    ] {
        assert_eq!(hash_fixes(source), (source.to_string(), 0), "{source}");
    }
}

#[test]
fn hash_literals_are_fixed_in_values_and_parenthesized_prefix_expressions() {
    for (source, expected) in [
        ("print(GetHashKey('adder'))", "print(`adder`)"),
        ("local hash = GetHashKey('adder')", "local hash = `adder`"),
        ("return GetHashKey('adder')", "return `adder`"),
        ("print({ [GetHashKey('adder')] = true })", "print({ [`adder`] = true })"),
        ("(GetHashKey('adder'))()", "(`adder`)()"),
        ("GetHashKey('adder')(GetHashKey('other'))", "GetHashKey('adder')(`other`)"),
    ] {
        let (fixed, applied) = hash_fixes(source);
        assert_eq!(applied, 1, "{source}");
        assert_eq!(fixed, expected);
        assert!(parse(&fixed).errors.is_empty(), "fix must remain valid Lua: {fixed}");
    }
}

#[test]
fn infinite_loop_detection_handles_nested_loops_and_gotos() {
    assert_eq!(codes("while true do\n    for _ = 1, 2 do break end\nend"), ["fivem/loop-never-yields"]);
    assert_eq!(codes("while true do\n    goto continue\n    ::continue::\nend"), ["fivem/loop-never-yields"]);
    assert_eq!(codes("repeat local x = GetGameTimer() until false"), ["fivem/loop-never-yields", "unused-local"]);
    assert_eq!(codes("while true do\n    if GetGameTimer() > 5 then goto done end\nend\n::done::"), Vec::<&str>::new());
}

#[test]
fn source_tracking_respects_handlers_and_locals() {
    let nested_handler = "CreateThread(function()\n    AddEventHandler('x', function() print(source) end)\nend)";
    assert_eq!(codes(nested_handler), Vec::<&str>::new());
    let shadowed = "AddEventHandler('x', function(source)\n    Wait(0)\n    print(source)\nend)";
    assert_eq!(codes(shadowed), Vec::<&str>::new());
    let awaited = "AddEventHandler('x', function()\n    local r = MySQL.query.await('q')\n    print(source, r)\nend)";
    assert_eq!(
        codes_with(awaited, &{
            let mut config = FileConfig::default();
            config.globals.push("MySQL".into());
            config
        }),
        ["fivem/source-after-yield"]
    );
}

/// An escrow-encrypted script, as it starts on disk.
const OPAQUE: &str = "FXAP";

/// The `missing-parameter` messages for `source`, a `side` script of a resource whose other scripts
/// are `others` (side, source).
fn missing_parameters(source: &str, side: Option<Side>, others: &[(Option<Side>, &str)]) -> Vec<String> {
    call_messages("missing-parameter", source, side, others)
}

/// The `redundant-parameter` messages for `source`, as `missing_parameters` finds them.
fn redundant_parameters(source: &str, side: Option<Side>, others: &[(Option<Side>, &str)]) -> Vec<String> {
    call_messages("redundant-parameter", source, side, others)
}

/// The messages of `code` for `source`, a `side` script of a resource whose other scripts are
/// `others` (side, source).
fn call_messages(code: &str, source: &str, side: Option<Side>, others: &[(Option<Side>, &str)]) -> Vec<String> {
    let chunk = parse(source);
    let resolution = resolve(&chunk);
    let summary = summarize(source, &chunk, &resolution);
    let mut env = qbx_lua_analysis::project::ResourceEnv::default();
    env.add_summary(&summary, side);
    for (other_side, other) in others {
        if qbx_lua_analysis::project::is_not_source(other.as_bytes()) {
            env.opaque = true;
            continue;
        }
        let chunk = parse(other);
        env.add_summary(&summarize(other, &chunk, &resolve(&chunk)), *other_side);
    }
    let manifest = qbx_lua_analysis::manifest::Manifest::default();
    let resource = qbx_lua_analysis::ResourceInput {
        name: "res",
        env: &env,
        manifest: &manifest,
        started_before: None,
        installed: None,
    };
    let config = FileConfig::default();
    let input = FileInput {
        source,
        chunk: &chunk,
        resolution: &resolution,
        summary: &summary,
        config: &config,
        side,
        resource: Some(resource),
        crossrefs: None,
        locale: None,
        relative_path: "",
    };
    check_file(&input).into_iter().filter(|d| d.code == code).map(|d| d.message).collect()
}

#[test]
fn documented_parameters_need_an_argument() {
    let missing = |source: &str| missing_parameters(source, None, &[]);
    let bezier = "---@param handler fun(progress: number)\n---@param time number\n---@param transition? \"ease\"|\"linear\" | { p1: vector2, p2: vector2 }\nfunction BezierAnimate(handler, time, transition) end\n";
    assert_eq!(
        missing(&format!("{bezier}BezierAnimate(function(progress) end)")),
        ["'BezierAnimate' is called with 1 argument, but needs 2; 'time' (number) will be nil"]
    );
    assert!(missing(&format!("{bezier}BezierAnimate(function(progress) end, 500)")).is_empty());

    let opt_first = "---@param a? string\n---@param b number\nlocal function f(a, b) end\n";
    assert_eq!(
        missing(&format!("{opt_first}f()")),
        ["'f' is called with 0 arguments, but needs 2; 'b' (number) will be nil"],
        "an optional parameter before a required one still takes a position"
    );
    assert!(missing(&format!("{opt_first}f(nil, 1)")).is_empty());

    // As in LuaLS, the docs reach the function past a directive; only a blank line detaches them.
    let directive =
        "---@param level string\n-- qbx-lint: disable-next-line lowercase-global\nfunction infoprint(level) end\n";
    assert_eq!(
        missing(&format!("{directive}infoprint()")),
        ["'infoprint' is called with 0 arguments, but needs 1; 'level' (string) will be nil"]
    );
    assert!(missing("---@param level string\n-- note\n\nlocal function f(level) end\nf()").is_empty());

    // The cases below behave as in LuaLS: nothing that may be nil or is left undocumented is required.
    for source in [
        "function Plain(a, b) end\nPlain(1)",
        "---@param a string\n---@param b any\n---@param c string|nil\nlocal function f(a, b, c) end\nf('x')",
        "---@param a string\n---@param b number\n---@overload fun(a: string)\nlocal function f(a, b) end\nf('x')",
        "---@alias MaybeNum number|nil\n---@param a string\n---@param b MaybeNum\nlocal function f(a, b) end\nf('x')",
        "---@param a string\n---@param b number\nlocal function f(a, b) end\nlocal function two() return 1, 2 end\nf(two())",
        "---@param a string\n---@param b number\nlocal function f(a, b) end\nlocal function g(...) f(...) end",
        "---@param a string\nlocal function f(a) end\n---@diagnostic disable-next-line: missing-parameter\nf()",
    ] {
        assert_eq!(missing(source), Vec::<String>::new(), "{source}");
    }
}

#[test]
fn locals_typed_as_functions_need_their_arguments() {
    let missing = |source: &str| missing_parameters(source, None, &[]);
    let handler = "---@alias Handler fun(a: integer, b: integer)\n";
    let needs_b = "is called with 1 argument, but needs 2; 'b' (integer) will be nil";
    for (source, called) in [
        ("---@type fun(a: integer, b: integer)\nlocal f = SomeGlobal\nf(1)", "f"),
        (&format!("{handler}---@type Handler\nlocal f = function(a, b) end\nf(1)"), "f"),
        (&format!("{handler}---@type Handler?\nlocal f\nf(1)"), "f"),
        (&format!("{handler}---@type Handler|nil\nlocal f\nf = SomeGlobal\nf(1)"), "f"),
        ("local M = {}\n---@type fun(a: integer, b: integer)\nM.cb = nil\nM.cb(1)", "M.cb"),
    ] {
        assert_eq!(missing(source), [format!("'{called}' {needs_b}")], "{source}");
    }
    assert_eq!(
        missing("---@type fun(a: integer, b?: integer)\nlocal f\nf()"),
        ["'f' is called with 0 arguments, but needs 1; 'a' (integer) will be nil"]
    );
    for source in [
        "---@type fun(a?: integer)\nlocal f\nf()",
        "---@type fun(a: integer)|fun()\nlocal f\nf()",
        "---@type table\nlocal f = {}\nf()",
        "---@alias Unknown Missing\n---@type Unknown\nlocal f\nf()",
    ] {
        assert_eq!(missing(source), Vec::<String>::new(), "{source}");
    }
    assert_eq!(
        redundant_parameters("---@type fun(a: integer)\nlocal f = function(...) end\nf(1, 2)", None, &[]),
        ["'f' is called with 2 arguments, but takes at most 1"],
        "the declared type holds over the value it is given"
    );
}

#[test]
fn methods_count_self_the_way_they_are_called() {
    let missing = |call: &str| {
        missing_parameters(&format!("local M = {{}}\n---@param x number\nfunction M:m(x) end\n{call}"), None, &[])
    };
    assert_eq!(missing("M:m()"), ["'M:m' is called with 0 arguments, but needs 1; 'x' (number) will be nil"]);
    assert_eq!(
        missing("M.m()"),
        ["'M.m' is called with 0 arguments, but needs 2; 'self' (call it with ':') will be nil"]
    );
    assert!(missing("M.m(M, 1)").is_empty());
    assert!(missing("M:m(1)").is_empty());

    let locale = "Locale = {}\n---@param opts table\nfunction Locale.new(_, opts) end\n";
    assert!(
        missing_parameters(&format!("{locale}Lang = Locale:new({{}})"), None, &[]).is_empty(),
        "the receiver fills '_'"
    );
}

#[test]
fn global_functions_are_checked_on_the_side_that_calls_them() {
    let client = (Some(Side::Client), "---@param message string\nfunction Notify(message) end");
    let server = (
        Some(Side::Server),
        "---@param source integer\n---@param message string\nfunction Notify(source, message) end",
    );
    let both = [client, server];
    assert!(missing_parameters("Notify('hi')", Some(Side::Client), &both).is_empty());
    assert_eq!(
        missing_parameters("Notify('hi')", Some(Side::Server), &both),
        ["'Notify' is called with 1 argument, but needs 2; 'message' (string) will be nil"]
    );
    assert!(
        missing_parameters("Notify('hi')", Some(Side::Shared), &both).is_empty(),
        "shared code may reach the client definition"
    );
    assert_eq!(
        missing_parameters("if IsDuplicityVersion() then Notify('hi') end", Some(Side::Shared), &both).len(),
        1,
        "only the server definition runs inside an IsDuplicityVersion branch"
    );

    let utils = (
        Some(Side::Shared),
        "Utils = {}\n---@param a number\n---@param b number\nfunction Utils.add(a, b) return a + b end",
    );
    assert_eq!(missing_parameters("Utils.add(1)", Some(Side::Client), &[utils]).len(), 1);
    assert!(missing_parameters("Utils.other(1)", Some(Side::Client), &[utils]).is_empty());
    assert!(
        missing_parameters("Notify('hi')", Some(Side::Server), &[client, server, (Some(Side::Server), OPAQUE)])
            .is_empty(),
        "an encrypted script may define Notify differently"
    );
    assert_eq!(missing_parameters("---@param a number\nfunction Loose(a) end\nLoose()", None, &[]).len(), 1);
}

#[test]
fn overloads_scoped_to_the_other_side_do_not_excuse_a_call() {
    let shared = (
        Some(Side::Shared),
        "---@param source integer\n---@param message string\n---@overload (client) fun(message: string)\nfunction Notify(source, message) end",
    );
    assert!(missing_parameters("Notify('hi')", Some(Side::Client), &[shared]).is_empty());
    assert_eq!(
        missing_parameters("Notify('hi')", Some(Side::Server), &[shared]),
        ["'Notify' is called with 1 argument, but needs 2; 'message' (string) will be nil"]
    );
    assert!(missing_parameters("Notify('hi')", Some(Side::Shared), &[shared]).is_empty());
    assert_eq!(
        missing_parameters("if IsDuplicityVersion() then Notify('hi') end", Some(Side::Shared), &[shared]).len(),
        1,
        "a guarded server branch cannot use the client overload"
    );
    let local = "---@param a string\n---@param b number\n---@overload (server) fun(a: string)\nlocal function f(a, b) end\nf('x')";
    assert_eq!(missing_parameters(local, Some(Side::Client), &[]).len(), 1);
    assert!(missing_parameters(local, Some(Side::Server), &[]).is_empty());
}

#[test]
fn calls_with_more_arguments_than_parameters() {
    let redundant = |source: &str| redundant_parameters(source, None, &[]);
    assert_eq!(
        redundant("---@param n number\nlocal function f(n) end\nf(1, 2, 3)"),
        ["'f' is called with 3 arguments, but takes at most 1"]
    );
    assert_eq!(
        redundant("local function noop() end\nnoop(function() end)"),
        ["'noop' is called with 1 argument, but takes none"],
        "undocumented functions take their parameters"
    );
    let two = "local function two(a, b) return a, b end\n";
    assert_eq!(
        redundant(&format!("{two}two(1, 2, two(3, 4))")),
        ["'two' is called with 3 arguments, but takes at most 2"],
        "a call after the parameters counts as one argument"
    );

    let methods = "local M = {}\nfunction M:m(x) end\nfunction M.f(x) end\n";
    let method = |call: &str| redundant(&format!("{methods}{call}"));
    assert_eq!(method("M:m(1, 2)"), ["'M:m' is called with 2 arguments, but takes at most 1"]);
    assert_eq!(method("M.m(M, 1, 2)"), ["'M.m' is called with 3 arguments, but takes at most 2"]);
    assert_eq!(method("M:f(1)"), ["'M:f' is called with 1 argument, but takes none"], "the receiver fills 'x'");
    assert!(method("M.m(M, 1)").is_empty());

    for source in [
        &format!("{two}two(1, two(2, 3))"),
        "local function f(a, ...) end\nf(1, 2, 3)",
        "---@param ... number\nlocal function f(...) end\nf(1, 2, 3)",
        "---@param a number\n---@overload fun(a: number, b: string)\nlocal function f(a) end\nf(1, 'x')",
        "local function f(a) end\nf = print\nf(1, 2)",
        "local function f(a) end\n---@diagnostic disable-next-line: redundant-parameter\nf(1, 2)",
        "SetEntityCoords(PlayerPedId(), 1.0, 2.0, 3.0, false, false, false, true, 1)",
    ] {
        assert_eq!(redundant(source), Vec::<String>::new(), "{source}");
    }
}

#[test]
fn the_definition_that_takes_the_most_arguments_decides() {
    let client = (Some(Side::Client), "---@param message string\nfunction Notify(message) end");
    let server = (Some(Side::Server), "function Notify(source, message) end");
    let both = [client, server];
    assert_eq!(
        redundant_parameters("Notify(1, 'hi')", Some(Side::Client), &both),
        ["'Notify' is called with 2 arguments, but takes at most 1"]
    );
    assert!(redundant_parameters("Notify(1, 'hi')", Some(Side::Server), &both).is_empty());
    assert!(
        redundant_parameters("Notify(1, 'hi')", Some(Side::Shared), &both).is_empty(),
        "shared code may reach the server definition"
    );

    let local =
        "---@param a string\n---@overload (server) fun(a: string, b: number)\nlocal function f(a) end\nf('x', 1)";
    assert_eq!(redundant_parameters(local, Some(Side::Client), &[]).len(), 1);
    assert!(redundant_parameters(local, Some(Side::Server), &[]).is_empty());
}

#[test]
fn values_the_linter_cannot_follow_are_not_checked() {
    let missing = |source: &str| missing_parameters(source, Some(Side::Client), &[]);
    for source in [
        // Another definition of the name, which may be the one that runs.
        "---@param a number\nfunction F(a) end\nF = print\nF()",
        "---@param a number\nlocal function f(a) end\nf = print\nf()",
        "local t = {}\n---@param a number\nfunction t.f(a) end\nt.f = nil\nt.f()",
        // A parameter holds whatever the caller passed.
        "---@param cb fun(a: number)\nlocal function run(cb) cb() end",
        // Natives and runtime functions are not defined by the resource.
        "SetEntityCoords(PlayerPedId(), 1.0, 2.0, 3.0)",
    ] {
        assert_eq!(missing(source), Vec::<String>::new(), "{source}");
    }
    assert_eq!(
        missing("local f\n---@param a number\nfunction f(a) end\nf()").len(),
        1,
        "a forward-declared local takes the function assigned to it"
    );
}

/// The findings of `code` in `source`, linted on its own, as `(line, message)`.
fn findings(source: &str, code: &str) -> Vec<(u32, String)> {
    let chunk = parse(source);
    let resolution = resolve(&chunk);
    let summary = summarize(source, &chunk, &resolution);
    let config = FileConfig::default();
    let input = FileInput {
        source,
        chunk: &chunk,
        resolution: &resolution,
        summary: &summary,
        config: &config,
        side: None,
        resource: None,
        crossrefs: None,
        locale: None,
        relative_path: "",
    };
    let lines = qbx_lua_syntax::LineIndex::new(source);
    check_file(&input)
        .into_iter()
        .filter(|d| d.code == code)
        .map(|d| (lines.line_of(d.span.start) + 1, d.message))
        .collect()
}

fn reported_lines(source: &str, code: &str) -> Vec<u32> {
    findings(source, code).into_iter().map(|(line, _)| line).collect()
}

#[test]
fn loops_that_count_up_past_their_end() {
    let source = "local t, n, step = {}, 5, 2
for i = 10, 1 do print(i) end
for i = 10, 1, 2 do print(i) end
for i = -1, -10 do print(i) end
for i = (3), 1.5 do print(i) end
for i = #t, 1 do print(i) end
for i = #t - 1, 1 do print(i) end
for i = 10, 1, -1 do print(i) end
for i = 1, 10 do print(i) end
for i = 1, 1 do print(i) end
for i = n, 1 do print(i) end
for i = #t, 0 do print(i) end
for i = #t, 1, -1 do print(i) end
for i = 10, 1, step do print(i) end
for i = 10, n do print(i) end";
    assert_eq!(reported_lines(source, "count-down-loop"), [2, 3, 4, 5, 6, 7]);
    assert_eq!(
        findings("for i = 10, 1 do print(i) end", "count-down-loop")[0].1,
        "the loop never runs: it counts up from 10 to 1; did you mean `10, 1, -1`?"
    );
    // `#t - 1, 1` still runs for two items or fewer, so the message names no number of items.
    assert_eq!(
        findings("local t = {}\nfor i = #t - 1, 1 do print(i) end", "count-down-loop")[0].1,
        "the loop counts up from #t - 1 to 1, so it never runs when #t - 1 is greater than 1; did you mean `#t - 1, 1, -1`?"
    );

    let fixed = |source: &str| {
        let chunk = parse(source);
        let resolution = resolve(&chunk);
        let summary = summarize(source, &chunk, &resolution);
        let config = FileConfig::default();
        let input = FileInput {
            source,
            chunk: &chunk,
            resolution: &resolution,
            summary: &summary,
            config: &config,
            side: None,
            resource: None,
            crossrefs: None,
            locale: None,
            relative_path: "",
        };
        qbx_lua_analysis::apply_fixes(source, &check_file(&input)).0
    };
    assert_eq!(fixed("for i = 10, 1 do print(i) end"), "for i = 10, 1, -1 do print(i) end");
    assert_eq!(fixed("for i = 10, 1, 2 do print(i) end"), "for i = 10, 1, -2 do print(i) end");
    assert_eq!(fixed("local t = {}\nfor i = #t, 1 do print(i) end"), "local t = {}\nfor i = #t, 1, -1 do print(i) end");
}

#[test]
fn functions_set_twice_on_one_field() {
    let source = "local M = {}
function M.f() end
function M.f() end
M.g = function() end
function M:g() end
M['h'] = function() end
M.h = function() end
Shared = {}
function Shared.a.b() end
function Shared.a.b() end
do
    function M.inDo() end
end
function M.inDo() end
local function setup()
    M.inner = function() end
    M.inner = function() end
end
return setup";
    assert_eq!(reported_lines(source, "duplicate-set-field"), [3, 5, 7, 10, 14, 17]);
    assert_eq!(
        findings(source, "duplicate-set-field")[0].1,
        "'M.f' is already defined on line 2; this definition replaces it"
    );

    for quiet in [
        // Branches of an `if`, and an `if` beside the code around it, as LuaLS reads them.
        "local M = {}\nif Config.Fast then\n    function M.f() end\nelse\n    function M.f() end\nend\nfunction M.g() end\nif Config.Fast then\n    function M.g() end\nend\nreturn M",
        // Each side by branch, and a shared default overridden after a guard.
        "Bridge = {}\nif IsDuplicityVersion() then\n    function Bridge.notify() end\nelse\n    function Bridge.notify() end\nend",
        "Bridge = {}\nif lib.context == 'server' then\n    function Bridge.notify() end\nelse\n    function Bridge.notify() end\nend",
        "Bridge = {}\nfunction Bridge.notify() end\nif not IsDuplicityVersion() then return end\nfunction Bridge.notify() end",
        // Code after an `if` that can return, like the code in a branch of an `if`.
        "F = {}\nfunction F.Get() end\nif GetResourceState('qbx_core') ~= 'started' then return end\nfunction F.Get() end",
        // A field read between the definitions, as a wrapper does, or called through.
        "local M = {}\nfunction M.n() end\nlocal old = M.n\nfunction M.n(m) old(m) end\nreturn M",
        "local M = {}\nfunction M.n() end\nM.n()\nfunction M.n() end\nreturn M",
        "local p = {}\nfunction p:enter() end\np:enter()\nfunction p:enter() end",
        // A new table in the variable, or in a field above the one set.
        "local p = lib.points.new(a)\nfunction p:onEnter() end\np = lib.points.new(b)\nfunction p:onEnter() end",
        "local M = { sub = {} }\nfunction M.sub.f() end\nM.sub = {}\nfunction M.sub.f() end\nreturn M",
        // Other tables, other functions, and values that are not function literals.
        "local A, B = {}, {}\nfunction A.f() end\nfunction B.f() end\nreturn A, B",
        "local function make()\n    local C = {}\n    function C.f() end\n    return C\nend\nlocal function other()\n    local C = {}\n    function C.f() end\n    return C\nend\nreturn make, other",
        "local M = {}\nM.x = function() end\nM.x = nil\nM.y = print\nM.y = print\nlocal f = function() end\nf = function() end\nreturn M, f",
        // Definition files declare signatures.
        "---@meta\nM = {}\nfunction M.f(a) end\nfunction M.f(a, b) end",
        // LuaLS's directive at either definition.
        "local M = {}\n---@diagnostic disable-next-line: duplicate-set-field\nfunction M.f() end\nfunction M.f() end\nreturn M",
        "local M = {}\nfunction M.f() end\n---@diagnostic disable-next-line: duplicate-set-field\nfunction M.f() end\nreturn M",
    ] {
        assert_eq!(reported_lines(quiet, "duplicate-set-field"), Vec::<u32>::new(), "{quiet}");
    }

    // Reads and new tables in other functions run later, when the second definition has replaced
    // the first, and a new table in another field changes nothing.
    let source = "local M = {}
function M.n() return M.n() end
AddEventHandler('x', function() M.n() end)
function M.n() end
local p = {}
function p:a() end
local function reset() p = {} end
function p:a() end
function M.f() end
M.fx = {}
function M.f() end
return reset";
    assert_eq!(reported_lines(source, "duplicate-set-field"), [4, 8, 11]);
}

#[test]
fn doc_params_name_parameters_of_the_documented_function() {
    let source = "---@param undocumented integer
local function wrongName(actual) return actual end
---@param ... any
local function noVararg(x) return x end
---@param self table
function M.dot() end
---@param src number
---@param extra string
RegisterNetEvent('ev', function(src) print(src) end)
---@param source number
AddEventHandler('playerDropped', function() print(source) end)
---@param a number
local notAFunction = 5
---@param a number

local afterBlank = function(a) return a end
return wrongName, noVararg, notAFunction, afterBlank";
    assert_eq!(reported_lines(source, "undefined-doc-param"), [1, 3, 5, 8, 10, 12, 14]);
    let messages: Vec<String> = findings(source, "undefined-doc-param").into_iter().map(|(_, m)| m).collect();
    assert_eq!(messages[0], "the function below has no parameter 'undocumented'");
    assert_eq!(messages[5], "no function follows the @param 'a' annotation");

    for quiet in [
        "M = {}\n---@param self table\n---@param x number\nfunction M:method(x) return self, x end",
        "---@param x number\n---@param y? number\n---@param ... any\nlocal function vararg(x, y, ...) return x, y, ... end\nreturn vararg",
        // The functions of the statement below, also on its later lines.
        "---@param source number\n---@param data table\nlib.callback.register('name',\n    function(source, data) return source, data end)",
        "---@param a number\nlocal function multi(\n    a\n) return a end\nreturn multi",
        "---@param a number\n---@param b number\nlocal x, y = 1, function(a, b) return a + b end\nreturn x, y",
        // Every function that starts on the line below, as LuaLS binds it.
        "local t = {\n    ---@param args table\n    onSelect = function(args) return args end,\n}\nreturn t",
        "---@param x number\nfoo(bar(function(x) return x end))",
        "---@param b number\nreturn function(b) return b end",
        // The variables of a `for ... in` loop.
        "---@param k string\nfor k in pairs({}) do print(k) end",
        // Comment lines between the doc comment and the code.
        "---@param d number\n-- qbx-lint: disable-next-line lowercase-global\nfunction lower(d) return d end",
        "---@param e number\n--[[ note ]]\nlocal function block(e) return e end\nreturn block",
        // A doc comment after code describes that line, and directives inside the comment apply.
        "local z = 1 ---@param q number\nreturn z",
        "---@diagnostic disable-next-line: undefined-doc-param\n---@param targets table\nfunction AddTargets(...) return ... end",
        "local Config = {} ---@type table\n---@param a number\nlocal function f(a) return a, Config end\nreturn f",
        // The parameter names of a `@type fun(...)` in the comment.
        "---@param a number\n---@type fun(a: number)\nlocal handler\nreturn handler",
        "---@param b number\n---@param ... any\n---@type fun(b: number, ...: any)|nil\nlocal wrapped = wrap(print)\nreturn wrapped",
        // A local or global function the statement passes by name.
        "local function play(data) print(data) end\n---@param data table\nexports('Play', play)",
        "local onDrop = function(reason) print(reason) end\n---@param reason string\nAddEventHandler('playerDropped', onDrop)",
        "function setChannel(channel) print(channel) end\n---@param channel number\nexports('setChannel', setChannel)",
        // Definition files document stubs.
        "---@meta\n\n---@param ped integer\nfunction Stub(...) end",
    ] {
        assert_eq!(reported_lines(quiet, "undefined-doc-param"), Vec::<u32>::new(), "{quiet}");
    }

    // The lines below a doc comment after code, names a `@type fun(...)` or a passed function does
    // not have, and names passed that hold no function.
    let source = "local Config = {} ---@type table
---@param wrong number
local function f(a) return a, Config end
---@param c number
---@type fun(a: number)
local handler
---@param other table
exports('F', f)
local value = 1
---@param x number
print(value)
return handler";
    assert_eq!(
        findings(source, "undefined-doc-param"),
        [
            (2, "the function below has no parameter 'wrong'".to_string()),
            (4, "the function below has no parameter 'c'".to_string()),
            (7, "the function below has no parameter 'other'".to_string()),
            (10, "no function follows the @param 'x' annotation".to_string()),
        ]
    );
}

#[test]
fn aliases_declared_twice_in_one_file() {
    let source = "---@alias Dup string
---@alias Dup integer
---@class Shape
---@alias Shape string
---@enum Color
local Color = { Red = 1 }
---@alias Color string
---@alias Later string
---@class Later
return Color";
    assert_eq!(reported_lines(source, "duplicate-doc-alias"), [2, 4, 7, 8]);
    let messages: Vec<String> = findings(source, "duplicate-doc-alias").into_iter().map(|(_, m)| m).collect();
    assert_eq!(messages[0], "'Dup' is already declared as an alias on line 1");
    assert_eq!(messages[1], "'Shape' is already declared as a class on line 3");
    assert_eq!(messages[2], "'Color' is already declared as an enum on line 5");

    for quiet in [
        // One declaration for each side.
        "---@alias (server) Sided string\n---@alias (client) Sided integer",
        "---@enum (server) Jobs\nlocal Jobs = { A = 1 }\n---@alias (client) Jobs string\nreturn Jobs",
        // `(partial)` on any of them.
        "---@class (partial) Part\n---@alias Part string",
        "---@alias (partial) Part string\n---@alias Part integer",
        // Classes merge their declarations.
        "---@class Merged\n---@field a string\n---@class Merged\n---@field b string",
    ] {
        assert_eq!(reported_lines(quiet, "duplicate-doc-alias"), Vec::<u32>::new(), "{quiet}");
    }
}

#[test]
fn fields_declared_twice_for_one_class() {
    let source = "---@class Twice
---@field a string
---@field a integer
---@field g fun()|nil
---@field g fun()|nil
---@field [string] number
---@field [string] boolean
---@field ['b'] string
---@field b number

---@class Twice
---@field a boolean";
    assert_eq!(reported_lines(source, "duplicate-doc-field"), [3, 5, 7, 9, 12]);
    assert_eq!(
        findings(source, "duplicate-doc-field")[0].1,
        "field 'a' of class 'Twice' is already declared on line 2"
    );

    for quiet in [
        // Repeated function fields are overloads, scoped or not.
        "---@class Phone\n---@field Has fun(): boolean\n---@field Has fun(source: number): boolean",
        "---@class Phone\n---@field (client) Has fun(): boolean\n---@field (server) Has fun(source: number): boolean",
        "---@class Mixed\n---@field h string\n---@field h fun()",
        // One declaration for each side, by field or by class.
        "---@class Phone\n---@field (client) hud table\n---@field (server) hud string",
        "---@class (server) Account\n---@field money number\n\n---@class (client) Account\n---@field money string",
        // Other classes, including a child that narrows a parent's field.
        "---@class A\n---@field x string\n---@class B\n---@field x string",
        "---@class Base\n---@field x string\n---@class Child : Base\n---@field x 'a'|'b'",
        "---@class Lit\n---@field [1] string\n---@field [2] string",
    ] {
        assert_eq!(reported_lines(quiet, "duplicate-doc-field"), Vec::<u32>::new(), "{quiet}");
    }
}
