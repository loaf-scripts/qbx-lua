---@meta

---Sends a network event to one client, or to every connected client when `playerSrc` is -1.
---@param eventName string
---@param playerSrc integer|string
---@param ... any
function TriggerClientEvent(eventName, playerSrc, ...) end

---Sends a network event to a client (-1 for all) over a rate-limited channel, intended for large payloads. `bps` is the transfer rate in bytes per second.
---@param eventName string
---@param playerSrc integer|string
---@param bps integer
---@param ... any
function TriggerLatentClientEvent(eventName, playerSrc, bps, ...) end

---Returns every identifier of the player, such as "license:...", "discord:..." or "steam:...".
---@param playerSrc integer|string
---@return string[] identifiers
---@nodiscard
function GetPlayerIdentifiers(playerSrc) end

---Returns the hardware tokens reported for the player.
---@param playerSrc integer|string
---@return string[] tokens
---@nodiscard
function GetPlayerTokens(playerSrc) end

---Returns the server ids of all connected players, as strings.
---@return string[] players
---@nodiscard
function GetPlayers() end

---Writes text to the server console; equivalent to `Citizen.Trace`.
---@param text string
function RconPrint(text) end

---@class HttpRequestOptions
---@field followLocation? boolean
local HttpRequestOptions = {}

---Starts an asynchronous HTTP request. The callback receives the status code, the response body (nil on failure), the response headers and an error description when the request failed.
---@param url string
---@param cb fun(status: integer, body?: string, headers: table<string, string>, errorData?: string)
---@param method? string
---@param data? string
---@param headers? table<string, string>
---@param options? HttpRequestOptions
function PerformHttpRequest(url, cb, method, data, headers, options) end

---Performs an HTTP request and yields the current thread until it completes. Must be called from a scheduler thread.
---@param url string
---@param method? string
---@param data? string
---@param headers? table<string, string>
---@param options? HttpRequestOptions
---@return integer status
---@return string? body
---@return table<string, string> headers
---@return string? errorData
function PerformHttpRequestAwait(url, method, data, headers, options) end

---Writes a structured entry to the server log that RCON clients and txAdmin can read.
---@param data table
function RconLog(data) end

---Returns the network endpoint (ip:port) of a player.
---@param playerSrc integer|string
---@return string endpoint
function GetPlayerEP(playerSrc) end

---Returns a timestamp in microseconds. CfxLua extension.
---@return integer
function os.microtime() end

---Returns a timestamp in nanoseconds. CfxLua extension.
---@return integer
function os.nanotime() end

---Returns the difference between two timestamps. CfxLua extension.
---@param a integer
---@param b integer
---@return integer
function os.deltatime(a, b) end

---Returns the processor time stamp counter. CfxLua extension.
---@return integer
function os.rdtsc() end

---Returns the processor time stamp counter, serialized. CfxLua extension.
---@return integer
function os.rdtscp() end

---Creates a directory. Returns true, or nil plus an error message and code when it already exists or cannot be created. CfxLua extension.
---@param path string
---@return boolean? ok
---@return string? err
---@return integer? code
function os.createdir(path) end

---The entries of a directory, as `io.readdir` lists them. CfxLua extension.
---@class directory
local directory = {}

---Returns an iterator over the names of the entries, without "." and "..".
---@return fun(): string iterator
function directory:lines() end

---Frees the list of entries; `lines` fails once it is closed.
---@return boolean ok
function directory:close() end

---Lists the entries of a directory. Returns them, or nil plus an error message and code when the path is on no known device. CfxLua extension.
---@param path string
---@return directory? entries
---@return string? err
---@return integer? code
---@nodiscard
function io.readdir(path) end
