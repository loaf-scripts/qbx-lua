---@meta

---@class CitizenLib
Citizen = {}

---Suspends the current scheduler thread for at least `msec` milliseconds; 0 resumes on the next tick. Only valid inside a thread created by the scheduler.
---@param msec integer
function Citizen.Wait(msec) end

---Queues `handler` as a new scheduler thread (coroutine) that starts on the next tick, and returns an id that `Citizen.ClearTimeout` cancels it with before it starts.
---@param handler fun()
---@return integer threadId
function Citizen.CreateThread(handler) end

---Creates a scheduler thread and runs it immediately up to its first yield, instead of waiting for the next tick. Returns whether the thread yielded, so that it is still running. `name` names the thread in profiler and error output.
---@param handler fun()
---@param name? string
---@return boolean yielded
function Citizen.CreateThreadNow(handler, name) end

---Runs `callback` once in a new thread after `msec` milliseconds and returns a timer id usable with `Citizen.ClearTimeout`.
---@param msec integer
---@param callback fun()
---@return integer timerId
function Citizen.SetTimeout(msec, callback) end

---Cancels a pending timer created by `Citizen.SetTimeout`, or a thread created by `Citizen.CreateThread` that has not started yet. Returns whether one was cancelled.
---@param timerId integer
---@return boolean removed
function Citizen.ClearTimeout(timerId) end

---Yields the current thread until the promise settles. Returns the resolved value, or raises the rejection value as an error.
---@param p promise
---@return any ...
function Citizen.Await(p) end

---Writes raw text to the console/log without appending a newline. A number is written as text.
---@param text string|number
function Citizen.Trace(text) end

---Calls a game native by hash. Arguments may include the pointer and result marker values produced by the other `Citizen.*` helpers.
---@param hash integer|string
---@param ... any
---@return any ...
function Citizen.InvokeNative(hash, ...) end

---Calls a game native by hash like `Citizen.InvokeNative`, with the same arguments and marker values.
---@param hash integer|string
---@param ... any
---@return any ...
function Citizen.InvokeNative2(hash, ...) end

---Returns a directly callable function for the native with the given hash, or nil when it cannot be resolved.
---@param hash integer|string
---@return function? native
---@nodiscard
function Citizen.GetNative(hash) end

---Returns the Lua source of the lazily loaded wrapper for the named native, or nil when the native is unknown.
---@param name string
---@return string? source
---@nodiscard
function Citizen.LoadNative(name) end

---Marker for an integer out-parameter when calling `Citizen.InvokeNative`.
---@return any marker
---@nodiscard
function Citizen.PointerValueInt() end

---Marker for a float out-parameter when calling `Citizen.InvokeNative`.
---@return any marker
---@nodiscard
function Citizen.PointerValueFloat() end

---Marker for a vector3 out-parameter when calling `Citizen.InvokeNative`.
---@return any marker
---@nodiscard
function Citizen.PointerValueVector() end

---Marker for an integer in/out parameter that is initialised with `value` before the native runs.
---@param value integer
---@return any marker
---@nodiscard
function Citizen.PointerValueIntInitialized(value) end

---Marker for a float in/out parameter that is initialised with `value` before the native runs.
---@param value number
---@return any marker
---@nodiscard
function Citizen.PointerValueFloatInitialized(value) end

---Marker telling `Citizen.InvokeNative` to return the native's result even though out-parameters are present.
---@return any marker
---@nodiscard
function Citizen.ReturnResultAnyway() end

---Marker requesting the native result as a 32-bit integer.
---@return any marker
---@nodiscard
function Citizen.ResultAsInteger() end

---Marker requesting the native result as a 64-bit integer.
---@return any marker
---@nodiscard
function Citizen.ResultAsLong() end

---Marker requesting the native result as a float.
---@return any marker
---@nodiscard
function Citizen.ResultAsFloat() end

---Marker requesting the native result as a string.
---@return any marker
---@nodiscard
function Citizen.ResultAsString() end

---Marker requesting the native result as a vector3.
---@return any marker
---@nodiscard
function Citizen.ResultAsVector() end

