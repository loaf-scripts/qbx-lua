use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{CompletionResponse, NumberOrString, Url};
use qbx_fivem_data::Side;
use qbx_lua_analysis::crossref::CrossRefs;
use qbx_lua_ls::document::Document;
use qbx_lua_ls::features::{completion, diagnostics, event_call};
use qbx_lua_ls::index::FileOrigin;
use qbx_lua_ls::types::FunType;
use qbx_lua_ls::workspace::{path_to_uri, Workspace};
use serde_json::{json, Value};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qbx-lua-ls-refresh-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn workspace(&self, root: &str) -> Workspace {
        let mut ws = Workspace::default();
        ws.roots.push(self.0.join(root));
        ws.load_stubs();
        ws.scan();
        ws
    }

    fn document(&self, ws: &mut Workspace, relative: &str, text: &str) -> Document {
        let path = self.0.join(relative);
        let mut doc = Document::new(path_to_uri(&path), path, 1, text.to_string());
        doc.file = ws.index_parsed(&doc.path, FileOrigin::Workspace, &doc.text, &doc.chunk, &doc.resolution);
        doc
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
            if root.parent() == Some(temp.as_path()) {
                let _ = std::fs::remove_dir_all(root);
            }
        }
    }
}

const GUARDED: &str = "if IsDuplicityVersion() then\n\
    RegisterNetEvent('demo:server', function(serverValue) end)\n\
    RegisterNetEvent('demo:both', function(serverValue) end)\n\
    lib.callback.register('demo:callback', function(source, serverValue) end)\n\
else\n\
    AddEventHandler('demo:client', function(clientValue) end)\n\
    AddEventHandler('demo:both', function(clientValue) end)\n\
    lib.callback.register('demo:callback', function(clientValue) end)\n\
end\n";

fn event_workspace() -> (Fixture, Workspace) {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "shared_script 'shared.lua'\nclient_script 'client.lua'\n");
    fixture.write("demo/shared.lua", GUARDED);
    fixture.write("demo/client.lua", "");
    let ws = fixture.workspace("demo");
    (fixture, ws)
}

#[test]
fn guarded_event_diagnostics_match_the_cli() {
    let (fixture, mut ws) = event_workspace();
    let refs = ws.crossrefs();
    let mut cli = CrossRefs::default();
    cli.collect(&qbx_lua_syntax::parse(GUARDED), Some(Side::Shared), Some("demo"));
    for (name, expected) in cli.events {
        let actual = &refs.events[&name];
        assert_eq!(actual.len(), expected.len(), "{name}");
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.side, expected.side, "{name}");
            assert_eq!(actual.handler, expected.handler, "{name}");
        }
    }
    let doc = fixture.document(&mut ws, "demo/client.lua", "TriggerEvent('demo:server', 1)\n");
    let found = diagnostics::diagnostics(&ws, &doc, &[], &ws.crossrefs());
    assert!(found.iter().any(|d| d.code == Some(NumberOrString::String("fivem/event-wrong-side".into()))));
}

fn labels(ws: &Workspace, doc: &Document) -> Vec<String> {
    let offset = doc.text.rfind("''").unwrap() + 1;
    let result = completion::completion(ws, doc, doc.position(offset as u32), true, false, None).unwrap();
    let items = match result {
        CompletionResponse::List(list) => list.items,
        CompletionResponse::Array(items) => items,
    };
    items.into_iter().map(|item| item.label).collect()
}

fn payload(ws: &Workspace, doc: &Document) -> Option<Vec<String>> {
    let offset = doc.text.rfind("1)").unwrap();
    let site = qbx_lua_ls::locate::locate(&doc.chunk, offset as u32).call.unwrap();
    let event = qbx_lua_ls::features::with_infer(ws, doc, |infer| {
        event_call::event_call(ws, doc, infer, site.base, site.args, Some(&FunType::default()))
    })?;
    Some(event.fun.params.into_iter().map(|p| p.name.to_string()).collect())
}

