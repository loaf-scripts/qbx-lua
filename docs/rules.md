# Rules

These analysis rules power both the `qbx-lint` CLI and `qbx-lua-ls` editor diagnostics.
Configure them through `qbxlint.toml`; language-server clients can supply
[levels for the rules it does not set](../crates/qbx_lua_ls/README.md#configuration).

Use `qbx-lint --list-rules` to inspect the rules in your installed version. Levels below are the
defaults; `hint` findings require `--min-severity hint` to appear in CLI output. Configuration can
set any rule to `off`, `hint`, `info`, `warning`, or `error`.

## Lua

| Rule | Default | Check |
| --- | --- | --- |
| `syntax-error` | error | Source rejected by the parser, including its nesting limit. |
| `undefined-global` | warning | A read of a global absent from the known environment. |
| `undefined-field` | warning | An unknown field on a standard library table. qbx-lua-ls also reports a field read from a value whose `---@class` or table type does not have it, such as `point.z` for a class with `x` and `y`, as TypeScript reports a property its type does not declare. A type has the fields that `inject-field` lets code set; global tables, the exports of a resource without a declared type, empty tables and values of unknown type have any. See [Undefined fields](reference.md#undefined-fields). |
| `unused-local` | warning | A local variable is never read. A local declared with a table constructor that code only sets fields of, as `t.a = 1`, `t[k] = v` or `function t.f()` do, is not read either, as in lua-language-server; the table a `---@class` annotation declares is left alone. |
| `unused-function` | warning | A local function is never used. One that only itself, or local functions that are never used, call is not used either, as in lua-language-server; a local declared with a function is reported under `unused-local` then. A call from a callback inside such a function, or from a function that starts with `ignore_unused_prefix`, counts as a use. `---@meta` files are not checked for these. |
| `unused-argument` | hint | A function parameter is never read. |
| `unused-loop-variable` | hint | A loop variable is never read. |
| `unused-vararg` | hint | A function takes `...` and its body never uses it; a `...` of a function inside it is that function's own. Empty bodies and `---@meta` files are not checked, as in lua-language-server. |
| `unused-label` | warning | A label is never targeted by a `goto`. |
| `undefined-label` | error | A `goto` targets a label outside its visible scope. |
| `redefined-local` | warning | A local is declared twice in one scope. The parameters of a function and the locals at the top of its body share one, as in Lua, so `function(source) local source = source end` declares `source` twice; a local in a nested block or a loop body hides the outer one instead, which `shadowed-local` reports. |
| `shadowed-local` | off | A local hides one in an enclosing scope. |
| `unreachable-code` | warning | Code follows a statement that never finishes: a `break` or `goto`, an `if` with an `else` none of whose branches runs past its end, by `return`, `break`, `goto` or a call of `error` or `os.exit`, or a `while true` loop that only a `return` leaves. Code from a label on can still be reached by a `goto`. A loop with a `return` in it, a `do return end` and an `error()` call of its own are not exits, as in lua-language-server. |
| `redundant-return` | hint | A `return` without values ends a function body, where the function returns anyway. One inside an `if` or `do` block, which skips the code after it, and one that ends the file are not reported. |
| `empty-block` | info | A block has no statements. |
| `trailing-space` | hint | A line ends in spaces or tabs, or holds nothing else. Whitespace inside a comment or a string, such as a long string that spans lines, is left alone, as the formatter leaves it. Fix available. |
| `unbalanced-assignments` | warning | An assignment or `local` statement has mismatched target and value counts, as in `local a, b = 1`. A last value that is a call or `...` gives any number of values. |
| `newline-call` | warning | A line starts with an expression in parentheses that is then indexed or called, such as `(value):method()` or `(function() end)()`, so Lua reads it as the argument of a call to the expression that ends the line above. A `;` before the `(` starts a new statement. |
| `newfield-call` | warning | An entry of a table constructor ends a line and the next line starts with `(`, a string or a table, so Lua reads both as one call: `{ print` followed by `("x") }` holds what `print("x")` returns. A `,` between them makes two entries. Like lua-language-server, entries with a key are not checked. |
| `duplicate-index` | warning | A table constructor assigns the same key twice. |
| `duplicate-set-field` | warning | A function is assigned to the same field of the same table twice in one block of a file, so the second replaces the first. A block is the main chunk, a function body, one branch of an `if`, or the code after an `if` with a branch that returns, such as an `IsDuplicityVersion()` or `lib.context` guard. A read of the field between the two, as a wrapper has, or an assignment to the table's variable or to a field it is in, also separates them. Definitions in other files or in `---@meta` files are not compared. |
| `duplicate-argument` | error | Function parameters share a name. |
| `missing-parameter` | warning | A call leaves out an argument that the function's LuaCATS annotations require. qbx-lua-ls also reports `---@callback` wrapper calls whose payload leaves out a value the registered handler requires. |
| `redundant-parameter` | warning | A call passes more arguments than the function has parameters, documented or not, so the extra values are lost. qbx-lua-ls also reports `---@callback` wrapper calls whose payload passes more values than the registered handler takes. |
| `undefined-doc-name` | warning | A LuaCATS annotation names a type that no `@class`, `@alias` or `@enum` declares, or that only `(server)` or `(client)` declarations of the other side declare. Reported by qbx-lua-ls only. |
| `undefined-doc-param` | warning | A `@param` names no parameter of the function its doc comment documents, or no function follows the comment. A doc comment documents the functions of the statement below it, including a local or global function of the file that the statement passes by name, and every function whose parameter list starts on the next line of code; comment lines in between do not separate them, a blank line does. The parameter names of a `@type fun(...)` in the comment also count. `---@meta` files are not checked. |
| `duplicate-doc-alias` | warning | An `@alias` or `@enum` repeats the name of an alias, enum or class that the same file declares for the same side. `(server)` and `(client)` declarations of one name, and names marked `(partial)`, are allowed. |
| `duplicate-doc-field` | warning | A class declares a field twice in one file for the same side. A repeated function field is another signature of it. |
| `duplicate-doc-param` | warning | A doc comment has two `@param` lines for the same parameter. Each of them is reported. |
| `doc-field-no-class` | warning | A `@field` has no `@class` above it in its doc comment, or another tag, such as `@deprecated` or `@type`, comes between them. lua-language-server then gives the field to no class; qbx-lua-ls still gives it to the class above. Only other fields, `@operator`, `@overload`, `@source`, `@diagnostic` and description lines may come between a `@class` and its fields; other comments and tags lua-language-server does not read, such as `@author`, are descriptions. |
| `circle-doc-class` | warning | A `@class` inherits from itself, directly as `---@class A : A` does, or through its parents, which other files may declare. Reported by qbx-lua-ls only. |
| `unknown-operator` | warning | An `@operator` names an operator that annotations cannot declare, such as `eq` or `index`. The operators are `add`, `sub`, `mul`, `div`, `mod`, `pow`, `idiv`, `band`, `bor`, `bxor`, `shl`, `shr`, `concat`, `unm`, `bnot`, `len` and `call`. |
| `unknown-cast-variable` | warning | A `---@cast` names no local that is in scope where it is written, such as a global, a field like `M.x`, or a local declared below it. |
| `unknown-diag-code` | warning | A `---@diagnostic` or `-- qbx-lint:` comment names a code that is neither a rule here nor a lua-language-server diagnostic or syntax error, so it suppresses nothing. A close match is suggested. |
| `missing-global-doc` | off | A global function, defined with `function Name()` or `Name = function()`, has a parameter that no `@param` names, or a `return` whose values no `@return` covers. One without parameters or returned values has no comment. See [Missing documentation](reference.md#missing-documentation). |
| `missing-local-export-doc` | off | The same for a local function that a file exports: assigned by name to a field of a local table that a `return` gives, or passed to `exports('Name', fn)`, and for a function written in an `exports` call. |
| `incomplete-signature-doc` | off | A function whose doc comment has `@param` or `@return` lines leaves out a parameter or a returned value. A parameter that the function type its function is passed as names, such as `source` of a handler passed to a documented callback wrapper, needs none. |
| `missing-fields` | warning | A table constructor typed as a `---@class` or a shape such as `{ name: string }`, including one returned for an `@return` type, leaves out required fields. A table typed as a union of them needs the required fields of one. The tables that an array, a `table<K, V>` or a tuple holds are checked by the type of their entry, like the `{}` of `{ {} }` for `Dog[]`. Reported by qbx-lua-ls only. |
| `assign-type-mismatch` | warning | A table constructor or an assignment stores a value that the `@field` type, or the value type of an index such as `[string] number`, does not take, such as a number for a `string` field. Tables typed as a shape, such as `{ value: string }`, an array or a `table<K, V>` are checked by the types of their entries. Clearing a field with `nil` needs a type that allows it, such as `string?` or `string|nil`. The same goes for a value stored in a variable typed with `---@type` or `@param`, such as `"test"` for `---@type number`, except for the `nil` that the statement under a `---@type` gives the names it declares. Reported by qbx-lua-ls only. |
| `param-type-mismatch` | warning | A call passes an argument that the type of its parameter does not take, such as a string for `number`, a `string\|number` for `string`, a literal the type does not list, or `nil` or a `string?` for a parameter that does not take `nil`. Natives are checked as the runtime converts their arguments. Reported by qbx-lua-ls only. |
| `undeclared-field` | warning | A field or key that a strict `---@class`, marked `(strict)` or `(exact)` or made strict by `strict_classes`, does not declare is set in a table constructor or assignment, or read. Reported by qbx-lua-ls only. |
| `inject-field` | warning | A field is set, by an assignment or a `function value:name()` statement, through a value whose `---@class` or table type does not have it, like a parameter typed as a class or a loop variable over tables built as `{ label = 'a' }`, as TypeScript reports a property its type does not declare. Fields set through the name that owns a table declare them instead. See [Injected fields](reference.md#injected-fields). Reported by qbx-lua-ls only. |
| `invisible` | warning | A field or method that a `---@class` keeps `private`, `protected` or `package`, with `---@field private name type` or `---@private` above the statement that sets it, is read, set or called outside its class, subclasses or file. See [Member visibility](reference.md#member-visibility). Reported by qbx-lua-ls only. |
| `return-type-mismatch` | warning | A function returns a value that its `@return` type does not take, such as a number for `string`. A function without `@return` is checked against the function type it is written as, such as the `fun(): string` of the parameter it is passed for. Reported by qbx-lua-ls only. |
| `missing-return` | warning | A function with a required `@return` value, or written as a function type that returns one, can reach its end without returning, or has a `return` with fewer values than required. Reported by qbx-lua-ls only. |
| `redundant-return-value` | warning | A `return` passes more values than the function's `@return` annotations, or the function type it is written as, declare. A trailing `...T` takes any number. Reported by qbx-lua-ls only. |
| `discard-returns` | warning | A call on a line of its own drops the values of a function marked `@nodiscard`, such as `tostring(value)`. Reported by qbx-lua-ls only. |
| `cast-type-mismatch` | warning | A `---@cast name T` gives a local a type that its declared type does not take, such as `string` for a local declared as `integer`, a literal the type does not list, or a class it neither names nor extends. Reported by qbx-lua-ls only. |
| `cast-local-type` | warning | An assignment gives a local without `---@type` or `@param` a value that the type of the value it is declared with does not take, such as a string after `local speed = 5`, or `nil` for a local that cannot be `nil`. A literal written out widens to its kind, a loop variable has the type its loop gives it, and a parameter of a function passed for a function type the type it declares; a local declared without a value or as `nil` takes any value. See [Local types](reference.md#local-types). Reported by qbx-lua-ls only. |
| `no-unknown` | off | A parameter, local or loop variable has no type: none is declared with `@param` or `@type`, and none can be inferred. A value of unknown type is stored in a typed variable or field, passed for a typed parameter, or returned where `@return` declares a type. Reported by qbx-lua-ls only. |
| `const-reassign` | error | An assignment to a `<const>` or `<close>` local. |
| `close-non-object` | warning | A `<close>` local has no value, or one that cannot be closed, such as a number, a string, `true` or a function. A value passes when part of its type is `nil`, `false`, a table or a class, which may have a `__close` metamethod, or unknown. Reported by qbx-lua-ls only. |
| `self-assignment` | warning | A variable is assigned to itself. |
| `self-comparison` | warning | A comparison has the same expression on both sides. |
| `ambiguity-1` | warning | An `or` sits next to arithmetic, concatenation or a bitwise operator, which binds tighter: `x + y or 0` adds before the `or`, where `x + (y or 0)` was likely meant, and `x or 1 + y` adds after it, where `(x or 1) + y` was likely meant. As in lua-language-server, the `or` has to give a literal in the first case, other than one the arithmetic ends with, and take one in the second. Parentheses around either part say which was meant. |
| `count-down-loop` | warning | A numeric `for` loop starts above its end without a negative step: a literal start above a literal end, with no step or a positive one, which never runs, or `#list, 1` or arithmetic on `#list` such as `#list - 1, 1`, with no step, which never runs once the start is above 1. Fix available. |
| `impossible-comparison` | info | An `==` or `~=` compares values whose declared types share no value, such as a string with a number, or a literal with a type that does not list it, so it always gives the same answer. Comparisons with `nil` are not reported. Reported by qbx-lua-ls only. |
| `need-check-nil` | warning | A local whose declared type allows `nil` or `false`, such as the `string?` a function's `@return` declares, is indexed, called, used as a key to store a value under, used in arithmetic, concatenation, `#` or a `<`, `<=`, `>` or `>=` comparison, given as a numeric `for` bound, or passed for a parameter typed with a native handle name such as `Vehicle` that does not take `nil`, with no guard or cast ruling the missing value out. Every such read is reported. Reported by qbx-lua-ls only. |
| `lowercase-global` | warning | A global definition starts with a lowercase letter. |
| `implicit-global` | warning | A function creates a global without a file-scope declaration. |
| `builtin-overwrite` | warning | Code overwrites a known runtime global or native. |
| `global-in-nil-env` | warning | A name that is no local is read or set after `local _ENV = nil`, or a `local _ENV` without a value, which raises an error, since Lua looks such names up in `_ENV`. Under another `local _ENV`, or a parameter of that name, they are fields of that table, so the global rules leave them alone; `local _ENV = _ENV` keeps the globals. |
| `deprecated` | warning | Code uses a deprecated runtime function, such as `RegisterServerEvent`. qbx-lua-ls also reports reads of a global, field or method whose definitions all have `---@deprecated` above them, such as `Lib.old()` after `---@deprecated use Lib.new` `function Lib.old() end`, with the reason it gives. A `---@field` of a class does not count as a definition, and locals are not reported, as in lua-language-server. |

## FiveM and Qbox

| Rule | Default | Check |
| --- | --- | --- |
| `fivem/native-wrong-side` | error | A native or runtime global is used on a side that lacks it, such as `TriggerClientEvent` in a client script. The `io` and `os` libraries only exist on the server, so they are reported in shared scripts too, unless an `IsDuplicityVersion()` or `lib.context` guard keeps the code on the server. |
| `fivem/import-not-declared` | warning | A library global is used without its manifest import on that side. |
| `fivem/loop-never-yields` | error | An apparently infinite loop has no recognized yield or exit. |
| `fivem/source-after-yield` | warning | Global `source` is read after a yield or in a deferred callback. |
| `fivem/citizen-prefix` | info | A `Citizen` call has a shorter global alias. Fix available. |
| `fivem/hash-literal` | info | A literal `GetHashKey` argument can become a compile-time hash. Fix available in valid value positions. |
| `fivem/legacy-native-pattern` | info | Use of `GetPlayerPed(-1)`, `GetDistanceBetweenCoords`, `Vdist`, or `Vdist2`. A fix is available for `GetPlayerPed(-1)`. |
| `qbox/prefer-cache` | hint | A native lookup has a corresponding ox_lib cache value. |
| `qbox/legacy-core-object` | hint | Use of the QBCore compatibility core object. |
| `fivem/event-argument-count` | warning | More event arguments than known handlers accept. |
| `fivem/event-missing-arguments` | info | Fewer arguments than known handlers declare. |
| `fivem/event-wrong-side` | warning | Known handlers exist only on the other side. |
| `fivem/export-argument-count` | warning | More arguments than the known exported function accepts. |
| `fivem/unknown-export` | info | An analyzed resource does not register the named export. |
| `fivem/resource-not-found` | info | A call outside an `if` targets an export of a resource not found in the server. |
| `qbox/unknown-locale-key` | warning | A static key is absent from the resource's selected locale file. |
| `qbox/unused-locale-key` | info | A locale key has no known use in the resource. |

## Manifests

| Rule | Default | Check |
| --- | --- | --- |
| `manifest/missing-field` | warning | Missing `fx_version` or `game`. |
| `manifest/missing-file` | warning | A referenced file or glob has no match. |
| `manifest/unknown-directive` | warning | A directive resembles a misspelled known name. |
| `manifest/missing-dependency` | info | An export dependency lacks a declared or recognized start order. |
| `manifest/unlisted-script` | off | A Lua file is not referenced by the manifest. |
| `manifest/lua54` | off | Missing `lua54 'yes'`; an opt-in check for older server artifacts. Fix available. |

## Security patterns

| Rule | Default | Check |
| --- | --- | --- |
| `security/client-supplied-source` | warning | A server net event accepts a player identifier from its caller. |
| `security/unvalidated-event-argument` | warning | A client argument reaches a sensitive call without a recognized check. |
| `security/sql-concatenation` | warning | A query is assembled with concatenation or formatting without a recognized parameter argument. |

These checks do not prove validation or authorization. For example, recognizing a parameter in
a guard does not establish that the guard always runs before the sensitive operation. Review the
handler's control flow and runtime behavior separately.

See the [analysis reference](reference.md) for exclusions, suppressions, and limits on checks
across files and resources.
