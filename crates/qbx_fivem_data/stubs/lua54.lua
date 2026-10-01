---@meta

---The names `type` gives: those of Lua, and those of the vectors, quaternions and matrices CfxLua adds.
---@alias lua_type "nil"|"number"|"string"|"boolean"|"table"|"function"|"thread"|"userdata"|"vector2"|"vector3"|"vector4"|"quat"|"matrix"
---LuaLS's name for the same values, which annotations written for it use.
---@alias type lua_type
---@alias gc_option "collect"|"stop"|"restart"|"count"|"step"|"isrunning"|"incremental"|"generational"
---@alias load_mode "b"|"t"|"bt"
---@alias open_mode "r"|"w"|"a"|"r+"|"w+"|"a+"|"rb"|"wb"|"ab"|"r+b"|"w+b"|"a+b"
---@alias popen_mode "r"|"w"
---@alias read_format integer|"n"|"l"|"L"|"a"|"*n"|"*l"|"*L"|"*a"
---@alias seek_whence "set"|"cur"|"end"
---@alias vbuf_mode "no"|"full"|"line"
---@alias hook_mask "c"|"r"|"l"|"cr"|"cl"|"rl"|"crl"|""
---@alias hook_event "call"|"tail call"|"return"|"line"|"count"
---@alias locale_category "all"|"collate"|"ctype"|"monetary"|"numeric"|"time"
---@alias coroutine_status "running"|"suspended"|"normal"|"dead"

---The global environment table; every global variable is a field of it.
---@type table<string, any>
_G = {}

---Version string of the running interpreter.
---@type string
_VERSION = "Lua 5.4"

---Raises an error when `v` is false or nil, otherwise returns every argument unchanged.
---@generic T
---@param v? T
---@param message? any
---@param ... any
---@return T v
---@return any ...
function assert(v, message, ...) end

---Controls the garbage collector. The meaning of the extra arguments and of the result depends on `opt` (default "collect").
---@param opt? gc_option
---@param ... any
---@return any result
function collectgarbage(opt, ...) end

---Runs the given file as a Lua chunk (stdin when omitted) and returns whatever the chunk returns.
---@param filename? string
---@return any ...
function dofile(filename) end

---Throws `message` as an error and never returns. `level` selects which call frame gets blamed (1 = caller, 0 = no position info).
---@param message any
---@param level? integer
function error(message, level) end

---Returns the metatable of `object`, or its `__metatable` field when that is set, or nil.
---@param object any
---@return any metatable
---@nodiscard
function getmetatable(object) end

---Iterates the array part of `t` as (index, value) pairs starting at 1 until the first nil value.
---@generic T
---@param t T[]
---@return fun(t: T[], i: integer): integer, T iterator
---@return T[] t
---@return integer i
function ipairs(t) end

---Compiles a chunk given as a string or as a piece-producing function. Returns the compiled function, or nil plus an error message.
---@param chunk string|fun(): string?
---@param chunkname? string
---@param mode? load_mode
---@param env? table
---@return function? fn
---@return string? err
---@nodiscard
function load(chunk, chunkname, mode, env) end

---Like `load`, but reads the chunk from a file (stdin when omitted).
---@param filename? string
---@param mode? load_mode
---@param env? table
---@return function? fn
---@return string? err
---@nodiscard
function loadfile(filename, mode, env) end

---Returns the key/value pair that follows `index` in the table traversal order; pass nil to get the first pair, nil is returned after the last.
---@generic K, V
---@param t table<K, V>
---@param index? K
---@return K? key
---@return V? value
---@nodiscard
function next(t, index) end

---Iterates every key/value pair of `t`, honouring the `__pairs` metamethod.
---@generic K, V
---@param t table<K, V>
---@return fun(t: table<K, V>, index?: K): K, V iterator
---@return table<K, V> t
---@return nil index
function pairs(t) end

