-- herdr-terminal: run gmacs terminals in herdr-core, so they keep running
-- after gmacs closes (and show in `hc ls` and the Herdr app).
--
-- A terminal backend (start, send, read, alive, kill) done with herdr's
-- `hc`. Pick it with :terminal-backend herdr (or uncomment the last line);
-- SPC o t then opens herdr terminals.
--
-- Input is by line: `hc keys` can't type characters like $ ( " |, so what
-- you type collects here (shown at the end of the screen) and RET sends the
-- line with `hc send`. Keys like ctrl+c, arrows and tab go at once. That
-- suits shells and agents; for full-screen programs (vim), use "local".

local function hc(...)
  return gmacs.exec("hc", { ... })
end

-- The line being typed into each terminal, not sent yet.
local pending = {}

gmacs.terminal_backend("herdr", {
  -- Starts the program (a shell for "") and returns its address, like t7.
  start = function(command, cwd, rows, cols)
    local args = { "run", "--bg", "--json", "--no-parent", "--outlive-parent", "--cwd", cwd }
    if command ~= "" then
      args[#args + 1] = "--"
      for word in command:gmatch("%S+") do args[#args + 1] = word end
    end
    local address = gmacs.exec("hc", args):match('"address":"([^"]+)"') or ""
    pending[address] = ""
    return address
  end,

  send = function(t, text, key)
    local line = pending[t] or ""
    if text then
      pending[t] = line .. text
    elseif key == "space" then
      pending[t] = line .. " "
    elseif key == "enter" then
      pending[t] = ""
      if line == "" then hc("keys", t, "enter") else hc("send", t, line) end
    elseif key == "backspace" and line ~= "" then
      pending[t] = line:sub(1, utf8.offset(line, -1) - 1)
    else
      hc("keys", t, key)
    end
  end,

  -- The screen, with the line being typed at its end.
  read = function(t)
    local screen = hc("read", t):gsub("%s+$", "")
    return screen .. (pending[t] or "")
  end,

  alive = function(t)
    return hc("get", t, "--json"):match('"state":"live"') ~= nil
  end,

  kill = function(t)
    pending[t] = nil
    hc("kill", t)
  end,
})

-- gmacs.set_terminal_backend("herdr")
