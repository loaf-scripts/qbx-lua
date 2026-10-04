# Configuration and analysis reference

The `qbx-lint` CLI and `qbx-lua-ls` language server share the parser, analysis rules and
`qbxlint.toml` formatting settings described here. Command-line options apply to the CLI;
for editor settings and LSP requests, see the
[language-server protocol reference](../crates/qbx_lua_ls/docs/protocol.md).

## Syntax

The parser handles Lua 5.4 and CfxLua extensions, including backtick hash literals, optional
chaining (`value?.field`), compound assignments (`count += 1`), `local a, b in object`, set
constructors (`{ .police, .ambulance }`), `defer` blocks, and `/* ... */` comments.

## Configuration files

`qbx-lint` loads one configuration per invocation. Without `--config`, it searches upward from
the first input file or directory for `qbxlint.toml` or `.qbxlint.toml`. An explicit configuration
path can be relative or absolute. Exclusion and override patterns use the configuration directory
as their base.

All settings are optional. Unknown keys and rule names are errors.

```toml
exclude = ["web/**", "**/vendor/**"]
ignore_diagnostics = ['\[standalone\]/', 'third_party/']
globals = ["SomeRuntimeGlobal"]
ignore_unused_prefix = "_"
strict_classes = false

[rules]
"unused-argument" = "off"
"fivem/citizen-prefix" = "warning"
"qbox/prefer-cache" = "info"

[imports]
shared = ["@my_lib/shared/**.lua"]

[[overrides]]
files = ["tests/**", "**/*.spec.lua"]
globals = ["describe", "it"]
rules = { "undefined-global" = "off" }

[format]
indent_width = 4
use_tabs = false
line_width = 120
quote_style = "preserve"
```

