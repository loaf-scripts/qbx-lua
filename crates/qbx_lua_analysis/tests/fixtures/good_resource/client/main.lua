local config = require 'modules.settings'
local count = 0

CreateThread(function()
    while true do
        count += 1
        local coords = GetEntityCoords(cache.ped)
        local nearby = lib.points.getClosestPoint()
        if nearby and #(coords - nearby.coords) < config.distance then
            qbx.drawText3d({ text = SharedHelper(QBX.PlayerData), coords = coords })
        end
        Wait(SharedConfig.enabled and 0 or 1000)
    end
end)

RegisterNetEvent('good:client', function(_unused, used)
    local x, y, z in used
    print(x, y, z, `hash`)
    TriggerServerEvent('good:server', { x, y, z })
end)

RegisterNUICallback('close', function(_, cb)
    SetNuiFocus(false, false)
    cb(1)
end)

---@return integer
exports('getCount', function() return count end)
