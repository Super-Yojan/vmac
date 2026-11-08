-- Sessions: projects open side by side, like tmux sessions. Each has its
-- own buffers, windows, terminals, language servers and agent, so one
-- project's files never show up in another's buffer list.
--
--   SPC p p   open a project: its own session (made the first time)
--   SPC S s   switch session                SPC S l   the last session (on any machine)
--   SPC S n   new session (asks a name)     SPC S r   rename this session
--   SPC S k   kill a session                SPC S K   kill it, unsaved files and all
--   SPC S c   connect to another machine (:connect HOST [TARGET]), over
--             ssh: its sessions join this window's
--
-- The switcher lists every machine's sessions: this machine's by name,
-- another's as host:name (and, from another machine, this one's as
-- local:name). :q in another machine's session detaches from it.
--
-- The first project a session opens names it. To open projects in the
-- session you're in instead: vmac.set_session_per_project(false)

local function others()
  local here, list = vmac.session_name(), {}
  for _, name in ipairs(vmac.session_list()) do
    if name ~= here then list[#list + 1] = name end
  end
  return list
end

vmac.command("session-switch-command", function()
  local list = others()
  if #list == 0 then
    vmac.message("Only one session (SPC p p opens a project in its own)")
    return
  end
  vmac.pick("Switch session", list, function(name) vmac.session_switch(name) end)
end)

vmac.command("session-last-command", function() vmac.session_last() end)

vmac.command("session-new-command", function()
  vmac.prompt("New session: ", function(name)
    if name ~= "" then vmac.session_new(name, "") end
  end)
end)

vmac.command("session-rename-command", function()
  vmac.prompt("Rename session: ", function(name)
    if name ~= "" then vmac.session_rename(name) end
  end, vmac.session_name())
end)

local function kill(force)
  return function()
    vmac.pick("Kill session", vmac.session_list(), function(name) vmac.session_kill(name, force) end)
  end
end
vmac.command("session-kill-command", kill(false))
vmac.command("session-kill-force-command", kill(true))

local function connect(line)
  local host, target = line:match("^(%S+)%s*(.*)$")
  if host then vmac.connect(host, target) end
end

vmac.command("connect-command", function() vmac.prompt("Connect to (HOST [TARGET]): ", connect) end)
core.ex_commands["connect"] = function(arg)
  if arg == "" then core.call_binding("connect-command") else connect(arg) end
end

vmac.map("normal", "SPC S c", "connect-command")
vmac.map("normal", "SPC S s", "session-switch-command")
vmac.map("normal", "SPC S l", "session-last-command")
vmac.map("normal", "SPC S n", "session-new-command")
vmac.map("normal", "SPC S r", "session-rename-command")
vmac.map("normal", "SPC S k", "session-kill-command")
vmac.map("normal", "SPC S K", "session-kill-force-command")
