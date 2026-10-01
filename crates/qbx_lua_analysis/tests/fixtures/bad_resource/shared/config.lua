Config = {}
Config.Debug = false
Config.Debug = Config.Debug
helper = 1

---@param message string
---@param duration integer
function Notify(message, duration)
    print(message, duration)
end

---@param msg string
function Config.Notify(message)
    print(message)
end

function Config.Notify(message)
    print('replaced', message)
end
