# qbx-lua-ls

The FiveM Lua language server included in the [qbx-lua tooling workspace](../../README.md).
It lives in `crates/qbx_lua_ls` alongside the `qbx-lint` CLI and their shared parser, formatter,
analysis and FiveM data crates. It reads `fxmanifest.lua` to resolve resource imports and
client/server scripts, uses LuaCATS annotations for editor help, and provides the same lint
rules as the CLI.

The former standalone `Qbox-project/qbx-lua-ls` repository is archived. Development, issues,
pull requests and releases for both tools now live in
[Qbox-project/qbx-lua](https://github.com/Qbox-project/qbx-lua). The original Git history is
preserved; see [what moved and how](../../docs/repository-migration.md).

The server communicates over standard input and output using the Language Server Protocol
(LSP). Editor integrations and setup instructions live in
[qbx-editor](https://github.com/Qbox-project/qbx-editor/blob/main/docs/editors.md).
Available features depend on the editor's LSP client.

## Features

- Completion and hover for Lua symbols, FiveM natives, exports, events and callbacks.
- Call snippets for functions that take a callback, with the function literal written out.
- The values a parameter lists, through aliases, `---|` lines and `@overload`s, as soon as its
  argument starts: strings, integers, `true` and `false`, and `nil` beside them. Strings are also
  offered inside the argument's string, and a function literal with the matching parameters where
  an argument takes one. Call snippets stop in the quotes of listed strings, and end after a value
  that picks an overload, so the handler written next matches it. The `# description` of a value
  on a `---|` line under an `@alias` or `@param` comes with it. Hovers of the alias, or of the
  function for its `@param`s and `@return`s, list the described values. The members of an
  `---@enum` come before its values, written as the code reaches its table, like `Colors.Red`.
- The same values where a value of such a type, like `"busy"|"ready"`, `1|2|3` or `boolean`, is
  assigned, set as a field of a class, returned or compared with `==` or `~=`, and the function
  literal where a function is assigned or returned. They open on the space typed after the `=`,
  `==`, `~=` or `return`, beside the names in scope once a word is typed.
- `---@` continuation on Enter inside LuaCATS annotation blocks, through on-type formatting.
- Reference hovers for literal control IDs in PAD natives and ped configuration flags
  in `SetPedConfigFlag` / `GetPedConfigFlag`, using bundled Cfx documentation.
- Optional read-only reference search/detail requests for native, control and ped flag browsers,
  with filters, pagination, source links and Lua insertion templates.
- Definitions, references and rename for locals, globals and fields, including static string
  keys such as `Config['name']` and supported `---@field` declarations.
- Hover and definitions for the classes, aliases and enums named in LuaCATS annotations.
- Go to type definition: the `---@class`, `---@alias` or `---@enum` of a value's type, through
  unions, arrays, `table<K, V>` and what a function returns, and the classes an alias of a union
  stands for. Go to implementation: where code sets a field or method, without the `---@field`
  lines that declare it.
- Classes written with metatables and no annotations: what `setmetatable({}, Class)` returns, and
  a table given a metatable by `setmetatable`, have the fields of its `__index` table at every
  level, as `Class.__index = Class` sets up, for completion, hover and navigation. A `---@class`
  keeps the fields it declares.
- The `---@operator` lines of a class type what operations on its values give, as
  `---@operator add(Vec): Vec` makes `a + b` a `Vec`, for an operand of the type the line names,
  on either side. A class does not take the operators of its parents, as Lua looks a metamethod up
  in the metatable itself, and `@operator call` types calling a value when no `@overload` does.
- Exports typed by LuaLS definitions, as `---@type PhoneExports` above `exports.phone = {}`, or
  `---@class qbx_core` above `exports.qbx_core = {}` with `function exports.qbx_core:GetCid()`
  methods: `exports.phone` and `exports['phone']` take the members of the declared type before the
  exports the resource registers, also for resources the workspace lacks.
- Type guards: after `if not name then return end`, inside `if name then`, and in the other
  [guarded code](#type-guards), a local is no longer `nil` or `false`, and `type(data) == "table"`
  or `state == "busy"` narrow it to that kind or value. A guard on one value of
  `local ok, err = f()` also narrows the others, for functions that return
  [sets of values](#sets-of-returned-values) such as `false | (string, string)`, and a
  [`---@cast`](#casts) changes the type of a local from its line on.
- Diagnostics and quick fixes with resource and client/server context, plus LuaCATS type checks:
  `missing-fields` and `assign-type-mismatch` for tables and assignments that leave out required
  fields of their class or store a value of the wrong type, in a field or in a variable typed with
  `---@type` or `@param`, `param-type-mismatch` for arguments of the wrong type, `undeclared-field`
  for fields and keys a `---@class (strict)` does not declare, `return-type-mismatch`,
  `missing-return` and `redundant-return-value` for functions, including callback handlers, that
  do not return what their `@return`, or the function type they are passed as, declares, and
  `discard-returns` for calls that drop the
  values of a `@nodiscard` function. `invisible` reports the use of fields and methods a class
  keeps `private`, `protected` or `package` from outside its class, subclasses or file, and
  completion leaves them out there.
  `impossible-comparison` reports an `==` or `~=` between values whose declared types share no
  value, such as `GetState() == "invalid"` for a function that returns `"active"|"busy"`, and
  `cast-type-mismatch` a `---@cast` to a type the local is not declared to take. The opt-in
  `no-unknown` reports parameters, locals and loop variables that have no type.
- Signature help, parameter hints, semantic tokens, folding and document/workspace symbols.
- QB-Core and ESX server callback completion, navigation and payload hints from local handlers.
- Callback systems a resource wraps itself, declared with `---@callback`, with name completion,
  payload hints, response types, and `missing-parameter` and `redundant-parameter` for payloads
  that leave out a value the handler requires or pass more than it takes.
- References on event and callback names: every registration and trigger of the name. Go to
  definition on a registration returns the registration itself, so editors such as VS Code show
  the calls that trigger it.
- Whole-document formatting, configured through `qbxlint.toml`.
- Completion for manifest paths, locale keys, convars, state bag keys and LuaCATS annotations.
- Read-only resource, dependency-health, NUI callback and asset-reference requests
  for editor tabs, plus paginated diagnostic and symbol-reference queries for assistants.

Custom requests are documented in the [protocol reference](docs/protocol.md).
The VS Code asset/utility interfaces and assistant MCP adapter live in `qbx-editor`.

Opening a resource also indexes dependencies and imported scripts found in sibling resource
folders. Add other locations through the `library` setting. As LuaLS reads its library, files there
that belong to no resource, and workspace files outside resources with `---@meta` above their
first statement, are definition files: their globals, classes and export types reach every
resource, on the side their names give and only through the imports of the resource their folder
is named after, while a resource's own declarations of a name come first. The
[reference](../../docs/reference.md#definition-files-outside-resources) has the details.

Hovering `38` in `IsControlJustPressed(0, 38)` shows `INPUT_PICKUP` and its default
QWERTY/Xbox bindings. Ped configuration flag hovers show the documented symbol;
undocumented behavior is identified as such. References work offline and include
source links. ID hovers apply only to recognized native arguments, not variables,
calculated expressions or functions that shadow the native.

## Client and server annotations

A `(server)` or `(client)` attribute scopes a LuaCATS declaration to one side:

```lua
---@class (server) BankAccount
---@field (client) hud table
---@alias (client) Key 'E'|'F'
---@enum (server) Jobs
---@overload (server) fun(source: integer, message: string)
```

Overloads follow the side of the call, including `IsDuplicityVersion()` and `lib.context`
guards. A function `@field` that repeats the name of an unscoped one is another signature of it,
as in LuaLS, and `---@field (server) Name fun(...)` repeated adds a signature for server calls
only. Classes, fields, aliases and enums follow the manifest side of the file. Shared files and
files of an unknown side see both sides. A `side` in a `qbxlint.toml` override gives a side
to files a loader runs without a manifest entry. Naming a type in a script of a side that only
the other side declares is reported as `undefined-doc-name`.

## Type guards

A condition narrows the type of the locals it tests in the code that only runs when it held or
failed:

```lua
local name = GetName() -- string?
if not name then
    return -- name is nil here
end
print(name) -- string
```

Guards are `name`, comparisons with `nil`, `true`, `false`, a string or an integer, `type(name)`
and `math.type(name)` compared with a name they give, reads from the local, and those joined by
`and`, `or` and `not`. A read such as `name.job`, `name?.job`, `name[key]` or `name:get()` is only
true when `name` holds a value, and so are `name?.job == "police"` and `name?.job ~= nil`.
They apply to the branches of an `if` or `elseif`, to the code after an `if` whose other branches
all end in `return`, `error(...)`, `break` or `goto`, to the body of a `while`, to the right side of
`and` and `or`, and to the code after `assert(name)`. A local that is assigned again after its
declaration is only narrowed by the guards after a [`---@cast`](#casts) of it, since a guard says
nothing about the new value, and globals and fields are not narrowed.

A comparison with a literal narrows the local to that literal: inside `if state == "busy" then`, a
`"active"|"busy"|nil` and a `string` are both `"busy"`, and in the `else` branch the first is
`"active"|nil`. `type(name)` keeps the values of the kind it names. Classes count as tables, and
the `vector2`, `vector3`, `vector4`, `quat` and `matrix` of CfxLua go by their own names:

```lua
---@param data PlayerData|string|nil
local function load(data)
    if type(data) ~= "table" then
        return
    end
    print(data) -- PlayerData
end
```

A value of no known type takes the kind it is checked for, and so does one whose type has none of
that kind, since such a check handles values the annotations leave out: inside
`if type(count) == "string" then`, an `integer` is a `string`. `math.type(name)` tells an `integer`
from a `float`, and `kind == "table"` after `local kind = type(name)` narrows like
`type(name) == "table"`. Only the global `type` counts, also through a local that holds it, as
after `local type = type`, and not `table.type` of ox_lib. Where one of
several checks held, as inside `if type(value) == "string" or type(value) == "number" then`, the
local is of the kinds they let through.

### Casts

A `---@cast` line changes the type of a local from its line on, as in lua-language-server:

```lua
local data = json.decode(payload)
---@cast data PlayerData
```

`---@cast name T` makes it a `T`, `+T` adds `T` to it, `-T` takes `T` out, `+?` and `-?` add and
take out `nil`, and one line can list several, as in `---@cast value +?, -string`. Adding to a local
whose type is unknown leaves a value that may still be anything else, as `string|unknown` for
`+string`, as in lua-language-server. A cast holds to
the end of the block of the code after it, including the functions defined there, and until the
statement that assigns the local again. Unlike lua-language-server, a cast inside an `if` ends with
its branch, except on the last line of the branch: there it holds after the `if`, as it does in
lua-language-server, which is how code types what the branch leaves:

```lua
if type(translation) == "table" then
    translation = translation[1]
    ---@cast translation string
end
print(translation) -- string
```

On the last line of another block, such as the body of a function or a loop, a cast holds nowhere.
Casts also apply to locals that are assigned again, and only to locals. A guard that
took effect before a cast tells nothing about the type it gives, while one after it narrows that
type. `+T` and `-T` change the type the guards around their line leave instead, so
`---@cast items +number[]` inside `if items then` keeps out the `nil` of a `string[]?`.
`cast-type-mismatch` reports a cast to a type the declared type of the local does not take,
such as `---@cast count string` for an `integer`; see the
[reference](../../docs/reference.md#casts).

### Sets of returned values

A function often returns either one set of values or another: `false` when it fails, and
otherwise two names. `@return` lists such sets separated by `|`, with the values of a set in
parentheses:

```lua
---@return false | (string, string)
local function GetName()
    if math.random(1, 2) == 2 then
        return false
    end
    return "Joe", "Doe"
end

local firstname, lastname = GetName() -- false|string, string?
if not firstname then
    return -- lastname is nil here
end
print(firstname, lastname) -- string, string
```

The locals that one call declares are narrowed together: a guard on one of them rules out the
sets that do not fit it, and the others are read from the sets that are left. Without the guard,
each holds what its position has across the sets, and `nil` for a set that ends before it.

An undocumented function gets its sets from its `return` statements, so the example works
without the annotation as well. The syntax is also accepted after the `:` of a `fun(...)` type. A
function lists its sets on one `@return` line; further `@return` lines add values and turn the
line into an ordinary list of types. This notation is an extension: LuaLS reads parentheses as
grouping one type.

## Framework callbacks

The server recognizes QB-Core `Functions.CreateCallback` / `Functions.TriggerCallback`
and ESX `RegisterServerCallback` / `TriggerServerCallback`. Receiver provenance comes
from standard framework export initialization (including local aliases), supported
framework imports or the framework's own resource. Arbitrary same-named tables are
not framework receivers. Manifest sides and `IsDuplicityVersion()` guards restrict
registrations to server code and triggers to client code.

Literal callback names complete and navigate to locally indexed registrations.
Payload signature help and inlay hints omit the handler's leading `source` and `cb`;
LuaCATS annotations on local functions supply types. QB-Core, ESX, ox_lib and native
events have separate name spaces. Conflicting payload definitions suppress derived
hints, while navigation still lists the matching registrations. Handler return
values are not treated as asynchronous callback responses.

No framework API documentation is downloaded or bundled. Client-callback/Await
variants and response-type inference are not included for these frameworks.
See the [convention and maintenance notes](docs/framework-callbacks.md) for source revisions and recognition limits.

A resource's own callback wrappers are declared with a `---@callback` tag:

```lua
---@callback register
---@param name string
---@param handler fun(source: integer, ...): ...
function RegisterServerCallback(name, handler) end

---@callback await
---@param name string
---@param ... any
function AwaitServerCallback(name, ...) end
```

Calls to `AwaitServerCallback('name', ...)` then complete registered names and show
the handler's payload parameters and return type, taken from the `---@param` and
`---@return` lines above each `RegisterServerCallback('name', function(source, ...) end)`.
A `---@callback trigger` wrapper passes the response to a function argument instead,
and an optional family name (`---@callback register shop`) keeps separate systems apart.

## Build and run

Download the `qbx-lua-ls-<target>` archive for your platform from the
[shared releases](https://github.com/Qbox-project/qbx-lua/releases). These releases also
contain `qbx-lint-<target>` CLI archives; choose the server archive for an LSP client.
Archives cover Windows x64, Linux x64/ARM64 (musl), and macOS x64/ARM64. You can also build
from source using the steps below.

Install stable Rust and clone the shared tooling workspace. The server uses the parser,
formatter, analysis and FiveM data crates from the same checkout.

```sh
git clone https://github.com/Qbox-project/qbx-lua.git
cd qbx-lua
cargo build --release --locked -p qbx_lua_ls
```

To install the server on `PATH` with Cargo instead, run
`cargo install --path crates/qbx_lua_ls --locked` from the workspace root.

The executable is `target/release/qbx-lua-ls`, or `target/release/qbx-lua-ls.exe` on Windows.
Put it on `PATH`, or configure its absolute path in your editor. Start it without arguments for
LSP over stdio; `--version` prints the version. It does not need a running FiveM server.

Use a resource folder or the server's `resources` folder as the editor workspace. Follow the
[editor setup instructions](https://github.com/Qbox-project/qbx-editor/blob/main/docs/editors.md)
to start the server from your editor.

## Configuration

Send this object directly as LSP `initializationOptions`:

```json
{
  "library": [],
  "diagnostics": {
    "enable": true,
    "workspace": true,
    "rules": {}
  },
  "inlayHints": { "enable": true },
  "semanticTokens": { "enable": true }
}
```

For `workspace/didChangeConfiguration`, put the same object in `settings.qbxLua` or directly in
`settings`. Restart the server after changing `library`. A discovered `qbxlint.toml` supplies
lint and formatting settings; its rule levels take precedence over the editor's `diagnostics.rules`.

The server relies on the editor for file-watch notifications. If the client does not send them,
send a `qbx/reindex` request with `null` parameters or restart after external file or manifest
changes. Open-document edits continue to update normally.

See the [client configuration and request reference](docs/protocol.md) for the full settings
and custom requests.

## Limits

- Types support completion, hover and navigation. The server does not check assignment types
  or provide full control-flow narrowing. Generic functions, classes and aliases take the types
  their arguments give them, but the constraints of their parameters, such as the `table` of
  `---@generic T: table`, are not checked.
- Field references depend on the inferred owner type. Computed keys and fields reached through
  unknown types may not be found; known declarations that cannot be edited safely prevent rename.
- Formatting applies to whole documents. Range formatting is not implemented.
- Encrypted scripts cannot be analyzed. Security diagnostics are heuristic checks, not proof
  that an event handler or resource is secure.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development checks and the
[releases page](https://github.com/Qbox-project/qbx-lua/releases) for release notes.

## License

[GPL-3.0-or-later](LICENSE).