#[test]
fn guarded_event_completion_and_signatures_follow_both_sides() {
    let (fixture, mut ws) = event_workspace();
    for (guard, wanted, absent) in [
        ("IsDuplicityVersion()", "demo:server", "demo:client"),
        ("not IsDuplicityVersion()", "demo:client", "demo:server"),
    ] {
        let text = format!("if {guard} then\nTriggerEvent('')\nend");
        let doc = fixture.document(&mut ws, "demo/caller.lua", &text);
        let found = labels(&ws, &doc);
        assert!(found.iter().any(|name| name == wanted), "{found:?}");
        assert!(!found.iter().any(|name| name == absent), "{found:?}");
    }
    for (call, expected) in [("TriggerServerEvent", "serverValue"), ("TriggerEvent", "clientValue")] {
        let text = format!("{call}('demo:both', 1)");
        let doc = fixture.document(&mut ws, "demo/client.lua", &text);
        assert_eq!(payload(&ws, &doc).unwrap().last().unwrap(), expected);
    }
    // A client callback has no implicit player/source argument, even in a shared file.
    for (guard, expected) in [("IsDuplicityVersion()", "clientValue"), ("not IsDuplicityVersion()", "serverValue")] {
        let text = format!("if {guard} then\nlib.callback.await('demo:callback', false, 1)\nend");
        let doc = fixture.document(&mut ws, "demo/caller.lua", &text);
        let params = payload(&ws, &doc).unwrap();
        assert_eq!(params.len(), 3, "{params:?}");
        assert_eq!(params[2], expected);
    }
    let doc = fixture.document(&mut ws, "demo/client.lua", "TriggerEvent('demo:server', 1)");
    assert!(payload(&ws, &doc).is_none(), "an unreachable handler must not supply a signature");
}

#[test]
fn scan_reloads_manifests_and_removes_deleted_and_excluded_state() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "client_script '*.lua'");
    fixture.write("demo/main.lua", "Current = 1");
    fixture.write("demo/deleted.lua", "Deleted = 1");
    fixture.write("demo/skip/old.lua", "Excluded = 1");
    fixture.write("empty/fxmanifest.lua", "fx_version 'cerulean'");
    let mut ws = fixture.workspace("");
    assert!(ws.index.resource_by_name("empty").is_some());
    let main = fixture.0.join("demo/main.lua");
    assert_eq!(ws.index.file(ws.index.file_id(&main).unwrap()).unwrap().side, Some(Side::Client));
    fixture.write("demo/fxmanifest.lua", "server_script '*.lua'");
    fixture.write("qbxlint.toml", "exclude = ['demo/skip/**']");
    std::fs::remove_file(fixture.0.join("demo/deleted.lua")).unwrap();
    std::fs::remove_file(fixture.0.join("empty/fxmanifest.lua")).unwrap();
    ws.scan();
    assert_eq!(ws.index.file(ws.index.file_id(&main).unwrap()).unwrap().side, Some(Side::Server));
    assert!(ws.index.file_id(&fixture.0.join("demo/deleted.lua")).is_none());
    assert!(ws.index.file_id(&fixture.0.join("demo/skip/old.lua")).is_none());
    assert!(ws.index.resource_by_name("empty").is_none());
    std::fs::remove_file(fixture.0.join("demo/fxmanifest.lua")).unwrap();
    ws.scan();
    assert!(ws.index.resources.is_empty());
    let entry = ws.index.file(ws.index.file_id(&main).unwrap()).unwrap();
    assert!(entry.resource.is_none());
    assert!(entry.side.is_none());
}

#[test]
fn configured_imports_define_globals_by_side_and_follow_the_config() {
    let fixture = Fixture::new();
    fixture.write(
        "qbxlint.toml",
        "[[overrides]]\nfiles = ['[[]lib[]]/**']\n\
         [overrides.imports]\nshared = ['@lib/shared/**.lua']\nclient = ['@lib/client/*.lua']\n",
    );
    fixture.write("lib/fxmanifest.lua", "files { 'shared/**.lua', 'client/*.lua' }");
    fixture.write("lib/shared/deep/api.lua", "SharedApi = {}\n");
    fixture.write("lib/client/api.lua", "function ClientApi() end\n");
    fixture.write("[lib]/shop/fxmanifest.lua", "client_script 'client.lua'\nserver_script 'server.lua'\n");
    fixture.write("other/fxmanifest.lua", "client_script 'client.lua'\n");
    let undefined = |ws: &mut Workspace, relative: &str| -> Vec<String> {
        let doc = fixture.document(ws, relative, "print(SharedApi, ClientApi)\n");
        let found = diagnostics::diagnostics(ws, &doc, &[], &ws.crossrefs());
        let undefined = found.into_iter().filter(|d| d.code == Some(NumberOrString::String("undefined-global".into())));
        undefined.map(|d| d.message.split('\'').nth(1).unwrap().to_string()).collect()
    };

    let mut ws = fixture.workspace("");
    assert_eq!(undefined(&mut ws, "[lib]/shop/client.lua"), Vec::<String>::new());
    assert_eq!(undefined(&mut ws, "[lib]/shop/server.lua"), ["ClientApi"]);
    assert_eq!(undefined(&mut ws, "other/client.lua"), ["SharedApi", "ClientApi"]);
    let file = |ws: &Workspace, relative: &str| ws.index.file_id(&fixture.0.join(relative)).unwrap();
    let client = file(&ws, "[lib]/shop/client.lua");
    assert_eq!(ws.index.globals_named("ClientApi", client).len(), 1, "definitions reach the imported file");
    assert_eq!(ws.index.globals_named("ClientApi", file(&ws, "[lib]/shop/server.lua")).len(), 0);

    fixture.write("qbxlint.toml", "");
    ws.scan();
    assert_eq!(undefined(&mut ws, "[lib]/shop/client.lua"), ["SharedApi", "ClientApi"]);
    assert_eq!(ws.index.globals_named("ClientApi", file(&ws, "[lib]/shop/client.lua")).len(), 0);
}

