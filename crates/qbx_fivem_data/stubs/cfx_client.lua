---@meta

---Registers a handler for NUI fetch requests sent to `https://<resource>/<name>`. The handler receives the decoded request body and a reply function that must be called once.
---@param name string
---@param cb fun(data: unknown, cb: fun(response: any))
function RegisterNUICallback(name, cb) end

---Alternative spelling of `RegisterNUICallback`.
---@param name string
---@param cb fun(data: unknown, cb: fun(response: any))
function RegisterNuiCallback(name, cb) end

---JSON-encodes `message` and posts it to this resource's NUI page, where it arrives as a window `message` event.
---@param message table
function SendNUIMessage(message) end

---Sends a network event to the server; the handler there sees this player's id in `source`.
---@param eventName string
---@param ... any
function TriggerServerEvent(eventName, ...) end

---Sends a network event to the server over a rate-limited channel, intended for large payloads. `bps` is the transfer rate in bytes per second.
---@param eventName string
---@param bps integer
---@param ... any
function TriggerLatentServerEvent(eventName, bps, ...) end

---Returns a directly callable function for the native with the given hash, or nil when it cannot be resolved. Only the client has it.
---@param hash integer|string
---@return function? native
---@nodiscard
function Citizen.GetNative(hash) end

---Calls a game native by hash like `Citizen.InvokeNative`, with the same arguments and marker values. Only the client has it.
---@param hash integer|string
---@param ... any
---@return any ...
function Citizen.InvokeNative2(hash, ...) end

---State bag wrapper for the local player; `LocalPlayer.state` is that player's state bag.
---@type PlayerInterface
LocalPlayer = {}