---Calls `f` in protected mode. Returns true plus the results of `f`, or false plus the error value.
---@param f fun(...: any): ...any
---@param ... any
---@return boolean success
---@return any ...
function pcall(f, ...) end

---Writes all arguments to standard output, converted with `tostring` and separated by tabs.
---@param ... any
function print(...) end

---Compares two values for primitive equality without invoking `__eq`.
---@param v1 any
---@param v2 any
---@return boolean equal
---@nodiscard
function rawequal(v1, v2) end

---Reads `t[index]` without invoking `__index`.
---@param t table
---@param index any
---@return any value
---@nodiscard
function rawget(t, index) end

---Returns the length of a table or string without invoking `__len`.
---@param v table|string
---@return integer length
---@nodiscard
function rawlen(v) end

---Assigns `t[index] = value` without invoking `__newindex` and returns `t`.
---@generic T: table
---@param t T
---@param index any
---@param value any
---@return T t
function rawset(t, index, value) end

---Loads the named module once and returns its cached value, plus loader data describing where it was found.
---@param modname string
---@return any module
---@return any loaderdata
function require(modname) end

---Returns all arguments after position `index` (negative counts from the end). With "#" it returns the number of extra arguments instead.
---@param index integer|"#"
---@param ... any
---@return any ...
---@overload fun(index: "#", ...: any): integer
---@nodiscard
function select(index, ...) end

---The fields Lua reads from the metatable of a table: the metamethods it calls for operations on the table, and its settings.
---@class metatable
---@field __index? table|fun(t: table, key: any): any
---@field __newindex? table|fun(t: table, key: any, value: any)
---@field __call? fun(t: table, ...: any): ...any
---@field __tostring? fun(t: table): string
---@field __len? fun(t: table): integer
---@field __pairs? fun(t: table): function, table, any
---@field __close? fun(t: table, err: any)
---@field __gc? fun(t: table)
---@field __unm? fun(a: table): any
---@field __bnot? fun(a: table): any
---@field __add? fun(a: any, b: any): any
---@field __sub? fun(a: any, b: any): any
---@field __mul? fun(a: any, b: any): any
---@field __div? fun(a: any, b: any): any
---@field __mod? fun(a: any, b: any): any
---@field __pow? fun(a: any, b: any): any
---@field __idiv? fun(a: any, b: any): any
---@field __band? fun(a: any, b: any): any
---@field __bor? fun(a: any, b: any): any
---@field __bxor? fun(a: any, b: any): any
---@field __shl? fun(a: any, b: any): any
---@field __shr? fun(a: any, b: any): any
---@field __concat? fun(a: any, b: any): any
---@field __eq? fun(a: table, b: table): boolean
---@field __lt? fun(a: any, b: any): boolean
---@field __le? fun(a: any, b: any): boolean
---@field __mode? "k"|"v"|"kv"
---@field __name? string
---@field __metatable? any

---Sets (or clears, with nil) the metatable of `t` and returns `t`. Fails if the current metatable has a `__metatable` field.
---@generic T: table
---@param t T
---@param metatable? table|metatable
---@return T t
function setmetatable(t, metatable) end

---Converts a value to a number, or returns nil if it cannot be converted. With `base` (2-36) the argument must be a string holding an integer in that base.
---@param e any
---@param base? integer
---@return number? value
---@nodiscard
function tonumber(e, base) end

---Converts any value to a human-readable string, honouring `__tostring` and `__name`.
---@param v any
---@return string str
---@nodiscard
function tostring(v) end

---Returns the name of the basic type of `v`.
---@param v any
---@return lua_type name
---@nodiscard
function type(v) end

---Emits a warning built from the concatenation of its string arguments. Messages starting with "@" are control messages ("@on", "@off").
---@param msg1 string
---@param ... string
function warn(msg1, ...) end

