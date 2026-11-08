-- word-count: SPC t w shows how many words and lines the buffer has.
-- A Lua plugin; it can require modules from its folder or its lua/.
local count = require("count")

gmacs.command("word-count", function()
  local text = gmacs.buffer_string()
  gmacs.message(count.words(text) .. " words, " .. count.lines(text) .. " lines")
end)
gmacs.map("normal", "SPC t w", "word-count")
