-- Options you toggle while you work.
--
--   SPC t l   line numbers: off -> on -> relative -> off
--
-- In init.lua: vmac.set_line_numbers("on"), or "relative", or "off".

vmac.command("toggle-line-numbers", function()
  local next = { off = "on", on = "relative", relative = "off" }
  local mode = next[vmac.line_numbers()] or "on"
  vmac.set_line_numbers(mode)
  vmac.message("Line numbers: " .. mode)
end)
vmac.map("normal", "SPC t l", "toggle-line-numbers")