---Like `pcall`, but runs the message handler `msgh` on the error value before the stack unwinds.
---@param f fun(...: any): ...any
---@param msgh fun(err: any): any
---@param ... any
---@return boolean success
---@return any ...
function xpcall(f, msgh, ...) end

---@class coroutinelib
coroutine = {}

---Closes a suspended or dead coroutine, running its pending to-be-closed variables. Returns true, or false plus the error.
---@param co thread
---@return boolean noerror
---@return any errorobject
function coroutine.close(co) end

---Creates a new suspended coroutine whose body is `f`.
---@param f fun(...: any): ...any
---@return thread co
---@nodiscard
function coroutine.create(f) end

---Tells whether the given coroutine (default: the running one) is allowed to yield.
---@param co? thread
---@return boolean yieldable
---@nodiscard
function coroutine.isyieldable(co) end

---Starts or continues `co`, passing the extra arguments in. Returns true plus yielded/returned values, or false plus the error.
---@param co thread
---@param ... any
---@return boolean success
---@return any ...
function coroutine.resume(co, ...) end

---Returns the running coroutine and a boolean that is true when it is the main one.
---@return thread running
---@return boolean ismain
---@nodiscard
function coroutine.running() end

---Returns the state of `co`.
---@param co thread
---@return coroutine_status status
---@nodiscard
function coroutine.status(co) end

---Creates a coroutine and returns a function that resumes it on each call, propagating errors instead of returning a status flag.
---@param f fun(...: any): ...any
---@return fun(...: any): ...any resume
---@nodiscard
function coroutine.wrap(f) end

---Suspends the running coroutine; the arguments become the extra results of `resume`, and the next resume's arguments are returned here.
---@param ... any
---@return any ...
function coroutine.yield(...) end

---@class debuginfo
---@field name string
---@field namewhat string
---@field source string
---@field short_src string
---@field linedefined integer
---@field lastlinedefined integer
---@field what string
---@field currentline integer
---@field istailcall boolean
---@field nups integer
---@field nparams integer
---@field isvararg boolean
---@field func function
---@field ftransfer integer
---@field ntransfer integer
---@field activelines table<integer, boolean>
local debuginfo = {}

---@class debuglib
debug = {}

---Enters an interactive prompt that executes each entered line until a line containing only "cont" is read.
function debug.debug() end

---Returns the current hook function, its mask and its count for the given thread.
---@param co? thread
---@return function? hook
---@return string mask
---@return integer count
---@nodiscard
function debug.gethook(co) end

---Returns a table describing a function or a stack level. `what` selects which groups of fields get filled in.
---@param f integer|function
---@param what? string
---@return debuginfo? info
---@overload fun(thread: thread, f: integer|function, what?: string): debuginfo?
---@nodiscard
function debug.getinfo(f, what) end

---Returns the name and value of local number `index` at stack level `f`, or only the parameter name when `f` is a function.
---@param f integer|function
---@param index integer
---@return string? name
---@return any value
---@overload fun(thread: thread, f: integer|function, index: integer): string?, any
---@nodiscard
function debug.getlocal(f, index) end

---Returns the real metatable of any value, ignoring `__metatable`.
---@param object any
---@return table? metatable
---@nodiscard
function debug.getmetatable(object) end

---Returns the registry table.
---@return table registry
---@nodiscard
function debug.getregistry() end

---Returns the name and value of upvalue number `up` of function `f`.
---@param f function
---@param up integer
---@return string? name
---@return any value
---@nodiscard
function debug.getupvalue(f, up) end

---Returns the `n`-th user value attached to a userdata and a boolean telling whether that slot exists.
---@param u userdata
---@param n? integer
---@return any value
---@return boolean exists
---@nodiscard
function debug.getuservalue(u, n) end

---Sets a new C stack limit. Has no effect since Lua 5.4.3.
---@deprecated
---@param limit integer
---@return integer|boolean previous
function debug.setcstacklimit(limit) end

