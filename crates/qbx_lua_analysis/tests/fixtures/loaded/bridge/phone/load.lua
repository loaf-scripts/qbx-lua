local files = { 'server/services.lua' }
for i = 1, #files do
    local loaded = load(LoadResourceFile('bridge', 'phone/' .. files[i]))
    if loaded then
        loaded()
    end
end
