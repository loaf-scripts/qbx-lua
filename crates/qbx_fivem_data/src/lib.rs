use std::sync::OnceLock;

#[cfg(feature = "docs")]
mod references;
#[cfg(feature = "docs")]
pub use references::{
    control, controls, ped_config_flag, ped_config_flags, Control, PedConfigFlag, CONTROLS_SOURCE_URL,
    PED_CONFIG_FLAGS_SOURCE_URL,
};

static NATIVES: &str = include_str!("../data/natives.tsv");
#[cfg(feature = "docs")]
static NATIVE_DOCS: &str = include_str!("../data/natives_docs.tsv");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Client,
    Server,
    Shared,
}

impl Side {
    pub fn is_available_on(self, script_side: Side) -> bool {
        self == Side::Shared || script_side == Side::Shared || self == script_side
    }

    pub fn label(self) -> &'static str {
        match self {
            Side::Client => "client",
            Side::Server => "server",
            Side::Shared => "shared",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Native {
    pub name: &'static str,
    pub side: Side,
    pub namespace: &'static str,
    pub hash: &'static str,
    returns: &'static str,
    params: &'static str,
    pub alias_of: Option<&'static str>,
    /// The hash, returned values and parameters of the server native of the same name, when they
    /// differ from those of the client one: `GetAllVehicles` returns a table on the server.
    server: Option<(&'static str, &'static str, &'static str)>,
}

impl Native {
    fn from_line(line: &'static str) -> Option<Self> {
        let mut cols = line.split('\t');
        let name = cols.next()?;
        let side = match cols.next()? {
            "c" => Side::Client,
            "s" => Side::Server,
            _ => Side::Shared,
        };
        let namespace = cols.next()?;
        let hash = cols.next()?;
        let returns = cols.next()?;
        let params = cols.next()?;
        let alias_of = cols.next().filter(|a| !a.is_empty());
        let server = match (cols.next(), cols.next(), cols.next()) {
            (Some(hash), Some(returns), Some(params)) => Some((hash, returns, params)),
            _ => None,
        };
        Some(Self { name, side, namespace, hash, returns, params, alias_of, server })
    }

    /// The native that code on `side` calls: on the server, the server native of the same name
    /// when its signature differs from the client one.
    pub fn on(self, side: Option<Side>) -> Self {
        match (side, self.server) {
            (Some(Side::Server), Some((hash, returns, params))) => {
                Self { side: Side::Server, namespace: "CFX", hash, returns, params, server: None, ..self }
            }
            _ => self,
        }
    }

    pub fn returns(&self) -> impl Iterator<Item = &'static str> {
        self.returns.split(',').filter(|r| !r.is_empty())
    }

    /// `(name, type, optional)` triples. The table writes an optional parameter `name?`: the
    /// pointer that the Lua wrapper takes as an initial value, which nil leaves 0.
    pub fn params(&self) -> impl Iterator<Item = (&'static str, &'static str, bool)> {
        self.params.split(',').filter_map(|p| p.split_once(':')).map(|(name, ty)| match name.strip_suffix('?') {
            Some(name) => (name, ty, true),
            None => (name, ty, false),
        })
    }

    pub fn signature(&self) -> String {
        let params: Vec<String> =
            self.params().map(|(n, t, optional)| format!("{n}{}: {t}", if optional { "?" } else { "" })).collect();
        let returns: Vec<&str> = self.returns().collect();
        let mut out = format!("function {}({})", self.name, params.join(", "));
        if !returns.is_empty() {
            out.push_str(": ");
            out.push_str(&returns.join(", "));
        }
        out
    }
}

struct LineTable {
    text: &'static str,
    starts: Vec<u32>,
}

impl LineTable {
    fn new(text: &'static str) -> Self {
        let mut starts = vec![0u32];
        starts.extend(text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i as u32 + 1));
        if starts.last().is_some_and(|&s| s as usize >= text.len()) {
            starts.pop();
        }
        Self { text, starts }
    }

    fn line(&self, index: usize) -> &'static str {
        let start = self.starts[index] as usize;
        let end = self.starts.get(index + 1).map_or(self.text.len(), |&e| e as usize);
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }

    fn key(line: &str) -> &str {
        line.split('\t').next().unwrap_or(line)
    }

    fn find(&self, key: &str) -> Option<&'static str> {
        let (mut lo, mut hi) = (0usize, self.starts.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            let line = self.line(mid);
            match Self::key(line).cmp(key) {
                std::cmp::Ordering::Equal => return Some(line),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }
}

fn native_table() -> &'static LineTable {
    static TABLE: OnceLock<LineTable> = OnceLock::new();
    TABLE.get_or_init(|| LineTable::new(NATIVES))
}

pub fn native(name: &str) -> Option<Native> {
    native_table().find(name).and_then(Native::from_line)
}

pub fn natives() -> impl Iterator<Item = Native> {
    let table = native_table();
    (0..table.starts.len()).filter_map(|i| Native::from_line(table.line(i)))
}

pub fn native_count() -> usize {
    native_table().starts.len()
}