---Marker requesting the native result as a msgpack-serialised object that is decoded into a Lua value.
---@return any marker
---@nodiscard
function Citizen.ResultAsObject() end

---Marker requesting the native result as a msgpack-serialised object that `unpacker`, usually `msgpack.unpack`, decodes.
---@param unpacker fun(data: string): any
---@return any marker
---@nodiscard
function Citizen.ResultAsObject2(unpacker) end

---Runtime internal: marks the start of a scheduler boundary used to stitch stack traces across resources.
---@param boundaryId integer
---@param co? thread
function Citizen.SubmitBoundaryStart(boundaryId, co) end

---Runtime internal: marks the end of the current scheduler boundary.
---@param co? thread
function Citizen.SubmitBoundaryEnd(co) end

---Runtime internal: installs the function the host calls every tick. Used by the scheduler; resources should not call it.
---@param routine fun()
function Citizen.SetTickRoutine(routine) end

---Runtime internal: installs the function the host calls to deliver events. Used by the scheduler; resources should not call it.
---@param routine fun(eventName: string, eventPayload: string, eventSource: string)
function Citizen.SetEventRoutine(routine) end

---Alias of `Citizen.Wait`.
---@param msec integer
function Wait(msec) end

---Alias of `Citizen.CreateThread`.
---@param handler fun()
---@return integer threadId
function CreateThread(handler) end

---Alias of `Citizen.SetTimeout`.
---@param msec integer
---@param callback fun()
---@return integer timerId
function SetTimeout(msec, callback) end

---Alias of `Citizen.ClearTimeout`.
---@param timerId integer
---@return boolean removed
function ClearTimeout(timerId) end

---Handle returned by `AddEventHandler`; pass it to `RemoveEventHandler` to unregister the handler.
---@class EventHandlerData
---@field key integer
---@field name string
local EventHandlerData = {}

---The name fivem-lls-addon gives the handle of `AddEventHandler`.
---@alias EventHandler EventHandlerData

---While a network event handler runs: the server id of the player that triggered it (server side), or 65535 for server-sent events (client side).
---@type integer
source = 0

---Registers `handler` for the named event and returns a handle for `RemoveEventHandler`. Network events additionally need `RegisterNetEvent`.
---@param eventName string
---@param handler fun(...: any)
---@return EventHandlerData eventData
function AddEventHandler(eventName, handler) end

---Unregisters a handler previously added with `AddEventHandler` or `RegisterNetEvent`.
---@param eventData EventHandlerData
function RemoveEventHandler(eventData) end

---Marks the event as safe to receive over the network. When `handler` is given it is also registered and its handle returned.
---@param eventName string
---@param handler? fun(...: any)
---@return EventHandlerData? eventData
---@overload fun(eventName: string)
function RegisterNetEvent(eventName, handler) end

---Old name of `RegisterNetEvent`.
---@deprecated
---@param eventName string
---@param handler? fun(...: any)
---@return EventHandlerData? eventData
function RegisterServerEvent(eventName, handler) end

---Triggers a local event on this side (client or server), invoking every handler in every resource with the given arguments.
---@param eventName string
---@param ... any
function TriggerEvent(eventName, ...) end

---Cross-resource export registry. Call it as `exports('name', fn)` to publish a function, or index it by resource name to call one: `exports.resource:fn(...)` / `exports['resource']:fn(...)`.
---@class CitizenExports
---@overload fun(name: string, fn: function)
---@field [string] table<string, function>
exports = {}

---The options of `json.getoption` and `json.setoption`, which `json.encode` also takes in its state table.
---@alias json_options "indent"|"pretty"|"sort_keys"|"null"|"nesting"|"unsigned"|"nan"|"inf"|"bit32"|"lua_format_float"|"lua_round_float"|"vectorarray"|"single_line"|"empty_table_as_array"|"with_hole"|"decoder_preset"|"max_depth"|"indent_char"|"indent_count"|"level"|"decimal_count"