#[test]
fn scan_rediscovers_missing_external_dependencies_and_prunes_removed_libraries() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "shared_script '@external/init.lua'");
    fixture.write("demo/main.lua", "print(External)");
    let mut ws = fixture.workspace("demo");
    assert!(ws.index.resource_by_name("external").is_none());
    fixture.write("external/fxmanifest.lua", "shared_script 'init.lua'");
    fixture.write("external/init.lua", "External = true");
    ws.scan();
    let (_, resource) = ws.index.resource_by_name("demo").unwrap();
    assert_eq!(resource.imports.len(), 1);
    assert!(ws.index.resource_by_name("external").is_some());
    fixture.write("library/fxmanifest.lua", "shared_script 'library.lua'");
    fixture.write("library/library.lua", "Library = true");
    ws.library.push(fixture.0.join("library"));
    ws.scan();
    assert!(ws.index.resource_by_name("library").is_some());
    ws.library.clear();
    fixture.write("demo/fxmanifest.lua", "shared_script 'main.lua'");
    ws.scan();
    assert!(ws.index.resource_by_name("external").is_none());
    assert!(ws.index.resource_by_name("library").is_none());
}

struct Client {
    connection: Connection,
    server: Option<JoinHandle<()>>,
    next: i32,
}

impl Client {
    fn start(root: &Path) -> Self {
        let (server, connection) = Connection::memory();
        let server = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                qbx_lua_ls::server::run_connection(server).unwrap();
            })
            .unwrap();
        let mut client = Self { connection, server: Some(server), next: 0 };
        client.request("initialize", json!({"processId": null, "rootUri": path_to_uri(root), "capabilities": {}}));
        client.notify("initialized", json!({}));
        client
    }

    fn notify(&self, method: &str, params: Value) {
        self.connection.sender.send(Message::Notification(Notification::new(method.to_string(), params))).unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = RequestId::from(self.next);
        self.connection.sender.send(Message::Request(Request::new(id.clone(), method.to_string(), params))).unwrap();
        loop {
            match self.connection.receiver.recv_timeout(Duration::from_secs(20)).expect("server did not respond") {
                Message::Response(response) if response.id == id => {
                    assert!(response.error.is_none(), "{:?}", response.error);
                    return response.result.unwrap_or(Value::Null);
                }
                Message::Request(request) => {
                    self.connection.sender.send(Message::Response(Response::new_ok(request.id, Value::Null))).unwrap();
                }
                _ => {}
            }
        }
    }

    fn open(&self, uri: &Url, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "languageId": "lua", "version": 1, "text": text
            }}),
        );
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        self.server.take().unwrap().join().unwrap();
    }
}

