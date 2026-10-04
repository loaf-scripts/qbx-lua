local files = { 'server/queries.lua', 'server/services.lua' }
for i = 1, #files do
    local loaded = load(LoadResourceFile('bridge', 'tablet/' .. files[i]))
    if loaded then
        loaded()
    end
end
