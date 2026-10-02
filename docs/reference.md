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
Directory traversal also skips hidden directories and does not follow directory symlinks.

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
| `diagnostics.disable` | `off` for `undefined-global`, `lowercase-global`, `unused-local`, `unused-function`, `unused-label`, `redefined-local`, `unreachable-code`, `empty-block`, `unbalanced-assignments`, `duplicate-index`, `duplicate-set-field`, `count-down-loop`, `missing-parameter`, `redundant-parameter`, `undefined-doc-name`, `undefined-doc-param`, `duplicate-doc-alias`, `duplicate-doc-field`, `missing-fields`, `assign-type-mismatch`, `invisible`, `param-type-mismatch`, `return-type-mismatch`, `missing-return`, `redundant-return-value`, `discard-returns`, `cast-type-mismatch`, `no-unknown` and `need-check-nil`; EmmyLua's `unused` covers the `unused-*` rules |
| `diagnostics.severity` | Levels for the same codes (`Error`, `Warning`, `Information`, `Hint`, with or without a trailing `!`) |
| `workspace.ignoreDir` | Exclusions. LuaLS entries are gitignore-style patterns; `.emmyrc.json` entries are directories from the root |
| `workspace.ignoreGlobs` | Exclusions, as glob patterns |

Codes that only share a name with a rule here, such as `undefined-field` and `deprecated`, are
ignored, as are all other settings. Keys may be dotted (`"diagnostics.globals"`), nested, or
prefixed with `Lua.`. Comments and trailing commas are accepted. Formatting keeps its defaults; in
the language server, the editor's indentation settings still apply. Discovery skips a file or
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
to the server. Event registrations inside these regions use that effective side.

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
or a literal the type does not list, is reported.

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
operator, an annotation, a stub, or the value a local that is never assigned again is declared
with. A literal stored in such a local counts by its kind only: `local mode = 'dev'` is a setting to
change, so it passes for `"fast" | "slow"`, while `local count = 5` is still no `string`. A
`--[[@as T]]` right after an argument casts it. Only clear cases count, and the rest is left alone:

- Natives. The runtime converts their arguments, so `0` and `1` pass for a `boolean`, a string for
  a hash, a number for a string, and a vector for three floats.
- `nil`, written out or as the `?` of a `string?`, which is checked as a `string`. Annotations often
  leave out the `?` of a parameter that code skips.
- `false`, which FiveM code passes to skip a parameter, as in `AddItem(source, item, 1, false, info)`,
  since exports and events serialize their arguments.
- Types inferred from assigned values, such as that of `Config.Value = ''`.
- Parameters typed with a generic of the function called: the arguments of the call bind it, which
  declares nothing. A function that a callee passes to a callback, such as `resolve` of
  `fun(resolve: fun(value: T))`, takes what the other arguments of that call declare for the
  generic, as the `boolean` of `Promise:New('boolean', function(resolve) end)` for a `` `T` ``, and
  any value for a generic they leave unbound. The values such a function returns are left out.
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
`---@field [string] any`, takes any name. A table type named as a parent, like `{ id: integer }` or
`table<string, any>`, declares its fields and indices for the class, so `Name : table<string, any>`
takes any name too, while a parent that is neither a class nor such a table type, like a plain
`table`, takes any key.

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

Only clear cases count. Values whose type is not known are skipped, and so is a local that is
assigned again after its declaration, since its declared type may not be what it holds. The same
applies to the values `assign-type-mismatch` checks, where `nil` is a value like any other:
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
type of their own, and other locals whose type is inferred from what they hold, take any value,
and so do globals in assignments that have no `---@type` above them.

