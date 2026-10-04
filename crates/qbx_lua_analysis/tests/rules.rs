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
    assert_eq!(client("print(IsDuplicityVersion() and GetPlayers() or {})"), none);
    assert_eq!(client("print(IsDuplicityVersion() or GetPlayers())"), wrong);
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
fn server_libraries_are_missing_on_the_client() {
    let wrong_side = |source: &str, side: Option<Side>| call_messages("fivem/native-wrong-side", source, side, &[]);
    let (client, server, shared) = (Some(Side::Client), Some(Side::Server), Some(Side::Shared));
    let none = Vec::<String>::new();

    let os_on_client = "'os' only exists on the server, but this is a client script";
    assert_eq!(wrong_side("print(os.time())", client), [os_on_client]);
    assert_eq!(wrong_side("local now = os.microtime", client), [os_on_client], "CfxLua's additions");
    assert_eq!(
        wrong_side("local file = io.open('data.json')", client),
        ["'io' only exists on the server, but this is a client script"]
    );
    assert_eq!(wrong_side("print(os.time(), io.open('data.json'), os.createdir('logs'))", server), none);
    assert_eq!(wrong_side("print(os.time())", None), none, "a file no manifest lists");
    assert_eq!(wrong_side("if IsDuplicityVersion() then\n    print(os.time())\nend", client), none);

    assert_eq!(
        wrong_side("print(os.time())", shared),
        ["'os' only exists on the server, but this shared script also runs on the client"]
    );
    assert_eq!(wrong_side("print(string.format('%d', math.floor(1.5)))", shared), none);
    assert_eq!(wrong_side("TriggerClientEvent('a', -1)", shared), none, "CfxLua globals follow the calling side");

    for source in [
        "if IsDuplicityVersion() then\n    print(os.time())\nend",
        "if IsDuplicityVersion() and os.time() > 0 then\n    print(1)\nend",
        "local isServer = IsDuplicityVersion()\nprint(isServer and os.time() or GetGameTimer())",
        "if not IsDuplicityVersion() then return end\nprint(os.time())",
        "local function now()\n    if not IsDuplicityVersion() then return 0 end\n    return os.time()\nend\nprint(now())",
        "local isServer = IsDuplicityVersion()\nif isServer then print(os.time()) end",
        "local isServer = IsDuplicityVersion()\nif not isServer then return end\nprint(os.time())",
        "if not IsDuplicityVersion() then\n    print(1)\nelse\n    print(os.time())\nend",
        "if lib.context == 'server' then\n    print(os.time())\nend",
        "if lib.context ~= 'client' then\n    print(os.time())\nend",
    ] {
        assert_eq!(wrong_side(source, shared), none, "only the server runs: {source}");
    }

    let os_in_client_code = "'os' only exists on the server, but this is code that only runs on the client";
    for source in [
        "if not IsDuplicityVersion() then\n    print(os.time())\nend",
        "if IsDuplicityVersion() then return end\nprint(os.time())",
        "local isServer = IsDuplicityVersion()\nif not isServer then print(os.time()) end",
        "print(not IsDuplicityVersion() and os.time())",
        "if IsDuplicityVersion() then\n    print(1)\nelse\n    print(os.time())\nend",
        "if lib.context == 'client' then\n    print(os.time())\nend",
    ] {
        assert_eq!(wrong_side(source, shared), [os_in_client_code], "only the client runs: {source}");
    }
}

#[test]
fn server_libraries_have_the_fields_fivem_gives_them() {
    let undefined = |source: &str| call_messages("undefined-field", source, Some(Side::Server), &[]);
    assert_eq!(undefined("os.exit(1)"), ["'os' has no field 'exit'"]);
    assert_eq!(undefined("print(io.read('l'))"), ["'io' has no field 'read'"]);
    assert_eq!(
        undefined("print(os.createdir('logs'), os.nanotime(), io.readdir('logs'), io.write('a'))"),
        Vec::<String>::new()
    );
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
        "repeat\n    local line = coroutine.yield()\nuntil line == nil",
    ] {
        assert_eq!(codes(source), Vec::<&str>::new(), "{source}");
    }
}