/// Every native is also callable through its hash as `N_0x...`, whether or not it is documented.
pub fn is_hash_native_name(name: &str) -> bool {
    name.strip_prefix("N_0x").is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(feature = "docs")]
pub fn native_docs(name: &str) -> Option<String> {
    static TABLE: OnceLock<LineTable> = OnceLock::new();
    let table = TABLE.get_or_init(|| LineTable::new(NATIVE_DOCS));
    let target = native(name).and_then(|n| n.alias_of).unwrap_or(name);
    let line = table.find(target)?;
    let (_, escaped) = line.split_once('\t')?;
    let mut out = String::with_capacity(escaped.len());
    let mut chars = escaped.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    Some(out)
}

pub struct Stub {
    pub name: &'static str,
    pub side: Side,
    /// Whether it declares Lua standard libraries rather than CfxLua additions.
    pub library: bool,
    pub source: &'static str,
}

const fn stub(name: &'static str, side: Side, library: bool, source: &'static str) -> Stub {
    Stub { name, side, library, source }
}

/// The runtime definitions. FiveM only opens the `io` and `os` libraries on the server
/// (`IS_FXSERVER` in citizen-scripting-lua's `LuaScriptRuntime.cpp`), so `lua54_server.lua` holds
/// them; stubs extending a table come after the one declaring it.
pub static STUBS: &[Stub] = &[
    stub("lua54.lua", Side::Shared, true, include_str!("../stubs/lua54.lua")),
    stub("lua54_server.lua", Side::Server, true, include_str!("../stubs/lua54_server.lua")),
    stub("cfx.lua", Side::Shared, false, include_str!("../stubs/cfx.lua")),
    stub("glm.lua", Side::Shared, false, include_str!("../stubs/glm.lua")),
    stub("cfx_client.lua", Side::Client, false, include_str!("../stubs/cfx_client.lua")),
    stub("cfx_server.lua", Side::Server, false, include_str!("../stubs/cfx_server.lua")),
    stub("cfx_events.lua", Side::Shared, false, EVENTS_STUB),
];

static EVENTS_STUB: &str = include_str!("../stubs/cfx_events.lua");

/// An event that FiveM itself triggers, such as `onResourceStop` or `playerDropped`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinEvent {
    pub name: &'static str,
    /// The only side that triggers it, or `Shared` when both do.
    pub side: Side,
}

/// The events that FiveM itself triggers, as the `---@field` lines of the `CfxEvents` class in
/// `cfx_events.lua` declare them: a `(client)` or `(server)` before the name scopes one to that side.
pub fn builtin_events() -> &'static [BuiltinEvent] {
    static EVENTS: OnceLock<Vec<BuiltinEvent>> = OnceLock::new();
    EVENTS.get_or_init(|| {
        let class = EVENTS_STUB.lines().skip_while(|line| *line != "---@class CfxEvents").skip(1);
        class
            .map_while(|line| line.strip_prefix("---@field "))
            .filter_map(|field| {
                let (side, field) = match field.split_once(") ") {
                    Some(("(client", rest)) => (Side::Client, rest),
                    Some(("(server", rest)) => (Side::Server, rest),
                    _ => (Side::Shared, field),
                };
                Some(BuiltinEvent { name: field.split(' ').next()?, side })
            })
            .collect()
    })
}

/// The event `name` when FiveM itself triggers it.
pub fn builtin_event(name: &str) -> Option<BuiltinEvent> {
    builtin_events().iter().find(|event| event.name == name).copied()
}

pub struct KnownImport {
    pub path: &'static str,
    pub globals: &'static [&'static str],
}

/// Globals provided by commonly imported `@resource/file.lua` scripts, used when the
/// providing resource is not part of the linted tree (the usual case in single-repo CI).
pub static KNOWN_IMPORTS: &[KnownImport] = &[
    KnownImport {
        path: "@ox_lib/init.lua",
        globals: &["lib", "cache", "locale", "require", "SetInterval", "ClearInterval"],
    },
    KnownImport { path: "@oxmysql/lib/MySQL.lua", globals: &["MySQL"] },
    KnownImport { path: "@qbx_core/modules/lib.lua", globals: &["qbx"] },
    KnownImport { path: "@qbx_core/modules/playerdata.lua", globals: &["QBX"] },
    KnownImport { path: "@qbx_core/shared/locale.lua", globals: &["Lang", "Locale"] },
    KnownImport { path: "@qb-core/shared/locale.lua", globals: &["Lang", "Locale"] },
    KnownImport { path: "@es_extended/imports.lua", globals: &["ESX"] },
    KnownImport { path: "@es_extended/locale.lua", globals: &["Locales", "Translate", "TranslateCap", "_U", "_"] },
    KnownImport { path: "@ox_core/lib/init.lua", globals: &["Ox"] },
    KnownImport { path: "@ox_core/imports/client.lua", globals: &["Ox", "player", "NetEventHandler"] },
    KnownImport { path: "@ox_core/imports/server.lua", globals: &["Ox"] },
    KnownImport { path: "@PolyZone/client.lua", globals: &["PolyZone"] },
    KnownImport { path: "@PolyZone/BoxZone.lua", globals: &["BoxZone"] },
    KnownImport { path: "@PolyZone/CircleZone.lua", globals: &["CircleZone"] },
    KnownImport { path: "@PolyZone/ComboZone.lua", globals: &["ComboZone"] },
    KnownImport { path: "@PolyZone/EntityZone.lua", globals: &["EntityZone"] },
    KnownImport { path: "@menuv/menuv.lua", globals: &["MenuV"] },
    KnownImport { path: "@mysql-async/lib/MySQL.lua", globals: &["MySQL"] },
    KnownImport { path: "@async/async.lua", globals: &["Async"] },
];