| Setting | Meaning |
| --- | --- |
| `exclude` | Additional glob patterns for paths to skip. Skipped files are not analyzed at all, so other files do not see their globals, exports or events. |
| `ignore_diagnostics` | Gitignore-style patterns for paths that are analyzed but never reported. |
| `globals` | Names supplied at runtime that the linter cannot discover. |
| `imports` | Files every resource runs without an fxmanifest.lua entry, grouped as `shared`, `client` and `server`. See [Runtime imports](#runtime-imports). |
| `ignore_unused_prefix` | Locals and arguments with this prefix are exempt from unused checks; defaults to `_`. |
| `strict_classes` | Makes every `---@class` without `(loose)` strict, as if it said `(strict)`. Only classes declared in files whose diagnostics are reported follow it. See [Strict classes](#strict-classes). Defaults to `false`. |
| `rules` | Per-rule levels: `off`, `hint`, `info`, `warning` (or `warn`), and `error`. |
| `overrides` | Per-file globals, rule levels and `side`, selected by the `files` patterns, and imports for the resources whose `fxmanifest.lua` the patterns match. Later matching overrides take precedence for rule levels and sides. See [Sides of unlisted scripts](#sides-of-unlisted-scripts). |
| `format` | Formatting options, shown with their defaults above. |

The default exclusions include `node_modules`, `.git`, and `[builders]` directory contents.
Directory traversal also skips hidden directories. It follows directory symlinks, as FiveM does,
except links into the folder it traverses or to a folder above it, and reads a folder that several
links lead to once.

### Ignoring diagnostics

`ignore_diagnostics` is for code you do not maintain, such as third-party resources in a server
folder. Matching files are still parsed and indexed, so their globals, exports and events keep
resolving in your own files, and the language server still offers definitions and completion for
them. Nothing is reported for a matching file, not even syntax errors, and that includes matching
`fxmanifest.lua` and locale files. Formatting is not affected.

Patterns follow `.gitignore` rules, relative to the configuration directory:

| Pattern | Matches |
| --- | --- |
| `vendor/` | Every directory named `vendor`, at any depth |
| `/vendor/` | Only the `vendor` directory next to the configuration file |
| `resources/vendor` | A path relative to the configuration directory, because it contains a `/` |
| `*.min.lua` | Matching files at any depth |
| `!vendor/patched.lua` | Reports this file again although an earlier pattern ignores it |

FiveM category folders need escaped brackets: `[standalone]` is a character class that matches
one letter, while `\[standalone\]` matches the folder. Write such patterns as TOML literal strings
in single quotes, because `\[` is not a valid escape in a double-quoted TOML string.

`--rule CODE=LEVEL` changes a rule's base level for the invocation. Matching file overrides are
applied afterward. `--min-severity hint` includes hints, which the default `info` threshold hides.
`--max-warnings 0` makes reported warnings fail a lint run.

### Fallback: LuaLS and EmmyLua settings

Configure qbx-lint with `qbxlint.toml`. Projects that are already set up for LuaLS or EmmyLua get
a fallback: when no `qbxlint.toml` or `.qbxlint.toml` exists in any parent directory, the nearest
directory with `.luarc.json`, `.luarc.jsonc` or `.emmyrc.json` supplies the few settings that
have an equivalent here, and the CLI names the files it used on stderr. Files found together in
that directory are merged. A `qbxlint.toml` replaces the fallback entirely, so add one as soon as
qbx-lint needs anything beyond it. `--config` also accepts these files.

| LuaLS / EmmyLua setting | Used as |
| --- | --- |
| `diagnostics.globals` | `globals`, without the names qbx-lint already knows: runtime globals, natives, and globals of imports such as `@ox_lib/init.lua`, so the manifest and client/server checks still apply to them |
| `diagnostics.disable` | `off` for `undefined-global`, `lowercase-global`, `unused-local`, `unused-function`, `unused-label`, `redefined-local`, `unreachable-code`, `empty-block`, `unbalanced-assignments`, `duplicate-index`, `duplicate-set-field`, `count-down-loop`, `missing-parameter`, `redundant-parameter`, `undefined-doc-name`, `undefined-doc-param`, `duplicate-doc-alias`, `duplicate-doc-field`, `missing-global-doc`, `missing-local-export-doc`, `incomplete-signature-doc`, `missing-fields`, `assign-type-mismatch`, `invisible`, `param-type-mismatch`, `return-type-mismatch`, `missing-return`, `redundant-return-value`, `discard-returns`, `cast-type-mismatch`, `cast-local-type`, `no-unknown`, `need-check-nil`, `inject-field`, `undefined-field`, `newline-call`, `trailing-space`, `redundant-return` and `duplicate-doc-param`; EmmyLua's `unused` covers the `unused-*` rules |
| `diagnostics.severity` | Levels for the same codes (`Error`, `Warning`, `Information`, `Hint`, with or without a trailing `!`) |
| `diagnostics.neededFileStatus` | For the same codes, `Any` or `Opened` turns a rule that is off by default, such as `no-unknown` or `missing-global-doc`, on as a warning, unless `diagnostics.severity` gives its level. `None` turns a rule off, whatever its severity. With or without a trailing `!` |
| `workspace.ignoreDir` | Exclusions. LuaLS entries are gitignore-style patterns; `.emmyrc.json` entries are directories from the root |
| `workspace.ignoreGlobs` | Exclusions, as glob patterns |

Codes that only share a name with a rule here, such as `deprecated`, are ignored, as are all other
settings. Keys may be dotted (`"diagnostics.globals"`), nested, or prefixed with `Lua.`. Comments
and trailing commas are accepted. Formatting keeps its defaults; in the language server, the
editor's indentation settings still apply. Discovery skips a file or
exclusion pattern it cannot read and says so on stderr; pass the file with `--config` to make that
an error.

Annotations written for [fivem-lls-addon](https://github.com/overextended/fivem-lls-addon) keep
their meaning: the type names of its runtime library, `EventHandler`, `vector`, `json_options`,
`json_encode_state` and `msgpack_options`, are declared beside the runtime's own, and the classes
the two share carry its fields.

## Suppressing findings

Use a rule code to keep the suppression specific:

```lua
-- qbx-lint: disable-next-line unused-local
local keepMe = 1

print(externalValue) -- qbx-lint: disable-line undefined-global

-- qbx-lint: disable fivem/citizen-prefix
Citizen.Wait(0)
-- qbx-lint: enable fivem/citizen-prefix
```

Omitting rule codes suppresses all findings in the selected line or range, except syntax errors.
Comma-separated and space-separated rule codes are accepted. LuaLS-style directives are also
recognized:

```lua
---@diagnostic disable-next-line: undefined-global
print(fromSomewhereElse)
```

`-- luacheck: ignore` is recognized as a broad suppression: on its own line it disables subsequent
findings; after code it suppresses that line. Luacheck's numeric codes and name filters are not
translated. Prefer explicit `qbx-lint` rule codes when narrowing a suppression.

## Globals and client/server context

For a file inside a resource, the nearest `fxmanifest.lua` or `__resource.lua` supplies script
paths and sides. The analysis combines:

1. Lua and CfxLua runtime definitions from the [bundled stubs](../crates/qbx_fivem_data/stubs).
2. Bundled FiveM native signatures and client/server metadata, including `N_0x...` names.
3. Globals defined by scripts available on the file's side.
4. Globals from manifest imports such as `@resource/file.lua`, and from configured `imports`.
5. Configured `globals`.

An import is read from a sibling resource when available. Otherwise, known imports supply their
usual globals, such as `lib` and `cache` for `@ox_lib/init.lua`, `MySQL` for
`@oxmysql/lib/MySQL.lua`, and `qbx` for `@qbx_core/modules/lib.lua`.

### Runtime imports

Some resources run code from another resource without an fxmanifest.lua entry, for example a
loader that calls `load(LoadResourceFile(...))` for each file of a shared library. `imports` lists
those files as `@resource/path` patterns, grouped by the side they run on, and each resource then
sees their globals as if its manifest imported them:

```toml
[imports]
shared = ["@my_lib/shared/**.lua"]
client = ["@my_lib/client/**.lua"]
server = ["@my_lib/server/**.lua", "@oxmysql/lib/MySQL.lua"]
```

Paths may use manifest globs, where `*` stays within a folder and `**` crosses folders. Top-level
`imports` apply to every resource. To limit them to some resources, put them in an override: its
imports apply to each resource whose `fxmanifest.lua` its `files` patterns match, since all
scripts of a resource share their globals. Keeping those resources in one category folder makes
the pattern short:

```toml
[[overrides]]
files = ["resources/[[]my_lib[]]/**"]

[overrides.imports]
shared = ["@my_lib/shared/**.lua"]
```

In `files` patterns, `[[]` and `[]]` match literal brackets; `[my_lib]` alone would be a character
class. Server scripts do not see the globals of `client` imports, and client scripts do not see
those of `server` imports. Excluded files add nothing.

Files not listed as manifest scripts, including modules loaded through `require` or `lib.load`,
use globals from both sides unless an override gives them a side. Files outside a resource are
checked without a manifest environment.

### Sides of unlisted scripts

A resource whose loader runs its own files at runtime, listing them only under `files`, leaves
their side unknown. An override's `side` (`client`, `server` or `shared`) supplies it:

```toml
[[overrides]]
files = ["client/**"]
side = "client"

[[overrides]]
files = ["server/**"]
side = "server"
```

The side only applies to Lua files inside a resource that its manifest does not list as scripts;
a manifest entry always wins. Such a file is then checked like a script of that side: it sees the
globals, natives and `(server)` or `(client)` annotations of that side, and its own globals only
reach scripts of that side. The side is never guessed from folder names.

Recognized runtime guards narrow side checks within a file. Examples include
`IsDuplicityVersion()`, a local flag initialized from it, and `lib.context == 'server'`.
An early return such as `if not IsDuplicityVersion() then return end` narrows the following code
to the server, and `isServer and os.time()` narrows the right side of the `and`. Event
registrations inside these regions use that effective side.

FiveM only loads the `io` and `os` libraries on the server. `fivem/native-wrong-side` reports them
in client scripts, and in shared scripts too, since those also run on the client, unless one of
these guards keeps the code on the server. Scripts whose side is unknown are not reported. Natives
and the other runtime globals of one side, such as `TriggerClientEvent`, are only reported in
scripts or guarded code of the other side, as shared code usually calls them where they exist. In
client scripts, `os` and `io` are not completed and have no hover. Their functions are FiveM's:
there is no `os.exit`, `io.input`, `io.output` or `io.read`, and CfxLua adds `os.createdir`,
`io.readdir` and timers such as `os.nanotime`.

Where client and server scripts define a global function differently, as two `GetJob`s, a call
from code that runs on both sides takes neither definition: what it returns is unknown and passes
every type check. Inside one of these guards the call takes that side's definition, as long as its
arguments fit it.

### Definition files outside resources

qbx-lua-ls reads type libraries the way LuaLS reads its `library`: a file that belongs to no
resource is a definition file when it comes from a folder of the `library` setting, or when it is a
workspace file with a `---@meta` line above its first statement. A `---@meta` further down does
not count, here or for `missing-return`. A `library` folder inside the workspace is indexed as
workspace files first, so its files need `---@meta`. Other workspace files outside resources keep
their globals to themselves.

The globals, classes and aliases of definition files reach the scripts of every resource, unless
the resource declares the same name itself, imports it, or gets it from the bundled stubs: a
resource's own `---@alias VehicleData` wins over a library's `---@class VehicleData`, and its own
`vehicle = GetVehiclePedIsIn(ped, false)` over a library's `vehicle`. Two things limit where a
definition file applies:

- Its side comes from an override's `side`, or else from the words of its file name or nearest
  folder name: `client` or `cl` for the client, `server` or `sv` for the server, as in
  `server_vehicle.lua`, `cl_main.lua` or a `client` folder. `shared`, `common`, or both sides make
  it shared, and a name without any of these leaves the decision to the folder above it.
- A file inside a folder named after a resource, one the workspace has or imports from, or one of
  the well-known imports such as `ox_core` or `qbx_core`, declares what that resource's imports
  provide. Its globals only reach resources that load a file of that resource on the script's side,
  through the manifest, configured `imports`, `lib.load` or `require`. A library holding
  `ox_core/server_vehicle.lua` with `vehicle = {}` thus declares `vehicle` for the server scripts
  of resources that import `@ox_core/lib/init.lua`, and `undefined-global` still reports it
  anywhere else.

The globals of well-known imports, such as `lib` or `MySQL`, are reported by
`fivem/import-not-declared` without their import, whatever a definition file declares. Folders
above the workspace or library root that holds a file do not count for its side or resource. The
CLI does not read definition files outside resources.

## Function arguments

`missing-parameter` compares calls with the LuaCATS annotations of the function they call. A
parameter is required when a `@param` documents it with a type that does not allow `nil`: no `?`
after its name, and a type other than `nil`, `any`, `unknown`, a union with `nil`, or an `@alias`
that includes one of those. Undocumented parameters are optional, so a function without `@param`
lines is never reported. Every parameter before the last required one has to be passed as well.
`@overload` and `---@type fun(...)` signatures count, and the one that needs the fewest arguments
decides. An `@overload (server) fun(...)` or `@overload (client) fun(...)` only counts for calls
on that side, as decided by the script's manifest side and any `IsDuplicityVersion()` or
`lib.context` guard around the call; shared code counts both.

`redundant-parameter` reports the arguments a call passes beyond the parameters of the function,
whose values are lost. Every parameter counts, documented or not, and a `...` takes any number of
arguments. Here the signature that takes the most arguments decides. A call or `...` among the
extra arguments counts as one:

```lua
local function noop() end
noop(function() end) -- 'noop' is called with 1 argument, but takes none
```

A function `@field` that repeats the name of an unscoped one is another signature of it, as LuaLS
reads it, rather than a second field: a call picks the signature its arguments fit, and hover shows
the descriptions of both. Repeated with `(server)` or `(client)`, the signature only applies on
that side. Fields that are themselves scoped to a side stay apart, and repeated fields that are not
functions are kept as they are.

Calls are checked when the function is:

- a local function, or a local that is only ever assigned functions;
- a local, or a field of a local table, that a `---@type fun(...)` above its declaration or an
  assignment types, also through an `@alias` or with `?`, whatever value it is given;
- a global function defined by a script on the caller's side, or by one of its imports;
- a field of a global table, or of a local table in the same file, that a `function Utils.round(x)`
  or `function Utils:round(x)` statement or an assignment defines.

A method defined with `:` and called with `.` needs `self` as its first argument. When a name has
several definitions, for example a client and a server `Notify`, a call is compared with those its
side can reach, and is not checked when any of them is not a function. A `:` call passes its
receiver as the first argument whatever that parameter is named, so `function Locale.new(_, opts)`
called as `Locale:new(opts)` receives both. Calls whose last argument is another call or `...`
pass an unknown number of arguments, so `missing-parameter` skips them. Neither rule checks
natives, runtime functions, exports, or methods of objects returned by calls, such as
`GetPlayer(source):setJob(job)`.

qbx-lua-ls also checks the payload of calls to `---@callback await` and `trigger` wrappers. The
values passed in the wrapper's `...` go to the handler registered under the name the call passes,
on the other side, so they have to cover that handler's required parameters, and
`redundant-parameter` reports those it does not take. A handler registered outside the client
receives the calling player first, so that parameter is not counted:

```lua
---@param num1 number
---@param num2 number
RegisterServerCallback('add', function(source, num1, num2) ... end)

-- Callback 'add' is called with 0 arguments, but needs 2; 'num1' (number) will be nil
AwaitServerCallback('add')
```

When several handlers are registered under the name, the one that needs the fewest values decides,
and for `redundant-parameter` the one that takes the most.

`param-type-mismatch` compares each argument with the type of its parameter, as
`assign-type-mismatch` compares a value with the type of its variable: a different kind of value,
or a literal the type does not list, is reported. Each type a union lists has to fit, so a
`string|number` is no `string`, as in lua-language-server.

```lua
---@param mode "fast" | "slow"
---@param count integer
local function run(mode, count) end

run("instant", 1) -- Cannot assign `"instant"` to parameter `mode` of type `"fast"|"slow"`
run("fast", "2")  -- Cannot assign `string` to parameter `count` of type `integer`
```

A parameter typed `any` or `unknown` takes any value, also beside other types, as in
`unknown|string`.

It checks every call to a function whose parameters have types, also runtime functions, exports,
methods of classes and fields typed `fun(...)`. A call is compared with each definition the side
of the call reaches and with their `@overload`s, and passes when a signature that takes as many
arguments as it passes takes each of them; when none takes that many, any of them may. A `:` call
passes its receiver as the first argument, as in Lua, except through `exports`, whose proxy drops
it. The payload of a `---@callback` wrapper call is compared with the handlers registered under its
name.

An argument has a type when something declares it, as for `impossible-comparison`: a literal, an
operator, an annotation, a stub, or the values a local is declared with or assigned that reach the
call. A literal stored in a local counts by its kind only: `local mode = 'dev'` is a setting to
change, so it passes for `"fast" | "slow"`, while `local count = 5` is still no `string`. A
`--[[@as T]]` right after an argument casts it.

As in LuaLS, `nil`, written out or as the `?` of a `string?`, needs a parameter that takes it: one
marked optional, or typed with `nil`, `any` or `unknown`. A guard such as `if name then` rules the
`nil` out first. `false` is a `boolean` like any other, also where code passes it to skip a
parameter, as in `AddItem(source, item, 1, false, info)`.

```lua
---@param name string
local function greet(name) end

---@type string?
local maybe
greet(maybe) -- Cannot assign `string?` to parameter `name` of type `string`
greet(nil)   -- Cannot assign `nil` to parameter `name` of type `string`
```

Natives are checked as the runtime passes their arguments on:

- Numbers and booleans pass for each other, as a `BOOL` is an integer to natives: `0` and `1` pass
  for a `boolean`, and `false` for an integer.
- A string parameter takes `nil` or a `string?`, and `0` and `false`, which the native wrappers
  turn into NULL, as in `DrawMarker(..., nil, nil, false)`, and a number or boolean written out,
  which they turn into the text it is written as, as in `SetConvarReplicated('volume', 30)`. A
  parameter for the server id of a player, such as the `playerSrc` of `DropPlayer`, takes any
  number. Other numbers and booleans are reported: `ReleaseNamedRendertarget(GetHashKey(name))`
  passes a hash, not the name.
- A hash parameter takes a string, which the wrappers hash.
- A vector fills a number parameter for each of its parts, as in `SetEntityCoords(ped, coords, ...)`.
  The values after a value of unknown type that may be a vector are not checked when the call
  passes fewer values than the native takes, since they may go to later parameters.

Other parameters of natives do not take `nil`: `DoesEntityExist(entity)` with an `Entity?` is
reported. Only clear cases count, and the rest is left alone:

- Types inferred from assigned values, such as that of `Config.Value = ''`.
- Parameters typed with a generic of the function called: the arguments of the call bind it, which
  declares nothing. A function that a callee passes to a callback, such as `resolve` of
  `fun(resolve: fun(value: T))`, takes and returns what the other arguments of that call declare
  for the generic, as the `boolean` of `Promise:New('boolean', function(resolve) end)` for a
  `` `T` ``, and any value for a generic they leave unbound.
- Parameters typed with the name of a native handle such as `Vehicle`, which resources also declare
  as classes.
- Calls through globals in an opaque resource, such as an escrowed one; see
  [Escrowed, obfuscated and mixed-language resources](#escrowed-obfuscated-and-mixed-language-resources).

## Strict classes

A strict class takes only the fields it declares. Mark one with `(strict)`, or LuaLS's `(exact)`,
which means the same here:

```lua
---@class (strict) Test
---@field test string
local Test = {}

function Test:greet() end

---@type Test
local abc = {
    test = "2543",
    other = true, -- undeclared-field
}
abc.more = 1 -- undeclared-field
```

qbx-lua-ls reports `undeclared-field` for a name that a table constructor typed as the class sets,
for fields that assignments and `function value:name()` statements set on values of the class,
such as a `---@type` local, a parameter or `self`, and for fields that code reads from them, like
`abc.nope` or `abc:nope()`. A field counts as declared when an `@field` of the class or one of its
parents names it, or when it is set on the table the `---@class` annotation declares, like `greet`
above; fields set through values of the class do not count. A `[string]` index, such as
`---@field [string] any`, takes any name, and an index of string literals, such as
`---@field ['a'|'b'] integer` or an alias of them, takes those names. A table type named as a
parent, like `{ id: integer }` or `table<string, any>`, declares its fields and indices for the
class, so `Name : table<string, any>` takes any name too, while a parent that is neither a class
nor such a table type, like a plain `table`, takes any key.

A table that `setmetatable` gives a metatable whose `__index` is the class table, as
`setmetatable(obj, Test)` does after `Test.__index = Test`, is a value of the class from the call
on: `obj.more = 1` after the call is reported, and so is `self.more = 1` after
`local self = setmetatable({}, Test)`. What is set on `obj` before the call is not. A table built
with fields of its own, like `setmetatable({ id = 1 }, Test)`, keeps them, so its fields are not
checked against the class.

Other keys need an index of their type. With only `---@field [string] number`, `abc[1]`,
`abc[1] = x` and array entries such as `{ 'a' }` are reported, since the class has no `integer`
keys. A key held in a string variable, as in `abc[key]`, may name a field and is not reported.

Fields keyed by an integer or boolean literal, such as the entries of a tuple, are declared one by
one:

```lua
---@class (strict) Employee
---@field [1] number Source
---@field [2] string Character name

---@type Employee
local employee = { 1, "Jane" }
employee[3] = true -- undeclared-field
```

Array entries are checked by their position, so `{ "Jane", 1 }` is an `assign-type-mismatch` for
both entries. A key held in an `integer` variable, as in `employee[i]`, may be either field, so it
is neither reported nor checked against their types.

Index types also decide what a field without its own `@field` holds, for every class and not only
strict ones: with `---@field [string] number`, `abc.other` is a `number`, and `{ other = true }` and
`abc.other = true` are an `assign-type-mismatch`.

A generic class gives its fields the type arguments a value is declared with. With
`---@class List<T>` and `---@field first T`, `first` of a `---@type List<string>` local is a
`string`, so `{ first = 1 }` is an `assign-type-mismatch`. A parent passes its type arguments on,
as in `---@class Names : List<string>`, and a generic `---@alias Box<T> { value: T }` reads
`Box<integer>` the same way. A parameter left without an argument, like the `R` of `Pair<string>`
for `Pair<L, R>`, stays `R`, and so do all of them where the class is named without arguments,
as in `---@type List`, just as the table the `---@class` annotation declares and `self` in its
methods keep them, being `List<T>`. A field of such a type takes any value but must still be
given, and a method's own `---@generic T` binds from the call. An alias named without its type
arguments, as in `---@type Box`, reads its parameters as unknown instead, as lua-language-server
does, and so does a table type that a class names as a parent, like the `{ [number]: T }` of
`---@class Array<T> : { [number]: T }` for a `---@type Array`.

With `strict_classes = true`, every class is strict unless it is marked `(loose)`. That default
only covers classes declared in workspace files outside `exclude` and `ignore_diagnostics`, so the
classes of third-party resources stay as they are unless they say `(strict)` themselves. When one
class is declared in several places, one `(strict)` makes it strict, and otherwise one `(loose)`
keeps it loose.

## Injected fields

A field belongs to a table when the code that owns the table sets it: through a global or a path
from one, like `Config.debug = true`, on the table a `---@class` annotation declares, through
`self` in a method, through a local declared with a table at the top of the file, and through the
instance a constructor makes in a local, as `local self = setmetatable({}, Base)` does for a table
without a `---@class`. As in TypeScript, what is set through other values adds no field: a
parameter or local typed as a class, a loop variable over another file's tables, or another local
that holds the same table. Hover and completion leave such fields out, and lua-language-server
leaves them out of a class too.

`inject-field` reports the fields that assignments and `function value:name()` statements set
through a value whose type does not have them: a class, a table type such as the `{ label: string }`
a table constructor gives, or a table declared under another name, as TypeScript and
lua-language-server do. To turn it off:

```toml
[rules]
"inject-field" = "off"
```

```lua
---@class Thing
---@field name string
local Thing = {}

function Thing:init()
    self.ready = true -- declares `ready`
end

---@param thing Thing
local function tag(thing)
    thing.ready = false
    thing.extra = 5 -- inject-field: Field `extra` is not declared in `Thing`
end

local rows = { { label = 'a' }, { label = 'b' } }
for _, row in pairs(rows) do
    row.count = 0 -- inject-field: Field `count` is not declared in `{ label: string }`
end
```

An empty table, such as `local result = {}`, takes any field, and so do values of unknown type,
`table` and `any`. Keys held in variables are not checked, and strict classes are left to
`undeclared-field`. lua-language-server's `inject-field` checks classes only, and its
`---@diagnostic disable: inject-field` comments and `diagnostics.disable` entries apply here too.

## Undefined fields

`undefined-field` reports a field read from a value whose type does not have it, as TypeScript
reports a property its type does not declare. Outside the language server it covers the standard
library tables, such as `string.nope`. In qbx-lua-ls it also covers values whose type the language
server knows, with the fields that `inject-field` lets code set: those a class or table type
declares, including the `{ label: string }` a table constructor gives, and those set through the
names that own a table. Reads with `.`, `['name']` and `:` count, also in conditions such as
`if point.z then`. A union lacks a field when none of its parts has it, and a local that is
assigned again has the types of all the values that may reach the read.

```lua
---@class Point
---@field x number
---@field y number

---@param point Point
local function show(point)
    print(point.x, point.z) -- undefined-field: Field `z` is not declared in `Point`
end

local rows = { { label = 'a' }, { label = 'b' } }
for _, row in pairs(rows) do
    print(row.count) -- undefined-field: Field `count` is not declared in `{ label: string }`
end
```

Some values may have any field, so reads from them are not checked:

- values of unknown type, `table` and `any`, empty tables such as `local result = {}`, and classes
  with an index that takes the name or a parent that is no class, such as `table`;
- global tables and the paths from them, such as `Config.debug` or `ESX.PlayerData`, whose fields
  other files and resources set, and the instances made from them, unless an annotation types them;
  the exports of a resource are checked only against a type declared for them, as `---@type
  PhoneExports` above `exports['phone'] = {}` declares one, with what the resource registers;
- `self` in a method of a table without a `---@class`, whose instances get their fields elsewhere,
  and tables whose metatable has an `__index` function;
- numbers, booleans and functions whose type is only inferred, not declared;
- strict classes, whose fields `undeclared-field` checks.

lua-language-server's `undefined-field` does not check tables built from table constructors, and
it counts a field set through any value of a class, which `inject-field` reports instead. Its
`---@diagnostic disable: undefined-field` comments and `diagnostics.disable` entries apply here too,
also to the standard library tables. To turn it off:

```toml
[rules]
"undefined-field" = "off"
```

## Member visibility

A class can keep fields and methods to itself, as in LuaLS:

```lua
---@class Account
---@field private balance number
---@field owner string
local Account = {}

---@private
function Account:audit() end

function Account:deposit(amount)
    self.balance = self.balance + amount
end

---@type Account
local account = Account
print(account.balance) -- invisible
account:audit() -- invisible
```

`---@field private`, `protected` and `package` mark a field. `---@private`, `---@protected` and
`---@package` above `function Account:name()`, `Account.name = value` or `self.name = value` mark
what the statement sets, and `---@public` changes nothing. qbx-lua-ls reports `invisible` where
code reads, sets or calls such a member, or defines it with `function value:name()`, outside where
it may:

- A private member is used through the table the `---@class` annotation declares, like
  `Account.balance` or ox_lib's `lib.array:new()`, from anywhere, or inside a function defined on
  that table: `function Account:name()`, `function Account.name()`, `Account.name = function() end`
  or a function written in the table's own constructor, with the functions nested in it.
- A protected member is also used through the table of a subclass, or inside its functions.
- A package member is used in the file that declares it.

The nearest class that declares a name decides, and one restrictive declaration there makes the
member restrictive, so a subclass that declares the name again without a keyword makes it public.
Setting a member on the table of a class declares it for that class, so a subclass may define a
member its parent keeps private, as ox_lib's `function lib.ped:__index()` does.
Keys of table constructors are not checked, and `missing-fields` asks for private and protected
fields wherever a table of the class is built, as LuaLS does. Completion leaves such members out
where the code may not use them.

Unlike LuaLS, functions nested in a method, such as a `table.sort` comparator or a `CreateThread`
body, and functions written in the class table's constructor count as inside the class. LuaLS's
`doc.privateName`, `doc.protectedName` and `doc.packageName` settings, which make fields private by
their names, are not read.

## Return values

A function documented with `@return` has to return values of those types. That includes a
function passed to the call below the doc comment, such as the handler of
`RegisterServerCallback('name', function(source) ... end)`, also when the statement assigns what
the call returns, as `local handler = RegisterNetEvent('name', function() ... end)` does, and a
function written in a table, whose doc comment goes above its field. qbx-lua-ls reports:

- `return-type-mismatch` for a returned value that clearly is not of its `@return` type: a
  different kind of value, such as `return 5` for `---@return string`, or a literal the type does
  not list. A trailing `---@return ...string`, or `---@return string ...`, covers every further
  value.
- `missing-return` for a `return` with fewer values than the function requires, and at the `end`
  of a function whose body can run past it without returning. A value is required unless its type
  allows `nil`, as `string?`, `string|nil` or `any` do.
- `redundant-return-value` for the values a `return` passes beyond those `@return` declares. A
  trailing `...T` takes any number of values, and an `@overload` that declares more values allows
  them. Only the values written out count: a call or `...` at the end may give none, so
  `return 1, print()` passes for `---@return integer`, and `return text:gsub(...)` for
  `---@return string` although `gsub` also returns a count.

A function without `@return` gets the same checks from the function type it is written as, as in
lua-language-server: the `fun(n: integer): string` of the parameter it is passed for, of the
`---@type` above the statement it is the value of, or of the field of a typed table it is written
in. A `fun()` that lists no values takes none, so `return true` in a handler passed for
`fun(...: any)` is a `redundant-return-value`. A value whose type names a generic of the callee
or of the function type is not checked, since only the arguments of a call bind it, and nothing
is checked when the call fits several signatures or the type lists several function types.

```lua
---@param cb fun(n: integer): string
local function withCallback(cb) end

withCallback(function(n) return n end) -- Cannot return `integer` as return value #1 of type `string`
```

A body finishes without running past its end when it ends in `return`, `error(...)`, an
`if`/`else` whose branches all finish, or a loop such as `while true do` that only a `return`
leaves. An empty body runs past its end too, except in a definition file marked `---@meta`, whose
functions only declare their signatures. Returned tables typed as a class get the same checks as
`---@type` tables: `missing-fields`, `assign-type-mismatch` and `undeclared-field`.

A function that returns either one set of values or another lists them on its `@return` line,
separated by `|`, with the values of a set in parentheses:

```lua
---@return false | (string, string)
local function GetName() ... end
```

Each `return` then has to be one of the sets: `return false` and `return "Joe", "Doe"` are, while
`return "Joe"` is a `missing-return`, `return 1, "Doe"` a `return-type-mismatch` and
`return "Joe", "Doe", "Smith"` a `redundant-return-value`. A `return` is compared with the set it
comes closest to. This notation is a qbx extension of LuaCATS, which qbx-lua-ls also uses to narrow
the locals a call declares; see its [type guards](../crates/qbx_lua_ls/README.md#type-guards).

Only clear cases count. Values whose type is not known are skipped, including a local that one of
the values that may reach the `return` leaves without a known type; see
[locals that are assigned again](../crates/qbx_lua_ls/README.md#locals-that-are-assigned-again). The
same applies to the values `assign-type-mismatch` checks, where `nil` is a value like any other:
`abc.field = nil` needs a field type that allows it, such as `string?` or `string|nil`.

`discard-returns` reports a call on a line of its own whose function is marked `@nodiscard`, as
its values are what it is called for. The runtime stubs mark the functions that only compute a
value, such as `tostring`, `math.floor`, `json.encode` and `vector3`:

```lua
---@nodiscard
---@return integer
local function count() ... end

count() -- The values that `count` returns cannot be discarded
```

A call that may run one of several functions, such as a client and a server definition, is only
reported when each of them is marked. Of a function with `@overload`s, the signature the call picks
decides, as for LuaLS: an overload is not marked, so `math.random()`, which warms up the generator,
passes, while `math.random(1, 10)` is reported. `string.gsub` is not marked either, since with a
function as `repl` it loops over the matches. Calls through globals in an opaque resource, such as
an escrowed one, are not checked; see
[Escrowed, obfuscated and mixed-language resources](#escrowed-obfuscated-and-mixed-language-resources).

## Typed variables

`assign-type-mismatch` also covers variables whose type is declared. A `local` or an assignment
with a `---@type` above it has to store values of that type, and so does every later assignment to
a local declared with `---@type` or to a parameter documented with `@param`:

```lua
---@alias State 1 | 2 | 3 | false | true

---@type State
local state = "test" -- Cannot assign `"test"` to `state` of type `State`

state = "abc"        -- Cannot assign `"abc"` to `state` of type `State`
state = nil          -- Cannot assign `nil` to `state` of type `State`
```

The check is that of `return-type-mismatch`: a different kind of value, or a literal the type does
not list. A local is compared with the type it is declared with, not with what the guards around
the assignment narrow it to, so `name = "x"` inside `if not name then` passes for a `string?`. A
`local` declared without a value is not reported, and neither is the `nil` that the statement under
a `---@type` gives the names it declares, as lua-language-server allows `---@type number` above
`Hp = nil`. A later `= nil` needs a type that allows it. A call at the end of the values gives each
name the value it returns at that position.

A `---@type` line that lists several types gives each name of the statement its own, as
`---@type boolean, string?` does above `local ok, err = pcall(...)`. A single type is the first
name's alone, as lua-language-server binds it, so `---@type boolean` above
`local onScreen, x, y = GetScreenCoordFromWorldCoord(...)` types only `onScreen`. Names without a
type of their own, and other locals whose type is inferred from what they hold, have the type of
the value they are declared with, which [`cast-local-type`](#local-types) checks. Globals in
assignments that have no `---@type` above them take any value.

The value stored is what the code is known to give. A global's `nil` tells nothing: one declared as
`CurrentZone = nil` and set to a name by an event handler holds whatever the handler gives it, so
storing it anywhere passes. A `--[[@as T]]` or `---@as T` right after a value, on its line, casts
it to `T` as in lua-language-server, which silences a check that the code knows better than:
`local count = GetValue() --[[@as integer]]` stores an `integer`. On a call, the cast types its
first value.

A table constructor typed as a table type rather than a class is checked like a class table, by
the type of each entry: a shape such as `{ value: string, count?: integer }`, an alias of one,
generic ones such as `Box<string>` for `---@alias Box<T> { value: T }` included, an array such as
`string[]`, or a `table<K, V>`. That holds wherever a class table is checked: under a `---@type`,
as an argument, as a returned value, and in the fields of another typed table, also for the class
tables a shape holds. An assignment without a `---@type` checks a table against the type its
target is declared with, not one inferred from what the target held before. As in
lua-language-server, such a table may set fields its type does not name, and the entries of a table
typed as a union are not checked.

`missing-fields` reports a table typed as a shape that leaves out a field the shape requires, one
without `?` whose type does not allow `nil`, as TypeScript does and lua-language-server does not.
The tables that an array, a `table<K, V>` or a tuple holds need the fields of the type of their
entry, so the `{}` of `{ {} }` for `Dog[]` and of `{ rex = {} }` for `table<string, Dog>` are
reported, as in both. An entry whose key the type does not take, like the `{}` of `{ {} }` for
`table<string, Dog>`, is not checked.

A table typed as a union of classes and shapes needs the required fields of one of them, and the
report has a line for each, as lua-language-server's does. Types that hold no table are left out,
so a table for `Dog|string` needs the fields of `Dog`, and as in TypeScript, a union that also
lists an array, a `table<K, V>`, a tuple or a type that takes any table, such as `table` or `any`,
is not reported. A table that a table typed as a union holds, like the `{}` of `{ pet = {} }`,
needs the fields of one of the types that its members declare for its key, as `Dog|Cat` for an
entry of `Dog[]|Cat[]`. As in TypeScript and unlike in lua-language-server, the tables held by one
that may be a `table` or `any`, as for `Dog[]|table`, are not checked.

## Local types

`cast-local-type` reports an assignment that gives a local without a `---@type` or `@param` a value
its type does not take. As in lua-language-server and TypeScript, such a local has the type of the
value it is declared with:

```lua
---@return string?
local function find() end

local speed = 5
speed = "5"   -- Cannot assign `string` to `speed`, defined as `integer`
speed = 7.5   -- passes: an `integer` local takes any number
speed = nil   -- Cannot assign `nil` to `speed`, defined as `integer`

local mode = "dev"
mode = "live" -- passes: a literal written out stands for its kind

local name = find()
name = 5      -- Cannot assign `integer` to `name`, defined as `string?`
```

The check is that of `assign-type-mismatch`, for each type the value may be: a different kind of
value, or a literal the type does not list. So a `string?` needs a local that may be `nil`, and
clearing a local with `nil` needs a type that allows it, also for a table, as in TypeScript and
unlike in lua-language-server. A local that starts as `false` or `true` is a `boolean`, and one
declared with a table constructor takes any table. A literal that a function declares it returns,
as the `"a"|"b"` of `---@return "a"|"b"`, is one of the only values its local takes, while a literal
written out, also through `and` and `or`, widens to its kind. Each name of an assignment is checked
against its own local, also inside the functions that a local is assigned in.

A local declared without a value, as `nil`, or with a value of unknown type or `any`, takes any
value, and so do `_`, `self` and parameters without `@param`. A loop variable has the type its loop
gives it, such as the `integer` of `for i, v in ipairs(list)`, and a parameter of a function passed
for a function type, such as the handler of `fun(id: integer)`, has the type the function type
declares, as in TypeScript. Locals with a `---@type` or `@param` are left to
[`assign-type-mismatch`](#typed-variables), and `<const>` and `<close>` locals to `const-reassign`.
A `---@cast` changes what a local holds from its line on, but not the type it is declared with, as
in lua-language-server, so assigning a value of the cast type is still reported; a `---@type` above
an assignment is the type of the value it stores. A `BOOL` that a native returns may be a boolean
or an integer, so either passes for the other where a native gives it.

## Casts

A `---@cast` line changes the type of a local from its line on: `---@cast name T` makes it a `T`,
`+T` and `-T` add and take out a type, and `+?` and `-?` add and take out `nil`. qbx-lua-ls
describes where a cast holds with its
[type guards](../crates/qbx_lua_ls/README.md#casts).

`cast-type-mismatch` reports a `---@cast name T` whose `T` the declared type of the local does not
take. The check is that of `assign-type-mismatch`, for each type `T` lists: a different kind of
value, or a literal the declared type does not list.

```lua
---@type integer
local count = 1
---@cast count string  -- Cannot convert `integer` to `string`
---@cast count integer? -- Cannot convert `integer` to `integer?`
---@cast count number   -- passes
```

The declared type is the `---@type` or `@param` of the local, or else that of the value it is
declared with, as for `impossible-comparison`: a literal, or what a function declares it returns.
A local that is assigned again and has no annotation has the declared types of the values that
reach the cast, so `local count = 5`, `count = 6` and then `---@cast count string` is reported. A
local whose type is only inferred is not checked, and neither is one declared as `nil`, which the
cast gives its type as in lua-language-server, nor the `+T` and `-T` entries, which change the type
rather than replace it.
As in lua-language-server, a class has to be one that the declared type names or extends: a
`Test.Animal` can be cast to its subclass `Test.Dog`, but a `Test.Dog` not to `Test.Animal`, and
neither to an unrelated class. Type arguments are not compared, a declared type that is no class,
like `table` or `any`, takes any class, and so does a local declared with a table constructor.

## Impossible comparisons

`impossible-comparison` reports an `==` or `~=` whose two sides can never be equal, so that the
comparison always gives the same answer. Either the sides are different kinds of value, such as a
string and a number, or they list literals and none is on both sides:

```lua
---@return "active" | "busy" | "ready"
local function GetState() ... end

if GetState() == "invalid_state" then end -- Comparing `"active"|"busy"|"ready"` with `"invalid_state"` is always false
if GetState() == 5 then end               -- Comparing `"active"|"busy"|"ready"` with `5` is always false
if not name == "admin" then end           -- Comparing `boolean` with `"admin"` is always false
if type(value) == "tabel" then end        -- Comparing `lua_type` with `"tabel"` is always false
```

A side has a type when something declares it: a literal, an operator such as `not` or `..`, a
`@param`, `@type` or `@return`, a `@field` of a class, an alias or enum, a stub or a native. A
parameter of a function passed to a call has the type the callee declares for it, with the generics
that the other arguments declare: `value` is a `Player` in
`onValue('Player', function(value) end)` for `---@param cb fun(value: T)` and a `` `T` ``. A
local has the types of the values it is declared with or assigned that may reach the comparison,
so `local state = GetState()` is checked like the call, and inside a
[type guard](../crates/qbx_lua_ls/README.md#type-guards) the type the guard narrows it to: the right
side of `class ~= 13 or class ~= 14` only runs when `class` is `13`, so it is always true. Guards
narrow the [fields](../crates/qbx_lua_ls/README.md#fields) of locals the same way. A
literal stored in a local counts by its kind only: `local mode = 'dev'` is a setting to change, so
`mode == 'prod'` passes, while `mode == false` is still reported as a `string` compared with
`false`. A value assigned to a local with a `---@type` or `@param` keeps the parts of that type it
may be, as described for
[locals that are assigned again](../crates/qbx_lua_ls/README.md#locals-that-are-assigned-again).

Only clear cases count, and the rest is left alone:

- Types that are inferred from assigned values. `Config.Webhook = ''` tells what the config holds
  today, not that `Config.Webhook == false` cannot be true once someone edits it. The same goes
  for what an undocumented function returns, and for the value of `a or b` and `a and b`.
- Comparisons with `nil`. Annotations often leave out the `?` of a value that may be missing, and
  the check for it is deliberate.
- A local that one of the values that may reach the comparison leaves without a declared type, as
  `name = untyped(name)` does, unless a `---@cast name T` types it. `---@cast name +T` does not: it
  adds `T` to a type that is still not known, as it does for a local with no declared type.
- Natives documented as `boolean` compared with a number. Scripts can get `1` instead of `true`
  from them, so `IsPedInAnyVehicle(ped) == 1` is valid.

## Unknown types

`no-unknown` is off by default. Turned on, qbx-lua-ls reports each parameter, local and loop
variable whose type is unknown: none is declared, and none can be inferred from its value, from
the function a callback is passed to, from the declared type of the table field a function is
written in or defined for, or from what a loop goes through.

```toml
[rules]
"no-unknown" = "warning"
```

```lua
function test(foo) end -- Parameter `foo` has no type; add `---@param foo <type>`

---@param data table
local function list(data)
    for key, value in pairs(data) do end -- a plain `table` tells nothing about its keys and values
end
```

A `---@param` or `---@type` gives the name a type, `any` included. Without one, a parameter that
only takes `any` from the `...` of the `fun(...)` its function is passed as, like the handler of
`RegisterNetEvent`, counts as untyped, and so does the `data` of a `RegisterNUICallback` handler,
which only the resource's own UI decides. Its `---@param` goes above the statement that makes the
call, whether that is the call alone or `handlers[name] = RegisterNetEvent(name, function(id) end)`.
A function written in a table takes the `---@param` lines above its field, and the parameter
types of the field's `fun(...)` when the table has a declared type: the `---@type` above its
`local`, the type of the parameter it is passed for, or `metatable` for the second argument of
`setmetatable`. A function defined for a `---@field name fun(...)` of a class, with
`function Class:name()`, `function Class.name()` or `Class.name = function()`, takes the parameter
types of that `fun(...)` when it has as many parameters, not counting the `self` of `:`. A local
declared without a value needs a `---@type`, since
later assignments are not followed. Names that start with `ignore_unused_prefix`, and `self`, are
not reported. To check only your own resources, set the level in an `[[overrides]]` entry instead
of `[rules]`.

It also reports a value of unknown type where a type is declared for what takes it: a local or
parameter with a `---@type` or `@param`, a field of a class or of a table built for a declared
table type, an element of such a table, a parameter that each signature the call may use declares,
and a value that `@return` declares:

```lua
---@param entities number[]
local function target(entities)
    if type(entities) ~= "table" then
        entities = { entities } -- The type of the value assigned to field `[1]` of type `number` is unknown
    end
end
```

A target declared `any` takes any value, and the parameters that `param-type-mismatch` leaves out,
those of natives and those typed with a generic of the function called, are left out here too. A
value read from a local that is itself reported, as `data.id` for an untyped parameter `data`, is
not reported again: typing the local types the value.

## Nil checks

`need-check-nil` reports a local that may hold `nil` where code indexes it, calls it, stores a value
under it as a key, uses it in arithmetic, concatenation, `#` or a `<`, `<=`, `>` or `>=` comparison,
or gives it as a bound of a numeric `for`, all of which raise an error for a missing value, as
TypeScript and lua-language-server do. `==` and `~=` compare any values and are not reported. To
turn it off:

```toml
[rules]
"need-check-nil" = "off"
```

```lua
---@return string?
local function GetName() end

local name = GetName()
print(name:upper()) -- `name` may be nil: its type here is `string?`
print(name:lower()) -- `name` may be nil: its type here is `string?`
```

As TypeScript does for a value that may be `undefined`, every such read is reported, also after one
that would raise the error first, until a guard or cast rules the missing value out.

A local may hold `nil` when its declared type allows it: its `---@type` or `@param`, the
`@return` or `@field` of the function or class its value comes from, or for a parameter of a
function passed to a call, what the callee declares for it, whoever declares them, stubs and other
resources included. `false`, as in `false|string`, counts as well. The
[type guards](../crates/qbx_lua_ls/README.md#type-guards) and casts around the read narrow the
type first, so `if not name then return end`, `if name then`, `name and name:upper()`,
`assert(name)` and `---@cast name -?` all check it, as does a condition that reads from it, such as
`if player?.job then` for `player`. `?.`, `?[` and `?:` give `nil` for a missing value and are not
reported. Where guards together rule out every value of the type, the read has the type that the
[type guards](../crates/qbx_lua_ls/README.md#type-guards) give such code. Inside
`if not count then`, a `number` is `nil`, which holds no value that may be missing and is not
reported, as in lua-language-server. After `round and (round == true or i < round)`, a `boolean?`
keeps its type, but the `nil` that `round and` rules out stays out.

A local that may hold `nil`, passed for a parameter that does not take `nil`, is a
`param-type-mismatch`, as in lua-language-server:

```lua
---@param target Player
local function greet(target) end

local player = GetPlayer() -- Player?
greet(player) -- Cannot assign `Player?` to parameter `target` of type `Player`
```

`need-check-nil` still reports one passed for a parameter typed with the name of a native handle,
such as `Vehicle`, which `param-type-mismatch` leaves out, in every signature the call may use. A
`---@diagnostic disable` comment for `param-type-mismatch` silences it too.

Only a guard on the local itself checks it. After
`local vehicle, coords = lib.getClosestVehicle(pos)` and `if not vehicle then return end`, `coords`
is still reported where it is declared `vector3?`, unless the function declares the
[sets of values](../crates/qbx_lua_ls/README.md#sets-of-returned-values) it returns, as
`@return nil | (integer, vector3)` does. A lookup such as `ESX.GetPlayerFromId(source)` that is
declared to return `xPlayer?` counts like any other call.

Only clear cases count, and the rest is left alone:

- Fields, which lua-language-server leaves alone too, and the values of calls. A local that takes
  the value of a field has the type the guards around it leave of the field, so
  `local job = data.job` inside `if data.job then` holds a value; see
  [fields](../crates/qbx_lua_ls/README.md#fields).
- A local that one of the values that may reach the read leaves without a declared type, as
  `name = name or 'none'` does, unless a `---@cast name T` types it. A local that is assigned again
  is checked with the values that reach the read, as described for
  [locals that are assigned again](../crates/qbx_lua_ls/README.md#locals-that-are-assigned-again),
  and `local name` declares no missing value: only the values given later count.

Guards only narrow the code after them, so a read inside a function defined before the guard is
reported, as `return name:upper()` is in a `local function` above `if not name then return end`:
the function may run before the guard does. lua-language-server reports it as well.

Unlike lua-language-server, qbx-lua-ls also checks arithmetic, concatenation, `#`, `<`, `<=`, `>`,
`>=` and `for` bounds.

## Missing documentation

`missing-global-doc`, `missing-local-export-doc` and `incomplete-signature-doc` are off by default
and keep lua-language-server's names, so a `.luarc.json` that turns them on with
`diagnostics.neededFileStatus` turns them on here too. Turned on, each reports the parameters of a
function that no `@param` names, and each value of each `return` at a position that no `@return`
covers:

```toml
[rules]
"missing-global-doc" = "warning"
"incomplete-signature-doc" = "warning"
```

```lua
function GetName(id) return tostring(id) end -- parameter 'id' of global function 'GetName' has no @param annotation

---@param id integer
local function getName(id) return tostring(id) end -- incomplete signature: return value #1 has no @return annotation
```

- `missing-global-doc` checks the global functions defined with `function Name()` or
  `Name = function()`, also inside other functions, but not fields of global tables or methods. One
  without parameters or returned values needs a comment instead: any comment directly above it, or
  after code on the line its parameters start on, other than `---@diagnostic` lines.
- `missing-local-export-doc` checks the local functions a file exports, and asks for a comment the
  same way. A module exports those it assigns by name to a field of a local table that a `return`
  gives, as `M.name = name` before `return M`; functions defined with `function M.name()` are not
  checked, as in lua-language-server. `exports('Name', fn)` exports `fn`, and a function written in
  the call. A global function passed to `exports` is left to `missing-global-doc`.
- `incomplete-signature-doc` checks every function whose doc comment has `@param` or `@return`
  lines, handlers passed to calls and functions in table fields included. A global function can be
  reported by it and `missing-global-doc` alike.

A function's doc comment is the one above the line it starts on, or above a call that spans lines
and passes it; comment lines in between do not separate them, a blank line does. Only the first
function that starts on the line takes it. `@param ...` or `@vararg` documents `...`, and `self`
and parameters that start with `ignore_unused_prefix` need no `@param`. A call or `...` at the end
of a `return` counts as one value, and a last `@return` of `...T`, or named `...`, covers every
later value. A `---@type fun(...)` in the doc comment documents the parameters and values it lists.

A parameter that the function type its function is passed as names needs no `@param` either, such
as `source` and `phoneNumber` of a handler passed to a wrapper documented with
`---@param callback fun(source: number, phoneNumber: string, ...)`; the parameters that fall into
its `...` still do. qbx-lint knows the function types of the global functions its resource
documents and of the local functions and local tables of the file. qbx-lua-ls knows every declared
type: those of stubs and other resources, class fields, and the `---@type` of a variable or field.

Unlike lua-language-server, these rules count `---@type fun(...)`, the function type a function is
passed as, `@vararg` and a `...T` `@return` as documentation, takes a comment above
`Name = function()` as that function's comment, asks for a comment on a function whose only
`return` gives no value, and marks the name of a function that needs a comment rather than the
whole function.

## Events, exports, and locales

The CLI first collects event registrations and exports from the resources being analyzed, then
checks their callers. When an input belongs to a resource, the other Lua files in that resource
contribute to analysis even if only one file was requested. Including related resources in the
same run provides more context for calls between resources.

Static event names can be checked for the target side and missing or excess arguments. Export
checks report excess arguments and unknown names. Unavailable resources and computed names limit
these checks. A diagnostic about missing event arguments may describe an intentional optional
parameter.

qbx-lua-ls also takes the type of a resource's exports from LuaLS definitions:
`---@type PhoneExports` above `exports.phone = {}` or `exports['phone'] = {}`, or
`---@class qbx_core` above `exports.qbx_core = {}` with methods such as
`function exports.qbx_core:GetCid(source) end`. Such a declaration counts in a
[definition file](#definition-files-outside-resources) or any file with `---@meta`, and elsewhere
when the annotation is there; a plain `exports.Name = fn` registers an export of its own resource,
and a table a test assigns to `exports.phone` declares nothing. `exports.phone` and
`exports['phone']` then offer the members of the declared type first and the exports the resource
registers after them, the declared one winning when both name a member, also for resources the
workspace lacks. Completion of `exports['` and of resource names, as in `GetResourceState('`,
lists those resources too. `fivem/unknown-export` only knows the exports resources register.

Locale checks compare static `locale('key')` calls with `locales/en.json`, falling back to the first
JSON file in `locales/` when that file is absent. Unused keys are reported on the JSON file when
the run includes the resource manifest. Dynamic locale usage limits which keys can be considered
unused.

## Escrowed, obfuscated and mixed-language resources

Encrypted FiveM files beginning with `FXAP`, Lua bytecode, and detected binary blobs are skipped.
Obfuscated or minified files are skipped too: a file counts as obfuscated when one of its lines is
at least 4096 bytes long and contains the `function` keyword. Long data lines, such as tables or
encoded strings, do not count. To skip other generated files, use `exclude`.

A `.fxap` marker or a skipped or unreadable script makes the resource opaque to checks that need
complete knowledge of its globals or locale usage. Readable scripts are still analyzed.

Unknown-export checks are suppressed for opaque resources and resources with non-Lua scripts.
An opaque resource may also handle events named with its `resource:` prefix, so a wrong-side
diagnostic is suppressed when the missing handler could be in that resource. Computed export
registrations similarly prevent a complete list of exports. `missing-parameter`,
`redundant-parameter`, `param-type-mismatch` and `discard-returns` do not check calls to globals in
an opaque resource, or to fields of global tables, such as `Utils.round()`, since an encrypted
script may define them differently. `param-type-mismatch` and `discard-returns` still check the
runtime's own functions, such as `math.floor`, and calls through `exports`.

Read-only linting uses replacement characters for invalid UTF-8 bytes. `--fix`, `fmt`, and
`fmt --check` report an encoding error for such source, and do not rewrite it. Convert its encoding
explicitly before using editing commands.

## Start order and optional integrations

For a resource below the `resources` directory next to a `server.cfg`, the linter reads `ensure`
and `start` commands, included `exec` files, and `ensure [category]` groups. A resource that starts
earlier satisfies a dependency check. Resources started by the same category command have no
relative order in this model.

The resources directory also supplies installed resource names, including manifest `provide`
aliases. `fivem/resource-not-found` reports an export call to a missing resource only when that
call is outside an `if`. Its default level is `info`, because the enclosing function may never run.

Dependency checks make allowances for integrations selected at runtime:

```lua
if Config.Inventory == 'ox' then
    exports.ox_inventory:AddItem(source, 'water', 1)
elseif Config.Inventory == 'qs' then
    exports['qs-inventory']:AddItem(source, 'water', 1)
end
```

Recognized string comparisons and early-return guards suppress missing-dependency and
resource-not-found findings for the selected code. The same allowances apply inside `pcall` or
`xpcall`, in files below `bridge`, `bridges`, `framework`, `frameworks`, `compat`, or `integrations`,
and in resource files not loaded as manifest scripts. Dependency checks also account for
`GetResourceState` usage. These are heuristics, not proof that a dependency is available at runtime.

## Formatting and fixes

```sh
qbx-lint fmt resources
qbx-lint fmt --check resources
qbx-lint fmt --config config/qbxlint.toml resources
qbx-lint --fix resources
```

The formatter adjusts spacing, indentation, and line wrapping while preserving constructs such
as one-line guards, expanded tables, trailing commas, inline casts, and blank-line grouping.
`quote_style` accepts `preserve`, `single`, and `double`; quote changes are limited to strings that
do not need new escapes.

Before writing, the formatter compares the input and output tokens and comments. String contents
are compared after decoding escapes. A verification failure leaves that file unchanged. Statements
with comments the printer cannot place are retained verbatim. Syntax errors also prevent formatting.

`--fix` applies available, non-overlapping fixes and then lints again, including those of rules
whose findings `--min-severity` leaves out, such as the removal of the whitespace `trailing-space`
reports at the end of lines. The candidate output must parse before it is written. A source file
that changed after analysis is not overwritten. Hash literal replacements are offered in value
positions; a standalone `GetHashKey('adder')` call cannot become a bare hash statement.

Editing commands can process several files before encountering an error; they are not a
transaction over the entire input. Review the resulting diff.
