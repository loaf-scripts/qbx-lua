use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::Url;
use serde_json::{json, Value};

struct Client {
    connection: Connection,
    server: Option<JoinHandle<()>>,
    next_id: i32,
    diagnostics: HashMap<String, Value>,
    registrations: Vec<Value>,
    logs: Vec<String>,
    root: PathBuf,
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/resources")
}

impl Client {
    fn start(root: PathBuf) -> Self {
        Self::start_with_capabilities(
            root,
            json!({
                "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } },
                "textDocument": { "completion": { "completionItem": { "snippetSupport": true } } }
            }),
        )
    }

    fn start_with_capabilities(root: PathBuf, capabilities: Value) -> Self {
        Self::start_with_options(root, capabilities, Value::Null)
    }

    fn start_with_library(root: PathBuf, library: &Path) -> Self {
        Self::start_with_options(root, json!({}), json!({ "library": [library] }))
    }

    fn start_with_options(root: PathBuf, capabilities: Value, options: Value) -> Self {
        let (server_side, client_side) = Connection::memory();
        let server = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || qbx_lua_ls::server::run_connection(server_side).expect("server failed"))
            .unwrap();
        let mut client = Client {
            connection: client_side,
            server: Some(server),
            next_id: 0,
            diagnostics: HashMap::new(),
            registrations: Vec::new(),
            logs: Vec::new(),
            root,
        };
        let root_uri = Url::from_file_path(&client.root).unwrap();
        let result = client.request(
            "initialize",
            json!({ "processId": null, "rootUri": root_uri, "capabilities": capabilities, "initializationOptions": options, "workspaceFolders": [{ "uri": root_uri, "name": "fixture" }] }),
        );
        assert!(result["capabilities"]["completionProvider"].is_object());
        client.notify("initialized", json!({}));
        client
    }

    fn notify(&self, method: &str, params: Value) {
        self.connection.sender.send(Message::Notification(Notification { method: method.into(), params })).unwrap();
    }

    fn handle_incoming(&mut self, message: Message) -> Option<(RequestId, Value)> {
        match message {
            Message::Response(response) => {
                assert!(response.error.is_none(), "server returned an error: {:?}", response.error);
                return Some((response.id, response.result.unwrap_or(Value::Null)));
            }
            Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                let uri = n.params["uri"].as_str().unwrap().to_string();
                self.diagnostics.insert(uri, n.params["diagnostics"].clone());
            }
            Message::Notification(n) if n.method == "window/logMessage" => {
                self.logs.push(n.params["message"].as_str().unwrap_or_default().to_string());
            }
            Message::Request(request) => {
                if request.method == "client/registerCapability" {
                    self.registrations.extend(request.params["registrations"].as_array().unwrap().iter().cloned());
                }
                let reply = lsp_server::Response { id: request.id, result: Some(Value::Null), error: None };
                self.connection.sender.send(Message::Response(reply)).unwrap();
            }
            Message::Notification(_) => {}
        }
        None
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.connection
            .sender
            .send(Message::Request(Request { id: id.clone(), method: method.into(), params }))
            .unwrap();
        loop {
            let message =
                self.connection.receiver.recv_timeout(Duration::from_secs(20)).expect("server did not answer");
            if let Some((response_id, result)) = self.handle_incoming(message) {
                if response_id == id {
                    return result;
                }
            }
        }
    }

    fn request_error(&mut self, method: &str, params: Value) -> lsp_server::ResponseError {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.connection
            .sender
            .send(Message::Request(Request { id: id.clone(), method: method.into(), params }))
            .unwrap();
        loop {
            let message =
                self.connection.receiver.recv_timeout(Duration::from_secs(20)).expect("server did not answer");
            if let Message::Response(response) = &message {
                if response.id == id {
                    return response.error.clone().expect("expected request validation error");
                }
            }
            self.handle_incoming(message);
        }
    }

    fn uri(&self, relative: &str) -> Url {
        Url::from_file_path(self.root.join(relative)).unwrap()
    }

    fn open(&mut self, relative: &str) -> String {
        let text = std::fs::read_to_string(self.root.join(relative)).unwrap().replace("\r\n", "\n");
        self.open_with(relative, &text);
        text
    }

    fn open_with(&mut self, relative: &str, text: &str) {
        let uri = self.uri(relative);
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": "lua", "version": 1, "text": text } }),
        );
    }

    fn change(&mut self, relative: &str, version: i32, text: &str) {
        let uri = self.uri(relative);
        self.notify(
            "textDocument/didChange",
            json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] }),
        );
    }

    /// Diagnostics are published once the server is idle, so round-trip a request first.
    fn diagnostics_for(&mut self, relative: &str) -> Vec<(String, u64)> {
        self.request("qbx/status", Value::Null);
        while let Ok(message) = self.connection.receiver.recv_timeout(Duration::from_millis(300)) {
            self.handle_incoming(message);
        }
        let uri = self.uri(relative).to_string();
        let list = self.diagnostics.get(&uri).cloned().unwrap_or(json!([]));
        list.as_array()
            .unwrap()
            .iter()
            .map(|d| (d["code"].as_str().unwrap().to_string(), d["range"]["start"]["line"].as_u64().unwrap()))
            .collect()
    }

    fn position_params(&self, relative: &str, line: u32, character: u32) -> Value {
        json!({ "textDocument": { "uri": self.uri(relative) }, "position": { "line": line, "character": character } })
    }

    fn completion_labels(&mut self, relative: &str, line: u32, character: u32) -> Vec<String> {
        let result = self.request("textDocument/completion", self.position_params(relative, line, character));
        result["items"]
            .as_array()
            .map(|items| items.iter().map(|i| i["label"].as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    }

    fn hover_text(&mut self, relative: &str, line: u32, character: u32) -> String {
        let result = self.request("textDocument/hover", self.position_params(relative, line, character));
        result["contents"]["value"].as_str().unwrap_or_default().to_string()
    }

    /// The hover asked for at `level`, or at the server's own level without one, and the `maxLevel`
    /// it is answered with.
    fn hover_at_level(&mut self, relative: &str, line: u32, character: u32, level: Option<u32>) -> (String, u64) {
        let mut params = self.position_params(relative, line, character);
        if let Some(level) = level {
            params["level"] = json!(level);
        }
        let result = self.request("textDocument/hover", params);
        let text = result["contents"]["value"].as_str().unwrap_or_default().to_string();
        (text, result["maxLevel"].as_u64().expect("maxLevel"))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.next_id += 1;
        let shutdown = Request { id: RequestId::from(self.next_id), method: "shutdown".into(), params: Value::Null };
        let _ = self.connection.sender.send(Message::Request(shutdown));
        let _ = self.connection.receiver.recv_timeout(Duration::from_secs(5));
        self.notify("exit", Value::Null);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// Finds `needle` in `text` and returns the position `delta` characters into it.
fn pos(text: &str, needle: &str, delta: u32) -> (u32, u32) {
    let offset = text.find(needle).unwrap_or_else(|| panic!("{needle:?} not found"));
    let line = text[..offset].matches('\n').count() as u32;
    let col = (offset - text[..offset].rfind('\n').map_or(0, |i| i + 1)) as u32;
    (line, col + delta)
}

const CLIENT: &str = "myresource/client/main.lua";
const SERVER: &str = "myresource/server/main.lua";

#[test]
fn reference_search_is_offline_paginated_and_available_without_open_documents() {
    let mut client = Client::start_with_capabilities(fixture_root(), json!({}));
    let before = client.request("qbx/status", Value::Null);
    let first = client.request("qbx/referenceSearch", Value::Null);
    assert_eq!(first["items"].as_array().unwrap().len(), 50);
    assert_eq!(first["offset"], 0);
    assert_eq!(first["limit"], 50);
    assert!(first["total"].as_u64().unwrap() > 7000);
    let namespaces = first["namespaces"].as_array().unwrap();
    assert!(namespaces.iter().any(|value| value == "PAD"));
    assert!(namespaces.windows(2).all(|pair| pair[0].as_str() < pair[1].as_str()));
    assert!(first["items"].as_array().unwrap().iter().all(|item| item.get("documentation").is_none()));
    assert_eq!(first, client.request("qbx/referenceSearch", json!({})), "default search order is deterministic");
    let second = client.request("qbx/referenceSearch", json!({ "offset": 50 }));
    let first_ids: Vec<_> = first["items"].as_array().unwrap().iter().map(|item| &item["id"]).collect();
    assert!(second["items"].as_array().unwrap().iter().all(|item| !first_ids.contains(&&item["id"])));
    let capped = client.request("qbx/referenceSearch", json!({ "limit": 1000000 }));
    assert_eq!(capped["items"].as_array().unwrap().len(), 100);
    assert_eq!(capped["limit"], 100);
    let minimum = client.request("qbx/referenceSearch", json!({ "limit": 0 }));
    assert_eq!(minimum["limit"], 1);
    assert_eq!(minimum["items"].as_array().unwrap().len(), 1);
    let past_end = client.request("qbx/referenceSearch", json!({ "offset": u64::MAX }));
    assert!(past_end["items"].as_array().unwrap().is_empty());
    assert_eq!(past_end["offset"], past_end["total"]);
    let after = client.request("qbx/status", Value::Null);
    assert_eq!(before, after, "reference requests do not open files or change the workspace index");
}

#[test]
fn reference_search_matches_names_hashes_aliases_ids_and_default_bindings() {
    let mut client = Client::start(fixture_root());
    for (query, kind, expected) in [
        ("GetEntityCoords", "native", "native:GetEntityCoords"),
        ("gEtEnTiTyCoOrDs", "native", "native:GetEntityCoords"),
        ("SET_PED_CONFIG_FLAG", "native", "native:SetPedConfigFlag"),
        ("0x3FEF770D40960D5A", "native", "native:GetEntityCoords"),
        ("3fef770d40960d5a", "native", "native:GetEntityCoords"),
        ("N_0x580417101DDB492F", "native", "native:IsControlJustPressed"),
        ("N_0xe8a25867fba3b05e", "native", "native:SetControlNormal"),
        ("GetLastInputMethod", "native", "native:IsUsingKeyboard"),
        ("38", "control", "control:38"),
        ("input_pickup", "control", "control:38"),
        ("input pickup", "control", "control:38"),
        ("pickup e", "control", "control:38"),
        ("51 DPAD RIGHT", "control", "control:51"),
        ("48", "pedFlag", "pedFlag:48"),
        ("BlockWeaponSwitching", "pedFlag", "pedFlag:48"),
    ] {
        let result = client.request("qbx/referenceSearch", json!({ "query": query, "kind": kind }));
        assert_eq!(result["items"][0]["id"], expected, "{query}: {result}");
    }
    let result = client.request("qbx/referenceSearch", json!({ "query": "38" }));
    assert_eq!(result["items"][0]["id"], "control:38", "exact IDs outrank substrings in hashes");
    assert_eq!(result["items"][1]["id"], "pedFlag:38");
    let result = client.request("qbx/referenceSearch", json!({ "query": "IsControl", "kind": "native" }));
    assert!(result["items"][0]["name"].as_str().unwrap().starts_with("IsControl"));
    let result = client.request("qbx/referenceSearch", json!({ "query": "not-a-real-native-or-control" }));
    assert_eq!(result["total"], 0);
    assert!(result["items"].as_array().unwrap().is_empty());
    let result = client.request("qbx/referenceSearch", json!({ "kind": "native" }));
    let canonical_count = qbx_fivem_data::natives().filter(|native| native.alias_of.is_none()).count();
    assert_eq!(result["total"], canonical_count, "default listings deduplicate documented aliases");
}

#[test]
fn reference_search_filters_side_namespace_and_catalog_without_conflating_them() {
    let mut client = Client::start(fixture_root());
    for side in ["client", "server", "shared"] {
        let result = client.request("qbx/referenceSearch", json!({ "side": side, "limit": 100 }));
        assert!(result["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["side"] == side || side != "shared" && item["side"] == "shared"));
        let shared = client
            .request("qbx/referenceSearch", json!({ "query": "GetEntityCoords", "side": side, "kind": "native" }));
        assert_eq!(shared["items"][0]["id"], "native:GetEntityCoords", "shared native remains available on {side}");
    }
    let result =
        client.request("qbx/referenceSearch", json!({ "kind": "native", "namespace": "pad", "side": "server" }));
    assert_eq!(result["total"], 0, "PAD natives are client-only");
    let result = client.request("qbx/referenceSearch", json!({ "kind": "native", "namespace": "PAD", "limit": 100 }));
    assert!(result["total"].as_u64().unwrap() > 20);
    assert!(result["items"].as_array().unwrap().iter().all(|item| item["namespace"] == "PAD"));
    let all = client.request("qbx/referenceSearch", json!({ "namespace": "PAD" }));
    assert_eq!(
        all["total"].as_u64().unwrap(),
        result["total"].as_u64().unwrap()
            + qbx_fivem_data::controls().count() as u64
            + qbx_fivem_data::ped_config_flags().count() as u64,
        "all-catalog search retains numeric catalogs when filtering native namespace"
    );
    for (kind, count) in
        [("control", qbx_fivem_data::controls().count()), ("pedFlag", qbx_fivem_data::ped_config_flags().count())]
    {
        let result = client.request("qbx/referenceSearch", json!({ "kind": kind, "namespace": "PED" }));
        assert_eq!(result["total"], count, "native namespace filters do not hide other catalogs");
        assert!(result["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] == kind && item["side"] == "client" && item.get("namespace").is_none()));
        let result = client.request("qbx/referenceSearch", json!({ "kind": kind, "side": "server" }));
        assert_eq!(result["total"], 0);
    }
}

#[test]
fn reference_details_return_documentation_and_safe_lua_insertion_text() {
    let mut client = Client::start(fixture_root());
    let detail = client.request("qbx/referenceDetail", json!({ "id": "native:GetEntityCoords" }));
    assert_eq!(detail["id"], "native:GetEntityCoords");
    assert_eq!(detail["kind"], "native");
    assert_eq!(detail["side"], "shared");
    assert_eq!(detail["namespace"], "ENTITY");
    assert_eq!(detail["hash"], "0x3FEF770D40960D5A");
    assert_eq!(detail["signature"], "function GetEntityCoords(entity: Entity, alive: boolean): vector3");
    assert_eq!(
        detail["parameters"],
        json!([{ "name": "entity", "type": "Entity" }, { "name": "alive", "type": "boolean" }])
    );
    assert_eq!(detail["returns"], json!(["vector3"]));
    assert!(!detail["documentation"].as_str().unwrap().is_empty());
    assert_eq!(detail["sourceUrl"], "https://docs.fivem.net/natives/?_0x3FEF770D40960D5A");
    assert_eq!(detail["copyText"], "GetEntityCoords");
    assert_eq!(detail["insertText"], "GetEntityCoords(entity, alive)");
    assert_eq!(detail["insertSnippet"], "GetEntityCoords(${1:entity}, ${2:alive})$0");
    let alias = client.request("qbx/referenceDetail", json!({ "id": "native:GetLastInputMethod" }));
    assert_eq!(alias["id"], "native:IsUsingKeyboard");
    let hash = client.request("qbx/referenceDetail", json!({ "id": "native:N_0x580417101DDB492F" }));
    assert_eq!(hash["id"], "native:IsControlJustPressed");
    let no_args = client.request("qbx/referenceDetail", json!({ "id": "native:PlayerPedId" }));
    assert_eq!(no_args["insertText"], "PlayerPedId()");
    assert_eq!(no_args["insertSnippet"], "PlayerPedId()$0");

    for (id, expected) in [
        ("control:38", "INPUT_PICKUP"),
        ("control:243", "`` ~ / ` ``"),
        ("control:360", "Not documented"),
        ("pedFlag:48", "CPED_CONFIG_FLAG_BlockWeaponSwitching"),
    ] {
        let detail = client.request("qbx/referenceDetail", json!({ "id": id }));
        assert_eq!(detail["id"], id);
        assert!(detail["documentation"].as_str().unwrap().contains(expected), "{detail}");
        let number = id.split_once(':').unwrap().1;
        assert_eq!(detail["copyText"], number);
        assert_eq!(detail["insertText"], number);
        assert!(detail.get("insertSnippet").is_none());
        assert!(detail.get("signature").is_none());
        if id.starts_with("pedFlag:") {
            assert!(detail["documentation"].as_str().unwrap().contains("Behavior is not documented"));
            assert!(detail["documentation"].as_str().unwrap().contains("potential names and hash collisions"));
        }
    }
    for id in [
        "unknown:1",
        "native:MissingNative",
        "native:PED",
        "control:9999",
        "control:-1",
        "control:038",
        "pedFlag:4294967296",
        "",
        "control:38:other",
    ] {
        assert_eq!(client.request("qbx/referenceDetail", json!({ "id": id })), Value::Null, "{id}");
    }
}

#[test]
fn reference_requests_reject_invalid_params_without_disrupting_lsp() {
    let mut client = Client::start(fixture_root());
    for params in [
        json!({ "query": "a".repeat(257) }),
        json!({ "namespace": "a".repeat(65) }),
        json!({ "offset": -1 }),
        json!({ "offset": 0.5 }),
        json!({ "limit": -1 }),
        json!({ "kind": "invalid" }),
        json!({ "side": "invalid" }),
        json!({ "query": 38 }),
        json!([]),
    ] {
        let error = client.request_error("qbx/referenceSearch", params);
        assert_eq!(error.code, -32602);
    }
    for params in
        [json!({ "id": "a".repeat(513) }), json!({ "id": 38 }), Value::Null, json!(["native:GetEntityCoords"])]
    {
        let error = client.request_error("qbx/referenceDetail", params);
        assert_eq!(error.code, -32602);
    }
    let text = "IsControlJustPressed(0, 38)";
    client.open_with(CLIENT, text);
    let (line, character) = pos(text, "38", 0);
    assert!(client.hover_text(CLIENT, line, character).contains("INPUT_PICKUP"));
    assert!(client.request("qbx/status", Value::Null)["natives"].as_u64().unwrap() > 7000);
}

#[test]
fn indexes_the_workspace_and_reports_status() {
    let mut client = Client::start(fixture_root());
    let status = client.request("qbx/status", Value::Null);
    assert_eq!(status["resources"], 5);
    assert!(status["files"].as_u64().unwrap() >= 10, "{status}");
}

#[test]
fn lists_every_rule_with_its_default_level() {
    let mut client = Client::start(fixture_root());
    let rules = client.request("qbx/rules", Value::Null);
    let rules = rules.as_array().unwrap();
    assert_eq!(rules.len(), qbx_lua_analysis::rules::RULES.len());
    let rule = |code: &str| rules.iter().find(|rule| rule["code"] == code).unwrap_or_else(|| panic!("{code}"));
    assert_eq!(
        rule("unused-local"),
        &json!({
            "code": "unused-local",
            "category": "suspicious",
            "default": "warning",
            "fixable": false,
            "summary": "A local variable is never read, or is declared with a table whose fields are set and never read.",
        })
    );
    assert_eq!(rule("shadowed-local")["default"], "off");
    assert_eq!(rule("fivem/citizen-prefix")["category"], "fivem");
}

#[test]
fn hover_shows_types_docs_and_natives() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);

    let (l, c) = pos(&text, "MyLib.round(1.2345", 7);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("function MyLib.round(value: number, decimals?: integer): number"), "{hover}");
    assert!(hover.contains("Rounds a number"), "{hover}");
    assert!(hover.contains("`value`: the value to round"), "{hover}");

    let (l, c) = pos(&text, "local garage", 7);
    assert!(client.hover_text(CLIENT, l, c).contains("local garage: Garage"));

    let (l, c) = pos(&text, "local count", 7);
    assert!(client.hover_text(CLIENT, l, c).contains("local count: integer"));

    let (l, c) = pos(&text, "ok, reason", 4);
    assert!(client.hover_text(CLIENT, l, c).contains("local reason: string?"));

    let (l, c) = pos(&text, "local coords", 7);
    assert!(client.hover_text(CLIENT, l, c).contains("local coords: vector3"));

    let (l, c) = pos(&text, "local distance", 7);
    assert!(client.hover_text(CLIENT, l, c).contains("local distance: number"));

    let (l, c) = pos(&text, "GetEntityCoords", 3);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("function GetEntityCoords(entity: Entity, alive: boolean): vector3"), "{hover}");
    assert!(hover.contains("native"), "{hover}");

    let (l, c) = pos(&text, "garage:getVehicleCount", 8);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("Garage:getVehicleCount"), "{hover}");
    assert!(hover.contains("how many vehicles"), "{hover}");

    let (l, c) = pos(&text, "settings.maxGarages", 10);
    assert!(client.hover_text(CLIENT, l, c).contains("maxGarages: integer"));

    let (l, c) = pos(&text, "Config.Garages.legion.label", 23);
    assert!(client.hover_text(CLIENT, l, c).contains("label: string"));
}

#[test]
fn server_code_calls_the_server_native_of_a_shared_name() {
    let mut client = Client::start(fixture_root());
    let text = "local vehicles = GetAllVehicles()\nlocal weapon = GetCurrentPedWeapon(1)\nprint(vehicles, weapon)\n";
    for (file, vehicles, weapon) in
        [(SERVER, "vehicles: table", "weapon: Hash"), (CLIENT, "vehicles: integer", "weapon: boolean")]
    {
        client.open_with(file, text);
        for expected in [vehicles, weapon] {
            let (l, c) = pos(text, &format!("local {}", &expected[..expected.find(':').unwrap()]), 6);
            let hover = client.hover_text(file, l, c);
            assert!(hover.contains(expected), "{file}: expected {expected:?} in {hover}");
        }
    }
    let (l, c) = pos(text, "GetAllVehicles", 0);
    let hover = client.hover_text(SERVER, l, c);
    assert!(hover.contains("function GetAllVehicles(): table") && hover.contains("*server native* · `CFX`"), "{hover}");
}

#[test]
fn native_argument_hovers_show_defaults_flags_and_exact_ranges() {
    // Numeric hovers also work for clients that advertise no optional hover capabilities.
    for capabilities in [json!({}), json!({ "textDocument": { "hover": { "contentFormat": ["markdown"] } } })] {
        let mut client = Client::start_with_capabilities(fixture_root(), capabilities);
        let text = "IsControlJustPressed(0, 38)\nSetPedConfigFlag(PlayerPedId(), 48, true)\nGetControlNormal(0, 360)\nGetControlNormal(0, 3)\nGetControlNormal(0, 243)\n";
        client.open_with(CLIENT, text);
        for (literal, symbol) in [("38", "INPUT_PICKUP"), ("48", "CPED_CONFIG_FLAG_BlockWeaponSwitching")] {
            let (line, character) = pos(text, literal, 0);
            let hover = client.request("textDocument/hover", client.position_params(CLIENT, line, character));
            let content = hover["contents"]["value"].as_str().unwrap();
            assert!(content.contains(symbol), "{content}");
            assert!(content.contains("https://"), "reference should link its source: {content}");
            assert_eq!(hover["contents"]["kind"], "markdown");
            assert_eq!(
                hover["range"],
                json!({
                    "start": { "line": line, "character": character },
                    "end": { "line": line, "character": character + 2 }
                })
            );
            if literal == "38" {
                assert!(content.contains("Default keyboard (QWERTY): `E`"), "{content}");
                assert!(content.contains("Default Xbox controller: `LB`"), "{content}");
                assert!(content.contains("remapped"), "{content}");
            } else {
                assert!(content.contains("Behavior is not documented"), "do not infer behavior from a name: {content}");
                assert!(content.contains("potential names and hash collisions"), "{content}");
            }
            // Hovering the comma/space after a literal must not inherit its enum documentation.
            assert!(client.hover_text(CLIENT, line, character + 2).is_empty());
        }
        let hover = client.hover_text(CLIENT, 0, 3);
        assert!(hover.contains("function IsControlJustPressed"), "ordinary native hover is preserved: {hover}");
        let (line, character) = pos(text, "360", 0);
        let hover = client.hover_text(CLIENT, line, character);
        assert!(hover.contains("INPUT_HUDMARKER_SELECT"), "{hover}");
        assert!(hover.contains("Default keyboard (QWERTY): Not documented"), "{hover}");
        assert!(hover.contains("Default Xbox controller: Not documented"), "{hover}");
        let (line, character) = pos(text, ", 3)", 2);
        let hover = client.hover_text(CLIENT, line, character);
        assert!(hover.contains("Default keyboard (QWERTY): `(NONE)`"), "explicit unbound values stay intact: {hover}");
        let (line, character) = pos(text, "243", 0);
        let hover = client.hover_text(CLIENT, line, character);
        assert!(
            hover.contains("Default keyboard (QWERTY): `` ~ / ` ``"),
            "backtick keys must remain valid Markdown: {hover}"
        );
    }
}

#[test]
fn native_argument_hovers_cover_control_variants_hashes_and_numeric_literals() {
    let mut client = Client::start(fixture_root());
    let cases = [
        ("IsControlEnabled(0, 38)", "38", "INPUT_PICKUP"),
        ("IsControlJustReleased(0, 38)", "38", "INPUT_PICKUP"),
        ("IsControlPressed(0, 38)", "38", "INPUT_PICKUP"),
        ("IsControlReleased(0, 38)", "38", "INPUT_PICKUP"),
        ("IsDisabledControlJustPressed(0, 38)", "38", "INPUT_PICKUP"),
        ("IsDisabledControlJustReleased(0, 51)", "51", "INPUT_CONTEXT"),
        ("IsDisabledControlPressed(0, 38)", "38", "INPUT_PICKUP"),
        ("IsDisabledControlReleased(0, 38)", "38", "INPUT_PICKUP"),
        ("GetControlValue(0, 38)", "38", "INPUT_PICKUP"),
        ("GetControlNormal(2, 0)", "0", "INPUT_NEXT_CAMERA"),
        ("GetControlUnboundNormal(0, 38)", "38", "INPUT_PICKUP"),
        ("GetDisabledControlNormal(0, 38)", "38", "INPUT_PICKUP"),
        ("GetDisabledControlUnboundNormal(0, 38)", "38", "INPUT_PICKUP"),
        ("GetControlInstructionalButton(0, 38, true)", "38", "INPUT_PICKUP"),
        ("DisableControlAction(0, 38, true)", "38", "INPUT_PICKUP"),
        ("EnableControlAction(0, 38, true)", "38", "INPUT_PICKUP"),
        ("SetControlNormal(0, 38, 0.5)", "38", "INPUT_PICKUP"),
        ("SetInputExclusive(0, 38)", "38", "INPUT_PICKUP"),
        ("IsControlJustPressed(0, 0X26)", "0X26", "INPUT_PICKUP"),
        ("IsControlJustPressed(0, 38.0)", "38.0", "INPUT_PICKUP"),
        ("IsControlJustPressed(0, 3.8e1)", "3.8e1", "INPUT_PICKUP"),
        ("IsControlJustPressed(0, 0x26p0)", "0x26p0", "INPUT_PICKUP"),
        ("N_0xe8a25867fba3b05e(0, 38, 0.5)", "38", "INPUT_PICKUP"),
        ("N_0x580417101DDB492F(0, 0x26)", "0x26", "INPUT_PICKUP"),
        ("GetPedConfigFlag(PlayerPedId(), 32, true)", "32", "CPED_CONFIG_FLAG_WillFlyThroughWindscreen"),
        ("N_0x1913FE4CBF41C463(PlayerPedId(), 0x30, true)", "0x30", "CPED_CONFIG_FLAG_BlockWeaponSwitching"),
        ("N_0x7ee53118c892b513(PlayerPedId(), 48, true)", "48", "CPED_CONFIG_FLAG_BlockWeaponSwitching"),
    ];
    client.open_with(CLIENT, cases[0].0);
    for (version, (text, literal, symbol)) in cases.into_iter().enumerate() {
        client.change(CLIENT, version as i32 + 2, text);
        let (line, character) = pos(text, literal, 0);
        let hover = client.request("textDocument/hover", client.position_params(CLIENT, line, character));
        assert!(hover["contents"]["value"].as_str().unwrap_or_default().contains(symbol), "{text}: {hover}");
        assert_eq!(hover["range"]["start"]["character"], character, "{text}: {hover}");
        assert_eq!(hover["range"]["end"]["character"], character + literal.len() as u32, "{text}: {hover}");
    }
}

#[test]
fn native_argument_hovers_respect_ast_nesting_comments_and_argument_positions() {
    let mut client = Client::start(fixture_root());
    let text = "\
local label = 'é🎮'; Consume(IsControlJustPressed(0, 38))
SetPedConfigFlag(
    GetPed(48), -- a nested number is not a flag
    ( -- flag 48 in a comment is not a literal
      0x30 -- end of actual flag
    ),
    true
)
IsControlJustPressed(38, 51)
GetControlGroupInstructionalButton(0, 38, true)
SetControlGroupColor(0, 38, 0, 0)
SetPedResetFlag(PlayerPedId(), 48, true)
SetControlNormal(0, 38, 51)
GetPedConfigFlag(32, 48, 32)
";
    client.open_with(CLIENT, text);
    // LSP ranges use UTF-16, including when non-ASCII text precedes the number.
    let first_line = text.lines().next().unwrap();
    let offset = first_line.find("38").unwrap();
    let character = first_line[..offset].encode_utf16().count() as u32;
    let hover = client.request("textDocument/hover", client.position_params(CLIENT, 0, character));
    assert!(hover["contents"]["value"].as_str().unwrap().contains("INPUT_PICKUP"), "{hover}");
    assert_eq!(
        hover["range"],
        json!({
            "start": { "line": 0, "character": character },
            "end": { "line": 0, "character": character + 2 }
        })
    );
    let (line, character) = pos(text, "0x30", 0);
    let hover = client.request("textDocument/hover", client.position_params(CLIENT, line, character));
    assert!(hover["contents"]["value"].as_str().unwrap().contains("CPED_CONFIG_FLAG_BlockWeaponSwitching"), "{hover}");
    assert_eq!(hover["range"]["start"]["character"], character);
    assert_eq!(hover["range"]["end"]["character"], character + 4);
    for (needle, literal) in [
        ("GetPed(48)", "48"),
        ("flag 48", "48"),
        ("IsControlJustPressed(38", "38"),
        ("GetControlGroupInstructionalButton(0, 38", "38"),
        ("SetControlGroupColor(0, 38", "38"),
        ("SetPedResetFlag(PlayerPedId(), 48", "48"),
        ("SetControlNormal(0, 38, 51)", "51"),
        ("GetPedConfigFlag(32", "32"),
        ("48, 32)", "32"),
    ] {
        let (line, character) = pos(text, needle, needle.rfind(literal).unwrap() as u32);
        assert!(client.hover_text(CLIENT, line, character).is_empty(), "unrelated argument/comment: {needle}");
    }
    let (line, character) = pos(text, "38, 51)", 4);
    assert!(client.hover_text(CLIENT, line, character).contains("INPUT_CONTEXT"));
}

#[test]
fn native_argument_hovers_ignore_expressions_unknown_ids_and_unrelated_functions() {
    let mut client = Client::start(fixture_root());
    let cases = [
        ("local value = 38", "38"),
        ("CustomControl(0, 38)", "38"),
        ("controls.IsControlJustPressed(0, 38)", "38"),
        ("controls:IsControlJustPressed(0, 38)", "38"),
        ("_G.IsControlJustPressed(0, 38)", "38"),
        ("Citizen.InvokeNative(0x580417101DDB492F, 0, 38)", "38"),
        ("IsControlJustPressed(0, 38 + 1)", "38"),
        ("IsControlJustPressed(0, 38 | 1)", "38"),
        ("IsControlJustPressed(0, -38)", "38"),
        ("IsControlJustPressed(0, tonumber(38))", "38"),
        ("IsControlJustPressed(0, {38})", "38"),
        ("IsControlJustPressed(0, '38')", "38"),
        ("IsControlJustPressed(0, 38.5)", "38.5"),
        ("IsControlJustPressed(0, 9999)", "9999"),
        ("IsControlJustPressed(0, 38oops)", "38oops"),
        ("IsControlJustPressed(0, 0xZZ)", "0xZZ"),
        ("GetPedResetFlag(PlayerPedId(), 48)", "48"),
        ("SetPedConfigFlag(PlayerPedId(), 9999, true)", "9999"),
        ("SetPedConfigFlag(PlayerPedId(), 48 + 1, true)", "48"),
        ("N_0x0000000000000000(0, 38)", "38"),
    ];
    client.open_with(CLIENT, cases[0].0);
    for (version, (text, literal)) in cases.into_iter().enumerate() {
        client.change(CLIENT, version as i32 + 2, text);
        let (line, character) = pos(text, literal, 0);
        assert!(client.hover_text(CLIENT, line, character).is_empty(), "must not label {text}");
    }
    let text = "local control = 38\nIsControlJustPressed(0, control)";
    client.change(CLIENT, 100, text);
    let (line, character) = pos(text, ", control", 2);
    let hover = client.hover_text(CLIENT, line, character);
    assert!(hover.contains("local control: integer"), "ordinary variable hover is preserved: {hover}");
    assert!(!hover.contains("INPUT_PICKUP"), "do not evaluate dynamic arguments: {hover}");
}

#[test]
fn native_argument_hovers_ignore_shadowed_and_redefined_natives() {
    let mut client = Client::start(fixture_root());
    let cases = [
        "local IsControlJustPressed = function(...) end\nIsControlJustPressed(0, 38)",
        "local function IsControlJustPressed(...) end\nIsControlJustPressed(0, 38)",
        "local IsControlJustPressed = IsControlJustPressed\nIsControlJustPressed(0, 38)",
        "function check(IsControlJustPressed)\nIsControlJustPressed(0, 38)\nend",
        "IsControlJustPressed = unknown\nIsControlJustPressed(0, 38)",
        "function IsControlJustPressed(...) end\nIsControlJustPressed(0, 38)",
        "_G.IsControlJustPressed = function(...) end\nIsControlJustPressed(0, 38)",
        "_ENV['IsControlJustPressed'] = function(...) end\nIsControlJustPressed(0, 38)",
        "local _ENV = {}\nIsControlJustPressed(0, 38)",
        "_ENV = {}\nIsControlJustPressed(0, 38)",
        "local N_0x580417101DDB492F = function(...) end\nN_0x580417101DDB492F(0, 38)",
        "local SetPedConfigFlag = function(...) end\nSetPedConfigFlag(ped, 38, true)",
    ];
    client.open_with(CLIENT, cases[0]);
    for (version, text) in cases.into_iter().enumerate() {
        client.change(CLIENT, version as i32 + 2, text);
        let (line, character) = pos(text, "38", 0);
        assert!(client.hover_text(CLIENT, line, character).is_empty(), "must not label shadowed call: {text}");
    }
    // A local declaration in a different scope must not hide the real native here.
    let text = "do local IsControlJustPressed = function(...) end end\nIsControlJustPressed(0, 38)";
    client.change(CLIENT, 100, text);
    let (line, character) = pos(text, "38", 0);
    assert!(client.hover_text(CLIENT, line, character).contains("INPUT_PICKUP"));

    // A definition in a visible resource file must suppress the native annotation too.
    let other = "myresource/shared/config.lua";
    client.open_with(other, "IsControlJustPressed = function(...) end");
    assert!(client.hover_text(CLIENT, line, character).is_empty());
    client.change(other, 2, "");
    assert!(
        client.hover_text(CLIENT, line, character).contains("INPUT_PICKUP"),
        "removing an override restores native hover"
    );
}

#[test]
fn hover_keeps_literal_types_of_inline_loop_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
for _, sex in pairs({ 'male', 'female' }) do end
for i, n in ipairs({ 1, 2, extra = 'x' }) do end
for key, value in pairs({ a = true, [3] = 'c' }) do end
";
    client.open_with(CLIENT, text);
    let cases = [
        ("sex", "sex: \"male\"|\"female\""),
        ("i,", "i: integer"),
        ("n in", "n: 1|2"),
        ("key", "key: \"a\"|3"),
        ("value", "value: true|\"c\""),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_indexes_fields_with_literal_typed_keys() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@type ['male', 'female']
local sexes = { 'male', 'female' }

---@param metadata { male: table, female: table, age: integer }
---@param field 'male'|'age'
---@param kind GarageKind
local function send(metadata, field, kind)
    for _, sex in pairs({ 'male', 'female' }) do
        local sexData = metadata[sex]
    end
    for _, listed in pairs(sexes) do
        local listedData = metadata[listed]
    end
    local either = metadata[field]
    local missing = metadata[kind]
end
";
    client.open_with(CLIENT, text);
    let cases = [
        ("sexData", "sexData: table"),
        ("listed in", "listed: \"male\"|\"female\""),
        ("listedData", "listedData: table"),
        ("either", "either: table|integer"),
        ("missing", "missing: unknown"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_reads_undeclared_names_and_keys_through_indices() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Counts
---@field total integer
---@field [string] number
---@field [integer] boolean

---@class Test.Tally : Test.Counts

---@alias Test.Slot 'head'|'feet'

---@class Test.Outfit
---@field [Test.Slot] integer

---@class Test.Left : Test.Counts, Test.Right
---@class Test.Right : Test.Counts, Test.Left

---@type { [string]: integer, [integer]: boolean, name: string }
local mixed = {}
---@type Test.Tally
local tally = {}
---@type table<string, integer>
local scores = {}
---@type Test.Outfit
local outfit = {}
---@type { [Test.Slot]: boolean }
local slots = {}
---@type Test.Left
local left = {}

local named = mixed.name
local other = mixed.other
local quoted = mixed['quoted']
local first = mixed[1]
local total = tally.total
local inherited = tally.wins
local position = tally[1]
local score = scores.alice
local head = outfit.head
local hands = outfit.hands
local feet = slots.feet
local gloves = slots.gloves
local shared = left.wins
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("named", "named: string"),
        ("other", "other: integer"),
        ("quoted", "quoted: integer"),
        ("first", "first: boolean"),
        ("total =", "total: integer"),
        ("inherited", "inherited: number"),
        ("position", "position: boolean"),
        ("score =", "score: integer"),
        // An index of the values of an alias takes those values only.
        ("head", "head: integer"),
        ("hands", "hands: unknown"),
        ("feet", "feet: boolean"),
        ("gloves", "gloves: unknown"),
        // Parents that reach the same class, or each other, read it once.
        ("shared", "shared: number"),
    ] {
        let (l, c) = pos(text, &format!("local {needle}"), 6);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    let (l, c) = pos(text, "Test.Counts\n", 0);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("[string]: number,\n    [integer]: boolean,"), "{hover}");
}

#[test]
fn a_class_parent_written_as_a_union_is_its_first_type() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.First
---@field first string

---@class Test.Second
---@field second string

---@class Test.Either : Test.First | Test.Second

---@type Test.Either
local either = {}

local first = either.first
local second = either.second
";
    client.open_with(CLIENT, text);
    // As lua-language-server reads it, and as the parent was read before parents were types.
    for (needle, expected) in [("local first", "first: string"), ("local second", "second: unknown")] {
        let (l, c) = pos(text, needle, 6);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_infers_loop_variables_of_top_level_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
local list = { 'male', 'female' }
local mixed = { 'a', count = 1, [10] = true }
local garages = { legion = { label = 'Legion' }, pillbox = { label = 'Pillbox' } }
local first = list[1]
for _, item in pairs(list) do end
for k, v in pairs(mixed) do end
for i, element in ipairs(mixed) do end
for name, garage in pairs(garages) do end
local lookup = { [1] = 'one', [2] = 'two' }
local fromLookup = lookup[1]
for _, looked in ipairs(lookup) do end
local flagged = { 'a', [true] = 5 }
local sparse = { 'a', [10] = true }
";
    client.open_with(CLIENT, text);
    let cases = [
        ("list", "local list: string[]"),
        ("first", "first: string"),
        ("item", "item: string"),
        ("k, v", "k: string|integer"),
        ("v in", "v: integer|string|boolean"),
        // `ipairs` stops before `[10]`.
        ("element", "element: string\n"),
        ("name,", "name: string"),
        ("garage in", "label: string"),
        ("lookup =", "local lookup: string[]"),
        ("fromLookup", "fromLookup: string"),
        ("looked", "looked: string"),
        ("flagged", "local flagged: table"),
        ("sparse", "local sparse: (string|boolean)[]"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_infers_loops_over_classes_unions_next_and_mixed_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param payload Garage
---@param both string[]|table<string, integer>
---@param t table<string, boolean>
local function f(payload, both, t)
    for k1, v1 in pairs(payload) do end
    for k2, v2 in pairs(both) do end
    for k3, v3 in next, t do end
    local mixed = { 'a', x = 1 }
    for k4, v4 in pairs(mixed) do end
    local keyed = { [1] = 'a', [2] = 5 }
    local fromKeyed = keyed[1]
    local flags = { 'a', [true] = 5 }
    for _, flagged in ipairs(flags) do end
    for flag in pairs(flags) do end
end
";
    client.open_with(CLIENT, text);
    let cases: &[(&str, &[&str])] = &[
        ("flags =", &["flags: { [integer]: string, [boolean]: integer }"]),
        ("flagged", &["flagged: string\n"]),
        ("flag in", &["flag: integer|boolean"]),
        ("k1", &["k1: string"]),
        ("v1", &["coords: vector3", "type GarageKind ="]),
        ("k2", &["k2: integer|string"]),
        ("v2", &["v2: string|integer"]),
        ("k3", &["k3: string"]),
        ("v3", &["v3: boolean"]),
        ("k4", &["k4: string|integer"]),
        ("v4", &["v4: integer|string"]),
        ("fromKeyed", &["fromKeyed: string|integer"]),
    ];
    for &(needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        for part in expected {
            assert!(hover.contains(part), "{needle}: expected {part:?} in {hover}");
        }
    }
}

#[test]
fn hover_indexes_global_arrays_and_unions() {
    let mut client = Client::start(fixture_root());
    let text = "\
TestShop = {}
TestShop.Items = { 'bread', 'water' }
TestShop.Lookup = { [1] = 'one', [2] = 2 }
local firstItem = TestShop.Items[1]
for _, item in ipairs(TestShop.Items) do end
for k, v in pairs(TestShop.Lookup) do end

---@param either string[]|integer[]
local function pick(either)
    local picked = either[1]
end

local shelf = { 'bread', 'water' }

---@param slot string
local function restock(index, slot)
    local byIndex = shelf[index]
    local byNumber = TestShop.Items[tonumber(slot)]
    local bySlot = shelf[slot]
end
";
    client.open_with(CLIENT, text);
    let cases = [
        ("Items[1]", "TestShop.Items: string[]"),
        ("firstItem", "firstItem: string"),
        ("item in", "item: string"),
        ("k, v", "k: integer"),
        ("v in", "v: string|integer"),
        ("picked", "picked: string|integer"),
        ("byIndex", "byIndex: string"),
        ("byNumber", "byNumber: string"),
        ("bySlot", "bySlot: unknown"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_picks_the_overload_a_call_fits() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@overload fun(name: string): string
---@param id integer
---@return integer
local function find(id) end

---@overload fun(): boolean
---@param x string
---@return string
local function arity(x) end

---@overload fun(name: string, cb: fun(found: string))
---@param id integer
---@param cb fun(found: integer)
local function lookup(id, cb) end

---@overload fun(filter: table?, cb: fun(found: string))
---@param id? integer
---@param cb fun(found: integer)
local function search(id, cb) end

---@class OverloadedShop
local OverloadedShop = {}

---@overload fun(self: OverloadedShop, label: string): string
---@param id integer
---@return integer
function OverloadedShop:price(id) end

---@overload fun(label: string): string
---@param id integer
---@return integer
function OverloadedShop:stock(id) end

---@alias MoveAction 'moved'|'dragged'

---@param action string
---@param handler fun(...)
---@return number id
---@overload fun(action: \"keyPressed\", handler: fun(key: string)) : number
---@overload fun(action: MoveAction, handler: fun(x: number, y: number)): number
---@overload fun(action: 'closed', handler: fun()): boolean
local function onAction(action, handler) end

---@overload fun(label: string, cb: fun(entry: string))
---@overload fun(label: 'all', cb: fun(entry: table))
---@param id integer
---@param cb fun(entry: integer)
local function listing(id, cb) end

---@alias ShopAction 'opened'|'closed'|string

---@param action ShopAction
---@param handler fun(...)
---@overload fun(action: 'opened', handler: fun(shopId: integer))
local function onShop(action, handler) end

local byId = find(1)
local byName = find('x')
local none = arity()
lookup('x', function(named) end)
lookup(1, function(numbered) end)
search(nil, function(byNil) end)
local priceByLabel = OverloadedShop:price('bread')
local priceById = OverloadedShop:price(1)
local stockByLabel = OverloadedShop:stock('bread')
local stockViaDot = OverloadedShop.stock(OverloadedShop, 'bread')
local priceViaDot = OverloadedShop.price(OverloadedShop, 'bread')
local flat = vec(1, 2)
local deep = vec(1, 2, 3)
local pressedId = onAction('keyPressed', function(pressedKey) end)
onAction('dragged', function(dragX, dragY) end)
local closedId = onAction('closed', function() end)
local scrolledId = onAction('scrolled', function(scrollArg) end)
listing('all', function(allEntry) end)
listing('shop', function(labelEntry) end)
onShop('opened', function(openedShop) end)
onShop('closed', function(closedShop) end)
";
    client.open_with(CLIENT, text);
    let cases = [
        ("byId", "byId: integer"),
        ("byName", "byName: string"),
        ("none", "none: boolean"),
        ("named", "named: string"),
        ("numbered", "numbered: integer"),
        // `nil` fills the optional `id`, so the declared signature still fits.
        ("byNil", "byNil: integer"),
        ("priceByLabel", "priceByLabel: string"),
        ("priceById", "priceById: integer"),
        ("stockByLabel", "stockByLabel: string"),
        ("stockViaDot", "stockViaDot: string"),
        ("priceViaDot", "priceViaDot: string"),
        // `vec(...)` takes any number of values, but its overloads name the exact ones.
        ("flat", "flat: vector2"),
        ("deep", "deep: vector3"),
        // A literal argument picks the overload that lists it over the declared `action: string`.
        ("pressedKey", "pressedKey: string"),
        ("pressedId", "pressedId: number"),
        ("dragY", "dragY: number"),
        ("closedId", "closedId: boolean"),
        // A literal no overload lists stays with the declared signature.
        ("scrolledId", "scrolledId: number"),
        ("scrollArg", "scrollArg: any"),
        // Of two overloads that fit, the one listing the literal wins, though it comes later.
        ("allEntry", "allEntry: table"),
        ("labelEntry", "labelEntry: string"),
        // An alias listing the literal among other values loses to the overload that takes only it.
        ("openedShop", "openedShop: integer"),
        ("closedShop", "closedShop: any"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn methods_holding_a_class_call_its_overload() {
    let mut client = Client::start(fixture_root());
    // ox_lib's `lib.array:new()` calls the `ArrayConstructor` that its `new` field holds.
    let text = "\
---@class Test.Array : { [number]: integer }
---@field private new Test.ArrayConstructor
local Array = {}

---@class Test.ArrayConstructor
---@overload fun(self: Test.Array, ...: integer): Test.Array

local list = Array:new(1, 2)
local same = Array.new(Array, 1, 2)
";
    client.open_with(CLIENT, text);
    for name in ["list", "same"] {
        let (l, c) = pos(text, &format!("local {name}"), 6);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(&format!("local {name}: Test.Array")), "{hover}");
    }
}

#[test]
fn hover_reads_docs_past_other_comments() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param level string
-- qbx-lint: disable-next-line lowercase-global
function infoprint(level, ...) end

--- Spawns a vehicle.
---@param model string
-- The model may also be a hash.
---@return number
local function spawn(model) end

---@param detached number
-- note

local function blank(detached) end

local spawned = spawn('adder')

---@class Probe.Plain
-- note
---@field size number

---@class Probe.Blank

---@field size number

---@alias Probe.Mode
--[[ note ]]
---| 'on'
---| 'off'

---@type Probe.Plain
local plain = {}
local plainSize = plain.size
---@type Probe.Blank
local blanked = {}
local blankSize = blanked.size
---@type Probe.Mode
local modeValue
";
    client.open_with(CLIENT, text);
    let cases = [
        ("infoprint(level", "level: string"),
        ("spawn(model)", "spawn(model: string): number"),
        ("spawn(model)", "Spawns a vehicle."),
        ("spawned =", "spawned: number"),
        ("plainSize =", "plainSize: number"),
        ("modeValue", "type Probe.Mode = \"on\"|\"off\""),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    // A blank line detaches the docs.
    for (needle, unexpected) in [("blank(detached)", "detached: number"), ("blankSize =", "blankSize: number")] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(!hover.contains(unexpected), "{needle}: unexpected {unexpected:?} in {hover}");
    }
}

#[test]
fn generics_reach_annotations_past_other_comments() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@generic Plain
-- note
---@param x Plain
local function idPlain(x) return x end

---@generic Block
--[[ note ]]
---@param y Block
local function idBlock(y) return y end

---@generic Blank

---@param z Blank
local function idBlank(z) return z end

print(idPlain, idBlock, idBlank)
";
    client.open_with(CLIENT, text);
    client.diagnostics_for(CLIENT);
    let uri = client.uri(CLIENT).to_string();
    let found: Vec<(u64, String)> = client.diagnostics[&uri]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "undefined-doc-name")
        .map(|d| (d["range"]["start"]["line"].as_u64().unwrap(), d["message"].as_str().unwrap().into()))
        .collect();
    let (line, _) = pos(text, "z Blank", 0);
    assert_eq!(found, [(line as u64, "Undefined type or alias `Blank`".to_string())], "as in LuaLS");
}

#[test]
fn calls_show_the_overload_they_pick() {
    let mut client = Client::start(fixture_root());
    let text = "---@param action string
---@param handler fun(...)
---@return number id
---@overload (client) fun(action: 'keyPressed', handler: fun(key: string)): number
---@overload (server) fun(action: 'keyPressed', handler: fun(source: integer, key: string)): number
function OnInput(action, handler) end

---@param action string
---@overload fun(action: 'jobUpdated', job: table, oldJob?: table)
function EmitInput(action, ...) end

---@class InputShop
local InputShop = {}

---@overload fun(label: string): string
---@param id integer
---@return integer
function InputShop:stock(id) end

---@overload fun(name: string): string
---@param id integer
---@return integer
local function findInput(id) end

OnInput('keyPressed', function(key) end)
OnInput('scrolled', function(delta) end)
EmitInput('jobUpdated', {}, nil)
local labelStock = InputShop:stock('bread')
local named = findInput('x')
local numbered = findInput(1)
";
    client.open_with(CLIENT, text);
    let hovers = [
        (
            "OnInput('keyPressed'",
            "(global) function OnInput(action: \"keyPressed\", handler: fun(key: string)): number",
        ),
        // A literal no overload lists keeps the declared signature, as does the declaration itself.
        ("OnInput('scrolled'", "(global) function OnInput(action: string, handler: fun(...: any)): number"),
        ("OnInput(action", "(global) function OnInput(action: string, handler: fun(...: any)): number"),
        ("EmitInput('jobUpdated'", "(global) function EmitInput(action: \"jobUpdated\", job: table, oldJob?: table)"),
        ("stock('bread')", "function InputShop:stock(label: string): string"),
        ("findInput('x')", "local function findInput(name: string): string"),
        ("findInput(1)", "local function findInput(id: integer): integer"),
    ];
    for (needle, expected) in hovers {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }

    // Signature help lists the declared signature and the overloads for this side.
    let (l, c) = pos(text, "OnInput('keyPressed', f", 22);
    let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
    let labels: Vec<&str> =
        result["signatures"].as_array().unwrap().iter().filter_map(|s| s["label"].as_str()).collect();
    assert_eq!(
        labels,
        [
            "OnInput(action: string, handler: fun(...: any)): number",
            "OnInput(action: \"keyPressed\", handler: fun(key: string)): number"
        ]
    );
    assert_eq!(result["activeSignature"], 1);
    assert_eq!(result["activeParameter"], 1);

    let (l, c) = pos(text, "OnInput('scrolled', f", 20);
    let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
    assert_eq!(result["activeSignature"], 0);

    let (l, c) = pos(text, "EmitInput('jobUpdated', {}, nil", 28);
    let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
    assert_eq!(result["activeSignature"], 1);
    assert_eq!(result["signatures"][1]["activeParameter"], 2);
    // The declared signature passes the same argument to its `...`.
    assert_eq!(result["signatures"][0]["activeParameter"], 1);

    // Inlay hints name the parameters of the signature the call picks.
    let (first, _) = pos(text, "EmitInput('jobUpdated'", 0);
    let (last, _) = pos(text, "local numbered", 0);
    let hints = client.request(
        "textDocument/inlayHint",
        json!({ "textDocument": { "uri": client.uri(CLIENT) }, "range": { "start": { "line": first, "character": 0 }, "end": { "line": last + 1, "character": 0 } } }),
    );
    let labels: Vec<&str> = hints.as_array().unwrap().iter().map(|h| h["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["action:", "job:", "oldJob:", "label:", "name:", "id:"]);
}

#[test]
fn signature_help_shows_the_docs_of_the_function_a_local_holds() {
    let mut client = Client::start(fixture_root());
    let text = "\
local each = function(a, b) end
each(1, 2)
---Counts the values.
---@param n integer
local function count(n) end
count(1)
local GetEntityCoords = GetEntityCoords
GetEntityCoords(1)
";
    client.open_with(CLIENT, text);
    // The cursor goes after the `(` of each call.
    let mut documentation = |call: &str| {
        let (l, c) = pos(text, call, call.find('(').unwrap() as u32 + 1);
        let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
        result["signatures"][0]["documentation"]["value"].as_str().map(str::to_string)
    };
    assert_eq!(documentation("each(1, 2)"), None, "not the docs of the global `each` the local hides");
    assert_eq!(documentation("count(1)").as_deref(), Some("Counts the values."));
    let native = documentation("GetEntityCoords(1)").unwrap_or_default();
    assert!(native.contains("coordinates"), "a local holding a native keeps its docs: {native}");
}

#[test]
fn side_scoped_annotations_follow_the_side_of_the_code() {
    const SHARED: &str = "myresource/shared/config.lua";
    let mut client = Client::start(fixture_root());
    let shared = "\
---@class Account
---@field (server) balance number
---@field (client) balance string
---@field name string

---@class (server) BankRecord
---@field id integer

---@alias (client) Key 'E'|'F'
---@alias (server) Key integer

---@param event string
---@param handler fun(...)
---@overload (server) fun(event: 'tick', handler: fun(delta: number))
---@overload (client) fun(event: 'tick', handler: fun(frame: string))
function OnTick(event, handler) end

if IsDuplicityVersion() then
    OnTick('tick', function(guardedDelta) end)
else
    OnTick('tick', function(guardedFrame) end)
end
";
    client.open_with(SHARED, shared);
    let uses = |side: &str| {
        format!(
            "---@type Account\nlocal {side}Account\nlocal {side}Balance = {side}Account.balance\nlocal {side}Name = {side}Account.name\nOnTick('tick', function({side}Tick) end)\n---@type BankRecord\nlocal {side}Record\nlocal {side}RecordId = {side}Record.id\n---@type Key\nlocal {side}Key\n"
        )
    };
    let client_text = uses("client");
    let server_text = uses("server");
    client.open_with(CLIENT, &client_text);
    client.open_with(SERVER, &server_text);

    let cases = [
        (CLIENT, &client_text, "clientBalance", "clientBalance: string"),
        (CLIENT, &client_text, "clientName", "clientName: string"),
        (CLIENT, &client_text, "clientTick", "clientTick: string"),
        // The server-only class does not exist for the client.
        (CLIENT, &client_text, "clientRecordId", "clientRecordId: unknown"),
        (SERVER, &server_text, "serverBalance", "serverBalance: number"),
        (SERVER, &server_text, "serverTick", "serverTick: number"),
        (SERVER, &server_text, "serverRecordId", "serverRecordId: integer"),
        // Guards narrow the side of shared code.
        (SHARED, &shared.to_string(), "guardedDelta", "guardedDelta: number"),
        (SHARED, &shared.to_string(), "guardedFrame", "guardedFrame: string"),
    ];
    for (file, text, needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(file, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    for (file, text, expected) in [(CLIENT, &client_text, "\"E\"|\"F\""), (SERVER, &server_text, "= integer")] {
        let (l, c) = pos(text, "@type Key", 6);
        let hover = client.hover_text(file, l, c);
        assert!(hover.contains(expected), "{file}: expected {expected:?} in {hover}");
    }

    // A type that only the other side declares is reported where it is used.
    let mut doc_names = |file: &str| -> Vec<String> {
        client.diagnostics_for(file);
        let uri = client.uri(file).to_string();
        let found = client.diagnostics[&uri].as_array().unwrap().iter().filter(|d| d["code"] == "undefined-doc-name");
        found.map(|d| d["message"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(doc_names(CLIENT), ["Type `BankRecord` only exists on the server, but this is a client script"]);
    assert!(doc_names(SERVER).is_empty());
    assert!(doc_names(SHARED).is_empty());
}

#[test]
fn callback_wrappers_link_registrations_to_their_calls() {
    const SHARED: &str = "myresource/shared/config.lua";
    let mut client = Client::start(fixture_root());
    client.open_with(
        SHARED,
        "\
---@callback register
---@param name string
---@param handler fun(source: integer, ...): ...
function RegisterServerCallback(name, handler) end

---@callback await
---@param name string
---@param ... any
function AwaitServerCallback(name, ...) end

---@callback trigger
---@param event string
---@param cb fun(...)
---@param ... any
function TriggerServerCallback(event, cb, ...) end

Shop = {}

---@callback register shop
---@param name string
---@param handler function
function Shop.register(name, handler) end

---@callback await shop
---@param name string
---@param ... any
function Shop:await(name, ...) end
",
    );
    let server = "\
---@param storeId number
---@param index number
---@param location vector3
---@return boolean success
RegisterServerCallback('removeStoreLocation', function(source, storeId, index, location)
    print(storeId)
end)

---@param item string
---@return integer price
Shop.register('shop:price', function(source, item) end)

---@class Appearance
---@field model string

---@return Appearance
local function FixAppearanceKeys(data) return data end

RegisterServerCallback('getPlayerAppearance', function(source)
    if not source then
        return
    end
    return FixAppearanceKeys(source)
end)

---@param plate string the plate to look for
---@return number clamps
local registered = RegisterServerCallback('countClamps', function(source, plate) return 1 end)
";
    client.open_with(SERVER, server);
    let text = "\
local removed = AwaitServerCallback('removeStoreLocation', 1, 2, vector3(0, 0, 0))
TriggerServerCallback('removeStoreLocation', function(ok) end, 1, 2, vector3(0, 0, 0))
local price = Shop:await('shop:price', 'water')
AwaitServerCallback('')
Shop:await('')
local awaitAlias = AwaitServerCallback
local aliased = awaitAlias('removeStoreLocation')
local appearance = AwaitServerCallback('getPlayerAppearance')
local counted = AwaitServerCallback('countClamps', 'ABC 123')
";
    client.open_with(CLIENT, text);

    for (file, source, needle, expected) in [
        (CLIENT, text, "removed", "removed: boolean"),
        (CLIENT, text, "ok)", "ok: boolean"),
        (CLIENT, text, "price", "price: integer"),
        (CLIENT, text, "aliased", "aliased: boolean"),
        // An undocumented handler that can return nothing.
        (CLIENT, text, "appearance", "appearance: Appearance?"),
        // The doc comment above the registration types the handler's parameters.
        (SERVER, server, "storeId)", "storeId: number"),
        // So does the one above a statement that keeps what the registration returns.
        (SERVER, server, "plate)", "plate: string"),
        (SERVER, server, "plate)", "the plate to look for"),
        (CLIENT, text, "counted", "counted: number"),
    ] {
        let (l, c) = pos(source, needle, 0);
        let hover = client.hover_text(file, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }

    let signature = |client: &mut Client, needle: &str| {
        let (l, c) = pos(text, needle, 0);
        let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
        (result["signatures"][0]["label"].as_str().unwrap_or_default().to_string(), result["activeParameter"].clone())
    };
    assert_eq!(
        signature(&mut client, "2, vector3(0, 0, 0))\nTrigger"),
        (
            "AwaitServerCallback(name: string, storeId: number, index: number, location: vector3): boolean".into(),
            json!(2)
        ),
        "the source the server passes is left out"
    );
    assert_eq!(
        signature(&mut client, "1, 2, vector3(0, 0, 0))\nlocal price").0,
        "TriggerServerCallback(event: string, cb: fun(response: boolean), storeId: number, index: number, location: vector3)"
    );
    assert_eq!(signature(&mut client, "'water'").0, "await(name: string, item: string): integer");

    // Each family completes its own names.
    // Between the quotes of `('')`.
    let names = |client: &mut Client, needle: &str| {
        let (l, c) = pos(text, needle, needle.len() as u32 - 2);
        let mut labels = client.completion_labels(CLIENT, l, c);
        labels.sort();
        labels
    };
    assert_eq!(
        names(&mut client, "AwaitServerCallback('')"),
        ["countClamps", "getPlayerAppearance", "removeStoreLocation"]
    );
    assert_eq!(names(&mut client, "Shop:await('')"), ["shop:price"]);

    let (l, c) = pos(text, "removeStoreLocation", 3);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("callback `removeStoreLocation`") && hover.contains("storeId: number"), "{hover}");
    let result = client.request("textDocument/definition", client.position_params(CLIENT, l, c));
    assert!(result[0]["uri"].as_str().unwrap().ends_with("server/main.lua"), "{result}");
    assert_eq!(result[0]["range"]["start"]["line"], 4);
}

#[test]
fn callback_payloads_need_what_their_handler_requires_and_takes() {
    const SHARED: &str = "myresource/shared/config.lua";
    let mut client = Client::start(fixture_root());
    client.open_with(
        SHARED,
        "\
---@callback register
---@param name string
---@param handler fun(source: integer, ...): ...
function RegisterServerCallback(name, handler) end

---@callback await
---@param name string
---@param ... any
function AwaitServerCallback(name, ...) end

---@callback trigger
---@param event string
---@param cb fun(...)
---@param ... any
function TriggerServerCallback(event, cb, ...) end

Shop = {}

---@callback register shop
---@param name string
---@param handler function
function Shop.register(name, handler) end

---@callback await shop
---@param name string
---@param ... any
function Shop:await(name, ...) end
",
    );
    client.open_with(
        SERVER,
        "\
---@param num1 number
---@param num2 number
---@return number
RegisterServerCallback('add', function(source, num1, num2)
    return num1 + num2
end)

---@param label string
---@param amount? number
RegisterServerCallback('notify', function(source, label, amount) end)

---@param item string
Shop.register('shop:price', function(source, item) return 1 end)

RegisterServerCallback('untyped', function(source, a, b) end)
",
    );
    let text = "\
local sum = AwaitServerCallback('add')
local one = AwaitServerCallback('add', 1)
local both = AwaitServerCallback('add', 1, 2)
TriggerServerCallback('add', function(result) end)
TriggerServerCallback('add', function(result) end, 1, 2)
AwaitServerCallback('notify', 'hi')
AwaitServerCallback('add', ...)
AwaitServerCallback('add', GetValues())
AwaitServerCallback('untyped')
local price = Shop:await('shop:price')
AwaitServerCallback('unknown')
print(sum, one, both, price)
AwaitServerCallback('add', 1, 2, 3)
AwaitServerCallback('add', 1, 2, GetValues())
AwaitServerCallback('add', 1, GetValues())
AwaitServerCallback('untyped', 1, 2)
TriggerServerCallback('notify', function() end, 'hi', 5, true)
AwaitServerCallback('add', 'one', 2)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |at: u64, message: &str| ("missing-parameter".to_string(), at, message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["missing-parameter"]),
        [
            finding(
                line("AwaitServerCallback('add')"),
                "Callback 'add' is called with 0 arguments, but needs 2; 'num1' (number) will be nil"
            ),
            finding(
                line("AwaitServerCallback('add', 1)"),
                "Callback 'add' is called with 1 argument, but needs 2; 'num2' (number) will be nil"
            ),
            finding(
                line("function(result) end)"),
                "Callback 'add' is called with 0 arguments, but needs 2; 'num1' (number) will be nil"
            ),
            finding(
                line("Shop:await"),
                "Callback 'shop:price' is called with 0 arguments, but needs 1; 'item' (string) will be nil"
            ),
        ],
        "the player the server passes first, optional and undocumented parameters, open-ended calls and unknown names pass"
    );
    let redundant =
        |needle: &str, message: &str| ("redundant-parameter".to_string(), line(needle), message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["redundant-parameter"]),
        [
            redundant(
                "AwaitServerCallback('add', 1, 2, 3)",
                "Callback 'add' is called with 3 arguments, but its handler takes at most 2"
            ),
            redundant(
                "AwaitServerCallback('add', 1, 2, GetValues())",
                "Callback 'add' is called with 3 arguments, but its handler takes at most 2"
            ),
            redundant(
                "TriggerServerCallback('notify'",
                "Callback 'notify' is called with 3 arguments, but its handler takes at most 2"
            ),
        ],
        "a call after the handler's parameters counts as one value, and undocumented parameters take one each"
    );
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch"]),
        [(
            "param-type-mismatch".to_string(),
            line("AwaitServerCallback('add', 'one', 2)"),
            "Cannot assign `string` to parameter `num1` of type `number`".to_string()
        )]
    );
}

#[test]
fn new_files_register_callbacks_through_wrappers_of_other_files() {
    const NEW: &str = "myresource/callback-test.lua";
    let mut client = Client::start(fixture_root());
    client.open_with(
        "myresource/shared/config.lua",
        "\
---@callback register
---@param event string
---@param handler fun(source: number, ...): ...
function RegisterServerCallback(event, handler) end

---@callback await
---@param event string
---@param ... any
function AwaitServerCallback(event, ...) end
",
    );
    // Neither on disk nor listed in the manifest, so the file is indexed for the first time on open.
    let text = "\
---@param num1 number
---@param num2 number
RegisterServerCallback('add', function(source, num1, num2) end)

AwaitServerCallback('add')
";
    client.open_with(NEW, text);
    assert_eq!(
        findings(&mut client, NEW, &["missing-parameter"]),
        [(
            "missing-parameter".to_string(),
            pos(text, "AwaitServerCallback('add')", 0).0 as u64,
            "Callback 'add' is called with 0 arguments, but needs 2; 'num1' (number) will be nil".to_string()
        )]
    );
}

#[test]
fn callback_tags_complete_roles_and_the_families_in_use() {
    let mut client = Client::start(fixture_root());
    let wrappers = "\
Shop = {}
---@callback register shop
---@param name string
---@param handler function
function Shop.register(name, handler) end

---@callback await garage
---@param name string
---@param ... any
function AwaitGarage(name, ...) end

---@callback register
---@param name string
---@param handler function
function RegisterPlain(name, handler) end
";
    client.open_with("myresource/shared/config.lua", wrappers);
    let mut labels = |line: &str| {
        let text = format!("{line}\nfunction Wrapper(name, handler) end\n");
        client.open_with(CLIENT, &text);
        let mut labels = client.completion_labels(CLIENT, 0, line.len() as u32);
        labels.sort();
        labels
    };
    assert_eq!(labels("---@callback "), ["await", "register", "trigger"]);
    assert_eq!(labels("---@callback reg"), ["await", "register", "trigger"]);
    // Wrappers tagged without a family share the unnamed one, which is not offered.
    assert_eq!(labels("---@callback register "), ["garage", "shop"]);
    assert_eq!(labels("---@callback await sh"), ["garage", "shop"]);
    assert!(labels("---@callback call ").is_empty());
    assert!(labels("---@callback register shop ").is_empty());
}

#[test]
fn wrapper_call_snippets_stop_in_the_name_and_reopen_suggestions() {
    let wrappers = "\
---@callback register
---@param name string
---@param handler fun(source: integer, ...): ...
function RegisterServerCallback(name, handler) end

---@callback await
---@param name string
---@param ... any
function AwaitServerCallback(name, ...) end

---@callback trigger
---@param cb fun(...)
---@param event string
---@param ... any
function TriggerServerCallback(cb, event, ...) end
";
    let snippets = |capabilities: Value| -> Vec<(String, Value)> {
        let mut client = Client::start_with_capabilities(fixture_root(), capabilities);
        client.open_with("myresource/shared/config.lua", wrappers);
        let mut found = Vec::new();
        for typed in ["RegisterServerCallb", "AwaitServerCallb", "TriggerServerCallb"] {
            client.open_with(CLIENT, typed);
            let result =
                client.request("textDocument/completion", client.position_params(CLIENT, 0, typed.len() as u32));
            let items = result["items"].as_array().cloned().unwrap_or_default();
            let snippet = items.iter().find(|item| item["labelDetails"]["description"] == "snippet");
            let snippet = snippet.unwrap_or_else(|| panic!("{typed}: no call snippet in {result}"));
            found.push((snippet["insertText"].as_str().unwrap().to_string(), snippet["command"].clone()));
        }
        found
    };
    let snippet_client = json!({ "textDocument": { "completion": { "completionItem": { "snippetSupport": true } } } });
    let mut vscode = snippet_client.clone();
    vscode["experimental"] = json!({ "commands": { "commands": ["editor.action.triggerSuggest"] } });

    let reopen = json!({ "title": "Suggest callback names", "command": "editor.action.triggerSuggest" });
    let found = snippets(vscode);
    // A name to register is new, so the snippet keeps its placeholder and asks for no suggestions.
    assert_eq!(
        found[0],
        ("RegisterServerCallback('${1:name}', function(${2:source, ...})\n\t$0\nend)".into(), Value::Null)
    );
    assert_eq!(found[1], ("AwaitServerCallback('$1'$2)".into(), reopen.clone()));
    // The name is the first stop wherever the wrapper takes it.
    assert_eq!(found[2], ("TriggerServerCallback(function(${2:...})\n\t$0\nend, '$1'$3)".into(), reopen));

    let found = snippets(snippet_client);
    assert_eq!(found[1], ("AwaitServerCallback('$1'$2)".into(), Value::Null), "the client cannot reopen suggestions");
}

#[test]
fn undocumented_returns_merge_every_exit() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Appearance
---@field model string

---@return Appearance
local function fix(data) return data end

local function lookup(id)
    if not id then
        return
    end
    return fix(id)
end

local function pick(flag)
    if flag then return 'yes' end
    return 'no'
end

local function scan(list)
    for _, v in ipairs(list) do
        if v then return 1 end
    end
end

local function strict(flag)
    if flag then
        return 1
    else
        error('no flag')
    end
end

local function number(flag)
    if flag then return 1 end
    return 0.5
end

local function pair(flag)
    if flag then return 1, 'a' end
    return 2
end

local function unclear(value)
    if value then return value.whatever end
end

local looked = lookup(1)
local picked = pick(true)
local scanned = scan({})
local strictValue = strict(true)
local numbered = number(true)
local first, second = pair(true)
local unclearValue = unclear(1)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        // A bare `return` returns nil.
        ("looked", "looked: Appearance?"),
        ("picked", "picked: string"),
        // Running past the end of the loop returns nil.
        ("scanned", "scanned: integer?"),
        // `error` ends the function; it does not return nil.
        ("strictValue", "strictValue: integer"),
        ("numbered", "numbered: number"),
        ("first", "first: integer"),
        (", second", "second: string?"),
        // An unknown value merged with nil stays unknown instead of reading as nil.
        ("unclearValue", "unclearValue: unknown"),
    ] {
        let (l, c) = match needle.strip_prefix(", ") {
            Some(_) => pos(text, needle, 2),
            None => pos(text, &format!("local {needle}"), 6),
        };
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn returns_listed_on_one_line_type_each_value() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return boolean, string?
local function getName()
    if math.random(1, 2) == 2 then
        return false
    end
    return true, 'test'
end

---@return boolean ok, integer count, string? reason # why it failed
local function count() return true, 1 end

local found, name = getName()
local ok, amount, reason = count()
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("found, name", "found: boolean"),
        ("name = getName", "name: string?"),
        ("ok, amount", "ok: boolean"),
        ("amount, reason", "amount: integer"),
        ("reason = count", "reason: string?"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    let (l, c) = pos(text, "count() return", 0);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("local function count(): boolean, integer, string?"), "{hover}");
    assert!(findings(&mut client, CLIENT, &["return-type-mismatch", "missing-return"]).is_empty());
}

#[test]
fn locals_keep_the_type_of_the_call_that_declares_them() {
    let mut client = Client::start(fixture_root());
    let text = "---@return 'active'|'busy'|'ready'
local function GetState()
    return 'active'
end

---@return 1|2 level, 'low'|'high' label
local function GetLevel()
    return 1, 'low'
end

local state = GetState()
local copy = state
local level, label = GetLevel()
local changed = GetState()
changed = 'other'
local mode = 'dev'
print(state, copy, level, label, changed, mode)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("state = GetState", "local state: \"active\"|\"busy\"|\"ready\""),
        // A local that takes the value of another keeps its literals.
        ("copy = state", "local copy: \"active\"|\"busy\"|\"ready\""),
        ("level, label", "local level: 1|2"),
        ("label = GetLevel", "local label: \"low\"|\"high\""),
        // One that is assigned again holds them until then, and what is assigned later after that.
        ("changed = GetState", "local changed: \"active\"|\"busy\"|\"ready\""),
        ("changed, mode)", "local changed: string"),
        ("mode = 'dev'", "local mode: string"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn and_and_or_give_what_each_side_can_be() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Probe.Thing
---@field name string

---@return Probe.Thing
local function make() end
---@type boolean
local cond
---@type Probe.Thing?
local optional
local thing = make()
local untyped = {}
local picked = cond and 'a' or 'b'
local mixed = cond and 1 or 'x'
local guarded = optional and optional.name
local always = thing and thing.name
local partial = cond and 'a'
local skipped = cond and nil or 5
local kept = cond and optional or nil
local flag = cond and true or false
local compared = cond and #untyped == 1
local fallback = cond or 'x'
local unknown = untyped and untyped.missing
print(picked, mixed, guarded, always, partial, skipped, kept, flag, compared, fallback, unknown)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("picked =", "local picked: string\n"),
        ("mixed =", "local mixed: integer|string\n"),
        // `a and b` is the `nil` or `false` of `a`, or `b`...
        ("guarded =", "local guarded: string?\n"),
        ("always =", "local always: string\n"),
        ("partial =", "local partial: string|false\n"),
        // ...and `a or b` what `a` holds when it holds a value, or `b`.
        ("skipped =", "local skipped: integer\n"),
        ("kept =", "local kept: Probe.Thing? {"),
        ("flag =", "local flag: boolean\n"),
        ("compared =", "local compared: boolean\n"),
        ("fallback =", "local fallback: true|string\n"),
        ("unknown =", "local unknown: unknown\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn guards_narrow_the_locals_they_test() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Guarded
---@field job string

---@return string?
local function getName()
    if math.random(1, 2) == 2 then
        return
    end
    return 'Joe'
end

---@param player Guarded?
---@param flag boolean
---@param label string?
---@param count integer?
local function check(player, flag, label, count)
    if player then
        print(player.job) -- held
    else
        print(player) -- missing
    end
    if flag == false then
        return flag -- off
    end
    print(flag) -- on
    local shown = label and label:upper() -- and
    if label == nil or count == nil then
        return shown
    end
    print(label, count) -- both
    for _ = 1, 2 do
        local item = getName()
        if not item then
            goto continue
        end
        print(item) -- item held
        ::continue::
        print(item) -- item skipped
    end
    local again = getName()
    assert(again, 'no name')
    print(again) -- asserted
    local changed = getName()
    if not changed then
        changed = getName()
    end
    if not changed then
        return
    end
    print(changed) -- reassigned
end

local name = getName()
if not name then
    print(name) -- inside
    return check()
end
print(name) -- after
local copy = name
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("name = getName()\nif", "name: string?\n"),
        ("name) -- inside", "name: nil\n"),
        ("name) -- after", "name: string\n"),
        ("copy = name", "copy: string\n"),
        ("player.job) -- held", "player: Guarded {"),
        ("player) -- missing", "player: nil\n"),
        ("flag -- off", "flag: false\n"),
        ("flag) -- on", "flag: true\n"),
        ("label:upper() -- and", "label: string\n"),
        ("label, count) -- both", "label: string\n"),
        ("count) -- both", "count: integer\n"),
        ("item) -- item held", "item: string\n"),
        // A `goto` that skipped the guard reaches the code after its label.
        ("item) -- item skipped", "item: string?\n"),
        ("again) -- asserted", "again: string\n"),
        // A guard narrows what the assignments before it give a local that is assigned again.
        ("changed) -- reassigned", "changed: string\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn locals_that_are_assigned_again_hold_what_reaches_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return string?
local function maybe() end
---@param cb fun(value: string)
local function each(cb) end
local cond = math.random() > 0.5

local filled = maybe()
filled = filled or 'default'
print(filled) -- filled
local branch = maybe()
if not branch then
    branch = 'x'
end
print(branch) -- joined
local cleared = 'x'
if cond then
    cleared = nil
end
print(cleared) -- one branch
local later
print(later) -- not yet
later = 5
print(later) -- assigned
---@type string?
local typed = nil
typed = 'x'
print(typed) -- bounded
---@type integer
local wrong = 1
wrong = 'x'
print(wrong) -- mismatch
local looped = 'start'
while cond do
    print(looped) -- loop start
    looped = nil
end
print(looped) -- after loop
local counted = maybe()
for _ = 1, 3 do
    counted = counted or 'y'
end
print(counted) -- after for
local repeated = maybe()
repeat
    repeated = 'z'
until cond
print(repeated) -- after repeat
local endless = maybe()
while true do
    if endless then
        break
    end
    endless = maybe()
end
print(endless) -- after break
local captured = maybe()
captured = captured or 'x'
local function read()
    print(captured) -- closure
end
captured = nil
local function clear()
    print(captured) -- closure after
end
local found
each(function(value)
    found = value
end)
print(found) -- callback
local jumped = maybe()
for _ = 1, 2 do
    if not jumped then
        goto continue
    end
    print(jumped) -- before label
    ::continue::
    print(jumped) -- after label
    jumped = 'w'
end
local flag = true
for _, a in ipairs({ 1 }) do
    for _, b in ipairs({ a }) do
        if b then
            flag = false
            break
        end
        print(flag) -- inner loop
    end
    if flag == false then
        break
    end
end
local ready = false
local function waitReady()
    if not ready then
        each(function()
            print(ready) -- given later
        end)
    end
end
local function setReady()
    ready = true
end
print(read, clear, waitReady, setReady)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("filled) -- filled", "filled: string\n"),
        ("branch) -- joined", "branch: string\n"),
        ("cleared) -- one branch", "cleared: string?\n"),
        ("later) -- not yet", "later: nil\n"),
        ("later) -- assigned", "later: integer\n"),
        // An annotation keeps the parts the value may be...
        ("typed) -- bounded", "typed: string\n"),
        // ...and all of it when the value fits none of them, which `assign-type-mismatch` reports.
        ("wrong) -- mismatch", "wrong: integer\n"),
        // A loop starts with what the runs before it leave.
        ("looped) -- loop start", "looped: string?\n"),
        ("looped) -- after loop", "looped: string?\n"),
        ("counted) -- after for", "counted: string?\n"),
        ("repeated) -- after repeat", "repeated: string\n"),
        ("endless) -- after break", "endless: string\n"),
        // A function runs any time after it is created, also after the assignments that follow it.
        ("captured) -- closure\n", "captured: string?\n"),
        ("captured) -- closure after", "captured: nil\n"),
        // The function a call is given may have run once the call returns, and `local found` makes
        // way for the values assigned later.
        ("found) -- callback", "found: string\n"),
        ("jumped) -- before label", "jumped: string\n"),
        ("jumped) -- after label", "jumped: string?\n"),
        ("flag) -- inner loop", "flag: boolean\n"),
        // What a guard told about a value holds no more once something may give it again.
        ("ready) -- given later", "ready: boolean\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    // An assignment shows the value it gives.
    let (l, c) = pos(text, "later = 5", 0);
    assert!(client.hover_text(CLIENT, l, c).contains("later: integer\n"));
}

#[test]
fn a_type_line_on_a_reassignment_types_the_value_it_assigns() {
    let mut client = Client::start(fixture_root());
    let text = "---@alias Test.Data { job: string }

local points = {}
local function reset()
    ---@type vector2[]
    points = { vec(1.0, 2.0) }
    print(points) -- typed
end
local function draw()
    for i = 1, #points do
        points[i] = vec(points[i].x, points[i].y, 3.0)
        local corner = points[i]
        print(points, corner.z) -- drawn
    end
end

local cached = nil
RegisterNetEvent('test:cached', function(data)
    ---@type Test.Data
    cached = data
end)
local holder = {}
RegisterNetEvent('test:holder', function(data)
    ---@type Test.Data
    holder = data
end)
local function read()
    return cached, holder -- read
end
print(reset, draw, read)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("points) -- typed", "local points: vector2[]"),
        // Code that runs at other times may fill the local's own table with other values.
        ("points, corner.z) -- drawn", "local points: table"),
        ("corner = points", "local corner: unknown"),
        // A local declared without a type, or with a table that is given a value of no known type,
        // has the type the line gives.
        ("cached, holder -- read", "local cached: Test.Data"),
        ("holder -- read", "job: string"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    assert_eq!(findings(&mut client, CLIENT, &["undefined-field"]), []);
}

#[test]
fn locals_that_are_assigned_again_are_checked_with_what_reaches_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return 'active'|'busy'
local function getState() end
local function untyped(value) return value end
---@param count integer
local function wants(count) end

local state = getState()
if state == 'other' then end
state = getState()
if state == 'idle' then end
state = tostring(state)
if state == 'other' then end
local loose = getState()
loose = untyped(loose)
if loose == 'other' then end
local name = 'x'
name = name .. 'y'
wants(name)
local mixed = 1
if untyped(true) then
    mixed = untyped(mixed)
end
wants(mixed)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, code: &str, message: &str| {
        (code.to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["impossible-comparison", "param-type-mismatch"]),
        [
            finding(
                "state == 'other' then end\nstate = getState",
                "impossible-comparison",
                "Comparing `\"active\"|\"busy\"` with `\"other\"` is always false",
            ),
            finding(
                "state == 'idle'",
                "impossible-comparison",
                "Comparing `\"active\"|\"busy\"` with `\"idle\"` is always false",
            ),
            finding(
                "wants(name)",
                "param-type-mismatch",
                "Cannot assign `string` to parameter `count` of type `integer`",
            ),
        ],
        "a local that is assigned again has the declared types of the values that reach it, and none when \
         one of them has none"
    );
}

#[test]
fn calls_that_yield_let_other_code_change_locals() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@type 'pending'|'done'
local status = 'pending'
RegisterNetEvent('probe:done', function() status = 'done' end)
---@type 'idle'|'busy'
local mode = 'idle'
AddEventHandler('probe:busy', function() mode = 'busy' end)
local function sleep(ms) Wait(ms) end

CreateThread(function()
    if status == 'pending' then
        while status ~= 'done' do Wait(0) end
    end
    if mode ~= 'idle' then return end
    Wait(5000)
    if mode == 'busy' then return end
    sleep(10)
    if mode == 'busy' then return end
    local result = lib.callback.await('probe:get')
    if mode == 'busy' then return end
    print(result)
    if mode == 'busy' then return end
end)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("impossible-comparison".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["impossible-comparison"]),
        [finding("mode == 'busy' then return end\nend)", "Comparing `\"idle\"` with `\"busy\"` is always false")],
        "`Wait`, a function of the file that waits and `await` let the handlers run, while `print` does not"
    );
}

#[test]
fn functions_that_runtime_functions_run_later_leave_the_narrowing_alone() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@type table?
local buffer
---@type table?
local other

function Log(arg)
    if not buffer then
        buffer = {}
        SetTimeout(500, function() buffer = nil end)
    end
    buffer[1] = arg
    if not other then other = {} end
    pcall(function() other = nil end)
    other[1] = arg
end

function Later(arg)
    if buffer then
        Wait(0)
        buffer[2] = arg
    end
end
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("need-check-nil".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["need-check-nil"]),
        [
            finding("other[1]", "`other` may be nil: its type here is `table?`"),
            finding("buffer[2]", "`buffer` may be nil: its type here is `table?`"),
        ],
        "`SetTimeout` runs its function once the code yields, while `pcall` runs it right away"
    );
}

#[test]
fn assignments_narrow_the_fields_of_locals() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Probe.Entry
---@field length? number
---@field radius number
---@field vehicle? integer

---@param n number
local function needsNumber(n) end

---@param entry Probe.Entry
local function add(entry)
    entry.length = entry.length or entry.radius * 2
    needsNumber(entry.length)
    entry.vehicle = GetGameTimer()
    needsNumber(entry.vehicle)
end

---@param entry Probe.Entry
local function later(entry)
    entry.length = 5
    CreateThread(function() needsNumber(entry.length) end)
    Wait(0)
    needsNumber(entry.length)
end
print(add, later)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str| {
        let message = "Cannot assign `number?` to parameter `n` of type `number`";
        ("param-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch"]),
        [finding("function() needsNumber(entry.length)"), finding("needsNumber(entry.length)\nend")],
        "a field holds the value assigned to it until a function that runs later, or a yield, may find another"
    );
}

#[test]
fn locals_that_hold_what_type_gives_narrow_values_assigned_before_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param s string
local function needsString(s) end
---@return string|number
local function either() return 1 end

---@param value string|number
local function show(value)
    value = value or 'none'
    local kind = type(value)
    if kind == 'string' then needsString(value) end
    local before = type(value)
    value = either()
    if before == 'string' then needsString(value) end
end
print(show)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str| {
        let message = "Cannot assign `string|number` to parameter `s` of type `string`";
        ("param-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch"]),
        [finding("before == 'string'")],
        "`type(value)` tells about the value the local holds until it is assigned again"
    );
}

#[test]
fn many_functions_that_assign_the_same_locals_stay_fast() {
    let mut client = Client::start(fixture_root());
    // Each handler may run while any other has assigned the locals, and the guards in each tell about
    // all of what the others give.
    let mut text =
        "---@return string?\nlocal function maybe() end\nlocal busy = false\nlocal current = maybe()\n".to_string();
    text.push_str(
        &"AddEventHandler('probe', function()\n    if busy then return end\n    busy = true\n    current = maybe()\n    \
          busy = false\nend)\n"
            .repeat(600),
    );
    text.push_str("print(busy, current) -- last\n");
    client.open_with(CLIENT, &text);
    let started = Instant::now();
    for (needle, expected) in
        [("busy, current) -- last", "busy: boolean\n"), ("current) -- last", "current: string?\n")]
    {
        let (line, character) = pos(&text, needle, 0);
        let hover = client.hover_text(CLIENT, line, character);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
}

#[test]
fn guards_narrow_the_fields_of_locals_until_something_may_change_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Probe.Job
---@field name string
---@field grade integer?

---@class Probe.Data
---@field job Probe.Job?
---@field label string?
---@field mode 'a'|'b'|nil

---@param data Probe.Data
---@param other Probe.Data
---@param key string
local function check(data, other, key)
    if data.job then
        print(data.job) -- held
        print(data.job.grade) -- deeper
        local job = data.job
        print(job) -- copied
    end
    print(data.job) -- after
    if data.job and data.job.grade then
        print(data.job.grade) -- both
    end
    if data.label then
        other.label = nil
        print(data.label) -- same name
    end
    if data.label then
        reset(data)
        print(data.label) -- passed
    end
    if data.label then
        data.label:upper()
        print(data.label) -- method of the field
    end
    if data.mode == 'a' then
        print(data.mode) -- compared
        data = other
        print(data.mode) -- local assigned
    end
    if data['label'] then
        print(data['label']) -- index
    end
    if data.label then
        CreateThread(function()
            print(data.label) -- later
        end)
    end
    if data.label then
        for _ = 1, 2 do
            print(data.label) -- loop
            data.label = nil
        end
    end
    if data.job and data.job.grade then
        local job = data.job
        reset(job)
        print(data.job.grade) -- copy passed
    end
    if data.label then
        local copy = data
        copy[key] = nil
        print(data.label) -- key not known
    end
    if data.label then
        local list = {}
        list[#list + 1] = data.label
        print(data.label) -- other table
    end
    if data.label then
        clear({ data })
        print(data.label) -- in a table
    end
    if data.label then
        local list = { data }
        clear(list)
        print(data.label) -- in a local table
    end
    if data.label then
        Wait(0)
        print(data.label) -- waited
    end
end
check()
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("job) -- held", "Probe.Job {"),
        ("grade) -- deeper", "grade: integer?\n"),
        ("job) -- copied", "local job: Probe.Job {"),
        ("job) -- after", "Probe.Job? {"),
        ("grade) -- both", "grade: integer\n"),
        // An assignment to a field of that name may be one to this field, through another name.
        ("label) -- same name", "label: string?\n"),
        // A call that is given the table may change its fields.
        ("label) -- passed", "label: string?\n"),
        ("label) -- method of the field", "label: string\n"),
        ("mode) -- compared", "mode: \"a\"\n"),
        ("mode) -- local assigned", "mode: \"a\"|\"b\"|nil\n"),
        ("label']) -- index", "label: string\n"),
        // A function runs later, when the field may hold anything.
        ("label) -- later", "label: string?\n"),
        // The loop starts again after the field is assigned.
        ("label) -- loop", "label: string?\n"),
        // A call that is given a copy of the table, or a table built with it, may change its fields,
        // as may an assignment to a key of the copy that is not known.
        ("grade) -- copy passed", "grade: integer?\n"),
        ("label) -- key not known", "label: string?\n"),
        ("label) -- other table", "label: string\n"),
        ("label) -- in a table", "label: string?\n"),
        ("label) -- in a local table", "label: string?\n"),
        // Other code runs while the call yields.
        ("label) -- waited", "label: string?\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn checks_read_the_fields_that_guards_narrow() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Probe.Slot
---@field label string?
---@field mode 'a'|'b'|nil
---@field spots vector3|vector3[]

---@param slot Probe.Slot
local function show(slot)
    if type(slot.spots) ~= 'table' then
        slot.spots = { slot.spots }
    end
    if slot.label then
        local label = slot.label
        print(label:upper())
    end
    local plain = slot.label
    print(plain:upper())
    if slot.mode == 'a' then
        if slot.mode == 'b' then return end
    end
    print(slot.label:upper())
end
show()
";
    client.open_with(CLIENT, text);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": { "need-check-nil": "warning" } } } } }),
    );
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    assert_eq!(
        findings(&mut client, CLIENT, &["need-check-nil", "impossible-comparison", "missing-fields"]),
        [
            (
                "need-check-nil".to_string(),
                line("plain:upper"),
                "`plain` may be nil: its type here is `string?`".to_string()
            ),
            (
                "impossible-comparison".to_string(),
                line("slot.mode == 'b'"),
                "Comparing `\"a\"` with `\"b\"` is always false".to_string()
            ),
        ],
        "a local takes the type a guard narrows a field to, fields themselves are not checked for nil, and a          field is given values of its declared type"
    );
}

#[test]
fn guards_that_read_from_a_local_narrow_it() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Reader
---@field job string?
---@field get fun(self: Reader): string

---@param data Reader?
---@param other Reader?
---@param third Reader?
---@param fourth Reader?
local function read(data, other, third, fourth)
    if data?.job then
        print(data) -- safe field
    end
    if other?.job == 'police' then
        print(other) -- compared field
    end
    if third:get() then
        print(third) -- method
    end
    if not fourth?.job then
        print(fourth) -- not read
        return
    end
    print(fourth) -- after read
    if data?.job == nil then
        print(data) -- compared with nil
    end
    if data?.job ~= nil then
        print(data) -- not nil
    end
end
read()
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("data) -- safe field", "data: Reader {"),
        ("other) -- compared field", "other: Reader {"),
        ("third) -- method", "third: Reader {"),
        // A false `fourth?.job` may come from a `nil` field.
        ("fourth) -- not read", "fourth: Reader? {"),
        ("fourth) -- after read", "fourth: Reader {"),
        ("data) -- compared with nil", "data: Reader? {"),
        ("data) -- not nil", "data: Reader {"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn elseif_conditions_see_what_the_conditions_before_them_ruled_out() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param items? table
---@param weight number
local function create(items, weight)
    if not items then
        print('none')
    elseif weight == 0 and next(items) then -- second
        print(items) -- body
    elseif items.x then -- third
        print(1)
    end
end

---@param items? table
---@param weight number
local function other(items, weight)
    if weight == 0 then
        print('none')
    elseif items.x then -- nothing ruled out
        print(1)
    end
end
create()
other()
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("items) then -- second", "items: table\n"),
        ("items) -- body", "items: table\n"),
        ("items.x then -- third", "items: table\n"),
        ("items.x then -- nothing ruled out", "items: table?\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": { "need-check-nil": "warning" } } } } }),
    );
    let (line, _) = pos(text, "items.x then -- nothing ruled out", 0);
    assert_eq!(
        findings(&mut client, CLIENT, &["need-check-nil"]),
        [("need-check-nil".to_string(), line as u64, "`items` may be nil: its type here is `table?`".to_string())],
        "an `elseif` after `if not items` reads a value"
    );
}

#[test]
fn guards_pick_the_set_of_values_a_call_returned() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return false | (string, string)
local function getName()
    if math.random(1, 2) == 2 then
        return false
    end
    return 'Joe', 'Doe'
end

local function find(id)
    if id == 0 then
        return nil, 'not found'
    end
    return 'name'
end

---@type fun(): false | (integer, string)
local getter

local function use()
    local found, problem = find(1)
    if found then
        print(found, problem) -- held
    else
        print(found, problem) -- failed
    end
    local id, label = getter()
    if id == false then
        return label -- no label
    end
    print(id, label) -- typed
    local first, last = getName()
    last = last or 'unknown'
    if not first then
        return
    end
    print(first, last) -- reassigned
end

local firstname, lastname = getName()
if not firstname then
    print(firstname, lastname) -- inside
    return use()
end
print(firstname, lastname) -- after
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("getName()\n    if math", "local function getName(): false | (string, string)"),
        // The sets of an undocumented function are those its `return`s pass.
        ("find(id)", "local function find(id): (nil, string) | string"),
        ("firstname, lastname = getName", "firstname: false|string\n"),
        ("lastname = getName", "lastname: string?\n"),
        ("firstname, lastname) -- inside", "firstname: false\n"),
        ("lastname) -- inside", "lastname: nil\n"),
        ("firstname, lastname) -- after", "firstname: string\n"),
        ("lastname) -- after", "lastname: string\n"),
        ("found, problem) -- held", "found: string\n"),
        ("problem) -- held", "problem: nil\n"),
        ("found, problem) -- failed", "found: nil\n"),
        ("problem) -- failed", "problem: string\n"),
        ("label -- no label", "label: nil\n"),
        ("id, label) -- typed", "id: integer\n"),
        ("label) -- typed", "label: string\n"),
        // A local that is assigned again holds what it is given, and tells nothing about the others.
        ("first, last) -- reassigned", "first: string\n"),
        ("last) -- reassigned", "last: string\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn type_checks_and_literal_comparisons_narrow_the_locals_they_test() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Checked
---@field name string

---@return integer|string|fun()|nil
local function many() end

---@return 'a'|'b'|nil
local function letter() end

---@param action 'open'|'close'
local function toggle(action)
    if action ~= 'open' and action ~= 'close' then
        print(action == 'open') -- neither
    end
    if action == 'open' then return end
    if action == 'close' then return end
    print(action) -- ruled out
end
toggle('open')

---@param id string|integer
local function aliased(id)
    local type = type
    if type(id) == 'string' then
        print(id) -- aliased type
    end
end
aliased(1)

---@param data Checked|string|nil
---@param coords vector3|table
---@param count integer
local function check(data, coords, count, cb, decoded)
    if type(data) == 'table' then
        print(data) -- class
    elseif type(data) == 'nil' then
        print(data) -- nil kind
    else
        print(data) -- rest
    end
    if type(coords) == 'vector3' then
        print(coords) -- vector
    end
    if type(cb) == 'function' then
        print(cb) -- callback
    end
    if type(count) == 'string' then
        print(count) -- defensive
    end
    if count == 0 then
        print(count) -- zero
    end
    local value = many()
    if type(value) == 'number' then
        print(value) -- number
    end
    if not (type(value) == 'string') then
        print(value) -- not string
    end
    if type(value) == 'string' or type(value) == 'number' then
        print(value) -- either
    end
    if math.type(value) == 'integer' then
        print(value) -- integer
    end
    if table.type(value) == 'array' then
        print(value) -- other function
    end
    local kind = type(value)
    if kind == 'string' then
        print(value) -- through a local
    end
    local which = letter()
    if which == 'a' then
        if which == 'b' then return end
        print(which) -- equal
    elseif which then
        print(which) -- other letter
    end
    if which ~= 'a' then
        print(which) -- differs
    end
    if type(decoded) ~= 'table' then
        return
    end
    print(decoded) -- after
end
check()
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("data) -- class", "data: Checked {"),
        ("data) -- nil kind", "data: nil\n"),
        ("data) -- rest", "data: string\n"),
        // CfxLua's `type` names its vectors.
        ("coords) -- vector", "coords: vector3 {"),
        // A value of no known type is of the kind it is checked for.
        ("cb) -- callback", "cb: function\n"),
        ("decoded) -- after", "decoded: table\n"),
        // So is one whose type has no value of that kind, as code checks for what annotations leave out.
        ("count) -- defensive", "count: string\n"),
        ("count) -- zero", "count: 0\n"),
        ("value) -- number", "value: integer\n"),
        ("value) -- not string", "value: integer|(fun())|nil\n"),
        ("value) -- either", "value: integer|string\n"),
        ("value) -- integer", "value: integer\n"),
        ("value) -- other function", "value: integer|string|(fun())|nil\n"),
        ("value) -- through a local", "value: string\n"),
        ("which) -- equal", "which: \"a\"\n"),
        ("which) -- other letter", "which: \"b\"\n"),
        ("which) -- differs", "which: \"b\"?\n"),
        // Guards that leave no value keep the declared type, so `action == 'open'` is not reported.
        ("action == 'open') -- neither", "action: \"open\"|\"close\"\n"),
        ("action) -- ruled out", "action: \"open\"|\"close\"\n"),
        // `local type = type` keeps the checks.
        ("id) -- aliased type", "id: string\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    let (line, _) = pos(text, "which == 'b'", 0);
    assert_eq!(
        findings(&mut client, CLIENT, &["impossible-comparison"]),
        [(
            "impossible-comparison".to_string(),
            line as u64,
            "Comparing `\"a\"` with `\"b\"` is always false".to_string()
        )],
        "comparisons see the narrowed types"
    );
}

#[test]
fn guards_that_rule_out_every_declared_value_read_as_lua_language_server_does() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param entities number[]
local function plain(entities)
    if type(entities) ~= 'table' then
        print(entities) -- plain branch
        entities = { entities }
    end
    print(entities) -- plain after
end

---@param entities number[] | number
local function either(entities)
    if type(entities) ~= 'table' then
        print(entities) -- either branch
        entities = { entities }
        print(entities) -- either assigned
    end
    print(entities) -- either after
end

---@param entities number[] | number
local function checked(entities)
    if type(entities) == 'number' then
        print(entities) -- checked branch
        entities = { entities }
    end
    print(entities) -- checked after
end

---@param count number
---@param name string
---@param both number|string
---@param missing nil
---@param action 'open'|'close'
local function guards(count, name, both, missing, action)
    if not count then print(count) end -- not
    if count == nil then print(count) end -- equal nil
    if count ~= nil then print(count) else print(count) end -- else of not nil
    if count == false then print(count) end -- false
    if name == 5 then print(name) end -- integer
    if count == 'x' then print(count) end -- string
    if type(both) ~= 'number' and type(both) ~= 'string' then print(both) end -- neither kind
    if missing then print(missing) end -- truthy nil
    if action ~= 'open' and action ~= 'close' then print(action) end -- other literal
end
print(plain, either, checked, guards)
";
    client.open_with(CLIENT, text);
    assert_eq!(
        findings(&mut client, CLIENT, &["assign-type-mismatch"]),
        [],
        "a `number[]` that is not a table is unknown, which `{{ entities }}` stores without a mismatch"
    );
    for (needle, expected) in [
        ("entities) -- plain branch", "entities: unknown\n"),
        ("entities) -- plain after", "entities: number[]\n"),
        ("entities) -- either branch", "entities: number\n"),
        ("entities) -- either assigned", "entities: number[]\n"),
        ("entities) -- either after", "entities: number[]\n"),
        ("entities) -- checked branch", "entities: number\n"),
        ("entities) -- checked after", "entities: number[]\n"),
        // A check for a missing value leaves `nil`...
        ("count) end -- not", "count: nil\n"),
        ("count) end -- equal nil", "count: nil\n"),
        ("count) end -- else of not nil", "count: nil\n"),
        // ...a comparison with a literal of another kind leaves that kind...
        ("count) end -- false", "count: boolean\n"),
        ("name) end -- integer", "name: integer\n"),
        ("count) end -- string", "count: string\n"),
        // ...and a check that the value is not of its kind, or holds a value, leaves nothing known.
        ("both) end -- neither kind", "both: unknown\n"),
        ("missing) end -- truthy nil", "missing: unknown\n"),
        // Other comparisons with literals keep the declared type.
        ("action) end -- other literal", "action: \"open\"|\"close\"\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn values_of_unknown_type_that_typed_targets_take_are_reported_by_no_unknown() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Probe.Box
---@field label string

---@param n number
local function takesNumber(n) end
---@param value any
local function takesAny(value) end

---@param entities number[]
local function target(entities, options)
    if type(entities) ~= 'table' then
        entities = { entities }
    end
    takesNumber(options.count)
    return entities
end

---@type number
local typed = Undefined()
takesNumber(Undefined)
takesAny(Undefined)
---@type Probe.Box
local box = { label = Undefined }
box.label = Undefined

---@return string
local function name()
    return Undefined
end

---@return string
local function call()
    return Undefined()
end
print(target, typed, name, box, call)
";
    client.open_with(CLIENT, text);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": { "no-unknown": "warning" } } } } }),
    );
    let finding =
        |needle: &str, message: &str| ("no-unknown".to_string(), pos(text, needle, 0).0 as u64, message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["no-unknown"]),
        [
            finding("target(entities, options)", "Parameter `options` has no type; add `---@param options <type>`"),
            finding(
                "entities = { entities }",
                "The type of the value assigned to field `[1]` of type `number` is unknown"
            ),
            finding("local typed", "The type of the value assigned to `typed` of type `number` is unknown"),
            finding(
                "takesNumber(Undefined)",
                "The type of the value passed to parameter `n` of type `number` is unknown"
            ),
            finding(
                "label = Undefined }",
                "The type of the value assigned to field `label` of type `string` is unknown"
            ),
            finding(
                "box.label = Undefined",
                "The type of the value assigned to field `label` of type `string` is unknown"
            ),
            finding("return Undefined\n", "The type of return value #1 of type `string` is unknown"),
            finding("return Undefined()", "The type of return value #1 of type `string` is unknown"),
        ],
        "a value of unknown type is reported where a declared type takes it, but not for `any`, nor when it is read \
         from a local that is reported itself"
    );
}

#[test]
fn missing_doc_rules_report_when_turned_on_in_the_editor() {
    let codes = ["missing-global-doc", "missing-local-export-doc", "incomplete-signature-doc"];
    let rules: serde_json::Map<String, Value> = codes.iter().map(|code| (code.to_string(), json!("warning"))).collect();
    let mut client =
        Client::start_with_options(fixture_root(), json!({}), json!({ "diagnostics": { "rules": rules } }));
    let text = "\
---@param id integer
function GetName(id) return tostring(id) end

local function format(text) return text end
exports('Format', format)
";
    client.open_with(CLIENT, text);
    let finding = |code: &str, line: u64, message: &str| (code.to_string(), line, message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &codes),
        [
            finding("incomplete-signature-doc", 1, "incomplete signature: return value #1 has no @return annotation"),
            finding("missing-global-doc", 1, "return value #1 of global function 'GetName' has no @return annotation"),
            finding(
                "missing-local-export-doc",
                3,
                "parameter 'text' of exported local function 'format' has no @param annotation"
            ),
            finding(
                "missing-local-export-doc",
                3,
                "return value #1 of exported local function 'format' has no @return annotation"
            ),
        ]
    );

    // A parameter that the function type a function is passed as names needs no `@param`, also
    // where only the types the server reads say what that function type is.
    let text = "\
---@class Probe.Bus
---@field on fun(self: Probe.Bus, name: string, callback: fun(player: table, ...))

---@type Probe.Bus
local bus = GetBus()
---@param reason string
bus:on('dropped', function(player, reason, extra) end)
";
    client.change(CLIENT, 2, text);
    assert_eq!(
        findings(&mut client, CLIENT, &codes),
        [finding("incomplete-signature-doc", 6, "incomplete signature: parameter 'extra' has no @param annotation")]
    );
}

#[test]
fn casts_change_the_type_of_a_local_from_their_line_on() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return string|integer|nil
local function get() end

local value = get()
print(value) -- before
---@cast value string
print(value) -- cast
local other = get()
---@cast other -?
print(other) -- without nil
---@cast other +boolean, -integer
print(other) -- added and removed
local scoped = get()
if scoped then
    ---@cast scoped integer
    print(scoped) -- in block
end
print(scoped) -- after block
local changed = get()
---@cast changed string
changed = changed:upper()
print(changed) -- reassigned
local guarded = get()
if type(guarded) == 'string' then
    ---@cast guarded integer
    print(guarded) -- cast over guard
end
---@cast guarded string?
if guarded then
    print(guarded) -- guard over cast
end
local converted = get()
if type(converted) == 'number' then
    converted = tostring(converted)
    ---@cast converted string
end
print(converted) -- after branch
local nested = get()
if nested then
    if type(nested) == 'number' then
        print(nested)
        ---@cast nested integer
    end
end
print(nested) -- after nested branch
local branched = get()
if branched == 1 then
    print(branched)
    ---@cast branched boolean
elseif branched == 'a' then
    print(branched) -- next branch
end
local looped = get()
for _ = 1, 2 do
    print(looped)
    ---@cast looped integer
end
print(looped) -- after loop
local called = get()
local function body()
    print(called)
    ---@cast called integer
end
body()
print(called) -- after function
local typing = get()
if not typing then return end
---@cast typing
print(typing) -- no type yet
---@cast typing, other string
print(typing) -- no name
local plain = get()
plain = get()
if plain then
    print(plain) -- reassigned guard
end
local data = get()
data = get()
---@cast data string?
if data then
    print(data) -- guard after cast
    data = get()
    print(data) -- assigned after guard
end
local function later()
    print(value) -- closure
end
later()
local function untyped(x) return x end
local loose = untyped(1)
---@cast loose +string
print(loose) -- added to unknown
if loose then
    print(loose) -- guarded unknown
end
---@cast loose -string
print(loose) -- removed from unknown
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("value) -- before", "value: string|integer|nil\n"),
        ("value) -- cast", "value: string\n"),
        ("other) -- without nil", "other: string|integer\n"),
        ("other) -- added and removed", "other: string|boolean\n"),
        ("scoped) -- in block", "scoped: integer\n"),
        // A cast gives the local a value, as an assignment does: where the ways meet, it joins what
        // the others leave, here the `nil` that skips the `if`...
        ("scoped) -- after block", "scoped: integer?\n"),
        // ...and the local holds it until it is assigned again.
        ("changed:upper()", "changed: string\n"),
        ("changed) -- reassigned", "changed: string\n"),
        ("guarded) -- cast over guard", "guarded: integer\n"),
        ("guarded) -- guard over cast", "guarded: string\n"),
        // On the last line of an `if` branch, a cast types what the branch leaves for the code after
        // the `if`, beside what the other ways leave, but not for its other branches.
        ("converted) -- after branch", "converted: string?\n"),
        ("nested) -- after nested branch", "nested: integer|string|nil\n"),
        ("branched) -- next branch", "branched: \"a\"\n"),
        // A loop may run no time, and a function runs on its own.
        ("looped) -- after loop", "looped: string|integer|nil\n"),
        ("called) -- after function", "called: string|integer|nil\n"),
        // A line with no type keeps the guards before it.
        ("typing) -- no type yet", "typing: string|integer\n"),
        ("typing) -- no name", "typing: string|integer\n"),
        // A guard on a local that is assigned again narrows what it is given, and the type a cast
        // before the guard gives, up to the next assignment.
        ("plain) -- reassigned guard", "plain: string|integer\n"),
        ("data) -- guard after cast", "data: string\n"),
        ("data) -- assigned after guard", "data: string|integer|nil\n"),
        ("value) -- closure", "value: string\n"),
        // Adding to a type nothing tells leaves a value that may still be anything else.
        ("loose) -- added to unknown", "loose: string|unknown\n"),
        ("loose) -- guarded unknown", "loose: string|unknown\n"),
        ("loose) -- removed from unknown", "loose: unknown\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn casts_follow_the_ways_the_code_runs() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return string|number
local function get() end

local value = get()
if math.random() > 0.5 then
    ---@cast value string
    print(value) -- branch
end
print(value) -- joined
local only = get()
if math.random() > 0.5 then
    ---@cast only string
else
    return
end
print(only) -- only way
local block = get()
do
    ---@cast block number
end
print(block) -- after do
local added = get()
if math.random() > 0.5 then
    ---@cast added +boolean
end
print(added) -- added on one way
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("value) -- branch", "value: string\n"),
        ("value) -- joined", "value: string|number\n"),
        ("only) -- only way", "only: string\n"),
        ("block) -- after do", "block: number\n"),
        ("added) -- added on one way", "added: string|number|boolean\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn casts_above_the_first_statement_of_a_loop_change_its_variables() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@type table
local list = {}
for _, item in ipairs(list) do
    ---@cast item string
    print(item) -- generic
end
for i = 1, 2 do
    ---@cast i string
    print(i) -- numeric
end
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [("item) -- generic", "item: string\n"), ("i) -- numeric", "i: string\n")] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn casts_that_add_or_remove_types_keep_the_guards_before_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return string[]|boolean|nil
local function get() end

local items = get()
if items then
    ---@cast items +number[]
    print(items) -- added
end
if type(items) ~= 'boolean' then
    ---@cast items -nil
    print(items) -- removed
end
---@cast items +integer
print(items) -- outside guards

local skipped = get()
if not skipped then goto skip end
---@cast skipped +number
print(skipped) -- before the label
::skip::
print(skipped) -- after the label
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("items) -- added", "items: string[]|true|number[]\n"),
        // The `number[]` that the branch before adds is one of the values after it.
        ("items) -- removed", "items: string[]|number[]\n"),
        ("items) -- outside guards", "items: string[]|number[]|boolean|integer\n"),
        ("skipped) -- before the label", "skipped: string[]|true|number\n"),
        // The `goto` reaches the label without the guard or the cast.
        ("skipped) -- after the label", "skipped: string[]|boolean|number|nil\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn comparisons_read_the_types_casts_give() {
    let mut client = Client::start(fixture_root());
    let text = "\
local function untyped(value) return value end

local added = untyped(1)
---@cast added +string
if added == 5 then end
local reassigned = 1
reassigned = untyped(reassigned)
---@cast reassigned +string
if reassigned == 5 then end
local typed = 1
typed = untyped(typed)
---@cast typed string
if typed == 5 then end
local branched = untyped(1)
if branched == 1 then
    branched = 5
    ---@cast branched integer
elseif branched == 'a' then
    print(branched)
end
";
    client.open_with(CLIENT, text);
    let (line, _) = pos(text, "if typed == 5", 0);
    assert_eq!(
        findings(&mut client, CLIENT, &["impossible-comparison"]),
        [("impossible-comparison".to_string(), line as u64, "Comparing `string` with `5` is always false".to_string())],
        "a `+T` cast leaves a type that is not known as it is, and a cast on the last line of a branch \
         does not reach the next one"
    );
}

#[test]
fn many_casts_and_writes_of_one_local_stay_fast() {
    let mut client = Client::start(fixture_root());
    // Each cast holds until the next write, which is no reason to compare every cast with every
    // write in every statement.
    let mut text = "---@return string|integer|nil\nlocal function get() end\nlocal x = get()\n".to_string();
    text.push_str(&"---@cast x string\nprint(x)\nx = get()\n".repeat(2000));
    text.push_str("---@cast x integer\nprint(x) -- last\n");
    client.open_with(CLIENT, &text);
    let (line, character) = pos(&text, "x) -- last", 0);
    let started = Instant::now();
    let hover = client.hover_text(CLIENT, line, character);
    let elapsed = started.elapsed();
    assert!(hover.contains("x: integer\n"), "{hover}");
    assert!(elapsed < Duration::from_secs(5), "hover took {elapsed:?}");
}

#[test]
fn locals_assigned_many_times_in_loops_stay_fast() {
    let mut client = Client::start(fixture_root());
    // Each assignment reads what the others give, and each loop is walked again until what its
    // locals hold settles, which loops inside it must not multiply.
    let mut text = "local j, k = 1, 1\nwhile math.random() > 0.5 do\n".to_string();
    text.push_str(&"    if math.random() > 0.5 then j = j + 3 end\n".repeat(100));
    text.push_str("end\n");
    text.push_str(&"for _ = 1, 2 do\n    k = k + 1\n".repeat(12));
    text.push_str(&"end\n".repeat(12));
    text.push_str("print(j, k) -- last\n");
    client.open_with(CLIENT, &text);
    let started = Instant::now();
    findings(&mut client, CLIENT, &["param-type-mismatch"]);
    for (needle, expected) in [("j, k) -- last", "j: integer\n"), ("k) -- last", "k: integer\n")] {
        let (line, character) = pos(&text, needle, 0);
        let hover = client.hover_text(CLIENT, line, character);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
}

#[test]
fn returns_have_to_match_one_of_the_sets_of_values() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param flag integer
---@return false | (string, string)
local function names(flag)
    if flag == 1 then
        return 'Joe'
    elseif flag == 2 then
        return 1, 'Doe'
    elseif flag == 3 then
        return false
    end
    return 'Joe', 'Doe'
end

---@return nil | (string, integer)
local function entry(flag)
    if flag then
        return 'a', 1
    end
end
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |code: &str, needle: &str, message: &str| (code.to_string(), line(needle), message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["return-type-mismatch", "missing-return"]),
        [
            finding("missing-return", "return 'Joe'\n", "`@return` requires 2 values, but this returns 1 value"),
            finding(
                "return-type-mismatch",
                "return 1, 'Doe'",
                "Cannot return `integer` as return value #1 of type `string`"
            ),
        ],
        "`return false` and both names are sets the function lists, and falling off the end returns nil"
    );
}

#[test]
fn registrations_list_the_calls_that_trigger_them() {
    let mut client = Client::start(fixture_root());
    client.open_with(
        "myresource/shared/config.lua",
        "\
---@callback register
---@param name string
---@param handler fun(source: integer, ...): ...
function RegisterServerCallback(name, handler) end

---@callback await
---@param name string
---@param ... any
function AwaitServerCallback(name, ...) end
",
    );
    let server = "\
RegisterNetEvent('probe:buy', function(item) end)
AddEventHandler('probe:buy', function(item) end)
lib.callback.register('probe:price', function(source, item) return 1 end)
RegisterServerCallback('probe:stock', function(source) return 1 end)
TriggerEvent('probe:buy', 'bread')
";
    let text = "\
TriggerServerEvent('probe:buy', 'water')
TriggerServerEvent('probe:buy', 'bread')
lib.callback.await('probe:price', false, 'water')
AwaitServerCallback('probe:stock')
lib.callback.await('probe:buy', false)
";
    client.open_with(SERVER, server);
    client.open_with(CLIENT, text);

    // Where each location is, as (file, line).
    let places = |result: &Value| -> Vec<(String, u64)> {
        let mut places: Vec<(String, u64)> = result
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|location| {
                let uri = location["uri"].as_str().unwrap();
                let file = if uri.ends_with("server/main.lua") { "server" } else { "client" };
                (file.to_string(), location["range"]["start"]["line"].as_u64().unwrap())
            })
            .collect();
        places.sort();
        places
    };
    let at = |file: &str, line: u64| (file.to_string(), line);
    let mut references = |file: &str, source: &str, needle: &str, declarations: bool| {
        let (l, c) = pos(source, needle, needle.find('\'').unwrap() as u32 + 2);
        let mut params = client.position_params(file, l, c);
        params["context"] = json!({ "includeDeclaration": declarations });
        places(&client.request("textDocument/references", params))
    };

    // The other family using the same name, `lib.callback.await('probe:buy')`, is not listed.
    let buy = [at("client", 0), at("client", 1), at("server", 0), at("server", 1), at("server", 4)];
    assert_eq!(references(SERVER, server, "RegisterNetEvent('probe:buy'", true), buy);
    assert_eq!(references(CLIENT, text, "TriggerServerEvent('probe:buy'", true), buy);
    assert_eq!(
        references(SERVER, server, "RegisterNetEvent('probe:buy'", false),
        [at("client", 0), at("client", 1), at("server", 4)],
        "without declarations, only the triggers"
    );
    assert_eq!(references(SERVER, server, "register('probe:price'", true), [at("client", 2), at("server", 2)]);
    assert_eq!(
        references(SERVER, server, "RegisterServerCallback('probe:stock'", true),
        [at("client", 3), at("server", 3)]
    );

    // A registration is its own definition, so VS Code shows its references on Ctrl+click.
    let mut definition = |file: &str, source: &str, needle: &str| {
        let (l, c) = pos(source, needle, needle.find('\'').unwrap() as u32 + 2);
        places(&client.request("textDocument/definition", client.position_params(file, l, c)))
    };
    assert_eq!(definition(SERVER, server, "AddEventHandler('probe:buy'"), [at("server", 1)]);
    assert_eq!(definition(SERVER, server, "RegisterServerCallback('probe:stock'"), [at("server", 3)]);
    // A trigger still goes to the registrations.
    assert_eq!(definition(CLIENT, text, "TriggerServerEvent('probe:buy'"), [at("server", 0), at("server", 1)]);
}

#[test]
fn annotations_naming_undeclared_types_are_reported() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Garage
---@field spot Spot
---@field owner Player
---@field position vector3

---@class List<T>
---@field items T[]

---@generic K
---@param key K
---@param cb fun(value: Missing): Garage
---@return self
local function get(key, cb) end

---@param item T
function AddItem(item) end

---@see UndefinedFunction
---@diagnostic disable-next-line: undefined-doc-name
---@type Hidden
local hidden

---@alias Mode 'a'|Unknown
---@alias Boxed<V> { value: V, kind: BoxKind }
print(get, hidden)

---@param kind WheelKind
---@param expected type
local function check(kind, expected) end
";
    // An enum a module returns, as in `qbx_customs/client/enums/WheelType.lua`.
    client.open_with("myresource/shared/config.lua", "---@enum WheelKind\nreturn {\n    Sport = 0,\n}\n");
    client.open_with(CLIENT, text);
    client.diagnostics_for(CLIENT);
    let uri = client.uri(CLIENT).to_string();
    let found: Vec<(u64, u64, String)> = client.diagnostics[&uri]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "undefined-doc-name")
        .map(|d| {
            let start = &d["range"]["start"];
            (
                start["line"].as_u64().unwrap(),
                start["character"].as_u64().unwrap(),
                d["message"].as_str().unwrap().into(),
            )
        })
        .collect();
    let at = |name: &str| {
        let (line, column) = pos(text, name, 0);
        (line as u64, column as u64, format!("Undefined type or alias `{name}`"))
    };
    // Handle types, stub classes, generics, `self`, `@see` and suppressed lines are not reported.
    assert_eq!(found, [at("Spot"), at("Missing"), at("Unknown"), at("BoxKind")]);
}

#[test]
fn reports_missing_fields_of_class_typed_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Identity
---@field sex 'male'|'female'
---@field name string
---@field age? integer
---@field owner? Test.Identity

---@type Test.Identity
local empty = {}

---@type Test.Identity
local partial = { name = 'Ann' }

---@type Test.Identity
local complete = { sex = 'female', name = 'Ann', owner = { name = 'Bob' } }

---@param identity Test.Identity
local function register(identity) end
register({ sex = 'male' })

---@type Test.Identity
---@diagnostic disable-next-line: missing-fields
local skipped = {}

---@class Test.Declared : Test.Identity
local declared = {}
";
    client.open_with(CLIENT, text);
    let lines: Vec<u64> = client
        .diagnostics_for(CLIENT)
        .into_iter()
        .filter(|(code, _)| code == "missing-fields")
        .map(|(_, line)| line)
        .collect();
    // The constructor for `owner` on line 13 lacks `sex`; the outer table there is complete.
    assert_eq!(lines, [7, 10, 13, 17]);

    let uri = client.uri(CLIENT).to_string();
    let messages: Vec<&str> = client.diagnostics[&uri]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "missing-fields")
        .map(|d| d["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        messages,
        [
            "Missing required fields in type `Test.Identity`: `sex`, `name`",
            "Missing required fields in type `Test.Identity`: `sex`",
            "Missing required fields in type `Test.Identity`: `sex`",
            "Missing required fields in type `Test.Identity`: `name`",
        ]
    );
}

/// The `missing-fields` diagnostics of `text`, with the line of each.
fn missing_field_messages(client: &mut Client, text: &str) -> Vec<(u64, String)> {
    client.open_with(CLIENT, text);
    client.diagnostics_for(CLIENT);
    let uri = client.uri(CLIENT).to_string();
    client.diagnostics[&uri]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "missing-fields")
        .map(|d| (d["range"]["start"]["line"].as_u64().unwrap(), d["message"].as_str().unwrap().into()))
        .collect()
}

#[test]
fn tables_typed_as_a_union_need_the_fields_of_one_of_its_types() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Animal
---@field legs integer

---@class Test.Dog : Test.Animal
---@field bark boolean

---@class Test.Cat : Test.Animal
---@field meow boolean

---@class Test.Owner
---@field pet Test.Dog|Test.Cat

---@alias Test.Pet Test.Dog|Test.Cat

---@type Test.Dog|Test.Animal
local empty = {}

---@type Test.Dog|Test.Animal
local animal = { legs = 4 }

---@type Test.Pet
local neither = { legs = 4 }

---@type Test.Dog|string
local dog = { legs = 4 }

---@type Test.Dog|table
local any = {}

---@type Test.Dog|Test.Cat[]
local list = {}

---@type Test.Owner
local owner = { pet = {} }

---@return Test.Dog|Test.Cat
local function adopt()
    return { legs = 4, meow = true }
end
print(empty, animal, neither, dog, any, list, owner, adopt)
";
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    assert_eq!(
        missing_field_messages(&mut client, text),
        [
            (
                line("local empty"),
                "Missing required fields in type `Test.Dog`: `bark`, `legs`\n\
                 Missing required fields in type `Test.Animal`: `legs`"
                    .into()
            ),
            (
                line("local neither"),
                "Missing required fields in type `Test.Dog`: `bark`\n\
                 Missing required fields in type `Test.Cat`: `meow`"
                    .into()
            ),
            // Of `Test.Dog|string`, only the class can be a table.
            (line("local dog"), "Missing required fields in type `Test.Dog`: `bark`".into()),
            // The table for `pet` has to be one of the types the field declares.
            (
                line("local owner"),
                "Missing required fields in type `Test.Dog`: `bark`, `legs`\n\
                 Missing required fields in type `Test.Cat`: `meow`, `legs`"
                    .into()
            ),
        ],
        "a table with the fields of one type passes, and `table` or an array takes any table"
    );
}

#[test]
fn tables_typed_as_a_shape_need_its_fields() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Dog
---@field bark boolean

---@class Test.Kennel
---@field size { width: integer, depth: integer, label?: string }

---@type { name: string, tag: string? }
local untagged = { name = 'Rex' }

---@type { name: string }
local unnamed = {}

---@type Test.Kennel
local kennel = { size = { width = 2 } }

---@type Test.Dog|{ name: string }
local named = { name = 'Rex' }

---@param options { pet: Test.Dog }
local function walk(options) end
walk({ pet = {} })
print(untagged, unnamed, kennel, named)
";
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    assert_eq!(
        missing_field_messages(&mut client, text),
        [
            (line("local unnamed"), "Missing required fields in type `{ name: string }`: `name`".into()),
            (
                line("local kennel"),
                "Missing required fields in type `{ width: integer, depth: integer, label?: string }`: `depth`".into()
            ),
            // A class-typed field of a shape is checked as a field of a class is.
            (line("walk({"), "Missing required fields in type `Test.Dog`: `bark`".into()),
        ]
    );
}

#[test]
fn tables_held_in_arrays_maps_and_tuples_need_the_fields_of_their_entries() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Dog
---@field bark boolean

---@class Test.Cat
---@field meow boolean

---@class Test.Owner
---@field pets Test.Dog[]

---@alias Test.Dogs Test.Dog[]

---@type Test.Dog[]
local dogs = { {}, { bark = true } }

---@type table<string, Test.Dog>
local named = { rex = {} }

---@type table<string, Test.Dog>
local positional = { {} }

---@type [Test.Dog, Test.Cat]
local pair = { {}, {}, {} }

---@type Test.Dogs?
local aliased = { {} }

---@type Test.Dog[][]
local nested = { { {} } }

---@type Test.Owner
local owner = { pets = { {} } }

---@param list Test.Dog[]
local function walk(list) end
walk({ {} })

---@return Test.Dog[]
local function adopt()
    return { {} }
end
print(dogs, named, positional, pair, aliased, nested, owner, adopt)
";
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let dog = || "Missing required fields in type `Test.Dog`: `bark`".to_string();
    assert_eq!(
        missing_field_messages(&mut client, text),
        [
            (line("local dogs"), dog()),
            (line("local named"), dog()),
            // A `[string]` index takes no array entry, and a tuple has no item past its end.
            (line("local pair"), dog()),
            (line("local pair"), "Missing required fields in type `Test.Cat`: `meow`".into()),
            (line("local aliased"), dog()),
            (line("local nested"), dog()),
            (line("local owner"), dog()),
            (line("walk({"), dog()),
            (line("return {"), dog()),
        ],
        "the tables an array, a map or a tuple holds have the type of its entries"
    );
}

#[test]
fn tables_held_in_a_union_with_arrays_need_the_fields_of_a_type_its_members_give_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Dog
---@field bark boolean

---@class Test.Cat
---@field meow boolean

---@type Test.Dog|Test.Dog[]
local either = {
    {},
}

---@type Test.Dog[]|Test.Cat[]
local lists = { {} }

---@type [Test.Dog, Test.Cat]|Test.Dog[]
local mixed = { {}, {} }

---@type { pet: Test.Dog }|Test.Dog[]
local shaped = { pet = {} }

---@type Test.Dog[]|{ name: string }
local unnamed = { {} }

---@type Test.Dog[]|table
local open = { {} }
print(either, lists, mixed, shaped, unnamed, open)
";
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let dog = || "Missing required fields in type `Test.Dog`: `bark`".to_string();
    assert_eq!(
        missing_field_messages(&mut client, text),
        [
            // The array makes the outer table pass, and types the one it holds.
            (line("    {},"), dog()),
            (
                line("local lists"),
                "Missing required fields in type `Test.Dog`: `bark`\n\
                 Missing required fields in type `Test.Cat`: `meow`"
                    .into()
            ),
            (line("local mixed"), dog()),
            (
                line("local mixed"),
                "Missing required fields in type `Test.Cat`: `meow`\n\
                 Missing required fields in type `Test.Dog`: `bark`"
                    .into()
            ),
            (line("local shaped"), dog()),
            (line("local unnamed"), dog()),
        ],
        "a held table has to be one of the types the members give its key, and `table` takes any table"
    );
}

#[test]
fn class_typed_tables_complete_the_fields_they_lack() {
    let mut client = Client::start(fixture_root());
    // `|` marks where completion is asked for.
    let marked = "\
---@class Test
---@field test string
---@field count? integer
---@field inner Test.Inner

---@class Test.Inner
---@field label string the text shown

---@type Test
local abc = {
    |
}

---@return Test
local function make()
    return { test = 'x', | }
end

---@type Test
local nested = { inner = { | } }

---@type Test
local untyped = { extra = { | } }
print(abc, make, nested, untyped)
";
    let text = marked.replace('|', "");
    client.open_with(CLIENT, &text);
    let mut cursors = Vec::new();
    for (i, _) in marked.match_indices('|') {
        let offset = i - cursors.len();
        let line = text[..offset].matches('\n').count() as u32;
        let column = (offset - text[..offset].rfind('\n').map_or(0, |n| n + 1)) as u32;
        cursors.push((line, column));
    }
    let mut fields = |(line, column): (u32, u32)| -> Vec<(String, String)> {
        let result = client.request("textDocument/completion", client.position_params(CLIENT, line, column));
        // No completion at all is `null`.
        let mut items: Vec<(String, String)> = result["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter(|item| item["kind"] == 10)
            .map(|item| (item["label"].as_str().unwrap().into(), item["detail"].as_str().unwrap_or_default().into()))
            .collect();
        items.sort();
        items
    };
    let field = |name: &str, ty: &str| (name.to_string(), ty.to_string());
    assert_eq!(fields(cursors[0]), [field("count", "integer?"), field("inner", "Test.Inner"), field("test", "string")]);
    assert_eq!(fields(cursors[1]), [field("count", "integer?"), field("inner", "Test.Inner")], "`test` is set already");
    assert_eq!(fields(cursors[2]), [field("label", "string")], "a field typed as a class");
    assert!(fields(cursors[3]).is_empty(), "`extra` is no field of Test, so its table is not typed");
}

#[test]
fn class_typed_tables_report_fields_of_the_wrong_type() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Typed
---@field test string
---@field count? integer
---@field sex 'male'|'female'
---@field position vector3
---@field cb fun()
---@field inner Test.TypedInner

---@class Test.TypedInner
---@field label string

---@type Test.Typed
local fine = {
    test = 'x',
    count = nil,
    sex = 'male',
    position = vector3(0, 0, 0),
    cb = function() end,
    inner = { label = 'a' },
}

---@type Test.Typed
local wrong = {
    test = 1,
    count = 'many',
    sex = 'other',
    position = {},
    cb = 5,
    inner = { label = true },
}
print(fine, wrong)
";
    client.open_with(CLIENT, text);
    client.diagnostics_for(CLIENT);
    let uri = client.uri(CLIENT).to_string();
    let found: Vec<(u64, String)> = client.diagnostics[&uri]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "assign-type-mismatch")
        .map(|d| (d["range"]["start"]["line"].as_u64().unwrap(), d["message"].as_str().unwrap().into()))
        .collect();
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    assert_eq!(
        found,
        [
            (line("test = 1"), "Cannot assign `integer` to field `test` of type `string`".into()),
            (line("count = 'many'"), "Cannot assign `string` to field `count` of type `integer?`".into()),
            (line("sex = 'other'"), "Cannot assign `\"other\"` to field `sex` of type `\"male\"|\"female\"`".into()),
            (line("cb = 5"), "Cannot assign `integer` to field `cb` of type `fun()`".into()),
            // A table for a class-typed field is checked as a table of that class.
            (line("label = true"), "Cannot assign `boolean` to field `label` of type `string`".into()),
        ],
        "a table for `vector3` passes, since classes also describe userdata"
    );
}

#[test]
fn field_types_use_the_declarations_their_resource_sees() {
    let mut client = Client::start(fixture_root());
    // Another resource declares a class under the enum's name, like ox_fuel's `State` next to
    // qbx_vehicles' `State` enum.
    client.open_with(
        "shop/client.lua",
        "---@class Probe.Status\n---@field fuel number\nlocal status = {}\nprint(status)\n",
    );
    client.open_with(
        "myresource/shared/config.lua",
        "---@enum Probe.Status\nProbeStatus = {\n    OUT = 0,\n    GARAGED = 1,\n}\n\n---@class Probe.Save\n---@field state Probe.Status\n",
    );
    let text = "---@type Probe.Save\nlocal save = { state = ProbeStatus.GARAGED }\n---@type Probe.Save\nlocal wrong = { state = 'garaged' }\nprint(save, wrong)\n";
    client.open_with(CLIENT, text);
    let found: Vec<u64> = client
        .diagnostics_for(CLIENT)
        .into_iter()
        .filter(|(code, _)| code == "assign-type-mismatch")
        .map(|(_, line)| line)
        .collect();
    assert_eq!(found, [3], "only the string is wrong for the enum this resource sees");
}

#[test]
fn missing_fields_leave_out_fields_of_the_other_side() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Account
---@field name string
---@field (server) license string

---@type Test.Account
local account = { name = 'Ann' }
";
    let missing = |client: &mut Client, file: &str| -> Vec<u64> {
        client.open_with(file, text);
        let found = client.diagnostics_for(file);
        found.into_iter().filter(|(code, _)| code == "missing-fields").map(|(_, line)| line).collect()
    };
    assert!(missing(&mut client, CLIENT).is_empty(), "clients never see `license`");
    assert_eq!(missing(&mut client, SERVER), [5]);
}

#[test]
fn missing_fields_of_partial_classes_leave_out_inherited_fields() {
    let mut client = Client::start(fixture_root());
    const SHARED: &str = "myresource/shared/config.lua";
    let shared = "\
---@class Test.Anim
---@field Dict string
---@field Player string

---@class (partial) Test.CutAnim : Test.Anim
---@field StartPhase number

---@class Test.Anims
---@field Cut Test.CutAnim

---@class (partial) Test.Grandchild : Test.CutAnim
---@field Extra string

---@class Test.Child : Test.CutAnim

---@class Test.Merged : Test.Anim
---@field First string

---@class (partial) Test.Merged
---@field Second string

---@class (partial) Test.Narrowed : Test.Anim
---@field Dict string

---@class (partial) Test.Empty : Test.Anim

---@class (partial) Test.Holder<T> : Test.Anim
---@field value T
";
    let text = "\
---@type Test.Anims
local nested = { Cut = { Dict = 'x', StartPhase = 1 } }

---@type Test.CutAnim
local direct = { Dict = 'x' }

---@type Test.Grandchild
local grandchild = {}

---@type Test.Child
local child = {}

---@type Test.Merged
local merged = {}

---@type Test.Narrowed
local narrowed = {}

---@type Test.Empty
local empty = {}

---@type Test.Holder<string?>
local optional = {}

---@type Test.Holder<string>
local holder = {}
";
    client.open_with(SHARED, shared);
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |at: &str, message: &str| ("missing-fields".to_string(), line(at), message.to_string());
    // Only a class that is itself partial leaves out the fields of its parents.
    assert_eq!(
        findings(&mut client, CLIENT, &["missing-fields"]),
        [
            finding("local direct", "Missing required fields in type `Test.CutAnim`: `StartPhase`"),
            finding("local grandchild", "Missing required fields in type `Test.Grandchild`: `Extra`"),
            finding("local child", "Missing required fields in type `Test.Child`: `StartPhase`, `Dict`, `Player`"),
            finding("local merged", "Missing required fields in type `Test.Merged`: `First`, `Second`"),
            finding("local narrowed", "Missing required fields in type `Test.Narrowed`: `Dict`"),
            finding("local holder", "Missing required fields in type `Test.Holder`: `value`"),
        ]
    );

    client.change(SHARED, 2, &shared.replace("(partial) Test.CutAnim", "Test.CutAnim"));
    let found = findings(&mut client, CLIENT, &["missing-fields"]);
    assert_eq!(
        found[..2],
        [
            finding("local nested", "Missing required fields in type `Test.CutAnim`: `Player`"),
            finding("local direct", "Missing required fields in type `Test.CutAnim`: `StartPhase`, `Player`"),
        ],
        "a class that is no longer partial asks for the fields of its parents again"
    );
}

#[test]
fn a_type_below_the_class_types_the_table_as_a_value_of_it() {
    let mut client = Client::start(fixture_root());
    // LuaLS binds a `---@type` that follows the `---@class` to the statement, and the class to
    // nothing. In the other order the class still declares the table.
    let text = "\
---@class Test.Secret
---@field shown integer
---@field hidden string
---@type Test.Secret
local secret = { shown = 'one' }
secret = 5

---@class Test.GlobalSecret
---@field shown integer
---@type Test.GlobalSecret
TestGlobalSecret = {}

---@class Test.Shape
---@type Test.Secret
local shaped = { shown = 1, hidden = 'x' }

---@type Test.Secret
---@class Test.Declared
---@field name string
local declared = {}
declared = 5

---@class Test.Handlers
---@field onCount fun(count: integer)
---@type Test.Handlers
local handlers = { onCount = function(count) end }
print(secret, shaped, declared, handlers)
";
    client.open_with(CLIENT, text);
    client.diagnostics_for(CLIENT);
    let uri = client.uri(CLIENT).to_string();
    let mut found: Vec<(u64, String)> = client.diagnostics[&uri]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "missing-fields" || d["code"] == "assign-type-mismatch")
        .map(|d| (d["range"]["start"]["line"].as_u64().unwrap(), d["message"].as_str().unwrap().into()))
        .collect();
    found.sort();
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    assert_eq!(
        found,
        [
            (line("local secret"), "Cannot assign `string` to field `shown` of type `integer`".into()),
            (line("local secret"), "Missing required fields in type `Test.Secret`: `hidden`".into()),
            (line("secret = 5"), "Cannot assign `integer` to `secret` of type `Test.Secret`".into()),
            (line("TestGlobalSecret = {}"), "Missing required fields in type `Test.GlobalSecret`: `shown`".into()),
        ]
    );
    let (l, c) = pos(text, "shaped =", 0);
    assert!(client.hover_text(CLIENT, l, c).contains("local shaped: Test.Secret"));
    let (l, c) = pos(text, "declared =", 0);
    assert!(client.hover_text(CLIENT, l, c).contains("local declared: Test.Declared"));
    // The functions such a table holds take the parameters its fields declare.
    let (l, c) = pos(text, "count) end", 0);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("count: integer"), "{hover}");
}

/// The `undeclared-field` findings of `file` as (line, message), in source order.
fn undeclared_fields(client: &mut Client, file: &str) -> Vec<(u64, String)> {
    client.diagnostics_for(file);
    let uri = client.uri(file).to_string();
    let mut found: Vec<(u64, String)> = client.diagnostics[&uri]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "undeclared-field")
        .map(|d| (d["range"]["start"]["line"].as_u64().unwrap(), d["message"].as_str().unwrap().into()))
        .collect();
    found.sort();
    found
}

#[test]
fn strict_classes_report_fields_they_do_not_declare() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class (strict) Test.Strict
---@field test string
---@field inner? Test.StrictInner
local Strict = {}
Strict.__index = Strict
Strict.count = 0

function Strict:greet() end

---@class (exact) Test.StrictInner
---@field label string

---@class Test.Loose
---@field test string

---@class (strict) Test.StrictChild : Test.Strict
---@field extra integer

---@class (strict) Test.StrictOpen
---@field [string] any

---@class Test.Pair<L, R>
---@field left L

---@class (strict) Test.StrictPair : Test.Pair<string, integer>

---@type Test.Strict
local abc = {
    test = '2543',
    other = true,
    greet = function() end,
    ['count'] = 1,
    inner = { label = 'a', nope = 1 },
}

---@type Test.Loose
local loose = { test = 'x', other = true }

---@type Test.StrictChild
local child = { test = 'x', extra = 1, greet = function() end, missing = 2 }

---@type Test.StrictOpen
local open = { anything = 1 }

---@type Test.StrictPair
local pair = { left = 'a', right = 1 }

abc.test = 'y'
abc.injected = true
abc['quoted'] = 1
function abc.helper() end
Strict.static = 1
function Strict:method() end
abc.static = 2

function Strict:init()
    self.test = 'z'
    self.cache = {}
end

loose.other = 1
---@diagnostic disable-next-line: undeclared-field
abc.skipped = 1
print(abc, loose, child, open, pair)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let message = |field: &str, class: &str| format!("Field `{field}` is not declared in strict class `{class}`");
    assert_eq!(
        undeclared_fields(&mut client, CLIENT),
        [
            (line("other = true"), message("other", "Test.Strict")),
            (line("nope = 1"), message("nope", "Test.StrictInner")),
            (line("missing = 2"), message("missing", "Test.StrictChild")),
            // The `integer` of `Test.Pair<string, integer>` is a type argument, not a parent.
            (line("right = 1"), message("right", "Test.StrictPair")),
            (line("abc.injected"), message("injected", "Test.Strict")),
            (line("abc['quoted']"), message("quoted", "Test.Strict")),
            (line("abc.helper"), message("helper", "Test.Strict")),
            (line("self.cache"), message("cache", "Test.Strict")),
        ],
        "fields and methods set on the class table count as declared, fields set through values of it do not"
    );
}

#[test]
fn strict_classes_check_a_table_typed_below_the_class() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class (strict) Test.StrictSecret
---@field shown integer
---@type Test.StrictSecret
local secret = { shown = 1, other = true }
secret.extra = 1
function secret:reveal() end

---@type Test.StrictSecret
local another = { shown = 2 }
another.extra = 2
print(secret, another)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let message = |field: &str| format!("Field `{field}` is not declared in strict class `Test.StrictSecret`");
    assert_eq!(
        undeclared_fields(&mut client, CLIENT),
        [
            (line("other = true"), message("other")),
            (line("secret.extra"), message("extra")),
            (line("secret:reveal"), message("reveal")),
            (line("another.extra"), message("extra")),
        ],
        "`secret` is a value of the class, so what it is given declares nothing"
    );
}

#[test]
fn strict_classes_whose_parents_name_each_other_check_their_keys() {
    let mut client = Client::start(fixture_root());
    // Seven classes that each name the other six as parents: every walk over the parents reads each
    // class once, and a parent that leads back to a class leaves no key open.
    let mut text = String::new();
    for i in 1..=7 {
        let parents: Vec<String> = (1..=7).filter(|j| *j != i).map(|j| format!("Test.Ring{j}")).collect();
        text.push_str(&format!("---@class (strict) Test.Ring{i} : {}\n---@field f{i} integer\n\n", parents.join(", ")));
    }
    text.push_str("---@type Test.Ring1\nlocal ring = { f1 = 1, f7 = 7, other = true }\nring.f2 = 2\nring.extra = 1\n");
    text.push_str("print(ring.f5, ring.missing, ring[1])\n");
    client.open_with(CLIENT, &text);
    let line = |needle: &str| pos(&text, needle, 0).0 as u64;
    let message = |field: &str| format!("Field `{field}` is not declared in strict class `Test.Ring1`");
    assert_eq!(
        undeclared_fields(&mut client, CLIENT),
        [
            (line("other = true"), message("other")),
            (line("ring.extra"), message("extra")),
            (line("print(ring"), message("[1]")),
            (line("print(ring"), message("missing")),
        ]
    );
}

#[test]
fn strict_classes_setting_makes_the_workspace_classes_strict() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-strict-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    let write = |relative: &str, text: &str| {
        let path = fixture.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("qbxlint.toml", "strict_classes = true\nignore_diagnostics = ['vendor/']\n");
    write(
        "fxmanifest.lua",
        "fx_version 'cerulean'\ngame 'gta5'\nclient_scripts { 'vendor/*.lua', 'api.lua', 'extend.lua', 'main.lua' }\n",
    );
    write(
        "vendor/types.lua",
        "---@class VendorOptions\n---@field id string\n\n---@class (strict) VendorStrict\n---@field id string\n",
    );
    write("api.lua", "---@class Api\n---@field name string\nApi = {}\n");
    // Extending the global class table from another file declares the fields.
    write("extend.lua", "Api.version = 1\nfunction Api.extend() end\n");
    let text = "\
---@class Own
---@field id string

---@class (loose) OwnLoose
---@field id string

---@type Own
local own = { id = 'a', extra = 1 }
---@type OwnLoose
local ownLoose = { id = 'a', extra = 1 }
---@type VendorOptions
local vendor = { id = 'a', extra = 1 }
---@type VendorStrict
local vendorStrict = { id = 'a', extra = 1 }
---@type Api
local api = { name = 'a', version = 2, extend = function() end, extra = 1 }
api.more = 1
print(own, ownLoose, vendor, vendorStrict, api)
";
    write("main.lua", text);
    let mut client = Client::start(fixture.0.clone());
    let lines = |client: &mut Client| -> Vec<u64> {
        undeclared_fields(client, "main.lua").into_iter().map(|(line, _)| line).collect()
    };
    assert_eq!(
        lines(&mut client),
        [7, 13, 15, 16],
        "(loose) opts out, and classes of ignored files stay loose unless marked (strict)"
    );
    client.open_with("main.lua", text);
    assert_eq!(lines(&mut client), [7, 13, 15, 16], "the same once the file is open");
}

#[test]
fn inject_field_reports_fields_set_through_values_whose_type_lacks_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Gadget
---@field name string
---@field [integer] string
local Gadget = {}
Gadget.__index = Gadget
Gadget.version = 2

function Gadget.new()
    local self = setmetatable({}, Gadget)
    self.made = 1
    return self
end

function Gadget:init()
    self.ready = true
end

Tracker = {}

function Tracker:reset()
    self.count = 0
end

---@param gadget Test.Gadget
local function tag(gadget, key)
    gadget.name = 'tagged'
    gadget.ready = false
    gadget.extra = 5
    gadget[key] = 'x'
    gadget[1] = 'first'
end

---@class (strict) Test.Sealed

---@type Test.Sealed
local sealed = {}
sealed.other = 1

local Config = { debug = false }
Config.verbose = true
local alias = Config
alias.copied = 1

local rows = { { label = 'a' }, { label = 'b' } }
for _, row in pairs(rows) do
    row.label = 'c'
    row.count = 0
end

local function build()
    local result = {}
    result.any = 1
    local options = { size = 1 }
    options.color = 'red'
    function options.reset() end
    return result, options
end

---@diagnostic disable-next-line: inject-field
alias.allowed = 1
print(tag, build)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let found = |needle: &str, message: &str| ("inject-field".to_string(), line(needle), message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["inject-field"]),
        [
            found("self.made", "Field `made` is not declared in `Test.Gadget`"),
            found("gadget.extra", "Field `extra` is not declared in `Test.Gadget`"),
            found("alias.copied", "Field `copied` is not declared in the table `alias` holds"),
            found("row.count", "Field `count` is not declared in `{ label: string }`"),
            found("options.color", "Field `color` is not declared in `{ size: integer }`"),
            found("options.reset", "Field `reset` is not declared in `{ size: integer }`"),
        ],
        "fields set through `self` in a method, also of a global declared empty, the class table, a \
         global or a table's own local are declared, and an empty table, keys in variables and strict \
         classes are left alone"
    );
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": { "inject-field": "off" } } } } }),
    );
    assert_eq!(findings(&mut client, CLIENT, &["inject-field"]), [], "the rule can be turned off");
}

#[test]
fn undefined_field_reports_fields_read_from_values_whose_type_lacks_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Point
---@field x number
local Point = {}
Point.__index = Point
Point.origin = 0

function Point:move()
    self.moved = true
end

---@class Test.Other
---@field y number

---@class Test.Open : table

---@class Test.Dict
---@field [string] number

---@class (strict) Test.Exact
---@field x number

---@param point Test.Point
---@param either Test.Point|Test.Other
---@param maybe? Test.Point
---@param open Test.Open
---@param dict Test.Dict
---@param exact Test.Exact
---@param count number
---@param name string
local function read(point, either, maybe, open, dict, exact, count, name)
    print(point.x, point.moved, point.origin, point:move())
    print(point.y, point['w'], point:jump())
    if point.z then
        print(either.y, either.k)
    end
    print(maybe and maybe.y)
    print(open.anything, dict.anything, exact.y)
    print(count.x, name:upper(), name:nope())
    ---@diagnostic disable-next-line: undefined-field
    print(point.allowed)
end

local Settings = { debug = false }
Settings.verbose = true
print(Settings.debug, Settings.verbose, Settings.missing)

Shared = { items = {} }
print(Shared.items, Shared.missing, Shared.items.anything)

local rows = { { label = 'a' }, { label = 'b' } }
for _, row in pairs(rows) do
    print(row.label, row.count)
end

local function build(first, second)
    local result = {}
    local options = { size = 1 }
    local delta = first - second
    return result.any, options.size, options.color, delta.x
end

local current = { id = 0 }
RegisterNetEvent('test:set', function(value)
    current = value
end)

local function show()
    return current.anything
end

local Lazy = setmetatable({ loaded = true }, { __index = function(_, key) return key end })
print(Lazy.loaded, Lazy.anything)

Tracker = {}

function Tracker:reset()
    return self.count, self.anything
end

function math.lerp(a, b, t)
    return a + (b - a) * t
end

print(json.nope, math.clamp(1, 0, 2), math.lerp(1, 2, 0.5), math.nope)
print(read, build, show)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let found = |needle: &str, message: &str| ("undefined-field".to_string(), line(needle), message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["undefined-field"]),
        [
            found("point.y", "Field `y` is not declared in `Test.Point`"),
            found("point.y", "Field `w` is not declared in `Test.Point`"),
            found("point.y", "Field `jump` is not declared in `Test.Point`"),
            found("point.z", "Field `z` is not declared in `Test.Point`"),
            found("either.k", "Field `k` is not declared in `Test.Point|Test.Other`"),
            found("maybe.y", "Field `y` is not declared in `Test.Point`"),
            found("count.x", "Field `x` is not declared in `number`"),
            found("count.x", "Field `nope` is not declared in `string`"),
            found("Settings.missing", "Field `missing` is not declared in the table `Settings` holds"),
            found("row.count", "Field `count` is not declared in `{ label: string }`"),
            found("options.color", "Field `color` is not declared in `{ size: integer }`"),
            found("json.nope", "'json' has no field 'nope'"),
            found("json.nope", "Field `nope` is not declared in `mathlib`"),
        ],
        "fields that a class declares, that its methods set through `self` or its table holds, or that \
         the table's own local sets are read, while open classes, strict classes, global tables, empty \
         tables, locals given values of unknown type, an `__index` function, `self` of a table that is \
         no class and an inferred `number` take any field; the linter alone reports `json`"
    );
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": { "undefined-field": "off" } } } } }),
    );
    assert_eq!(findings(&mut client, CLIENT, &["undefined-field"]), [], "the rule can be turned off");
}

#[test]
fn private_protected_and_package_members_stay_in_their_class_or_file() {
    let mut client = Client::start(fixture_root());
    const SHARED: &str = "myresource/shared/config.lua";
    let shared = "\
---@class Test.Secret
---@field private hidden integer
---@field protected guarded integer
---@field package pkg integer
---@field shown integer
Secret = {}

---@param others Test.Secret[]
function Secret:peek(others)
    CreateThread(function()
        ---@type Test.Secret
        local other = others[1]
        print(other.hidden)
    end)
    return self.hidden + self.guarded + self.pkg
end

---@param other Test.Secret
Secret.copy = function(other) return other.hidden end

---@private
function Secret:tidy() end

---@protected
function Secret:prepare() end

---@class Test.Child : Test.Secret
Child = {}

function Child:peek()
    return self.guarded, self:prepare(), self.hidden
end

---@class Test.Counter
---@field private count integer
local Counter = {
    ---@param self Test.Counter
    increment = function(self)
        self.count = self.count + 1
    end,
}

---@param counter Test.Counter
local function reset(counter)
    counter.count = 0
end
print(Counter, reset)

Classes = {}
---@class Test.Nested : Test.Secret
Classes.nested = {}

function Classes.nested:tidy() end
Classes.nested.hidden = 1
";
    let main = "\
---@type Test.Secret
local secret = Secret
print(secret.shown, secret.hidden, secret.guarded, secret.pkg)
secret.hidden = 1
secret:tidy()
secret:prepare()
print(Secret.hidden, Secret['guarded'], Child.guarded)
print(Child.hidden)
function secret:hidden() end
---@diagnostic disable-next-line: invisible
print(secret.hidden)
";
    client.open_with(SHARED, shared);
    client.open_with(CLIENT, main);
    let private =
        |field: &str, class: &str| format!("Field `{field}` is private, it can only be accessed in class `{class}`");
    let protected = |field: &str| {
        format!("Field `{field}` is protected, it can only be accessed in class `Test.Secret` and its subclasses")
    };
    let finding = |line: u64, message: String| ("invisible".to_string(), line, message);
    // Closures in a method, functions set on the class table and those its own constructor holds
    // are inside the class; a subclass reaches protected members, not private ones, but may set its
    // own on its table.
    assert_eq!(
        findings(&mut client, SHARED, &["invisible"]),
        [finding(30, private("hidden", "Test.Secret")), finding(44, private("count", "Test.Counter"))]
    );
    // The class table itself reaches its members anywhere, as LuaLS has ox_lib's `lib.array:new()`.
    assert_eq!(
        findings(&mut client, CLIENT, &["invisible"]),
        [
            finding(2, private("hidden", "Test.Secret")),
            finding(2, protected("guarded")),
            finding(2, "Field `pkg` can only be accessed in same file `shared/config.lua`".into()),
            finding(3, private("hidden", "Test.Secret")),
            finding(4, private("tidy", "Test.Secret")),
            finding(5, protected("prepare")),
            finding(7, private("hidden", "Test.Secret")),
            finding(8, private("hidden", "Test.Secret")),
        ]
    );
}

#[test]
fn completion_and_missing_fields_leave_out_members_the_code_cannot_use() {
    let mut client = Client::start(fixture_root());
    // `|` marks where completion is asked for.
    let marked = "\
---@class Secret
---@field private hidden integer
---@field shown integer
---@type Secret
local s = { shown = 1 }
print(s.hidden)

---@class Test.Account
---@field private balance number
---@field owner string
local Account = {}

function Account.new()
    ---@type Test.Account
    local account = { | }
    return account
end

---@type Test.Account
local outside = { | }
print(outside, s.|)
";
    let text = marked.replace('|', "");
    client.open_with(CLIENT, &text);
    let mut cursors = Vec::new();
    for (i, _) in marked.match_indices('|') {
        let offset = i - cursors.len();
        let line = text[..offset].matches('\n').count() as u32;
        let column = (offset - text[..offset].rfind('\n').map_or(0, |n| n + 1)) as u32;
        cursors.push((line, column));
    }
    let mut fields = |(line, column): (u32, u32)| -> Vec<String> {
        let result = client.request("textDocument/completion", client.position_params(CLIENT, line, column));
        let items = result["items"].as_array().cloned().unwrap_or_default();
        let mut labels: Vec<String> =
            items.iter().filter(|item| item["kind"] == 10).map(|item| item["label"].as_str().unwrap().into()).collect();
        labels.sort();
        labels
    };
    assert_eq!(fields(cursors[0]), ["balance", "owner"], "its own methods may set the private field");
    assert_eq!(fields(cursors[1]), ["owner"]);
    let (line, column) = cursors[2];
    assert_eq!(client.completion_labels(CLIENT, line, column), ["shown"], "the private field of a typed local");

    let found = findings(&mut client, CLIENT, &["invisible", "missing-fields"]);
    let found: Vec<(&str, u64, &str)> =
        found.iter().map(|(code, line, message)| (code.as_str(), *line, message.as_str())).collect();
    assert_eq!(
        found,
        [
            ("missing-fields", 4, "Missing required fields in type `Secret`: `hidden`"),
            ("invisible", 5, "Field `hidden` is private, it can only be accessed in class `Secret`"),
            ("missing-fields", 14, "Missing required fields in type `Test.Account`: `balance`, `owner`"),
            ("missing-fields", 19, "Missing required fields in type `Test.Account`: `balance`, `owner`"),
        ],
        "a private field is required wherever a table of the class is built, as in LuaLS"
    );
}

#[test]
fn a_type_above_a_class_leaves_its_table_the_class_table() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@type table
---@class Probe.Vault
---@field private key string
local Vault = {
    ---@param vault Probe.Vault
    read = function(vault) return vault.key end,
}
print(Vault.key)

---@class Probe.Safe
---@field private code string
---@type Probe.Safe
local safe = { code = 'x' }
print(safe.code)
";
    client.open_with(CLIENT, text);
    assert_eq!(
        findings(&mut client, CLIENT, &["invisible"]),
        [(
            "invisible".to_string(),
            pos(text, "print(safe.code)", 0).0 as u64,
            "Field `code` is private, it can only be accessed in class `Probe.Safe`".to_string()
        )],
        "a `@type` above the `@class` keeps the table and its functions those of the class; one below types a value"
    );
}

#[test]
fn members_are_restricted_by_the_declaration_of_the_class_the_file_sees() {
    let mut client = Client::start(fixture_root());
    // Another resource keeps the members of its own `Test.Vehicle` to itself.
    let other = "\
---@class Test.Vehicle
---@field private new fun(): Test.Vehicle
---@field private secret integer
Vehicle = {}
";
    client.open_with("shop/shared.lua", other);
    let marked = "\
---@class Test.Vehicle
---@field new fun(): Test.Vehicle
---@field secret integer

---@type Test.Vehicle
local vehicle = { new = function() end }
print(vehicle.new(), vehicle.secret, vehicle.|)
";
    let (line, column) = pos(marked, "|", 0);
    client.open_with(CLIENT, &marked.replace('|', ""));
    let mut labels = client.completion_labels(CLIENT, line, column);
    labels.sort();
    assert_eq!(labels, ["new", "secret"]);
    let found = findings(&mut client, CLIENT, &["invisible", "missing-fields"]);
    assert_eq!(
        found,
        [("missing-fields".to_string(), 5, "Missing required fields in type `Test.Vehicle`: `secret`".to_string())]
    );
}

#[test]
fn members_are_restricted_through_parents_that_name_each_other() {
    let mut client = Client::start(fixture_root());
    // Parents that lead back to a class are read once, also where none declares the member.
    let text = "\
---@class Test.Loop1 : Test.Loop2, Test.Loop3, Test.Loop4, Test.Loop5
---@field private hidden integer
local Loop1 = {}

---@class Test.Loop2 : Test.Loop3, Test.Loop4, Test.Loop5, Test.Loop1
---@field protected guarded integer

---@class Test.Loop3 : Test.Loop4, Test.Loop5, Test.Loop1, Test.Loop2

---@class Test.Loop4 : Test.Loop5, Test.Loop1, Test.Loop2, Test.Loop3

---@class Test.Loop5 : Test.Loop1, Test.Loop2, Test.Loop3, Test.Loop4

---@class Test.Elsewhere
---@field private unrelated integer
---@field protected kept integer

---@type Test.Loop3
local loop = {}
print(loop.hidden, loop.guarded, loop.unrelated)

---@param other Test.Elsewhere
function Loop1:peek(other)
    print(self.guarded, other.kept)
end
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |needle: &str, message: &str| ("invisible".to_string(), line(needle), message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["invisible"]),
        [
            finding("print(loop", "Field `hidden` is private, it can only be accessed in class `Test.Loop1`"),
            finding(
                "print(loop",
                "Field `guarded` is protected, it can only be accessed in class `Test.Loop2` and its subclasses"
            ),
            finding(
                "other.kept",
                "Field `kept` is protected, it can only be accessed in class `Test.Elsewhere` and its subclasses"
            ),
        ]
    );
}

#[test]
fn class_members_come_from_the_declarations_of_their_resource() {
    let mut client = Client::start(fixture_root());
    let shop = "---@class Test.Shape\n---@field other integer\n\n---@class Test.Box\n---@field depth integer\n";
    client.open_with("shop/shared.lua", shop);
    let marked = "\
---@class Test.Shape
---@field width integer
local Shape = {}

---@type Test.Shape
local shape
print(shape.other, shape.|)
";
    let (line, column) = pos(marked, "|", 0);
    let text = marked.replace('|', "");
    client.open_with(CLIENT, &text);
    assert_eq!(client.completion_labels(CLIENT, line, column), ["width"], "not the field of `shop`'s class");
    let (l, c) = pos(&text, "other", 0);
    assert!(!client.hover_text(CLIENT, l, c).contains("integer"));
    // A client script does not describe what the server scripts of its resource see, but a module
    // that it loads through `files` or `require` does, as a `---@meta` file does.
    let marked = "---@type Test.Shape\nlocal shape\nprint(shape.|)\n";
    let (line, column) = pos(marked, "|", 0);
    client.open_with(SERVER, &marked.replace('|', ""));
    let mut labels = client.completion_labels(SERVER, line, column);
    labels.sort();
    assert_eq!(labels, ["other", "width"]);
    let marked = "---@type Test.Box\nlocal box\nprint(box.|)\n";
    let (line, column) = pos(marked, "|", 0);
    client.open_with(SERVER, &marked.replace('|', ""));
    let module = "---@class Test.Box\n---@field size integer\n";
    client.open_with("myresource/modules/box.lua", module);
    assert_eq!(client.completion_labels(SERVER, line, column), ["size"]);
    client.change("myresource/modules/box.lua", 2, &format!("---@meta\n{module}"));
    assert_eq!(client.completion_labels(SERVER, line, column), ["size"]);

    // A resource that does not describe the class reads all declarations, also when it names the
    // class itself or imports one of them: a value of the class may come from another resource,
    // as through its exports.
    let marked = "---@class Test.Shape object from elsewhere\n\n---@type Test.Shape\nlocal shape\nprint(shape.|)\n";
    let (line, column) = pos(marked, "|", 0);
    client.open_with("late/server.lua", &marked.replace('|', ""));
    let mut labels = client.completion_labels("late/server.lua", line, column);
    labels.sort();
    assert_eq!(labels, ["other", "width"]);
    let mylib = client.open("[core]/mylib/init.lua");
    client.change("[core]/mylib/init.lua", 2, &format!("{mylib}\n---@class Test.Player\n---@field handle integer\n"));
    client.open_with("shop/server.lua", "---@class Test.Player\n---@field PlayerData table\n");
    let marked = "---@type Test.Player\nlocal player\nprint(player.|)\n";
    let (line, column) = pos(marked, "|", 0);
    client.open_with(SERVER, &marked.replace('|', ""));
    let mut labels = client.completion_labels(SERVER, line, column);
    labels.sort();
    assert_eq!(labels, ["PlayerData", "handle"]);
}

#[test]
fn class_values_from_another_resource_keep_its_declarations() {
    let mut client = Client::start(fixture_root());
    client.open_with("shop/shared.lua", "---@class Test.Crate\n---@field label string\n");
    client.open_with(
        "shop/server.lua",
        "---@return Test.Crate\nlocal function newCrate() end\nexports('NewCrate', newCrate)\n",
    );
    // This resource declares a class of the same name differently, which its own values have.
    let text = "\
---@class Test.Crate
---@field weight integer

local crate = exports.shop:NewCrate()
---@type Test.Crate
local own
print(crate., own.)
";
    client.open_with(SERVER, text);
    let (line, column) = pos(text, "crate.,", 6);
    assert_eq!(client.completion_labels(SERVER, line, column), ["label"], "the crate comes from shop");
    let (line, column) = pos(text, "own.)", 4);
    assert_eq!(client.completion_labels(SERVER, line, column), ["weight"]);
}

#[test]
fn private_members_complete_in_a_method_that_is_not_closed_yet() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Counter
---@field private count integer
local Counter = {}

---@private
function Counter:reset() end

function Counter:increment()
    local before = self.";
    client.open_with(CLIENT, text);
    let line = text.lines().count() as u32 - 1;
    let mut labels = client.completion_labels(CLIENT, line, text.lines().last().unwrap().len() as u32);
    labels.sort();
    assert_eq!(labels, ["count", "increment", "reset"], "the method runs to the end of the file");
}

#[test]
fn renames_fields_declared_with_a_side_or_named_like_a_visibility() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Box
---@field private integer
---@field (client) side integer
---@field private (client) both integer
local Box = {}

function Box:read()
    return self.private, self.side, self.both
end
";
    client.open_with(CLIENT, text);
    let cases = [
        ("self.private", "@field private integer", "@field renamed integer"),
        ("self.side", "@field (client) side", "@field (client) renamed"),
        ("self.both", "@field private (client) both", "@field private (client) renamed"),
    ];
    for (used, declared, renamed) in cases {
        let expected = text.replace(declared, renamed).replace(used, "self.renamed");
        assert_eq!(renamed_text(&mut client, CLIENT, text, used, 5), expected, "{declared}");
    }
}

#[test]
fn methods_set_on_a_class_table_at_a_nested_path_belong_to_the_class() {
    let mut client = Client::start(fixture_root());
    const SHARED: &str = "myresource/shared/config.lua";
    let shared = "\
Shared = {}

---@class Test.Nested
Shared.Nested = {}

---@private
function Shared.Nested:tidy() end

function Shared.Nested:size() return 1 end

function Shared.Nested:clear()
    self:tidy()
end
";
    client.open_with(SHARED, shared);
    client.diagnostics_for(SHARED);
    let marked = "\
---@type Test.Nested
local nested = {}
print(nested:size())
nested:tidy()
Shared.Nested:tidy()
print(nested:|)
";
    let (line, column) = pos(marked, "|", 0);
    client.open_with(CLIENT, &marked.replace('|', ""));
    let mut labels = client.completion_labels(CLIENT, line, column);
    labels.sort();
    assert_eq!(labels, ["clear", "size"], "the methods of the class, without the private one");
    assert_eq!(findings(&mut client, SHARED, &["invisible"]), []);
    assert_eq!(
        findings(&mut client, CLIENT, &["invisible"]),
        [(
            "invisible".to_string(),
            3,
            "Field `tidy` is private, it can only be accessed in class `Test.Nested`".into()
        )]
    );
}

#[test]
fn no_unknown_reports_names_without_a_type_when_turned_on() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-unknown-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    let write = |relative: &str, text: &str| {
        let path = fixture.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("qbxlint.toml", "[[overrides]]\nfiles = ['strict/**']\nrules = { 'no-unknown' = 'warning' }\n");
    write("fxmanifest.lua", "fx_version 'cerulean'\ngame 'gta5'\nclient_scripts { 'strict/main.lua', 'loose.lua' }\n");
    let text = "\
function test(foo)
end

---@param name string
---@param data table
---@param scores table<string, number>
---@param _unused any
local function typed(name, data, scores, _unused, self)
    for key, value in pairs(data) do
        print(key, value)
    end
    for player, score in pairs(scores) do
        print(player, score)
    end
    for index = 1, 3 do
        print(index)
    end
    local label = name .. '!'
    local pending
    ---@type integer
    local count
    local result = Undefined()
    ---@diagnostic disable-next-line: no-unknown
    local quiet = Undefined()
    print(label, pending, count, result, quiet, self)
end

CreateThread(function()
    typed('a', {}, {})
end)

RegisterNetEvent('demo:event', function(payload)
    print(payload)
end)

local handlers = {}

---@param requestId number
handlers['demo:request'] = RegisterNetEvent('demo:request', function(requestId, ...)
    print(requestId, ...)
end)

---@class TArgs
---@field action fun(scrollIndex?: number)

local documented = {
    ---@param a1 number
    action = function(a1) end,
}

---@type TArgs
local declared = {
    action = function(b1) end,
}

print(json.encode(documented, { exception = function(_, value) end }))

local proxy = setmetatable(declared, { __index = function(_, key) end })

local untyped = { run = function(arg) end }

RegisterNUICallback('demo', function(data, cb)
    cb(data)
end)
";
    write("strict/main.lua", text);
    write("loose.lua", "function other(bar) end\n");
    let mut client = Client::start(fixture.0.clone());
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |needle: &str, message: &str| ("no-unknown".to_string(), line(needle), message.to_string());
    let expected = [
        finding("function test(foo)", "Parameter `foo` has no type; add `---@param foo <type>`"),
        finding("for key, value", "Loop variable `key` has no type; give the value the loop goes through one"),
        finding("for key, value", "Loop variable `value` has no type; give the value the loop goes through one"),
        finding("local pending", "The type of `pending` is unknown; add `---@type <type>`"),
        finding("local result", "The type of `result` is unknown; add `---@type <type>`"),
        finding("function(payload)", "Parameter `payload` has no type; add `---@param payload <type>`"),
        finding("function(arg)", "Parameter `arg` has no type; add `---@param arg <type>`"),
        finding("function(data, cb)", "Parameter `data` has no type; add `---@param data <type>`"),
    ];
    assert_eq!(findings(&mut client, "strict/main.lua", &["no-unknown"]), expected);
    client.open_with("strict/main.lua", text);
    assert_eq!(findings(&mut client, "strict/main.lua", &["no-unknown"]), expected, "the same once it is open");
    assert!(findings(&mut client, "loose.lua", &["no-unknown"]).is_empty(), "the rule is off unless turned on");
}

#[test]
fn comparisons_of_types_that_share_no_value_are_reported() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Probe.DoorState 'open'|'closed'

---@class Probe.Door
---@field state Probe.DoorState
---@field count integer

---@return 'active'|'busy'|'ready'
local function GetState()
    return 'active'
end

---@return boolean
local function IsReady()
    return true
end

local function guess()
    return 'a'
end

Settings = {}
Settings.Webhook = ''

---@param door Probe.Door
---@param name string
---@param id integer
---@param extra string?
local function check(door, name, id, extra, untyped)
    if GetState() == 'invalid_state' then return end
    if GetState() ~= 'invalid_state' then return end
    if GetState() == 5 then return end
    local state = GetState()
    if state == 'invalid_state' then return end
    if door.state == 'ajar' then return end
    if door.count == '3' then return end
    if name == id then return end
    if extra == 5 then return end
    if not name == 'x' then return end
    if type(door) == 'tabel' then return end
    if GetEntityModel(id) == 'adder' then return end
    local ready = IsReady()
    if ready then
        if ready == false then return end
    end

    if GetState() == 'busy' or door.state == 'open' or type(door) == 'vector3' then return end
    if name == nil or extra == nil or nil ~= id then return end
    if untyped == 5 or (untyped or 'a') == 5 then return end
    local mode = 'dev'
    if mode == 'prod' then return end
    local current = GetState()
    current = 'other'
    if current == 'invalid_state' then return end
    if guess() == 5 or Settings.Webhook == false then return end
    local isCamActive = IsCamActive
    if IsPedInAnyVehicle(id, false) == 1 or isCamActive(id) == 1 then return end
    ---@diagnostic disable-next-line: impossible-comparison
    if GetState() == 'suppressed' then return end
end
check()
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding =
        |needle: &str, message: &str| ("impossible-comparison".to_string(), line(needle), message.to_string());
    let state = "`\"active\"|\"busy\"|\"ready\"`";
    assert_eq!(
        findings(&mut client, CLIENT, &["impossible-comparison"]),
        [
            finding("GetState() == 'inv", &format!("Comparing {state} with `\"invalid_state\"` is always false")),
            finding("GetState() ~= 'inv", &format!("Comparing {state} with `\"invalid_state\"` is always true")),
            finding("GetState() == 5", &format!("Comparing {state} with `5` is always false")),
            // A local holds what the call that declares it returned.
            finding("state == 'inv", &format!("Comparing {state} with `\"invalid_state\"` is always false")),
            finding("door.state == 'ajar'", "Comparing `Probe.DoorState` with `\"ajar\"` is always false"),
            finding("door.count == '3'", "Comparing `integer` with `\"3\"` is always false"),
            finding("name == id", "Comparing `string` with `integer` is always false"),
            finding("extra == 5", "Comparing `string?` with `5` is always false"),
            finding("not name == 'x'", "Comparing `boolean` with `\"x\"` is always false"),
            finding("'tabel'", "Comparing `lua_type` with `\"tabel\"` is always false"),
            finding("'adder'", "Comparing `Hash` with `\"adder\"` is always false"),
            // The guard around it leaves `ready` only `true`.
            finding("ready == false", "Comparing `true` with `false` is always false"),
        ],
        "values the types share, `nil`, and types that are only inferred from values are left alone"
    );
}

#[test]
fn reads_of_locals_that_may_be_nil_are_reported() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Probe.Holder
---@field name string
---@field job string?

---@return Probe.Holder?
local function GetHolder() end

---@return number? vehicle
---@return vector3? coords
local function GetClosest() end

---@return nil | (number, vector3)
local function GetNearest() end

---@return string?
local function GetName() end

---@param id integer
---@param label? string
---@param done? fun()
---@param list string[]?
---@param state false|string
local function handle(id, label, done, list, state)
    local holder = GetHolder()
    print(holder.name)
    print(holder.job)
    print(label:upper())
    done()
    print(#list)
    print(state:upper())
    local amount = tonumber('5')
    print(amount + 1)
    local key = GetHolder()
    local seen = {}
    seen[key] = true
    local branch = GetHolder()
    if id > 1 then
        print(branch.name)
    end
    local function afterBranch() return branch.name end
    local always = GetHolder()
    print(always.name)
    local function afterRead() return always.job end
    local twice = GetHolder()
    if id > 2 then print(twice.name) else print(twice.job) end
    local count = tonumber('5')
    if count > 0 then print(count * 2) end
    local limit = tonumber('5')
    for i = 1, limit do print(i) end
    local equal = tonumber('5')
    print(equal == 0)
    local suffix = GetName()
    print('id:' .. suffix --[[@as string]])
    local looped = GetName()
    repeat
        if id > 3 then break end
    until looped:upper()
    local function afterLoop() return looped:lower() end
    local ended = GetName()
    repeat until ended:upper()
    local function afterEnd() return ended:lower() end

    local guarded = GetHolder()
    if not guarded then return end
    print(guarded.name)
    local inside = GetHolder()
    if inside then print(inside.name) end
    local either = GetHolder()
    print(either and either.name)
    local asserted = GetHolder()
    assert(asserted)
    print(asserted.name)
    local safe = GetHolder()
    print(safe?.name)
    if safe?.job then print(safe.name) end
    local cast = GetHolder()
    ---@cast cast -?
    print(cast.name)
    local forced = GetHolder() --[[@as Probe.Holder]]
    print(forced.name)
    local filled = GetHolder()
    filled = filled or { name = 'none' }
    print(filled.name)
    local vehicle, coords = GetClosest()
    if not vehicle then return end
    print(coords.x)
    local nearest, spot = GetNearest()
    if not nearest then return end
    print(spot.x)
    print(guarded.job:upper())
    print(GetHolder().name)
    local quiet = GetHolder()
    ---@diagnostic disable-next-line: need-check-nil
    print(quiet.name)
end
handle(1)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |needle: &str, message: &str| ("need-check-nil".to_string(), line(needle), message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["need-check-nil"]),
        [
            finding("holder.name", "`holder` may be nil: its type here is `Probe.Holder?`"),
            // Every read is reported, also after one that would raise the error first.
            finding("holder.job", "`holder` may be nil: its type here is `Probe.Holder?`"),
            finding("label:upper", "`label` may be nil: its type here is `string?`"),
            finding("done()", "`done` may be nil: its type here is `(fun())?`"),
            finding("#list", "`list` may be nil: its type here is `string[]?`"),
            finding("state:upper", "`state` may be false: its type here is `false|string`"),
            finding("amount + 1", "`amount` may be nil: its type here is `number?`"),
            finding("seen[key]", "`key` may be nil: its type here is `Probe.Holder?`"),
            finding("print(branch.name)", "`branch` may be nil: its type here is `Probe.Holder?`"),
            finding("return branch.name", "`branch` may be nil: its type here is `Probe.Holder?`"),
            finding("print(always.name)", "`always` may be nil: its type here is `Probe.Holder?`"),
            finding("return always.job", "`always` may be nil: its type here is `Probe.Holder?`"),
            finding("print(twice.name)", "`twice` may be nil: its type here is `Probe.Holder?`"),
            finding("print(twice.name)", "`twice` may be nil: its type here is `Probe.Holder?`"),
            finding("count > 0", "`count` may be nil: its type here is `number?`"),
            finding("count > 0", "`count` may be nil: its type here is `number?`"),
            finding("1, limit", "`limit` may be nil: its type here is `number?`"),
            // The cast is of the whole concatenation.
            finding("'id:' .. suffix", "`suffix` may be nil: its type here is `string?`"),
            finding("until looped", "`looped` may be nil: its type here is `string?`"),
            // The `break` leaves the loop without running the condition.
            finding("return looped", "`looped` may be nil: its type here is `string?`"),
            finding("until ended", "`ended` may be nil: its type here is `string?`"),
            // A guard on `vehicle` tells nothing about `coords`, unless the function declares the sets
            // of values it returns, as `GetNearest` does.
            finding("coords.x", "`coords` may be nil: its type here is `vector3?`"),
        ],
        "every read that no guard or cast covers is reported, while fields and the values of calls are left \
         alone"
    );
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": { "need-check-nil": "off" } } } } }),
    );
    assert!(findings(&mut client, CLIENT, &["need-check-nil"]).is_empty(), "the rule can be turned off");
}

#[test]
fn reads_of_locals_that_are_assigned_again_are_checked_for_nil() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Probe.Box
---@field name string

---@return Probe.Box?
local function find() end
---@return Probe.Box
local function make() end
---@param code string
---@return function?, string?
local function compile(code) end
---@return Probe.Box?, Probe.Box?
local function locate() end
local function untyped() return GetSomething() end

---@param label string?
local function show(label)
    if not label then
        label = untyped()
    end
    print(label:upper())
end

---@param id integer
---@param input string
local function run(id, input)
    local before = find()
    print(before.name)
    before = make()
    local swapped = make()
    if id > 1 then
        swapped = find()
    end
    print(swapped.name)
    local fixed = find()
    if not fixed then
        fixed = make()
    end
    print(fixed.name)
    local again = find()
    if not again then
        return
    end
    again = find()
    print(again.name)
    local looped = make()
    while id > 2 do
        print(looped.name)
        looped = nil
    end
    local fallback = find()
    fallback = fallback or make()
    print(fallback.name)
    local cast = make()
    cast = find() --[[@as Probe.Box]]
    print(cast.name)
    local later
    if id > 3 then
        later = make()
    end
    print(later.name)
    local fn, err = compile('return ' .. input)
    if err then
        fn, err = compile(input)
    end
    if err then
        return
    end
    fn()
    local vehicle, spot = locate()
    if not vehicle then
        return
    end
    if id > 4 then
        spot = make()
    end
    print(spot.name)
    local other, place = locate()
    if not other then
        return
    end
    if id > 5 then
        place = find()
    end
    print(place.name)
end
run(1, '')
show()
";
    client.open_with(CLIENT, text);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": { "need-check-nil": "warning" } } } } }),
    );
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |needle: &str, message: &str| ("need-check-nil".to_string(), line(needle), message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["need-check-nil"]),
        [
            finding("before.name", "`before` may be nil: its type here is `Probe.Box?`"),
            // One way gives it a value that may be missing.
            finding("swapped.name", "`swapped` may be nil: its type here is `Probe.Box?`"),
            // The guard told about the value it held before.
            finding("again.name", "`again` may be nil: its type here is `Probe.Box?`"),
            // The loop gives it `nil` before it starts again.
            finding("looped.name", "`looped` may be nil: its type here is `Probe.Box?`"),
            // A guard on another value of the same call tells nothing about it.
            finding("fn()", "`fn` may be nil: its type here is `function?`"),
            finding("spot.name", "`spot` may be nil: its type here is `Probe.Box?`"),
            finding("place.name", "`place` may be nil: its type here is `Probe.Box?`"),
        ],
        "the values that reach a read of a local that is assigned again are checked; one that a guard, \
         `or` or a cast covers, one of no known type, and the missing value of `local later`, are left alone"
    );
}

#[test]
fn nil_checks_count_lookups_of_source_like_other_calls() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Probe.Member
---@field name string

---@param id integer
---@return Probe.Member?
local function GetMember(id) end

---@param id integer
---@param slot integer
---@return Probe.Member?
local function GetSlot(id, slot) end

local shared = GetMember(1)

local function first()
    print(shared.name)
end

RegisterNetEvent('probe:event', function(target)
    local src = source
    local member = GetMember(source)
    print(member.name)
    local same = GetMember(src)
    print(same.name)
    local other = GetMember(target)
    print(other.name)
    local slot = GetSlot(source, 1)
    print(slot.name)
end)
first()
";
    client.open_with(CLIENT, text);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": { "need-check-nil": "warning" } } } } }),
    );
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |needle: &str, message: &str| ("need-check-nil".to_string(), line(needle), message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["need-check-nil"]),
        [
            // A local declared outside any function is checked like any other.
            finding("print(shared.name)", "`shared` may be nil: its type here is `Probe.Member?`"),
            finding("print(member.name)", "`member` may be nil: its type here is `Probe.Member?`"),
            finding("print(same.name)", "`same` may be nil: its type here is `Probe.Member?`"),
            finding("print(other.name)", "`other` may be nil: its type here is `Probe.Member?`"),
            finding("print(slot.name)", "`slot` may be nil: its type here is `Probe.Member?`"),
        ],
        "what a call gives for `source` counts as its function declares it"
    );
}

#[test]
fn nil_checks_leave_alone_missing_values_the_guards_rule_out() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param round? boolean
---@param mode? 'open'|'close'
---@param label? string
---@param count number
local function scale(round, mode, label, count)
    local i = 0
    print(round and (round == true or i < round))
    if mode and mode ~= 'open' and mode ~= 'close' then print(mode:upper()) end
    if label ~= 'x' and label ~= 'y' then print(label:upper()) end
    if not count then print(count.value) end
end
scale()
";
    client.open_with(CLIENT, text);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": { "need-check-nil": "warning" } } } } }),
    );
    let finding = |needle: &str, message: &str| {
        ("need-check-nil".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["need-check-nil"]),
        [finding("label:upper", "`label` may be nil: its type here is `string?`")],
        "comparisons with literals that rule out every value of the type keep it whole, but not the `nil` \
         they rule out, and a check for a missing value that rules out every value leaves only `nil`, as \
         lua-language-server reads it, which is no value that may be missing"
    );
}

#[test]
fn nil_checks_report_values_passed_for_parameters_that_do_not_take_nil() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Probe.Target
---@field name string

---@return Probe.Target?
local function GetTarget() end

---@return number?
local function GetHandle() end

---@param target Probe.Target
local function greet(target) end
---@param target? Probe.Target
local function maybeGreet(target) end
---@param label Probe.Target|nil
local function show(label) end
---@generic T
---@param value T
local function keep(value) end
---@param n number
local function needsNumber(n) end
---@param target Probe.Target
---@overload fun(target: nil)
local function either(target) end
local Box = {}
---@param target Probe.Target
function Box:put(target) end

local function run()
    local missing = GetTarget()
    greet(missing)
    local optional = GetTarget()
    maybeGreet(optional)
    local nilable = GetTarget()
    show(nilable)
    local generic = GetTarget()
    keep(generic)
    local wrong = GetTarget()
    needsNumber(wrong)
    local overloaded = GetTarget()
    either(overloaded)
    local stored = GetTarget()
    Box:put(stored)
    local guarded = GetTarget()
    if guarded then greet(guarded) end
    local read = GetTarget()
    print(read.name)
    greet(read)
    local handle = GetHandle()
    SetEntityHeading(handle, 1.0)
    local printed = GetTarget()
    print(printed)
    local suppressed = GetTarget()
    ---@diagnostic disable-next-line: param-type-mismatch
    greet(suppressed)
end
run()
";
    client.open_with(CLIENT, text);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": { "need-check-nil": "warning" } } } } }),
    );
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |needle: &str, message: &str| ("need-check-nil".to_string(), line(needle), message.to_string());
    let mismatch = |needle: &str, given: &str, param: &str, ty: &str| {
        let message = format!("Cannot assign `{given}` to parameter `{param}` of type `{ty}`");
        ("param-type-mismatch".to_string(), line(needle), message)
    };
    let target = |needle: &str| mismatch(needle, "Probe.Target?", "target", "Probe.Target");
    assert_eq!(
        findings(&mut client, CLIENT, &["need-check-nil", "param-type-mismatch"]),
        [
            target("greet(missing)"),
            mismatch("needsNumber(wrong)", "Probe.Target?", "n", "number"),
            target("either(overloaded)"),
            target("Box:put(stored)"),
            finding("print(read.name)", "`read` may be nil: its type here is `Probe.Target?`"),
            target("greet(read)"),
            mismatch("SetEntityHeading(handle", "number?", "entity", "Entity"),
        ],
        "`param-type-mismatch` reports a value that may be nil passed for a parameter that does not take \
         it, as LuaLS does, also to a native, and also where the overload that takes `nil` takes no \
         `Probe.Target`; parameters that are optional or take nil, any value or a generic, guards and \
         suppressed lines are left alone"
    );
}

#[test]
fn rule_levels_of_the_config_file_win_over_the_client_settings() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-rule-levels-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    let write = |relative: &str, text: &str| {
        let path = fixture.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        "qbxlint.toml",
        "[rules]\n'lowercase-global' = 'off'\n[[overrides]]\nfiles = ['kept/**']\nrules = { 'unused-local' = 'warning' }\n",
    );
    write("fxmanifest.lua", "fx_version 'cerulean'\ngame 'gta5'\nclient_scripts { 'kept/main.lua', 'other.lua' }\n");
    let text = "function helper(value)\n    local unused = 1\nend\n";
    write("kept/main.lua", text);
    write("other.lua", text);
    let mut client = Client::start(fixture.0.clone());
    let rules = json!({ "lowercase-global": "warning", "unused-local": "off", "no-unknown": "warning" });
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "qbxLua": { "diagnostics": { "rules": rules } } } }),
    );
    let codes = |client: &mut Client, file: &str| -> Vec<String> {
        let found = findings(client, file, &["lowercase-global", "unused-local", "no-unknown"]);
        found.into_iter().map(|(code, ..)| code).collect()
    };
    assert_eq!(codes(&mut client, "kept/main.lua"), ["no-unknown", "unused-local"], "an override keeps its level");
    assert_eq!(codes(&mut client, "other.lua"), ["no-unknown"], "the client sets the rules the file leaves alone");
    client.open_with("kept/main.lua", text);
    assert_eq!(codes(&mut client, "kept/main.lua"), ["no-unknown", "unused-local"], "the same once it is open");
}

#[test]
fn open_files_keep_their_hints_when_the_client_spells_uris_its_own_way() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-uris-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    std::fs::create_dir_all(&fixture.0).unwrap();
    let text = "function test(foo)\nend\n";
    std::fs::write(fixture.0.join("fxmanifest.lua"), "fx_version 'cerulean'\ngame 'gta5'\nclient_script 'main.lua'\n")
        .unwrap();
    std::fs::write(fixture.0.join("main.lua"), text).unwrap();
    let mut client = Client::start(fixture.0.clone());
    let codes = |client: &Client, uri: &str| -> Option<Vec<String>> {
        let list = client.diagnostics.get(uri)?.as_array().unwrap();
        Some(list.iter().map(|d| d["code"].as_str().unwrap().to_string()).collect())
    };

    // The same file, as a client such as VS Code may write its URI: `%61` is `a`, the way VS Code
    // sends `c%3A` for `c:`.
    let indexed = client.uri("main.lua").to_string();
    let spelled = indexed.replace("main.lua", "m%61in.lua");
    assert_ne!(indexed, spelled);
    client.diagnostics_for("main.lua");
    assert_eq!(codes(&client, &indexed).unwrap(), ["lowercase-global"], "closed files are reported without hints");

    client.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": spelled, "languageId": "lua", "version": 1, "text": text } }),
    );
    // A save has the files nobody has open checked again.
    client.notify("textDocument/didSave", json!({ "textDocument": { "uri": spelled } }));
    client.diagnostics.clear();
    client.diagnostics_for("main.lua");
    assert_eq!(codes(&client, &spelled).unwrap(), ["lowercase-global", "unused-argument"]);
    assert_eq!(codes(&client, &indexed), None, "an open file is not reported a second time, without its hints");

    client.notify("textDocument/didClose", json!({ "textDocument": { "uri": spelled } }));
    client.diagnostics_for("main.lua");
    assert_eq!(codes(&client, &spelled).unwrap(), Vec::<String>::new(), "what the open file showed is cleared");
    assert_eq!(codes(&client, &indexed).unwrap(), ["lowercase-global"]);
}

/// The findings of `file` for the given codes as (code, line, message), sorted by line.
fn findings(client: &mut Client, file: &str, codes: &[&str]) -> Vec<(String, u64, String)> {
    client.diagnostics_for(file);
    let uri = client.uri(file).to_string();
    let mut found: Vec<(String, u64, String)> = client.diagnostics[&uri]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| codes.contains(&d["code"].as_str().unwrap()))
        .map(|d| {
            let code = d["code"].as_str().unwrap().to_string();
            (code, d["range"]["start"]["line"].as_u64().unwrap(), d["message"].as_str().unwrap().to_string())
        })
        .collect();
    found.sort_by_key(|(_, line, _)| *line);
    found
}

#[test]
fn index_fields_type_their_keys_and_strict_classes_check_reads() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class (strict) Test.Indexed
---@field test string
---@field [string] number

---@type Test.Indexed
local abc = {
    test = '2543',
    other = true,
    fine = 1,
    [1] = 2,
}

print(abc[1], abc.anything)
abc.more = 'x'
abc.test = 5
abc[2] = 1

---@class (strict) Test.Closed
---@field name string
---@field note? string
---@field label string|nil
local Closed = {}
function Closed:greet() end

---@type Test.Closed
local closed = { name = 'a', 'positional' }
print(closed.name, closed.nope)
closed:greet()
closed:missing()
closed.note = nil
closed.label = nil
closed.name = nil
local key = 'name'
print(closed[key])

---@class Test.LooseMap
---@field [string] integer

---@type Test.LooseMap
local map = { a = 'x' }
print(map[true], map.b)
abc.test = nil

---@class (strict) Test.Tuple
---@field [1] number
---@field [2] string

---@type Test.Tuple
local tuple = { 1, 'a' }
print(tuple[2]:upper())

---@type Test.Tuple
local swapped = {
    'swapped',
    12,
    false,
}
print(tuple[3])
tuple[1] = 'x'
for i = 1, 2 do print(tuple[i]) end
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |code: &str, needle: &str, message: &str| (code.to_string(), line(needle), message.to_string());
    let mismatch = "assign-type-mismatch";
    let undeclared = "undeclared-field";
    assert_eq!(
        findings(&mut client, CLIENT, &[mismatch, undeclared]),
        [
            finding(mismatch, "other = true", "Cannot assign `boolean` to field `other` of type `number`"),
            finding(undeclared, "[1] = 2", "Field `[1]` is not declared in strict class `Test.Indexed`"),
            finding(undeclared, "print(abc[1]", "Field `[1]` is not declared in strict class `Test.Indexed`"),
            finding(mismatch, "abc.more", "Cannot assign `string` to field `more` of type `number`"),
            finding(mismatch, "abc.test = 5", "Cannot assign `integer` to field `test` of type `string`"),
            finding(undeclared, "abc[2]", "Field `[2]` is not declared in strict class `Test.Indexed`"),
            finding(undeclared, "'positional'", "Field `[1]` is not declared in strict class `Test.Closed`"),
            finding(undeclared, "closed.nope", "Field `nope` is not declared in strict class `Test.Closed`"),
            finding(undeclared, "closed:missing", "Field `missing` is not declared in strict class `Test.Closed`"),
            finding(mismatch, "closed.name = nil", "Cannot assign `nil` to field `name` of type `string`"),
            finding(mismatch, "a = 'x'", "Cannot assign `string` to field `a` of type `integer`"),
            finding(mismatch, "abc.test = nil", "Cannot assign `nil` to field `test` of type `string`"),
            finding(mismatch, "'swapped'", "Cannot assign `string` to field `[1]` of type `number`"),
            finding(mismatch, "12,", "Cannot assign `integer` to field `[2]` of type `string`"),
            finding(undeclared, "false,", "Field `[3]` is not declared in strict class `Test.Tuple`"),
            finding(undeclared, "tuple[3]", "Field `[3]` is not declared in strict class `Test.Tuple`"),
            finding(mismatch, "tuple[1] = 'x'", "Cannot assign `string` to field `[1]` of type `number`"),
        ],
        "a string variable may name a field, loose classes take any key, `= nil` only clears fields whose \
         type allows `nil`, tuple fields like `[1]` check their own key and value, and an `integer` \
         variable may be any of them"
    );
}

#[test]
fn strict_classes_take_only_the_keys_a_literal_index_lists() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Test.Keys 'a'|'b'

---@class (exact) Test.Keyed
---@field [Test.Keys] integer

---@class (exact) Test.Listed
---@field ['x'|'y'] string

---@type Test.Keyed
local keyed = { a = 1, c = 2 }
keyed.b = 'no'
keyed.d = 3
print(keyed.a, keyed.e)

---@type Test.Listed
local listed = { x = 'a', z = 'b' }
local name = 'y'
print(listed[name])
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |code: &str, needle: &str, message: &str| (code.to_string(), line(needle), message.to_string());
    let mismatch = "assign-type-mismatch";
    let undeclared = "undeclared-field";
    assert_eq!(
        findings(&mut client, CLIENT, &[mismatch, undeclared]),
        [
            finding(undeclared, "c = 2", "Field `c` is not declared in strict class `Test.Keyed`"),
            finding(mismatch, "keyed.b", "Cannot assign `string` to field `b` of type `integer`"),
            finding(undeclared, "keyed.d", "Field `d` is not declared in strict class `Test.Keyed`"),
            finding(undeclared, "keyed.e", "Field `e` is not declared in strict class `Test.Keyed`"),
            finding(undeclared, "z = 'b'", "Field `z` is not declared in strict class `Test.Listed`"),
        ],
        "an index of string literals, written out or through an alias, takes only those names"
    );
}

#[test]
fn tables_typed_as_table_types_check_their_fields() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Test.Box<T> { value: T }
---@class Test.Named
---@field name string

---@type { value: string, count?: integer }
local shape = { value = 1, count = 'x', extra = true }
---@type Test.Box<string>
local box = { value = 1 }
---@type Test.Box
local open = { value = 1 }
---@type { inner: { n: integer }, named: Test.Named }
local nested = { inner = { n = 'x' }, named = { name = 1 } }
---@type string[]
local list = { 'a', 2 }
---@type table<string, integer>
local map = { a = 'x', [1] = 'y' }
---@type { [1]: string, [2]: integer }
local pair = { 1, 'x' }
---@type { mode: 'a'|'b' }?
local mode = { mode = 'c' }
---@type { value: string }|{ value: integer }
local either = { value = true }

---@param options { id: integer }
local function use(options) end
use({ id = 'x' })

---@return { id: integer }
local function make() return { id = 'x' } end

---@type { value: string }
local later
later = { value = 1 }
local inferred = { value = 1 }
inferred = { value = 'x' }
print(shape, box, open, nested, list, map, pair, mode, either, make, later, inferred)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("assign-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    let mut found = findings(&mut client, CLIENT, &["assign-type-mismatch"]);
    found.sort();
    let mut expected = vec![
        finding("value = 1, count", "Cannot assign `integer` to field `value` of type `string`"),
        finding("count = 'x'", "Cannot assign `string` to field `count` of type `integer?`"),
        finding("local box = { value = 1 }", "Cannot assign `integer` to field `value` of type `string`"),
        finding("n = 'x'", "Cannot assign `string` to field `n` of type `integer`"),
        finding("name = 1", "Cannot assign `integer` to field `name` of type `string`"),
        finding("'a', 2", "Cannot assign `integer` to field `[2]` of type `string`"),
        finding("a = 'x'", "Cannot assign `string` to field `a` of type `integer`"),
        finding("1, 'x'", "Cannot assign `integer` to field `[1]` of type `string`"),
        finding("1, 'x'", "Cannot assign `string` to field `[2]` of type `integer`"),
        finding("mode = 'c'", "Cannot assign `\"c\"` to field `mode` of type `\"a\"|\"b\"`"),
        finding("use({ id", "Cannot assign `string` to field `id` of type `integer`"),
        finding("make() return { id", "Cannot assign `string` to field `id` of type `integer`"),
        finding("later = {", "Cannot assign `integer` to field `value` of type `string`"),
    ];
    expected.sort();
    assert_eq!(
        found, expected,
        "shapes, aliases of them, arrays and `table<K, V>` check what their tables hold, and the class tables \
         they hold; an alias without its type argument, a union of table types and a table that only replaces \
         an inferred one are not checked"
    );
}

#[test]
fn values_assigned_to_typed_variables_are_checked() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Test.State 1 | 2 | 3 | false | true

---@type Test.State
local state = 'test'
state = 'abc'
state = 2
state = 4
state = nil

---@type Test.State?
local maybe = nil
if not maybe then maybe = 1 end
maybe = {}

---@type number
local handle = nil
if handle then handle = nil end
---@type string
TestZone = nil
---@type number, string
local width, caption = nil, nil

---@type string
local unset
unset = 'later'

---@type 'a'|'b'
local letter = 'c'
letter = state

---@class Test.Holder
---@field name string
local Holder = {}
Holder = nil

---@type Test.Holder
local holder = { name = 'a' }
---@type number
holder.name = true
---@type string
holder.label = 5
---@type string
TestLabel = false
---@type string, integer
TestName, TestCount = 'a', 1
---@type integer
local counted = TestCount

---@return string, integer
local function pair() return 'a', 1 end
---@type string
local first, second = pair()
---@type string, integer?
local name, count, extra = pair()
name, count, extra = 1, 'two', 3

---@param id number
---@param label? string
local function show(id, label, other)
    id = tostring(id)
    label = label or 'none'
    label = nil
    other = 1
end

local plain = 1
plain = 'text'
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("assign-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["assign-type-mismatch"]),
        [
            finding("local state", "Cannot assign `\"test\"` to `state` of type `Test.State`"),
            finding("state = 'abc'", "Cannot assign `\"abc\"` to `state` of type `Test.State`"),
            finding("state = 4", "Cannot assign `4` to `state` of type `Test.State`"),
            finding("state = nil", "Cannot assign `nil` to `state` of type `Test.State`"),
            finding("maybe = {}", "Cannot assign `table` to `maybe` of type `Test.State?`"),
            // The statement whose `---@type` declares a name may give it `nil`, as in LuaLS.
            finding("then handle = nil", "Cannot assign `nil` to `handle` of type `number`"),
            finding("local letter", "Cannot assign `\"c\"` to `letter` of type `\"a\"|\"b\"`"),
            // A local that is assigned again has the type of what may reach it: what `state = nil`
            // gives is still a `Test.State`, as that is reported.
            finding("letter = state", "Cannot assign `Test.State` to `letter` of type `\"a\"|\"b\"`"),
            // The `@field` decides what a class field takes, and reports it once.
            finding("holder.name = true", "Cannot assign `boolean` to field `name` of type `string`"),
            finding("holder.label = 5", "Cannot assign `integer` to `holder.label` of type `string`"),
            finding("TestLabel = false", "Cannot assign `boolean` to `TestLabel` of type `string`"),
            // A list of types gives each name its own, and the names after it none, as one type
            // gives every name after the first.
            finding("name, count, extra = 1", "Cannot assign `integer` to `name` of type `string`"),
            finding("name, count, extra = 1", "Cannot assign `string` to `count` of type `integer?`"),
            finding("id = tostring(id)", "Cannot assign `string` to `id` of type `number`"),
        ],
        "values the type takes, a guarded local, a local without a value, the table a `---@class` declares, \
         optional and undocumented parameters and untyped locals pass"
    );
    let (line, column) = pos(text, "second = pair()", 0);
    let hover = client.hover_text(CLIENT, line, column);
    assert!(hover.contains("local second: integer"), "{hover}");
    let (line, column) = pos(text, "TestCount =", 0);
    let hover = client.hover_text(CLIENT, line, column);
    assert!(hover.contains("TestCount: integer"), "{hover}");
}

#[test]
fn as_casts_type_the_expression_right_before_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return integer, string
local function pair() return 1, 'a' end

---@type string
local a = 5 --[[@as string]]
---@type string, string
local b, c = pair() --[[@as string]]
---@type string
local d = 5 ---@as string
---@type string
local e = 5 -- @as string
---@type string
local f = 5
--[[@as string]]
local cast = 5 --[[@as string]]
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("assign-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["assign-type-mismatch"]),
        [
            finding("local e", "Cannot assign `integer` to `e` of type `string`"),
            finding("local f =", "Cannot assign `integer` to `f` of type `string`"),
        ],
        "a cast on the line of its value types the first value, and a plain comment or a line of its own casts nothing"
    );
    let (line, column) = pos(text, "cast =", 0);
    let hover = client.hover_text(CLIENT, line, column);
    assert!(hover.contains("local cast: string"), "{hover}");
}

#[test]
fn casts_to_types_the_declared_type_does_not_take_are_reported() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return string|integer
local function get() end

---@type integer
local count = 1
---@cast count string
---@cast count number
---@cast count integer?
---@param mode 'a'|'b'
local function use(mode)
    ---@cast mode 'c'
    ---@cast mode 'a'
    ---@cast mode string
end
local value = get()
---@cast value boolean
---@cast value +boolean, -integer
local literal = 5
---@cast literal string
local function untyped() return {} end
local decoded = untyped()
---@cast decoded string
local placeholder = nil
---@cast placeholder string?
---@cast missing string
---@diagnostic disable-next-line: cast-type-mismatch
---@cast count boolean
use(value, decoded)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("cast-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["cast-type-mismatch"]),
        [
            finding("@cast count string", "Cannot convert `integer` to `string`"),
            finding("@cast count integer?", "Cannot convert `integer` to `integer?`"),
            finding("@cast mode 'c'", "Cannot convert `\"a\"|\"b\"` to `\"c\"`"),
            finding("@cast value boolean", "Cannot convert `string|integer` to `boolean`"),
            finding("@cast literal", "Cannot convert `integer` to `string`"),
        ],
        "types inferred from what a function returns, locals declared as `nil`, `+` and `-` entries, unknown names \
         and suppressed lines are left alone"
    );
}

#[test]
fn closed_locals_need_a_value_that_can_be_closed() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Test.Name string
---@type Test.Name
local name
---@type number?
local maybe
---@class Test.Handle
local handle = {}
local function pair() return 1, nil end
local count <close> = 1
local label <close> = 'text'
local flag <close> = true
local callback <close> = function() end
local alias <close> = name
local empty <close>
local first, second <close> = 1
local nothing <close> = nil
local off <close> = false
local optional <close> = maybe
local object <close> = handle
local closable <close> = setmetatable({}, { __close = function() end })
local a, b <close> = pair()
local varargs <close> = ...
---@diagnostic disable-next-line: close-non-object
local suppressed <close> = 2
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("close-non-object".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    let cannot = |ty: &str| {
        format!(
            "Cannot close a value of type `{ty}`; a `<close>` local takes `nil`, `false` or a value with a `__close` metamethod"
        )
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["close-non-object"]),
        [
            finding("count <close>", &cannot("integer")),
            finding("label <close>", &cannot("string")),
            finding("flag <close>", &cannot("boolean")),
            finding("callback <close>", &cannot("fun()")),
            finding("alias <close>", &cannot("Test.Name")),
            finding("empty <close>", "`empty` is declared `<close>` without a value to close"),
            finding("first, second", "`second` is declared `<close>` without a value to close"),
        ],
        "`nil`, `false`, values that may be `nil`, tables, classes, values a call leaves out and `...` can be closed"
    );
}

#[test]
fn classes_that_inherit_from_themselves_are_reported_in_each_file() {
    const SHARED: &str = "myresource/shared/config.lua";
    let mut client = Client::start(fixture_root());
    let shared = "---@class Test.Loop.B : Test.Loop.C\n---@class Test.Loop.C : Test.Loop.A\nreturn {}\n";
    client.open_with(SHARED, shared);
    let text = "\
---@class Test.Loop.A : Test.Loop.B
---@class Test.Self : Test.Self
---@class Test.Pair.A : Test.Pair.B
---@class Test.Pair.B : Test.Pair.A
---@class Test.Base
---@class Test.Fine : Test.Base
---@class Test.Generic<T> : Test.Base
---@class Test.Child : Test.Generic<string>
---@class Test.Above : Test.Pair.A
";
    client.open_with(CLIENT, text);
    let finding = |line: u64, message: &str| ("circle-doc-class".to_string(), line, message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["circle-doc-class"]),
        [
            finding(0, "Class `Test.Loop.A` inherits from itself through `Test.Loop.B` and `Test.Loop.C`"),
            finding(1, "Class `Test.Self` inherits from itself"),
            finding(2, "Class `Test.Pair.A` inherits from itself through `Test.Pair.B`"),
            finding(3, "Class `Test.Pair.B` inherits from itself through `Test.Pair.A`"),
        ],
        "a class that only extends a cycle is not on it"
    );
    assert_eq!(
        findings(&mut client, SHARED, &["circle-doc-class"]),
        [
            finding(0, "Class `Test.Loop.B` inherits from itself through `Test.Loop.C` and `Test.Loop.A`"),
            finding(1, "Class `Test.Loop.C` inherits from itself through `Test.Loop.A` and `Test.Loop.B`"),
        ]
    );
}

#[test]
fn casts_to_classes_the_declared_type_does_not_extend_are_reported() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Animal
---@class Test.Dog : Test.Animal
---@class Test.Puppy : Test.Dog
---@class Test.Car
---@class Test.List<T>
---@alias Test.Pet Test.Dog|Test.Car

---@return Test.Animal
local function getAnimal() end

---@param animal Test.Animal?
---@param list Test.List<string>
---@param any table
---@param named string|Test.Animal
local function use(animal, list, any, named)
    ---@cast animal Test.Car
    ---@cast animal Test.Puppy
    ---@cast animal Test.Dog|Test.Car
    ---@cast animal Test.Pet
    ---@cast animal table
    ---@cast list Test.List<integer>
    ---@cast any Test.Car
    ---@cast named Test.Car
    ---@cast named Test.Dog
end

---@param dog Test.Dog
local function back(dog)
    ---@cast dog Test.Animal
end

local returned = getAnimal()
---@cast returned Test.Car
---@type Test.Animal
local built = {}
---@cast built Test.Car
use(back, returned, built)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("cast-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["cast-type-mismatch"]),
        [
            finding("@cast animal Test.Car", "Cannot convert `Test.Animal?` to `Test.Car`"),
            finding("@cast animal Test.Dog|", "Cannot convert `Test.Animal?` to `Test.Dog|Test.Car`"),
            finding("@cast animal Test.Pet", "Cannot convert `Test.Animal?` to `Test.Pet`"),
            finding("@cast named Test.Car", "Cannot convert `string|Test.Animal` to `Test.Car`"),
            finding("@cast dog", "Cannot convert `Test.Dog` to `Test.Animal`"),
            finding("@cast returned", "Cannot convert `Test.Animal` to `Test.Car`"),
        ],
        "a class needs one the declared type names or extends, as in lua-language-server: a subclass passes, a \
         parent does not, type arguments are not compared, `table` takes any class, and so does a local declared \
         with a table constructor"
    );
}

#[test]
fn locals_take_values_of_the_type_they_are_declared_with() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return string?
local function maybe() end

local speed = 5
speed = '5'
local count = 5
count = count .. 'x'
local done = false
done = {}
local enabled = true
enabled = 'yes'
local list = {}
list = 5
local name = maybe()
name = 5
local label = 'a'
label = nil
local total = 0
total = nil
local ratio = 1
ratio = 1.5
local state = 'on'
state = 'off'
local data = {}
data = { a = 1 }
local cleared = nil
cleared = 5
local unset
unset = 'x'
local fallback = 'a'
fallback = maybe()
print(speed, count, done, enabled, list, name, label, total, ratio, state, data, cleared, unset, fallback)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("cast-local-type".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["cast-local-type"]),
        [
            finding("speed = '5'", "Cannot assign `string` to `speed`, defined as `integer`"),
            finding("count = count", "Cannot assign `string` to `count`, defined as `integer`"),
            finding("done = {}", "Cannot assign `table` to `done`, defined as `boolean`"),
            finding("enabled = 'yes'", "Cannot assign `string` to `enabled`, defined as `boolean`"),
            finding("list = 5", "Cannot assign `integer` to `list`, defined as `table`"),
            finding("name = 5", "Cannot assign `integer` to `name`, defined as `string?`"),
            finding("label = nil", "Cannot assign `nil` to `label`, defined as `string`"),
            finding("total = nil", "Cannot assign `nil` to `total`, defined as `integer`"),
            finding("fallback = maybe()", "Cannot assign `string?` to `fallback`, defined as `string`"),
        ],
        "as in lua-language-server and TypeScript, a local has the type of its first value: `integer` widens to \
         `number`, a literal written out to its kind, a table takes any table, `nil` needs a type that allows it, and \
         a local declared without a value or as `nil` takes anything"
    );
}

#[test]
fn locals_typed_by_annotations_loops_and_callees_take_values_of_their_type() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Test.Mode 'a'|'b'
---@return Test.Mode
local function getMode() return 'a' end
---@return 'x'|'y'
local function getAxis() return 'x' end
---@param handler fun(id: integer, name: string)
local function each(handler) end
---@class Test.Point
---@field x number
local Point = {}
Point.__index = Point

---@type integer
local typed = 1
typed = 'x'
---@param documented string
local function document(documented, plain)
    documented = 1
    plain = 1
    plain = 'x'
end
local cast = 5
---@cast cast string
cast = 'x'
local captured = 5
local function capture() captured = 'x' end
local first, second = 1, 's'
first, second = 'x', 2
for i = 1, 3 do i = 'x' end
for index, value in ipairs({ 1, 2 }) do index = 'x'; value = 2.5 end
each(function(id, name) id = 'x'; name = 'y' end)
local loose = undefinedGlobal
loose = 5
local fn = function() end
fn = 5
local point = setmetatable({}, Point)
point = 5
point = {}
local mode = getMode()
mode = 'c'
mode = 'a'
local axis = getAxis()
axis = 'z'
local choice = math.random() > 0.5 and 'a' or 'b'
choice = 'c'
local fixed <const> = 5
fixed = 'x'
local _ = 5
_ = 'x'
local annotated = 5
---@type string
annotated = 'x'
local text = 0
text ..= 'x'
local hit = 0
_, hit = GetShapeTestResult(1)
local _, struck = GetShapeTestResult(1)
struck = 0
local part = string.strsplit(',', 'a,b')
part = 1
print(typed, document, cast, capture, first, second, loose, fn, point, mode, axis, choice, fixed, annotated, text)
print(hit, struck, part)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |code: &str, needle: &str, message: &str| (code.to_string(), line(needle), message.to_string());
    let (local, mismatch) = ("cast-local-type", "assign-type-mismatch");
    assert_eq!(
        findings(&mut client, CLIENT, &[local, mismatch]),
        [
            finding(mismatch, "typed = 'x'", "Cannot assign `string` to `typed` of type `integer`"),
            finding(mismatch, "documented = 1", "Cannot assign `integer` to `documented` of type `string`"),
            finding(local, "cast = 'x'", "Cannot assign `string` to `cast`, defined as `integer`"),
            finding(local, "captured = 'x'", "Cannot assign `string` to `captured`, defined as `integer`"),
            finding(local, "first, second = 'x'", "Cannot assign `string` to `first`, defined as `integer`"),
            finding(local, "first, second = 'x'", "Cannot assign `integer` to `second`, defined as `string`"),
            finding(local, "i = 'x'", "Cannot assign `string` to `i`, defined as `number`"),
            finding(local, "index = 'x'", "Cannot assign `string` to `index`, defined as `integer`"),
            finding(local, "id = 'x'", "Cannot assign `string` to `id`, defined as `integer`"),
            finding(local, "fn = 5", "Cannot assign `integer` to `fn`, defined as `fun()`"),
            finding(local, "point = 5", "Cannot assign `integer` to `point`, defined as `Test.Point`"),
            finding(local, "mode = 'c'", "Cannot assign `\"c\"` to `mode`, defined as `Test.Mode`"),
            finding(local, "axis = 'z'", "Cannot assign `\"z\"` to `axis`, defined as `\"x\"|\"y\"`"),
            finding(local, "annotated = 'x'", "Cannot assign `string` to `annotated`, defined as `integer`"),
            finding(local, "text ..= 'x'", "Cannot assign `string` to `text`, defined as `integer`"),
            finding(local, "part = 1", "Cannot assign `integer` to `part`, defined as `string`"),
        ],
        "a `---@type` or `@param` local is left to `assign-type-mismatch`, a cast or a `---@type` above an \
         assignment does not change the type a local is declared with, loop variables and the parameters of a \
         function passed for a function type have the type they are given, and the literals a function declares \
         it returns are the only ones its local takes; parameters without a type, `<const>` locals and `_` take \
         anything, and a `BOOL` a native returns may be an integer"
    );
}

#[test]
fn casts_of_locals_assigned_again_compare_the_values_that_reach_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
local count = 5
count = 6
---@cast count string
local label = 5
label = 'five'
---@cast label string
local unknown = 5
unknown = UnknownValue
---@cast unknown string
print(count, label, unknown)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    assert_eq!(
        findings(&mut client, CLIENT, &["cast-type-mismatch"]),
        [("cast-type-mismatch".to_string(), line("cast count"), "Cannot convert `integer` to `string`".to_string())],
        "a local that nothing annotates holds the values that reach the cast, and one of unknown type takes any"
    );
}

#[test]
fn arguments_their_parameters_do_not_take_are_reported() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param n number
local function needsNumber(n) end
---@param mode 'a'|'b'
---@param ... string
local function pick(mode, ...) end
local Box = {}
---@param size integer
function Box:resize(size) end
---@param id number
---@overload fun(name: string, label: string)
local function find(id) end
---@return string
local function name() return 'x' end
---@generic T
---@param list T[]
local function first(list) return list[1] end
---@param v string|number
local function either(v)
    if type(v) == 'number' then needsNumber(v) end
end
local mode = 'c'
---@type string?
local maybe
Settings = { Count = 'five' }
function string:shout() return string.upper(self) end

needsNumber('str')
needsNumber(5)
pick('c')
pick('a', 'x', 2)
pick(mode)
Box:resize('big')
Box.resize(Box, 'large')
find('name', 'label')
find('alone')
find(1, 'label')
needsNumber(nil)
needsNumber(false)
needsNumber(maybe)
needsNumber(Settings.Count)
needsNumber(name() --[[@as number]])
needsNumber(name())
local rest = string.sub(123, 2)
local head = first(5)
SetEntityHeading(PlayerPedId(), 'north')
local position = vector4(vector3(1, 2, 3), 4.0)
---@diagnostic disable-next-line: param-type-mismatch
needsNumber('suppressed')
print(either, rest, head, position)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("param-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch"]),
        [
            finding("needsNumber('str')", "Cannot assign `string` to parameter `n` of type `number`"),
            finding("pick('c')", "Cannot assign `\"c\"` to parameter `mode` of type `\"a\"|\"b\"`"),
            finding("pick('a', 'x', 2)", "Cannot assign `integer` to parameter `...` of type `string`"),
            finding("Box:resize('big')", "Cannot assign `string` to parameter `size` of type `integer`"),
            finding("Box.resize(Box, 'large')", "Cannot assign `string` to parameter `size` of type `integer`"),
            finding("find('alone')", "Cannot assign `string` to parameter `id` of type `number`"),
            finding("find(1, 'label')", "Cannot assign `integer` to parameter `name` of type `string`"),
            finding("needsNumber(nil)", "Cannot assign `nil` to parameter `n` of type `number`"),
            finding("needsNumber(false)", "Cannot assign `boolean` to parameter `n` of type `number`"),
            finding("needsNumber(maybe)", "Cannot assign `string?` to parameter `n` of type `number`"),
            finding("needsNumber(name())", "Cannot assign `string` to parameter `n` of type `number`"),
            finding("SetEntityHeading", "Cannot assign `string` to parameter `heading` of type `number`"),
        ],
        "a call passes when a signature that takes as many arguments takes them all; `nil` and `false` are \
         values like any other, as in LuaLS; values inferred from assignments, casts, type guards, generics, \
         the string a string method is called on and suppressed lines pass"
    );
}

#[test]
fn natives_take_their_arguments_as_the_runtime_converts_them() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@type Ped?
local maybePed
---@type string?
local maybeName
local coords = vector3(1, 2, 3)
local ped = PlayerPedId()
SetEntityVisible(ped, 1, 0)
SetEntityHeading(ped, true)
print(GetHashKey(123), GetHashKey(maybeName))
RequestModel('adder')
SetEntityCoords(ped, coords, false, false, false, false)
SetEntityCoords(ped, coords.x, coords.y, coords.z, false, false, false, false)
DrawMarker(1, coords, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 255, 0, 0, 100, false, true, 2, false, nil, nil, false)
SetEntityCoords(ped, UnknownCoords, 'x')
AddExplosion(1.0, 2.0, 3.0, 'EXPLOSION_TANKER', 2.0, true, false, 2.0)
NetworkSetInSpectatorMode(false, nil)
print(DoesEntityExist(maybePed))
SetEntityCoords(ped, coords, 'flag', false, false, false)
---@param point vector
local function place(point)
    SetEntityCoords(ped, point, 'size unknown')
end
print(place)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("param-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch"]),
        [
            finding("AddExplosion", "Cannot assign `string` to parameter `explosionType` of type `integer`"),
            finding("NetworkSetInSpectatorMode", "Cannot assign `nil` to parameter `playerPed` of type `Ped`"),
            finding("DoesEntityExist", "Cannot assign `Ped?` to parameter `entity` of type `Entity`"),
            finding("'flag'", "Cannot assign `string` to parameter `alive` of type `boolean`"),
        ],
        "numbers and booleans pass for each other, a string parameter takes numbers and `nil`, a hash \
         parameter strings, a vector fills a number parameter for each of its parts, and after a value of \
         unknown type where a vector may go, the values that follow are not checked"
    );
}

#[test]
fn native_string_parameters_take_only_numbers_and_booleans_written_out() {
    let mut client = Client::start(fixture_root());
    let text = "\
local name = 'target'
---@type number
local volume = 30
---@type string|number
local value = 1
ReleaseNamedRendertarget(GetHashKey(name))
ReleaseNamedRendertarget(0)
ReleaseNamedRendertarget(false)
ReleaseNamedRendertarget(nil)
SetConvarReplicated('volume', 30)
SetConvarReplicated('volume', true)
SetConvarReplicated('volume', volume)
SetConvarReplicated('volume', value)
TaskStartScenarioInPlace(PlayerPedId(), 'WORLD_HUMAN_SMOKING', true)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("param-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch"]),
        [
            finding("GetHashKey(name)", "Cannot assign `Hash` to parameter `name` of type `string`"),
            finding("volume', volume)", "Cannot assign `number` to parameter `value` of type `string`"),
            finding("volume', value)", "Cannot assign `string|number` to parameter `value` of type `string`"),
        ],
        "`nil`, `0` and `false` are NULL to a native and a literal is the text it is written as, while \
         other numbers and booleans are no string; a boolean still passes for a number"
    );
    let server = "\
---@type number
local src = 1
DropPlayer(src, 'kicked')
";
    client.open_with(SERVER, server);
    assert!(
        findings(&mut client, SERVER, &["param-type-mismatch"]).is_empty(),
        "a player's server id is a number to scripts"
    );
}

#[test]
fn arguments_are_compared_with_the_definitions_the_side_of_the_call_reaches() {
    let mut client = Client::start(fixture_root());
    client.open_with("myresource/shared/config.lua", "Lib = {}\n");
    let client_text = "\
---@param data table
function Lib.notify(data) end

---@param text string
---@param kind string
exports('Notify', function(text, kind) end)

Lib.notify({ title = 'saved' })
Lib.notify('saved')
exports.myresource:Notify('saved', 'success')
exports.myresource:Notify('saved', 5)
";
    let server_text = "\
---@param source number
---@param data table
function Lib.notify(source, data) end

Lib.notify(1, { title = 'saved' })
Lib.notify('saved')
";
    client.open_with(CLIENT, client_text);
    client.open_with(SERVER, server_text);
    let finding = |text: &str, needle: &str, message: &str| {
        ("param-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch"]),
        [
            finding(client_text, "Lib.notify('saved')", "Cannot assign `string` to parameter `data` of type `table`"),
            finding(client_text, "Notify('saved', 5)", "Cannot assign `integer` to parameter `kind` of type `string`"),
        ],
        "the exports proxy passes the arguments of a `:` call without the receiver"
    );
    assert_eq!(
        findings(&mut client, SERVER, &["param-type-mismatch"]),
        [finding(server_text, "Lib.notify('saved')", "Cannot assign `string` to parameter `source` of type `number`")],
        "the client definition does not run on the server"
    );
}

#[test]
fn callbacks_take_the_generics_that_declared_arguments_bind() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param n number
local function needsNumber(n) end
---@generic T
---@param _type `T`
---@param handler fun(resolve: fun(value: T), reject: fun(reason: string))
local function promise(_type, handler) end
---@generic T
---@param value T
---@param handler fun(set: fun(value: T), get: fun(): T)
local function watch(value, handler) end
Settings = { Count = 'five' }

promise('boolean', function(resolve, reject)
    resolve(true)
    reject(5)
end)
watch(true, function(set, get)
    set(1)
    needsNumber(get())
end)
watch(Settings.Count, function(set) set(1) end)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("param-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch"]),
        [
            finding("reject(5)", "Cannot assign `integer` to parameter `reason` of type `string`"),
            finding("set(1)\n", "Cannot assign `integer` to parameter `value` of type `boolean`"),
            finding("needsNumber(get())", "Cannot assign `boolean` to parameter `n` of type `number`"),
        ],
        "a function the callee passes takes and returns what the other arguments declare for its generics; \
         a generic bound from an inferred value takes any value"
    );
}

#[test]
fn parameters_of_callbacks_have_the_types_their_callee_declares() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Probe.Player
---@field name string
---@param n number
local function needsNumber(n) end
---@generic T
---@param kind `T`
---@param cb fun(value: T)
local function onValue(kind, cb) end
---@param cb fun(value: string)
local function onString(cb) end
---@param action string
---@param handler fun(...)
---@overload fun(action: 'updated', handler: fun(player: Probe.Player))
---@overload fun(action: 'updated', handler: fun(source: number, player: Probe.Player))
local function onAction(action, handler) end
Settings = { Kind = 'Probe.Player' }

onValue('Probe.Player', function(value) needsNumber(value) end)
onValue('boolean', function(value) needsNumber(value) end)
onValue(Settings.Kind, function(value) needsNumber(value) end)
onString(function(value) needsNumber(value) end)
---@param value number
onString(function(value) needsNumber(value) end)
onString(function(value)
    if value == 5 then print(value) end
end)
onAction('updated', function(source) needsNumber(source) end)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |code: &str, needle: &str, message: &str| (code.to_string(), line(needle), message.to_string());
    let mismatch = |needle: &str, given: &str| {
        let message = format!("Cannot assign `{given}` to parameter `n` of type `number`");
        finding("param-type-mismatch", needle, &message)
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch", "impossible-comparison"]),
        [
            mismatch("onValue('Probe.Player'", "Probe.Player"),
            mismatch("onValue('boolean'", "boolean"),
            mismatch("onString(function(value) needs", "string"),
            finding("impossible-comparison", "value == 5", "Comparing `string` with `5` is always false"),
        ],
        "a parameter takes what the callee declares for it, with the generics the other arguments declare, \
         unless a `@param` line above the call types it or overloads that fit as well type it differently"
    );
}

#[test]
fn unknown_beside_other_types_takes_any_value() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param card unknown|string
local function present(card) end
---@param value unknown?
local function store(value) end
---@type unknown|string
local anything = {}
---@return unknown|string
local function describe() return {} end

present({})
store(5)
print(anything, describe)
";
    client.open_with(CLIENT, text);
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch", "assign-type-mismatch", "return-type-mismatch"]),
        []
    );
    for (needle, expected) in [
        ("present(card)", "present(card: string|unknown)"),
        ("store(value)", "store(value?: unknown?)"),
        ("anything = {}", "anything: string|unknown"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn literals_in_locals_count_by_their_kind_and_negation_by_its_operand() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param s string
local function needsString(s) end
---@param n number
local function needsNumber(n) end
---@param mode 'fast'|'slow'
local function run(mode) end
local count = 5
local mode = 'dev'

needsNumber(-'5')
needsString(-count)
needsString(count)
run(mode)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str| {
        let message = "Cannot assign `integer` to parameter `s` of type `string`".to_string();
        ("param-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message)
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["param-type-mismatch"]),
        [finding("needsString(-count)"), finding("needsString(count)")],
        "a negated string is a number, and a literal stored in a local passes for the literals of its kind"
    );
}

#[test]
fn arguments_of_calls_to_globals_of_escrowed_resources_are_not_checked() {
    let mut client = Client::start(fixture_root());
    let text = "\
exports('Open', function() end)

---@param amount number
function VaultDeposit(amount) end

VaultDeposit('all')
VaultDeposit(1, 2)
print(math.floor('x'))
";
    client.open_with("vault/open.lua", text);
    assert_eq!(
        findings(&mut client, "vault/open.lua", &["param-type-mismatch", "redundant-parameter"]),
        [(
            "param-type-mismatch".to_string(),
            pos(text, "math.floor", 0).0 as u64,
            "Cannot assign `string` to parameter `x` of type `number`".to_string()
        )],
        "the encrypted script of vault may define `VaultDeposit` differently, but not the runtime's functions"
    );
}

#[test]
fn a_global_declared_as_nil_takes_any_value() {
    let mut client = Client::start(fixture_root());
    let text = "\
TestCurrentZone = nil

RegisterNetEvent('test:enterZone', function(name)
    TestCurrentZone = name
end)

---@type string
local zone = TestCurrentZone

---@param name string
local function enter(name)
    name = TestCurrentZone
end

---@return string
local function current()
    return TestCurrentZone
end
";
    client.open_with(CLIENT, text);
    assert_eq!(findings(&mut client, CLIENT, &["assign-type-mismatch", "return-type-mismatch"]), []);
}

#[test]
fn shared_calls_of_a_function_each_side_defines_differently_return_unknown() {
    let mut client = Client::start(fixture_root());
    client.open_with(CLIENT, "---@return string\nfunction TestGetJob()\n    return 'police'\nend\n");
    client.open_with(
        SERVER,
        "---@param source number\n---@return { name: string }\nfunction TestGetJob(source)\n    return { name = 'police' }\nend\n",
    );
    let shared = "myresource/shared/config.lua";
    let text = "\
local function check(source)
    ---@type string?
    local jobName
    if source then
        jobName = TestGetJob(source).name
    else
        jobName = TestGetJob()
    end
    return jobName
end

---@return string
local function current()
    return TestGetJob()
end

---@return { name: string }
local function record()
    return TestGetJob()
end

---@type table
local job = TestGetJob(1)

if IsDuplicityVersion() then
    ---@type string
    local onServer = TestGetJob(1)
else
    ---@type table
    local onClient = TestGetJob()
    ---@type table
    local unfit = TestGetJob(1)
end
";
    client.open_with(shared, text);
    let finding = |needle: &str, message: &str| {
        ("assign-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, shared, &["assign-type-mismatch", "return-type-mismatch"]),
        [
            finding("local onServer", "Cannot assign `{ name: string }` to `onServer` of type `string`"),
            finding("local onClient", "Cannot assign `string` to `onClient` of type `table`"),
        ],
        "code that runs on both sides calls neither definition, and one that its arguments do not fit \
         is none either"
    );
}

#[test]
fn literal_keyed_class_fields_type_their_own_keys() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.RawEmployee
---@field [1] number Source
---@field [2] string Character name
---@field [9] boolean Visible
---@field [true] string

---@param employee Test.RawEmployee
---@param slot integer
local function show(employee, slot)
    local source = employee[1]
    local name = employee[2]
    local visible = employee[9]
    local flagged = employee[true]
    local absent = employee[3]
    local picked = employee[slot]
    for key, value in pairs(employee) do end
    for i, element in ipairs(employee) do end
end
";
    client.open_with(CLIENT, text);
    let cases = [
        ("source =", "source: number"),
        ("name =", "name: string"),
        ("visible =", "visible: boolean"),
        ("flagged", "flagged: string"),
        ("absent", "absent: unknown"),
        ("picked", "picked: number|string|boolean"),
        ("key,", "key: integer|boolean"),
        ("value in", "value: number|string|boolean"),
        ("i,", "i: integer"),
        ("element", "element: number|string|boolean"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }

    let (l, c) = pos(text, "Test.RawEmployee\n---@param slot", 1);
    let hover = client.hover_text(CLIENT, l, c);
    for part in ["[1]: number,", "[2]: string,", "[9]: boolean,", "[true]: string,"] {
        assert!(hover.contains(part), "missing {part:?} in {hover}");
    }
}

#[test]
fn class_table_diagnostics_and_completion_follow_the_selected_overload() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class (strict) Test.OverloadA
---@field a string
---@class (strict) Test.OverloadB
---@field b number

---@overload fun(kind: 'b', value: Test.OverloadB)
---@param kind 'a'
---@param value Test.OverloadA
local function consume(kind, value) end
consume('a', { a = 'ok' })
consume('b', { b = 42 })
consume('b', { b = 'wrong' })
consume('b', {})
consume('b', { b = 42, extra = true })
";
    client.open_with(CLIENT, text);
    let (line, character) = pos(text, "consume('b', { b = 42 })", 2);
    let hover = client.hover_text(CLIENT, line, character);
    assert!(hover.contains("Test.OverloadB"), "{hover}");
    let codes = ["missing-fields", "assign-type-mismatch", "undeclared-field"];
    let found = findings(&mut client, CLIENT, &codes);
    for (code, needle) in [
        ("assign-type-mismatch", "consume('b', { b = 'wrong' })"),
        ("missing-fields", "consume('b', {})"),
        ("undeclared-field", "consume('b', { b = 42, extra = true })"),
    ] {
        assert!(
            found.iter().any(|(seen, line, _)| seen == code && *line == pos(text, needle, 0).0 as u64),
            "{found:?}"
        );
    }
    assert_eq!(found.len(), 3, "valid overload calls must not be checked against Test.OverloadA: {found:?}");
    let (line, character) = pos(text, "consume('b', {})", 14);
    let labels = client.completion_labels(CLIENT, line, character);
    assert!(labels.contains(&"b".to_string()) && !labels.contains(&"a".to_string()), "{labels:?}");
}

#[test]
fn indexed_class_fields_keep_their_side_in_diagnostics_and_inference() {
    let mut client = Client::start(fixture_root());
    client.open_with(
        "myresource/shared/config.lua",
        "\
---@class (strict) Test.SidedFields
---@field (server) [1] string
---@field (client) [1] number
---@field (server) [true] string
---@field (client) [true] boolean

---@class Test.SidedIndex
---@field (server) [integer] string
---@field (client) [integer] number

---@class (strict) Test.ServerOnlyField
---@field (server) [2] string

---@class (strict) Test.ServerOnlyIndex
---@field (server) [integer] string

---@class Test.InheritedFields : Test.SidedFields

---@class Test.InheritedIndex : Test.SidedIndex

---@class Test.UnscopedIndex
---@field [integer] boolean
---@field [integer] number

---@class Test.UnscopedFields
---@field [1] number

---@class Test.ReversedIndex
---@field (client) [integer] number
---@field (server) [integer] string

",
    );
    for (file, value, flag, expected, expected_flag, wrong) in
        [(CLIENT, "42", "false", "number", "boolean", "'wrong'"), (SERVER, "'ok'", "'flag'", "string", "string", "42")]
    {
        let text = format!(
            "\
---@type Test.SidedFields
local fields = {{ {value}, [true] = {flag} }}
local first = fields[1]
local flagged = fields[true]
for slot, element in ipairs(fields) do end
---@type Test.SidedIndex
local indexed = {{ {value} }}
local item = indexed[42]
for key, entry in pairs(indexed) do end
---@type Test.InheritedFields
local inherited = {{ {value}, [true] = {flag} }}
---@type Test.InheritedIndex
local inheritedIndex = {{ {value} }}
---@type Test.ReversedIndex
local reversed = {{ {value} }}
---@type Test.UnscopedIndex
local unscoped = {{ 42 }}
---@type Test.UnscopedFields
local unscopedFields = {{ 42 }}
fields[1] = {wrong}
indexed[42] = {wrong}
---@type Test.ServerOnlyField
local serverField = {{ [2] = 'ok' }}
---@type Test.ServerOnlyIndex
local serverIndex = {{ [2] = 'ok' }}
"
        );
        client.open_with(file, &text);
        let found = findings(&mut client, file, &["assign-type-mismatch", "undeclared-field"]);
        let expected_count = if file == CLIENT { 4 } else { 2 };
        assert_eq!(found.len(), expected_count, "{file}: {found:?}");
        for needle in ["fields[1] =", "indexed[42] ="] {
            assert!(
                found
                    .iter()
                    .any(|(code, line, _)| code == "assign-type-mismatch" && *line == pos(&text, needle, 0).0 as u64),
                "{found:?}"
            );
        }
        if file == CLIENT {
            for needle in ["local serverField", "local serverIndex"] {
                assert!(
                    found
                        .iter()
                        .any(|(code, line, _)| code == "undeclared-field" && *line == pos(&text, needle, 0).0 as u64),
                    "{found:?}"
                );
            }
        }
        for (needle, ty) in [
            ("first", expected),
            ("flagged", expected_flag),
            ("element", expected),
            ("item", expected),
            ("entry", expected),
        ] {
            let (line, character) = pos(&text, needle, 0);
            let hover = client.hover_text(file, line, character);
            assert!(hover.contains(&format!("{needle}: {ty}")), "{file} {needle}: {hover}");
        }
        let (line, character) = pos(&text, "Test.SidedFields", 1);
        let hover = client.hover_text(file, line, character);
        assert!(hover.contains(&format!("[1]: {expected},")), "{hover}");
        assert!(hover.contains(&format!("[true]: {expected_flag},")), "{hover}");
        let (line, character) = pos(&text, "Test.SidedIndex", 1);
        let hover = client.hover_text(file, line, character);
        assert!(hover.contains(&format!("[integer]: {expected},")), "{hover}");
    }
}

#[test]
fn repeat_bodies_that_always_return_have_no_fallthrough() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return number
local function direct()
    repeat return 42 until true
end
---@return number
local function branches(flag)
    repeat
        if flag then return 1 else return 2 end
    until flag
end
---@return number
local function canBreak(flag)
    repeat
        if flag then break end
        return 42
    until true
end -- canBreak
---@return number
local function canFinish(flag)
    repeat
        if flag then return 42 end
    until true
end -- canFinish
local function inferred()
    repeat return 42 until true
end
local result = inferred()
";
    client.open_with(CLIENT, text);
    let found = findings(&mut client, CLIENT, &["missing-return"]);
    let lines: Vec<_> = found.iter().map(|(_, line, _)| *line).collect();
    assert_eq!(
        lines,
        [pos(text, "end -- canBreak", 0).0 as u64, pos(text, "end -- canFinish", 0).0 as u64],
        "{found:?}"
    );
    let (line, character) = pos(text, "result", 0);
    let hover = client.hover_text(CLIENT, line, character);
    assert!(hover.contains("result: integer") && !hover.contains("integer?"), "{hover}");
}

#[test]
fn literal_completions_preserve_values_in_both_quote_styles() {
    use qbx_lua_syntax::ast::{ExprKind, StmtKind};
    let mut client = Client::start(fixture_root());
    for value in ["owner's", "say \"hello\"", "C:\\temp\\file", "with\ttab", "control\u{7f}", "café", "${1:literal}"] {
        let delimiter = if value.contains('\'') { '"' } else { '\'' };
        let defs = format!("---@param value {delimiter}{value}{delimiter}\nfunction SayLiteral(value) end\n");
        client.open_with("myresource/shared/config.lua", &defs);
        for quote in ['\'', '"'] {
            for in_string in [false, true] {
                let call =
                    if in_string { format!("SayLiteral({quote}prefix|{quote})") } else { "SayLiteral(|)".to_string() };
                let text = format!("local label = {quote}x{quote}\n{call}");
                let (line, character) = pos(&text, "|", 0);
                let text = text.replace('|', "");
                client.open_with(CLIENT, &text);
                let result = client.request("textDocument/completion", client.position_params(CLIENT, line, character));
                let item = result["items"].as_array().unwrap().iter().find(|item| item["kind"] == 20).unwrap();
                let completed = if in_string {
                    let edit = &item["textEdit"];
                    let start = edit["range"]["start"]["character"].as_u64().unwrap() as usize;
                    let end = edit["range"]["end"]["character"].as_u64().unwrap() as usize;
                    let call = text.lines().last().unwrap();
                    format!("{}{}{}", &call[..start], edit["newText"].as_str().unwrap(), &call[end..])
                } else {
                    format!("SayLiteral({})", item["insertText"].as_str().unwrap())
                };
                let parsed = qbx_lua_syntax::parse(&completed);
                assert!(parsed.errors.is_empty(), "{completed}: {:?}", parsed.errors);
                let StmtKind::Expr(call) = &parsed.block.stmts[0].kind else { panic!("{completed}") };
                let ExprKind::Call { args, .. } = &call.kind else { panic!("{completed}") };
                assert_eq!(args[0].as_string().map(|value| value.as_str()), Some(value), "{completed}");
            }
        }
    }
}

#[test]
fn documented_returns_have_to_match_their_annotations() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Point
---@field x number
---@field y number

---@class (strict) Test.Sealed
---@field name string

---@return string
local function wrong() return 5 end

---@return string
local function maybe(flag)
    if flag then return 'a' end
end

---@return string?
local function optional(flag)
    if flag then return 'a' end
end

---@return integer
local function waitFor()
    while true do
        if math.random() > 0.5 then return 1 end
    end
end

---@return string
---@return integer
local function pair() return 'a' end

---@return Test.Point
local function point() return { x = 1 } end

---@return Test.Sealed
local function sealed() return { name = 'a', extra = 1 } end

---@return boolean
local function fail() error('no') end

---@return string
function TestStub() end

---@return string
local nothing = function() return nil end

---@return 'a'|'b'
local function letter() return 'c' end

---@return ...string
local function many() return 'a', 1 end

---@return string
local function reassigned()
    local result = nil
    result = 'x'
    return result
end

---@return integer
local function outer()
    local inner = function() return 'x' end
    return inner and 1 or 2
end

---@return string
local function forwarded() return wrong() end

---@return table?
local function slotOf(inventory)
    local slot = inventory and inventory.items[1]
    return slot
end
print(maybe, optional, waitFor, pair, point, sealed, fail, nothing, letter, many, reassigned, outer, forwarded, slotOf)

---@param a number
---@param b number
---@return number
RegisterServerCallback('test:add', function(source, a, b)
end)

---@return string
Test:register('test:name', function(source) return 1 end)

---@return string
local kept = RegisterServerCallback('test:kept', function(source) return 2 end)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |code: &str, at: u64, message: &str| (code.to_string(), at, message.to_string());
    let (mismatch, missing) = ("return-type-mismatch", "missing-return");
    assert_eq!(
        findings(&mut client, CLIENT, &[mismatch, missing, "missing-fields", "undeclared-field"]),
        [
            finding(mismatch, line("return 5"), "Cannot return `integer` as return value #1 of type `string`"),
            finding(
                missing,
                line("if flag then return 'a' end\nend") + 1,
                "The function can reach its end without returning, but `@return` requires `string`"
            ),
            finding(
                missing,
                line("return 'a' end\n\n---@return Test.Point"),
                "`@return` requires 2 values, but this returns 1 value"
            ),
            finding("missing-fields", line("return { x = 1 }"), "Missing required fields in type `Test.Point`: `y`"),
            finding(
                "undeclared-field",
                line("extra = 1"),
                "Field `extra` is not declared in strict class `Test.Sealed`"
            ),
            finding(
                missing,
                line("function TestStub() end"),
                "The function can reach its end without returning, but `@return` requires `string`"
            ),
            finding(mismatch, line("return nil"), "Cannot return `nil` as return value #1 of type `string`"),
            finding(mismatch, line("return 'c'"), "Cannot return `\"c\"` as return value #1 of type `\"a\"|\"b\"`"),
            finding(mismatch, line("return 'a', 1"), "Cannot return `integer` as return value #2 of type `string`"),
            // `forwarded` returns what `wrong` declares; its own `return 5` is reported above.
            // The doc comment above a call applies to the functions passed to it.
            finding(
                missing,
                line("function(source, a, b)") + 1,
                "The function can reach its end without returning, but `@return` requires `number`"
            ),
            finding(mismatch, line("return 1 end)"), "Cannot return `integer` as return value #1 of type `string`"),
            // The same above a statement that keeps what the call returns.
            finding(mismatch, line("return 2 end)"), "Cannot return `integer` as return value #1 of type `string`"),
        ],
        "optional values, endless loops, error(), reassigned locals and nested functions pass"
    );

    // A `---@meta` file only declares signatures, so its empty bodies pass.
    let declared = "---@return string\nfunction TestDeclared() end\n";
    client.open_with(SERVER, declared);
    assert_eq!(findings(&mut client, SERVER, &[missing]).len(), 1, "an empty body is missing its value");
    client.change(SERVER, 2, &format!("---@meta\n\n{declared}"));
    let found = findings(&mut client, SERVER, &[missing]);
    assert!(found.is_empty(), "{found:?}");
    client.change(SERVER, 3, &format!("local loaded = true\n---@meta\n\n{declared}"));
    assert_eq!(findings(&mut client, SERVER, &[missing]).len(), 1, "`---@meta` only counts above the first statement");
}

#[test]
fn values_of_nodiscard_functions_have_to_be_used() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@nodiscard
---@return integer
local function important() return 1 end
local Store = {}
---@nodiscard
---@return string
function Store:key() return 'k' end
local function plain() return 1 end

important()
local kept = important()
Store:key()
plain()
tostring(5)
local text = 'abc'
text:upper()
text:gsub('%w+', print)
math.random()
math.random(5)
math.random(1, 10)
print(important(), kept)
---@diagnostic disable-next-line: discard-returns
important()
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("discard-returns".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["discard-returns"]),
        [
            finding("important()\nlocal kept", "The values that `important` returns cannot be discarded"),
            finding("Store:key()\nplain", "The values that `Store:key` returns cannot be discarded"),
            finding("tostring(5)", "The values that `tostring` returns cannot be discarded"),
            finding("text:upper()", "The values that `text:upper` returns cannot be discarded"),
            finding("math.random(1, 10)", "The values that `math.random` returns cannot be discarded"),
        ],
        "values that are used, functions without `@nodiscard`, `@overload`s, which are not marked, and \
         suppressed lines pass"
    );
}

#[test]
fn discarded_values_of_globals_of_escrowed_resources_are_not_reported() {
    let mut client = Client::start(fixture_root());
    let text = "\
exports('Open', function() end)

---@nodiscard
---@return integer
function VaultCount() return 1 end

VaultCount()
tostring(5)
";
    client.open_with("vault/open.lua", text);
    assert_eq!(
        findings(&mut client, "vault/open.lua", &["discard-returns"]),
        [(
            "discard-returns".to_string(),
            pos(text, "tostring(5)", 0).0 as u64,
            "The values that `tostring` returns cannot be discarded".to_string()
        )],
        "the encrypted script of vault may define `VaultCount` differently, but not the runtime's functions"
    );
}

#[test]
fn uses_of_deprecated_globals_fields_and_methods_are_reported() {
    let mut client = Client::start(fixture_root());
    client.open_with(
        "myresource/shared/config.lua",
        "\
---@deprecated use NewThing
function OldThing() end

DepLib = {}
---@deprecated
function DepLib.old() end
function DepLib.new() end
---@deprecated use DepLib:fresh
function DepLib:stale() end

---@class DepClass
---@field kept fun()
local DepClass = {}
---@deprecated
function DepClass:legacy() end
---@deprecated
function DepClass.kept() end
---@deprecated
function DepClass.twice() end
function DepClass.twice() end
",
    );
    let text = "\
OldThing()
local alias = OldThing
DepLib.old()
DepLib:stale()
DepLib.new()
print(DepLib['old'], alias)
---@type DepClass
local value = DepClass
value:legacy()
value.kept()
value.twice()
local M = {}
---@deprecated
function M.gone() end
M.gone()
---@deprecated
local function hidden() end
hidden()
RegisterServerEvent('x')
---@diagnostic disable-next-line: deprecated
OldThing()
";
    client.open_with(CLIENT, text);
    let finding =
        |needle: &str, message: &str| ("deprecated".to_string(), pos(text, needle, 0).0 as u64, message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &["deprecated"]),
        [
            finding("OldThing()", "'OldThing' is deprecated: use NewThing"),
            finding("local alias", "'OldThing' is deprecated: use NewThing"),
            finding("DepLib.old()", "'DepLib.old' is deprecated"),
            finding("DepLib:stale()", "'DepLib:stale' is deprecated: use DepLib:fresh"),
            finding("print(DepLib", "'DepLib.old' is deprecated"),
            finding("value:legacy()", "'value:legacy' is deprecated"),
            finding("value.kept()", "'value.kept' is deprecated"),
            finding(
                "M.gone()
---@deprecated",
                "'M.gone' is deprecated"
            ),
            finding("RegisterServerEvent", "'RegisterServerEvent' is deprecated"),
        ],
        "a `---@field` does not keep a field from being deprecated, another definition does, locals are left \
         alone, and the runtime's `RegisterServerEvent` is reported once, by the linter"
    );
}

#[test]
fn returns_with_more_values_than_declared_are_reported() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@return integer
local function one() return 1, 2 end

---@return boolean ok
---@return string? err
local function two() return true, nil, 3, 4 end

---@return integer, ...string
local function rest() return 1, 'a', 'b' end

---@return number? ...
local function scalars() return 1, 2, 3 end

---@return integer first ...
local function described() return 1, 2 end

---@return false | (string, string)
local function name() return 'Joe', 'Doe', 'Smith' end

---@param a? string
---@return integer
---@overload fun(a: string): integer, string
local function both(a) return 1, a end

---@return string
local function escape(text) return text:gsub('%%', '%%%%') end

---@return integer
local function tail() return 1, print() end

---@return integer
local function more() return 1, 2, one() end

---@return integer
local function quiet()
    ---@diagnostic disable-next-line: redundant-return-value
    return 1, 2
end

local function plain() return 1, 2, 3 end

---@return boolean
RegisterNetEvent('test:check', function() return true, 'extra' end)

print(rest, scalars, described, name, both, escape, tail, more, quiet, plain)
";
    client.open_with(CLIENT, text);
    let finding = |needle: &str, message: &str| {
        ("redundant-return-value".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, CLIENT, &["redundant-return-value"]),
        [
            finding("local function one()", "`@return` allows at most 1 value, but this returns 2 values"),
            finding("local function two()", "`@return` allows at most 2 values, but this returns 4 values"),
            finding("local function described()", "`@return` allows at most 1 value, but this returns 2 values"),
            finding("local function name()", "`@return` allows at most 2 values, but this returns 3 values"),
            finding("local function more()", "`@return` allows at most 1 value, but this returns at least 2 values"),
            finding("return true, 'extra'", "`@return` allows at most 1 value, but this returns 2 values"),
        ],
        "a trailing `...T` or `T ...`, the longest set of values or `@overload`, what a call at the end gives, \
         undocumented functions and suppressed lines pass, while a `...` after the name of a value describes it"
    );
}

#[test]
fn functions_written_as_a_typed_function_return_its_values() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@param cb fun(n: integer): string
local function withCb(cb) return cb end
withCb(function(n) return n end)
withCb(function(n) return 'a', 2 end)
withCb(function(n) end)
withCb(function(n) return tostring(n) end)

---@param cb (fun(): string)?
local function optional(cb) end
optional(function() return 1 end)

---@param cb fun()
local function none(cb) end
none(function() return 1 end)

---@generic T
---@param cb fun(): T
---@return T
local function generic(cb) return cb() end
generic(function() return 1 end)

---@param cb fun(): string
---@overload fun(cb: fun(): integer)
local function overloaded(cb) end
overloaded(function() return 1 end)

---@alias Test.Searcher
---| fun(name: string): function
---| fun(name: string): nil, string
---@param searcher Test.Searcher
local function search(searcher) end
search(function(name) return nil, name end)

---@type fun(): string
local typed = function() return 3 end

---@class Test.Options
---@field onDone fun(): boolean

---@param options Test.Options
local function open(options) end
open({ onDone = function() return 'no' end })

---@param cb fun(): string
local function documented(cb) end
---@return integer
documented(function() return 1 end)

---@param cb function
local function untyped(cb) end
untyped(function() return 1 end)

AddEventHandler('test:local', function() return 5 end)
RegisterNetEvent('test:net', function() return 'x', 1 end)
print(typed)
";
    client.open_with(CLIENT, text);
    let codes = ["return-type-mismatch", "missing-return", "redundant-return-value"];
    let finding = |code: &str, needle: &str, message: &str| {
        (code.to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    let mut found = findings(&mut client, CLIENT, &codes);
    found.sort();
    let mut expected = vec![
        finding("return-type-mismatch", "return n end", "Cannot return `integer` as return value #1 of type `string`"),
        finding(
            "redundant-return-value",
            "return 'a', 2",
            "Its function type allows at most 1 value, but this returns 2 values",
        ),
        finding(
            "missing-return",
            "withCb(function(n) end)",
            "The function can reach its end without returning, but its function type requires `string`",
        ),
        finding(
            "return-type-mismatch",
            "optional(function",
            "Cannot return `integer` as return value #1 of type `string`",
        ),
        finding(
            "redundant-return-value",
            "none(function",
            "Its function type allows at most no values, but this returns 1 value",
        ),
        finding("return-type-mismatch", "return 3 end", "Cannot return `integer` as return value #1 of type `string`"),
        finding("return-type-mismatch", "return 'no'", "Cannot return `string` as return value #1 of type `boolean`"),
        finding(
            "redundant-return-value",
            "return 5 end",
            "Event handlers' return values are discarded, but this returns 1 value",
        ),
        finding(
            "redundant-return-value",
            "return 'x', 1",
            "Event handlers' return values are discarded, but this returns 2 values",
        ),
    ];
    expected.sort();
    assert_eq!(
        found, expected,
        "the type of the parameter, `---@type` or field a function is written as declares its values, unless a \
         generic, several fitting signatures, a union of function types or its own `@return` decides them"
    );
}

#[test]
fn hover_binds_generics_from_arguments_and_callbacks() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@generic K, V, RK, RV
---@param tbl table<K, V>
---@param fun fun(value: V, key: K): RV, RK
---@return table<RK, RV>
function table.mapEntries(tbl, fun)
    local result = {}
    for key, value in pairs(tbl) do
        local newValue, newKey = fun(value, key)
        result[newKey or key] = newValue
    end
    return result
end

local function normalize(step)
    return step / 10
end

---@generic T
---@param value? T
---@return T|string
local function orName(value) end

---@generic T
---@param value T
---@param onBox? fun(box: { value: T })
---@return { value: T }
local function box(value, onBox) end

---@param steps { [number]: number }
local function send(steps)
    local mapped = table.mapEntries(steps, function(step, featureId)
        return normalize(step), tostring(featureId)
    end)
    local fromList = table.mapEntries({ 'a', 'b' }, function(letter, position)
        return position, letter
    end)
    local unbound = table.mapEntries(steps, function() end)
    local named = orName()
    local boxed = box(1, function(opened) end)
end
";
    client.open_with(CLIENT, text);
    let cases = [
        ("mapped", "mapped: table<string, number>"),
        ("step, f", "step: number"),
        ("featureId", "featureId: number"),
        ("fromList", "fromList: table<string, integer>"),
        ("letter,", "letter: string"),
        ("position)", "position: integer"),
        ("unbound", "unbound: table<unknown, unknown>"),
        ("named =", "named: string"),
        ("boxed", "value: integer"),
        ("opened", "value: integer"),
        // Inside the generic function its parameters stay generic.
        ("key, value", "key: K"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn hover_types_the_parameters_of_functions_in_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class TArgs
---@field action fun(scrollIndex?: number)
---@field items? { onSelect: fun(args: string) }[]

---@alias Handler fun(source: integer, payload: string)

local documented = {
    ---@param a1 number
    action = function(a1) end,
    ---@param k1 boolean
    [ 'keyed' ] = function(k1) end,
}

---@type TArgs
local declared = {
    action = function(b1) end,
    items = { { onSelect = function(selected) end } },
}

---@param options { handlers: table<string, Handler> }
local function register(options) end

register({ handlers = { ping = function(source, payload) end } })

local encoded = json.encode({}, { exception = function(reason, value) end })

local proxy = setmetatable({}, {
    __index = function(_, key) end,
    __newindex = function(_, field, assigned) end,
})

---@type fun(count: integer)
local counter = function(n) end
";
    client.open_with(CLIENT, text);
    let cases = [
        // `@param` lines above the field.
        ("a1)", "a1: number"),
        ("k1)", "k1: boolean"),
        // The `@field` of the class a `---@type` local declares, and of the tables in its fields.
        ("b1)", "b1: number?"),
        ("selected)", "selected: string"),
        // The type of the parameter a table is passed for.
        ("source, payload", "source: integer"),
        ("payload) end", "payload: string"),
        ("reason, value", "reason: string"),
        ("value) end", "value: any"),
        // The metamethods of `setmetatable`'s second argument.
        ("key) end", "key: any"),
        ("assigned)", "assigned: any"),
        // A function a `---@type` local holds.
        ("n) end", "n: integer"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn functions_defined_for_a_declared_field_take_its_parameter_types() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Timer
---@field start fun(self: Test.Timer, async?: boolean)
---@field getTimeLeft fun(self: Test.Timer, format?: 'ms'|'s'): number
---@field onEnd fun(reason: string)
---@field render fun(self: Test.Timer, alpha: number)
---@field stale fun(self: Test.Timer, first: integer)
local Timer = {}

function Timer:start(async) end
function Timer:getTimeLeft(format) return 0 end
Timer.onEnd = function(reason) end
function Timer.render(self, alpha) end
---@param second string
function Timer:stale(first, second) end
function Timer:undeclared(value) end

---@type Test.Timer
local timer = Timer
timer.onEnd = function(why) end
";
    client.open_with(CLIENT, text);
    let cases = [
        // A method declared with `:` takes the `self` of the field itself.
        ("async)", "async: boolean?"),
        ("format)", "format: \"ms\"|\"s\"|nil"),
        ("reason)", "reason: string"),
        ("alpha)", "alpha: number"),
        ("why)", "why: string"),
        // A function with more parameters than the field lists does not implement it.
        ("first, second", "first: unknown"),
        ("second)", "second: string"),
        // Only a `@field` declares a field, not another function set on the class.
        ("value)", "value: unknown"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn functions_in_table_fields_return_what_the_return_above_their_field_says() {
    let mut client = Client::start(fixture_root());
    let text = "\
local handlers = {
    ---@param x integer
    ---@return string
    name = function(x) return x end,
    ---@return integer
    count = function() end,
    nested = {
        ---@return boolean
        ready = function() return true end,
    },
}

Register({
    ---@return string
    label = function() end,
})

return {
    ---@return integer
    size = function() return 'big' end,
}
";
    client.open_with(CLIENT, text);
    let found: Vec<(String, u64)> = findings(&mut client, CLIENT, &["missing-return", "return-type-mismatch"])
        .into_iter()
        .map(|(code, line, _)| (code, line))
        .collect();
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    assert_eq!(
        found,
        [
            ("return-type-mismatch".to_string(), line("name =")),
            ("missing-return".to_string(), line("count =")),
            ("missing-return".to_string(), line("label =")),
            ("return-type-mismatch".to_string(), line("size =")),
        ]
    );

    // A `return` in such a function completes the values its `@return` lists.
    let text = "local modes = {\n    ---@return 'on'|'off'\n    get = function()\n        return ";
    client.change(CLIENT, 2, text);
    let mut labels = client.completion_labels(CLIENT, 3, 15);
    labels.retain(|label| label.starts_with('\''));
    assert_eq!(labels, ["'on'", "'off'"]);
}

#[test]
fn files_see_the_classes_they_declare_as_soon_as_they_are_indexed() {
    let mut client = Client::start(fixture_root());
    // The global table `Test{name}` of the class `Test.{name}`, with methods that return `self`.
    let source = |name: &str, declared: bool| {
        let class = if declared { format!("---@class Test.{name}\n") } else { String::new() };
        format!(
            "{class}Test{name} = {{}}

---@return self
function Test{name}:chain() return self end
function Test{name}:inferred() return self end

---@type Test.{name}
local value = {{}}
local chained = value:chain()
local inferred = value:inferred()
"
        )
    };
    let assert_methods_return = |client: &mut Client, file: &str, text: &str, class: &str| {
        for needle in ["chained", "inferred"] {
            let (l, c) = pos(text, &format!("local {needle}"), 6);
            let hover = client.hover_text(file, l, c);
            assert!(hover.contains(&format!("{needle}: {class}")), "{file}: expected {class} in {hover}");
        }
    };

    // A file that is not on disk is indexed for the first time when it is opened.
    let fresh = "myresource/client/fresh.lua";
    let text = source("Fresh", true);
    client.open_with(fresh, &text);
    assert_methods_return(&mut client, fresh, &text, "Test.Fresh");

    // An edit that declares the class takes effect without another edit.
    let edited = "myresource/client/edited.lua";
    client.open_with(edited, &source("Edited", false));
    client.hover_text(edited, 0, 0);
    let text = source("Edited", true);
    client.change(edited, 2, &text);
    assert_methods_return(&mut client, edited, &text, "Test.Edited");
}

#[test]
fn fields_set_in_several_places_hold_each_value() {
    let mut client = Client::start(fixture_root());
    let text = "\
local Machine = {}
Machine.state = 0
function Machine:start()
    self.state = 'running'
end
Machine.ratio = 1
Machine.ratio = 1.5
Machine.cleared = 1
Machine.cleared = nil
---@type string
Machine.named = nil
Machine.named = 5
Machine.handler = nil
Machine.handler = function() end
Machine.reason = 'closed'
function Machine.close(...) Machine.reason = ... end
Machine.size = 1
Machine.size = 2
Machine.size = 3
Machine.size = 4
Machine.size = 5
Machine.size = 6

---@class Test.Holder
local Holder = {}
Holder.__index = Holder
function Holder.new()
    local self = setmetatable({}, Holder)
    self.item = nil
    return self
end
---@param item Test.Holder
function Holder:set(item)
    self.item = item
end
-- A function runs later, when the fields may hold any of their values.
function Show()
    print(Machine.state, Machine.ratio, Machine.cleared, Machine.named, Machine.handler, Machine.reason, Holder.new().item, Machine.size)
end
";
    client.open_with(CLIENT, text);
    let print = pos(text, "print(", 0).0;
    let line = text.lines().nth(print as usize).unwrap();
    for (needle, expected) in [
        ("Machine.state", "state: integer|string = 0|'running'"),
        ("Machine.size", "size: integer = 1|2|3|4|5|...\n"),
        ("Machine.ratio", "ratio: number = 1|1.5"),
        ("Machine.cleared", "cleared: integer = 1"),
        ("Machine.named", "named: string"),
        ("Machine.handler", "function handler()"),
        ("Machine.reason", "reason: string"),
        ("new().item", "item: Test.Holder"),
    ] {
        let column = (line.find(needle).unwrap() + needle.len() - 1) as u32;
        let hover = client.hover_text(CLIENT, print, column);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn fields_are_set_only_through_the_names_that_own_their_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Thing
---@field name string
local Thing = {}
Thing.version = 2

function Thing:init()
    self.ready = true
end

---@param thing Test.Thing
local function tag(thing)
    thing.extra = 5
end

local Config = { debug = false }
Config.verbose = true
local alias = Config
alias.copied = 1

local rows = { { label = 'a' }, { label = 'b' } }
for _, row in pairs(rows) do
    row.count = 0
end

---@type Test.Thing
local other
print(other.version, other.ready, other.extra, Config.verbose, Config.copied, rows[1].count, tag)
";
    client.open_with(CLIENT, text);
    let print = pos(text, "print(", 0).0;
    let line = text.lines().nth(print as usize).unwrap();
    // The table of the `---@class`, `self` in its methods and a top-level local's own table take
    // the fields set through them.
    for (needle, expected) in [
        ("other.version", "version: integer"),
        ("other.ready", "ready: boolean"),
        ("Config.verbose", "verbose: boolean"),
    ] {
        let column = (line.find(needle).unwrap() + needle.len() - 1) as u32;
        let hover = client.hover_text(CLIENT, print, column);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    // A parameter typed as the class, another local holding the table and a loop variable over
    // the rows hold tables they do not own, so what is set through them is no field of those.
    for needle in ["other.extra", "Config.copied", "rows[1].count"] {
        let column = (line.find(needle).unwrap() + needle.len() - 1) as u32;
        let hover = client.hover_text(CLIENT, print, column);
        assert!(hover.is_empty(), "{needle}: expected no hover, got {hover}");
    }
}

#[test]
fn class_operators_type_the_operations_on_their_values() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Vec<T>
---@operator add(Test.Vec): Test.Vec
---@operator add(number): string
---@operator unm: Test.Vec
---@operator concat(string): Test.Vec
---@operator len: integer
---@operator band(Test.Vec): Test.Vec
---@operator call(integer): boolean

---@class Test.Sub : Test.Vec

---@class Test.Callable
---@overload fun(): string
---@operator call: integer

---@type Test.Vec<integer>
local v
---@type Test.Vec?
local maybe
---@type Test.Sub
local sub
---@type Test.Callable
local callable
local sum = v + v
local scaled = v + 2
local swapped = 2 + v
local missing = v + true
local negated = -v
local joined = v .. 'x'
local count = #v
local masked = v & v
local called = v(1)
local optional = maybe + v
local inherited = sub + sub
local overloaded = callable()
local moved = vector3(1, 2, 3) + 1
";
    client.open_with(CLIENT, text);
    for (name, expected) in [
        ("sum", "Test.Vec"),
        ("scaled", "string"),
        ("swapped", "string"),
        ("missing", "unknown"),
        ("negated", "Test.Vec"),
        ("joined", "Test.Vec"),
        ("count", "integer"),
        ("masked", "Test.Vec"),
        ("called", "boolean"),
        ("optional", "Test.Vec"),
        ("inherited", "number"),
        ("overloaded", "string"),
        ("moved", "vector3"),
    ] {
        let (l, c) = pos(text, &format!("local {name}"), 6);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(&format!("{name}: {expected}")), "{name}: expected {expected:?} in {hover}");
    }
}

#[test]
fn backtick_generics_bind_the_type_a_string_names() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Player
---@field name string

---@generic T
---@param name `T`
---@return T
local function new(name) end

---@generic T
---@param name `T`
---@param fallback? T
---@return T
local function find(name, fallback) end

---@generic T
---@param name `T`
---@return `T`
local function class(name)
    ---@type Test.Player
    local instance = { name = name }
    return instance
end

---@class Test.Car

---@generic T
---@param names `T`[]
---@return T
local function first(names) end

---@generic T
---@param names `T`[]
---@return T[]
local function all(names) end

---@generic T
---@param name `T`
---@return `T`
local function declare(name) end

local player = new('Test.Player')
local count = new('integer')
local missing = new('Test.Missing')
local kind = 'Test.Player'
local widened = new(kind)
local found = find('Test.Player', 1)
local made = class('Test.Player')
local listed = first({ 'Test.Player' })
local either = first({ 'Test.Player', 'Test.Car' })
local players = all({ 'Test.Player' })
local mixed = first({ 'Test.Player', 1 })
local names = { 'Test.Car' }
local held = first(names)
local changing = { 'Test.Car' }
changing = { 'Test.Player' }
local reassigned = first(changing)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("player", "player: Test.Player"),
        ("count", "count: integer"),
        ("missing", "missing: Test.Missing"),
        // A string that is not written in the call may name anything.
        ("widened", "widened: unknown"),
        ("found", "found: Test.Player"),
        // A returned `` `T` `` is the class too, as ox_lib's `lib.class` returns it.
        ("made", "made: Test.Player"),
        // The strings of a table written in the call name it for `` `T`[] ``.
        ("listed", "listed: Test.Player"),
        ("either", "either: Test.Player|Test.Car"),
        ("players", "players: Test.Player[]"),
        ("mixed", "mixed: unknown"),
        // ...and so do those of the table a local that is never assigned again is declared with.
        ("held", "held: Test.Car"),
        ("reassigned", "reassigned: unknown"),
    ] {
        let (l, c) = pos(text, &format!("local {needle}"), 6);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    let (l, c) = pos(text, "new(name) end", 0);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("new(name: `T`): T"), "{hover}");
    let mismatches: Vec<_> =
        client.diagnostics_for(CLIENT).into_iter().filter(|(code, _)| code == "return-type-mismatch").collect();
    assert!(mismatches.is_empty(), "{mismatches:?}");
    // The message writes `` `T` `` once, without wrapping it in more backticks.
    let (line, _) = pos(text, "local function declare", 0);
    let missing = findings(&mut client, CLIENT, &["missing-return"]);
    let declare = missing.iter().find(|(_, l, _)| *l == line as u64).map(|(_, _, message)| message.as_str());
    assert_eq!(declare, Some("The function can reach its end without returning, but `@return` requires `T`"));
}

#[test]
fn generic_classes_bind_their_type_parameters() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.List<T>
---@field first T
---@field items T[]
---@field getter fun(): T
---@field pick fun(self: Test.List<T>, i: integer): T?
---@field [integer] T
---@field next? self
local List = {}

---@return T
function List:get() return self.first end

---@return self
function List:chain() return self end

---@generic U
---@param f fun(value: T): U
---@return Test.List<U>
function List:map(f) end

---@generic T
---@param value T
---@return T
function List:id(value) return value end

---@generic T
---@param kind `T`
---@return Test.List<T>
function List:of(kind) end

function List:inside()
    local own = self.first
end

---@class Test.Strings : Test.List<string>

---@class Test.Pair<L, R>
---@field left L
---@field right R

---@class Test.Swap<A, B> : Test.Pair<B, A>

---@class Test.Mid<U> : Test.List<U[]>

---@class Test.Leaf : Test.Mid<boolean>

---@class Test.Factory<T>
---@overload fun(): T

---@class Test.Tree<T> : Test.Tree<T[]>
---@field node T

---@generic T
---@param from Test.List<T>
---@return T
local function firstOf(from) end

---@type Test.List<string>
local list = {}
---@type Test.Strings
local strings = {}
---@type Test.List<Test.List<integer>>
local nested = {}
---@type Test.List
local bare = {}
---@type Test.Pair<string>
local half = {}
---@type Test.Pair<string, integer, boolean>
local extra = {}
---@type Test.Swap<string, integer>
local swap = {}
---@type Test.Leaf
local leaf = {}
---@type Test.Factory<integer>
local factory = nil
---@type Test.Tree<integer>
local tree = {}

local first = list.first
local items = list.items
local got = list.getter()
local picked = list:pick(1)
local indexed = list[1]
local returned = list:get()
local following = list.next
local chained = list:chain().first
local mapped = list:map(function(value) return 1 end)
local same = list:id(1)
local fromTable = List:id(1)
local ofKind = List:of('integer').first
local inherited = strings:get()
local inner = nested.first.first
local unbound = bare.first
local bareSame = bare:id(1)
local missing = half.right
local ignored = extra.right
local swapped = swap.left
local deep = leaf.first
local made = factory()
local bound = firstOf(list)
local node = tree.node
for _, each in ipairs(list.items) do print(each) end
list.first = 2
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("local list", "list: Test.List<string> {"),
        ("local first", "first: string"),
        ("local items", "items: string[]"),
        ("local got", "got: string"),
        ("local picked", "picked: string?"),
        ("local indexed", "indexed: string"),
        ("local returned", "returned: string"),
        // `self` keeps the type arguments of the value it is read from.
        ("local following", "following: Test.List<string>? {"),
        ("local chained", "chained: string"),
        ("local mapped", "mapped: Test.List<integer> {"),
        ("value) return 1", "value: string"),
        // A method's own `@generic T` is the `T` of the class, as lua-language-server binds it.
        ("local same", "same: string"),
        // On the class table the method binds its own `T`, from the value or the class it names.
        ("local fromTable", "fromTable: integer"),
        ("local ofKind", "ofKind: integer"),
        ("local inherited", "inherited: string"),
        ("local inner", "inner: integer"),
        // Without type arguments the parameters stay themselves, as lua-language-server shows them,
        // and as the class table and `self` in its methods keep them: `list.first = 2` gives them
        // no type, and a method's own `@generic T` binds from the call.
        ("local unbound", "unbound: T"),
        ("local bareSame", "bareSame: integer"),
        ("local own", "own: T"),
        ("local List", "List: Test.List<T> {"),
        ("local missing", "missing: R"),
        ("local ignored", "ignored: integer"),
        ("local swapped", "swapped: integer"),
        ("local deep", "deep: boolean[]"),
        ("local made", "made: integer"),
        ("local bound", "bound: string"),
        // A class that names itself as a parent with other type arguments is read once.
        ("local node", "node: integer\n"),
        ("each in", "each: string"),
    ] {
        let delta = if needle.starts_with("local ") { 6 } else { 0 };
        let (l, c) = pos(text, needle, delta);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    let (l, c) = pos(text, "Test.Swap<string", 1);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("(class) Test.Swap<A, B> : Test.Pair<B, A>"), "{hover}");
    // A method is named by its class alone, as lua-language-server names it.
    for (needle, expected) in
        [("list:get", "function Test.List:get(): string"), ("List:get", "function Test.List:get(): T")]
    {
        let (l, c) = pos(text, needle, 5);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }

    let completed = format!("{text}local _ = list.");
    client.change(CLIENT, 2, &completed);
    let line = completed.matches('\n').count() as u32;
    let result = client.request("textDocument/completion", client.position_params(CLIENT, line, 15));
    let items = result["items"].as_array().cloned().unwrap_or_default();
    let detail = |label: &str| {
        let item = items.iter().find(|item| item["label"] == label);
        item.and_then(|item| item["detail"].as_str()).unwrap_or_default().to_string()
    };
    assert_eq!(detail("first"), "string", "{result}");
    assert_eq!(detail("items"), "string[]", "{result}");
}

#[test]
fn generic_classes_bind_the_type_parameters_their_side_declares() {
    let mut client = Client::start(fixture_root());
    client.open_with(
        "myresource/shared/config.lua",
        "\
---@class (server) Test.SidedBox<T>
---@field held T
---@field [1] T
---@overload fun(): T

---@class (client) Test.SidedBox<U>
---@field held U
---@field [1] U
---@overload fun(): U
",
    );
    let text = "\
---@type Test.SidedBox<string>
local box = nil
local held = box.held
local first = box[1]
local made = box()
for _, each in pairs(box) do print(each) end
";
    for file in [CLIENT, SERVER] {
        client.open_with(file, text);
        for (needle, expected) in [
            ("local held", "held: string"),
            ("local first", "first: string"),
            ("local made", "made: string"),
            ("each)", "each: string"),
        ] {
            let delta = if needle.starts_with("local ") { 6 } else { 0 };
            let (l, c) = pos(text, needle, delta);
            let hover = client.hover_text(file, l, c);
            assert!(hover.contains(expected), "{file} {needle}: expected {expected:?} in {hover}");
        }
    }
}

#[test]
fn generic_classes_check_fields_with_their_type_arguments() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Holder<T>
---@field value T
---@field label string

---@class Test.Pair<L, R>
---@field left L
---@field right R

---@type Test.Holder<string>
local holder = { value = 1, label = 'a' }
holder.value = 2

---@type Test.Pair<string>
local half = { left = 'a' }

---@type Test.Holder
local bare = { label = 'a' }

---@return integer
local function count() return holder.value end

if holder.value == 1 then end
print(count, half, bare)
";
    client.open_with(CLIENT, text);
    let codes = ["assign-type-mismatch", "missing-fields", "return-type-mismatch", "impossible-comparison"];
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    let finding = |code: &str, needle: &str, message: &str| (code.to_string(), line(needle), message.to_string());
    assert_eq!(
        findings(&mut client, CLIENT, &codes),
        [
            finding("assign-type-mismatch", "value = 1", "Cannot assign `integer` to field `value` of type `string`"),
            finding(
                "assign-type-mismatch",
                "holder.value = 2",
                "Cannot assign `integer` to field `value` of type `string`"
            ),
            finding("missing-fields", "half = {", "Missing required fields in type `Test.Pair`: `right`"),
            finding("missing-fields", "bare = {", "Missing required fields in type `Test.Holder`: `value`"),
            finding(
                "return-type-mismatch",
                "return holder",
                "Cannot return `string` as return value #1 of type `integer`"
            ),
            finding("impossible-comparison", "if holder", "Comparing `string` with `1` is always false"),
        ],
        "`right` of `Test.Pair<string>` and `value` of a `Test.Holder` without arguments take any value, but are required"
    );
}

#[test]
fn generic_aliases_bind_their_type_parameters() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Test.Box<T> { value: T }
---@alias Test.Result<T> T|nil

---@generic T
---@param result Test.Result<T>
---@return T
local function unwrap(result) end

---@param target number|Test.Box<integer>
local function send(target) return target end

---@type Test.Box<integer>
local box = {}
---@type Test.Box
local anyBox = {}
---@type Test.Result<string>
local result = nil

local boxed = box.value
local unboxed = anyBox.value
local unwrapped = unwrap('x')
print(result, send)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("local boxed", "boxed: integer"),
        // Without type arguments the parameters of an alias are unknown, as lua-language-server
        // reads them.
        ("local unboxed", "unboxed: unknown"),
        // A generic function binds `T` through the alias of its parameter.
        ("local unwrapped", "unwrapped: string"),
        // A value of several types keeps them while the fields of the table among them follow.
        (" target end", "target: number|Test.Box<integer> {\n    value: integer,"),
        ("local result", "type Test.Result<T> = T?"),
        ("Test.Box<integer>", "type Test.Box<T> = { value: T }"),
    ] {
        let delta = if needle.starts_with("local ") { 6 } else { 1 };
        let (l, c) = pos(text, needle, delta);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn table_types_named_as_parents_pass_their_fields_and_indices_on() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Array<T> : { [number]: T }

---@alias Test.ArrayLike<T> Test.Array | { [number]: T }

---@class Test.Dict<K, V> : table<K, V>

---@class Test.Flags : table<string, boolean>

---@class Test.Point : { x: number, y?: number }

---@class (strict) Test.Named : table<string, any>

---@class (strict) Test.Open : table

---@class Test.Indexed<T>
---@field [number] T

---@type Test.Array<string>
local array = {}
---@type Test.Array
local anyArray = {}
---@type Test.Indexed
local anyIndexed = {}
---@type Test.ArrayLike<integer>
local arrayLike = {}
---@type Test.Dict<string, integer>
local dict = {}
---@type Test.Flags
local flags = {}
---@type Test.Point
local point = {}
---@type Test.Named
local named = { anything = 1, [1] = 2 }
---@type Test.Open
local open = { anything = 1, [1] = 2 }

local element = array[1]
local anyElement = anyArray[1]
local anyIndexedElement = anyIndexed[1]
local likeElement = arrayLike[1]
local counted = dict.anything
local flagged = flags.anything
local x = point.x
local y = point.y
for _, item in ipairs(array) do print(item) end
for key, count in pairs(dict) do print(key, count) end
print(named, open)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("local element", "element: string"),
        // Without type arguments a table type named as a parent holds unknown values, as
        // lua-language-server reads it, while an index of the class keeps its parameter. So the
        // `Test.Array` of ox_lib's `ArrayLike` adds nothing to the `integer` of its other part.
        ("local anyElement", "anyElement: unknown"),
        ("local anyIndexedElement", "anyIndexedElement: T"),
        ("local likeElement", "likeElement: integer\n"),
        ("local counted", "counted: integer"),
        ("local flagged", "flagged: boolean"),
        ("local x", "x: number"),
        ("local y", "y: number?"),
        ("item) end", "item: string"),
        ("key, count", "key: string"),
        ("count) end", "count: integer"),
        ("Test.Array<string>", "(class) Test.Array<T> : { [number]: T }"),
    ] {
        let delta = if needle.starts_with("local ") { 6 } else { 1 };
        let (l, c) = pos(text, needle, delta);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    // `table<string, any>` takes string keys only, while a plain `table` takes any.
    let line = pos(text, "[1] = 2", 0).0 as u64;
    let message = "Field `[1]` is not declared in strict class `Test.Named`".to_string();
    assert_eq!(findings(&mut client, CLIENT, &["undeclared-field"]), [("undeclared-field".to_string(), line, message)]);
}

#[test]
fn self_in_doc_types_is_the_class_of_the_table_a_function_is_defined_on() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Builder
---@field parent? self
local Builder = {}

---@return self
function Builder:chain() return self end

---@param other self
---@return self
function Builder:merge(other)
    local merged = other
    return merged
end

---@return self
function Builder.new() return setmetatable({}, { __index = Builder }) end

---@return self
Builder.copy = function() return Builder.new() end

---@return self
function Builder:broken() return 5 end

---@class Test.Child : Test.Builder

---@param value self
---@return self
local function free(value) return value end

---@type Test.Builder
local built = Builder.new()
---@type Test.Child
local child = {}
local chained = built:chain()
local inherited = child:chain()
local made = Builder.new()
local copied = Builder.copy()
local parent = built.parent
local loose = free(1)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("chained", "chained: Test.Builder"),
        // As in lua-language-server, `self` is the class the method is defined on, not the receiver.
        ("inherited", "inherited: Test.Builder"),
        ("made", "made: Test.Builder"),
        ("copied", "copied: Test.Builder"),
        ("parent =", "parent: Test.Builder?"),
        ("merged =", "merged: Test.Builder"),
        // A function that belongs to no table has no class for `self`.
        ("loose", "loose: unknown"),
    ] {
        let (l, c) = pos(text, &format!("local {needle}"), 6);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    let (l, c) = pos(text, "merge(other)", 0);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("merge(other: Test.Builder): Test.Builder"), "{hover}");
    let mismatch = "Cannot return `integer` as return value #1 of type `Test.Builder`";
    assert_eq!(
        findings(&mut client, CLIENT, &["return-type-mismatch", "missing-return", "undefined-doc-name"]),
        [("return-type-mismatch".to_string(), pos(text, "return 5", 0).0 as u64, mismatch.to_string())]
    );
}

#[test]
fn async_function_types_are_function_types() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@type async fun(x: integer): string
local fetch

---@param job async fun(): integer
local function run(job)
    local done = job()
    return done
end

local fetched = fetch(1)
local ran = run(function() return 1 end)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [("fetched", "fetched: string"), ("done", "done: integer"), ("ran", "ran: integer")] {
        let (l, c) = pos(text, &format!("local {needle}"), 6);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    assert!(findings(&mut client, CLIENT, &["undefined-doc-name"]).is_empty());

    // Hover keeps the `async`, as lua-language-server shows it.
    let (l, c) = pos(text, "fetch(1)", 0);
    let fetch = client.hover_text(CLIENT, l, c);
    assert!(fetch.contains("(async) local function fetch(x: integer): string"), "{fetch}");
    let (l, c) = pos(text, "run(job)", 0);
    let run = client.hover_text(CLIENT, l, c);
    assert!(run.contains("local function run(job: async fun(): integer)"), "{run}");
}

#[test]
fn self_in_the_type_of_a_local_is_unknown() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@type fun(): self
local make
local made = make()

---@class Test.Maker
local Maker = {}

function Maker:build()
    ---@type self
    local me
    return me
end
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("make\n", "local function make(): unknown"),
        ("made", "local made: unknown"),
        // A local in a method is no member of the class either, as lua-language-server reads it.
        ("me\n", "local me: unknown"),
    ] {
        let (l, c) = pos(text, &format!("local {needle}"), 6);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn functions_tagged_async_show_it_in_hover() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@async
---@param ms integer
local function sleep(ms) end
sleep(1)
";
    client.open_with(CLIENT, text);
    let (l, c) = pos(text, "sleep(1)", 0);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("(async) local function sleep(ms: integer)"), "{hover}");
}

#[test]
fn tables_with_a_metatable_have_the_members_of_its_index() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-metatables-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    let write = |relative: &str, text: &str| {
        let path = fixture.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        "fxmanifest.lua",
        "fx_version 'cerulean'\ngame 'gta5'\nshared_script 'shared/*.lua'\nclient_scripts { 'client/*.lua' }\n",
    );
    write(
        "shared/vehicle.lua",
        "\
Vehicle = {}
Vehicle.__index = Vehicle

function Vehicle.new(plate)
    local self = setmetatable({}, Vehicle)
    self.plate = plate
    self.speed = 0
    return self
end
",
    );
    // A method another file adds to the global class table.
    write("client/honk.lua", "function Vehicle:honk() end\n");
    // Global classes whose constructors set `self.__index = self`, one inheriting from the other.
    write(
        "client/zones.lua",
        "\
Zone = {}

function Zone:new()
    local zone = { size = 1 }
    setmetatable(zone, self)
    self.__index = self
    return zone
end

function Zone:contains()
    return true
end

function Zone:resize(size)
    self.size = size
end

BoxZone = {}
setmetatable(BoxZone, { __index = Zone })

function BoxZone:new()
    local zone = Zone:new()
    zone.width = 2
    setmetatable(zone, self)
    self.__index = self
    return zone
end

function BoxZone:corners() end
",
    );
    let text = "\
local Base = {}
Base.__index = Base

function Base.new()
    return setmetatable({}, Base)
end

function Base:greet() end

local Child = setmetatable({}, { __index = Base })
Child.__index = Child

function Child.new()
    local self = setmetatable({}, Child)
    self.level = 1
    return self
end

function Child:wave() end

local Other = {}
Other.__index = Other
setmetatable(Other, Base)

local Animal = {}

function Animal:new(o)
    o = o or {}
    setmetatable(o, self)
    self.__index = self
    return o
end

function Animal:speak() end

local function make()
    local made = {}
    setmetatable(made, Animal)
    return made
end

---@class Point
---@field x number
local Point = {}
Point.__index = Point

function Point.new()
    return setmetatable({}, Point)
end

---@class Bar
---@field name string
local Bar = setmetatable({}, { __index = Base })

---@class Thing
---@field name string

local Holder = {}
Holder.__index = Holder

function Holder.new()
    local self = setmetatable({}, Holder)
    self.item = nil
    return self
end

---@param thing Thing
function Holder:set(thing)
    self.item = thing
end

local Looped = {}
Looped.__index = Looped
setmetatable(Looped, Looped)

function Looped:loop() end

local Left = {}
local Right = {}
setmetatable(Left, { __index = Right })
setmetatable(Right, { __index = Left })

function Left:left() end
function Right:right() end

local Late = {}
local late = setmetatable({}, Late)
Late.__index = Late

function Late:later() end

local base = Base.new()
local child = Child.new()
local other = setmetatable({}, Other)
local dog = Animal:new()
local built = make()
local point = Point.new()
local bar = Bar
local vehicle = Vehicle.new('abc')
local literal = setmetatable({}, { __index = { hello = 1 } })
local proxy = setmetatable({}, { __index = function(_, key) return key end })
local hello = literal.hello
local level = child.level
local missing = proxy.anything
local zone = Zone:new()
local zoneSize = zone.size
local box = BoxZone:new()
local inside = box:contains()
local childClass = Child
local vehicleClass = Vehicle
local animal = Animal
local held = Holder.new().item
---@type Thing
local typed = Holder.new().item
local looped = setmetatable({}, Looped)
local loopedMissing = looped.nothing
local fromRight = Left.right
local fromLeft = Right.left
local neither = Left.neither
function dog:bark() end
base:greet()
child:greet()
child:wave()
vehicle:honk()
print(other, built, point, bar, hello, level, missing, zone, zoneSize, inside, childClass, vehicleClass, animal)
print(held, typed, late, looped, loopedMissing, fromRight, fromLeft, neither)
";
    write("client/main.lua", text);
    let main = "client/main.lua";
    let mut client = Client::start(fixture.0.clone());
    client.open_with(main, text);

    let cases: [(&str, &[&str], &[&str]); 24] = [
        ("local base =", &["greet: function", "new: function"], &[]),
        // Each level of `__index`: `Child`'s own methods, and `Base`'s through its metatable.
        ("local child =", &["wave: function", "greet: function", "level: integer"], &[]),
        // `setmetatable(Other, Base)` gives `Other` the methods of `Base`.
        ("local other =", &["greet: function"], &[]),
        // `setmetatable(o, self)` inside `Animal:new`, and on a local before it is returned. A method
        // defined through a local that holds an instance made elsewhere adds nothing, to the
        // instances or the class, as TypeScript reads `dog.bark = ...`.
        ("local dog =", &["speak: function"], &["bark"]),
        ("local built =", &["speak: function"], &[]),
        // An instance of a `---@class` is that class, and the class keeps its own fields.
        ("local point =", &["local point: Point"], &[]),
        ("local bar =", &["local bar: Bar", "name: string"], &["greet"]),
        ("local vehicle =", &["honk: function", "speed: integer"], &[]),
        ("local hello =", &["local hello: integer"], &[]),
        ("local level =", &["local level: integer"], &[]),
        // An `__index` function decides the fields at runtime.
        ("local missing =", &["local missing: unknown"], &[]),
        // `self.__index = self` in the constructor of a global class, and in its subclass's. Making
        // an instance of `Zone` one of `BoxZone` leaves `Zone` as it is, also for the fields set on
        // the instance before. The `size` the instance holds shows over the one `Zone:resize` sets.
        ("local zone =", &["contains: function", "size: integer"], &["corners", "width", "size: unknown"]),
        ("local box =", &["corners: function", "contains: function", "width: integer"], &[]),
        ("local inside =", &["local inside: boolean"], &[]),
        // The fields a constructor sets on the new table belong to the instances, not to the class.
        ("local childClass =", &["wave: function", "greet: function"], &["level"]),
        ("local vehicleClass =", &["honk: function"], &["plate", "speed"]),
        ("local animal =", &["speak: function"], &["bark"]),
        // A method sets the field of an instance through `self`, so the `nil` the constructor sets
        // first does not hide it.
        ("local held =", &["local held: Thing"], &[]),
        // A metatable that is its own `__index`, and two tables falling back on each other.
        ("local looped =", &["loop: function"], &[]),
        ("local loopedMissing =", &["local loopedMissing: unknown"], &[]),
        ("local fromRight =", &["local function fromRight"], &[]),
        ("local fromLeft =", &["local function fromLeft"], &[]),
        ("local neither =", &["local neither: unknown"], &[]),
        // An `__index` set after the call.
        ("local late =", &["later: function"], &[]),
    ];
    for (needle, expected, absent) in cases {
        let (l, c) = pos(text, needle, 6);
        let hover = client.hover_text(main, l, c);
        for expected in expected {
            assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
        }
        for absent in absent {
            assert!(!hover.contains(absent), "{needle}: unexpected {absent:?} in {hover}");
        }
    }
    // A field of an instance is named as one of its class, not of the table it was made from.
    let (l, c) = pos(text, "zone.size", 5);
    let hover = client.hover_text(main, l, c);
    assert!(hover.contains("(field) Zone.size: integer"), "{hover}");
    let mismatches: Vec<(String, u64)> =
        client.diagnostics_for(main).into_iter().filter(|(code, _)| code == "assign-type-mismatch").collect();
    assert!(mismatches.is_empty(), "`typed` takes the `Thing` that `Holder:set` stores: {mismatches:?}");

    let (l, c) = pos(text, "child:wave", 6);
    let labels = client.completion_labels(main, l, c);
    for method in ["new", "wave", "greet"] {
        assert!(labels.iter().any(|label| label == method), "{method} in {labels:?}");
    }

    // Where a location is, as (file, line).
    let places = |result: &Value| -> Vec<(String, u64)> {
        let mut places: Vec<(String, u64)> = result
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|location| {
                let uri = location["uri"].as_str().unwrap();
                (uri.rsplit('/').next().unwrap().to_string(), location["range"]["start"]["line"].as_u64().unwrap())
            })
            .collect();
        places.sort();
        places
    };
    let line = |needle: &str| u64::from(pos(text, needle, 0).0);
    let (l, c) = pos(text, "child:greet", 6);
    let definition = client.request("textDocument/definition", client.position_params(main, l, c));
    assert_eq!(places(&definition), [("main.lua".to_string(), line("function Base:greet"))]);
    let (l, c) = pos(text, "vehicle:honk", 8);
    let definition = client.request("textDocument/definition", client.position_params(main, l, c));
    assert_eq!(places(&definition), [("honk.lua".to_string(), 0)]);
    let (l, c) = pos(text, "child.level", 6);
    let definition = client.request("textDocument/definition", client.position_params(main, l, c));
    assert_eq!(places(&definition), [("main.lua".to_string(), line("self.level = 1"))]);

    let greets =
        ["function Base:greet", "base:greet", "child:greet"].map(|needle| ("main.lua".to_string(), line(needle)));
    let (l, c) = pos(text, "Base:greet", 5);
    let mut params = client.position_params(main, l, c);
    params["context"] = json!({ "includeDeclaration": true });
    assert_eq!(places(&client.request("textDocument/references", params)), greets);
    let mut params = client.position_params(main, l, c);
    params["newName"] = json!("hello");
    let edit = client.request("textDocument/rename", params);
    let edits = edit["changes"][client.uri(main).as_str()].as_array().cloned().unwrap_or_default();
    let mut renamed: Vec<u64> = edits.iter().map(|e| e["range"]["start"]["line"].as_u64().unwrap()).collect();
    renamed.sort();
    assert_eq!(renamed, greets.map(|(_, line)| line), "{edit}");
}

#[test]
fn tables_become_values_of_a_class_metatable_at_the_call() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class (exact) Meta.Foo
---@field a number
local Foo = {}
Foo.__index = Foo

function Foo:get() return self.a end

local function fromFields()
    local self = { a = 1, b = 2 }
    self.extra = 3
    return setmetatable(self, Foo)
end

local function filledFirst()
    local o = {}
    o.a = 'one'
    o.tmp = true
    setmetatable(o, Foo)
    o.a = 2
    return o
end

local function readFirst()
    local p = { b = 1 }
    local early = p.b
    return setmetatable(p, Foo), early
end

---@class (exact) Meta.Indexed
---@field x number
local Indexed = {}

local function viaIndex()
    local self = setmetatable({}, { __index = Indexed })
    self.y = 1
    local t = { w = 1 }
    setmetatable(t, { __index = Indexed })
    t.q = 2
    return self, t
end

local made = fromFields()
local filled = filledFirst()
local built, early = readFirst()
local literal = setmetatable({ c = 1 }, Foo)
local plain = setmetatable({}, Foo)
local fromMade = made.b
---@param foo Meta.Foo
local function use(foo) return foo.a end
print(viaIndex, made, filled, built, early, literal, plain, fromMade, use)
";
    client.open_with(CLIENT, text);
    let line = |needle: &str| pos(text, needle, 0).0 as u64;
    // What is set on a table before `setmetatable` gives it a class is no field of the class, and a
    // table built with fields of its own keeps them, so it is no value of the class alone. An empty
    // table is one from the call on.
    assert_eq!(
        undeclared_fields(&mut client, CLIENT),
        [(line("self.y = 1"), "Field `y` is not declared in strict class `Meta.Indexed`".to_string())]
    );
    let mismatches: Vec<(String, u64)> =
        client.diagnostics_for(CLIENT).into_iter().filter(|(code, _)| code == "assign-type-mismatch").collect();
    assert!(mismatches.is_empty(), "{mismatches:?}");

    let cases: [(&str, u32, &[&str]); 7] = [
        ("local made =", 6, &["b: integer", "get: function"]),
        ("local early = p.b", 6, &["local early: integer"]),
        ("local literal =", 6, &["c: integer", "get: function"]),
        ("local plain =", 6, &["local plain: Meta.Foo"]),
        ("local fromMade =", 6, &["local fromMade: integer"]),
        ("made.b", 5, &["(field) Meta.Foo.b: integer"]),
        ("---@param foo Meta.Foo", 14, &["(class) Meta.Foo", "a: number"]),
    ];
    for (needle, delta, expected) in cases {
        let (l, c) = pos(text, needle, delta);
        let hover = client.hover_text(CLIENT, l, c);
        for expected in expected {
            assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
        }
    }
    let (l, c) = pos(text, "---@param foo Meta.Foo", 14);
    let class = client.hover_text(CLIENT, l, c);
    for absent in ["extra", "tmp", "b: integer", "a: string"] {
        assert!(!class.contains(absent), "the class takes nothing set before the call: {absent:?} in {class}");
    }
}

#[test]
fn deep_metatable_chains_leave_shallower_lookups_whole() {
    // Each table falls back on the one before, deeper than a lookup follows.
    let mut chain = String::from("local C1 = {}\nC1.v = 1\n");
    for i in 2..=40 {
        chain.push_str(&format!("local C{i} = setmetatable({{}}, {{ __index = C{} }})\n", i - 1));
    }
    let reads: String = (1..40).map(|i| format!("---@type string\nlocal s{i} = C{i}.v\n")).collect();
    let mut client = Client::start(fixture_root());
    client.open_with(CLIENT, &format!("{chain}{reads}"));
    // Reading the deepest table first stops at the depth limit, which leaves the tables it passed
    // whole for the reads after it.
    client.open_with(SERVER, &format!("{chain}---@type string\nlocal deep = C40.v\n{reads}"));
    let mut mismatched = |file: &str| -> Vec<String> {
        client.diagnostics_for(file);
        let uri = client.uri(file).to_string();
        client.diagnostics[&uri]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["code"] == "assign-type-mismatch")
            .map(|d| d["message"].as_str().unwrap().to_string())
            .filter(|message| !message.contains("`deep`"))
            .collect()
    };
    let resolved = mismatched(CLIENT);
    assert!(resolved.len() >= 10, "{resolved:?}");
    assert_eq!(mismatched(SERVER), resolved);
}

#[test]
fn hover_expands_aliases_and_lists_members_only_for_tables() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Test.Name string
---@param kind GarageKind
---@param name Test.Name
---@param garage Garage|string
---@param kinds GarageKind[]
---@param owned Garage
local function describe(kind, name, garage, kinds, owned)
    for _, each in ipairs(kinds) do end
    print(owned.kind)
end
";
    client.open_with(CLIENT, text);
    let garage_kind = "type GarageKind = \"public\"|\"job\"|\"gang\"";
    let cases: &[(&str, u32, &[&str])] = &[
        ("(kind", 1, &["kind: GarageKind", garage_kind]),
        ("name, garage", 0, &["name: Test.Name", "type Test.Name = string"]),
        ("garage, kinds", 0, &["point: GaragePoint"]),
        ("each", 0, &["each: GarageKind", garage_kind]),
        ("owned.kind", 6, &["Garage.kind: GarageKind", garage_kind]),
    ];
    for &(needle, delta, expected) in cases {
        let (l, c) = pos(text, needle, delta);
        let hover = client.hover_text(CLIENT, l, c);
        for part in expected {
            assert!(hover.contains(part), "{needle}: expected {part:?} in {hover}");
        }
        assert!(!hover.contains("byte"), "{needle}: lists the string library in {hover}");
    }
}

#[test]
fn hovers_and_signature_help_write_out_the_aliases_their_types_use() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Test.Mode \"a\"|\"b\"
---@alias Test.Pair { left: number, right: number }
---@alias Test.Wrapped<T> { value: T }
---@alias Test.Opts { mode: Test.Mode, size: integer }

---@class Test.Car
---@field mode Test.Mode
---@field speed number

---@param m Test.Mode
---@param p Test.Pair
---@return Test.Mode?, Test.Car
local function useMode(m, p) end

---@type Test.Car
local car = { mode = 'a', speed = 1 }
---@type Test.Wrapped<Test.Mode>
local wrapped = { value = 'a' }
---@type Test.Opts
local opts = { mode = 'a', size = 1 }
useMode('a', { left = 1, right = 2 })
print(car, wrapped, opts)
";
    client.open_with(CLIENT, text);
    let mode = "type Test.Mode = \"a\"|\"b\"";
    let pair = "type Test.Pair = { left: number, right: number }";
    let cases: &[(&str, &[&str])] = &[
        ("useMode(m", &["useMode(m: Test.Mode, p: Test.Pair): Test.Mode?, Test.Car", mode, pair]),
        ("car = {", &["mode: Test.Mode,", mode]),
        ("wrapped = {", &["Test.Wrapped<Test.Mode>", mode]),
        ("Test.Opts\nlocal", &["type Test.Opts = { mode: Test.Mode, size: integer }", mode]),
        ("Test.Car\nlocal car", &["(class) Test.Car", mode]),
    ];
    for &(needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        for part in expected {
            assert!(hover.contains(part), "{needle}: expected {part:?} in {hover}");
        }
    }

    let (l, c) = pos(text, "useMode('a'", 9);
    let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
    let parameters = &result["signatures"][0]["parameters"];
    let doc = |i: usize| parameters[i]["documentation"]["value"].as_str().unwrap_or_default().to_string();
    assert!(doc(0).contains(mode), "{}", doc(0));
    assert!(doc(1).contains(pair) && !doc(1).contains(mode), "{}", doc(1));
}

#[test]
fn hover_overviews_write_out_the_table_types_fields_declare() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Variation
---@field drawable number

---@class Test.Clothes
---@field components { [string]: Test.Variation }
---@field hidden boolean

---@type Test.Clothes
local clothes = { components = {}, hidden = false }
local nested = { inner = { value = 1 } }
print(clothes, nested)
";
    client.open_with(CLIENT, text);
    let (l, c) = pos(text, "print(clothes", 6);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("components: { [string]: Test.Variation },"), "{hover}");
    let (l, c) = pos(text, "nested)", 0);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(hover.contains("inner: table,"), "a table the index holds stays `table`: {hover}");
}

#[test]
fn hover_levels_write_out_the_classes_fields_use() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Test.Variation
---@field drawable number
---@field texture number

---@alias Test.HideMethod \"none\"|\"resetFlag\"

---@class Test.Clothes
---@field components { [string]: Test.Variation }
---@field hideHead Test.HideMethod
---@field coords vector3

---@type Test.Clothes
local clothes = { components = {}, hideHead = 'none', coords = vector3(0, 0, 0) }
print(clothes)
";
    client.open_with(CLIENT, text);
    let (l, c) = pos(text, "print(clothes", 6);
    let alias = "type Test.HideMethod = \"none\"|\"resetFlag\"";
    let variation = "(class) Test.Variation {\n    drawable: number,\n    texture: number,\n}";

    let (compact, max) = client.hover_at_level(CLIENT, l, c, Some(0));
    assert_eq!(compact, "```lua\nlocal clothes: Test.Clothes\n```");
    assert_eq!(max, 1);

    let (normal, max) = client.hover_at_level(CLIENT, l, c, Some(1));
    assert!(normal.contains("components: { [string]: Test.Variation },") && normal.contains(alias), "{normal}");
    assert!(!normal.contains("(class)"), "{normal}");
    assert_eq!(max, 2);
    assert_eq!(client.hover_at_level(CLIENT, l, c, None), (normal.clone(), 2), "level 1 is the default");

    let (detailed, max) = client.hover_at_level(CLIENT, l, c, Some(2));
    assert!(detailed.starts_with(&normal[..normal.len() - 4]), "{detailed}");
    assert!(detailed.contains(&format!("{alias}\n{variation}\n```")), "{detailed}");
    assert!(!detailed.contains("(class) vector3"), "classes of the built-in library are left out: {detailed}");
    assert_eq!(max, 2);
    assert_eq!(client.hover_at_level(CLIENT, l, c, Some(4)), (detailed, 2), "nothing more to write out");
}

#[test]
fn hover_levels_list_more_fields_and_write_out_tables_and_functions() {
    let mut client = Client::start(fixture_root());
    let fields: String = (1..=20).map(|n| format!("---@field f{n} number\n")).collect();
    let text = format!(
        "\
---@class Test.Wide
{fields}
---@type Test.Wide
local wide = {{}}
local nested = {{ inner = {{ value = 1, deeper = {{ flag = true }} }}, run = function(a) return a end }}
print(wide, nested)
"
    );
    client.open_with(CLIENT, &text);
    let (l, c) = pos(&text, "print(wide", 6);
    let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(1));
    assert!(hover.contains("f14: number,\n    ...(+6)\n}"), "{hover}");
    assert_eq!(max, 2);
    let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(2));
    assert!(hover.contains("f20: number,\n}") && !hover.contains("...("), "{hover}");
    assert_eq!(max, 2);

    let (l, c) = pos(&text, "nested)", 0);
    let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(1));
    assert!(hover.contains("inner: table,") && hover.contains("run: function,"), "{hover}");
    assert_eq!(max, 2);
    let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(2));
    let inner = "inner: {\n        value: integer = 1,\n        deeper: table,\n    },";
    assert!(hover.contains(inner) && hover.contains("run: fun(a"), "{hover}");
    assert_eq!(max, 3);
    let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(3));
    assert!(hover.contains("deeper: {\n            flag: boolean = true,\n        },"), "{hover}");
    assert_eq!(max, 3);
}

#[test]
fn hover_levels_write_out_tables_that_assignments_of_two_tables_merge() {
    let mut client = Client::start(fixture_root());
    let text = "\
local defaults = {}
defaults.Battery = {}
defaults.Battery.Enabled = false
TestPhoneConfig = {}
TestPhoneConfig.Battery = {}
TestPhoneConfig.Battery.Enabled = true
if not TestPhoneConfig.Battery then
    TestPhoneConfig = defaults
end
print(TestPhoneConfig)
";
    client.open_with(CLIENT, text);
    let (l, c) = pos(text, "print(TestPhoneConfig", 6);
    let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(1));
    assert!(hover.contains("Battery: table,"), "{hover}");
    assert_eq!(max, 2);
    let (hover, _) = client.hover_at_level(CLIENT, l, c, Some(2));
    assert!(hover.contains("Battery: {\n        Enabled: boolean = true,\n    },"), "{hover}");
}

#[test]
fn hover_levels_of_functions_types_and_plain_values() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Test.Mode \"fast\"|\"slow\"

---@class Test.Car
---@field speed number

---Drives the car.
---@param car Test.Car
---@param mode Test.Mode
local function drive(car, mode) end
local count = 1
drive(nil, 'fast')
print(count, GetPlayerPed(-1))
";
    client.open_with(CLIENT, text);
    let mode = "type Test.Mode = \"fast\"|\"slow\"";
    let car = "(class) Test.Car {\n    speed: number,\n}";
    let (l, c) = pos(text, "drive(nil", 0);
    let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(0));
    assert!(hover.contains("local function drive(car: Test.Car, mode: Test.Mode)\n```"), "{hover}");
    assert!(hover.contains("Drives the car."), "level 0 keeps the description: {hover}");
    assert_eq!(max, 1);
    let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(1));
    assert!(hover.contains(mode) && !hover.contains(car), "{hover}");
    assert_eq!(max, 2);
    let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(2));
    assert!(hover.contains(&format!("{mode}\n{car}")), "{hover}");
    assert_eq!(max, 2);

    let (l, c) = pos(text, "---@param car Test.Car", 18);
    let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(0));
    assert_eq!((hover.lines().nth(1), max), (Some("(class) Test.Car"), 1), "{hover}");

    for (needle, offset) in [("count, Get", 0), ("GetPlayerPed(-1)", 0)] {
        let (l, c) = pos(text, needle, offset);
        for level in [0, 1, 3] {
            let (hover, max) = client.hover_at_level(CLIENT, l, c, Some(level));
            assert_eq!(max, 0, "{needle} at level {level}: {hover}");
        }
    }
}

#[test]
fn hover_level_defaults_to_the_verbosity_setting() {
    let options = json!({ "hover": { "verbosity": 0 } });
    let mut client = Client::start_with_options(fixture_root(), json!({}), options);
    let text = "\
---@class Test.Point
---@field x number

---@type Test.Point
local point = { x = 1 }
print(point)
";
    client.open_with(CLIENT, text);
    let (l, c) = pos(text, "print(point", 6);
    assert_eq!(client.hover_at_level(CLIENT, l, c, None), ("```lua\nlocal point: Test.Point\n```".into(), 1));
    let (hover, _) = client.hover_at_level(CLIENT, l, c, Some(1));
    assert!(hover.contains("x: number,"), "a level in the request wins: {hover}");
}

#[test]
fn calls_through_function_aliases_use_their_signature() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@alias Check fun(value: string): boolean
---@alias Each fun(list: string[], cb: fun(item: string))
---@alias Handler fun(id: integer, name: string)
---@alias Run fun(self: Checker, cb: Handler)

---@type Check
local check = nil
---@type Check?
local maybeCheck = nil
---@type Each
local forEach = nil

---@class Checker
---@field check Check
---@field run Run
local Checker = {}

---@param cb Handler
local function on(cb) end

local checked = check('a')
local maybeChecked = maybeCheck('a')
local fieldChecked = Checker.check('a')
forEach({}, function(item) end)
on(function(handlerId, handlerName) end)
Checker:run(function(runId) end)
";
    client.open_with(CLIENT, text);
    let cases = [
        ("checked =", "checked: boolean"),
        ("maybeChecked", "maybeChecked: boolean"),
        ("fieldChecked", "fieldChecked: boolean"),
        // A function passed to a call takes its parameters from the alias of the callee or of the parameter.
        ("item)", "item: string"),
        ("handlerId", "handlerId: integer"),
        ("handlerName", "handlerName: string"),
        ("runId", "runId: integer"),
    ];
    for (needle, expected) in cases {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }

    let (l, c) = pos(text, "check('a'", 6);
    let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
    assert_eq!(result["signatures"][0]["label"], "check(value: string): boolean", "{result}");
    assert_eq!(result["activeParameter"], 0);
}

#[test]
fn cfxlua_extends_the_standard_libraries() {
    let mut client = Client::start(fixture_root());
    let text = "\
local nano = os.nanotime()
local delta = os.deltatime(nano, nano)
local clock = os.clock()
local trimmed = string.strtrim(' a ')
local kind = table.type({})
local entries = io.readdir('.')
local length = utf8.strlenutf8('a')
print(nano, delta, clock, trimmed, kind, entries, length)
";
    // FiveM only gives the server `os` and `io`.
    client.open_with(SERVER, text);
    for (needle, expected) in [
        ("nano = os", "nano: integer\n"),
        ("delta = os", "delta: integer\n"),
        ("clock = os", "clock: number\n"),
        ("trimmed = string", "trimmed: string\n"),
        ("kind = table", "kind: string\n"),
        ("entries = io", "entries: directory? {"),
        ("length = utf8", "length: integer\n"),
        // The functions CfxLua adds belong to the same table as the ones Lua defines.
        ("nanotime()", "function oslib.nanotime(): integer"),
        ("strtrim(", "function stringlib.strtrim(s: string, chars?: string): string"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(SERVER, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
}

#[test]
fn colon_calls_pass_their_receiver_to_functions_defined_with_a_dot() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@class Base
local Base = {}

---@generic T
---@param class T
---@return T
function Base.new(class, ...)
    return class
end

---@class Stall : Base
local Stall = {}

---@return Stall
local function open()
    return Stall:new(string.strtrim(' a '))
end

local made = Stall:new('name')
print(open, made)
";
    client.open_with(CLIENT, text);
    // `class` is the stall, not the string: the arguments start at the second parameter.
    let (l, c) = pos(text, "made = Stall", 0);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(!hover.contains("made: string"), "{hover}");
    assert_eq!(findings(&mut client, CLIENT, &["return-type-mismatch"]), []);
}

#[test]
fn type_names_of_fivem_lls_addon_are_declared() {
    let mut client = Client::start(fixture_root());
    let text = "\
---@type EventHandler
local handler = AddEventHandler('demo', function() end)
local key, name = handler.key, handler.name

---@param size vector
---@param transform matrix<vector3>
---@param state json_encode_state
---@param option json_options
---@param packing msgpack_options
---@param entity EntityInterface
---@param pending promise
local function addon(size, transform, state, option, packing, entity, pending)
    local position = vec3(1, 2, 3)
    local red, swizzle, count = position.r, position.xyz, position.n
    local data, queue = entity.__data, pending.queue
    local product, normal = dot(position, position), cross(position, position)
    local rotation = inv(quat(1, 0, 0, 0))
    local identity = mat4(1)
    for index, value in each({ 'a', 'b' }) do
        print(index, value)
    end
    local text = json.encode({}, { indent = true, sort_keys = true })
    print(size, transform, state, option, packing, red, swizzle, count, data, queue)
    print(product, normal, rotation, identity, text, json.getoption('indent'), msgpack.getoption('float'))
end

print(addon, key, name)
";
    client.open_with(CLIENT, text);
    for (needle, expected) in [
        ("handler = Add", "handler: EventHandler {"),
        ("key, name", "key: integer\n"),
        ("name = handler", "name: string\n"),
        ("size, transform, state, option", "size: vector {"),
        ("transform, state, option", "transform: matrix<vector3>\n"),
        ("red, swizzle", "red: number\n"),
        ("swizzle, count", "swizzle: vector3 {"),
        ("count = position", "count: integer\n"),
        ("data, queue", "data: integer\n"),
        ("queue = entity", "queue: promise[]\n"),
        ("product, normal", "product: number\n"),
        ("normal = dot", "normal: vector3 {"),
        ("rotation = inv", "rotation: quat {"),
        ("identity = mat4", "identity: matrix\n"),
        ("index, value in", "index: integer\n"),
        ("value in each", "value: \"a\"|\"b\"\n"),
    ] {
        let (l, c) = pos(text, needle, 0);
        let hover = client.hover_text(CLIENT, l, c);
        assert!(hover.contains(expected), "{needle}: expected {expected:?} in {hover}");
    }
    let codes = ["undefined-doc-name", "undefined-global", "undefined-field", "assign-type-mismatch"];
    assert_eq!(findings(&mut client, CLIENT, &codes), []);
}

#[test]
fn hover_shows_annotation_type_details_and_ranges() {
    let mut client = Client::start(fixture_root());
    let declarations = "\
---A named parking spot.
---@class Test.Point: GaragePoint
---@field name string
---@field locate fun(): Test.Point

---The result of a lookup.
---@alias Test.Result Test.Point|nil

---@enum Test.Mode
local modes = { active = 'active', closed = 'closed' }
";
    client.open_with("myresource/types.lua", declarations);
    let text = "local label = '🚗' ---@type Test.Point|Test.Result|Test.Mode|Test.Point\n";
    client.open_with(CLIENT, text);
    let cases: &[(&str, &[&str])] = &[
        (
            "Test.Point",
            &[
                "(class) Test.Point : GaragePoint",
                "name: string",
                "locate: fun(): Test.Point",
                "coords: vector3",
                "slots: integer?",
                "A named parking spot.",
            ],
        ),
        ("Test.Result", &["type Test.Result = Test.Point?", "The result of a lookup."]),
        ("Test.Mode", &["type Test.Mode = \"active\"|\"closed\""]),
    ];

    // Columns count UTF-16 units past the emoji; the last `Test.Point` must get its own range.
    let column = |name: &str| text[..text.rfind(name).unwrap()].encode_utf16().count() as u32;

    for &(name, expected) in cases {
        let column = column(name);
        let result = client.request("textDocument/hover", client.position_params(CLIENT, 0, column + 1));
        let hover = result["contents"]["value"].as_str().unwrap_or_default();
        for part in expected {
            assert!(hover.contains(*part), "{name}: missing {part:?} in {result}");
        }
        assert_eq!(
            result["range"],
            json!({
                "start": { "line": 0, "character": column },
                "end": { "line": 0, "character": column + name.len() as u32 }
            })
        );
    }

    client.change("myresource/types.lua", 2, &declarations.replace("name string", "name integer"));
    let hover = client.hover_text(CLIENT, 0, column("Test.Point") + 1);
    assert!(hover.contains("name: integer"), "{hover}");
}

#[test]
fn key_enums_are_the_union_of_their_keys() {
    let mut client = Client::start(fixture_root());
    let declarations = "---@enum (key) Test.Side\nlocal sides = { client = 1, ['server'] = 2 }\n";
    client.open_with("myresource/types.lua", declarations);
    client.open_with(CLIENT, "---@type Test.Side\n");
    let hover = client.hover_text(CLIENT, 0, 12);
    assert!(hover.contains("type Test.Side = \"client\"|\"server\""), "{hover}");
}

#[test]
fn hover_resolves_exports_across_resources() {
    let mut client = Client::start(fixture_root());
    let text = client.open(SERVER);
    let (l, c) = pos(&text, "GetPlayer(src)", 3);
    let hover = client.hover_text(SERVER, l, c);
    assert!(hover.contains("GetPlayer(source: integer)"), "{hover}");
    assert!(hover.contains("Looks a player up"), "{hover}");

    let (l, c) = pos(&text, "player.name", 8);
    assert!(client.hover_text(SERVER, l, c).contains("name: string"));
}

#[test]
fn completes_members_globals_natives_and_events() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let lines = text.lines().count() as u32;

    let with_line = |client: &mut Client, extra: &str, version: i32| {
        let changed = format!("{text}{extra}");
        client.change(CLIENT, version, &changed);
        (lines, extra.len() as u32)
    };

    let (l, c) = with_line(&mut client, "MyLib.", 2);
    let labels = client.completion_labels(CLIENT, l, c);
    for expected in ["round", "createGarage", "math", "version"] {
        assert!(labels.contains(&expected.to_string()), "{expected} missing from {labels:?}");
    }

    let (l, c) = with_line(&mut client, "garage:", 3);
    let labels = client.completion_labels(CLIENT, l, c);
    assert!(labels.contains(&"getVehicleCount".to_string()) && labels.contains(&"store".to_string()), "{labels:?}");
    assert!(!labels.contains(&"kind".to_string()), "fields should not be offered after ':' {labels:?}");

    let (l, c) = with_line(&mut client, "garage.point.", 4);
    assert_eq!(client.completion_labels(CLIENT, l, c), ["coords", "label", "slots"]);

    let (l, c) = with_line(&mut client, "local s = ('x'):", 5);
    assert!(client.completion_labels(CLIENT, l, c).contains(&"format".to_string()));

    let (l, c) = with_line(&mut client, "exports.", 6);
    let mut resources = client.completion_labels(CLIENT, l, c);
    resources.sort();
    assert_eq!(resources, ["late", "mylib", "myresource", "shop", "vault"]);

    let (l, c) = with_line(&mut client, "exports.mylib:", 7);
    let labels = client.completion_labels(CLIENT, l, c);
    assert!(labels.contains(&"GetPlayer".to_string()) && labels.contains(&"Ping".to_string()), "{labels:?}");

    let (l, c) = with_line(&mut client, "GetEntityCo", 8);
    let labels = client.completion_labels(CLIENT, l, c);
    assert!(labels.contains(&"GetEntityCoords".to_string()), "{labels:?}");

    let (l, c) = with_line(&mut client, "local z = Conf", 9);
    assert!(client.completion_labels(CLIENT, l, c).contains(&"Config".to_string()));

    let (l, c) = with_line(&mut client, "local z = roun", 10);
    assert!(client.completion_labels(CLIENT, l, c).contains(&"rounded".to_string()));

    let (l, _) = with_line(&mut client, "TriggerServerEvent('')", 11);
    let labels = client.completion_labels(CLIENT, l, 20);
    assert!(labels.contains(&"myresource:server:ping".to_string()), "{labels:?}");

    let (l, _) = with_line(&mut client, "MyLib.createGarage('public', {  })", 12);
    let labels = client.completion_labels(CLIENT, l, 31);
    assert_eq!(labels, ["coords", "label", "slots"]);

    let (l, c) = with_line(&mut client, "---@type Gar", 13);
    let labels = client.completion_labels(CLIENT, l, c);
    assert!(labels.contains(&"Garage".to_string()) && labels.contains(&"GarageKind".to_string()), "{labels:?}");
}

#[test]
fn completes_right_after_multibyte_characters() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;
    for (version, (extra, expected)) in [
        ("é", None),
        ("local a = →", None),
        ("local a = …pri", Some("print")),
        ("local a = 🚗GetEntityCo", Some("GetEntityCoords")),
        ("local a = “MyLib.", Some("round")),
        ("local a = ’.", None),
    ]
    .into_iter()
    .enumerate()
    {
        client.change(CLIENT, version as i32 + 2, &format!("{text}{extra}"));
        let labels = client.completion_labels(CLIENT, line, extra.encode_utf16().count() as u32);
        if let Some(expected) = expected {
            assert!(labels.contains(&expected.to_string()), "{extra}: {labels:?}");
        }
    }
}

#[test]
fn server_side_completion_hides_client_natives() {
    let mut client = Client::start(fixture_root());
    let text = client.open(SERVER);
    let changed = format!("{text}PlayerPed");
    client.change(SERVER, 2, &changed);
    let line = text.lines().count() as u32;
    let labels = client.completion_labels(SERVER, line, 9);
    assert!(!labels.contains(&"PlayerPedId".to_string()), "{labels:?}");

    let changed = format!("{text}GetPlayerIdent");
    client.change(SERVER, 3, &changed);
    assert!(client.completion_labels(SERVER, line, 14).contains(&"GetPlayerIdentifierByType".to_string()));

    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;
    client.change(CLIENT, 2, &format!("{text}if IsDuplicityVersion() then\n    GetPlayerIdent\nend"));
    let labels = client.completion_labels(CLIENT, line + 1, 18);
    assert!(labels.contains(&"GetPlayerIdentifierByType".to_string()), "server branch of a client file: {labels:?}");
    client.change(CLIENT, 3, &format!("{text}if IsDuplicityVersion() then\n    TriggerClientEvent('a', -1)\nend"));
    let found = client.diagnostics_for(CLIENT);
    assert!(!found.iter().any(|(code, _)| code == "fivem/native-wrong-side"), "{found:?}");
}

#[test]
fn goes_to_definitions_across_files() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);

    let (l, c) = pos(&text, "MyLib.round", 8);
    let result = client.request("textDocument/definition", client.position_params(CLIENT, l, c));
    assert!(result[0]["uri"].as_str().unwrap().ends_with("mylib/init.lua"), "{result}");
    assert_eq!(result[0]["range"]["start"]["line"], 16);

    let (l, c) = pos(&text, "require '@mylib.modules.settings'", 12);
    let result = client.request("textDocument/definition", client.position_params(CLIENT, l, c));
    assert!(result[0]["uri"].as_str().unwrap().ends_with("modules/settings.lua"), "{result}");

    let (l, c) = pos(&text, "'myresource:server:ping'", 5);
    let result = client.request("textDocument/definition", client.position_params(CLIENT, l, c));
    assert!(result[0]["uri"].as_str().unwrap().ends_with("server/main.lua"), "{result}");

    let (l, c) = pos(&text, "print(message, kind, count", 21);
    let result = client.request("textDocument/definition", client.position_params(CLIENT, l, c));
    assert_eq!(result[0]["range"]["start"]["line"], 2);
}

#[test]
fn annotation_definitions_and_hovers_use_closed_files() {
    let mut client = Client::start(fixture_root());
    let annotations = [
        ("---@type GaragePoint[]", "GaragePoint", 0),
        ("---@type table<string, Garage>", "Garage", 36),
        ("---@param garage Garage", "Garage", 36),
        ("---@field kind GarageKind", "GarageKind", 5),
        ("---@class TestGarage: Garage", "Garage", 36),
        ("---@alias GarageList Garage[]", "Garage", 36),
        ("---@return Garage garage, GarageKind kind", "GarageKind", 5),
        ("---@overload fun(point: GaragePoint): Garage", "Garage", 36),
        ("---@operator add(GaragePoint): Garage", "GaragePoint", 0),
        ("---@see Garage", "Garage", 36),
        ("local garage = {} --[[@as Garage]]", "Garage", 36),
    ];
    let text = annotations.iter().map(|(line, ..)| *line).collect::<Vec<_>>().join("\n");
    client.open_with(CLIENT, &text);

    for (line, (annotation, name, definition_line)) in annotations.iter().enumerate() {
        let character = annotation.rfind(*name).unwrap() as u32 + 1;
        let result = client.request("textDocument/definition", client.position_params(CLIENT, line as u32, character));
        assert_eq!(result.as_array().map(Vec::len), Some(1), "{annotation}: {result}");
        assert_eq!(result[0]["uri"], client.uri("[core]/mylib/init.lua").as_str(), "{annotation}");
        assert_eq!(result[0]["range"]["start"]["line"], *definition_line, "{annotation}");
        let hover = client.hover_text(CLIENT, line as u32, character);
        assert!(hover.contains(*name), "{annotation}: {hover}");
    }
}

#[test]
fn goes_to_namespaced_types_and_enums_in_unsaved_files() {
    let mut client = Client::start(fixture_root());
    let declarations = "---@class Test.Point\n\n---@alias Test.Result Test.Point\n\n---@enum Test.Mode\nlocal modes = { active = 'active' }\n";
    client.open_with("myresource/types.lua", declarations);
    let text = "local label = '🚗' ---@type Test.Point|Test.Result|Test.Mode\n";
    client.open_with(CLIENT, text);
    // One character into the name, counted in UTF-16 units past the emoji.
    let column = |name: &str| text[..text.find(name).unwrap() + 1].encode_utf16().count() as u32;

    for (name, line, character) in [("Test.Point", 0, 0), ("Test.Result", 2, 0), ("Test.Mode", 5, 6)] {
        let result = client.request("textDocument/definition", client.position_params(CLIENT, 0, column(name)));
        assert_eq!(result.as_array().map(Vec::len), Some(1), "{name}: {result}");
        assert_eq!(result[0]["uri"], client.uri("myresource/types.lua").as_str());
        assert_eq!(result[0]["range"]["start"], json!({ "line": line, "character": character }));
    }

    client.change("myresource/types.lua", 2, &format!("\n{declarations}"));
    let result = client.request("textDocument/definition", client.position_params(CLIENT, 0, column("Test.Point")));
    assert_eq!(result[0]["range"]["start"]["line"], 1);

    client.open_with("myresource/extra-types.lua", "---@class Test.Point\n---@alias Test.Result string\n");
    for name in ["Test.Point", "Test.Result"] {
        let result = client.request("textDocument/definition", client.position_params(CLIENT, 0, column(name)));
        let locations = result.as_array().unwrap();
        assert_eq!(locations.len(), 2, "{name}: {result}");
        for file in ["myresource/types.lua", "myresource/extra-types.lua"] {
            assert!(locations.iter().any(|location| location["uri"] == client.uri(file).as_str()), "{result}");
        }
    }
}

#[test]
fn goes_to_the_types_the_value_comes_with_and_to_the_enums_of_tables() {
    let mut client = Client::start(fixture_root());
    // Two resources that this one does not see declare `Test.Owner`; the export of one returns it.
    let shop =
        "---@class Test.Owner\n\n---@return Test.Owner\nfunction GetOwner() end\n\nexports('GetOwner', GetOwner)\n";
    client.open_with("shop/server.lua", shop);
    client.open_with("late/server.lua", "---@class Test.Owner\n");
    client.open_with("myresource/shared/config.lua", "---@enum Test.Color\nColors = { Red = 1 }\n");
    let text = "\
---@enum Test.Mode
local Modes = { On = 1 }
local owner = exports.shop:GetOwner()
print(Modes, Colors, owner)
";
    client.open_with(SERVER, text);
    let mut types_of = |needle: &str| -> Vec<(String, u64)> {
        let (line, column) = pos(text, needle, 0);
        let result = client.request("textDocument/typeDefinition", client.position_params(SERVER, line, column));
        let locations = result.as_array().cloned().unwrap_or_default();
        let file =
            |uri: &str| uri.rsplit('/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/");
        locations
            .iter()
            .map(|location| {
                (file(location["uri"].as_str().unwrap()), location["range"]["start"]["line"].as_u64().unwrap())
            })
            .collect()
    };
    let at = |file: &str, line: u64| (file.to_string(), line);
    assert_eq!(types_of("GetOwner()"), [at("shop/server.lua", 0)], "as the file of the export sees the name");
    assert_eq!(types_of("owner)"), [at("shop/server.lua", 0)], "a local that the export gives");
    assert_eq!(types_of("Colors,"), [at("shared/config.lua", 1)], "the global table of an enum");
    assert_eq!(types_of("Modes,"), [at("server/main.lua", 1)], "the local table of an enum");
}

#[test]
fn goes_to_the_declarations_of_the_types_of_values() {
    let mut client = Client::start(fixture_root());
    const TYPES: &str = "myresource/shared/config.lua";
    let types = "\
---@class Test.Player
---@field job Test.Job
Players = {}

---@class Test.Job

---@alias Test.Mode 'a'|'b'

---@enum Test.Color
Colors = { Red = 1 }

---@class Test.Box<T>
---@field value T

---@return Test.Player
function GetPlayer() end

---@alias Test.Pet Test.Player|Test.Job
---@alias Test.MaybePet Test.Pet?
---@alias Test.Pets Test.Pet[]
";
    client.open_with(TYPES, types);
    let text = "\
---@type Test.Player|Test.Job
local either
---@type Test.Player[]
local list = {}
---@type table<string, Test.Job>
local map = {}
---@type Test.Mode
local mode
---@type Test.Color
local color
---@type Test.Box<Test.Job>
local box
---@type integer
local count = 1
local vehicle = GetVehiclePedIsIn(PlayerPedId(), false)
local config = { a = 1 }
local player = GetPlayer()
---@param target Test.Player
local function use(target) return target end
---@type Test.MaybePet
local pet
---@type Test.Pets
local pets
print(either, list, map, mode, color, box, count, vehicle, config, player.job, GetPlayer(), use, pet, pets)
";
    client.open_with(CLIENT, text);
    let capabilities = serde_json::to_value(qbx_lua_ls::server::capabilities()).unwrap();
    assert_eq!(capabilities["typeDefinitionProvider"], true);
    let player = (TYPES, 0);
    let job = (TYPES, 4);
    let print_line = text.lines().count() as u32 - 1;
    let print = text.lines().last().unwrap();
    let mut types_of = |line: u32, column: u32| -> Vec<(&str, u64)> {
        let result = client.request("textDocument/typeDefinition", client.position_params(CLIENT, line, column));
        let locations = result.as_array().cloned().unwrap_or_default();
        locations
            .iter()
            .map(|location| {
                assert_eq!(location["uri"], client.uri(TYPES).as_str(), "{result}");
                (TYPES, location["range"]["start"]["line"].as_u64().unwrap())
            })
            .collect()
    };
    let at = |name: &str| print.find(name).unwrap() as u32 + 1;
    assert_eq!(types_of(print_line, at("either")), [player, job], "each type of a union");
    assert_eq!(types_of(print_line, at("list")), [player], "the elements of an array");
    assert_eq!(types_of(print_line, at("map")), [job], "the keys and values of a table");
    assert_eq!(types_of(print_line, at("mode")), [(TYPES, 6)], "an alias");
    assert_eq!(types_of(print_line, at("color")), [(TYPES, 9)], "the table of an enum");
    assert_eq!(types_of(print_line, at("box")), [(TYPES, 11), job], "a generic class and its argument");
    assert_eq!(types_of(print_line, at("player.job") + 7), [job], "a field");
    assert_eq!(types_of(print_line, at("GetPlayer")), [player], "what a function returns");
    assert_eq!(types_of(16, 7), [player], "a local that a call gives");
    assert_eq!(types_of(18, 35), [player], "a parameter");
    assert_eq!(
        types_of(print_line, at("pet,")),
        [(TYPES, 18), (TYPES, 17), player, job],
        "an alias and the types it stands for, through `?` and another alias"
    );
    assert_eq!(types_of(print_line, at("pets)")), [(TYPES, 19)], "not the elements of an array it stands for");
    for name in ["count", "vehicle", "config"] {
        assert_eq!(types_of(print_line, at(name)), [], "{name}: built-in types, native handles and plain tables");
    }
}

#[test]
fn members_set_through_self_belong_to_the_class_of_a_global_from_the_same_file() {
    let mut client = Client::start(fixture_root());
    // A new file, so the index knows nothing of `Players` when the file is first indexed.
    const PLAYERS: &str = "myresource/shared/players.lua";
    let text = "\
---@class Test.Player
Players = {}
Players.admin = {}

---@param name string
function Players:rename(name)
    self.nickname = name
end

function Players.admin:promote()
    self.level = 1
end

function Players.guest:greet()
    self.greeted = true
end

print(Players.nickname, Players.admin.level, Players.guest.greeted)
";
    client.open_with(PLAYERS, text);
    for (needle, line) in [("nickname,", 6), ("level,", 10), ("greeted)", 14)] {
        let (l, c) = pos(text, needle, 0);
        let result = client.request("textDocument/definition", client.position_params(PLAYERS, l, c));
        assert_eq!(result[0]["range"]["start"]["line"], line, "{needle}: {result}");
    }
}

#[test]
fn implementations_leave_out_the_annotations_that_declare_a_member() {
    let mut client = Client::start(fixture_root());
    const TYPES: &str = "myresource/shared/config.lua";
    let types = "\
---@class Test.Player
---@field name string
---@field greet fun(self: Test.Player): string
Players = {}

function Players:greet() return 'hi' end
";
    client.open_with(TYPES, types);
    let text = "\
---@type Test.Player
local player = Players
local count = 1
print(player:greet(), player.name, count)

---@param name string
function Players:rename(name)
    self.name = name
end
";
    client.open_with(CLIENT, text);
    let capabilities = serde_json::to_value(qbx_lua_ls::server::capabilities()).unwrap();
    assert_eq!(capabilities["implementationProvider"], true);
    let mut found = |method: &str, needle: &str, delta: u32| -> Vec<(String, u64)> {
        let (line, column) = pos(text, needle, delta);
        let result = client.request(method, client.position_params(CLIENT, line, column));
        let locations = result.as_array().cloned().unwrap_or_default();
        let file =
            |uri: &str| uri.rsplit('/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/");
        locations
            .iter()
            .map(|location| {
                (file(location["uri"].as_str().unwrap()), location["range"]["start"]["line"].as_u64().unwrap())
            })
            .collect()
    };
    let shared = |line: u64| ("shared/config.lua".to_string(), line);
    assert_eq!(found("textDocument/definition", "greet()", 0), [shared(2)], "definition picks the annotation");
    assert_eq!(found("textDocument/implementation", "greet()", 0), [shared(5)], "the method's body");
    let main = |line: u64| ("client/main.lua".to_string(), line);
    assert_eq!(found("textDocument/implementation", "name, count", 0), [main(7)], "an assignment through `self`");
    assert_eq!(found("textDocument/implementation", "count)", 0), [main(2)], "a local");
}

#[test]
fn implementations_of_members_no_code_sets_are_their_definition() {
    let mut client = Client::start(fixture_root());
    client.open_with(
        "shop/server.lua",
        "---@return integer\nfunction GetCount() return 1 end\n\nexports('GetCount', GetCount)\n",
    );
    let text = "print(exports.shop:GetCount())\n";
    client.open_with(SERVER, text);
    let (line, column) = pos(text, "GetCount", 0);
    let definition = client.request("textDocument/definition", client.position_params(SERVER, line, column));
    assert!(definition.as_array().is_some_and(|locations| !locations.is_empty()), "{definition}");
    let implementation = client.request("textDocument/implementation", client.position_params(SERVER, line, column));
    assert_eq!(implementation, definition, "an export the resource registers");
}

#[test]
fn members_set_through_self_belong_to_the_class_of_a_global_from_a_document_that_changed_with_them() {
    let mut client = Client::start(fixture_root());
    // Each file declares the global whose method the other one writes through `self`, so the file
    // indexed first cannot type `self` from the index in either order.
    const ALPHA: &str = "myresource/shared/alpha.lua";
    const BETA: &str = "myresource/shared/beta.lua";
    let alpha = "\
---@class Test.Alpha
Alphas = {}

function Betas:touch()
    self.fromAlpha = true
end
";
    let beta = "\
---@class Test.Beta
Betas = {}

function Alphas:touch()
    self.fromBeta = true
end
";
    client.open_with(ALPHA, alpha);
    client.open_with(BETA, beta);
    let text = "print(Alphas.fromBeta, Betas.fromAlpha)\n";
    client.open_with(CLIENT, text);
    for (needle, file) in [("fromBeta", BETA), ("fromAlpha", ALPHA)] {
        let (l, c) = pos(text, needle, 0);
        let result = client.request("textDocument/definition", client.position_params(CLIENT, l, c));
        assert_eq!(result[0]["uri"], client.uri(file).as_str(), "{needle}: {result}");
        assert_eq!(result[0]["range"]["start"]["line"], 4, "{needle}: {result}");
    }
}

#[test]
fn members_set_through_self_belong_to_the_class_of_a_global_typed_from_another_document() {
    let mut client = Client::start(fixture_root());
    // Documents are indexed in the order of their paths: `Mid` before the `Base` it is typed from,
    // and the method on `Mid` before `Mid` is indexed again, so it takes a third pass.
    let files = [
        ("myresource/shared/chain_a.lua", "Mid = Base\n"),
        ("myresource/shared/chain_b.lua", "---@class Test.Base\nBase = {}\n"),
        (
            "myresource/shared/chain_c.lua",
            "function Mid:touch()\n    self.touched = true\nend\n\nprint(Base.touched)\n",
        ),
    ];
    for (file, text) in files {
        client.open_with(file, text);
    }
    let (file, text) = files[2];
    let (l, c) = pos(text, "touched)", 0);
    let result = client.request("textDocument/definition", client.position_params(file, l, c));
    assert_eq!(result[0]["range"]["start"]["line"], 1, "{result}");
}

#[test]
fn annotation_features_ignore_names_outside_type_positions() {
    let mut client = Client::start(fixture_root());
    let lines = [
        "---@param Garage string",
        "---@field Garage string",
        "---@return string Garage",
        "---@type 'Garage'",
        "---@type fun(Garage: string): boolean",
        "---@type { Garage: string }",
        "---@type string # Garage",
        "---Garage is a class.",
        "-- Garage",
        "local text = '---@type Garage'",
        "local text = [[---@type Garage]]",
        "--[[---@type Garage]]",
        "---@type MissingGarage",
    ];
    client.open_with(CLIENT, &lines.join("\n"));

    for (line, text) in lines.iter().enumerate() {
        let column = text.find("Garage").unwrap() as u32 + 1;
        for method in ["textDocument/definition", "textDocument/hover"] {
            let result = client.request(method, client.position_params(CLIENT, line as u32, column));
            assert!(result.is_null(), "{method} on {text}: {result}");
        }
    }
}

#[test]
fn signature_help_tracks_the_active_parameter() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let (l, c) = pos(&text, "MyLib.round(1.2345, 2)", 20);
    let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
    assert_eq!(result["signatures"][0]["label"], "MyLib.round(value: number, decimals?: integer): number");
    assert_eq!(result["activeParameter"], 1);

    let (l, c) = pos(&text, "garage:store('ABC123')", 14);
    let result = client.request("textDocument/signatureHelp", client.position_params(CLIENT, l, c));
    assert_eq!(result["signatures"][0]["label"], "store(plate: string): boolean, string?");
}

#[test]
fn trigger_calls_show_the_parameters_of_the_handler() {
    let mut client = Client::start(fixture_root());
    let text = client.open(SHOP_CLIENT);
    let (l, c) = pos(&text, "TriggerServerEvent('shop:buy', 'water'", 31);
    let result = client.request("textDocument/signatureHelp", client.position_params(SHOP_CLIENT, l, c));
    let signature = &result["signatures"][0];
    assert_eq!(signature["label"], "TriggerServerEvent(eventName: string, item, amount)");
    assert_eq!(result["activeParameter"], 1);
    let note = signature["documentation"]["value"].as_str().unwrap_or_default();
    assert!(note.contains("shop/server.lua:1"), "{note}");

    let hints = client.request(
        "textDocument/inlayHint",
        json!({ "textDocument": { "uri": client.uri(SHOP_CLIENT) }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 40, "character": 0 } } }),
    );
    let labels: Vec<&str> = hints.as_array().unwrap().iter().filter_map(|h| h["label"].as_str()).collect();
    assert!(labels.contains(&"item:") && labels.contains(&"amount:"), "{labels:?}");
}

#[test]
fn calls_through_exports_pass_what_follows_the_first_value() {
    fn hint_labels(client: &mut Client, relative: &str) -> Vec<String> {
        let hints = client.request(
            "textDocument/inlayHint",
            json!({ "textDocument": { "uri": client.uri(relative) }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 40, "character": 0 } } }),
        );
        hints.as_array().unwrap().iter().filter_map(|hint| hint["label"].as_str().map(str::to_owned)).collect()
    }
    const MYLIB: &str = "[core]/mylib/server.lua";
    let mut client = Client::start(fixture_root());
    let mylib = client.open(MYLIB);
    let registered = "
---@param text string
---@param notifyType string
---@param duration integer
local function Notify(text, notifyType, duration) end
exports('Notify', Notify)

---@param cb fun(player: integer)
exports('OnReady', function(cb) end)
";
    client.change(MYLIB, 2, &format!("{mylib}{registered}"));
    let text = "exports.mylib:Notify('hello', 'error')
exports['mylib'].Notify(nil, 'hello', 'error')
exports.mylib:OnReady(function(player) end)
";
    client.open_with(SERVER, text);
    assert_eq!(
        hint_labels(&mut client, SERVER),
        ["text:", "notifyType:", "text:", "notifyType:"],
        "the proxy drops the receiver of a `:` call and the first argument of a `.` call"
    );

    let (l, c) = pos(text, "'error')", 1);
    let result = client.request("textDocument/signatureHelp", client.position_params(SERVER, l, c));
    assert_eq!(result["signatures"][0]["label"], "Notify(text: string, notifyType: string, duration: integer)");
    assert_eq!(result["activeParameter"], 1);
    let (l, c) = pos(text, "nil, 'hello', 'error'", 15);
    let result = client.request("textDocument/signatureHelp", client.position_params(SERVER, l, c));
    assert_eq!(result["activeParameter"], 1);

    let (l, c) = pos(text, "player)", 0);
    let player = client.hover_text(SERVER, l, c);
    assert!(player.contains("player: integer"), "a function argument takes the type of its parameter: {player}");

    // Declared types keep lining up as declared: `fun(self, ...)` fields skip `self`, and methods
    // declared on `exports.<resource>` with `:` do not list it.
    let root = declared_exports_root();
    let mut client = Client::start_with_library(root.join("workspace"), &root.join("types"));
    let text = "local cid = exports.qbx_core:GetCid(1)
local rang = exports.tablet:Ring(2)
print(cid, rang)
";
    client.open_with("app/server.lua", text);
    assert_eq!(hint_labels(&mut client, "app/server.lua"), ["source:", "times:"]);
}

#[test]
fn arguments_through_exports_go_to_the_parameters_the_proxy_passes() {
    const MYLIB: &str = "[core]/mylib/server.lua";
    let mut client = Client::start(fixture_root());
    let mylib = client.open(MYLIB);
    let registered = "
---@param self string
---@param count integer
local function Track(self, count) end
exports('Track', Track)
";
    client.change(MYLIB, 2, &format!("{mylib}{registered}"));
    let text = "exports.mylib:Track('id', 2)
exports.mylib:Track(5)
exports['mylib'].Track(nil, 'id', 'two')
";
    client.open_with(SERVER, text);
    let finding = |needle: &str, message: &str| {
        ("param-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, SERVER, &["param-type-mismatch"]),
        [
            finding("Track(5)", "Cannot assign `integer` to parameter `self` of type `string`"),
            finding("'two'", "Cannot assign `string` to parameter `count` of type `integer`"),
        ],
        "a first parameter named `self` takes the first value the proxy passes, as hover and signature help show"
    );

    let root = declared_exports_root();
    let mut client = Client::start_with_library(root.join("workspace"), &root.join("types"));
    let text = "local cid = exports.qbx_core:GetCid('one')
local rang = exports.tablet:Ring('twice')
print(cid, rang)
";
    client.open_with("app/server.lua", text);
    let finding = |needle: &str, message: &str| {
        ("param-type-mismatch".to_string(), pos(text, needle, 0).0 as u64, message.to_string())
    };
    assert_eq!(
        findings(&mut client, "app/server.lua", &["param-type-mismatch"]),
        [
            finding("GetCid", "Cannot assign `string` to parameter `source` of type `number`"),
            finding("Ring", "Cannot assign `string` to parameter `times` of type `number`"),
        ],
        "declared types line up as declared"
    );
}

#[test]
fn exports_of_methods_pass_the_first_value_to_self() {
    const MYLIB: &str = "[core]/mylib/server.lua";
    let mut client = Client::start(fixture_root());
    let mylib = client.open(MYLIB);
    let registered = "
---@class Test.Garage
local Garage = {}

---@param plate string
---@return boolean
function Garage:Park(plate) return plate ~= '' end
exports('Park', Garage.Park)
";
    client.change(MYLIB, 2, &format!("{mylib}{registered}"));
    // `Garage.Park` takes the garage as `self` before the plate, and the proxy passes it the first
    // value of the call.
    let text = "local parked = exports.mylib:Park({}, 'ABC')\nprint(parked)\n";
    client.open_with(SERVER, text);
    let (l, c) = pos(text, "'ABC'", 0);
    let result = client.request("textDocument/signatureHelp", client.position_params(SERVER, l, c));
    assert_eq!(result["signatures"][0]["label"], "Park(self: Test.Garage, plate: string): boolean");
    assert_eq!(result["activeParameter"], 1);
    let hints = client.request(
        "textDocument/inlayHint",
        json!({ "textDocument": { "uri": client.uri(SERVER) }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 0 } } }),
    );
    let labels: Vec<&str> = hints.as_array().unwrap().iter().map(|h| h["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["self:", "plate:"]);
    assert!(findings(&mut client, SERVER, &["fivem/export-argument-count"]).is_empty());
}

#[test]
fn publishes_lint_diagnostics_with_resource_context() {
    let mut client = Client::start(fixture_root());
    client.open(CLIENT);
    assert_eq!(client.diagnostics_for(CLIENT), []);

    let text = client.open(SERVER);
    let found = client.diagnostics_for(SERVER);
    assert_eq!(found, [("fivem/import-not-declared".to_string(), 8), ("unused-argument".to_string(), 8)], "{found:?}");

    let broken = format!("{text}\nlocal ped = PlayerPedId()\nprint(notDefinedAnywhere)\n");
    client.change(SERVER, 2, &broken);
    let codes: Vec<String> = client.diagnostics_for(SERVER).into_iter().map(|(code, _)| code).collect();
    assert!(codes.contains(&"fivem/native-wrong-side".to_string()), "{codes:?}");
    assert!(codes.contains(&"undefined-global".to_string()), "{codes:?}");
    assert!(codes.contains(&"unused-local".to_string()), "{codes:?}");
}

#[test]
fn server_libraries_only_exist_where_the_server_runs_the_code() {
    const SHARED: &str = "myresource/shared/config.lua";
    let mut client = Client::start(fixture_root());
    let text = "local now = os.time()\nlocal entries = io.readdir('logs')\nprint(now, entries)\n";
    let mut wrong_side = |file: &str| -> Vec<u64> {
        client.open_with(file, text);
        let found = client.diagnostics_for(file).into_iter().filter(|(code, _)| code == "fivem/native-wrong-side");
        found.map(|(_, line)| line).collect()
    };
    assert_eq!(wrong_side(CLIENT), [0, 1]);
    assert_eq!(wrong_side(SHARED), [0, 1]);
    assert!(wrong_side(SERVER).is_empty());
    client.open_with(SHARED, &format!("if not IsDuplicityVersion() then return end\n{text}"));
    assert!(client.diagnostics_for(SHARED).iter().all(|(code, _)| code != "fivem/native-wrong-side"));

    let (l, c) = pos(text, "os.time", 0);
    assert!(client.hover_text(SERVER, l, c).contains("oslib"));
    assert!(!client.hover_text(CLIENT, l, c).contains("oslib"), "the client has no `os`");

    let typed = "print(os.)\n";
    let (l, c) = pos(typed, "os.", 3);
    for file in [SERVER, SHARED] {
        client.open_with(file, typed);
        let labels = client.completion_labels(file, l, c);
        assert!(["time", "createdir", "nanotime"].iter().all(|f| labels.iter().any(|l| l == f)), "{file}: {labels:?}");
        assert!(!labels.iter().any(|l| l == "exit"), "FiveM's `os` has no `exit`: {labels:?}");
    }
    client.open_with(CLIENT, typed);
    assert!(client.completion_labels(CLIENT, l, c).is_empty());
}

#[test]
fn missing_arguments_follow_annotations_in_other_files() {
    const SHARED: &str = "myresource/shared/config.lua";
    let mut client = Client::start(fixture_root());
    let shared = client.open(SHARED);
    let text = client.open(CLIENT);
    let notify = "\n---@param message string\n---@param duration integer\nfunction Notify(message, duration) print(message, duration) end\n";
    client.change(SHARED, 2, &format!("{shared}{notify}"));
    let calling = format!("{text}\nNotify('saved')\n");
    client.change(CLIENT, 2, &calling);
    let line = calling.lines().count() as u64 - 1;
    let found = client.diagnostics_for(CLIENT);
    assert!(found.contains(&("missing-parameter".to_string(), line)), "{found:?}");

    client.change(SHARED, 3, &format!("{shared}{}", notify.replace("duration integer", "duration? integer")));
    let found = client.diagnostics_for(CLIENT);
    assert!(!found.iter().any(|(code, _)| code == "missing-parameter"), "the parameter became optional: {found:?}");
}

#[test]
fn references_rename_and_symbols() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);

    let (l, c) = pos(&text, "local count", 7);
    let mut params = client.position_params(CLIENT, l, c);
    params["context"] = json!({ "includeDeclaration": true });
    let refs = client.request("textDocument/references", params);
    assert_eq!(refs.as_array().unwrap().len(), 2);

    let (l, c) = pos(&text, "Config.SpawnDistance", 2);
    let mut params = client.position_params(CLIENT, l, c);
    params["context"] = json!({ "includeDeclaration": true });
    let refs = client.request("textDocument/references", params);
    let files: Vec<&str> = refs.as_array().unwrap().iter().map(|r| r["uri"].as_str().unwrap()).collect();
    assert!(
        files.iter().any(|f| f.ends_with("shared/config.lua")) && files.iter().any(|f| f.ends_with("server/main.lua")),
        "{files:?}"
    );

    let mut params = client.position_params(CLIENT, l, c);
    params["newName"] = json!("Settings");
    let edit = client.request("textDocument/rename", params);
    assert!(edit["changes"].as_object().unwrap().len() >= 3, "{edit}");

    let symbols =
        client.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": client.uri(CLIENT) } }));
    let names: Vec<&str> = symbols.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"garage") && names.contains(&"RegisterNetEvent 'myresource:client:notify'"), "{names:?}");

    let found = client.request("workspace/symbol", json!({ "query": "garage" }));
    let names: Vec<&str> = found.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"MyLib.createGarage") && names.contains(&"Garage"), "{names:?}");
}

#[test]
fn code_actions_inlay_hints_tokens_and_folding() {
    let mut client = Client::start(fixture_root());
    let text = "Citizen.CreateThread(function()\n    local hash = GetHashKey('adder')\n    SetEntityCoords(hash, 1.0, 2.0, 3.0, false, false, false, true)\nend)\n";
    client.open_with(CLIENT, text);
    client.diagnostics_for(CLIENT);
    let uri = client.uri(CLIENT).to_string();
    let diagnostics = client.diagnostics[&uri].clone();
    let actions = client.request(
        "textDocument/codeAction",
        json!({ "textDocument": { "uri": uri }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 3, "character": 0 } }, "context": { "diagnostics": diagnostics } }),
    );
    let titles: Vec<&str> = actions.as_array().unwrap().iter().map(|a| a["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"Replace with 'CreateThread'"), "{titles:?}");
    assert!(titles.contains(&"Convert to a compile-time hash literal"), "{titles:?}");
    assert!(titles.iter().any(|t| t.starts_with("Disable fivem/hash-literal")), "{titles:?}");

    let hints = client.request(
        "textDocument/inlayHint",
        json!({ "textDocument": { "uri": uri }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 4, "character": 0 } } }),
    );
    let labels: Vec<&str> = hints.as_array().unwrap().iter().map(|h| h["label"].as_str().unwrap()).collect();
    assert_eq!(&labels[..3], ["string:", "xPos:", "yPos:"], "{labels:?}");

    let tokens = client.request("textDocument/semanticTokens/full", json!({ "textDocument": { "uri": uri } }));
    assert!(tokens["data"].as_array().unwrap().len() >= 5 * 6);

    let folds = client.request("textDocument/foldingRange", json!({ "textDocument": { "uri": uri } }));
    assert_eq!(folds[0]["startLine"], 0);
}

#[test]
fn count_down_loops_get_a_negative_step_as_a_quick_fix() {
    let mut client = Client::start(fixture_root());
    let text = "local total = 0\nfor i = 3, 1 do total = total + i end\nprint(total)\n";
    client.open_with(CLIENT, text);
    let found = client.diagnostics_for(CLIENT);
    assert_eq!(found, [("count-down-loop".to_string(), 1)], "{found:?}");
    let uri = client.uri(CLIENT).to_string();
    let diagnostics = client.diagnostics[&uri].clone();
    let actions = client.request(
        "textDocument/codeAction",
        json!({ "textDocument": { "uri": uri }, "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 0 } }, "context": { "diagnostics": diagnostics } }),
    );
    let fix =
        actions.as_array().unwrap().iter().find(|a| a["title"] == "Count down with a step of -1").expect("quick fix");
    let edit = &fix["edit"]["changes"][&uri][0];
    assert_eq!(
        (edit["range"]["start"]["line"].as_u64(), edit["range"]["start"]["character"].as_u64()),
        (Some(1), Some(12))
    );
    assert_eq!(edit["newText"], ", -1");
}

#[test]
fn trailing_whitespace_is_removed_by_a_quick_fix() {
    let mut client = Client::start(fixture_root());
    let text = "local total = 0  \nprint(total)\n";
    client.open_with(CLIENT, text);
    let found = client.diagnostics_for(CLIENT);
    assert_eq!(found, [("trailing-space".to_string(), 0)], "{found:?}");
    let uri = client.uri(CLIENT).to_string();
    let diagnostics = client.diagnostics[&uri].clone();
    let actions = client.request(
        "textDocument/codeAction",
        json!({ "textDocument": { "uri": uri }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "context": { "diagnostics": diagnostics } }),
    );
    let fix =
        actions.as_array().unwrap().iter().find(|a| a["title"] == "Remove trailing whitespace").expect("quick fix");
    let edit = &fix["edit"]["changes"][&uri][0];
    assert_eq!(
        edit["range"],
        json!({ "start": { "line": 0, "character": 15 }, "end": { "line": 0, "character": 17 } })
    );
    assert_eq!(edit["newText"], "");
}

#[test]
fn survives_garbage_input_while_typing() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    for (version, cut) in (2..).zip((0..text.len()).step_by(37)) {
        if !text.is_char_boundary(cut) {
            continue;
        }
        client.change(CLIENT, version, &text[..cut]);
        let line = text[..cut].matches('\n').count() as u32;
        let col = (cut - text[..cut].rfind('\n').map_or(0, |i| i + 1)) as u32;
        client.request("textDocument/completion", client.position_params(CLIENT, line, col));
        client.request("textDocument/hover", client.position_params(CLIENT, line, col.saturating_sub(1)));
        client.request("textDocument/signatureHelp", client.position_params(CLIENT, line, col));
    }
}

#[test]
fn reports_problems_for_files_that_are_not_open() {
    let mut client = Client::start(fixture_root());
    let found = client.diagnostics_for(SERVER);
    assert_eq!(found, [("fivem/import-not-declared".to_string(), 8)], "{found:?}");
    assert_eq!(client.diagnostics_for(CLIENT), []);
}

#[test]
fn event_completion_follows_the_call_direction() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;

    client.change(CLIENT, 2, &format!("{text}TriggerServerEvent('')"));
    let labels = client.completion_labels(CLIENT, line, 20);
    assert_eq!(labels, ["myresource:server:ping", "shop:buy", "shop:refund"], "only server handlers are reachable");

    client.change(CLIENT, 3, &format!("{text}TriggerEvent('')"));
    let labels = client.completion_labels(CLIENT, line, 14);
    assert_eq!(labels, ["myresource:client:notify", "shop:bought"]);
}

#[test]
fn event_completion_replaces_the_whole_name_across_colons() {
    for snippets in [false, true] {
        let mut client = Client::start_with_capabilities(
            fixture_root(),
            json!({ "textDocument": { "completion": { "completionItem": { "snippetSupport": snippets } } } }),
        );
        client.open_with(CLIENT, "");
        let mut version = 1;
        for (call, name) in [
            ("TriggerServerEvent", "myresource:server:ping"),
            ("TriggerLatentServerEvent", "myresource:server:ping"),
            ("TriggerEvent", "shop:bought"),
            ("RegisterNetEvent", "myresource:server:ping"),
            ("lib.callback.await", "myresource:getGarages"),
        ] {
            for quote in ['\'', '"'] {
                for suffix in ["", "stale"] {
                    for length in 0..=name.len() {
                        let prefix = &name[..length];
                        let head = format!("local label = '🚗'; {call}({quote}");
                        let tail = format!("{suffix}{quote}, 42)");
                        let text = format!("{head}{prefix}{tail}");
                        let start = head.encode_utf16().count() as u32;
                        let cursor = start + prefix.len() as u32;
                        version += 1;
                        client.change(CLIENT, version, &text);
                        let mut params = client.position_params(CLIENT, 0, cursor);
                        params["context"] = if prefix.ends_with(':') {
                            json!({ "triggerKind": 2, "triggerCharacter": ":" })
                        } else {
                            json!({ "triggerKind": 1 })
                        };
                        let result = client.request("textDocument/completion", params);
                        let item = result["items"].as_array().unwrap().iter().find(|i| i["label"] == name).unwrap();
                        assert_eq!(
                            item["textEdit"],
                            json!({
                                "range": { "start": { "line": 0, "character": start },
                                    "end": { "line": 0, "character": cursor + suffix.len() as u32 } },
                                "newText": name
                            }),
                            "{text} at {cursor}"
                        );
                        assert_eq!(item["insertTextFormat"], Value::Null);
                    }
                }
                let head = format!("{call}({quote}");
                let prefix = name.rsplit_once(':').unwrap().0.to_string() + ":";
                let text = format!("{head}{prefix}");
                version += 1;
                client.change(CLIENT, version, &text);
                let result =
                    client.request("textDocument/completion", client.position_params(CLIENT, 0, text.len() as u32));
                let item = result["items"].as_array().unwrap().iter().find(|i| i["label"] == name).unwrap();
                assert_eq!(
                    item["textEdit"]["range"],
                    json!({
                        "start": { "line": 0, "character": head.len() },
                        "end": { "line": 0, "character": text.len() }
                    }),
                    "unterminated string: {text}"
                );
            }
        }
    }
}

#[test]
fn reports_the_side_of_a_file() {
    let mut client = Client::start(fixture_root());
    let info = client.request("qbx/fileInfo", json!({ "uri": client.uri(CLIENT) }));
    assert_eq!(info, json!({ "side": "client", "resource": "myresource" }));
    let info = client.request("qbx/fileInfo", json!({ "uri": client.uri("myresource/shared/config.lua") }));
    assert_eq!(info["side"], "shared");
    let info = client.request("qbx/fileInfo", json!({ "uri": client.uri("[core]/mylib/modules/settings.lua") }));
    assert_eq!(info["side"], "module");
}

const SHOP_CLIENT: &str = "shop/client.lua";
const SHOP_SERVER: &str = "shop/server.lua";

#[test]
fn cross_file_rules_run_in_the_editor() {
    let mut client = Client::start(fixture_root());
    let found = client.diagnostics_for(SHOP_CLIENT);
    let expected = [
        ("qbox/unknown-locale-key", 4),
        ("fivem/event-argument-count", 7),
        ("fivem/event-wrong-side", 8),
        ("fivem/export-argument-count", 9),
        ("manifest/missing-dependency", 9),
    ];
    for (code, line) in expected {
        assert!(found.contains(&(code.to_string(), line)), "{code} on line {line} missing from {found:?}");
    }

    let codes: Vec<String> = client.diagnostics_for(SHOP_SERVER).into_iter().map(|(code, _)| code).collect();
    for code in ["security/client-supplied-source", "security/unvalidated-event-argument", "security/sql-concatenation"]
    {
        assert!(codes.contains(&code.to_string()), "{code} missing from {codes:?}");
    }
    assert!(codes.contains(&"fivem/event-argument-count".to_string()), "shop:bought takes one argument: {codes:?}");

    let unused = client.diagnostics_for("shop/locales/en.json");
    assert_eq!(unused, [("qbox/unused-locale-key".to_string(), 3), ("qbox/unused-locale-key".to_string(), 5)]);
}

#[test]
fn completes_locale_keys_convars_and_state_bags() {
    let mut client = Client::start(fixture_root());
    let text = client.open(SHOP_CLIENT);
    let line = text.lines().count() as u32;

    client.change(SHOP_CLIENT, 2, &format!("{text}print(locale(''))"));
    assert_eq!(client.completion_labels(SHOP_CLIENT, line, 14), ["buy.success", "buy.failed", "never_used"]);

    client.change(SHOP_CLIENT, 3, &format!("{text}print(GetConvarInt(''))"));
    assert_eq!(client.completion_labels(SHOP_CLIENT, line, 20), ["shop_debug"]);

    client.change(SHOP_CLIENT, 4, &format!("{text}print(LocalPlayer.state.)"));
    assert!(client.completion_labels(SHOP_CLIENT, line, 24).contains(&"isShopping".to_string()));

    let (l, c) = pos(&text, "'buy.success'", 3);
    assert!(client.hover_text(SHOP_CLIENT, l, c).contains("You bought %s"));
    let definition = client.request("textDocument/definition", client.position_params(SHOP_CLIENT, l, c));
    assert!(definition[0]["uri"].as_str().unwrap().ends_with("locales/en.json"), "{definition}");
    assert_eq!(definition[0]["range"]["start"]["line"], 2);
}

#[test]
fn finds_and_renames_fields_across_files() {
    let mut client = Client::start(fixture_root());
    let text = client.open(SHOP_CLIENT);
    let (l, c) = pos(&text, "Shop.getPrice", 7);

    let mut params = client.position_params(SHOP_CLIENT, l, c);
    params["context"] = json!({ "includeDeclaration": true });
    let refs = client.request("textDocument/references", params);
    let mut files: Vec<String> = refs
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().rsplit('/').next().unwrap().to_string())
        .collect();
    files.sort();
    assert_eq!(files, ["client.lua", "server.lua", "shared.lua"], "{refs}");

    let mut params = client.position_params(SHOP_CLIENT, l, c);
    params["newName"] = json!("priceOf");
    let edit = client.request("textDocument/rename", params);
    assert_eq!(edit["changes"].as_object().unwrap().len(), 3, "{edit}");

    let (l, c) = pos(&text, "exports.mylib:Ping", 15);
    let prepared = client.request("textDocument/prepareRename", client.position_params(SHOP_CLIENT, l, c));
    assert!(prepared.is_object(), "exports defined in the workspace can be renamed: {prepared}");
}

fn renamed_text(client: &mut Client, relative: &str, text: &str, needle: &str, delta: u32) -> String {
    let (line, column) = pos(text, needle, delta);
    let params = client.position_params(relative, line, column);
    let prepared = client.request("textDocument/prepareRename", params.clone());
    assert!(prepared.is_object(), "rename should be available on {needle}: {prepared}");
    let mut params = params;
    params["newName"] = json!("renamed");
    let result = client.request("textDocument/rename", params);
    let edit: lsp_types::WorkspaceEdit = serde_json::from_value(result.clone()).expect("rename edit");
    let changes = edit.changes.unwrap();
    assert_eq!(changes.len(), 1, "unrelated objects must not be renamed: {result}");
    let mut edits = changes[&client.uri(relative)].clone();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
    let lines = qbx_lua_syntax::LineIndex::new(text);
    let mut output = text.to_string();
    let mut previous_start = text.len() as u32;
    for edit in edits {
        let start = lines.offset_utf16(
            text,
            qbx_lua_syntax::LineCol { line: edit.range.start.line, col: edit.range.start.character },
        );
        let end = lines
            .offset_utf16(text, qbx_lua_syntax::LineCol { line: edit.range.end.line, col: edit.range.end.character });
        assert!(end <= previous_start, "rename edits must not overlap: {result}");
        previous_start = start;
        output.replace_range(start as usize..end as usize, &edit.new_text);
    }
    assert!(qbx_lua_syntax::parse(&output).errors.is_empty(), "renamed Lua must parse: {output}");
    output
}

#[test]
fn rename_static_string_reads_and_writes_preserves_delimiters() {
    let mut client = Client::start(fixture_root());
    let text = "Audit = { foo = 1 }\nAudit['foo'] = 2\nprint(Audit.foo, Audit[\"foo\"], Audit[ [=[foo]=] ], Audit['f\\111o'])\nOther = { foo = 3 }\nprint(Other['foo'])\n";
    client.open_with(SHOP_CLIENT, text);
    let expected = "Audit = { renamed = 1 }\nAudit['renamed'] = 2\nprint(Audit.renamed, Audit[\"renamed\"], Audit[ [=[renamed]=] ], Audit['renamed'])\nOther = { foo = 3 }\nprint(Other['foo'])\n";
    assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, "Audit.foo", 7), expected);
    assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, "Audit['foo']", 8), expected);
    let (line, column) = pos(text, "Audit['foo']", 8);
    let mut params = client.position_params(SHOP_CLIENT, line, column);
    params["context"] = json!({ "includeDeclaration": true });
    let refs = client.request("textDocument/references", params);
    assert_eq!(refs.as_array().unwrap().len(), 6, "{refs}");
    let highlights =
        client.request("textDocument/documentHighlight", client.position_params(SHOP_CLIENT, line, column));
    assert_eq!(highlights.as_array().unwrap().len(), 6, "{highlights}");
}

#[test]
fn rename_static_string_declarations_and_nested_paths() {
    let mut client = Client::start(fixture_root());
    let cases = [
        ("Audit = { ['foo'] = 1 }\nprint(Audit.foo, Audit['foo'])\n",
         "['foo']", 3,
         "Audit = { ['renamed'] = 1 }\nprint(Audit.renamed, Audit['renamed'])\n"),
        ("Audit = {}\nAudit['foo'] = 1\nprint(Audit.foo, Audit['foo'])\n",
         "Audit.foo", 7,
         "Audit = {}\nAudit['renamed'] = 1\nprint(Audit.renamed, Audit['renamed'])\n"),
        ("Audit = { ['nested'] = { [ [=[\nfoo]=] ] = 1 } }\nprint(Audit['nested'].foo, Audit.nested['foo'])\n",
         ".foo", 2,
         "Audit = { ['nested'] = { [ [=[\nrenamed]=] ] = 1 } }\nprint(Audit['nested'].renamed, Audit.nested['renamed'])\n"),
    ];
    for (version, (text, needle, delta, expected)) in (1..).zip(cases) {
        if version == 1 {
            client.open_with(SHOP_CLIENT, text);
        } else {
            client.change(SHOP_CLIENT, version, text);
        }
        assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, needle, delta), expected);
    }
}

#[test]
fn rename_annotation_fields_updates_declarations_and_typed_constructors() {
    let mut client = Client::start(fixture_root());
    let text = "---@class RenameOptions\n---@field private foo? number foo description\nlocal audit = { ['foo'] = 1 }\n---@type RenameOptions\nlocal other = { foo = 2 }\nprint(audit.foo, other['foo'])\n";
    client.open_with(SHOP_CLIENT, text);
    let expected = "---@class RenameOptions\n---@field private renamed? number foo description\nlocal audit = { ['renamed'] = 1 }\n---@type RenameOptions\nlocal other = { renamed = 2 }\nprint(audit.renamed, other['renamed'])\n";
    assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, "audit.foo", 7), expected);
    assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, "private foo", 9), expected);

    let text = "---@class RenameOptions\n---@field ['foo'] number foo description\nlocal audit = { foo = 1 }\nprint(audit['foo'])\n";
    client.change(SHOP_CLIENT, 2, text);
    let expected = "---@class RenameOptions\n---@field ['renamed'] number foo description\nlocal audit = { renamed = 1 }\nprint(audit['renamed'])\n";
    assert_eq!(renamed_text(&mut client, SHOP_CLIENT, text, "audit['foo']", 8), expected);
}

#[test]
fn rename_finds_escaped_keys_in_closed_files_and_aborts_if_a_file_is_unreadable() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-rename-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    std::fs::create_dir(&fixture.0).unwrap();
    std::fs::write(
        fixture.0.join("fxmanifest.lua"),
        "fx_version 'cerulean'\ngame 'gta5'\nshared_scripts { 'main.lua', 'closed.lua' }\n",
    )
    .unwrap();
    std::fs::write(fixture.0.join("main.lua"), "Audit = { foo = 1 }\nprint(Audit.foo)\n").unwrap();
    std::fs::write(fixture.0.join("closed.lua"), "print(Audit['\\102\\111\\111'])\n").unwrap();
    let mut client = Client::start(fixture.0.clone());
    let text = client.open("main.lua");
    let (line, column) = pos(&text, "Audit.foo", 7);
    let mut params = client.position_params("main.lua", line, column);
    params["newName"] = json!("renamed");
    let result = client.request("textDocument/rename", params.clone());
    let changes = result["changes"].as_object().expect("rename edit");
    assert_eq!(changes.len(), 2, "escaped references in closed files must be found: {result}");
    let edits = changes[client.uri("closed.lua").as_str()].as_array().unwrap();
    assert_eq!(edits.len(), 1, "{result}");
    assert_eq!(edits[0]["range"], json!({"start": {"line": 0, "character": 13}, "end": {"line": 0, "character": 25}}));
    std::fs::remove_file(fixture.0.join("closed.lua")).unwrap();
    assert_eq!(
        client.request("textDocument/rename", params),
        Value::Null,
        "an unreadable indexed file must not result in partial edits"
    );
}

#[test]
fn formats_documents() {
    let mut client = Client::start(fixture_root());
    client.open_with(SHOP_CLIENT, "local   a=1\nif a   then\nprint( a )\nend\n");
    let params = json!({ "textDocument": { "uri": client.uri(SHOP_CLIENT) }, "options": { "tabSize": 2, "insertSpaces": true } });
    let edits = client.request("textDocument/formatting", params);
    assert_eq!(edits[0]["newText"], "local a = 1\nif a then\n  print(a)\nend\n");

    client.change(SHOP_CLIENT, 2, "local a = 1\n");
    let params = json!({ "textDocument": { "uri": client.uri(SHOP_CLIENT) }, "options": { "tabSize": 4, "insertSpaces": true } });
    assert_eq!(client.request("textDocument/formatting", params), json!([]));
}

#[test]
fn lua_ls_config_supplies_lint_settings_but_not_formatting() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-luarc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    std::fs::create_dir(&fixture.0).unwrap();
    std::fs::write(fixture.0.join("fxmanifest.lua"), "fx_version 'cerulean'\ngame 'gta5'\nclient_script 'main.lua'\n")
        .unwrap();
    std::fs::write(
        fixture.0.join(".luarc.json"),
        r#"{ "diagnostics.globals": ["Config"], "diagnostics.disable": ["lowercase-global"] }"#,
    )
    .unwrap();
    let text = "helper = function() return Config end\nif helper   then\nprint( helper )\nend\n";
    std::fs::write(fixture.0.join("main.lua"), text).unwrap();
    let mut client = Client::start(fixture.0.clone());
    client.open_with("main.lua", text);
    assert_eq!(client.diagnostics_for("main.lua"), []);
    let fallback =
        |logs: &[String]| logs.iter().filter(|l| l.contains("falling back") && l.contains(".luarc.json")).count();
    assert_eq!(fallback(&client.logs), 1, "{:?}", client.logs);

    let params =
        json!({ "textDocument": { "uri": client.uri("main.lua") }, "options": { "tabSize": 2, "insertSpaces": true } });
    let edits = client.request("textDocument/formatting", params);
    assert_eq!(
        edits[0]["newText"], "helper = function() return Config end\nif helper then\n  print(helper)\nend\n",
        "the editor's indentation applies without a qbxlint.toml"
    );

    let watchers: Vec<&str> = client
        .registrations
        .iter()
        .flat_map(|r| r["registerOptions"]["watchers"].as_array().into_iter().flatten())
        .filter_map(|w| w["globPattern"].as_str())
        .collect();
    assert!(watchers.contains(&"**/.luarc.json") && watchers.contains(&"**/.emmyrc.json"), "{watchers:?}");
    std::fs::write(fixture.0.join(".luarc.json"), r#"{ "diagnostics.globals": ["Config"] }"#).unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": client.uri(".luarc.json"), "type": 2 }] }),
    );
    assert_eq!(
        client.diagnostics_for("main.lua"),
        [("lowercase-global".to_string(), 0)],
        "a changed .luarc.json applies"
    );
    assert_eq!(fallback(&client.logs), 2, "a reloaded fallback is logged again: {:?}", client.logs);

    std::fs::write(fixture.0.join(".luarc.json"), r#"{ "diagnostics.globals": ["#).unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": client.uri(".luarc.json"), "type": 2 }] }),
    );
    let diagnostics = client.diagnostics_for("main.lua");
    assert!(diagnostics.iter().any(|(code, _)| code == "undefined-global"), "{diagnostics:?}");
    assert!(client.logs.iter().any(|l| l.starts_with("skipped") && l.contains(".luarc.json")), "{:?}", client.logs);
}

#[test]
fn excluded_files_stay_out_and_ignored_files_stay_quiet() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-ignore-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    let write = |relative: &str, text: &str| {
        let path = fixture.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("qbxlint.toml", "exclude = ['skip/**']\nignore_diagnostics = ['vendor/']\n");
    write(
        "fxmanifest.lua",
        "fx_version 'cerulean'\ngame 'gta5'\nclient_scripts { 'vendor/*.lua', 'skip/*.lua', 'main.lua' }\n",
    );
    write("vendor/lib.lua", "VendorApi = {}\nCitizen.Wait(0)\n");
    write("skip/old.lua", "SkippedApi = {}\nCitizen.Wait(0)\n");
    write("main.lua", "print(VendorApi, SkippedApi)\n");
    let mut client = Client::start(fixture.0.clone());
    let undefined = |client: &mut Client| {
        client.diagnostics_for("main.lua");
        let list = client.diagnostics[&client.uri("main.lua").to_string()].as_array().unwrap().clone();
        list.iter().map(|d| d["message"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    };

    let messages = undefined(&mut client);
    assert!(messages.len() == 1 && messages[0].contains("SkippedApi"), "{messages:?}");
    assert_eq!(client.diagnostics_for("vendor/lib.lua"), []);
    assert_eq!(client.diagnostics_for("skip/old.lua"), []);
    let symbols = client.request("workspace/symbol", json!({ "query": "VendorApi" }));
    assert!(!symbols.as_array().unwrap().is_empty(), "ignored files are still indexed: {symbols}");

    client.open("vendor/lib.lua");
    assert_eq!(client.diagnostics_for("vendor/lib.lua"), []);
    client.open("skip/old.lua");
    assert_eq!(client.diagnostics_for("skip/old.lua"), []);
    assert_eq!(undefined(&mut client).len(), 1, "an open excluded file is not indexed");
    client.notify("textDocument/didClose", json!({ "textDocument": { "uri": client.uri("skip/old.lua") } }));
    assert_eq!(client.diagnostics_for("skip/old.lua"), [], "a closed excluded file stays out of the Problems panel");
    assert_eq!(undefined(&mut client).len(), 1);

    write("skip/new.lua", "NewApi = {}\nCitizen.Wait(0)\n");
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": client.uri("skip/new.lua"), "type": 1 }] }),
    );
    assert_eq!(client.diagnostics_for("skip/new.lua"), []);
    let symbols = client.request("workspace/symbol", json!({ "query": "NewApi" }));
    assert!(symbols.as_array().unwrap().is_empty(), "{symbols}");
}

#[test]
fn overrides_give_scripts_a_loader_runs_a_side() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-side-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    let write = |relative: &str, text: &str| {
        let path = fixture.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    // Like loaf_wrapper: the manifest only ships the files, and load.lua runs them.
    write(
        "fxmanifest.lua",
        "fx_version 'cerulean'\ngame 'gta5'\nfiles { 'client/**.lua', 'server/**.lua' }\nshared_script 'load.lua'\n",
    );
    write("load.lua", "print('loader')\n");
    write("server/job.lua", "---@class (server) QBJob\n---@field label string\n");
    write("client/api.lua", "function ClientApi() end\n");
    write("client/main.lua", "---@type QBJob\nlocal job = { label = 1 }\nprint(job)\n");
    write("server/main.lua", "ClientApi()\n");
    let mut client = Client::start(fixture.0.clone());
    client.open("client/main.lua");
    client.open("server/main.lua");
    let codes = |client: &mut Client, file: &str| -> Vec<String> {
        client.diagnostics_for(file).into_iter().map(|(code, _)| code).collect()
    };
    assert_eq!(codes(&mut client, "client/main.lua"), ["assign-type-mismatch"], "an unknown side sees both");
    assert_eq!(codes(&mut client, "server/main.lua"), Vec::<String>::new());

    write(
        "qbxlint.toml",
        "[[overrides]]\nfiles = ['client/**']\nside = 'client'\n[[overrides]]\nfiles = ['server/**']\nside = 'server'\n",
    );
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": client.uri("qbxlint.toml"), "type": 1 }] }),
    );
    assert_eq!(codes(&mut client, "client/main.lua"), ["undefined-doc-name"]);
    let uri = client.uri("client/main.lua").to_string();
    assert_eq!(
        client.diagnostics[&uri][0]["message"],
        "Type `QBJob` only exists on the server, but this is a client script"
    );
    // client/api.lua is closed, so the changed configuration has to move it to the client too.
    assert_eq!(codes(&mut client, "server/main.lua"), ["undefined-global"]);
    let info = client.request("qbx/fileInfo", json!({ "uri": client.uri("client/api.lua") }));
    assert_eq!(info["side"], "client");
}

#[test]
fn server_cfg_start_order_settles_dependencies() {
    let mut client = Client::start(fixture_root());
    let late = client.diagnostics_for("late/server.lua");
    assert_eq!(late, [], "server.cfg ensures [core] before late, so mylib is already running");

    let uri = client.uri(SHOP_CLIENT).to_string();
    client.diagnostics_for(SHOP_CLIENT);
    let messages: Vec<String> = client.diagnostics[&uri]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == "manifest/missing-dependency")
        .map(|d| d["message"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(messages.len(), 1, "shop is ensured before [core]: {messages:?}");
    assert!(messages[0].contains("server.cfg does not start it earlier"), "{messages:?}");
}

#[test]
fn bridge_code_is_not_a_dependency_but_missing_resources_are_reported() {
    let mut client = Client::start(fixture_root());
    let bridge = client.diagnostics_for("late/bridge.lua");
    assert_eq!(bridge, [("fivem/resource-not-found".to_string(), 10)], "only the unconditional call matters");
    assert_eq!(client.diagnostics_for("late/guarded.lua"), [], "everything after the selector guard is optional");
}

#[test]
fn snippets_outrank_the_plain_name() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;
    let mut version = 1;
    let mut first_item = |typed: &str| {
        version += 1;
        client.change(CLIENT, version, &format!("{text}{typed}"));
        let params = client.position_params(CLIENT, line, typed.len() as u32);
        let result = client.request("textDocument/completion", params);
        let mut items = result["items"].as_array().cloned().unwrap_or_default();
        items.sort_by_key(|i| i["sortText"].as_str().unwrap_or_default().to_string());
        items.into_iter().next().unwrap_or(Value::Null)
    };

    let thread = first_item("CreateThread");
    assert_eq!(thread["labelDetails"]["description"], "snippet", "{thread}");
    let preview = thread["documentation"]["value"].as_str().unwrap();
    assert!(preview.contains("Wait(0)") && !preview.contains('$'), "{preview}");
    let body = thread["insertText"].as_str().unwrap();
    assert!(body.contains("while true do") && body.contains("Wait(${1:0})"), "{body}");

    let on_cache = first_item("oncache");
    let body = on_cache["insertText"].as_str().unwrap_or_default();
    assert!(body.starts_with("lib.onCache('${1|ped,"), "falls back to the usual keys without ox_lib: {on_cache}");

    let member = first_item("lib.onCa");
    assert!(member["insertText"].as_str().unwrap_or_default().starts_with("onCache('${1|"), "{member}");
}

#[test]
fn snippets_write_strings_in_the_configured_or_prevailing_quote() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let (Ok(root), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize()) {
                if root.parent() == Some(temp.as_path()) {
                    let _ = std::fs::remove_dir_all(root);
                }
            }
        }
    }
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "qbx-quotes-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    )));
    std::fs::create_dir(&fixture.0).unwrap();
    std::fs::write(fixture.0.join("fxmanifest.lua"), "fx_version 'cerulean'\ngame 'gta5'\nclient_script 'main.lua'\n")
        .unwrap();
    std::fs::write(fixture.0.join("main.lua"), "").unwrap();
    // The snippet labelled `label` when completing at the end of `text`.
    let snippet = |client: &mut Client, relative: &str, text: &str, label: &str| -> String {
        client.open_with(relative, text);
        let line = text.lines().count() as u32 - 1;
        let column = text.lines().last().unwrap().len() as u32;
        let result = client.request("textDocument/completion", client.position_params(relative, line, column));
        client.notify("textDocument/didClose", json!({ "textDocument": { "uri": client.uri(relative) } }));
        let items = result["items"].as_array().unwrap();
        let found = items.iter().find(|item| item["label"] == label && item["insertTextFormat"] == 2);
        found.and_then(|item| item["insertText"].as_str()).unwrap_or_else(|| panic!("{label}: {result}")).to_string()
    };

    // Without a `quote_style`, strings take the quote most of the document's strings use.
    let mut client = Client::start(fixture.0.clone());
    let body = snippet(&mut client, "main.lua", "local label = \"x\"\nRegisterNetEv", "RegisterNetEvent");
    assert!(body.starts_with("RegisterNetEvent(\"${1:resource}:${2:event}\""), "{body}");
    let body = snippet(&mut client, "main.lua", "local label = \"x\"\noncache", "onCache");
    assert!(body.starts_with("lib.onCache(\"${1|"), "{body}");
    assert!(snippet(&mut client, "main.lua", "RegisterNetEv", "RegisterNetEvent").starts_with("RegisterNetEvent('"));
    client.open_with("main.lua", "local label = \"x\"\n");
    let quote = client.request("qbx/quote", json!({ "uri": client.uri("main.lua") }));
    assert_eq!(quote, "\"");
    assert_eq!(client.request("qbx/quote", Value::Null), "'", "no document and no quote_style");
    drop(client);

    // `quote_style` decides over the strings of the document.
    std::fs::write(fixture.0.join("qbxlint.toml"), "[format]\nquote_style = \"double\"\n").unwrap();
    let mut client = Client::start(fixture.0.clone());
    let body = snippet(&mut client, "main.lua", "local label = 'x'\nlib.onCa", "onCache");
    assert!(body.starts_with("onCache(\"${1|"), "{body}");
    let body = snippet(&mut client, "fxmanifest.lua", "fx_v", "fx_version");
    assert_eq!(body, "fx_version \"${1|cerulean,bodacious,adamant|}\"");
    assert_eq!(snippet(&mut client, "fxmanifest.lua", "lua5", "lua54"), "lua54 \"yes\"");
    assert_eq!(client.request("qbx/quote", Value::Null), "\"");
    let snippets = client.request("qbx/snippets", Value::Null);
    let bodies: Vec<&str> = snippets.as_array().unwrap().iter().map(|s| s["body"].as_str().unwrap()).collect();
    assert!(bodies.iter().any(|body| body.starts_with("AddEventHandler(\"")), "{bodies:?}");
    assert!(bodies.iter().all(|body| !body.contains('\'')), "{bodies:?}");
}

#[test]
fn call_snippets_write_out_the_callbacks_a_function_takes() {
    let mut client = Client::start(fixture_root());
    let defs = "\
---@param event string
---@param cb fun(...)
---@param ... any
function TriggerCallback(event, cb, ...) end

---@param ms integer
---@param cb fun(elapsed: number, late: boolean)
---@param label? string
local function after(ms, cb, label) end

---@class Poller
local Poller = {}

---@param cb fun()
function Poller:every(cb) end

---@param name string
function PlainGreeting(name) end
";
    // `|` marks the cursor.
    let mut snippets = |typed: &str, label: &str| -> Vec<String> {
        let text = format!("{defs}{}", typed.replace('|', ""));
        let (line, column) = pos(&format!("{defs}{typed}"), "|", 0);
        client.open_with(CLIENT, &text);
        let result = client.request("textDocument/completion", client.position_params(CLIENT, line, column));
        let items = result["items"].as_array().cloned().unwrap_or_default();
        assert!(items.iter().any(|item| item["label"] == label), "{typed}: no plain {label} in {result}");
        items
            .iter()
            .filter(|item| item["label"] == label && item["labelDetails"]["description"] == "snippet")
            .map(|item| item["insertText"].as_str().unwrap_or_default().to_string())
            .collect()
    };

    let cases = [
        ("TriggerCall|", "TriggerCallback", "TriggerCallback('${1:event}', function(${2:...})\n\t$0\nend$3)"),
        ("aft|", "after", "after(${1:ms}, function(${2:elapsed, late})\n\t$0\nend$3)"),
        ("Poller:ev|", "every", "every(function()\n\t$0\nend)"),
        // The runtime stub names the callback's parameters.
        (
            "RegisterNuiCall|",
            "RegisterNuiCallback",
            "RegisterNuiCallback('${1:name}', function(${2:data, cb})\n\t$0\nend)",
        ),
        // Native data only says `function`.
        (
            "AddStateBagChange|",
            "AddStateBagChangeHandler",
            "AddStateBagChangeHandler('${1:keyFilter}', '${2:bagFilter}', function(${3})\n\t$0\nend)",
        ),
    ];
    for (typed, label, expected) in cases {
        assert_eq!(snippets(typed, label), [expected], "{typed}");
    }
    for (typed, label) in [
        // Only functions that take a callback get one.
        ("PlainGree|", "PlainGreeting"),
        // A `(` already follows the name.
        ("TriggerCall|()", "TriggerCallback"),
        ("function TriggerCall|", "TriggerCallback"),
        // A dot call of a `:` method would have to pass `self` itself.
        ("Poller.ev|", "every"),
    ] {
        assert!(snippets(typed, label).is_empty(), "{typed}");
    }
    // `CreateThread` keeps its hand-written snippet and gets no second one.
    let thread = snippets("CreateThre|", "CreateThread");
    assert!(thread.len() == 1 && thread[0].contains("while true do"), "{thread:?}");
}

#[test]
fn string_arguments_list_the_literals_their_signatures_take() {
    let mut client = Client::start(fixture_root());
    let defs = "\
---@alias Actions
---| \"playerLoaded\"
---| \"playerUnloaded\"
---| \"keyPressed\"
---| string

---@param action Actions
---@param handler fun(...)
---@return number id
---@overload fun(action: \"keyPressed\", handler: fun(key: string)): number
---@overload fun(action: \"jobUpdated\", handler: fun(job: table)): number
---@overload (server) fun(action: \"serverOnly\", handler: fun(source: number)): number
function OnAction(action, handler) end

---@class Emitter
Emitter = {}

---@param event \"open\"|\"close\"
---@param mode \"once\"|\"always\"
function Emitter:on(event, mode) end

---@param name string
function PlainGreeting(name) end
";
    client.open_with("myresource/shared/config.lua", defs);
    // `|` marks the cursor. Items come back in the order their `sortText` lists them.
    let mut literals = |file: &str, typed: &str| -> Vec<Value> {
        let (line, column) = pos(typed, "|", 0);
        client.open_with(file, &typed.replace('|', ""));
        let result = client.request("textDocument/completion", client.position_params(file, line, column));
        let mut items = result["items"].as_array().cloned().unwrap_or_default();
        items.sort_by_key(|item| item["sortText"].as_str().unwrap_or_default().to_string());
        items
    };
    let labels = |items: &[Value]| -> Vec<String> {
        items.iter().map(|item| item["label"].as_str().unwrap_or_default().to_string()).collect()
    };

    // The alias lists its values in order, then the overloads for this side add theirs.
    let items = literals(CLIENT, "OnAction('|')");
    assert_eq!(labels(&items), ["playerLoaded", "playerUnloaded", "keyPressed", "jobUpdated"]);
    assert_eq!(items[0]["detail"], "Actions");
    assert_eq!(items[0]["textEdit"]["range"]["start"]["character"], 10);
    assert_eq!(items[0]["textEdit"]["range"]["end"]["character"], 10);
    // A value names the overloads that take it alone.
    let documentation = items[2]["documentation"]["value"].as_str().unwrap_or_default();
    assert!(
        documentation.contains("OnAction(action: \"keyPressed\", handler: fun(key: string)): number"),
        "{documentation}"
    );
    assert!(items[0].get("documentation").is_none(), "{}", items[0]);

    let items = literals(SERVER, "OnAction(\"play|\")");
    assert_eq!(labels(&items), ["playerLoaded", "playerUnloaded", "keyPressed", "jobUpdated", "serverOnly"]);
    // The whole content is replaced, so accepting does not keep the typed prefix twice.
    assert_eq!(items[0]["textEdit"]["range"]["start"]["character"], 10);
    assert_eq!(items[0]["textEdit"]["range"]["end"]["character"], 14);

    let emitter = "local emitter = Emitter\n";
    assert_eq!(labels(&literals(CLIENT, &format!("{emitter}emitter:on('|')"))), ["open", "close"]);
    assert_eq!(labels(&literals(CLIENT, &format!("{emitter}emitter:on('open', '|')"))), ["once", "always"]);
    assert!(literals(CLIENT, "PlainGreeting('|')").is_empty());
}

#[test]
fn arguments_that_take_a_function_offer_it_as_a_snippet() {
    let mut client = Client::start(fixture_root());
    let defs = "\
---@alias Actions \"playerUnloaded\"|\"jobUpdated\"|string

---@param action Actions
---@param handler fun(...)
---@return number id
---@overload fun(action: \"playerUnloaded\", handler: fun(source: number)): number
---@overload fun(action: \"jobUpdated\", handler: fun(job: table, oldJob?: table)): number
---@overload fun(action: \"jobUpdated\", handler: fun(source: number, job: table)): number
---@overload (server) fun(action: \"serverOnly\", handler: fun(source: number, reason: string)): number
function OnAction(action, handler) end

---@class Poller
Poller = {}

---@param cb fun()
function Poller:every(cb) end

---@param ms integer
---@param cb function
function Later(ms, cb) end
";
    client.open_with("myresource/shared/config.lua", defs);
    // `|` marks the cursor; `trigger` is the character typed to ask for completions.
    let mut snippets = |file: &str, typed: &str, trigger: Option<&str>| -> Option<Vec<(String, String)>> {
        let (line, column) = pos(typed, "|", 0);
        client.open_with(file, &typed.replace('|', ""));
        let mut params = client.position_params(file, line, column);
        if let Some(trigger) = trigger {
            params["context"] = json!({ "triggerKind": 2, "triggerCharacter": trigger });
        }
        let result = client.request("textDocument/completion", params);
        let items = result["items"].as_array()?;
        Some(
            items
                .iter()
                .filter(|item| item["label"].as_str().is_some_and(|label| label.starts_with("function(")))
                .map(|item| {
                    (item["label"].as_str().unwrap().to_string(), item["insertText"].as_str().unwrap().to_string())
                })
                .collect(),
        )
    };
    let one = |label: &str, body: &str| Some(vec![(label.to_string(), body.to_string())]);

    // The overload that the value before the cursor picks gives the parameters, and a `,` right
    // before the cursor gets a space.
    assert_eq!(
        snippets(CLIENT, "OnAction('playerUnloaded',|)", Some(",")),
        one("function(source)", " function(${1:source})\n\t$0\nend")
    );
    assert_eq!(
        snippets(CLIENT, "OnAction('playerUnloaded', |)", None),
        one("function(source)", "function(${1:source})\n\t$0\nend")
    );
    assert_eq!(
        snippets(CLIENT, "local id = OnAction('playerUnloaded', fun|)", None),
        one("function(source)", "function(${1:source})\n\t$0\nend")
    );
    // Overloads that fit equally well each offer theirs.
    assert_eq!(
        snippets(CLIENT, "OnAction('jobUpdated',|", Some(",")),
        Some(vec![
            ("function(job, oldJob)".to_string(), " function(${1:job, oldJob})\n\t$0\nend".to_string()),
            ("function(source, job)".to_string(), " function(${1:source, job})\n\t$0\nend".to_string()),
        ])
    );
    // A value no overload lists keeps the declared handler, and a server overload applies on its side.
    assert_eq!(
        snippets(CLIENT, "OnAction('serverOnly', |)", None),
        one("function(...)", "function(${1:...})\n\t$0\nend")
    );
    assert_eq!(
        snippets(SERVER, "OnAction('serverOnly', |)", None),
        one("function(source, reason)", "function(${1:source, reason})\n\t$0\nend")
    );
    assert_eq!(
        snippets(CLIENT, "local poller = Poller\npoller:every(|)", None),
        one("function()", "function()\n\t$0\nend")
    );
    assert_eq!(snippets(CLIENT, "Later(100,|)", Some(",")), one("function()", " function($1)\n\t$0\nend"));

    // A `,` asks for nothing else, and nothing where no argument takes a function.
    for typed in [
        "local t = { a = 1,|}",
        "OnAction('playerUnloaded', { 1,|})",
        "OnAction('a,|')",
        "-- OnAction('playerUnloaded',|)",
        "print(1,|)",
        "local a,|",
        // The argument after the cursor is already written.
        "OnAction('playerUnloaded',| handler)",
    ] {
        let found = snippets(CLIENT, typed, Some(","));
        assert!(found.is_none(), "{typed}: {found:?}");
    }
}

#[test]
fn arguments_without_quotes_offer_the_values_their_parameter_lists() {
    let mut client = Client::start(fixture_root());
    let defs = "\
---@alias Actions \"playerLoaded\"|\"playerUnloaded\"|string

---@param action Actions
---@param handler fun(...)
---@overload fun(action: \"playerUnloaded\", handler: fun(source: number))
function OnAction(action, handler) end

---@class Emitter
Emitter = {}

---@param event \"open\"|\"close\"
---@param mode \"once\"|\"always\"
function Emitter:on(event, mode) end
";
    client.open_with("myresource/shared/config.lua", defs);
    // `|` marks the cursor; `trigger` is the character typed to ask for completions. Items come back
    // in the order their `sortText` lists them, as label and inserted text.
    let mut values = |typed: &str, trigger: Option<&str>| -> Option<Vec<(String, String)>> {
        let (line, column) = pos(typed, "|", 0);
        client.open_with(CLIENT, &typed.replace('|', ""));
        let mut params = client.position_params(CLIENT, line, column);
        if let Some(trigger) = trigger {
            params["context"] = json!({ "triggerKind": 2, "triggerCharacter": trigger });
        }
        let result = client.request("textDocument/completion", params);
        let mut items = result["items"].as_array()?.clone();
        items.sort_by_key(|item| item["sortText"].as_str().unwrap_or_default().to_string());
        Some(
            items
                .iter()
                .filter(|item| item["kind"] == 20)
                .map(|item| {
                    let label = item["label"].as_str().unwrap().to_string();
                    (label.clone(), item["insertText"].as_str().map_or(label, str::to_string))
                })
                .collect(),
        )
    };
    let pairs = |pairs: &[(&str, &str)]| -> Option<Vec<(String, String)>> {
        Some(pairs.iter().map(|(label, text)| (label.to_string(), text.to_string())).collect())
    };

    // Typing the `(` lists the values quoted, as does asking without it.
    let quoted = pairs(&[("'playerLoaded'", "'playerLoaded'"), ("'playerUnloaded'", "'playerUnloaded'")]);
    assert_eq!(values("OnAction(|)", Some("(")), quoted);
    assert_eq!(values("OnAction(|", None), quoted);
    // Strings take the quote most of the document's strings use.
    assert_eq!(
        values("local label = \"x\"\nOnAction(|)", None),
        pairs(&[("\"playerLoaded\"", "\"playerLoaded\""), ("\"playerUnloaded\"", "\"playerUnloaded\"")])
    );
    // After a `,`, the values of the next parameter, with a space.
    assert_eq!(
        values("local emitter = Emitter\nemitter:on('open',|)", Some(",")),
        pairs(&[("'once'", " 'once'"), ("'always'", " 'always'")])
    );
    // A handler lists no values.
    assert_eq!(values("OnAction('playerUnloaded',|)", Some(",")), Some(Vec::new()));
    // A typed `(` asks for nothing where no call's first argument lists values.
    for typed in ["if (|", "print(|", "function Handle(|", "local value = (|", "OnAction((|"] {
        let found = values(typed, Some("("));
        assert!(found.is_none(), "{typed}: {found:?}");
    }
    // A typed word filters them by the value, beside the names in scope.
    let (line, column) = pos("OnAction(pl|)", "|", 0);
    client.open_with(CLIENT, "OnAction(pl)");
    let result = client.request("textDocument/completion", client.position_params(CLIENT, line, column));
    let loaded = result["items"].as_array().unwrap().iter().find(|item| item["label"] == "'playerLoaded'");
    assert_eq!(loaded.map(|item| &item["filterText"]), Some(&json!("playerLoaded")), "{result}");
}

#[test]
fn value_lines_list_more_values_of_params_returns_types_and_fields() {
    let mut client = Client::start(fixture_root());
    let defs = "\
---Runs the job.
---@param mode string
---| 'fast'
---| 'slow'
---@param level? integer
---| 1
---| 2
function RunJob(mode, level) end

---@class Opts
---@field kind string
---| 'car'
---| 'boat'
";
    client.open_with("myresource/shared/config.lua", defs);
    // The values offered where `$` is, in the order they are listed.
    let mut values = |typed: &str| -> Vec<String> {
        let (line, column) = pos(typed, "$", 0);
        client.open_with(CLIENT, &typed.replace('$', ""));
        let result = client.request("textDocument/completion", client.position_params(CLIENT, line, column));
        let mut items = result["items"].as_array().cloned().unwrap_or_default();
        items.retain(|item| item["kind"] == 20 || item["kind"] == 14);
        items.sort_by_key(|item| item["sortText"].as_str().unwrap_or_default().to_string());
        items.iter().map(|item| item["label"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(values("RunJob($"), ["'fast'", "'slow'"]);
    assert_eq!(values("RunJob('fast', $)"), ["1", "2"]);
    assert_eq!(values("---@type string\n---| 'a'\n---| 'b'\nlocal letter = $"), ["'a'", "'b'"]);
    assert_eq!(values("---@return string\n---| 'ok'\nlocal function status()\n\treturn $\nend"), ["'ok'"]);
    assert_eq!(values("---@type Opts\nlocal opts = { kind = $ }"), ["'car'", "'boat'"]);

    let text = "RunJob('fast')";
    client.open_with(CLIENT, text);
    let hover = client.hover_text(CLIENT, 0, 1);
    assert!(hover.contains("RunJob(mode: string|\"fast\"|\"slow\", level?: integer|1|2)"), "{hover}");
    assert!(hover.contains("Runs the job.") && !hover.contains("'fast'"), "{hover}");
}

#[test]
fn value_lines_describe_their_values_in_completion_and_hover() {
    let mut client = Client::start(fixture_root());
    let defs = "\
---@alias Speed
---| 'fast' # goes quickly
---| 'slow' # takes its time

---@param speed Speed
---@param mode string
---| 'once' # runs one time
---| 'loop'
---@return boolean
---| nil # when it did not start
function StartTask(speed, mode) end

---@param target any
---| 'self' # the caller
---| 'all'
function Notify(target) end

---@return
---| 'on' # switched on
---| 'off' # switched off
function GetSwitch() end

---@class Test.Lamp
---@field color
---| 'red' # warm light
---| 'blue'
Lamp = {}
";
    client.open_with("myresource/shared/config.lua", defs);
    // The values offered where `$` is, with their documentation.
    let mut documented = |typed: &str| -> Vec<(String, String)> {
        let (line, column) = pos(typed, "$", 0);
        client.open_with(CLIENT, &typed.replace('$', ""));
        let result = client.request("textDocument/completion", client.position_params(CLIENT, line, column));
        let mut items = result["items"].as_array().cloned().unwrap_or_default();
        items.retain(|item| item["kind"] == 20);
        items.sort_by_key(|item| item["sortText"].as_str().unwrap_or_default().to_string());
        let doc = |item: &Value| item["documentation"]["value"].as_str().unwrap_or_default().to_string();
        items.iter().map(|item| (item["label"].as_str().unwrap().to_string(), doc(item))).collect()
    };
    let pairs = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs.iter().map(|(label, doc)| (label.to_string(), doc.to_string())).collect()
    };
    let speeds = pairs(&[("'fast'", "goes quickly"), ("'slow'", "takes its time")]);
    assert_eq!(documented("StartTask($)"), speeds);
    assert_eq!(documented("StartTask('fast', $)"), pairs(&[("'once'", "runs one time"), ("'loop'", "")]));
    assert_eq!(documented("---@type Speed\nlocal speed = $"), speeds);
    // `any` keeps none of the values listed under it, which are offered all the same.
    assert_eq!(documented("Notify($)"), pairs(&[("'self'", "the caller"), ("'all'", "")]));
    assert_eq!(documented("Notify('$')"), pairs(&[("self", "the caller"), ("all", "")]));
    // The values listed under `@return`, `@type` and `@field`, where a value of them is written.
    let switches = pairs(&[("'on'", "switched on"), ("'off'", "switched off")]);
    assert_eq!(documented("print(GetSwitch() == $)"), switches);
    assert_eq!(documented("local switch = GetSwitch()\nprint(switch == $)"), switches);
    let directions = pairs(&[("'up'", "going up"), ("'down'", "")]);
    assert_eq!(documented("---@type\n---| 'up' # going up\n---| 'down'\nlocal direction = $"), directions);
    let answer = "---@return\n---| 'yes' # agreed\n---| 'no'\nlocal function answer()\n    return $\nend";
    assert_eq!(documented(answer), pairs(&[("'yes'", "agreed"), ("'no'", "")]));
    let colors = pairs(&[("'red'", "warm light"), ("'blue'", "")]);
    assert_eq!(documented("---@type Test.Lamp\nlocal lamp = { color = $ }"), colors);
    assert_eq!(documented("---@type Test.Lamp\nlocal lamp = Lamp\nprint(lamp.color == $)"), colors);

    let text = "StartTask('fast', 'once')\n---@type Speed\nlocal speed = 'fast'\n";
    client.open_with(CLIENT, text);
    let hover = client.hover_text(CLIENT, 0, 1);
    let listed = "```lua\nmode:\n    | \"once\" -- runs one time\n    | \"loop\"\n\nreturn #1:\n    | nil -- when it did not start\n```";
    assert!(hover.contains(listed), "{hover}");
    let (l, c) = pos(text, "speed =", 0);
    let hover = client.hover_text(CLIENT, l, c);
    assert!(
        hover.contains("type Speed =\n    | \"fast\" -- goes quickly\n    | \"slow\" -- takes its time"),
        "{hover}"
    );
}

#[test]
fn values_offer_what_their_declared_type_lists() {
    let mut client = Client::start(fixture_root());
    let defs = "\
---@alias State \"busy\" | \"working\" | \"ready\"
---@alias Level 1 | 2 | 3
---@alias Volume \"mute\" | 0 | 100

---@class Machine
---@field state State
---@field mode \"auto\"|\"manual\"
---@field fallback? State
---@field label string
---@field level Level
---@field enabled boolean
---@field onStop? fun(reason: string)

---@alias Handler fun(id: integer, name: string)

---@param level Level
---@param volume? Volume
function SetLevel(level, volume) end

---@param enabled boolean
---@param mode? \"auto\"|\"manual\"|false
function SetEnabled(enabled, mode) end

---@param mode \"auto\"|nil
---@overload fun(mode: \"manual\")
function SetMode(mode) end
";
    client.open_with("myresource/shared/config.lua", defs);
    // `|` marks the cursor; `trigger` is the character typed to ask for completions. Items come back
    // in the order their `sortText` lists them, as label and inserted text.
    let mut values = |typed: &str, trigger: Option<&str>| -> Option<Vec<(String, String)>> {
        let (line, column) = pos(typed, "|", 0);
        client.open_with(CLIENT, &typed.replace('|', ""));
        let mut params = client.position_params(CLIENT, line, column);
        if let Some(trigger) = trigger {
            params["context"] = json!({ "triggerKind": 2, "triggerCharacter": trigger });
        }
        let result = client.request("textDocument/completion", params);
        let mut items = result["items"].as_array()?.clone();
        items.sort_by_key(|item| item["sortText"].as_str().unwrap_or_default().to_string());
        Some(
            items
                .iter()
                // The listed values are numbered, and a function literal sorts ahead of them.
                .filter(|item| {
                    let sort = item["sortText"].as_str().unwrap_or_default();
                    (sort.len() == 4 && sort.bytes().all(|b| b.is_ascii_digit())) || sort.starts_with(".function(")
                })
                .map(|item| {
                    let label = item["label"].as_str().unwrap().to_string();
                    let text = item["insertText"].as_str().or(item["textEdit"]["newText"].as_str());
                    (label.clone(), text.map_or(label, str::to_string))
                })
                .collect(),
        )
    };
    let quoted = |values: &[&str], space: &str| -> Option<Vec<(String, String)>> {
        Some(values.iter().map(|value| (format!("'{value}'"), format!("{space}'{value}'"))).collect())
    };
    let plain = |values: &[&str]| -> Option<Vec<(String, String)>> {
        Some(values.iter().map(|value| (value.to_string(), value.to_string())).collect())
    };
    let pairs = |pairs: &[(&str, &str)]| -> Option<Vec<(String, String)>> {
        Some(pairs.iter().map(|(label, text)| (label.to_string(), text.to_string())).collect())
    };
    let states = ["busy", "working", "ready"];
    let state = "---@type State\nlocal state = 'busy'\n";
    let machine = "---@type Machine\nlocal machine = GetMachine()\n";

    // A `---@type` above a `local` lists the values when the space after the `=` is typed, and when
    // asked, with a space when asked right after the `=`.
    assert_eq!(values("---@type State\nlocal state =|", None), quoted(&states, " "));
    assert_eq!(values("---@type State\nlocal state = |", Some(" ")), quoted(&states, ""));
    assert_eq!(values("---@type State\nlocal state = |", None), quoted(&states, ""));
    // Strings take the quote most of the document's strings use.
    assert_eq!(
        values("local label = \"x\"\n---@type Machine\nlocal machine = { mode = | }", None),
        Some(vec![
            ("\"auto\"".to_string(), "\"auto\"".to_string()),
            ("\"manual\"".to_string(), "\"manual\"".to_string())
        ])
    );
    // An assignment takes what its target is declared as: a local that is assigned again keeps
    // its annotation, and a field has the type of its `@field`.
    assert_eq!(values(&format!("{state}state = |"), Some(" ")), quoted(&states, ""));
    assert_eq!(values(&format!("{state}if state == 'busy' then\n\tstate = |\nend"), None), quoted(&states, ""));
    assert_eq!(values(&format!("{state}state = 'ready'\nif state == |"), None), quoted(&states, ""));
    assert_eq!(values(&format!("{machine}machine.state = |"), Some(" ")), quoted(&states, ""));
    assert_eq!(values(&format!("{machine}machine.mode =|"), None), quoted(&["auto", "manual"], " "));
    assert_eq!(values("---@type State\nCurrent = |", None), quoted(&states, ""));
    // A table typed as a class takes them for the field being set.
    assert_eq!(values("---@type Machine\nlocal machine = { state = | }", Some(" ")), quoted(&states, ""));
    assert_eq!(
        values("---@type Machine\nlocal machine = {\n\tstate = 'busy',\n\tmode = |\n}", None),
        quoted(&["auto", "manual"], "")
    );
    // A `return` takes the `@return` of its function.
    let returns = "---@return State\nlocal function current()\n\treturn |\nend";
    assert_eq!(values(returns, Some(" ")), quoted(&states, ""));
    // A comparison takes what its other side is declared as.
    let handle = "---@param state State\nlocal function handle(state)\n\t";
    assert_eq!(values(&format!("{handle}if state == | then\n\tend\nend"), Some(" ")), quoted(&states, ""));
    assert_eq!(values(&format!("{handle}if state ~=| then\n\tend\nend"), None), quoted(&states, " "));
    assert_eq!(values(&format!("{handle}return state == |\nend"), None), quoted(&states, ""));
    // An `elseif` leaves out what the conditions before it ruled out.
    assert_eq!(
        values(&format!("{handle}if state == 'busy' then\n\telseif state == | then\n\tend\nend"), None),
        quoted(&["working", "ready"], "")
    );
    assert_eq!(values(&format!("{machine}if machine.state == |"), Some(" ")), quoted(&states, ""));
    assert_eq!(values(&format!("{state}if ready and state == |"), None), quoted(&states, ""));
    // The statement on the next line is no value of the `=` that is still without one.
    assert_eq!(values(&format!("{state}state = |\nprint(state)"), None), quoted(&states, ""));
    // Inside a string, the values replace its contents.
    assert_eq!(values("---@type State\nlocal state = '|'", None), plain(&states));
    assert_eq!(values("---@type State\nlocal state = '|", Some("'")), plain(&states));
    assert_eq!(values(&format!("{state}state = \"bu|\""), None), plain(&states));
    assert_eq!(values("---@type Machine\nlocal machine = { state = '|' }", None), plain(&states));
    assert_eq!(values(&format!("{handle}if state == '|' then\n\tend\nend"), None), plain(&states));
    assert_eq!(values(&format!("{handle}if 'busy|' ~= state then\n\tend\nend"), None), plain(&states));
    assert_eq!(values("---@return State\nlocal function current()\n\treturn '|'\nend", None), plain(&states));

    // Integers are written as they are, in the order the type lists them beside its strings, and
    // stay out of a string.
    let levels = plain(&["1", "2", "3"]);
    assert_eq!(values("---@type Level\nlocal level = |", Some(" ")), levels);
    assert_eq!(values("---@type Level\nlocal level =|", None), pairs(&[("1", " 1"), ("2", " 2"), ("3", " 3")]));
    assert_eq!(values(&format!("{machine}machine.level = |"), None), levels);
    assert_eq!(values(&format!("{machine}if machine.level ~= | then\nend"), Some(" ")), levels);
    assert_eq!(values("---@return Level\nlocal function current()\n\treturn |\nend", None), levels);
    let volumes = pairs(&[("'mute'", "'mute'"), ("0", "0"), ("100", "100")]);
    assert_eq!(values("---@type Volume\nlocal volume = |", None), volumes);
    assert_eq!(values("---@type Volume\nlocal volume = '|'", None), plain(&["mute"]));
    assert_eq!(values("---@type Level\nlocal level = '|'", None), Some(Vec::new()));
    // So are those of an argument.
    assert_eq!(values("SetLevel(|)", Some("(")), levels);
    assert_eq!(values("SetLevel(1,|)", Some(",")), pairs(&[("'mute'", " 'mute'"), ("0", " 0"), ("100", " 100")]));
    assert_eq!(values("SetLevel('|')", None), Some(Vec::new()));
    assert_eq!(values("SetLevel(1, '|')", None), plain(&["mute"]));

    // A `boolean` lists `true` and `false`, and a type that lists values also lists the `nil` it
    // allows, last.
    let booleans = plain(&["true", "false"]);
    assert_eq!(values("---@type boolean\nlocal enabled = |", Some(" ")), booleans);
    assert_eq!(values("---@type boolean\nlocal enabled =|", None), pairs(&[("true", " true"), ("false", " false")]));
    assert_eq!(values("---@type boolean?\nlocal enabled = |", None), plain(&["true", "false", "nil"]));
    assert_eq!(values(&format!("{machine}machine.enabled = |"), Some(" ")), booleans);
    assert_eq!(values(&format!("{machine}if machine.enabled == | then\nend"), Some(" ")), booleans);
    assert_eq!(values("---@return boolean\nlocal function ok()\n\treturn |\nend", Some(" ")), booleans);
    let optional_states =
        pairs(&[("'busy'", "'busy'"), ("'working'", "'working'"), ("'ready'", "'ready'"), ("nil", "nil")]);
    assert_eq!(values("---@type State?\nlocal state = |", Some(" ")), optional_states);
    assert_eq!(
        values("---@param state? State\nlocal function handle(state)\n\tif state ~= | then\n\tend\nend", None),
        optional_states
    );
    assert_eq!(values(&format!("{machine}machine.fallback = |"), None), optional_states);
    // Also after the values of other signatures.
    assert_eq!(
        values("SetMode(|)", Some("(")),
        pairs(&[("'auto'", "'auto'"), ("'manual'", "'manual'"), ("nil", "nil")])
    );
    assert_eq!(values("---@type boolean\nlocal enabled = '|'", None), Some(Vec::new()));
    // A typed `(` or `,` opens the list for strings and integers, not for booleans alone, which
    // are listed when asked for.
    assert_eq!(values("SetEnabled(|)", Some("(")), None);
    assert_eq!(values("SetEnabled(|)", None), booleans);
    assert_eq!(
        values("SetEnabled(true,|)", Some(",")),
        pairs(&[("'auto'", " 'auto'"), ("'manual'", " 'manual'"), ("false", " false")])
    );

    // A value that is stored or returned as a function gets a function literal with the
    // parameters its type names, but one that is compared with a function does not.
    let on_stop = pairs(&[("function(reason)", "function(${1:reason})\n\t$0\nend")]);
    assert_eq!(values("---@type Machine\nlocal machine = { onStop = | }", Some(" ")), on_stop);
    assert_eq!(values(&format!("{machine}machine.onStop = |"), None), on_stop);
    assert_eq!(
        values("---@type Handler\nlocal handler =|", None),
        pairs(&[("function(id, name)", " function(${1:id, name})\n\t$0\nend")])
    );
    assert_eq!(
        values("---@return fun()\nlocal function make()\n\treturn |\nend", Some(" ")),
        pairs(&[("function()", "function()\n\t$0\nend")])
    );
    assert_eq!(values("---@type function\nlocal run = |", None), pairs(&[("function()", "function($1)\n\t$0\nend")]));
    assert_eq!(values(&format!("{machine}if machine.onStop == | then\nend"), Some(" ")), None);

    // A typed space asks for nothing else, and for nothing where no type lists values.
    for (typed, trigger) in [
        ("local state = |", " "),
        ("local |", " "),
        ("print(1, |)", " "),
        ("---@type string\nlocal label = |", " "),
        // `nil` alone tells that a value may be missing, not what it can be.
        ("---@type string?\nlocal label = |", " "),
        ("---@type Machine\nlocal machine = { label = | }", " "),
        ("---@type State\nlocal state = 'busy'\nlocal ready = state >= |", " "),
        ("---@type State\nlocal state = 'busy' -- state = |", " "),
        ("---@type State\nlocal state = 'a = |'", " "),
        // The value after the cursor is already written.
        ("---@type State\nlocal state = | 'busy'", " "),
        ("---@type boolean\nlocal enabled = |other", " "),
        ("---@type boolean\nlocal enabled = |123", " "),
        ("---@type boolean\nlocal enabled = |true", " "),
        ("---@type State\nlocal state = 'busy'\nif state == |other then end", " "),
        // What the code assigns tells what a local holds today, not what it may.
        ("local mode = 'dev'\nmode = |", " "),
    ] {
        let found = values(typed, Some(trigger));
        assert!(found.is_none(), "{typed}: {found:?}");
    }
    // Nor are they in front of the rest of a word, which a value would run into.
    assert_eq!(values("---@type boolean\nlocal enabled = |other", None), None);
    assert_eq!(values("---@type boolean\nlocal enabled = t|other", None), Some(Vec::new()));
    assert_eq!(values("---@type boolean\nlocal enabled = t|", None), plain(&["true", "false"]));
    // What a typed space lists is incomplete, so the client asks again once a word is typed, and
    // gets the names in scope beside the values.
    let (line, column) = pos("---@type boolean\nlocal enabled = |", "|", 0);
    client.open_with(CLIENT, "---@type boolean\nlocal enabled = ");
    let mut params = client.position_params(CLIENT, line, column);
    params["context"] = json!({ "triggerKind": 2, "triggerCharacter": " " });
    let result = client.request("textDocument/completion", params);
    assert_eq!(result["isIncomplete"], json!(true), "{result}");

    // A typed word filters them by the value, beside the names in scope.
    let (line, column) = pos("local bucket = 1\n---@type State\nlocal state = bu|", "|", 0);
    client.open_with(CLIENT, "local bucket = 1\n---@type State\nlocal state = bu");
    let result = client.request("textDocument/completion", client.position_params(CLIENT, line, column));
    let items = result["items"].as_array().unwrap();
    let busy = items.iter().find(|item| item["label"] == "'busy'");
    assert_eq!(busy.map(|item| &item["filterText"]), Some(&json!("busy")), "{result}");
    assert_eq!(busy.map(|item| &item["detail"]), Some(&json!("State")), "{result}");
    assert!(items.iter().any(|item| item["label"] == "bucket"), "{result}");
    // `true` is listed once, as the value, and not again as a keyword.
    let (line, column) = pos("---@type boolean\nlocal enabled = t|", "|", 0);
    client.open_with(CLIENT, "---@type boolean\nlocal enabled = t");
    let result = client.request("textDocument/completion", client.position_params(CLIENT, line, column));
    let listed: Vec<&Value> =
        result["items"].as_array().unwrap().iter().filter(|item| item["label"] == "true").collect();
    assert_eq!(listed.len(), 1, "{result}");
    assert_eq!(listed[0]["sortText"], "0000", "{result}");
    assert_eq!(listed[0]["detail"], "boolean", "{result}");

    // A call snippet writes quotes only around a stop whose parameter lists strings.
    let (line, column) = pos("SetLev|", "|", 0);
    client.open_with(CLIENT, "SetLev");
    let result = client.request("textDocument/completion", client.position_params(CLIENT, line, column));
    let items = result["items"].as_array().unwrap();
    let snippets: Vec<&Value> =
        items.iter().filter(|item| item["label"] == "SetLevel" && item["insertTextFormat"] == 2).collect();
    assert!(snippets.is_empty(), "{snippets:?}");
}

#[test]
fn enum_members_come_before_their_values_where_the_table_is_reachable() {
    let mut client = Client::start(fixture_root());
    let defs = "\
---@enum Color
Colors = { Red = 1, Green = 2, ['Dark Blue'] = 3 }

---@enum (key) Key
Keys = { Alpha = 1, Beta = 2 }

---@enum (server) Job
Jobs = { Police = 'police' }

---@enum Size
local Sizes = { Small = 1 }
print(Sizes)

---@class Car
---@field color Color

---@param color Color
function Paint(color) end

---@param key Key
function Press(key) end

---@param job Job
function Hire(job) end

---@param size Size
function Resize(size) end

---@param weather Weather
function Forecast(weather) end
";
    client.open_with("myresource/shared/config.lua", defs);
    client.open_with("myresource/modules/weather.lua", "---@enum Weather\nreturn { Sun = 'sun', Rain = 'rain' }\n");
    // `|` marks the cursor; `trigger` is the character typed to ask for completions. The listed items
    // come back in the order their `sortText` gives, as label and inserted text.
    let mut values = |file: &str, typed: &str, trigger: Option<&str>| -> Vec<(String, String)> {
        let (line, column) = pos(typed, "|", 0);
        client.open_with(file, &typed.replace('|', ""));
        let mut params = client.position_params(file, line, column);
        if let Some(trigger) = trigger {
            params["context"] = json!({ "triggerKind": 2, "triggerCharacter": trigger });
        }
        let result = client.request("textDocument/completion", params);
        let mut items = result["items"].as_array().cloned().unwrap_or_default();
        items.retain(|item| {
            let sort = item["sortText"].as_str().unwrap_or_default();
            sort.len() == 4 && sort.bytes().all(|b| b.is_ascii_digit())
        });
        items.sort_by_key(|item| item["sortText"].as_str().unwrap_or_default().to_string());
        let written = |item: &Value| item["insertText"].as_str().unwrap_or_default().to_string();
        items.iter().map(|item| (item["label"].as_str().unwrap().to_string(), written(item))).collect()
    };
    let listed = |values: &[&str], space: &str| -> Vec<(String, String)> {
        values.iter().map(|value| (value.to_string(), format!("{space}{value}"))).collect()
    };
    let colors = ["Colors.Red", "Colors.Green", "Colors['Dark Blue']", "1", "2", "3"];

    // Where a value of the enum's type starts: an argument, a `---@type` local, a field of a class,
    // a `return` and a comparison.
    assert_eq!(values(CLIENT, "Paint(|)", Some("(")), listed(&colors, ""));
    assert_eq!(values(CLIENT, "---@type Color\nlocal color =|", None), listed(&colors, " "));
    assert_eq!(values(CLIENT, "---@type Color\nlocal color = |", Some(" ")), listed(&colors, ""));
    assert_eq!(values(CLIENT, "---@type Car\nlocal car = { color = | }", None), listed(&colors, ""));
    assert_eq!(values(CLIENT, "---@type Car\nlocal car\ncar.color = |", None), listed(&colors, ""));
    assert_eq!(values(CLIENT, "---@return Color\nlocal function pick()\n\treturn |\nend", None), listed(&colors, ""));
    let compared = "---@type Color\nlocal color = Colors.Red\nif color == | then end";
    assert_eq!(values(CLIENT, compared, Some(" ")), listed(&colors, ""));

    // The members are written as the code reaches the table: a local of this file, one that holds a
    // module's table or contains the table, but not a local of another file or a hidden global.
    let speeds = "---@enum Speed\nlocal Speeds = { Slow = 1 }\n---@param speed Speed\nlocal function go(speed) end\n";
    assert_eq!(values(CLIENT, &format!("{speeds}go(|)"), Some("(")), listed(&["Speeds.Slow", "1"], ""));
    let modes = "local Config = {}\n---@enum Mode\nConfig.Modes = { On = 1 }\n---@param mode Mode\nlocal function set(mode) end\n";
    assert_eq!(values(CLIENT, &format!("{modes}set(|)"), Some("(")), listed(&["Config.Modes.On", "1"], ""));
    let weather = "local Weather = require 'modules.weather'\nForecast(|)";
    assert_eq!(values(CLIENT, weather, Some("(")), listed(&["Weather.Sun", "Weather.Rain", "'sun'", "'rain'"], ""));
    assert_eq!(values(CLIENT, "Resize(|)", Some("(")), listed(&["1"], ""));
    assert_eq!(values(CLIENT, "local Colors = 5\nPaint(|)", Some("(")), listed(&["1", "2", "3"], ""));

    // A `(key)` enum lists its keys, which are its values, and a `(server)` one only on that side.
    assert_eq!(values(CLIENT, "Press(|)", Some("(")), listed(&["'Alpha'", "'Beta'"], ""));
    assert_eq!(values(SERVER, "Hire(|)", Some("(")), listed(&["Jobs.Police", "'police'"], ""));
    assert_eq!(values(CLIENT, "Hire(|)", Some("(")), Vec::new());

    // Members are enum members that show their value.
    let (line, column) = pos("Paint(|)", "|", 0);
    client.open_with(CLIENT, "Paint()");
    let result = client.request("textDocument/completion", client.position_params(CLIENT, line, column));
    let red = result["items"].as_array().unwrap().iter().find(|item| item["label"] == "Colors.Red").cloned();
    assert_eq!(red.map(|item| (item["kind"].clone(), item["detail"].clone())), Some((json!(20), json!("1"))));
}

#[test]
fn enum_members_are_written_through_globals_the_file_sees_and_brackets_for_keywords() {
    let mut client = Client::start(fixture_root());
    client.open_with("shop/shared.lua", "---@enum Test.Group\nGroups = { Job = 'job' }\n");
    let defs = "\
---@enum Test.Kind
Kinds = { ['nil'] = 'nil', ['end'] = 'end', ok = 'ok' }

---@param group Test.Group
function Join(group) end

---@param grade Test.Grade
function Promote(grade) end

---@param kind Test.Kind
function Sort(kind) end
";
    client.open_with("myresource/shared/config.lua", defs);
    // `|` marks the cursor of a completion that `(` asks for; the listed items come back as the text
    // they insert, in the order their `sortText` gives.
    let mut values = |file: &str, typed: &str| -> Vec<String> {
        let (line, column) = pos(typed, "|", 0);
        client.open_with(file, &typed.replace('|', ""));
        let mut params = client.position_params(file, line, column);
        params["context"] = json!({ "triggerKind": 2, "triggerCharacter": "(" });
        let result = client.request("textDocument/completion", params);
        let mut items = result["items"].as_array().cloned().unwrap_or_default();
        items.retain(|item| {
            let sort = item["sortText"].as_str().unwrap_or_default();
            sort.len() == 4 && sort.bytes().all(|b| b.is_ascii_digit())
        });
        items.sort_by_key(|item| item["sortText"].as_str().unwrap_or_default().to_string());
        items.iter().map(|item| item["insertText"].as_str().unwrap_or_default().to_string()).collect()
    };

    // The global table of an enum that another resource, or a script of the other side, declares is
    // undefined here, so only its values are listed.
    assert_eq!(values(CLIENT, "Join(|)"), ["'job'"]);
    let grades = "---@enum Test.Grade\nGrades = { Boss = 'boss' }\nPromote(|)";
    assert_eq!(values(SERVER, grades), ["Grades.Boss", "'boss'"]);
    assert_eq!(values(CLIENT, "Promote(|)"), ["'boss'"]);

    // A key that is a keyword cannot follow a `.`.
    assert_eq!(values(CLIENT, "Sort(|)"), ["Kinds['nil']", "Kinds['end']", "Kinds.ok", "'nil'", "'end'", "'ok'"]);
}

#[test]
fn call_snippets_leave_listed_values_to_the_list() {
    let defs = "\
---@alias Actions \"playerLoaded\"|\"playerUnloaded\"|string

---@param action Actions
---@param handler fun(...)
---@overload fun(action: \"playerUnloaded\", handler: fun(source: number))
function OnAction(action, handler) end

---@param action string
---@param handler fun(...)
---@overload (server) fun(action: \"serverOnly\", handler: fun(source: number))
function OnServer(action, handler) end

---@param topic \"chat\"|\"news\"
---@param handler fun(message: string)
function Subscribe(topic, handler) end

---@param id integer
---@param kind \"a\"|\"b\"
---@param cb fun()
function Tagged(id, kind, cb) end

---@class Emitter
Emitter = {}

---@param event \"open\"|\"close\"
---@param mode \"once\"|\"always\"
function Emitter:on(event, mode) end
";
    let mut vscode = json!({ "textDocument": { "completion": { "completionItem": { "snippetSupport": true } } } });
    vscode["experimental"] = json!({ "commands": { "commands": ["editor.action.triggerSuggest"] } });
    let mut client = Client::start_with_capabilities(fixture_root(), vscode);
    client.open_with("myresource/shared/config.lua", defs);
    // `|` marks the cursor. The call snippet of `label`, with its command.
    let mut snippet = |file: &str, typed: &str, label: &str| -> (String, Value) {
        let (line, column) = pos(typed, "|", 0);
        client.open_with(file, &typed.replace('|', ""));
        let result = client.request("textDocument/completion", client.position_params(file, line, column));
        let items = result["items"].as_array().cloned().unwrap_or_default();
        let found =
            items.iter().find(|item| item["label"] == label && item["labelDetails"]["description"] == "snippet");
        let found = found.unwrap_or_else(|| panic!("{typed}: no call snippet in {result}"));
        (found["insertText"].as_str().unwrap().to_string(), found["command"].clone())
    };
    let reopen = json!({ "title": "Suggest values", "command": "editor.action.triggerSuggest" });

    // The handler depends on the value an overload takes alone, so the snippet ends after it.
    assert_eq!(snippet(CLIENT, "OnAct|", "OnAction"), ("OnAction('$1'$0)".into(), reopen.clone()));
    assert_eq!(
        snippet(CLIENT, "local label = \"x\"\nOnAct|", "OnAction"),
        ("OnAction(\"$1\"$0)".into(), reopen.clone())
    );
    // Only the overloads of the side count.
    assert_eq!(
        snippet(CLIENT, "OnServ|", "OnServer"),
        ("OnServer('${1:action}', function(${2:...})\n\t$0\nend)".into(), Value::Null)
    );
    assert_eq!(snippet(SERVER, "OnServ|", "OnServer"), ("OnServer('$1'$0)".into(), reopen.clone()));
    // Values that decide nothing leave the handler written out.
    assert_eq!(
        snippet(CLIENT, "Subscri|", "Subscribe"),
        ("Subscribe('$1', function(${2:message})\n\t$0\nend)".into(), reopen.clone())
    );
    // Suggestions reopen only when the first stop is a list.
    assert_eq!(
        snippet(CLIENT, "Tagg|", "Tagged"),
        ("Tagged(${1:id}, '$2', function()\n\t$0\nend)".into(), Value::Null)
    );
    // Parameters that list values get a snippet without taking a callback.
    assert_eq!(snippet(CLIENT, "local emitter = Emitter\nemitter:o|", "on"), ("on('$1', '$2')".into(), reopen));
}

#[test]
fn minimal_clients_receive_plain_completions_and_no_dynamic_watch_registration() {
    for capabilities in [
        json!({}),
        json!({
            "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": false } },
            "textDocument": { "completion": { "completionItem": { "snippetSupport": false } } }
        }),
    ] {
        let mut client = Client::start_with_capabilities(fixture_root(), capabilities);
        client.request("qbx/status", Value::Null);
        assert!(client.registrations.is_empty(), "{:?}", client.registrations);

        let cases = [
            (CLIENT, "CreateThread", 0, 12, Some("CreateThread")),
            (CLIENT, "RegisterNuiCall", 0, 15, Some("RegisterNuiCallback")),
            (CLIENT, "local Useful = 1\nUse", 1, 3, Some("Useful")),
            (CLIENT, "lib.onCa", 0, 8, None),
            (CLIENT, "oncache", 0, 7, None),
            (CLIENT, "---@par", 0, 7, Some("param")),
            ("myresource/fxmanifest.lua", "fx_v", 0, 4, Some("fx_version")),
        ];
        for (relative, text, line, column, expected) in cases {
            client.open_with(relative, text);
            let result = client.request("textDocument/completion", client.position_params(relative, line, column));
            let items = result["items"].as_array().unwrap();
            if let Some(label) = expected {
                assert!(items.iter().any(|item| item["label"] == label), "{result}");
            }
            for item in items {
                assert_ne!(item["insertTextFormat"], 2, "{item}");
                assert_ne!(item["labelDetails"]["description"], "snippet", "{item}");
                assert!(!item["insertText"].as_str().unwrap_or_default().contains('$'), "{item}");
            }
            if text == "---@par" {
                assert_eq!(items.iter().find(|item| item["label"] == "param").unwrap()["insertText"], "param");
            }
            client.notify("textDocument/didClose", json!({"textDocument": {"uri": client.uri(relative)}}));
        }
    }
}

#[test]
fn capable_clients_keep_file_watches_and_annotation_and_manifest_snippets() {
    let mut client = Client::start(fixture_root());
    client.request("qbx/status", Value::Null);
    assert_eq!(client.registrations.len(), 1);
    let registration = &client.registrations[0];
    assert_eq!(registration["method"], "workspace/didChangeWatchedFiles");
    assert!(registration["registerOptions"]["watchers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|watch| watch["globPattern"] == "**/*.lua"));

    for (relative, text, label) in [(CLIENT, "---@par", "param"), ("myresource/fxmanifest.lua", "fx_v", "fx_version")] {
        client.open_with(relative, text);
        let result = client.request("textDocument/completion", client.position_params(relative, 0, text.len() as u32));
        let item = result["items"].as_array().unwrap().iter().find(|item| item["label"] == label).unwrap();
        assert_eq!(item["insertTextFormat"], 2, "{item}");
        assert!(item["insertText"].as_str().unwrap().contains("${1"), "{item}");
    }
}

#[test]
fn enter_continues_and_clears_annotation_lines() {
    let mut client = Client::start(fixture_root());
    let mut edits = |text: &str, line: u32, character: u32, ch: &str| -> Vec<(u32, u32, u32, u32, String)> {
        client.open_with(CLIENT, text);
        let result = client.request(
            "textDocument/onTypeFormatting",
            json!({
                "textDocument": { "uri": client.uri(CLIENT) },
                "position": { "line": line, "character": character },
                "ch": ch,
                "options": { "tabSize": 4, "insertSpaces": true },
            }),
        );
        let n = |v: &Value| v.as_u64().unwrap() as u32;
        result
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let (start, end) = (&e["range"]["start"], &e["range"]["end"]);
                let text = e["newText"].as_str().unwrap().to_string();
                (n(&start["line"]), n(&start["character"]), n(&end["line"]), n(&end["character"]), text)
            })
            .collect()
    };

    let continued = [(1, 0, 1, 0, "---@".to_string())];
    assert_eq!(edits("---@param event string\n", 1, 0, "\n"), continued);
    assert_eq!(edits("    ---@return boolean\n    ", 1, 4, "\n"), [(1, 0, 1, 4, "    ---@".to_string())]);
    // Some editors continue `---` comments themselves.
    assert_eq!(edits("---@param event string\n--- ", 1, 4, "\n"), [(1, 0, 1, 4, "---@".to_string())]);
    // Enter on a bare `---@` clears it and stays on that line.
    assert_eq!(edits("---@param a string\n    ---@\n    ", 2, 4, "\n"), [(1, 4, 2, 4, String::new())]);
    assert_eq!(edits("---@param a string\r\n---@ \r\n", 2, 0, "\n"), [(1, 0, 2, 0, String::new())]);

    for (text, line, character, ch) in [
        // Enter in the middle of a line.
        ("---@param a\n string", 1, 0, "\n"),
        // Not an annotation.
        ("-- note\n", 1, 0, "\n"),
        ("--- A description.\n", 1, 0, "\n"),
        // Inside a long string.
        ("local s = [[\n---@param x\n\n]]", 2, 0, "\n"),
        ("---@param event string\n", 1, 0, "}"),
        ("---@param event string", 0, 22, "\n"),
    ] {
        assert!(edits(text, line, character, ch).is_empty(), "{text:?}");
    }
}

#[test]
fn knows_glm_and_keeps_native_handle_names() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;
    let added = "local veh = GetVehiclePedIsIn(PlayerPedId(), false)\nlocal dir = glm.normalize(vector3(1, 2, 3))\nprint(veh, dir, glm.pi)\nglm.quatLook\nlocal g = require 'glm'\nlocal zone = g.polygon.new({ vector3(0, 0, 0) })\nprint(zone:contains(vector3(0, 0, 0), 2), g.tointeger(1.0))\nveh.";
    client.change(CLIENT, 2, &format!("{text}{added}"));

    let hover = client.hover_text(CLIENT, line, 7);
    assert!(hover.contains("local veh: Vehicle"), "{hover}");
    let native = client.hover_text(CLIENT, line, 16);
    assert!(native.contains("ped: Ped") && native.contains("): Vehicle"), "{native}");
    assert_eq!(client.completion_labels(CLIENT, line + 7, 4), Vec::<String>::new(), "a handle has no members");

    let normalize = client.hover_text(CLIENT, line + 1, 20);
    assert!(normalize.contains("glm.normalize") && normalize.contains("length 1"), "{normalize}");
    assert!(client.hover_text(CLIENT, line + 2, 22).contains("number"));
    assert!(client.completion_labels(CLIENT, line + 3, 12).contains(&"quatLookAt".to_string()));

    let zone = client.hover_text(CLIENT, line + 5, 7);
    assert!(zone.contains("local zone: glm.polygon"), "require 'glm' is the built-in library: {zone}");
    let contains = client.hover_text(CLIENT, line + 6, 13);
    assert!(contains.contains("thickness?: number") && contains.contains("boolean"), "{contains}");

    let found = client.diagnostics_for(CLIENT);
    assert!(!found.iter().any(|(code, l)| code == "undefined-global" && *l >= u64::from(line)), "{found:?}");
}

#[test]
fn exports_of_escrowed_resources_are_not_second_guessed() {
    let mut client = Client::start(fixture_root());
    assert_eq!(
        client.diagnostics_for("late/hidden.lua"),
        [("fivem/unknown-export".to_string(), 1)],
        "vault has an encrypted file that may register anything; mylib is fully readable"
    );
}

#[test]
fn methods_that_the_declared_type_of_an_export_lacks_are_reported() {
    let root = declared_exports_root();
    let mut client = Client::start_with_library(root.join("workspace"), &root.join("types"));
    let file = "app/client.lua";
    let text = client.open(file);
    let added = "exports['tablet']:Flash()\nexports.tablet:Ring(1)\nexports.phone:Anything()\n\
                 ---@type TabletExports\nlocal tablet = exports.tablet\ntablet:Flash()\n";
    client.change(file, 2, &format!("{text}{added}"));
    let line = text.lines().count() as u64;
    let message = "Field `Flash` is not declared in `TabletExports`".to_string();
    assert_eq!(
        findings(&mut client, file, &["undefined-field"]),
        [("undefined-field".to_string(), line, message.clone()), ("undefined-field".to_string(), line + 5, message)],
        "a call through the exports proxy is checked as one through a local of the declared type; an index \
         of the type takes any name"
    );
}

#[test]
fn completes_resources_and_exports_in_both_spellings() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let line = text.lines().count() as u32;

    client.change(CLIENT, 2, &format!("{text}exports['']"));
    let mut resources = client.completion_labels(CLIENT, line, 9);
    resources.sort();
    assert_eq!(resources, ["late", "mylib", "myresource", "shop", "vault"]);

    client.change(CLIENT, 3, &format!("{text}exports['mylib']:"));
    let labels = client.completion_labels(CLIENT, line, 17);
    assert!(labels.contains(&"GetPlayer".to_string()) && labels.contains(&"Ping".to_string()), "{labels:?}");

    client.change(CLIENT, 4, &format!("{text}exports.mylib:GetPlayer(1)"));
    let hover = client.hover_text(CLIENT, line, 16);
    assert!(hover.contains("GetPlayer(source: integer)") && hover.contains("Looks a player up"), "{hover}");
}

fn declared_exports_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/declared_exports")
}

#[test]
fn declared_types_describe_the_exports_of_a_resource() {
    let root = declared_exports_root();
    let mut client = Client::start_with_library(root.join("workspace"), &root.join("types"));
    let text = client.open("app/client.lua");

    let (l, c) = pos(&text, "local config", 6);
    assert!(client.hover_text("app/client.lua", l, c).contains("local config: PhoneConfig"), "the declared type wins");
    let (l, c) = pos(&text, "local extra", 6);
    let extra = client.hover_text("app/client.lua", l, c);
    assert!(extra.contains("local extra: string"), "registered exports the type leaves out stay: {extra}");
    let (l, c) = pos(&text, "Ring(2)", 0);
    let ring = client.hover_text("app/client.lua", l, c);
    assert!(
        ring.contains("Ring(self: TabletExports, times: number): boolean"),
        "a resource the workspace lacks: {ring}"
    );
    let (l, c) = pos(&text, "IsInCall()", 0);
    let in_call = client.hover_text("app/client.lua", l, c);
    assert!(in_call.contains("IsInCall(self: PhoneExports): boolean"), "{in_call}");
    let (l, c) = pos(&text, "local slots", 6);
    let slots = client.hover_text("app/client.lua", l, c);
    assert!(slots.contains("local slots: table[]"), "a method declared twice is an overload: {slots}");
    let (l, c) = pos(&text, "local count", 6);
    assert!(client.hover_text("app/client.lua", l, c).contains("local count: number"));

    let line = text.lines().count() as u32;
    client.change("app/client.lua", 2, &format!("{text}exports['']"));
    let mut resources = client.completion_labels("app/client.lua", line, 9);
    resources.sort();
    assert_eq!(
        resources,
        ["app", "fleet", "garage", "mocker", "ox_inventory", "phone", "qbx_core", "rental", "tablet"]
    );
    client.change("app/client.lua", 3, &format!("{text}exports.tablet:"));
    assert_eq!(client.completion_labels("app/client.lua", line, 15), ["Ring"]);

    let server = client.open("app/server.lua");
    let (l, c) = pos(&server, "callId", 0);
    assert!(
        client.hover_text("app/server.lua", l, c).contains("local callId: number?"),
        "a server call picks its signature"
    );
    let (l, c) = pos(&server, "call =", 0);
    assert!(client.hover_text("app/server.lua", l, c).contains("local call: PhoneCall?"));
    let (l, c) = pos(&server, "cid =", 0);
    let cid = client.hover_text("app/server.lua", l, c);
    assert!(cid.contains("local cid: string"), "methods declared on `exports.qbx_core`: {cid}");
    let (l, c) = pos(&server, "remote =", 0);
    let remote = client.hover_text("app/server.lua", l, c);
    assert!(!remote.contains("table[]"), "`ox_inventory/client.lua` declares for the client: {remote}");
}

#[test]
fn definition_files_outside_resources_declare_globals_for_every_resource() {
    let root = declared_exports_root();
    let mut client = Client::start_with_library(root.join("workspace"), &root.join("types"));
    let text = client.open("app/server.lua");

    let (l, c) = pos(&text, "rows", 0);
    assert!(client.hover_text("app/server.lua", l, c).contains("local rows: table[]"), "a ---@meta workspace file");
    let (l, c) = pos(&text, "value =", 0);
    assert!(client.hover_text("app/server.lua", l, c).contains("local value: number"), "any library file");
    let (l, c) = pos(&text, "loose =", 0);
    let loose = client.hover_text("app/server.lua", l, c);
    assert!(!loose.contains("number"), "a workspace file without ---@meta keeps its globals: {loose}");

    let found = client.diagnostics_for("app/server.lua");
    let undefined: Vec<u64> = found.iter().filter(|(code, _)| code == "undefined-global").map(|(_, l)| *l).collect();
    assert_eq!(undefined, [3], "{found:?}");

    let sql = client.open("types/sql.lua");
    client.change("types/sql.lua", 2, sql.trim_start_matches("---@meta"));
    let found = client.diagnostics_for("app/server.lua");
    let undefined: Vec<u64> = found.iter().filter(|(code, _)| code == "undefined-global").map(|(_, l)| *l).collect();
    assert_eq!(undefined, [1, 3], "dropping `---@meta` takes the globals back: {found:?}");
    client.change("types/sql.lua", 3, &sql);
    let found = client.diagnostics_for("app/server.lua");
    assert!(found.iter().filter(|(code, _)| code == "undefined-global").all(|(_, l)| *l == 3), "{found:?}");
}

#[test]
fn definition_files_declare_globals_for_their_side_and_provider() {
    fn undefined(client: &mut Client, file: &str) -> Vec<String> {
        client.open(file);
        findings(client, file, &["undefined-global"]).into_iter().map(|(_, _, message)| message).collect()
    }
    let root = declared_exports_root();
    let mut client = Client::start_with_library(root.join("workspace"), &root.join("types"));

    assert_eq!(undefined(&mut client, "fleet/server.lua"), ["undefined global 'player'"], "a client_ file");
    assert_eq!(undefined(&mut client, "fleet/client.lua"), ["undefined global 'vehicle'"], "a server_ file");
    assert_eq!(undefined(&mut client, "rental/server.lua"), ["undefined global 'vehicle'"], "no @ox_core import");

    let fleet = std::fs::read_to_string(root.join("workspace/fleet/server.lua")).unwrap();
    let (l, c) = pos(&fleet, "vehicle", 0);
    assert!(client.hover_text("fleet/server.lua", l, c).contains("OxVehicleServer"));
    let (l, c) = pos("print(vehicle)", "vehicle", 0);
    let hover = client.hover_text("rental/server.lua", l, c);
    assert!(!hover.contains("OxVehicleServer"), "{hover}");
}

#[test]
fn plain_assignments_to_exports_declare_no_types() {
    let root = declared_exports_root();
    let mut client = Client::start_with_library(root.join("workspace"), &root.join("types"));
    let text = client.open("mocker/server.lua");

    let (l, c) = pos(&text, "local config", 6);
    let config = client.hover_text("mocker/server.lua", l, c);
    assert!(config.contains("local config: PhoneConfig"), "a mock leaves the declared type: {config}");
    let line = text.lines().count() as u32;
    client.change("mocker/server.lua", 2, &format!("{text}exports['']"));
    let resources = client.completion_labels("mocker/server.lua", line, 9);
    assert!(!resources.iter().any(|r| r == "Ping"), "`exports.Ping = fn` registers an export: {resources:?}");
    client.change("mocker/server.lua", 3, &format!("{text}exports.phone:"));
    let members = client.completion_labels("mocker/server.lua", line, 14);
    assert!(members.contains(&"GetConfig".to_string()) && !members.contains(&"Fake".to_string()), "{members:?}");
}

#[test]
fn a_resource_keeps_its_own_types_and_globals_over_definition_files() {
    let root = declared_exports_root();
    let mut client = Client::start_with_library(root.join("workspace"), &root.join("types"));
    let text = client.open("garage/server.lua");

    let found = client.diagnostics_for("garage/server.lua");
    assert!(!found.iter().any(|(code, _)| code == "missing-fields"), "its own `VehicleData` alias wins: {found:?}");
    let (l, c) = pos(&text, "Vehicle)", 0);
    let hover = client.hover_text("garage/server.lua", l, c);
    assert!(hover.contains("(global) Vehicle") && !hover.contains("LibVehicle"), "its own global wins: {hover}");
}

#[test]
fn table_hover_lists_only_the_fields_in_scope() {
    let mut client = Client::start(fixture_root());
    let text = client.open(CLIENT);
    let (l, c) = pos(&text, "Config.SpawnDistance", 2);
    let hover = client.hover_text(CLIENT, l, c);
    let expected = "```lua\n(global) Config: {\n    Debug: boolean = true,\n    SpawnDistance: number = 25.0,\n    Garages: table,\n    isDebug: function,\n}\n```";
    assert!(hover.starts_with(expected), "{hover}");
    assert!(!hover.contains("ShopName"), "the shop resource has its own Config: {hover}");
    assert!(hover.contains("myresource/shared/config.lua"), "{hover}");

    let (l, c) = pos(&text, "Config.SpawnDistance", 10);
    assert!(client.hover_text(CLIENT, l, c).contains("(field) Config.SpawnDistance: number = 25.0"));

    let shop = client.open(SHOP_CLIENT);
    client.change(SHOP_CLIENT, 2, &format!("{shop}print(Config)"));
    let hover = client.hover_text(SHOP_CLIENT, shop.lines().count() as u32, 8);
    assert!(
        hover.contains("ShopName: string = 'General Store'") && hover.contains("OpenAtNight: boolean = false"),
        "{hover}"
    );
    assert!(!hover.contains("SpawnDistance"), "{hover}");
}

#[test]
fn escrow_encrypted_files_are_ignored() {
    let mut client = Client::start(fixture_root());
    assert_eq!(client.diagnostics_for("vault/escrowed.lua"), [], "closed escrowed files are not linted");

    client.open_with("vault/escrowed.lua", "FXAP\u{1}\u{fffd}\u{fffd}garbage(((");
    assert_eq!(client.diagnostics_for("vault/escrowed.lua"), [], "nor are they when opened in the editor");

    client.open_with(SHOP_CLIENT, &"local = = =\n".repeat(200));
    let found = client.diagnostics_for(SHOP_CLIENT);
    assert_eq!(found.len(), 11, "syntax errors are capped at ten plus a summary: {found:?}");
}

fn framework_fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/framework_callbacks")
}

const FRAMEWORK_CLIENT: &str = "adapters/client.lua";
const FRAMEWORK_IMPORTS: &str =
    "local Core = exports['qb-core']:GetCoreObject()\nlocal Framework = exports.es_extended:getSharedObject()\nlocal QB = Core\nlocal ESX = Framework\n";

fn framework_definitions(client: &mut Client, relative: &str, text: &str, needle: &str) -> Value {
    let (line, column) = pos(text, needle, 2);
    client.request("textDocument/definition", client.position_params(relative, line, column))
}

fn framework_hints(client: &mut Client, relative: &str, line: u32) -> Vec<String> {
    let hints = client.request(
        "textDocument/inlayHint",
        json!({ "textDocument": { "uri": client.uri(relative) }, "range": {
            "start": { "line": line, "character": 0 }, "end": { "line": line + 1, "character": 0 }
        } }),
    );
    hints.as_array().unwrap().iter().filter_map(|hint| hint["label"].as_str().map(str::to_owned)).collect()
}

#[test]
fn framework_callbacks_keep_completion_and_navigation_in_their_own_family() {
    let mut client = Client::start(framework_fixture_root());
    client.open_with(FRAMEWORK_CLIENT, "");
    for (version, (call, expected, definition)) in (2..).zip([
        ("QB.Functions.TriggerCallback", vec!["qb:guarded", "qb:only", "shared:call"], "adapters/server/qb.lua"),
        ("ESX.TriggerServerCallback", vec!["esx:imported", "esx:only", "shared:call"], "adapters/server/esx.lua"),
        ("lib.callback.await", vec!["ox:only", "shared:call"], "adapters/server/other.lua"),
        ("TriggerServerEvent", vec!["native:only", "shared:call"], "adapters/server/other.lua"),
    ]) {
        let text = format!("{FRAMEWORK_IMPORTS}{call}('')\n{call}('shared:call', function() end, 1, 2)\n");
        client.change(FRAMEWORK_CLIENT, version, &text);
        let (line, column) = pos(&text, "('')", 2);
        let mut labels = client.completion_labels(FRAMEWORK_CLIENT, line, column);
        labels.sort();
        assert_eq!(labels, expected, "{call}");

        let found = framework_definitions(&mut client, FRAMEWORK_CLIENT, &text, "shared:call");
        let locations = found.as_array().unwrap();
        assert_eq!(locations.len(), 1, "{call}: {found}");
        assert_eq!(locations[0]["uri"], client.uri(definition).as_str(), "{call}: {found}");
        let source = std::fs::read_to_string(client.root.join(definition)).unwrap();
        let registration =
            if call == "TriggerServerEvent" { "RegisterNetEvent('shared:call'" } else { "'shared:call'" };
        let (registration_line, _) = pos(&source, registration, 0);
        assert_eq!(locations[0]["range"]["start"]["line"], registration_line, "{call}: {found}");
    }
}

#[test]
fn framework_callbacks_show_typed_payloads_without_source_response_or_async_returns() {
    let mut client = Client::start(framework_fixture_root());
    let text = client.open(FRAMEWORK_CLIENT);
    for (call, value, family, payload, other_payload, expected_label, handler_file) in [
        (
            "QB.Functions.TriggerCallback",
            "'water'",
            "QB-Core callback",
            "qbItem",
            "esxVehicle",
            "QB.Functions.TriggerCallback(name: string, cb: function, qbItem: string, qbAmount: integer)",
            "qb.lua",
        ),
        (
            "ESX.TriggerServerCallback",
            "42",
            "ESX callback",
            "esxVehicle",
            "qbItem",
            "ESX.TriggerServerCallback(name: string, cb: function, esxVehicle: number, esxDepot: string)",
            "esx.lua",
        ),
    ] {
        let call_start = text.find(call).unwrap();
        let call_text =
            &text[call_start..text[call_start..].find('\n').map(|end| call_start + end).unwrap_or(text.len())];
        let (line, column) = pos(&text, call_text, call_text.find(value).unwrap() as u32 + 1);
        let result =
            client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, line, column));
        assert_eq!(result["signatures"][0]["label"], expected_label, "{result}");
        assert_eq!(result["activeParameter"], 2, "{result}");
        let note = result["signatures"][0]["documentation"]["value"].as_str().unwrap_or_default();
        assert!(note.contains(handler_file), "{note}");
        let name_column = call_text.find("shared:call").unwrap() as u32 + 2;
        let hover = client.hover_text(FRAMEWORK_CLIENT, line, name_column);
        assert!(hover.contains(family) && hover.contains(payload) && hover.contains(handler_file), "{hover}");
        assert!(!hover.contains(other_payload) && !hover.contains("source: integer"), "{hover}");
        assert!(hover.to_lowercase().contains("asynchronous"), "{hover}");
        let hints = framework_hints(&mut client, FRAMEWORK_CLIENT, line);
        assert!(hints.contains(&format!("{payload}:")), "{hints:?}");
        assert!(!hints.contains(&"source:".to_string()) && !hints.contains(&format!("{other_payload}:")), "{hints:?}");
    }
    for (call, expected, forbidden) in
        [("lib.callback.await", "oxPayload:", "nativePayload:"), ("TriggerServerEvent", "nativePayload:", "oxPayload:")]
    {
        let (line, _) = pos(&text, call, 0);
        let hints = framework_hints(&mut client, FRAMEWORK_CLIENT, line);
        assert!(hints.contains(&expected.to_string()) && !hints.contains(&forbidden.to_string()), "{hints:?}");
        assert!(!hints.iter().any(|hint| hint.starts_with("qb") || hint.starts_with("esx")), "{hints:?}");
    }
}

#[test]
fn framework_callbacks_complete_whole_strings_for_minimal_and_snippet_clients() {
    for snippets in [false, true] {
        let mut client = Client::start_with_capabilities(
            framework_fixture_root(),
            json!({ "textDocument": { "completion": { "completionItem": { "snippetSupport": snippets } } } }),
        );
        client.open_with(FRAMEWORK_CLIENT, "");
        for (version, (call, prefix, expected)) in (2..).zip([
            ("QB.Functions.TriggerCallback", "qb:", "qb:only"),
            ("ESX.TriggerServerCallback", "esx:", "esx:only"),
        ]) {
            let head = format!("local emoji = '🚗'; {call}('");
            let text = format!("{FRAMEWORK_IMPORTS}{head}{prefix}stale', function() end)");
            let line = FRAMEWORK_IMPORTS.lines().count() as u32;
            let start = head.encode_utf16().count() as u32;
            client.change(FRAMEWORK_CLIENT, version, &text);
            let result = client.request(
                "textDocument/completion",
                client.position_params(FRAMEWORK_CLIENT, line, start + prefix.len() as u32),
            );
            let item = result["items"].as_array().unwrap().iter().find(|item| item["label"] == expected).unwrap();
            assert_eq!(
                item["textEdit"],
                json!({ "range": {
                "start": { "line": line, "character": start },
                "end": { "line": line, "character": start + prefix.len() as u32 + 5 }
            }, "newText": expected }),
                "{result}"
            );
            assert!(item["insertTextFormat"].is_null(), "literal event names are not snippets: {item}");
        }
    }
}

#[test]
fn framework_callbacks_require_proven_unmodified_roots_and_the_name_argument() {
    let mut client = Client::start(framework_fixture_root());
    client.open_with(FRAMEWORK_CLIENT, "");
    let cases = [
        "local QB = {}\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local function demo(QB)\nQB.Functions.TriggerCallback('qb:only', function() end, 7)\nend",
        "local QB = exports['qb-core']:GetCoreObject()\nQB = {}\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback = function() end\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local exports = {}\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "_G.exports = {}\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "_ENV['exports'] = {}\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "_G.exports['qb-core'].GetCoreObject = function() return {} end\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local _ENV = {}\nlocal QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local QB = exports['qb-core']:GetCoreObject('subset')\nQB.Functions.TriggerCallback('qb:only', function() end, 7)",
        "local QB = exports['qb-core']:GetCoreObject()\nQB.Functions:TriggerCallback('qb:only', function() end, 7)",
        "local QB = exports['qb-core']:GetCoreObject()\nQB.Functions.TriggerCallback('missing', function() end, 'qb:only')",
        "local QB = exports['qb-core']:GetCoreObject()\nlocal name = 'qb:only'\nQB.Functions.TriggerCallback(name, function() end, 7)",
        "local ESX = {}\nESX.TriggerServerCallback('esx:only', function() end, 7)",
        "local ESX = exports.es_extended:getSharedObject()\nESX.TriggerServerCallback = function() end\nESX.TriggerServerCallback('esx:only', function() end, 7)",
        "local Core = exports.es_extended:getSharedObject()\nlocal ESX = Core\nCore.TriggerServerCallback = function() end\nESX.TriggerServerCallback('esx:only', function() end, 7)",
        "local ESX = exports.es_extended:getSharedObject()\nESX:TriggerServerCallback('esx:only', function() end, 7)",
        "print('qb:only')",
    ];
    for (version, text) in (2..).zip(cases) {
        client.change(FRAMEWORK_CLIENT, version, text);
        let needle = if text.contains("qb:only") { "qb:only" } else { "esx:only" };
        let (line, column) = pos(text, needle, 2);
        let labels = client.completion_labels(FRAMEWORK_CLIENT, line, column);
        assert!(!labels.contains(&needle.to_string()), "{text}: {labels:?}");
        let found = framework_definitions(&mut client, FRAMEWORK_CLIENT, text, needle);
        assert!(found.is_null() || found.as_array().is_some_and(Vec::is_empty), "{text}: {found}");
        let hover = client.hover_text(FRAMEWORK_CLIENT, line, column);
        assert!(!hover.contains("QB-Core callback") && !hover.contains("ESX callback"), "{text}: {hover}");
        let signature =
            client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, line, column));
        let rendered = signature.to_string();
        assert!(!rendered.contains("qbUnique") && !rendered.contains("esxUnique"), "{text}: {signature}");
    }

    client.open_with("imported_esx/client.lua", "");
    for (version, mutation) in (2..).zip([
        "_G.ESX = {}",
        "_ENV['ESX'] = {}",
        "_G.ESX.TriggerServerCallback = function() end",
        "_ENV['ESX']['TriggerServerCallback'] = function() end",
    ]) {
        let text = format!("{mutation}\nESX.TriggerServerCallback('esx:imported', function() end, 7)");
        client.change("imported_esx/client.lua", version, &text);
        let (line, column) = pos(&text, "esx:imported", 2);
        let labels = client.completion_labels("imported_esx/client.lua", line, column);
        assert!(!labels.contains(&"esx:imported".to_string()), "{text}: {labels:?}");
        let found = framework_definitions(&mut client, "imported_esx/client.lua", &text, "esx:imported");
        assert!(found.is_null() || found.as_array().is_some_and(Vec::is_empty), "{text}: {found}");
    }
}

#[test]
fn framework_callbacks_honor_server_registration_client_trigger_and_shared_guards() {
    let mut client = Client::start(framework_fixture_root());
    let shared = client.open("adapters/shared.lua");
    let (line, column) = pos(&shared, "'guarded'", 2);
    let signature =
        client.request("textDocument/signatureHelp", client.position_params("adapters/shared.lua", line, column));
    assert!(signature["signatures"][0]["label"].as_str().unwrap_or_default().contains("guardedPayload"), "{signature}");

    let imported = client.open("imported_esx/client.lua");
    let definitions = framework_definitions(&mut client, "imported_esx/client.lua", &imported, "esx:imported");
    assert_eq!(definitions[0]["uri"], client.uri("imported_esx/server.lua").as_str(), "{definitions}");
    let (line, column) = pos(&imported, "'imported'", 2);
    let signature =
        client.request("textDocument/signatureHelp", client.position_params("imported_esx/client.lua", line, column));
    assert!(
        signature["signatures"][0]["label"].as_str().unwrap_or_default().contains("importedPayload"),
        "{signature}"
    );

    client.open_with(FRAMEWORK_CLIENT, "");
    let wrong_registration = format!("{FRAMEWORK_IMPORTS}QB.Functions.CreateCallback('qb:client-invalid', function(source, cb, badPayload) end)\nESX.RegisterServerCallback('esx:client-invalid', function(source, cb, badPayload) end)\nQB.Functions.TriggerCallback('')\nESX.TriggerServerCallback('')");
    client.change(FRAMEWORK_CLIENT, 2, &wrong_registration);
    for call in ["QB.Functions.TriggerCallback('')", "ESX.TriggerServerCallback('')"] {
        let (line, column) = pos(&wrong_registration, call, call.find("''").unwrap() as u32 + 1);
        let labels = client.completion_labels(FRAMEWORK_CLIENT, line, column);
        assert!(!labels.iter().any(|label| label.ends_with("client-invalid")), "{labels:?}");
    }
    for (relative, text) in [
        ("adapters/server/wrongside.lua", format!("{FRAMEWORK_IMPORTS}QB.Functions.TriggerCallback('qb:only', function() end, 7)")),
        ("adapters/shared.lua", format!("{FRAMEWORK_IMPORTS}QB.Functions.CreateCallback('qb:unguarded', function(source, cb, badPayload) end)\nQB.Functions.TriggerCallback('qb:only', function() end, 7)")),
    ] {
        if relative == "adapters/shared.lua" { client.change(relative, 2, &text); } else { client.open_with(relative, &text); }
        let (line, column) = pos(&text, "qb:only", 2);
        let labels = client.completion_labels(relative, line, column);
        assert!(!labels.contains(&"qb:only".to_string()), "{relative}: {labels:?}");
        let found = framework_definitions(&mut client, relative, &text, "qb:only");
        assert!(found.is_null() || found.as_array().is_some_and(Vec::is_empty), "{relative}: {found}");
        let signature = client.request("textDocument/signatureHelp", client.position_params(relative, line, column));
        assert!(!signature.to_string().contains("qbUnique"), "{relative}: {signature}");
    }
}

#[test]
fn framework_callbacks_refresh_unsaved_registration_names_and_payloads() {
    let mut client = Client::start(framework_fixture_root());
    let original = client.open("adapters/server/qb.lua");
    let client_text = format!("{FRAMEWORK_IMPORTS}QB.Functions.TriggerCallback('qb:renamed', function() end, 'item', 3)\nQB.Functions.TriggerCallback('')");
    client.open_with(FRAMEWORK_CLIENT, &client_text);
    let (line, column) = pos(&client_text, "('')", 2);
    assert!(client.completion_labels(FRAMEWORK_CLIENT, line, column).contains(&"qb:only".to_string()));
    let changed = original.replace("qb:only", "qb:renamed").replace("qbUnique", "freshPayload");
    client.change("adapters/server/qb.lua", 2, &changed);
    let labels = client.completion_labels(FRAMEWORK_CLIENT, line, column);
    assert!(labels.contains(&"qb:renamed".to_string()) && !labels.contains(&"qb:only".to_string()), "{labels:?}");
    let (call_line, call_column) = pos(&client_text, "'item'", 2);
    let signature =
        client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, call_line, call_column));
    assert!(signature["signatures"][0]["label"].as_str().unwrap_or_default().contains("freshPayload"), "{signature}");
    let definitions = framework_definitions(&mut client, FRAMEWORK_CLIENT, &client_text, "qb:renamed");
    assert_eq!(definitions[0]["uri"], client.uri("adapters/server/qb.lua").as_str(), "{definitions}");

    client.change("adapters/server/qb.lua", 3, "local QB = exports['qb-core']:GetCoreObject()\nlocal name = 'qb:renamed'\nQB.Functions.CreateCallback(name, function(source, cb, shouldNotInfer) end)\n");
    let labels = client.completion_labels(FRAMEWORK_CLIENT, line, column);
    assert!(!labels.contains(&"qb:renamed".to_string()) && !labels.contains(&"qb:only".to_string()), "{labels:?}");
    let signature =
        client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, call_line, call_column));
    assert!(
        !signature.to_string().contains("freshPayload") && !signature.to_string().contains("shouldNotInfer"),
        "{signature}"
    );
}

#[test]
fn framework_callbacks_do_not_choose_between_conflicting_handler_payloads() {
    let mut client = Client::start(framework_fixture_root());
    client.open_with("adapters/server/conflict.lua", "local QB = exports['qb-core']:GetCoreObject()\nQB.Functions.CreateCallback('shared:call', function(source, cb, conflictingPayload) end)\n");
    let text = client.open(FRAMEWORK_CLIENT);
    let definitions = framework_definitions(&mut client, FRAMEWORK_CLIENT, &text, "shared:call");
    assert_eq!(definitions.as_array().unwrap().len(), 2, "{definitions}");
    let (line, column) = pos(&text, "'water'", 2);
    let signature =
        client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, line, column));
    assert!(
        !signature.to_string().contains("qbItem") && !signature.to_string().contains("conflictingPayload"),
        "{signature}"
    );
    let hints = framework_hints(&mut client, FRAMEWORK_CLIENT, line);
    assert!(
        !hints.contains(&"qbItem:".to_string()) && !hints.contains(&"conflictingPayload:".to_string()),
        "{hints:?}"
    );

    client.change(
        "adapters/server/conflict.lua",
        2,
        "local QB = exports['qb-core']:GetCoreObject()\nQB.Functions.CreateCallback('shared:call', unknownHandler)\n",
    );
    let definitions = framework_definitions(&mut client, FRAMEWORK_CLIENT, &text, "shared:call");
    assert_eq!(
        definitions.as_array().unwrap().len(),
        2,
        "an unresolved handler still has a registration: {definitions}"
    );
    let signature =
        client.request("textDocument/signatureHelp", client.position_params(FRAMEWORK_CLIENT, line, column));
    assert!(
        !signature.to_string().contains("qbItem"),
        "an unresolved duplicate must not select the known handler: {signature}"
    );
    let hints = framework_hints(&mut client, FRAMEWORK_CLIENT, line);
    assert!(!hints.contains(&"qbItem:".to_string()), "{hints:?}");

    let (name_line, name_column) = pos(&text, "shared:call", 2);
    let completion =
        client.request("textDocument/completion", client.position_params(FRAMEWORK_CLIENT, name_line, name_column));
    let item = completion["items"].as_array().unwrap().iter().find(|item| item["label"] == "shared:call").unwrap();
    let detail = item["detail"].as_str().unwrap_or_default();
    assert!(!detail.contains("qbItem") && detail.contains("Multiple handlers"), "{item}");
}
