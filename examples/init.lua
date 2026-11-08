-- An example ~/.config/vmac/init.lua. vmac's core is Lua too (lua/core/):
-- the primitives are in `vmac` (vmac.insert, vmac.buffer_name,
-- vmac.split_window, ...) and the core's state and functions in `core`
-- (core.state, core.keymaps, core.modeline, ...). Save it and vmac reloads it.

-- A command, callable from keys, SPC : or vmac.run("insert-date").
vmac.command("insert-date", function()
  vmac.insert(os.date("%Y-%m-%d"))
end)
vmac.map("normal", "SPC i d", "insert-date")

-- A key bound straight to a function.
vmac.map("normal", "SPC i t", function()
  vmac.insert("TODO: ")
  vmac.run("insert-state")   -- run a command by name
end)

-- Replace the modeline: a list of {text, face}.
vmac.hooks.modeline = function()
  local mark = vmac.buffer_modified() and " ●" or ""
  return {
    { vmac.buffer_name() .. mark, "strong" },
    { vmac.line_number() .. ":" .. vmac.column_number(), "dim" },
    { vmac.buffer_language(), "dim" },
    { vmac.diagnostic_summary(), "error" },
  }
end

-- Use other language servers.
-- vmac.hooks.lsp_servers = function()
--   return { {"python", "pylsp"}, {"rust", "rust-analyzer"} }
-- end

-- The coding agent: pick one (claude, codex, gemini, or your own), and let
-- it read and search without asking.
-- vmac.agent_use("codex")
-- vmac.agent_backend("my-agent", { command = "my-agent --acp" })
-- vmac.hooks.agent_permission = function(title, kind)
--   if kind == "read" or kind == "search" then return "allow" end
-- end