---Installs `hook` as the debug hook. `mask` picks the events and `count` adds an every-N-instructions event; calling with no arguments removes the hook.
---@param hook? fun(event: hook_event, line?: integer)
---@param mask? hook_mask
---@param count? integer
---@overload fun(thread: thread, hook?: fun(event: hook_event, line?: integer), mask?: hook_mask, count?: integer)
function debug.sethook(hook, mask, count) end

---Assigns `value` to local number `index` at stack level `level` and returns the local's name, or nil if there is none.
---@param level integer
---@param index integer
---@param value any
---@return string? name
---@overload fun(thread: thread, level: integer, index: integer, value: any): string?
function debug.setlocal(level, index, value) end

---Sets the metatable of any value (nil removes it) and returns the value.
---@generic T
---@param value T
---@param metatable? table
---@return T value
function debug.setmetatable(value, metatable) end

---Assigns `value` to upvalue number `up` of `f` and returns the upvalue's name, or nil if there is none.
---@param f function
---@param up integer
---@param value any
---@return string? name
function debug.setupvalue(f, up, value) end

---Stores `value` as the `n`-th user value of `udata` and returns `udata`, or nil when the slot does not exist.
---@param udata userdata
---@param value any
---@param n? integer
---@return userdata? udata
function debug.setuservalue(udata, value, n) end

---Returns a stack traceback string, optionally prefixed by `message` and starting at `level`. A non-string, non-nil message is returned untouched.
---@param message? any
---@param level? integer
---@return string traceback
---@overload fun(thread: thread, message?: any, level?: integer): string
---@nodiscard
function debug.traceback(message, level) end

---Returns a unique identifier for upvalue number `n` of `f`, useful to detect upvalues shared between closures.
---@param f function
---@param n integer
---@return lightuserdata? id
---@nodiscard
function debug.upvalueid(f, n) end

---Makes upvalue `n1` of closure `f1` refer to upvalue `n2` of closure `f2`.
---@param f1 function
---@param n1 integer
---@param f2 function
---@param n2 integer
function debug.upvaluejoin(f1, n1, f2, n2) end

---@class file
local file = {}

---Closes the file. For handles created by `io.popen` it returns the process exit status like `os.execute`.
---@return boolean? ok
---@return string? exitcode
---@return integer? code
function file:close() end

---Writes any buffered output to the file.
---@return file? self
---@return string? err
function file:flush() end

---Returns an iterator that reads the file using the given formats on every step (default: one line). The file is not closed when iteration ends.
---@param ... read_format
---@return fun(): any, ...any iterator
function file:lines(...) end

---Reads from the file according to the given formats and returns one value per format, or nil on failure/end of file.
---@param ... read_format
---@return any value
---@return any ...
---@nodiscard
function file:read(...) end

---Moves and reports the file position, measured in bytes from `whence` (default "cur") plus `offset` (default 0).
---@param whence? seek_whence
---@param offset? integer
---@return integer? position
---@return string? err
function file:seek(whence, offset) end

---Chooses the buffering mode of the file and, optionally, the buffer size in bytes.
---@param mode vbuf_mode
---@param size? integer
---@return boolean? ok
---@return string? err
function file:setvbuf(mode, size) end

---Writes each string or number argument to the file and returns the file for chaining.
---@param ... string|number
---@return file? self
---@return string? err
function file:write(...) end

---@class iolib
io = {}

---Standard input stream.
---@type file
io.stdin = {}

---Standard output stream.
---@type file
io.stdout = {}

---Standard error stream.
---@type file
io.stderr = {}

---Closes `f`, or the default output file when omitted.
---@param f? file
---@return boolean? ok
---@return string? exitcode
---@return integer? code
function io.close(f) end

---Flushes the default output file.
function io.flush() end

---Opens the named file (or uses the given handle) as the default input; with no argument returns the current default input.
---@param f? string|file
---@return file input
function io.input(f) end