---Options for one `json.encode` call.
---@class json_encode_state
---@field indent? boolean Write newlines and indentation.
---@field pretty? boolean Same as `indent`.
---@field sort_keys? boolean Write the keys of objects in sorted order.
---@field empty_table_as_array? boolean Encode an empty table as `[]`.
---@field with_hole? boolean Encode a table whose keys are all positive integers as an array, with `null` for its gaps.
---@field single_line? boolean Keep arrays on one line when indenting.
---@field max_depth? integer Deepest table nesting to encode.
---@field indent_char? integer
---@field indent_count? integer Indent characters per level.
---@field level? integer Same as `indent_count`.
---@field decimal_count? integer Most decimal places written for a number.
---@field nan? boolean Write `NaN` and `Infinity`.
---@field inf? boolean Same as `nan`.
---@field null? boolean
---@field nesting? boolean
---@field unsigned? boolean
---@field bit32? boolean
---@field indent_amt? integer
---@field keyorder? table
---@field exception? fun(reason: string, value: any): string?, string?

---@class jsonlib
json = {}

---Sentinel value that represents JSON `null` inside Lua tables.
---@type any
json.null = {}

---Serialises a Lua value to a JSON string. The optional state table tweaks output (for example `indent = true`).
---@param value any
---@param state? json_encode_state
---@return string encoded
---@nodiscard
function json.encode(value, state) end

---Parses a JSON string starting at `pos`. Returns the value, or nil with the failing position and an error message. `nullval` replaces JSON null, and `objectmeta` and `arraymeta` become the metatables of the decoded objects and arrays.
---@param str string
---@param pos? integer
---@param nullval? any
---@param objectmeta? table
---@param arraymeta? table
---@return any value
---@return integer? nextPos
---@return string? err
---@nodiscard
function json.decode(str, pos, nullval, objectmeta, arraymeta) end

---Returns the value of a global encoding/decoding option.
---@param option json_options
---@return any value
---@nodiscard
function json.getoption(option) end

---Sets a global encoding/decoding option.
---@param option json_options
---@param value any
function json.setoption(option, value) end

---Marks `t`, or a new table, to be encoded as a JSON object and returns it.
---@param t? table
---@return table
function json.object(t) end

---Marks `t`, or a new table, to be encoded as a JSON array and returns it.
---@param t? table
---@return table
function json.array(t) end

---Whether the table is marked as a JSON object.
---@param value any
---@return boolean
---@nodiscard
function json.isobject(value) end

---Whether the table is marked as a JSON array.
---@param value any
---@return boolean
---@nodiscard
function json.isarray(value) end

---The options of `msgpack.getoption` and `msgpack.setoption`.
---@alias msgpack_options "unsigned"|"integer"|"float"|"double"|"string_compat"|"string_binary"|"empty_table_as_array"|"without_hole"|"with_hole"|"always_as_map"|"small_lua"|"full64bits"|"long_double"|"sentinel"|"ignore_invalid"

---@class msgpacklib
msgpack = {}

---Sentinel value that stands for MessagePack `nil` inside Lua tables.
---@type any
msgpack.null = {}

---Same value as `msgpack.null`.
---@type any
msgpack.sentinel = msgpack.null

---Serialises the given values into one MessagePack byte string.
---@param ... any
---@return string packed
---@nodiscard
function msgpack.pack(...) end

---Serialises the arguments as a single MessagePack array, the wire format used for event and export arguments.
---@param ... any
---@return string packed
---@nodiscard
function msgpack.pack_args(...) end

---Decodes a MessagePack byte string and returns every value it contains, or up to `limit` values from `position` on, as far as `endPosition`.
---@param data string
---@param position? integer
---@param limit? integer
---@param endPosition? integer
---@return any ...
---@nodiscard
function msgpack.unpack(data, position, limit, endPosition) end

---Decodes up to `limit` values starting at `position` and returns where decoding stopped, 0 at the end of the string, followed by the values.
---@param data string
---@param position? integer
---@param limit? integer
---@param endPosition? integer
---@return integer position
---@return any ...
---@nodiscard
function msgpack.next(data, position, limit, endPosition) end

---Creates a packer object that serialises the values passed to it.
---@return userdata packer
---@nodiscard
function msgpack.new() end

