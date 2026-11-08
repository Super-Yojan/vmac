-- An example ~/.config/gmacs/init.lua. Lua sits beside gmacs's Scheme core:
-- the same primitives (gmacs.insert, gmacs.buffer_name, gmacs.split_window,
-- ... the Scheme names with - as _), and commands and keys that mix with
-- Scheme's. Save it and gmacs reloads it.

-- A command, callable from keys, SPC :, or Scheme ('insert-date).
gmacs.command("insert-date", function()
  gmacs.insert(os.date("%Y-%m-%d"))
end)
gmacs.map("normal", "SPC i d", "insert-date")

-- A key bound straight to a function.
gmacs.map("normal", "SPC i t", function()
  gmacs.insert("TODO: ")
  gmacs.run("insert-state")   -- run a Scheme command, after this one
end)

-- Replace the Scheme modeline: a list of {text, face}.
gmacs.hooks.modeline = function()
  local mark = gmacs.buffer_modified() and " ●" or ""
  return {
    { gmacs.buffer_name() .. mark, "strong" },
    { gmacs.line_number() .. ":" .. gmacs.column_number(), "dim" },
    { gmacs.buffer_language(), "dim" },
    { gmacs.diagnostic_summary(), "error" },
  }
end

-- Use other language servers.
-- gmacs.hooks.lsp_servers = function()
--   return { {"python", "pylsp"}, {"rust", "rust-analyzer"} }
-- end