---Opens the file in read mode and returns a line iterator that closes the file when it finishes. Without a name it iterates the default input.
---@param filename? string
---@param ... read_format
---@return fun(): any, ...any iterator
function io.lines(filename, ...) end

---Opens a file in the given mode (default "r"). Returns the handle, or nil plus an error message and code.
---@param filename string
---@param mode? open_mode
---@return file? handle
---@return string? err
---@return integer? code
---@nodiscard
function io.open(filename, mode) end

---Opens the named file (or uses the given handle) as the default output; with no argument returns the current default output.
---@param f? string|file
---@return file output
function io.output(f) end

---Starts `prog` in a separate process and returns a handle to read from ("r", default) or write to ("w") it. Not available on every platform.
---@param prog string
---@param mode? popen_mode
---@return file? handle
---@return string? err
function io.popen(prog, mode) end

---Reads from the default input file; see `file:read`.
---@param ... read_format
---@return any value
---@return any ...
---@nodiscard
function io.read(...) end

---Returns a handle to a temporary file opened in update mode that is removed when the program ends.
---@return file handle
---@nodiscard
function io.tmpfile() end

---Returns "file" for an open handle, "closed file" for a closed one and nil for anything else.
---@param obj any
---@return "file"|"closed file"|nil kind
---@nodiscard
function io.type(obj) end

---Writes to the default output file; see `file:write`.
---@param ... string|number
---@return file? output
---@return string? err
function io.write(...) end

---@class mathlib
math = {}

---The value of pi.
---@type number
math.pi = 3.141592653589793

---Positive infinity; larger than any other number.
---@type number
math.huge = 1e309

---Largest representable integer.
---@type integer
math.maxinteger = 0x7fffffffffffffff

---Smallest representable integer.
---@type integer
math.mininteger = 0x8000000000000000

---Returns the absolute value of `x`.
---@generic N: number
---@param x N
---@return N result
---@nodiscard
function math.abs(x) end

---Returns the arc cosine of `x` in radians.
---@param x number
---@return number result
---@nodiscard
function math.acos(x) end

---Returns the arc sine of `x` in radians.
---@param x number
---@return number result
---@nodiscard
function math.asin(x) end

---Returns the arc tangent of `y / x` in radians, using both signs to pick the quadrant. `x` defaults to 1.
---@param y number
---@param x? number
---@return number result
---@nodiscard
function math.atan(y, x) end

---Returns the smallest integral value greater than or equal to `x`.
---@param x number
---@return integer result
---@nodiscard
function math.ceil(x) end

---Returns the cosine of `x` (radians).
---@param x number
---@return number result
---@nodiscard
function math.cos(x) end

---Converts an angle from radians to degrees.
---@param x number
---@return number result
---@nodiscard
function math.deg(x) end

---Returns e raised to the power `x`.
---@param x number
---@return number result
---@nodiscard
function math.exp(x) end

---Returns the largest integral value less than or equal to `x`.
---@param x number
---@return integer result
---@nodiscard
function math.floor(x) end

---Returns the remainder of `x / y` with the quotient rounded towards zero.
---@param x number
---@param y number
---@return number result
---@nodiscard
function math.fmod(x, y) end

---Returns the logarithm of `x` in the given base (natural logarithm by default).
---@param x number
---@param base? number
---@return number result
---@nodiscard
function math.log(x, base) end

---Returns the largest of its arguments.
---@generic N: number
---@param x N
---@param ... N
---@return N result
---@nodiscard
function math.max(x, ...) end

---Returns the smallest of its arguments.
---@generic N: number
---@param x N
---@param ... N
---@return N result
---@nodiscard
function math.min(x, ...) end

---Splits `x` into its integral part and its fractional part.
---@param x number
---@return number integral
---@return number fractional
---@nodiscard
function math.modf(x) end

---Converts an angle from degrees to radians.
---@param x number
---@return number result
---@nodiscard
function math.rad(x) end

