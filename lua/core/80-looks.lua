-- Markdown (tables, viewing), syntax colors, faces, themes, the modeline
-- and plugins.

local V = vmac

---- Markdown tables, like Org mode
--
-- Leaving insert state in a table aligns it (columns padded to their widest
-- cell, the |---| row redrawn); TAB and S-TAB move between cells, aligning
-- as they go, and TAB in the last cell adds a row. SPC m a aligns on demand.

function core.in_markdown_table()
  return not V.minibuffer_active() and V.buffer_language() == "markdown" and V.table_at_point()
end

V.command("table-next-cell", function()
  if core.in_markdown_table() then V.align_table(1) else core.call_binding("indent") end
end)
V.command("table-previous-cell", function()
  if core.in_markdown_table() then V.align_table(-1) end
end)
V.command("table-align", function()
  if not (core.in_markdown_table() and V.align_table(0)) then V.message("Not in a table") end
end)

---- Viewing Markdown
--
-- Outside insert and visual state, a Markdown file is drawn like the
-- preview, by gpui-kit: headings, code blocks, tables, images and HTML
-- rendered. Motions still move the cursor through the text: the view
-- scrolls to its line and marks it. i, a, o... (or v) show the text as
-- typed, to edit. SPC t m cycles: rendered -> markup hidden in the text
-- (headings without #, code without backticks, still the editor's own
-- drawing) -> as typed. In init.lua: vmac.set_markdown_view("hidden"), or
-- "rendered" or "raw".
--
-- SPC m p opens a preview beside it instead (in a Markdown file; elsewhere
-- it's the PDF built from the file, see options.lua), following the file you're in
-- as you type; again closes it. vmac.set_markdown_preview(true) opens it
-- from the start.

V.command("markdown-preview", function() V.markdown_preview_toggle() end)
V.command("markdown-conceal", function() V.markdown_view_cycle() end)

---- Syntax highlighting
--
-- tree-sitter names what each piece of code is (keyword, string,
-- function.method, ...). This says what each name looks like: a face, which
-- is a theme color (red green blue yellow magenta cyan) or default, strong,
-- dim, accent, error. A name not listed uses its parent's entry
-- (function.method -> function), else plain text.

function core.syntax_theme()
  local theme = {
    { "keyword", "magenta" }, { "operator", "dim" },
    { "string", "green" }, { "string.special", "cyan" }, { "escape", "cyan" },
    { "comment", "dim" },
    { "function", "blue" }, { "function.macro", "magenta" }, { "constructor", "yellow" },
    { "type", "yellow" }, { "type.builtin", "yellow" },
    { "constant", "cyan" }, { "constant.builtin", "cyan" }, { "number", "cyan" },
    { "boolean", "cyan" }, { "attribute", "cyan" }, { "label", "cyan" },
    { "property", "default" }, { "variable.builtin", "red" },
    { "punctuation", "dim" }, { "tag", "red" }, { "embedded", "default" },
    -- Markdown (named by markdown.rs, styled by the md- faces below)
    { "markup.bold", "md-bold" }, { "markup.italic", "md-italic" },
    { "markup.raw", "md-code" }, { "markup.raw.block", "md-code-block" },
    { "markup.raw.block.info", "dim" },
    { "markup.link.text", "md-link" }, { "markup.link.url", "md-url" },
    { "markup.quote", "md-quote" }, { "markup.quote.marker", "md-quote" },
    { "markup.list", "md-list" }, { "markup.rule", "md-rule" },
    { "markup.table.delimiter", "dim" }, { "markup.table.header", "md-bold" },
  }
  for level = 1, 6 do
    theme[#theme + 1] = { "markup.heading." .. level, "md-h" .. level }
    theme[#theme + 1] = { "markup.heading." .. level .. ".marker", "md-h" .. level .. "-mark" }
  end
  return theme
end

-- How faces look beyond a color: { name, color=, background=, scale= (text
-- size, 1 = normal), bold=, italic=, underline=, sans= (the UI font instead
-- of the monospace one) }. A face used in core.syntax_theme without a spec
-- here is just a color.
local function heading_faces(out, level, scale, color)
  out[#out + 1] = { "md-h" .. level, color = color, scale = scale, bold = true, sans = true }
  out[#out + 1] = { "md-h" .. level .. "-mark", color = "dim", scale = scale, sans = true }
end

function core.default_face_specs()
  local specs = {}
  heading_faces(specs, 1, 1.8, "accent")
  heading_faces(specs, 2, 1.5, "accent")
  heading_faces(specs, 3, 1.3, "blue")
  heading_faces(specs, 4, 1.15, "blue")
  heading_faces(specs, 5, 1.05, "default")
  heading_faces(specs, 6, 1.0, "default")
  for _, spec in ipairs({
    { "md-bold", bold = true },
    { "md-italic", italic = true },
    { "md-code", color = "cyan", background = "subtle" },
    { "md-code-block", color = "default" },
    { "md-link", color = "blue", underline = true },
    { "md-url", color = "dim" },
    { "md-quote", color = "dim", italic = true },
    { "md-list", color = "accent", bold = true },
    { "md-rule", color = "dim" },
    -- The agent buffer
    { "agent-user", bold = true, color = "accent" },
    { "agent-heading", bold = true },
    { "agent-ask", bold = true, color = "yellow" },
    { "agent-thought", italic = true, color = "dim" },
    { "agent-output", color = "dim" },
  }) do
    specs[#specs + 1] = spec
  end
  return specs
end

-- Your faces, in front of the defaults (the first spec with a name wins):
--   vmac.set_faces({ { "md-bold", bold = true, color = "yellow" } })
core.custom_face_specs = {}
function V.set_faces(specs)
  for i = #specs, 1, -1 do table.insert(core.custom_face_specs, 1, specs[i]) end
end
function core.face_specs()
  local out = {}
  for _, s in ipairs(core.custom_face_specs) do out[#out + 1] = s end
  for _, s in ipairs(core.default_face_specs()) do out[#out + 1] = s end
  return out
end

---- Themes
--
-- SPC h t (or :theme NAME) picks a color theme: Catppuccin, Tokyo Night,
-- Gruvbox, Solarized, Everforest, Ayu, Flexoki... (built in), or yours in
-- ~/.config/vmac/themes/*.json (gpui-kit's theme format). A theme is light
-- or dark and switches to its mode; :theme system follows the system again,
-- with the themes last picked for each mode. In init.lua:
--   vmac.set_themes("Catppuccin Latte", "Catppuccin Mocha")   -- light, dark
-- or vmac.set_theme("Tokyo Night"). The syntax and Markdown colors above
-- are the theme's own red, green, blue..., so they follow it.

V.command("theme-picker", function()
  core.pick("Theme", V.theme_list(), "", function(name) V.set_theme(name) end)
end)

---- The modeline
--
-- A list of {text, face} segments. Faces: default, strong, dim, accent,
-- error. Redefine core.modeline (or set vmac.hooks.modeline) to change
-- what the bottom bar shows.

local state_names = {
  insert = "INSERT", visual = "VISUAL", ["visual-line"] = "V-LINE",
  ["visual-block"] = "V-BLOCK", operator = "O-PENDING",
}

function core.state_name()
  if V.minibuffer_active() and core.inserting() then return "INSERT" end
  return state_names[core.state] or "NORMAL"
end

function core.modeline()
  return {
    { core.state_name(), core.inserting() and "strong" or "accent" },
    { V.buffer_name() .. (V.buffer_modified() and " ●" or ""), "strong" },
    { V.line_number() .. ":" .. V.column_number(), "dim" },
    { V.buffer_language(), "dim" },
    { V.diagnostic_summary(), "error" },
    { V.lsp_status(), "dim" },
    { core.pending_count .. core.pending_keys, "dim" },
  }
end

---- Plugins
--
-- Plugins live in ~/.config/vmac/plugins: a folder with init.lua, or a
-- single name.lua. They load after the core and before your init.lua, so
-- your config can change what they set. A plugin that fails to load is
-- skipped (its error shows in the list); the others still load.
--
-- SPC h p lists them (picking one turns it on or off). :plugin-install URL
-- clones one with git, :plugin-update pulls them all, :plugin-remove NAME
-- deletes one. Editing a plugin's files reloads it.

V.command("plugin-manager", function()
  local items = V.plugin_list()
  if #items == 0 then return V.message("No plugins in " .. V.plugin_dir()) end
  core.pick("Plugins (RET turns one on or off)", items, "", function(item) V.plugin_toggle(item:match("^%S+")) end)
end)

V.command("plugin-update-all", function() V.plugin_update() end)

function core.plugin_remove_asking(name)
  core.confirm("Remove the plugin " .. name .. "?", function() V.plugin_remove(name) end)
end