#[test]
fn new_glob_imports_are_linked_for_closed_open_and_unsaved_documents() {
    for (open, saved, watched) in [(false, true, true), (true, true, true), (true, false, false)] {
        let fixture = Fixture::new();
        fixture.write("qbxlint.toml", "[imports]\nclient = ['@lib/*.lua']\n");
        fixture.write("lib/fxmanifest.lua", "files { '*.lua' }\n");
        fixture.write("lib/old.lua", "OldApi = true\n");
        fixture.write("shop/fxmanifest.lua", "client_script 'client.lua'\n");
        fixture.write("shop/client.lua", "print(NewApi)\n");
        let mut client = Client::start(&fixture.0);
        // Finish the initial scan before introducing the imported file.
        client.request("qbx/status", Value::Null);
        let caller = path_to_uri(&fixture.0.join("shop/client.lua"));
        let imported = path_to_uri(&fixture.0.join("lib/new.lua"));
        client.open(&caller, "print(NewApi)\n");
        if saved {
            fixture.write("lib/new.lua", "NewApi = true\n");
        }
        if open {
            client.open(&imported, "NewApi = true\n");
        }
        if watched {
            client.notify("workspace/didChangeWatchedFiles", json!({"changes": [{"uri": imported, "type": 1}]}));
        }
        let definitions = client.request(
            "textDocument/definition",
            json!({
                "textDocument": {"uri": caller}, "position": {"line": 0, "character": 8}
            }),
        );
        let definitions = definitions.as_array().expect("new glob import must have a definition");
        assert!(definitions.iter().any(|definition| definition["uri"] == imported.as_str()), "{definitions:?}");
    }
}

#[test]
fn configured_exact_imports_respect_exclusions() {
    let fixture = Fixture::new();
    fixture.write("qbxlint.toml", "exclude = ['lib/api.lua']\n[imports]\nclient = ['@lib/api.lua']\n");
    fixture.write("lib/fxmanifest.lua", "files { 'api.lua' }\n");
    fixture.write("lib/api.lua", "ExcludedApi = true\n");
    fixture.write("shop/fxmanifest.lua", "client_script 'client.lua'\n");
    fixture.write("shop/client.lua", "print(ExcludedApi)\n");
    let ws = fixture.workspace("");
    let caller = ws.index.file_id(&fixture.0.join("shop/client.lua")).unwrap();
    let resource = ws.index.file(caller).unwrap().resource.unwrap();
    assert!(!ws.resource_env(resource).defines("ExcludedApi", Some(Side::Client)));
    assert!(ws.index.globals_named("ExcludedApi", caller).is_empty());
}

#[test]
fn open_documents_alone_are_indexed_again_for_the_classes_they_declare() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "client_script '*.lua'\n");
    let mut ws = fixture.workspace("demo");
    // What `Demo{name}:chain()` returns, as the index holds it after indexing the file once.
    let mut chained = |name: &str, document: bool| -> String {
        let path = fixture.0.join(format!("demo/{name}.lua"));
        let text = format!(
            "---@class Demo.{name}\nDemo{name} = {{}}\n\n---@return self\nfunction Demo{name}:chain() return self end\n"
        );
        let doc = Document::new(path_to_uri(&path), path, 1, text);
        let file = match document {
            true => ws.index_document(&doc.path, &doc.text, &doc.chunk, &doc.resolution, &mut Default::default()),
            false => ws.index_parsed(&doc.path, FileOrigin::Workspace, &doc.text, &doc.chunk, &doc.resolution),
        };
        let index = &ws.index.file(file).unwrap().index;
        let chain = index.members.iter().find(|member| member.symbol.name == "chain").unwrap();
        chain.symbol.ty.as_fun().unwrap().returns[0].to_string()
    };
    assert_eq!(chained("Opened", true), "Demo.Opened");
    // The scan reads every file again once it has read them all, so each of its passes reads a
    // file once.
    assert_ne!(chained("Scanned", false), "Demo.Scanned");
}

/// What the member `name` of a table returns, as the index holds it.
fn returned(ws: &Workspace, path: &Path, name: &str) -> qbx_lua_ls::types::Type {
    let index = &ws.index.file(ws.index.file_id(path).unwrap()).unwrap().index;
    let member = index.members.iter().find(|member| member.symbol.name == name).unwrap();
    member.symbol.ty.as_fun().unwrap().returns[0].clone()
}