---With no arguments returns a float in [0, 1); with `m` an integer in [1, m]; with `m` and `n` an integer in [m, n]. `math.random(0)` yields a fully random integer.
---@param m integer
---@param n integer
---@return integer result
---@overload fun(): number
---@overload fun(m: integer): integer
---@nodiscard
function math.random(m, n) end

---Seeds the pseudo-random generator. With no arguments a reasonably random seed is chosen.
---@param x? integer
---@param y? integer
function math.randomseed(x, y) end

---Returns the sine of `x` (radians).
---@param x number
---@return number result
---@nodiscard
function math.sin(x) end

---Returns the square root of `x`.
---@param x number
---@return number result
---@nodiscard
function math.sqrt(x) end

---Returns the tangent of `x` (radians).
---@param x number
---@return number result
---@nodiscard
function math.tan(x) end

---Returns `x` as an integer when it has an exact integer representation, otherwise nil.
---@param x any
---@return integer? result
---@nodiscard
function math.tointeger(x) end

---Returns "integer", "float", or nil when `x` is not a number.
---@param x any
---@return "integer"|"float"|nil kind
---@nodiscard
function math.type(x) end

---Compares two integers as if they were unsigned and returns true when `m` is below `n`.
---@param m integer
---@param n integer
---@return boolean result
---@nodiscard
function math.ult(m, n) end

---@class osdate
---@field year integer
---@field month integer
---@field day integer
---@field hour? integer
---@field min? integer
---@field sec? integer
---@field wday? integer
---@field yday? integer
---@field isdst? boolean
local osdate = {}

---@class oslib
os = {}

---Returns an approximation of the CPU time used by the program, in seconds.
---@return number seconds
---@nodiscard
function os.clock() end

---Formats a time (default: now) using strftime-style `format`. A leading "!" selects UTC and the format "*t" returns a table of date fields.
---@param format? string
---@param time? integer
---@return string|osdate result
---@nodiscard
function os.date(format, time) end

---Returns the number of seconds from time `t1` to time `t2`.
---@param t2 integer
---@param t1 integer
---@return number seconds
---@nodiscard
function os.difftime(t2, t1) end

---Runs `command` through the system shell and returns success, "exit" or "signal", and the status code. Without a command it reports whether a shell is available.
---@param command? string
---@return boolean? ok
---@return string? exitcode
---@return integer? code
function os.execute(command) end

---Terminates the host program with the given status (default success). When `close` is true the Lua state is closed first.
---@param code? boolean|integer
---@param close? boolean
function os.exit(code, close) end

---Returns the value of an environment variable, or nil when it is not defined.
---@param varname string
---@return string? value
---@nodiscard
function os.getenv(varname) end

---Deletes a file or an empty directory. Returns true, or nil plus an error message and code.
---@param filename string
---@return boolean? ok
---@return string? err
---@return integer? code
function os.remove(filename) end

---Renames or moves a file or directory. Returns true, or nil plus an error message and code.
---@param oldname string
---@param newname string
---@return boolean? ok
---@return string? err
---@return integer? code
function os.rename(oldname, newname) end

---Sets the program locale for the given category, or queries it when `locale` is nil. Returns the locale name, or nil on failure.
---@param locale? string
---@param category? locale_category
---@return string? name
function os.setlocale(locale, category) end

---Returns the current time as a timestamp, or the timestamp described by the given date table.
---@param date? osdate
---@return integer timestamp
---@nodiscard
function os.time(date) end

---Returns a file name that can be used for a temporary file.
---@return string name
---@nodiscard
function os.tmpname() end

---@class packagelib
package = {}

---Compile-time path configuration: directory separator, template separator, substitution mark, executable-dir mark and ignore mark, one per line.
---@type string
package.config = ""

---Search path used by `require` to find C loaders.
---@type string
package.cpath = ""

---Cache of modules already loaded by `require`, keyed by module name.
---@type table<string, any>
package.loaded = {}

