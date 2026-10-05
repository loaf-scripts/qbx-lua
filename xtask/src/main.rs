use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::Value;

mod references;

const SOURCES: &[&str] =
    &["https://static.cfx.re/natives/natives.json", "https://static.cfx.re/natives/natives_cfx.json"];

struct Native {
    side: char,
    ns: String,
    hash: String,
    returns: Vec<String>,
    params: Vec<(String, String)>,
    alias_of: Option<String>,
    docs: String,
    /// The signature of a server native that shares the name of a client one but differs from it,
    /// as `GetAllVehicles` returns a table on the server and a count on the client.
    server: Option<ServerSignature>,
}

struct ServerSignature {
    hash: String,
    returns: Vec<String>,
    params: Vec<(String, String)>,
}

fn main() {
    let task = std::env::args().nth(1).unwrap_or_default();
    match task.as_str() {
        "natives" => generate_natives(),
        "references" => {
            if let Err(error) = references::generate(std::env::args().skip(2)) {
                eprintln!("reference generation failed: {error}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: cargo xtask natives | references [--pinned]");
            std::process::exit(2);
        }
    }
}

fn generate_natives() {
    let mut natives: BTreeMap<String, Native> = BTreeMap::new();
    for url in SOURCES {
        eprintln!("fetching {url}");
        let body = ureq::get(url).call().expect("request failed").into_string_with_limit();
        let json: Value = serde_json::from_str(&body).expect("invalid natives json");
        for (ns, entries) in json.as_object().expect("namespace map") {
            for (hash, native) in entries.as_object().expect("native map") {
                add_native(&mut natives, ns, hash, native);
            }
        }
    }

    let mut signatures = String::new();
    let mut docs = String::new();
    let param_list = |params: &[(String, String)]| -> String {
        params.iter().map(|(n, t)| format!("{n}:{t}")).collect::<Vec<_>>().join(",")
    };
    for (name, native) in &natives {
        write!(
            signatures,
            "{name}\t{}\t{}\t{}\t{}\t{}\t{}",
            native.side,
            native.ns,
            native.hash,
            native.returns.join(","),
            param_list(&native.params),
            native.alias_of.as_deref().unwrap_or("")
        )
        .unwrap();
        if let Some(server) = &native.server {
            write!(signatures, "\t{}\t{}\t{}", server.hash, server.returns.join(","), param_list(&server.params))
                .unwrap();
        }
        signatures.push('\n');
        if native.alias_of.is_none() && !native.docs.is_empty() {
            writeln!(docs, "{name}\t{}", escape(&native.docs)).unwrap();
        }
    }

    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crates/qbx_fivem_data/data");
    std::fs::create_dir_all(&data_dir).unwrap();
    // Natives are only ever added, so a much shorter list means a broken or partial download.
    let previous = std::fs::read_to_string(data_dir.join("natives.tsv")).map_or(0, |text| text.lines().count());
    if natives.len() * 10 < previous * 9 {
        eprintln!("refusing to replace {previous} natives with only {}; the sources look incomplete", natives.len());
        std::process::exit(1);
    }
    std::fs::write(data_dir.join("natives.tsv"), &signatures).unwrap();
    std::fs::write(data_dir.join("natives_docs.tsv"), &docs).unwrap();
    eprintln!(
        "wrote {} natives ({} KiB signatures, {} KiB docs)",
        natives.len(),
        signatures.len() / 1024,
        docs.len() / 1024
    );
}

trait BodyExt {
    fn into_string_with_limit(self) -> String;
}

impl BodyExt for ureq::Response {
    fn into_string_with_limit(self) -> String {
        let mut body = String::new();
        std::io::Read::read_to_string(&mut self.into_reader(), &mut body).expect("read body");
        body
    }
}

/// Natives whose declaration is filed under RedM although the FiveM client has them too.
const ALSO_IN_FIVEM: &[&str] = &["REGISTER_RAW_KEYMAP"];

fn add_native(natives: &mut BTreeMap<String, Native>, ns: &str, hash: &str, native: &Value) {
    let game = native["game"].as_str();
    let declared_name = native["name"].as_str().unwrap_or("");
    if matches!(game, Some("rdr3" | "ny")) && !ALSO_IN_FIVEM.contains(&declared_name) {
        return;
    }
    let raw_name = native["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(hash);
    let name = lua_name(raw_name);
    let side = match native["apiset"].as_str() {
        Some("server") => 's',
        Some("shared") => 'b',
        _ => 'c',
    };

    let mut params = Vec::new();
    let mut out_types = Vec::new();
    let mut param_docs = String::new();
    let declared: Vec<&Value> = native["params"].as_array().into_iter().flatten().collect();
    let type_of = |param: &Value| param["type"].as_str().unwrap_or("Any").to_string();
    // FiveM's Lua wrappers (ext/natives/codegen_out_lua.lua) return the value of every pointer after
    // the result, and take a pointer as an argument only when it is the native's one pointer and its
    // last parameter, as an initial value that nil leaves 0.
    let pointers = declared.iter().filter(|param| is_pointer(&type_of(param))).count();
    let last_is_pointer = declared.last().is_some_and(|param| is_pointer(&type_of(param)));
    let takes_pointer = pointers == 1 && last_is_pointer;
    for param in &declared {
        let param_name = sanitize_param(param["name"].as_str().unwrap_or("arg"));
        let ty = type_of(param);
        if is_pointer(&ty) {
            out_types.push(pointer_type(&ty));
            if takes_pointer {
                // A native that releases the handle it is given does nothing without one.
                let optional = if consumes_handle(raw_name) { "" } else { "?" };
                params.push((format!("{param_name}{optional}"), pointer_type(&ty)));
            }
        } else {
            params.push((param_name.clone(), lua_type(&ty)));
        }
        if let Some(desc) = param["description"].as_str().filter(|d| !d.trim().is_empty()) {
            writeln!(param_docs, "- `{param_name}`: {}", desc.trim().replace('\n', " ")).unwrap();
        }
    }

    let mut returns = Vec::new();
    let result = native["results"].as_str().unwrap_or("void");
    if result != "void" {
        returns.push(lua_type(result));
    }
    returns.extend(out_types);

    let mut docs = native["description"].as_str().unwrap_or("").trim().to_string();
    if !param_docs.is_empty() {
        write!(docs, "\n\n**Parameters**\n{}", param_docs.trim_end()).unwrap();
    }
    if let Some(desc) = native["resultsDescription"].as_str().filter(|d| !d.trim().is_empty()) {
        write!(docs, "\n\n**Returns** {}", desc.trim()).unwrap();
    }
    let lua_example = native["examples"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["lang"] == "lua")
        .and_then(|e| e["code"].as_str());
    if let Some(code) = lua_example {
        write!(docs, "\n\n```lua\n{}\n```", code.trim()).unwrap();
    }

    for alias in native["aliases"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let alias_name = lua_name(alias);
        if alias_name != name {
            if let Some(existing) = natives.get_mut(&alias_name) {
                if existing.side != side {
                    existing.side = 'b';
                }
                continue;
            }
            natives.entry(alias_name).or_insert_with(|| Native {
                side,
                ns: ns.to_string(),
                hash: hash.to_string(),
                returns: returns.clone(),
                params: params.clone(),
                alias_of: Some(name.clone()),
                docs: String::new(),
                server: None,
            });
        }
    }

    let entry = Native {
        side,
        ns: ns.to_string(),
        hash: hash.to_string(),
        returns,
        params,
        alias_of: None,
        docs,
        server: None,
    };
    match natives.get(&name) {
        Some(existing) if existing.alias_of.is_none() && existing.side != side => {
            let existing = natives.get_mut(&name).unwrap();
            // A server native named like a client one keeps its own signature for server code.
            if existing.side == 'c' && !same_signature(existing, &entry) {
                let Native { hash, returns, params, .. } = entry;
                existing.server = Some(ServerSignature { hash, returns, params });
            }
            existing.side = 'b';
        }
        Some(existing) if existing.alias_of.is_none() => {}
        Some(existing) => {
            let side = if existing.side == side { side } else { 'b' };
            natives.insert(name, Native { side, ..entry });
        }
        None => {
            natives.insert(name, entry);
        }
    }
}

/// Whether two declarations of a native take and return values of the same types, whatever their
/// parameters are named.
fn same_signature(a: &Native, b: &Native) -> bool {
    let types = |native: &Native| native.params.iter().map(|(_, ty)| ty.clone()).collect::<Vec<_>>();
    a.returns == b.returns && types(a) == types(b)
}

/// Mirrors the name mangling of the official FiveM Lua native codegen.
fn lua_name(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase().replace("0x", "n_0x");
    let mut out = String::with_capacity(lower.len());
    let mut chars = lower.chars().peekable();
    while let Some(c) = chars.next() {
        match chars.peek() {
            Some(next) if c == '_' && next.is_ascii_alphabetic() => {
                out.push(next.to_ascii_uppercase());
                chars.next();
            }
            _ => out.push(c),
        }
    }
    if let Some(first) = out.chars().next().filter(char::is_ascii_alphabetic) {
        out.replace_range(..1, &first.to_ascii_uppercase().to_string());
    }
    out
}

/// Whether a parameter of this type is a pointer, whose value the Lua wrapper returns: a `char*` is
/// a string.
fn is_pointer(ty: &str) -> bool {
    ty.ends_with('*') && ty != "char*"
}

/// The type of the value a pointer gives back. The wrapper reads an `Any*` as an integer.
fn pointer_type(ty: &str) -> String {
    match ty {
        "Any*" => "integer".into(),
        other => lua_type(other.trim_end_matches('*')),
    }
}

fn consumes_handle(raw_name: &str) -> bool {
    raw_name.starts_with("DELETE_") || raw_name.starts_with("REMOVE_") || raw_name.contains("_AS_NO_LONGER_NEEDED")
}

fn lua_type(ty: &str) -> String {
    match ty {
        "int" | "long" | "uint" | "Hash*" | "int*" => "integer".into(),
        "float" | "float*" => "number".into(),
        "BOOL" | "bool" | "BOOL*" => "boolean".into(),
        "char*" => "string".into(),
        "Vector3" | "Vector3*" => "vector3".into(),
        "Any" | "Any*" => "any".into(),
        "func" => "function".into(),
        "object" => "table".into(),
        other => other.trim_end_matches('*').to_string(),
    }
}

fn sanitize_param(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    match cleaned.as_str() {
        "" => "arg".into(),
        "end" | "repeat" | "function" | "local" | "in" | "until" | "then" | "nil" | "true" | "false" | "and" | "or"
        | "not" | "if" | "else" | "elseif" | "for" | "while" | "do" | "return" | "break" | "goto" => {
            format!("_{cleaned}")
        }
        _ => cleaned,
    }
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\r', "").replace('\n', "\\n").replace('\t', " ")
}