---Returns the value of a global encoding/decoding option.
---@param option msgpack_options
---@return any value
---@nodiscard
function msgpack.getoption(option) end

---Sets a global encoding/decoding option.
---@param option msgpack_options
---@param value any
function msgpack.setoption(option, value) end

---Sets one of the string options: "string", "string_compat" or "string_binary".
---@param value string
function msgpack.set_string(value) end

---Sets one of the array options: "without_hole", "with_hole" or "always_as_map".
---@param value string
function msgpack.set_array(value) end

---Sets one of the integer options: "signed" or "unsigned".
---@param value string
function msgpack.set_integer(value) end

---Sets one of the number options: "float" or "double".
---@param value string
function msgpack.set_number(value) end

---Registers an extension type from a table with its type id and its pack and unpack functions.
---@param encoder table
function msgpack.extend(encoder) end

---Returns the definition of the extension type with the given id.
---@param extId integer
---@return table? encoder
---@nodiscard
function msgpack.extend_get(extId) end

---Removes the extension types with the given ids.
---@param ... integer
function msgpack.extend_clear(...) end

---Associates the name of a Lua type with an extension type.
---@param typeName string
---@param extId? integer
function msgpack.settype(typeName, extId) end

---Returns the extension definition associated with the name of a Lua type.
---@param typeName string
---@return table? encoder
---@nodiscard
function msgpack.gettype(typeName) end

---Deferred/promise object. `state` is 0 pending, 1 resolving, 2 rejecting, 3 resolved, 4 rejected; `value` holds the settled value.
---@class promise
---@field state integer
---@field value any
---@field queue promise[] The promises chained on this one with `next`.
---@field success? fun(value: any)
---@field failure? fun(value: any)
promise = {}

---Creates a new pending promise.
---@return promise p
---@nodiscard
function promise.new() end

---Returns a promise that resolves with a list of all results once every given promise has resolved, or rejects as soon as one rejects.
---@param promises promise[]
---@return promise p
---@nodiscard
function promise.all(promises) end

---Returns a promise that settles the same way as the first of the given promises to settle.
---@param promises promise[]
---@return promise p
---@nodiscard
function promise.first(promises) end

---Runs `fn` over each item sequentially, where `fn` returns a promise, and resolves with the list of results.
---@param items any[]
---@param fn fun(item: any): promise
---@return promise p
---@nodiscard
function promise.map(items, fn) end

---Fulfils the promise with `value` and schedules its continuations.
---@param value? any
---@return promise self
function promise:resolve(value) end

---Rejects the promise with `err` and schedules its rejection handlers.
---@param err? any
---@return promise self
function promise:reject(err) end

---Attaches continuations and returns a new promise chained on their result.
---@param onFulfilled? fun(value: any): any
---@param onRejected? fun(err: any): any
---@return promise chained
function promise:next(onFulfilled, onRejected) end

---Two-component float vector value type.
---@class vector2
---@field x number
---@field y number
---@field r number Same as `x`.
---@field g number Same as `y`.
---@field xy vector2
---@field n integer Number of components.
---@operator add(vector2): vector2
---@operator sub(vector2): vector2
---@operator mul(number): vector2
---@operator mul(vector2): vector2
---@operator div(number): vector2
---@operator div(vector2): vector2
---@operator unm: vector2
---@operator len: number

---Three-component float vector value type. `#v` gives its length (magnitude).
---@class vector3
---@field x number
---@field y number
---@field z number
---@field r number Same as `x`.
---@field g number Same as `y`.
---@field b number Same as `z`.
---@field xy vector2
---@field xyz vector3
---@field n integer Number of components.
---@operator add(vector3): vector3
---@operator sub(vector3): vector3
---@operator mul(number): vector3
---@operator mul(vector3): vector3
---@operator div(number): vector3
---@operator div(vector3): vector3
---@operator unm: vector3
---@operator len: number