---Search path used by `require` to find Lua loaders.
---@type string
package.path = ""

---Loaders for specific modules, consulted by `require` before searching the paths.
---@type table<string, function>
package.preload = {}

---Ordered list of searcher functions that `require` uses to locate a module loader.
---@type function[]
package.searchers = {}

---Dynamically links the C library `libname` and returns `funcname` from it as a function; with "*" it only links the library.
---@param libname string
---@param funcname string
---@return function? fn
---@return string? err
---@return string? where
function package.loadlib(libname, funcname) end

---Searches `path` for `name`, replacing each `sep` (default ".") in the name with `rep` (default: directory separator). Returns the first readable file name, or nil plus a message.
---@param name string
---@param path string
---@param sep? string
---@param rep? string
---@return string? filename
---@return string? err
---@nodiscard
function package.searchpath(name, path, sep, rep) end

---@class stringlib
string = {}

---Returns the numeric codes of the bytes `s[i]` through `s[j]`; both default to `i` = 1.
---@param s string
---@param i? integer
---@param j? integer
---@return integer ...
---@nodiscard
function string.byte(s, i, j) end

---Builds a string from the given byte values.
---@param ... integer
---@return string result
---@nodiscard
function string.char(...) end

---Returns a binary representation of a Lua function that `load` can turn back into a function. `strip` drops debug information.
---@param f function
---@param strip? boolean
---@return string bytecode
---@nodiscard
function string.dump(f, strip) end

---Finds the first match of `pattern` in `s` starting at `init`. Returns start and end indices plus any captures, or nil. `plain` disables pattern matching.
---@param s string
---@param pattern string
---@param init? integer
---@param plain? boolean
---@return integer? start
---@return integer? finish
---@return any ...
---@nodiscard
function string.find(s, pattern, init, plain) end

---Builds a string from a printf-style format and its arguments. Supports `%q` for Lua-readable quoting and `%s` via `tostring`.
---@param s string
---@param ... any
---@return string result
---@nodiscard
function string.format(s, ...) end

---Returns an iterator that yields the captures (or whole match) of each successive match of `pattern` in `s`, starting at `init`.
---@param s string
---@param pattern string
---@param init? integer
---@return fun(): string, ...string iterator
---@nodiscard
function string.gmatch(s, pattern, init) end

---Returns a copy of `s` where matches of `pattern` (at most `n`) are replaced by `repl`, which may be a string, a lookup table or a function. Also returns the number of matches.
---@param s string
---@param pattern string
---@param repl string|number|table|fun(...: string): any
---@param n? integer
---@return string result
---@return integer count
---@nodiscard
function string.gsub(s, pattern, repl, n) end

---Returns the length of `s` in bytes.
---@param s string
---@return integer length
---@nodiscard
function string.len(s) end

---Returns a copy of `s` with uppercase letters converted to lowercase.
---@param s string
---@return string result
---@nodiscard
function string.lower(s) end

---Returns the captures of the first match of `pattern` in `s` (or the whole match when there are none), or nil.
---@param s string
---@param pattern string
---@param init? integer
---@return any ...
---@nodiscard
function string.match(s, pattern, init) end

---Serializes the given values into a binary string according to the format string `fmt`.
---@param fmt string
---@param v1 string|number
---@param ... string|number
---@return string binary
---@nodiscard
function string.pack(fmt, v1, ...) end

---Returns the size of the string `string.pack` would produce for `fmt`, which must not contain variable-length options.
---@param fmt string
---@return integer size
---@nodiscard
function string.packsize(fmt) end

---Returns `n` copies of `s` joined by `sep`.
---@param s string
---@param n integer
---@param sep? string
---@return string result
---@nodiscard
function string.rep(s, n, sep) end

---Returns `s` with its bytes in reverse order.
---@param s string
---@return string result
---@nodiscard
function string.reverse(s) end

