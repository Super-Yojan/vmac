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

---- Moving through a PDF
--
-- In a PDF, in front of normal state's keys (core.keymaps.pdf): j k and
-- C-e C-y scroll a little, C-d C-u half a window, C-f C-b a window, J K a
-- page, gg the first page and G the last (5G page 5). Counts work: 3j.
-- + and - zoom (Cmd = and Cmd -, and Ctrl with the wheel, too), = fits the pages to the window again
-- (Cmd 0), and h l scroll sideways across pages zoomed wider than it.

vmac.command("pdf-down", function() vmac.pdf_scroll("lines", 1) end)
vmac.command("pdf-up", function() vmac.pdf_scroll("lines", -1) end)
vmac.command("pdf-half-down", function() vmac.pdf_scroll("screens", 0.5) end)
vmac.command("pdf-half-up", function() vmac.pdf_scroll("screens", -0.5) end)
vmac.command("pdf-screen-down", function() vmac.pdf_scroll("screens", 1) end)
vmac.command("pdf-screen-up", function() vmac.pdf_scroll("screens", -1) end)
vmac.command("pdf-next-page", function() vmac.pdf_scroll("pages", 1) end)
vmac.command("pdf-previous-page", function() vmac.pdf_scroll("pages", -1) end)
local function page_or(fallback)
  return function()
    if core.count_given then vmac.pdf_scroll("page", core.count) else fallback() end
  end
end
local pdf_keys = {
  j = "pdf-down", k = "pdf-up", ["C-e"] = "pdf-down", ["C-y"] = "pdf-up",
  ["C-d"] = "pdf-half-down", ["C-u"] = "pdf-half-up",
  ["C-f"] = "pdf-screen-down", ["C-b"] = "pdf-screen-up",
  J = "pdf-next-page", K = "pdf-previous-page",
  ["g g"] = page_or(function() vmac.pdf_scroll("page", 1) end),
  G = page_or(function() vmac.pdf_scroll("end", 0) end),
}
vmac.command("pdf-zoom-in", function() vmac.pdf_zoom(1.25) end)
vmac.command("pdf-zoom-out", function() vmac.pdf_zoom(0.8) end)
vmac.command("pdf-zoom-fit", function() vmac.pdf_zoom(0) end)
vmac.command("pdf-left", function() vmac.pdf_scroll("sideways", -1) end)
vmac.command("pdf-right", function() vmac.pdf_scroll("sideways", 1) end)
for key, binding in pairs({
  ["+"] = "pdf-zoom-in", ["-"] = "pdf-zoom-out", ["="] = "pdf-zoom-fit",
  ["s-="] = "pdf-zoom-in", ["s-+"] = "pdf-zoom-in", ["s--"] = "pdf-zoom-out", ["s-0"] = "pdf-zoom-fit",
  h = "pdf-left", l = "pdf-right",
}) do pdf_keys[key] = binding end
for key, binding in pairs(pdf_keys) do core.define_key("pdf", key, binding) end