#[test]
fn types_that_functions_infer_from_themselves_stop_growing() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "shared_script '*.lua'\n");
    // A function that returns what it returns itself reads its own call as unknown.
    fixture.write("demo/wrap.lua", "Wrap = {}\n\nfunction Wrap.wrap()\n    return { Wrap.wrap() }\nend\n");
    // Functions that return what each other return grow to the depth that the index keeps.
    fixture.write("demo/a.lua", "A = {}\n\nfunction A.f()\n    return { B.g() }\nend\n");
    fixture.write("demo/b.lua", "B = {}\n\nfunction B.g()\n    return { A.f() }\nend\n");
    let mut ws = fixture.workspace("demo");
    let path = |name: &str| fixture.0.join("demo").join(name);
    for _ in 0..20 {
        for name in ["wrap.lua", "a.lua", "b.lua"] {
            ws.index_path(&path(name), FileOrigin::Workspace, None);
        }
    }
    assert_eq!(returned(&ws, &path("wrap.lua"), "wrap").to_string(), "unknown[]");
    // The function type nests 8 levels deep, of which its own takes one.
    let f = returned(&ws, &path("a.lua"), "f");
    assert_eq!(f.depth(), 7, "{f}");
}

/// What the index holds for each file, and the members of `owner` in the order lookups from
/// `reader` find them.
fn index_state(ws: &Workspace, owner: &str, reader: &Path) -> Vec<String> {
    let files = ws.index.files().filter(|(_, file)| file.origin != FileOrigin::Stub);
    let mut state: Vec<String> = files.map(|(_, file)| format!("{:?}", file.index)).collect();
    let reader = ws.index.file_id(reader).unwrap();
    state.extend(ws.index.members_of(owner, reader).into_iter().map(|(_, member)| member.name.to_string()));
    state
}

#[test]
fn a_scan_leaves_what_indexing_files_again_in_any_order_would() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "shared_script '*.lua'\n");
    // What `AddBlip` returns comes from `utils.lua`, indexed after it, which takes it from a
    // function beside it in turn. Indexing every file twice is not enough to know it.
    fixture.write("demo/blips.lua", "function lib.AddBlip()\n    return lib.Key()\nend\n");
    fixture.write("demo/init.lua", "lib = {}\n");
    fixture.write(
        "demo/utils.lua",
        "function lib.Key()\n    return lib.String()\nend\n\nfunction lib.String()\n    return ''\nend\n",
    );
    // Functions that return what each other return settle at the depth that the index keeps.
    fixture.write("demo/x.lua", "X = {}\n\nfunction X.f()\n    return { Y.g() }\nend\n");
    fixture.write("demo/y.lua", "Y = {}\n\nfunction Y.g()\n    return { X.f() }\nend\n");
    let mut ws = fixture.workspace("demo");
    let blips = fixture.0.join("demo/blips.lua");
    assert_eq!(returned(&ws, &blips, "AddBlip"), qbx_lua_ls::types::Type::String);

    // Indexing a file again, as opening and closing it does, leaves it and the order of what
    // several files set as the scan did.
    let scanned = index_state(&ws, "lib", &blips);
    let files = ["blips.lua", "init.lua", "utils.lua", "x.lua", "y.lua"].map(|name| fixture.0.join("demo").join(name));
    for path in files.iter().chain(files.iter().rev()) {
        ws.index_path(path, FileOrigin::Workspace, None);
        assert_eq!(index_state(&ws, "lib", &blips), scanned, "after indexing {} again", path.display());
    }
}

#[test]
fn manual_reindex_restores_unsaved_documents_and_their_new_file_ids() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "client_script '*.lua'");
    fixture.write("demo/a_deleted.lua", "Old = true");
    fixture.write("demo/main.lua", "DiskOnly = 1");
    let mut client = Client::start(&fixture.0);
    let main_uri = path_to_uri(&fixture.0.join("demo/main.lua"));
    let scratch_uri = path_to_uri(&fixture.0.join("demo/scratch.lua"));
    let manifest_uri = path_to_uri(&fixture.0.join("demo/fxmanifest.lua"));
    client.open(&main_uri, "UnsavedOnly = 42\nprint(UnsavedOnly)");
    client.open(&scratch_uri, "ScratchOnly = true");
    client.open(&manifest_uri, "client_script '*.lua'");
    client.request("qbx/status", Value::Null);
    std::fs::remove_file(fixture.0.join("demo/a_deleted.lua")).unwrap();
    fixture.write("demo/fxmanifest.lua", "server_script '*.lua'");
    client.request("qbx/reindex", Value::Null);
    assert_eq!(client.request("qbx/fileInfo", json!({"uri": main_uri}))["side"], "server");
    assert_eq!(client.request("qbx/fileInfo", json!({"uri": scratch_uri}))["side"], "server");
    for name in ["UnsavedOnly", "ScratchOnly"] {
        let symbols = client.request("workspace/symbol", json!({"query": name}));
        assert!(symbols.as_array().unwrap().iter().any(|s| s["name"] == name), "{symbols}");
    }
    for name in ["DiskOnly", "Old"] {
        let symbols = client.request("workspace/symbol", json!({"query": name}));
        assert!(symbols.as_array().unwrap().is_empty(), "{symbols}");
    }
    let hover = client.request(
        "textDocument/hover",
        json!({
            "textDocument": {"uri": main_uri}, "position": {"line": 1, "character": 9}
        }),
    );
    assert!(hover["contents"]["value"].as_str().unwrap().contains("42"), "{hover}");
    client.notify("textDocument/didClose", json!({"textDocument": {"uri": scratch_uri}}));
    client.request("qbx/reindex", Value::Null);
    let symbols = client.request("workspace/symbol", json!({"query": "ScratchOnly"}));
    assert!(symbols.as_array().unwrap().is_empty(), "{symbols}");
}

