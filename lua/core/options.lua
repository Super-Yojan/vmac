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

---- The PDF beside its source
--
-- SPC m p in a Typst, LaTeX (or any) file opens the PDF built from it in a
-- window on the right, the cursor staying in the source, and closes it
-- again (in Markdown, SPC m p is its preview). paper.typ's PDF is
-- paper.pdf beside it, or in out/, build/ or _build/; one not built yet
-- shows up when it is. It reloads as it's rebuilt: run `typst watch
-- paper.typ` or `latexmk -pvc -pdf paper.tex` (SPC o t for a terminal).
-- Redefine core.pdf_for(file) for PDFs kept elsewhere.

function core.pdf_for(file)
  local dir, stem = file:match("^(.*)/([^/]+)%.[^./]+$")
  if not dir then return nil end
  for _, sub in ipairs({ "", "out/", "build/", "_build/" }) do
    local pdf = dir .. "/" .. sub .. stem .. ".pdf"
    if vmac.file_exists(pdf) then return pdf end
  end
  return dir .. "/" .. stem .. ".pdf"
end

vmac.command("pdf-preview", function()
  local file = vmac.buffer_file_name()
  if file:lower():match("%.pdf$") then return vmac.message("This is the PDF") end
  local pdf = file ~= "" and core.pdf_for(file)
  if not pdf then return vmac.message("Not a file with a PDF built from it") end
  local result = vmac.toggle_file_beside(pdf)
  if result == "opened" then
    vmac.message(vmac.file_exists(pdf) and ("PDF: " .. pdf)
      or ("Not built yet: " .. pdf .. " shows up when it is"))
  elseif result == "closed" then
    vmac.message("PDF closed")
  else
    vmac.message(result)
  end
end)

-- SPC m p (in 90-bindings.lua): Markdown's preview in a Markdown file (and
-- closing it from anywhere while it's open), else the PDF.
vmac.command("preview", function()
  local markdown = vmac.buffer_language() == "markdown" or vmac.markdown_preview()
  vmac.run(markdown and "markdown-preview" or "pdf-preview")
end)

---- From the PDF to the source: SyncTeX
--
-- Double-click a PDF's page to jump to the source line it was built from,
-- in the source's window. It asks TeX Live's synctex, so it works for PDFs
-- built by pdflatex, xelatex or lualatex with -synctex=1 (latexmk -pvc
-- -pdf -synctex=1 paper.tex; many setups have it on already), which leaves
-- paper.synctex.gz beside the PDF. Typst doesn't write SyncTeX.
-- vmac.hooks.pdf_double_click(pdf, page, x, y) is called with the page
-- (from 1) and the point in PDF points from its top left: replace it for
-- another tool.

-- Where synctex says a point on a page came from: file, line, column (or nil).
function core.synctex_edit(pdf, page, x, y)
  local out = vmac.exec("synctex", { "edit", "-o", page .. ":" .. x .. ":" .. y .. ":" .. pdf })
  local file = out:match("\nInput:([^\n]+)") or out:match("^Input:([^\n]+)")
  local line = tonumber(out:match("\nLine:(%-?%d+)") or "")
  if not file or not line or line < 1 then return nil end
  local column = tonumber(out:match("\nColumn:(%-?%d+)") or "") or 0
  -- "./paper.tex" and the like are from the PDF's folder.
  if not file:match("^/") then file = pdf:match("^(.*)/") .. "/" .. file:gsub("^%./", "") end
  return file, line, math.max(column, 0)
end

vmac.hooks.pdf_double_click = function(pdf, page, x, y)
  local file, line, column = core.synctex_edit(pdf, page, x, y)
  if not file then
    return vmac.message("No source found there: is synctex installed, and was the PDF built with -synctex=1?")
  end
  local err = vmac.show_file_elsewhere(file)
  if err ~= "" then return vmac.message(err) end
  vmac.goto_line_column(line, column + 1)
  vmac.message(file:match("[^/]+$") .. ":" .. line)
end

---- From the source to the PDF: SyncTeX's forward search
--
-- SPC m s in a LaTeX file shows where the cursor's line is in its PDF
-- (opened beside it if it isn't showing): scrolled to, and marked for a
-- moment. Like the other way, it needs synctex and a PDF built with
-- -synctex=1.
--
-- While the PDF shows beside a .tex file, it follows the cursor on its own:
-- once the cursor rests on another line for a moment, the PDF scrolls there
-- if that's out of view. SPC t s turns that off and on
-- (core.pdf_follow = false in init.lua for off).

-- Where a source line is in its PDF: the page (from 1) and the box of
-- its text, x, y (top left), width and height in PDF points (or nil).
function core.synctex_view(file, line, column, pdf)
  local out = vmac.exec("synctex", { "view", "-i", line .. ":" .. column .. ":" .. file, "-o", pdf })
  local function number(key) return tonumber(out:match("\n" .. key .. ":(%-?[%d%.]+)") or "") end
  local page, h, v, width, height = number("Page"), number("h"), number("v"), number("W"), number("H")
  if not (page and h and v) then return nil end
  width, height = width or 0, height or 0
  -- v is the text's baseline: its box rises from there.
  return page, h, v - height, width, height
end

-- Shows a source line in the PDF: on a key, opening the PDF and saying
-- what happened; following the cursor, quietly, only where it shows.
function core.forward_search(file, line, follow)
  local pdf = file ~= "" and not file:lower():match("%.pdf$") and core.pdf_for(file)
  if not pdf then return follow or vmac.message("Not a file with a PDF built from it") end
  if not vmac.file_exists(pdf) then return follow or vmac.message("Not built yet: " .. pdf) end
  local page, x, y, width, height = core.synctex_view(file, line, 0, pdf)
  if not page then
    return follow or vmac.message("Not found in the PDF: is synctex installed, and was it built with -synctex=1?")
  end
  if not follow then
    local err = vmac.show_file_beside(pdf)
    if err ~= "" then return vmac.message(err) end
    vmac.message("Page " .. page)
  end
  vmac.pdf_reveal(pdf, page, x, y, width, height, follow)
end

vmac.command("pdf-forward-search", function()
  core.forward_search(vmac.buffer_file_name(), vmac.line_number(), false)
end)

-- Following the cursor: the line it's on, and for how many ticks. It acts
-- once the cursor has rested a few ticks, so moving through lines doesn't
-- run synctex for each.
core.pdf_follow = core.pdf_follow ~= false
local resting = { at = nil, ticks = 0 }
local REST_TICKS = 3

core.tick_hooks.pdf_follow = function()
  if not core.pdf_follow or vmac.minibuffer_active() then return false end
  local file = vmac.buffer_file_name()
  if not file:match("%.tex$") then return false end
  local pdf = core.pdf_for(file)
  if not (pdf and vmac.file_shown(pdf)) then
    resting.at = nil
    return false
  end
  -- A new line, or the PDF just opened: rest a moment, then show it.
  local at = file .. ":" .. vmac.line_number()
  if at ~= resting.at then
    resting.at, resting.ticks = at, 0
    return false
  end
  resting.ticks = resting.ticks + 1
  if resting.ticks ~= REST_TICKS then return false end
  core.forward_search(file, vmac.line_number(), true)
  return true
end

vmac.command("toggle-pdf-follow", function()
  core.pdf_follow = not core.pdf_follow
  vmac.message(core.pdf_follow and "The PDF follows the cursor" or "The PDF stays put (SPC m s to show the line)")
end)
