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