/// The file names and lines `textDocument/definition` finds for the `needle` in `text` of `uri`.
fn definitions(client: &mut Client, uri: &Url, text: &str, needle: &str) -> Vec<(String, u64)> {
    let offset = text.find(needle).unwrap();
    let line = text[..offset].matches('\n').count();
    let character = offset - text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let result = client.request(
        "textDocument/definition",
        json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}),
    );
    let locations = result.as_array().cloned().unwrap_or_default();
    let place = |location: &Value| {
        let file = location["uri"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
        (file, location["range"]["start"]["line"].as_u64().unwrap())
    };
    locations.iter().map(place).collect()
}

#[test]
fn closed_files_follow_the_globals_of_documents_once_saved_or_closed() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "shared_scripts { 'types.lua', 'methods.lua' }\nclient_script 'client.lua'\n");
    fixture.write("demo/types.lua", "---@class Test.Player\nPlayers = {}\n");
    fixture.write("demo/methods.lua", "function Players:rename(name)\n    self.nickname = name\nend\n");
    fixture.write("demo/client.lua", "");
    let mut client = Client::start(&fixture.0);
    let types = path_to_uri(&fixture.0.join("demo/types.lua"));
    let caller = path_to_uri(&fixture.0.join("demo/client.lua"));
    let text =
        "---@type Test.User\nlocal user\n---@type Test.Admin\nlocal admin\nprint(user.nickname, admin.nickname)\n";
    client.open(&types, "---@class Test.Player\nPlayers = {}\n");
    client.open(&caller, text);
    let methods = || vec![("methods.lua".to_string(), 1)];
    let change = |client: &Client, version: i32, class: &str| {
        let text = format!("---@class {class}\nPlayers = {{}}\n");
        client.notify(
            "textDocument/didChange",
            json!({"textDocument": {"uri": types, "version": version}, "contentChanges": [{"text": text}]}),
        );
    };

    change(&client, 2, "Test.User");
    assert_eq!(definitions(&mut client, &caller, text, "nickname,"), [], "closed files wait for a save");
    fixture.write("demo/types.lua", "---@class Test.User\nPlayers = {}\n");
    client.notify("textDocument/didSave", json!({"textDocument": {"uri": types}}));
    assert_eq!(definitions(&mut client, &caller, text, "nickname,"), methods());

    // Saving any document passes on what the others changed without saving.
    change(&client, 3, "Test.Admin");
    client.notify("textDocument/didSave", json!({"textDocument": {"uri": caller}}));
    assert_eq!(definitions(&mut client, &caller, text, "nickname)"), methods());

    // Closing without saving takes the declarations back to the file on disk.
    client.notify("textDocument/didClose", json!({"textDocument": {"uri": types}}));
    assert_eq!(definitions(&mut client, &caller, text, "nickname,"), methods());
    assert_eq!(definitions(&mut client, &caller, text, "nickname)"), []);
}

