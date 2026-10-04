Config = {}
Config.Label = locale('title')
Config.OnUse = function(item)
    LastUsed = item
    return EncryptedHelper(item)
end