The value stored is what the code is known to give. A global's `nil` tells nothing: one declared as
`CurrentZone = nil` and set to a name by an event handler holds whatever the handler gives it, so
storing it anywhere passes. A `--[[@as T]]` or `---@as T` right after a value, on its line, casts
it to `T` as in lua-language-server, which silences a check that the code knows better than:
`local count = GetValue() --[[@as integer]]` stores an `integer`. On a call, the cast types its
first value.

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
A local whose type is only inferred, or that is assigned again and has no annotation, is not
checked, and neither is one declared as `nil`, which the cast gives its type as in
lua-language-server, nor the `+T` and `-T` entries, which change the type rather than replace it.
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
local that is never assigned again has the type of the value it is declared with, so
`local state = GetState()` is checked like the call, and inside a
[type guard](../crates/qbx_lua_ls/README.md#type-guards) the type the guard narrows it to: the right
side of `class ~= 13 or class ~= 14` only runs when `class` is `13`, so it is always true. A
literal stored in such a local counts by its kind only: `local mode = 'dev'` is a setting to
change, so `mode == 'prod'` passes, while `mode == false` is still reported as a `string` compared
with `false`.

Only clear cases count, and the rest is left alone:

- Types that are inferred from assigned values. `Config.Webhook = ''` tells what the config holds
  today, not that `Config.Webhook == false` cannot be true once someone edits it. The same goes
  for what an undocumented function returns, and for the value of `a or b` and `a and b`.
- Comparisons with `nil`. Annotations often leave out the `?` of a value that may be missing, and
  the check for it is deliberate.
- A local that is assigned again after its declaration, since its declared type may not be what
  it holds, unless a `---@cast name T` types it. `---@cast name +T` does not: it adds `T` to a
  type that is still not known, as it does for a local with no declared type.
- Natives documented as `boolean` compared with a number. Scripts can get `1` instead of `true`
  from them, so `IsPedInAnyVehicle(ped) == 1` is valid.

## Unknown types

`no-unknown` is off by default. Turned on, qbx-lua-ls reports each parameter, local and loop
variable whose type is unknown: none is declared, and none can be inferred from its value, from
the function a callback is passed to, from the declared type of the table field a function is
written in, or from what a loop goes through.

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
`setmetatable`. A local declared without a value needs a `---@type`, since
later assignments are not followed. Names that start with `ignore_unused_prefix`, and `self`, are
not reported. To check only your own resources, set the level in an `[[overrides]]` entry instead
of `[rules]`.

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
print(name:lower()) -- the read above raises the error first
```

A local may hold `nil` when its declared type allows it: its `---@type` or `@param`, or the
`@return` or `@field` of the function or class its value comes from, whoever declares them,
stubs and other resources included. `false`, as in `false|string`, counts as well. The
[type guards](../crates/qbx_lua_ls/README.md#type-guards) and casts around the read narrow the
type first, so `if not name then return end`, `if name then`, `name and name:upper()`,
`assert(name)` and `---@cast name -?` all check it, as does a condition that reads from it, such as
`if player?.job then` for `player`. `?.`, `?[` and `?:` give `nil` for a missing value and are not
reported. Guards that together rule out every value of the type, as
`round and (round == true or i < round)` does for a `boolean?`, keep the type whole, but the `nil`
that `round and` rules out stays out.

A local that may hold `nil` is reported as well where a call passes it for a parameter that does not
take `nil`, in every signature the call may use as `param-type-mismatch` reads them. A parameter
takes `nil` when it is optional, as `label?`, or its type allows it or any value, as `string?`,
`any` or a generic do. Natives are left out, and so is an argument of another type than the
parameter takes, which `param-type-mismatch` reports:

```lua
---@param target Player
local function greet(target) end

local player = GetPlayer() -- Player?
greet(player) -- `player` may be nil, which parameter `target` of type `Player` does not take: ...
```

lua-language-server reports such an argument as a `param-type-mismatch`, so a
`---@diagnostic disable` comment for that rule silences it too.

Only clear cases count, and the rest is left alone:

- Fields and the values of calls, which guards do not narrow:
  `if self.target then self.target:kill() end` would be reported otherwise.
- A local that is assigned again after its declaration, unless a `---@cast name T` types it.
  `name = name or 'none'` is how code fills in a missing value.
- A value of a call that a guard on another of its values covers. After
  `local vehicle, coords = lib.getClosestVehicle(pos)` and `if not vehicle then return end`,
  `coords` is not reported: annotations declare such values one by one, rather than as
  [sets of values](../crates/qbx_lua_ls/README.md#sets-of-returned-values).
- What a call gives for `source` alone, or for a local that holds it, as
  `ESX.GetPlayerFromId(source)` or `exports.qbx_core:GetPlayer(src)` after `local src = source`:
  lookups of the player whose event or callback runs only miss one that has just left.
  `ESX.GetPlayerFromId(target)` is reported.
- Reads after one reported in the same function, which points at the same missing check.
- Reads after one that always runs before them. Once `name:upper()` has run without an error,
  `name` held a value, so the reads after it in its block are not reported, including those in
  functions defined there. A read inside an `if` branch, on the right of `and` and `or`, or in the
  condition of a `repeat` loop that a `break` can leave, does not count for the code after it.

Guards only narrow the code after them, so a read inside a function defined before the guard is
reported, as `return name:upper()` is in a `local function` above `if not name then return end`:
the function may run before the guard does. lua-language-server reports it as well.

Unlike lua-language-server, qbx-lua-ls also checks arithmetic, concatenation, `#`, `<`, `<=`, `>`,
`>=` and `for` bounds, leaves alone the lookups of `source` above, and reports a local once per
function rather than at every read.

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

`--fix` applies available, non-overlapping fixes and then lints again. The candidate output must
parse before it is written. A source file that changed after analysis is not overwritten. Hash
literal replacements are offered in value positions; a standalone `GetHashKey('adder')` call
cannot become a bare hash statement.

Editing commands can process several files before encountering an error; they are not a
transaction over the entire input. Review the resulting diff.