#[test]
fn closed_files_follow_globals_that_change_on_disk_in_any_order() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "shared_script '*.lua'\n");
    fixture.write("demo/query.lua", "");
    let mut client = Client::start(&fixture.0);
    client.request("qbx/status", Value::Null);
    // The method on `Mid` is indexed before `Mid`, which is indexed before the `Base` it is typed from.
    let files = [
        ("demo/c.lua", "function Mid:touch()\n    self.touched = true\nend\n"),
        ("demo/a.lua", "Mid = Base\n"),
        ("demo/b.lua", "---@class Test.Base\nBase = {}\n"),
    ];
    for (file, text) in files {
        fixture.write(file, text);
    }
    let changes: Vec<Value> =
        files.iter().map(|(file, _)| json!({"uri": path_to_uri(&fixture.0.join(file)), "type": 1})).collect();
    client.notify("workspace/didChangeWatchedFiles", json!({"changes": changes}));
    let query = path_to_uri(&fixture.0.join("demo/query.lua"));
    let text = "print(Base.touched)\n";
    client.open(&query, text);
    assert_eq!(definitions(&mut client, &query, text, "touched"), [("c.lua".to_string(), 1)]);

    // Without `Base`, neither `Mid` nor the method on it has a class any more: the method makes `Mid`
    // a table of its own, as a fresh scan reads it.
    let deleted = path_to_uri(&fixture.0.join("demo/b.lua"));
    std::fs::remove_file(fixture.0.join("demo/b.lua")).unwrap();
    client.notify("workspace/didChangeWatchedFiles", json!({"changes": [{"uri": deleted, "type": 3}]}));
    let symbols = client.request("workspace/symbol", json!({"query": "touched"}));
    assert_eq!(symbols[0]["containerName"], "Mid", "{symbols}");
    let fresh = Client::start(&fixture.0).request("workspace/symbol", json!({"query": "touched"}));
    assert_eq!(symbols, fresh);
}

#[test]
fn closed_files_follow_what_they_declare_themselves_once_changed_on_disk() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "shared_script '*.lua'\n");
    fixture.write("demo/init.lua", "Util = {}\n");
    fixture.write("demo/utils.lua", "function Util.Key()\n    return 1\nend\n");
    fixture.write("demo/query.lua", "");
    let mut client = Client::start(&fixture.0);
    client.request("qbx/status", Value::Null);
    // `Key` returns what `String` beside it returns, which the file is indexed without at first.
    let utils = "function Util.Key()\n    return Util.String()\nend\n\nfunction Util.String()\n    return ''\nend\n";
    fixture.write("demo/utils.lua", utils);
    let changed = path_to_uri(&fixture.0.join("demo/utils.lua"));
    client.notify("workspace/didChangeWatchedFiles", json!({"changes": [{"uri": changed, "type": 2}]}));
    let query = path_to_uri(&fixture.0.join("demo/query.lua"));
    let text = "Value = Util.Key()\n";
    client.open(&query, text);
    assert_eq!(hover(&mut client, &query, text, "Value"), "(global) Value: string");
}

/// The first line `textDocument/hover` shows for the `needle` in `text` of `uri`.
fn hover(client: &mut Client, uri: &Url, text: &str, needle: &str) -> String {
    let offset = text.find(needle).unwrap();
    let line = text[..offset].matches('\n').count();
    let character = offset - text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let result = client.request(
        "textDocument/hover",
        json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}),
    );
    let value = result["contents"]["value"].as_str().unwrap_or_default();
    value.lines().find(|line| !line.is_empty() && !line.starts_with("```")).unwrap_or_default().to_string()
}