#[test]
fn local_statements_and_assignments_give_each_name_a_value() {
    let unbalanced = |source: &str| codes(source).into_iter().filter(|code| *code == "unbalanced-assignments").count();
    assert_eq!(unbalanced("local a, b = 1\nprint(a, b)"), 1, "as in LuaLS");
    assert_eq!(unbalanced("gx, gy = 1\nprint(gx, gy)"), 1);
    assert_eq!(unbalanced("local a = 1, 2\nprint(a)"), 1, "an extra value");
    for source in [
        "local a, b\nprint(a, b)",
        "local a, b = pcall(error)\nprint(a, b)",
        "local function f(...) local a, b = ... return a, b end\nprint(f)",
        "local a, b = 1, 2\nprint(a, b)",
    ] {
        assert_eq!(unbalanced(source), 0, "{source}");
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

    // The parameters and the locals of a function body share one block, as in Lua.
    let source = "local function f(source, volume)
    local source = source
    local volume = volume / 100
    if source then local volume = 1 print(volume) end
    for i = 1, 2 do local i = i print(i) end
    return source, volume
end
local M = {}
function M:method() local self = self return self end
print(f, M)";
    assert_eq!(reported_lines(source, "redefined-local"), [2, 3, 9]);
    assert_eq!(findings(source, "redefined-local")[0].1, "local 'source' is already declared in this scope");
    let shadows: Vec<u32> =
        findings_with(source, "shadowed-local", &config).into_iter().map(|(line, _)| line).collect();
    assert_eq!(shadows, [4, 5], "a nested block, and the body of a loop, are blocks of their own");
}

#[test]
fn tables_whose_fields_are_only_set_are_unused() {
    let source = "local cache = {}
cache.a = 1
cache['b'] = 2
local handlers = { a = 1 }
function handlers.f() end
function handlers:m() end
RegisterNetEvent('x', function(k, v) handlers[k] = v end)
local list = {}
list[#list + 1] = 1
local nested = {}
nested.a.b = 1
local later
later = {}
later.a = 1
---@class Lt.Kind
local Kind = {}
function Kind.new() end
local counter = {}
counter.n += 1
local wrapped = setmetatable({}, {})
wrapped.a = 1
local returned = {}
returned.a = 1
return returned";
    assert_eq!(reported_lines(source, "unused-local"), [1, 4]);
    assert_eq!(findings(source, "unused-local")[0].1, "local 'cache' is never read; only its fields are set");
}

#[test]
fn functions_only_used_by_unused_functions_are_unused() {
    let source = "local function helper() end
local function wrapper() helper() end
local function recursive() recursive() end
local held = function() end
local function holder() held() end
local function first() end
local function second() first() end
local function third() second() end
local function a() end
local function b() a() end
local function c() b() end
c()
local function fromCallback() end
local function spawner() CreateThread(function() fromCallback() end) end
local function exported() end
local M = {}
function M.run() exported() end
local function viaKept() end
local function _kept() viaKept() end
return M";
    assert_eq!(reported_lines(source, "unused-function"), [1, 2, 3, 5, 6, 7, 8, 14]);
    assert_eq!(reported_lines(source, "unused-local"), [4]);
    assert_eq!(
        findings(source, "unused-function")[0].1,
        "function 'helper' is only used by functions that are never used"
    );
    assert_eq!(findings(source, "unused-function")[1].1, "unused function 'wrapper'");
    assert_eq!(reported_lines("---@meta\nlocal function a() end\nlocal function b() a() end", "unused-function"), [3]);
}

#[test]
fn varargs_that_the_body_never_uses() {
    let source = "local function a(...) print(1) end
local function b(x, ...) return x end
local function c(...) local function inner(...) print(...) end inner() end
local function d(...) end
local function e(...) print(...) end
local function f(...) return select('#', ...) end
local g = function(...) local t = { ... } return t end
print(a, b, c, d, e, f, g)";
    assert_eq!(reported_lines(source, "unused-vararg"), [1, 2, 3]);
    assert_eq!(findings(source, "unused-vararg")[0].1, "'...' is never used");
    assert_eq!(
        reported_lines(
            "---@meta
function Stub(...) print(1) end",
            "unused-vararg"
        ),
        Vec::<u32>::new()
    );
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
    findings_with(source, code, &FileConfig::default())
}

/// The findings of `code`, a rule that is off by default, with it turned on.
fn opt_in_findings(source: &str, code: &str) -> Vec<(u32, String)> {
    let mut config = FileConfig::default();
    config.set(code, Level::Warning);
    findings_with(source, code, &config)
}

fn findings_with(source: &str, code: &str, config: &FileConfig) -> Vec<(u32, String)> {
    let chunk = parse(source);
    let resolution = resolve(&chunk);
    let summary = summarize(source, &chunk, &resolution);
    let input = FileInput {
        source,
        chunk: &chunk,
        resolution: &resolution,
        summary: &summary,
        config,
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

fn owned(findings: &[(u32, &str)]) -> Vec<(u32, String)> {
    findings.iter().map(|&(line, message)| (line, message.to_string())).collect()
}

#[test]
fn global_functions_without_annotations() {
    let source = "function Add(a, b) return a + b end
function Nothing() end
-- Opens the menu.
function Commented() end
---@diagnostic disable-next-line: lowercase-global
function diagnosticOnly() end
---@param id integer
function Partial(id) return tostring(id) end
function Branches(flag)
    if flag then return 1, 2 end
    return 3
end
function Ignored(self, _, _unused, ...) end
---@type fun(a: integer): integer
Typed = function(a) return a end
---@param ... any
---@return integer ...
function Variadic(...) return 1, 2, 3 end
function Outer()
    function Inner() end
end
local function notGlobal(x) return x end
Table = {}
function Table.field(y) return y end
function Table:method(z) return z end
function BareReturn() if Table then return end end
return notGlobal";
    assert_eq!(
        opt_in_findings(source, "missing-global-doc"),
        owned(&[
            (1, "parameter 'a' of global function 'Add' has no @param annotation"),
            (1, "parameter 'b' of global function 'Add' has no @param annotation"),
            (1, "return value #1 of global function 'Add' has no @return annotation"),
            (2, "global function 'Nothing' has no comment"),
            (6, "global function 'diagnosticOnly' has no comment"),
            (8, "return value #1 of global function 'Partial' has no @return annotation"),
            (9, "parameter 'flag' of global function 'Branches' has no @param annotation"),
            (10, "return value #1 of global function 'Branches' has no @return annotation"),
            (10, "return value #2 of global function 'Branches' has no @return annotation"),
            (11, "return value #1 of global function 'Branches' has no @return annotation"),
            (13, "parameter '...' of global function 'Ignored' has no @param annotation"),
            (19, "global function 'Outer' has no comment"),
            (20, "global function 'Inner' has no comment"),
            (26, "global function 'BareReturn' has no comment"),
        ])
    );
    // Off unless turned on, like the other two.
    for code in ["missing-global-doc", "missing-local-export-doc", "incomplete-signature-doc"] {
        assert_eq!(findings(source, code), [], "{code}");
    }
}

#[test]
fn exported_local_functions_without_annotations() {
    let source = "local M = {}
local function helper(a) return a end
M.helper = helper
M.again = helper
function M.direct(b) return b end
M.assigned = function(c) return c end
---@param d integer
local function documented(d) return d end
M.documented = documented
local function nothing() end
M.nothing = nothing
local inline = function(e) end
exports('Inline', function(f) return f end)
exports('Local', inline)
function Global(g) return g end
exports('Global', Global)
-- Shows the menu.
exports('Commented', function() end)
exports('Bare', function() end)
local Other = {}
local function hidden(h) return h end
Other.hidden = hidden
return M";
    assert_eq!(
        opt_in_findings(source, "missing-local-export-doc"),
        owned(&[
            (2, "parameter 'a' of exported local function 'helper' has no @param annotation"),
            (2, "return value #1 of exported local function 'helper' has no @return annotation"),
            (8, "return value #1 of exported local function 'documented' has no @return annotation"),
            (10, "exported local function 'nothing' has no comment"),
            (12, "parameter 'e' of exported local function 'inline' has no @param annotation"),
            (13, "parameter 'f' of exported function 'Inline' has no @param annotation"),
            (13, "return value #1 of exported function 'Inline' has no @return annotation"),
            (19, "exported function 'Bare' has no comment"),
        ])
    );
}

#[test]
fn partly_annotated_functions() {
    let source = "---@param a integer
local function partial(a, b) return a end
local function undocumented(c) return c end
--- Only a description.
local function described(d) return d end
---@param src number
RegisterNetEvent('event', function(src, data) end)
local handlers = {
    ---@param e integer
    one = function(e, f) end, two = function(g) end,
}
---@param x integer
local nested = function(x) return function(y) return y end end
---@param h integer
---@return integer
---@return string
local function extra(h) return h, 'x', nil end
local function byName(i, j) return i, j end
---@param i integer
RegisterNetEvent('byName', byName)
---@param k integer
RegisterNetEvent('lines',
    function(k, l) end)
---@param self table
---@param m integer
function handlers:method(m, _n) end
---@type fun(o: integer)
local typed = function(o, p) end
return partial, undocumented, described, handlers, nested, extra, byName, typed";
    assert_eq!(
        opt_in_findings(source, "incomplete-signature-doc"),
        owned(&[
            (2, "incomplete signature: parameter 'b' has no @param annotation"),
            (2, "incomplete signature: return value #1 has no @return annotation"),
            (7, "incomplete signature: parameter 'data' has no @param annotation"),
            (10, "incomplete signature: parameter 'f' has no @param annotation"),
            (13, "incomplete signature: return value #1 has no @return annotation"),
            (17, "incomplete signature: return value #3 has no @return annotation"),
            (23, "incomplete signature: parameter 'l' has no @param annotation"),
        ])
    );

    // The parameters that the function type of a documented callback wrapper names.
    let source = "---@param event string
---@param callback fun(source: number, phoneNumber: string, ...): any
function BaseCallback(event, callback) end
---@param contact table
BaseCallback('saveContact', function(source, phoneNumber, contact, extra) return contact, extra end)
---@param name string
---@param handler fun(src: number, ...)
local function register(name, handler) end
---@param data table
register('local', function(src, data) end)
local Wrapper = {}
---@param cb fun(player: table)
function Wrapper:on(cb) end
---@param other string
Wrapper:on(function(player, other) end)
local function undocumented(name, cb) end
---@param data table
undocumented('x', function(src, data) end)";
    assert_eq!(
        opt_in_findings(source, "incomplete-signature-doc"),
        owned(&[
            (5, "incomplete signature: parameter 'extra' has no @param annotation"),
            (5, "incomplete signature: return value #1 has no @return annotation"),
            (5, "incomplete signature: return value #2 has no @return annotation"),
            (18, "incomplete signature: parameter 'src' has no @param annotation"),
        ])
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

/// `source` with every fix that the default rules offer applied.
fn all_fixes(source: &str) -> String {
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
}

#[test]
fn parentheses_that_continue_the_line_above() {
    let source = "local a = print
(\"x\"):len()
local b = print
(\"x\", \"y\"):len()
local c = print
(\"x\")
local d = print
(function() end)()
local e = print -- note
(a).f = 1
local f = a:m
(\"x\"):len()
local g = print
():len()
local h = print(\"x\"):len()
local i = Config.Some.Very.Long.Path.To.A.Function.Here
(1)()
print(b, c, d, e, f, g, h, i)";
    assert_eq!(reported_lines(source, "newline-call"), [1, 7, 9, 16]);
    let messages: Vec<String> = findings(source, "newline-call").into_iter().map(|(_, m)| m).collect();
    assert_eq!(
        messages[0],
        "'print' on the line above is called with the parentheses on this line; put a ';' before the '(' if this line starts a new statement"
    );
    assert!(messages[3].starts_with("the expression on the line above is called"), "{}", messages[3]);
}

#[test]
fn params_documented_twice() {
    let source = "---@param x number
---@param x string
---@param y number
local function f(x, y) return x, y end
---@param ... any
---@param ... any
local function g(...) return ... end
---@param z number

---@param z number
local function h(z) return z end
print(f, g, h)";
    assert_eq!(reported_lines(source, "duplicate-doc-param"), [1, 2, 5, 6]);
    let messages: Vec<String> = findings(source, "duplicate-doc-param").into_iter().map(|(_, m)| m).collect();
    assert_eq!(messages[0], "duplicate @param 'x', also on line 2");
    assert_eq!(messages[1], "duplicate @param 'x', also on line 1");
}

#[test]
fn fields_must_directly_follow_their_class() {
    let source = "---@field orphan number
local t = {}
---@class A
---@field a number
---Description
---@field b number
---@diagnostic disable-next-line: unused-local
---@field c number
---@operator add: A
---@overload fun(): A
---@field d number
---@deprecated
---@field e number
local A = {}
---@type A
---@field f number
local tA = {}
---@class B
---@param p number
---@field g number
local function fB(p) return p end
---@class C
---@author someone
---@field h number
local C = {}
---@class D
-- plain comment
---@field i number
local D = {}
local tail = {} ---@field j number
---@class E
---@field k number
---| 'x'
---@field l number
local E = {}
print(t, A, tA, fB, C, D, tail, E)";
    assert_eq!(
        findings(source, "doc-field-no-class"),
        [
            (1, "field 'orphan' has no @class above it".to_string()),
            (13, "the @deprecated line separates field 'e' from its @class".to_string()),
            (16, "field 'f' has no @class above it".to_string()),
            (20, "the @param line separates field 'g' from its @class".to_string()),
            (30, "field 'j' has no @class above it".to_string()),
        ]
    );
}

#[test]
fn operators_luacats_cannot_declare() {
    let source = "---@class Vec
---@operator add(Vec): Vec
---@operator unm: Vec
---@operator call: Vec
---@operator sar: Vec
---@operator eq: boolean
---@operator index(string): Vec
---@operator ad(Vec): Vec
local Vec = {}
return Vec";
    assert_eq!(reported_lines(source, "unknown-operator"), [6, 7, 8]);
    assert_eq!(
        findings(source, "unknown-operator")[0].1,
        "unknown operator 'eq'; @operator takes add, sub, mul, div, mod, pow, idiv, band, bor, bxor, shl, shr, concat, unm, bnot, len or call"
    );
}

#[test]
fn casts_name_a_local_in_scope() {
    let source = "Global = 1
---@cast Global string
local function f(p)
    ---@cast p string
    ---@cast self string
    return p
end
local M = {}
function M:m()
    ---@cast self table
    return self
end
---@cast later string
local later = 1
---@cast later string
local x = 1 ---@cast x string
---@cast M.field string
for _, item in ipairs({}) do
    ---@cast item string
    print(item)
end
for i = 1, 2 do
    ---@cast i string
end
print(f, later, x)";
    assert_eq!(reported_lines(source, "unknown-cast-variable"), [2, 5, 13, 17]);
    assert_eq!(
        findings(source, "unknown-cast-variable")[0].1,
        "no local 'Global' is in scope here; @cast changes the type of a local"
    );
}

#[test]
fn directives_name_known_codes() {
    let source = "---@diagnostic disable-next-line: unused-local, no-such-code
local a = 1
-- qbx-lint: disable-next-line fivem/loop-never-yields unused-locals
local b = 2
---@diagnostic disable: unused-vararg, miss-end, codestyle-check
---@diagnostic enable: undefined-globl
-- luacheck: ignore 211
print(a, b)";
    assert_eq!(reported_lines(source, "unknown-diag-code"), [1, 3, 6]);
    let messages: Vec<String> = findings(source, "unknown-diag-code").into_iter().map(|(_, m)| m).collect();
    assert_eq!(messages[0], "unknown diagnostic code 'no-such-code'");
    assert_eq!(messages[1], "unknown diagnostic code 'unused-locals'; did you mean 'unused-local'?");
    assert_eq!(messages[2], "unknown diagnostic code 'undefined-globl'; did you mean 'undefined-global'?");
}

#[test]
fn code_after_statements_that_never_finish() {
    let source = "local c = math.random() > 0.5
local function a() if c then return 1 else return 2 end print(1) end
local function b() if c then return 1 elseif c then return 2 else if c then return 3 else return 4 end end print(1) end
local function d() if c then error('x') else os.exit(1) end print(1) end
local function e() while true do if c then return end Wait(0) end print(1) end
local function f() for i = 1, 2 do if i then break else goto continue end print(i) ::continue:: end end
local function g() if c then return 1 else return 2 end ::later:: print(1) end
local function h() if c then return 1 else return 2 end print(1) ::later:: print(2) end
print(a, b, d, e, f, g, h)
if c then return else return end
print(1)";
    assert_eq!(reported_lines(source, "unreachable-code"), [2, 3, 4, 5, 6, 8, 11]);
    for source in [
        "local function f() if math.random() > 0.5 then return 1 end print(1) end\nprint(f)",
        "local function f() error('x') print(1) end\nprint(f)",
        "local function f() do return end print(1) end\nprint(f)",
        "local function f() while math.random() do Wait(0) end print(1) end\nprint(f)",
        "local function f() while true do if math.random() then break end end print(1) end\nprint(f)",
        "local function f() while true do goto out end ::out:: print(1) end\nprint(f)",
        "local function f() while true do for _ = 1, 2 do goto done end end ::done:: print(1) end\nprint(f)",
        "local function f(t) if t then for _, v in pairs(t) do return v end else return end print(1) end\nprint(f)",
        "local function f() repeat Wait(0) until false print(1) end\nprint(f)",
        "local function error() end\nlocal function f() if math.random() then error() else return end print(1) end\nprint(f)",
    ] {
        assert_eq!(reported_lines(source, "unreachable-code"), Vec::<u32>::new(), "{source}");
    }
}

#[test]
fn table_entries_called_with_arguments_on_the_next_line() {
    let source = "local list = {
    print
    ('x'),
    tostring
    'y',
    setmetatable
    { 1 },
    list:method
    (1),
}
local fine = {
    print, ('x'),
    print('x'),
    named = print
    ('x'),
    print
    ;('x'),
}
print(fine)";
    assert_eq!(reported_lines(source, "newfield-call"), [2, 4, 6, 8]);
    assert_eq!(
        findings(source, "newfield-call")[0].1,
        "'print' is called with the arguments on the next line, which make one entry of the table; put a ',' between them if they are two"
    );
}

#[test]
fn or_next_to_operators_that_bind_tighter() {
    let source = "local x, y = 1, 2
print(x + y or 0)
print(x or 1 + y)
print(x .. y or '')
print(x * y or {})
print(x or 2 * y)
print(x + (y or 0), (x + y) or 0, (x or 1) + y)
print(x + 1 or 0, -x or 0, x == 1 or 0, x or y + 1, x + y or nil)";
    assert_eq!(reported_lines(source, "ambiguity-1"), [2, 3, 4, 5, 6]);
    let messages: Vec<String> = findings(source, "ambiguity-1").into_iter().map(|(_, m)| m).collect();
    assert_eq!(messages[0], "'x + y' is computed before the 'or'; write 'x + (y or 0)' if that was meant");
    assert_eq!(messages[1], "'1 + y' is computed before the 'or'; write '(x or 1) + y' if that was meant");
}

#[test]
fn returns_without_values_that_end_a_function() {
    let source = "local function a() return end
local function b() if a then return end end
local function c() do return end end
local function d() print(1) return; end
local e = function() return nil end
print(b, c, d, e)
return";
    assert_eq!(reported_lines(source, "redundant-return"), [1, 4]);
    assert_eq!(findings(source, "redundant-return")[0].1, "redundant return at the end of the function");
}

#[test]
fn whitespace_at_the_end_of_lines() {
    let source = "local a = 1  \nlocal b = 2\t\n   \n-- comment  \nlocal s = [[x  \ny]]  \nprint(a, b, s) --[[ c ]]  \r\nprint(1)  ";
    assert_eq!(reported_lines(source, "trailing-space"), [1, 2, 3, 6, 7, 8]);
    let messages: Vec<String> = findings(source, "trailing-space").into_iter().map(|(_, m)| m).collect();
    assert_eq!(messages[0], "trailing whitespace");
    assert_eq!(messages[2], "line contains only whitespace");
    assert_eq!(
        all_fixes(source),
        "local a = 1\nlocal b = 2\n\n-- comment  \nlocal s = [[x  \ny]]\nprint(a, b, s) --[[ c ]]\r\nprint(1)"
    );
}
