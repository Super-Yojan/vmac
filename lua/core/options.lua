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

---- Moving through an image or PDF
--
-- In one, in front of normal state's keys (core.keymaps.media): j k and
-- C-e C-y scroll a little, C-d C-u half a window, C-f C-b a window, h l
-- sideways, gg to the top and G the end. In a PDF J K go a page and 5G to
-- page 5. Counts work: 3j. + and - zoom (Cmd = and Cmd -, and Ctrl with
-- the wheel, too), = fits it to the window again (Cmd 0).

vmac.command("media-down", function() vmac.media_scroll("lines", 1) end)
vmac.command("media-up", function() vmac.media_scroll("lines", -1) end)
vmac.command("media-half-down", function() vmac.media_scroll("screens", 0.5) end)
vmac.command("media-half-up", function() vmac.media_scroll("screens", -0.5) end)
vmac.command("media-screen-down", function() vmac.media_scroll("screens", 1) end)
vmac.command("media-screen-up", function() vmac.media_scroll("screens", -1) end)
vmac.command("media-next-page", function() vmac.media_scroll("pages", 1) end)
vmac.command("media-previous-page", function() vmac.media_scroll("pages", -1) end)
local function page_or(fallback)
  return function()
    if core.count_given then vmac.media_scroll("page", core.count) else fallback() end
  end
end
local media_keys = {
  j = "media-down", k = "media-up", ["C-e"] = "media-down", ["C-y"] = "media-up",
  ["C-d"] = "media-half-down", ["C-u"] = "media-half-up",
  ["C-f"] = "media-screen-down", ["C-b"] = "media-screen-up",
  J = "media-next-page", K = "media-previous-page",
  ["g g"] = page_or(function() vmac.media_scroll("page", 1) end),
  G = page_or(function() vmac.media_scroll("end", 0) end),
}
vmac.command("media-zoom-in", function() vmac.media_zoom(1.25) end)
vmac.command("media-zoom-out", function() vmac.media_zoom(0.8) end)
vmac.command("media-zoom-fit", function() vmac.media_zoom(0) end)
vmac.command("media-left", function() vmac.media_scroll("sideways", -1) end)
vmac.command("media-right", function() vmac.media_scroll("sideways", 1) end)
for key, binding in pairs({
  ["+"] = "media-zoom-in", ["-"] = "media-zoom-out", ["="] = "media-zoom-fit",
  ["s-="] = "media-zoom-in", ["s-+"] = "media-zoom-in", ["s--"] = "media-zoom-out", ["s-0"] = "media-zoom-fit",
  h = "media-left", l = "media-right",
}) do media_keys[key] = binding end
for key, binding in pairs(media_keys) do core.define_key("media", key, binding) end
