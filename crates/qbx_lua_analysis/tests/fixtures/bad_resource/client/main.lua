local unused = 1
local function neverCalled() end

Citizen.CreateThread(function()
    while true do
        local ped = GetPlayerPed(-1)
        local model = GetHashKey('adder')
        SetEntityCoords(ped, 0.0, 0.0, 0.0)
    end
end)

CreateThread(function()
    while true do
        Citizen.Wait(0)
        if Config.Debug then
        end
    end
end)

RegisterNetEvent('bad:client', function(a, a)
    local dist = GetDistanceBetweenCoords(0, 0, 0, 1, 1, 1, true)
    print(dist, undefinedThing, lib.notify, MySQL)
    TriggerClientEvent('nope', -1)
    local id = GetPlayerIdentifierByType(1, 'license')
    madeGlobal = string.notAFunction(id)
    local t = { a = 1, a = 2, ['b'] = 1, b = 2 }
    local x <const> = 1
    x = 2
    for i = 1, 2 do
        break
        print(i)
    end
    goto nowhere
    return t
end)

Notify('saved')

for i = 3, 1 do
    print(i)
end
