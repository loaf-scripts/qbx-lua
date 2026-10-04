Config = {}
Config.Debug = false
Config.Debug = Config.Debug
helper = 1

---@param message string
---@param duration integer
function Notify(message, duration)
    print(message, duration)
end

---@alias NotifyType 'inform'|'error'
---@alias NotifyType 'success'

---@class NotifyOptions
---@field duration integer
---@field duration number

---@param msg string
function Config.Notify(message)
    print(message)
end

function Config.Notify(message)
    print('replaced', message)
end

local item = Config.Item
local fallback = print
(item):Use()
print(fallback)

local spaced = 1   
print(spaced)

local function done() return end
done()

---@param amount number
---@param amount integer
function Config.Pay(amount)
    print(amount)
end

---@field orphan number
local orphaned = {}
print(orphaned)

---@class Config.Vector
---@operator eq: boolean
local vector = {}
print(vector)

---@cast missing string

---@diagnostic disable-next-line: no-such-rule
print(Config.Debug)