pub fn known_import(path: &str) -> Option<&'static KnownImport> {
    KNOWN_IMPORTS.iter().find(|import| import.path.eq_ignore_ascii_case(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_up_natives() {
        let native = native("GetEntityCoords").unwrap();
        assert_eq!(native.returns().collect::<Vec<_>>(), ["vector3"]);
        assert_eq!(native.params().next(), Some(("entity", "Entity", false)));
        assert_eq!(super::native("GetPlayerIdentifier").unwrap().side, Side::Server);
        assert!(super::native("NotARealNative").is_none());
        assert!(native_count() > 6000);
    }

    #[test]
    fn server_natives_keep_their_own_signature() {
        let vehicles = native("GetAllVehicles").unwrap();
        assert_eq!(vehicles.on(Some(Side::Client)).returns().collect::<Vec<_>>(), ["integer", "integer"]);
        assert_eq!(vehicles.on(None).returns().collect::<Vec<_>>(), ["integer", "integer"]);
        let server = vehicles.on(Some(Side::Server));
        assert_eq!(server.returns().collect::<Vec<_>>(), ["table"]);
        assert_eq!((server.side, server.namespace, server.hash), (Side::Server, "CFX", "0x332169F5"));
        let weapon = native("GetCurrentPedWeapon").unwrap().on(Some(Side::Server));
        assert_eq!(weapon.signature(), "function GetCurrentPedWeapon(ped: Ped): Hash");
        // A native without a server signature of its own is the same everywhere.
        let health = native("GetEntityHealth").unwrap();
        assert_eq!(health.on(Some(Side::Server)).hash, health.hash);
    }

    #[test]
    fn pointers_follow_the_lua_wrappers() {
        let shape = |name: &str| {
            let native = native(name).unwrap();
            (native.params().collect::<Vec<_>>(), native.returns().collect::<Vec<_>>())
        };
        // Two pointers: both are returned, neither is an argument.
        assert_eq!(shape("GetGroupSize"), (vec![("groupID", "integer", false)], vec!["integer", "integer"]));
        // The one pointer, last: an optional initial value, and returned.
        assert_eq!(
            shape("GetEntityPlayerIsFreeAimingAt"),
            (vec![("player", "Player", false), ("entity", "Entity", true)], vec!["boolean", "Entity"])
        );
        // A native that releases the handle it is given needs one.
        assert_eq!(shape("DeleteEntity"), (vec![("entity", "Entity", false)], vec!["Entity"]));
        let server = native("DeleteEntity").unwrap().on(Some(Side::Server));
        assert_eq!(server.returns().count(), 0, "the server native takes the handle itself");
        // The wrapper reads an Any* as an integer, and passes none it does not take.
        assert_eq!(shape("DataarrayGetInt"), (vec![("arrayIndex", "integer", false)], vec!["integer", "integer"]));
        assert_eq!(
            native("GetEntityPlayerIsFreeAimingAt").unwrap().signature(),
            "function GetEntityPlayerIsFreeAimingAt(player: Player, entity?: Entity): boolean, Entity"
        );
    }

    #[test]
    fn natives_are_sorted_for_binary_search() {
        let names: Vec<&str> = natives().map(|n| n.name).collect();
        assert!(names.windows(2).all(|w| w[0] < w[1]));
        assert!(names.iter().all(|n| native(n).is_some()));
    }

    #[test]
    fn hash_names() {
        assert!(is_hash_native_name("N_0xabcdef12"));
        assert!(!is_hash_native_name("N_0x"));
        assert!(!is_hash_native_name("Foo"));
    }

    #[test]
    fn builtin_events_keep_their_side() {
        let side = |name| builtin_event(name).map(|event| event.side);
        assert_eq!(side("onResourceStop"), Some(Side::Shared));
        assert_eq!(side("onClientResourceStart"), Some(Side::Client));
        assert_eq!(side("playerDropped"), Some(Side::Server));
        assert_eq!(side("rconCommand"), Some(Side::Server));
        assert_eq!(side("setModel"), None, "the fields of other classes are no events");
        assert_eq!(builtin_events().len(), 30);
    }

    #[cfg(feature = "docs")]
    #[test]
    fn docs_are_unescaped() {
        let docs = native_docs("GetEntityCoords").unwrap();
        assert!(docs.contains("coordinates"));
        assert!(docs.contains('\n'));
    }
}
