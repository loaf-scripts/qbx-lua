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
---@return boolean success
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

---State bag wrapper for the local player; `LocalPlayer.state` is that player's state bag.
---@type PlayerInterface
LocalPlayer = {}
