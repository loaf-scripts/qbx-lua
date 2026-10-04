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
  [`---@cast`](#casts) changes the type of a local from its line on. A local that is
  [assigned again](#locals-that-are-assigned-again) holds the values that reach each point, so
  `name = name or 'none'` leaves a `string`.
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
  `cast-type-mismatch` a `---@cast` to a type the local is not declared to take.
  `cast-local-type` reports an assignment that gives a local without an annotation a value of
  another type than the one it is declared with, such as `speed = "5"` after `local speed = 5`, as
  lua-language-server and TypeScript do. `inject-field`
  reports fields set through a value whose class or table type does not have them,
  `undefined-field` fields read from one, and
  `need-check-nil` a value that may be `nil` used where a missing value raises an error.
  `close-non-object` reports a `<close>` local whose value cannot be closed, such as a number, and
  `circle-doc-class` a `---@class` that inherits from itself through classes of any file.
  `deprecated` reports reads of globals, fields and methods whose definitions all have
  `---@deprecated` above them, in any file, with the reason it gives. The opt-in
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
guards. Fields and functions set on a table inside such a guard, as ox_lib sets its server
`lib.notify(playerId, data)`, only reach code of that side. A function `@field` that repeats the name of an unscoped one is another signature of it,
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
They apply to the branches of an `if` or `elseif`, failed ones to the conditions of the `elseif`s
after them, to the code after an `if` whose other branches all end in `return`, `error(...)`,
`break` or `goto`, to the body of a `while`, to the right side of `and` and `or`, and to the code
after `assert(name)`. A guard narrows what a local holds there, also one that is
[assigned again](#locals-that-are-assigned-again), and the [fields](#fields) of locals, while
globals are not narrowed.

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
`type(name) == "table"`, while `name` holds the value it held there and no other function assigns
it. Only the global `type` counts, also through a local that holds it, as
after `local type = type`, and not `table.type` of ox_lib. Where one of
several checks held, as inside `if type(value) == "string" or type(value) == "number" then`, the
local is of the kinds they let through.

Guards that rule out every value a local is declared to hold guard code for values the annotations
leave out, or code that never runs. There the local is what lua-language-server reads it as:

```lua
---@param entities number[]
local function target(entities)
    if type(entities) ~= "table" then
        entities = { entities } -- entities is unknown, not a number[], so nothing is reported
    end
end
```

`type(name) ~= "table"` for a table, and `if name` for a value that is always `nil`, leave it
`unknown`; `not count`, `count == nil` and the `else` of `count ~= nil` make a `number` `nil`; and a
comparison with a literal of a kind the local never has makes it that kind, as `count == false`
makes a `number` a `boolean`. After other comparisons with literals, as
`action ~= "open" and action ~= "close"` for an `"open"|"close"`, it keeps its declared type.

### Locals that are assigned again

A local that is assigned again holds, at each point, the values that may reach it there: that of its
declaration or of an assignment, each narrowed by the guards since, and those of the ways that meet
after an `if`, a loop or a label. An assignment gives its value from the end of its statement:

```lua
local name = GetName() -- string?
name = name or "none"
print(name) -- string

local label = GetName()
if not label then
    label = "none"
end
print(label) -- string
```

A literal is widened to its kind, as `mode = 'dev'` stores a `string`. A local declared without a
value holds `nil` until it is given one, and then makes way for the values it may be given, as in
lua-language-server: after `local found` and `if ok then found = item end`, `found` is the item. A
local with a `---@type` or `@param` keeps the parts of that type the value may be, as the `string` of
a `string?` for `"x"`, and all of it for a value of another kind, which `assign-type-mismatch`
reports, or of no known type. Other locals keep the type of their declaration for such a value. A
`---@type` above an assignment types the value it gives, as in lua-language-server, so a table
given again to a local declared with a table outside any function is still that local's own table
for the functions that run at other times. A loop starts with what the code before it and its runs before leave, and a label with what the
`goto`s to it leave.

Calls are taken to change no local, but a function runs any time after it is created. Inside it, a
local declared outside it may hold what it held where the function was created, and also what the
code after that and the other functions assign, with nothing known about those values. Once a
function that assigns a local is created, the code around it may find any value that a function
gives the local, as after `each(list, function(item) found = item end)`. A function written as an
argument of a runtime function that runs it later, as `SetTimeout`, `CreateThread`,
`AddEventHandler`, `RegisterNetEvent`, `RegisterCommand`, `RegisterNUICallback`,
`AddStateBagChangeHandler` and `exports` do, gives the locals around it nothing until a call
yields.

A call that yields lets that code run, as the event handlers of a script do while one of its threads
waits. After one, a local that other code assigns may hold any value that code gives it again, so a
guard before the call no longer tells about it:

```lua
local status = "pending"
RegisterNetEvent("job:done", function() status = "done" end)

CreateThread(function()
    while status ~= "done" do -- not always true: the handler may run while the thread waits
        Wait(0)
    end
end)
```

The calls that yield are those of `Wait`, `Citizen.Wait`, `Citizen.Await` and `coroutine.yield`,
those of a function read as `await`, as `lib.callback.await` and `MySQL.query.await`, and those of
the functions of the file that make such a call. Other functions that wait, such as `lib.progressBar`,
are taken to change no local.

### Fields

A guard on a field of a local, as `self.target`, `data.job.name` or `data["job"]`, narrows it like a
local, which lua-language-server does not, and an assignment to one gives it the part of its
declared type that the value is, as `entry.length = entry.length or 1` leaves a `number` of a
`number?`:

```lua
---@param data { job: Job? }
local function show(data)
    if data.job then
        print(data.job.name) -- data.job is a Job
    end
end
```

Reading through a field tells that it holds a value too, so `data.job.grade` inside
`if data.job.name then` reads from a `Job`. What a guard tells about a field holds until something
may change it: an assignment to the local, or to a field of the same name of any table, as
`other.job = nil` may set the same table through another name; a call that is given the local, a
table the field is read through, a local copied from one of them or a table built with them, or an
assignment to a key of one that is not known, as `reset(data)`, `data:reset()`, `reset(job)` after
`local job = data.job`, `clearAll({ data })` or `data[key] = nil`; a call that yields; and a loop
whose code may do any of these before it starts again. A function does not see what the guards
around it tell about fields, as it runs later. Other calls are taken to change no field, and an
assignment to a field still has to store a value of the type the field is declared with.

### Casts

A `---@cast` line changes the type of a local from its line on, as in lua-language-server:

```lua
local data = json.decode(payload)
---@cast data PlayerData
```

`---@cast name T` makes it a `T`, `+T` adds `T` to it, `-T` takes `T` out, `+?` and `-?` add and
take out `nil`, and one line can list several, as in `---@cast value +?, -string`. Adding to a local
whose type is unknown leaves a value that may still be anything else, as `string|unknown` for
`+string`, as in lua-language-server.

A cast gives the local a value, as an assignment does, and the value goes where the code goes: it
holds until the local is assigned again, a function created after it starts with it, and where the
ways through an `if`, a loop or a label meet, it joins what the other ways leave:

```lua
---@return string|number
local function get() end

local value = get()
if math.random() > 0.5 then
    ---@cast value string
    print(value) -- string
end
print(value) -- string|number: the way that skips the `if` leaves a number too

do
    ---@cast value number
end
print(value) -- number: a `do` block always runs
```

So a cast on the last line of a branch types what that branch leaves, and a branch that ends in
`return` or `error(...)` leaves nothing. lua-language-server instead keeps a cast on the last line
of an `if` branch after the `if`, whatever the other ways leave. Casts apply only to locals, also to
those that are assigned again. A guard that took effect before a cast tells nothing about the type
it gives, while one after it narrows that type. `+T` and `-T` change the type the guards around
their line leave instead, so `---@cast items +number[]` inside `if items then` keeps out the `nil`
of a `string[]?` there.
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
