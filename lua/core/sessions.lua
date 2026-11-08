-- Sessions: projects open side by side, like tmux sessions. Each has its
-- own buffers, windows, terminals, language servers and agent, so one
-- project's files never show up in another's buffer list.
--
--   SPC p p   open a project: its own session (made the first time)
--   SPC S s   switch session                SPC S l   the last session (on any machine)
--   SPC S a   all sessions (:session-list): every machine's, with each
--             one's project and files, the current one marked
--   SPC S n   new session (asks a name)     SPC S r   rename this session
--   SPC S k   kill a session (any machine's) SPC S K   kill it, unsaved files and all
--   SPC S c   connect to another machine (:connect HOST [TARGET]), over
--             ssh: its sessions join this window's
--
-- The switcher lists every machine's sessions: this machine's by name,
-- another's as host:name (and, from another machine, this one's as
-- local:name). :q in another machine's session detaches from it.
--
-- The first project a session opens names it. To open projects in the
-- session you're in instead: vmac.set_session_per_project(false)

-- Sessions as picker rows: name, project, files (and unsaved ones), in
-- columns; the current one marked. Returns the rows, and the session each
-- row stands for. `keep(name)` says which to list.
local function rows(keep)
  local here = vmac.session_name()
  local list = {}
  for _, d in ipairs(vmac.session_details()) do
    if keep(d[1]) then list[#list + 1] = d end
  end
  local name_w, project_w = 0, 0
  for _, d in ipairs(list) do
    name_w = math.max(name_w, utf8.len(d[1]) or #d[1])
    project_w = math.max(project_w, utf8.len(d[2]) or #d[2])
  end
  local pad = function(text, width) return text .. string.rep(" ", width - (utf8.len(text) or #text)) end
  local out, names = {}, {}
  for _, d in ipairs(list) do
    local files = d[3] .. (d[3] == "1" and " file" or " files")
    if d[4] ~= "0" then files = files .. ", " .. d[4] .. " unsaved" end
    local mark = d[1] == here and "● " or "  "
    local row = mark .. pad(d[1], name_w) .. "   " .. pad(d[2], project_w) .. "   " .. files
    out[#out + 1] = row
    names[row] = d[1]
  end
  return out, names
end

-- A picker over sessions; `act` gets the session picked.
local function pick_session(title, keep, act, none)
  local list, names = rows(keep)
  if #list == 0 then return vmac.message(none) end
  vmac.pick(title, list, function(row) act(names[row]) end)
end

local function not_here(name) return name ~= vmac.session_name() end

vmac.command("session-switch-command", function()
  pick_session("Switch session", not_here, vmac.session_switch, "Only one session (SPC p p opens a project in its own)")
end)

-- SPC S a: every session, on every machine, the current one marked.
vmac.command("session-list-command", function()
  pick_session("Sessions", function() return true end, function(name)
    if name ~= vmac.session_name() then vmac.session_switch(name) end
  end, "No sessions")
end)
core.ex_commands["session-list"] = function() core.call_binding("session-list-command") end

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

-- Any machine's session: another's is asked of that machine.
local function kill(force)
  return function()
    pick_session("Kill session", function() return true end, function(name) vmac.session_kill(name, force) end, "No sessions")
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
vmac.map("normal", "SPC S a", "session-list-command")
vmac.map("normal", "SPC S l", "session-last-command")
vmac.map("normal", "SPC S n", "session-new-command")
vmac.map("normal", "SPC S r", "session-rename-command")
vmac.map("normal", "SPC S k", "session-kill-command")
vmac.map("normal", "SPC S K", "session-kill-force-command")