---Four-component float vector value type.
---@class vector4
---@field x number
---@field y number
---@field z number
---@field w number
---@field r number Same as `x`.
---@field g number Same as `y`.
---@field b number Same as `z`.
---@field a number Same as `w`.
---@field xy vector2
---@field xyz vector3
---@field xyzw vector4
---@field n integer Number of components.
---@operator add(vector4): vector4
---@operator sub(vector4): vector4
---@operator mul(number): vector4
---@operator mul(vector4): vector4
---@operator div(number): vector4
---@operator div(vector4): vector4
---@operator unm: vector4
---@operator len: number

---A vector of any size.
---@alias vector vector2|vector3|vector4

---A string that holds an SQL query. Editors highlight the query in calls of a function whose first parameter has this type.
---@alias sql string

---Quaternion value type. Multiplying by a vector3 rotates that vector.
---@class quat
---@field x number
---@field y number
---@field z number
---@field w number
---@operator mul(quat): quat
---@operator mul(vector3): vector3
---@operator mul(number): quat
---@operator unm: quat
---@operator len: number

---Creates a vector2. Its components can also come from vectors, as in `vector2(v3)`, from a
---table, or from one number for all of them.
---@param x number
---@param y number
---@return vector2 v
---@overload fun(...: number|vector2|vector3|vector4|table): vector2
---@nodiscard
function vector2(x, y) end

---Creates a vector3. Its components can also come from vectors, as in `vector3(xy, z)`, from a
---table, or from one number for all of them.
---@param x number
---@param y number
---@param z number
---@return vector3 v
---@overload fun(...: number|vector2|vector3|vector4|table): vector3
---@nodiscard
function vector3(x, y, z) end

---Creates a vector4. Its components can also come from vectors, as in `vector4(coords, heading)`, from a
---table, or from one number for all of them.
---@param x number
---@param y number
---@param z number
---@param w number
---@return vector4 v
---@overload fun(...: number|vector2|vector3|vector4|table): vector4
---@nodiscard
function vector4(x, y, z, w) end

---Creates a quaternion. Note that the scalar part `w` comes first. It also takes an angle in degrees
---and the axis to rotate around, two directions to rotate the first into the second, or Euler
---angles in radians.
---@param w number
---@param x number
---@param y number
---@param z number
---@return quat q
---@overload fun(angle: number, axis: vector3): quat
---@overload fun(from: vector3, to: vector3): quat
---@overload fun(euler: vector3): quat
---@nodiscard
function quat(w, x, y, z) end

---Short alias of `vector2`.
---@param x number
---@param y number
---@return vector2 v
---@overload fun(...: number|vector2|vector3|vector4|table): vector2
---@nodiscard
function vec2(x, y) end

---Short alias of `vector3`.
---@param x number
---@param y number
---@param z number
---@return vector3 v
---@overload fun(...: number|vector2|vector3|vector4|table): vector3
---@nodiscard
function vec3(x, y, z) end

---Short alias of `vector4`.
---@param x number
---@param y number
---@param z number
---@param w number
---@return vector4 v
---@overload fun(...: number|vector2|vector3|vector4|table): vector4
---@nodiscard
function vec4(x, y, z, w) end

---Creates a vector whose dimension matches the number of components given (2, 3 or 4).
---@param ... number
---@return vector2|vector3|vector4 v
---@overload fun(x: number, y: number): vector2
---@overload fun(x: number, y: number, z: number): vector3
---@overload fun(x: number, y: number, z: number, w: number): vector4
---@nodiscard
function vec(...) end

---Returns the normalised (unit length) copy of a vector or quaternion.
---@generic T: vector2|vector3|vector4|quat
---@param v T
---@return T normalized
---@nodiscard
function norm(v) end

---Computes the Jenkins one-at-a-time hash of `str`, the same value game natives use for model and asset names.
---An integer is returned as it is.
---@param str string|integer
---@return integer hash
---@nodiscard
function joaat(str) end

---Key/value store synchronised between server and clients. Read and write keys as plain fields; use `set` to control replication explicitly.
---@class StateBag
---@field [string] any
local StateBag = {}

---Writes `key`. When `replicated` is true the change is sent across the network (a client sends to the server, the server sends to clients).
---@param key string
---@param value any
---@param replicated? boolean
function StateBag:set(key, value, replicated) end