---Returns the substring from `i` to `j` (default -1, the end). Negative indices count from the end.
---@param s string
---@param i integer
---@param j? integer
---@return string result
---@nodiscard
function string.sub(s, i, j) end

---Reads the values packed in `s` according to `fmt`, starting at `pos`. Returns them followed by the index of the first unread byte.
---@param fmt string
---@param s string
---@param pos? integer
---@return any ...
---@nodiscard
function string.unpack(fmt, s, pos) end

---Returns a copy of `s` with lowercase letters converted to uppercase.
---@param s string
---@return string result
---@nodiscard
function string.upper(s) end

---@class packedtable
---@field n integer
---@field [integer] any
local packedtable = {}

---@class tablelib
table = {}

---Joins the string/number elements `list[i]` through `list[j]` with `sep` between them.
---@param list (string|number)[]
---@param sep? string
---@param i? integer
---@param j? integer
---@return string result
---@nodiscard
function table.concat(list, sep, i, j) end

---Inserts `value` at position `pos`, shifting later elements up. With two arguments the value is appended at the end.
---@generic T
---@param list T[]
---@param pos integer
---@param value T
---@overload fun(list: table, value: any)
function table.insert(list, pos, value) end

---Copies elements `a1[f]` through `a1[e]` into `a2` (default `a1`) starting at index `t`, and returns the destination table.
---@generic T
---@param a1 T[]
---@param f integer
---@param e integer
---@param t integer
---@param a2? T[]
---@return T[] destination
function table.move(a1, f, e, t, a2) end

---Returns a new table holding all arguments at indices 1..n, with the count stored in field `n`.
---@param ... any
---@return packedtable result
---@nodiscard
function table.pack(...) end

---Removes and returns the element at `pos` (default: the last one), shifting later elements down.
---@generic T
---@param list T[]
---@param pos? integer
---@return T? removed
function table.remove(list, pos) end

---Sorts the list in place. `comp(a, b)` must return true when `a` has to come before `b`; defaults to `<`. The sort is not stable.
---@generic T
---@param list T[]
---@param comp? fun(a: T, b: T): boolean
function table.sort(list, comp) end

---Returns the elements `list[i]` through `list[j]` as multiple values; defaults are 1 and `#list`.
---@generic T
---@param list T[]
---@param i? integer
---@param j? integer
---@return T ...
---@nodiscard
function table.unpack(list, i, j) end

---@class utf8lib
utf8 = {}

---Pattern that matches exactly one UTF-8 byte sequence, assuming the subject is valid UTF-8.
---@type string
utf8.charpattern = "[\0-\x7F\xC2-\xFD][\x80-\xBF]*"

---Encodes each integer code point as UTF-8 and returns the concatenation.
---@param ... integer
---@return string result
---@nodiscard
function utf8.char(...) end

---Returns an iterator over `s` yielding the byte position and code point of each character. `lax` lifts the validity checks.
---@param s string
---@param lax? boolean
---@return fun(s: string, p: integer): integer, integer iterator
---@return string s
---@return integer p
function utf8.codes(s, lax) end

---Returns the code points of all characters that start between byte positions `i` and `j` (both default to `i` = 1).
---@param s string
---@param i? integer
---@param j? integer
---@param lax? boolean
---@return integer ...
---@nodiscard
function utf8.codepoint(s, i, j, lax) end

---Returns the number of UTF-8 characters that start between byte positions `i` and `j`, or nil plus the position of the first invalid byte.
---@param s string
---@param i? integer
---@param j? integer
---@param lax? boolean
---@return integer? count
---@return integer? errpos
---@nodiscard
function utf8.len(s, i, j, lax) end

---Returns the byte position where the `n`-th character (counting from byte position `i`) starts, or nil if there is no such character.
---@param s string
---@param n integer
---@param i? integer
---@return integer? position
---@nodiscard
function utf8.offset(s, n, i) end
