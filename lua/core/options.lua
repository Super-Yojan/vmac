-- Options you toggle while you work.
--
--   SPC t l   line numbers: off -> on -> relative -> off
--   SPC t p   PDFs in the theme's colors, or black on white as printed
--
-- In init.lua: vmac.set_line_numbers("on"), or "relative", or "off";
-- vmac.set_pdf_themed(false) for PDFs as printed.

vmac.command("toggle-line-numbers", function()
  local next = { off = "on", on = "relative", relative = "off" }
  local mode = next[vmac.line_numbers()] or "on"
  vmac.set_line_numbers(mode)
  vmac.message("Line numbers: " .. mode)
end)
vmac.map("normal", "SPC t l", "toggle-line-numbers")

vmac.command("toggle-pdf-theme", function()
  local on = not vmac.pdf_themed()
  vmac.set_pdf_themed(on)
  vmac.message(on and "PDFs: in the theme's colors" or "PDFs: as printed")
end)
vmac.map("normal", "SPC t p", "toggle-pdf-theme")