---State bag shared by the whole server. Writable on the server, read-only on clients.
---@type StateBag
GlobalState = {}

---@class EntityInterface
---@field state StateBag
---@field __data integer The entity handle the wrapper was created for.
local EntityInterface = {}

---@class PlayerInterface
---@field state StateBag
local PlayerInterface = {}

---Wraps an entity handle in an object whose `state` field is that entity's state bag.
---@param entity integer
---@return EntityInterface wrapper
---@nodiscard
function Entity(entity) end

---Wraps a player server id in an object whose `state` field is that player's state bag.
---@param playerSrc integer|string
---@return PlayerInterface wrapper
---@nodiscard
function Player(playerSrc) end

---Removes leading and trailing characters from a string. CfxLua extension.
---@param s string
---@param chars? string Characters to strip; defaults to whitespace.
---@return string
function string.strtrim(s, chars) end

---Splits a string on any of the delimiter characters and returns the pieces. CfxLua extension.
---@param delimiter string
---@param s string
---@param pieces? integer Maximum number of pieces to return.
---@return string ...
function string.strsplit(delimiter, s, pieces) end

---Joins the arguments into one string separated by the delimiter. CfxLua extension.
---@param delimiter string
---@param ... string|number
---@return string
function string.strjoin(delimiter, ...) end

---Concatenates the string form of every argument. CfxLua extension.
---@param ... any
---@return string
function string.strconcat(...) end

---Creates a table with preallocated array and hash parts. CfxLua extension.
---@param narr integer
---@param nrec integer
---@return table
function table.create(narr, nrec) end

---Same as `table.create`. CfxLua extension.
---@param narr integer
---@param nrec integer
---@return table
function table.new(narr, nrec) end

---Removes every key from the table and returns it. CfxLua extension.
---@generic T: table
---@param t T
---@return T
function table.wipe(t) end

---Same as `table.wipe`. CfxLua extension.
---@generic T: table
---@param t T
---@return T
function table.clear(t) end

---Describes the table layout. CfxLua extension.
---@param t table
---@return 'empty'|'array'|'hash'|'mixed'
function table.type(t) end

---Returns a shallow copy of the table, written into `destination` when one is given. CfxLua extension.
---@generic T: table
---@param t T
---@param destination? table
---@return T
function table.clone(t, destination) end

---Converts every argument to a string and returns them all. CfxLua extension.
---@param ... any
---@return string ...
function string.tostringall(...) end

---Creates a mutable string buffer of the given length. CfxLua extension.
---@param length integer
---@return string
function string.blob(length) end

---The difference between 1 and the next larger number. CfxLua extension.
---@type number
math.eps = 2.220446049250313e-16

---The difference between 1 and the next larger single-precision float. CfxLua extension.
---@type number
math.feps = 1.1920928955078125e-07

---Returns the inverse hyperbolic cosine of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.acosh(x) end

---Returns the inverse hyperbolic sine of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.asinh(x) end

---Returns the arc tangent of `y / x` in radians, using both signs to pick the quadrant. `x` defaults to 1. CfxLua extension.
---@param y number
---@param x? number
---@return number result
---@nodiscard
function math.atan2(y, x) end

---Returns the inverse hyperbolic tangent of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.atanh(x) end

---Returns the cube root of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.cbrt(x) end

---Returns `x` limited to the range from `min` to `max`, per component for vectors. CfxLua extension.
---@param x number
---@param min number
---@param max number
---@return number result
---@overload fun(x: vector2, min: vector2, max: vector2): vector2
---@overload fun(x: vector3, min: vector3, max: vector3): vector3
---@overload fun(x: vector4, min: vector4, max: vector4): vector4
---@nodiscard
function math.clamp(x, min, max) end

---Returns `x` with the sign of `y`. CfxLua extension.
---@param x number
---@param y number
---@return number result
---@nodiscard
function math.copysign(x, y) end

---Returns the hyperbolic cosine of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.cosh(x) end

---Returns the error function of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.erf(x) end

---Returns the complementary error function of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.erfc(x) end