#[test]
fn closed_files_follow_the_classes_members_exports_and_modules_they_read() {
    let fixture = Fixture::new();
    let types = "---@class Test.Store\n---@field admin Test.Admin\n\n---@class Test.Admin\n\n---@class Test.User\n";
    fixture.write("demo/fxmanifest.lua", "shared_scripts { 'types.lua', 'players.lua', 'readers.lua', 'query.lua' }\n");
    fixture.write("demo/types.lua", types);
    fixture.write("demo/players.lua", "---@type Test.Store\nStore = {}\nPlayers = {}\nPlayers.admin = 5\n");
    fixture.write("demo/mod.lua", "return { value = 1 }\n");
    fixture.write("shop/fxmanifest.lua", "server_script 'server.lua'\n");
    fixture.write("shop/server.lua", "exports('GetThing', function() return 1 end)\n");
    // Resources are found by name whatever its case.
    let readers = "CurrentAdmin = Store.admin\nBoss = Players.admin\nThing = exports.Shop:GetThing()\n";
    fixture.write("demo/readers.lua", &format!("{readers}Value = require('mod').value\n"));
    fixture.write("demo/query.lua", "");
    let mut client = Client::start(&fixture.0);
    let query = path_to_uri(&fixture.0.join("demo/query.lua"));
    let text = "print(CurrentAdmin, Boss, Thing, Value)\n";
    client.open(&query, text);
    let hovers = |client: &mut Client| {
        ["CurrentAdmin", "Boss", "Thing", "Value"].map(|global| hover(client, &query, text, global))
    };
    assert_eq!(
        hovers(&mut client),
        [
            "(global) CurrentAdmin: Test.Admin",
            "(global) Boss: integer",
            "(global) Thing: integer",
            "(global) Value: integer"
        ]
    );

    // What the readers stored follows the `@field` of a class, the export of another resource and
    // what a module returns, none of them a global.
    let changed = [
        ("demo/types.lua", types.replace("admin Test.Admin", "admin Test.User")),
        ("shop/server.lua", "exports('GetThing', function() return 'thing' end)\n".to_string()),
        ("demo/mod.lua", "return { value = 'value' }\n".to_string()),
    ];
    for (file, text) in &changed {
        fixture.write(file, text);
    }
    let changes: Vec<Value> =
        changed.iter().map(|(file, _)| json!({"uri": path_to_uri(&fixture.0.join(file)), "type": 2})).collect();
    client.notify("workspace/didChangeWatchedFiles", json!({"changes": changes}));
    assert_eq!(
        hovers(&mut client),
        [
            "(global) CurrentAdmin: Test.User",
            "(global) Boss: integer",
            "(global) Thing: string",
            "(global) Value: string"
        ]
    );
    // Also when the export alone changes.
    fixture.write("shop/server.lua", "exports('GetThing', function() return true end)\n");
    let server = path_to_uri(&fixture.0.join("shop/server.lua"));
    client.notify("workspace/didChangeWatchedFiles", json!({"changes": [{"uri": server, "type": 2}]}));
    assert_eq!(hover(&mut client, &query, text, "Thing"), "(global) Thing: boolean");

    // A member that a document sets follows once it is saved.
    let players = path_to_uri(&fixture.0.join("demo/players.lua"));
    let saved = "---@type Test.Store\nStore = {}\nPlayers = {}\nPlayers.admin = 'admin'\n";
    client.open(&players, "---@type Test.Store\nStore = {}\nPlayers = {}\nPlayers.admin = 5\n");
    client.notify(
        "textDocument/didChange",
        json!({"textDocument": {"uri": players, "version": 2}, "contentChanges": [{"text": saved}]}),
    );
    assert_eq!(hover(&mut client, &query, text, "Boss"), "(global) Boss: integer", "closed files wait for a save");
    fixture.write("demo/players.lua", saved);
    client.notify("textDocument/didSave", json!({"textDocument": {"uri": players}}));
    assert_eq!(hover(&mut client, &query, text, "Boss"), "(global) Boss: string");
}

#[test]
fn new_files_read_what_they_declare_themselves_from_the_start() {
    let fixture = Fixture::new();
    fixture.write("demo/fxmanifest.lua", "shared_script '*.lua'\n");
    fixture.write("demo/query.lua", "");
    let mut client = Client::start(&fixture.0);
    // A document that is not on disk yet, typed from the globals it declares without annotations.
    let fresh = path_to_uri(&fixture.0.join("demo/fresh.lua"));
    let text = "\
Shop = {}
Shop.items = { apple = 1 }

function Shop.count()
    return Shop.items.apple
end

Animal = {}
Animal.__index = Animal

function Animal.new()
    return setmetatable({}, Animal)
end

function Animal:speak()
    return 'hi'
end

local count = Shop.count()
local said = Animal.new():speak()
print(count, said)
";
    client.open(&fresh, text);
    assert_eq!(hover(&mut client, &fresh, text, "count ="), "local count: integer");
    assert_eq!(hover(&mut client, &fresh, text, "said ="), "local said: string");

    // A file created on disk, which the files that read it follow.
    fixture.write("demo/created.lua", "Stock = { pears = 2 }\n\nfunction GetPears()\n    return Stock.pears\nend\n");
    let created = path_to_uri(&fixture.0.join("demo/created.lua"));
    client.notify("workspace/didChangeWatchedFiles", json!({"changes": [{"uri": created, "type": 1}]}));
    let query = path_to_uri(&fixture.0.join("demo/query.lua"));
    let text = "local pears = GetPears()\nprint(pears)\n";
    client.open(&query, text);
    assert_eq!(hover(&mut client, &query, text, "pears ="), "local pears: integer");
}
