-- herdr-terminal: run vmac terminals in herdr-core, so they keep running
-- after vmac closes (and show in `hc ls` and the Herdr app).
--
-- A terminal backend in attach mode: herdr starts the terminal (`hc run`),
-- and vmac shows it by running herdr's own client, `hc attach t7`, on a
-- local PTY. Keys go straight through (vim works), the terminal takes the
-- window's size and follows it, and colors carry over. Closing the buffer
-- ends the terminal in herdr too.
--
-- Pick it with :terminal-backend herdr (or uncomment the last line); SPC o t
-- then opens herdr terminals.

vmac.terminal_backend("herdr", {
  -- Starts the program (a shell for "") in herdr, then attaches to it.
  start = function(command, cwd, rows, cols)
    local args = { "run", "--bg", "--json", "--no-parent", "--outlive-parent", "--cwd", cwd }
    if command ~= "" then
      args[#args + 1] = "--"
      for word in command:gmatch("%S+") do args[#args + 1] = word end
    end
    local address = vmac.exec("hc", args):match('"address":"([^"]+)"')
    if not address then return nil end
    return { handle = address, command = "hc attach " .. address }
  end,

  kill = function(t)
    vmac.exec("hc", { "kill", t })
  end,
})

-- vmac.set_terminal_backend("herdr")