---Returns 2 raised to the power `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.exp2(x) end

---Returns e raised to the power `x`, minus 1. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.expm1(x) end

---Returns `x - y` when it is positive, otherwise 0. CfxLua extension.
---@param x number
---@param y number
---@return number result
---@nodiscard
function math.fdim(x, y) end

---Splits `x` into a mantissa in [0.5, 1) and an exponent of 2. CfxLua extension.
---@param x number
---@return number mantissa
---@return integer exponent
---@nodiscard
function math.frexp(x) end

---Returns the gamma function of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.gamma(x) end

---Returns the length of the hypotenuse of a right triangle with sides `x` and `y`. CfxLua extension.
---@param x number
---@param y number
---@return number result
---@nodiscard
function math.hypot(x, y) end

---Returns whether `x` is neither infinite nor NaN. CfxLua extension.
---@param x number
---@return boolean result
---@nodiscard
function math.isfinite(x) end

---Returns whether `x` is infinite. CfxLua extension.
---@param x number
---@return boolean result
---@nodiscard
function math.isinf(x) end

---Returns whether `x` is NaN. CfxLua extension.
---@param x number
---@return boolean result
---@nodiscard
function math.isnan(x) end

---Returns whether `x` is a normal number: not zero, subnormal, infinite or NaN. CfxLua extension.
---@param x number
---@return boolean result
---@nodiscard
function math.isnormal(x) end

---Returns `m` multiplied by 2 raised to the power `e`. CfxLua extension.
---@param m number
---@param e integer
---@return number result
---@nodiscard
function math.ldexp(m, e) end

---Returns the natural logarithm of the absolute value of the gamma function of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.lgamma(x) end

---Returns the base-10 logarithm of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.log10(x) end

---Returns the natural logarithm of 1 plus `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.log1p(x) end

---Returns the exponent of `x` in base 2, as a float. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.logb(x) end

---Returns `x` rounded to an integral value, halfway cases to even, as a float. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.nearbyint(x) end

---Returns the next representable number after `x` in the direction of `y`. CfxLua extension.
---@param x number
---@param y number
---@return number result
---@nodiscard
function math.nextafter(x, y) end

---Returns `x` raised to the power `y`. CfxLua extension.
---@param x number
---@param y number
---@return number result
---@nodiscard
function math.pow(x, y) end

---Returns the remainder of `x / y` with the quotient rounded to the nearest integer. CfxLua extension.
---@param x number
---@param y number
---@return number result
---@nodiscard
function math.remainder(x, y) end

---Returns `x` rounded to an integral value, halfway cases away from zero, as a float. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.round(x) end

---Returns `x` multiplied by 2 raised to the power `n`. CfxLua extension.
---@param x number
---@param n integer
---@return number result
---@nodiscard
function math.scalbn(x, n) end

---Returns the hyperbolic sine of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.sinh(x) end

---Returns the hyperbolic tangent of `x`. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.tanh(x) end

---Returns `x` with its fractional part removed, as a float. CfxLua extension.
---@param x number
---@return number result
---@nodiscard
function math.trunc(x) end

---Builds a vector from its arguments; the result type depends on how many numbers are passed.
---@param ... number
---@return vector2|vector3|vector4|number
function vector(...) end

---Returns a to-be-closed value that runs `fn` when it goes out of scope: `local _ <close> = defer(fn)`.
---@param fn fun()
---@return table
function defer(fn) end

---Builds a one-component vector, which is a plain number.
---@param x number
---@return number
---@nodiscard
function vec1(x) end

---Same as `vec1`.
---@param x number
---@return number
---@nodiscard
function vector1(x) end

---Builds a vector from its arguments converted to integers; its size follows how many are passed.
---@param ... number
---@return vector2|vector3|vector4|integer
---@nodiscard
function ivec(...) end

---Builds a one-component integer vector, which is a plain integer.
---@param x number
---@return integer
---@nodiscard
function ivec1(x) end

---Builds a vector2 from its arguments converted to integers.
---@param x number
---@param y number
---@return vector2
---@nodiscard
function ivec2(x, y) end

