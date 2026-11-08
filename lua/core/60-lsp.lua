-- Language servers (LSP)
--
-- Which server runs for which language: {language, command, arg...}. A
-- server starts when a file of its language opens, one per project.
-- Errors and warnings are underlined; ]d and [d jump between them, SPC c x
-- lists them. Redefine core.lsp_servers (or set vmac.hooks.lsp_servers) to
-- use other servers.
--
-- A server that isn't installed is installed when its first file opens, as
-- is the language's tree-sitter grammar (like Neovim's Mason): into
-- ~/.local/share/vmac. SPC h m or :packages lists what vmac can install
-- (RET installs or removes one); :install NAME and :uninstall NAME do it by
-- name. vmac.set_auto_install(false) in init.lua installs only when asked.

local V = vmac

function core.lsp_servers()
  return {
    { "rust", "rust-analyzer" },
    { "python", "pyright-langserver", "--stdio" },
    { "typescript", "typescript-language-server", "--stdio" },
    { "tsx", "typescript-language-server", "--stdio" },
    { "javascript", "typescript-language-server", "--stdio" },
    { "c", "clangd" },
    { "cpp", "clangd" },
    { "go", "gopls" },
    { "lua", "lua-language-server" },
    { "bash", "bash-language-server", "start" },
    { "yaml", "yaml-language-server", "--stdio" },
    { "html", "vscode-html-language-server", "--stdio" },
    { "css", "vscode-css-language-server", "--stdio" },
    { "json", "vscode-json-language-server", "--stdio" },
    { "toml", "taplo", "lsp", "stdio" },
    { "markdown", "marksman", "server" },
  }
end

local function confirm(question, yes, no)
  core.read(question .. " (y/n) ", function(answer)
    if core.contains({ "y", "yes" }, answer) then yes() else V.message(no or "kept") end
  end)
end
core.confirm = confirm

-- :packages / SPC h m: everything vmac can install, ✓ when installed.
V.command("package-manager", function()
  core.pick("Packages (RET installs, or removes an installed one)", V.package_list(), "", function(line)
    local name = V.package_name(line)
    if V.package_installed(name) then
      confirm("Remove " .. name .. "?", function() V.package_uninstall(name) end)
    else
      V.package_install(name)
    end
  end)
end)

-- Completion: typing a word asks the server; the popup then narrows as you
-- type. . and : ask afresh (foo. std::); other characters close it.
function core.after_self_insert(text)
  if V.word_char(text) then
    if V.completion_active() then V.completion_refresh() else V.lsp_complete() end
  elseif text == "." or text == ":" then
    V.completion_close()
    V.lsp_complete()
  else
    V.completion_close()
  end
end

-- Insert-state keys that act on the popup while it's open, else as usual.
local function with_completion(when_open, otherwise)
  return function()
    if V.completion_active() then when_open() else core.call_binding(otherwise) end
  end
end

core.completion_insert_keys = {
  ["C-n"] = with_completion(function() V.completion_move(1) end, "line-down-cmd"),
  ["C-p"] = with_completion(function() V.completion_move(-1) end, "line-up"),
  ["<down>"] = with_completion(function() V.completion_move(1) end, "line-down-cmd"),
  ["<up>"] = with_completion(function() V.completion_move(-1) end, "line-up"),
  RET = with_completion(V.completion_accept, "newline"),
  TAB = with_completion(V.completion_accept, "table-next-cell"),
  DEL = function()
    V.delete_backward_char(1)
    V.completion_refresh()
  end,
}

-- "2. Title" -> 2.
function core.leading_number(item) return tonumber(item:match("^%s*(%d+)")) end

-- gr: everywhere it's used, in the picker (straight there if only one).
core.events["lsp-show-references"] = function(items)
  core.pick("References", items, V.project_root(), function(item) V.goto_location(item) end)
end

-- SPC c r: rename everywhere, starting from the current name.
V.command("rename-symbol", function()
  local name = V.word_at_point()
  core.read("Rename to: ", function(new)
    if new ~= "" and new ~= name then V.lsp_rename(new) end
  end)
  V.insert(name)
end)

-- SPC c a: fixes and refactorings for the cursor, or the selection.
V.command("code-actions", function()
  if core.visual() then
    local b = core.visual_bounds()
    core.state = "normal"
    V.lsp_code_actions(b[1], b[2])
  else
    V.lsp_code_actions(V.point(), V.point())
  end
end)

core.events["lsp-show-code-actions"] = function(items)
  core.pick("Code actions", items, "", function(item) V.apply_code_action(core.leading_number(item) - 1) end)
end

-- SPC c R or :lsp-restart: stop the language servers and start them again
-- (also those that failed to start, e.g. after installing one).
V.command("lsp-restart-command", function() V.lsp_restart() end)

-- SPC c f: format the file, or the selected lines.
V.command("format-buffer", function()
  if core.visual() then
    local b = core.visual_bounds()
    core.state = "normal"
    V.lsp_format_range(b[1], b[2])
  else
    V.lsp_format()
  end
end)

V.command("goto-definition", function() V.lsp_goto_definition() end)
V.command("find-references", function() V.lsp_references() end)
V.command("hover-documentation", function() V.lsp_hover() end)
V.command("jump-back-command", function() V.jump_back() end)

V.command("diagnostic-next", function() V.diagnostic_goto(1) end)
V.command("diagnostic-previous", function() V.diagnostic_goto(-1) end)

-- "12:5  error: ..." -> goes to line 12, column 5.
V.command("diagnostic-list", function()
  local items = V.diagnostic_items()
  if #items == 0 then return V.message("No diagnostics") end
  core.pick("Diagnostics", items, "", function(item)
    local line, column = item:match("^(%d+):(%d+)")
    V.goto_line_column(tonumber(line), tonumber(column))
  end)
end)
