-- An example ~/.config/vmac/init.lua. Lua sits beside vmac's Scheme core:
-- the same primitives (vmac.insert, vmac.buffer_name, vmac.split_window,
-- ... the Scheme names with - as _), and commands and keys that mix with
-- Scheme's. Save it and vmac reloads it.

-- A command, callable from keys, SPC :, or Scheme ('insert-date).
vmac.command("insert-date", function()
  vmac.insert(os.date("%Y-%m-%d"))
end)
vmac.map("normal", "SPC i d", "insert-date")

-- A key bound straight to a function.
vmac.map("normal", "SPC i t", function()
  vmac.insert("TODO: ")
  vmac.run("insert-state")   -- run a Scheme command, after this one
end)

-- Replace the Scheme modeline: a list of {text, face}.
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