---Builds a vector3 from its arguments converted to integers.
---@param x number
---@param y number
---@param z number
---@return vector3
---@nodiscard
function ivec3(x, y, z) end

---Builds a vector4 from its arguments converted to integers.
---@param x number
---@param y number
---@param z number
---@param w number
---@return vector4
---@nodiscard
function ivec4(x, y, z, w) end

---Builds a vector from its arguments converted to booleans; its size follows how many are passed.
---@param ... any
---@return vector2|vector3|vector4|boolean
---@nodiscard
function bvec(...) end

---Builds a one-component boolean vector, which is a plain boolean.
---@param x any
---@return boolean
---@nodiscard
function bvec1(x) end

---Builds a vector2 from its arguments converted to booleans.
---@param x any
---@param y any
---@return vector2
---@nodiscard
function bvec2(x, y) end

---Builds a vector3 from its arguments converted to booleans.
---@param x any
---@param y any
---@param z any
---@return vector3
---@nodiscard
function bvec3(x, y, z) end

---Builds a vector4 from its arguments converted to booleans.
---@param x any
---@param y any
---@param z any
---@param w any
---@return vector4
---@nodiscard
function bvec4(x, y, z, w) end

---Builds a quaternion; same as `quat`.
---@param w number
---@param x number
---@param y number
---@param z number
---@return quat q
---@nodiscard
function qua(w, x, y, z) end

---Builds a matrix from numbers, vectors or another matrix; its size follows the arguments.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat(...) end

---Builds a matrix of 2 columns and 2 rows.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat2x2(...) end

---Same as `mat2x2`.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat2(...) end

---Builds a matrix of 2 columns and 3 rows.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat2x3(...) end

---Builds a matrix of 2 columns and 4 rows.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat2x4(...) end

---Builds a matrix of 3 columns and 2 rows.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat3x2(...) end

---Builds a matrix of 3 columns and 3 rows.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat3x3(...) end

---Same as `mat3x3`.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat3(...) end

---Builds a matrix of 3 columns and 4 rows.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat3x4(...) end

---Builds a matrix of 4 columns and 2 rows.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat4x2(...) end

---Builds a matrix of 4 columns and 3 rows.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat4x3(...) end

---Builds a matrix of 4 columns and 4 rows.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat4x4(...) end

---Same as `mat4x4`.
---@param ... number|vector2|vector3|vector4|quat|matrix
---@return matrix
---@nodiscard
function mat4(...) end

---Returns the dot product of two vectors or quaternions of the same kind.
---@generic T: vector2|vector3|vector4|quat
---@param a T
---@param b T
---@return number
---@nodiscard
function dot(a, b) end

---Returns the cross product of two vector3 values, or of two quaternions.
---@generic T: vector3|quat
---@param a T
---@param b T
---@return T
---@nodiscard
function cross(a, b) end

---Returns the inverse of a quaternion or a matrix.
---@generic T: quat|matrix
---@param v T
---@return T inverse
---@nodiscard
function inv(v) end

---Interpolates between two quaternions, or two vectors of the same size, along the shortest arc.
---@generic T: vector2|vector3|vector4|quat
---@param a T
---@param b T
---@param t number
---@return T
---@nodiscard
function slerp(a, b, t) end

---Iterates `t` through its `__iter` metamethod, or like `pairs` when it has none. CfxLua extension.
---@generic K, V
---@param t table<K, V>
---@return fun(t: table<K, V>, index?: K): K, V iterator
---@return table<K, V> t
---@return nil index
function each(t) end

---Returns its arguments with every table, function, thread and userdata replaced by nil. CfxLua extension.
---@param ... any
---@return any ...
function scrub(...) end

---Returns the number of UTF-8 characters in `s`. CfxLua extension.
---@param s string
---@return integer
---@nodiscard
function utf8.strlenutf8(s) end

---Compares two UTF-8 strings without regard to case: negative, zero or positive as `a` sorts before, with or after `b`. CfxLua extension.
---@param a string
---@param b string
---@return integer
---@nodiscard
function utf8.strcmputf8i(a, b) end
