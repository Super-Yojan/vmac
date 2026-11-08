-- word-count: SPC t w shows how many words and lines the buffer has.
-- A Lua plugin; it can require modules from its folder or its lua/.
local count = require("count")

vmac.command("word-count", function()
  local text = vmac.buffer_string()
  vmac.message(count.words(text) .. " words, " .. count.lines(text) .. " lines")
end)
vmac.map("normal", "SPC t w", "word-count")
