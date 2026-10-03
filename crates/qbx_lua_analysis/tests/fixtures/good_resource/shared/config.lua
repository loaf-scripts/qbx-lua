SharedConfig = {
    models = { `adder`, `zentorno` },
    .enabled,
}

---@param value table
---@param fallback? string
---@return string?
function SharedHelper(value, fallback)
    return value?.nested?.field or fallback
end
