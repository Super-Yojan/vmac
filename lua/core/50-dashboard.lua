-- The dashboard and projects
--
-- vmac opens on a dashboard when started without a file (SPC b h shows it
-- again): what to do first, and the projects you opened lately. Each line's
-- key runs it, as does RET on it; 1-9 open a project.
--
-- A project is a folder with .git in it. vmac remembers the projects and
-- files you open (~/.local/state/vmac). SPC p p switches project: pick one
-- (those opened before, then repositories found in ~/git, ~/code,
-- ~/projects, ~/src, ~/dev, ~/work), and the file picker opens in it. SPC p
-- a opens any folder by path. SPC f r reopens a recent file. :project PATH
-- does what SPC p a does.
--
-- Change what it shows by redefining core.dashboard_banner (lines) and
-- core.dashboard_items ({{key, label, command}, ...}), e.g. in init.lua:
--   function core.dashboard_items()
--     local items = core.default_dashboard_items()
--     table.insert(items, 1, { "t", "Terminal", "terminal-toggle" })
--     return items
--   end
-- core.dashboard_on_start = false turns it off at startup.

local V = vmac

core.dashboard_on_start = true

function core.dashboard_banner()
  return {
    "█   █  █   █   ███    ████",
    "█   █  ██ ██  █   █  █    ",
    "█   █  █ █ █  █████  █    ",
    " █ █   █   █  █   █  █    ",
    "  █    █   █  █   █   ████",
  }
end

function core.default_dashboard_items()
  return {
    { "f", "Find file", "find-file-command" },
    { "r", "Recent files", "recent-files-command" },
    { "p", "Switch project", "project-switch" },
    { "o", "Open a folder", "project-add" },
    { "a", "Ask the agent", "agent-ask" },
    { "s", "Agent sessions", "agent-history-all" },
    { "c", "Open config", "open-config" },
    { "q", "Quit", "quit" },
  }
end

function core.dashboard_items() return core.default_dashboard_items() end

V.command("dashboard", function()
  local items = core.dashboard_items()
  -- Each item's key runs it here; 1-9 open the projects listed.
  local map = {}
  core.keymaps.dashboard = map
  for _, item in ipairs(items) do
    local command = item[3]
    map[item[1]] = function() core.call_binding(command) end
  end
  for n = 1, 9 do
    map[tostring(n)] = function()
      local path = V.dashboard_project(n)
      if path ~= "" then core.project_switch_to(path) end
    end
  end
  map.RET = "dashboard-run"
  V.dashboard_show(core.dashboard_banner(), items)
end)

-- RET: the line's item or project.
V.command("dashboard-run", function()
  local action = V.dashboard_action()
  if action == "" then return end
  if core.starts_with(action, "project:") then
    core.project_switch_to(action:sub(9))
  else
    core.call_binding(action)
  end
end)

-- Opens a folder as the project, then the file picker in it.
function core.project_switch_to(path)
  local root = V.project_open(path)
  if root ~= "" then core.find_file_in(root) end
end

V.command("project-switch", function()
  local projects = V.project_candidates()
  if #projects == 0 then return V.message("No projects yet: SPC p a opens a folder") end
  core.pick("Switch project", projects, "", core.project_switch_to)
end)

V.command("project-add", function()
  core.read("Open folder: ", core.project_switch_to)
  V.insert("~/")
end)

V.command("project-forget-command", function()
  local projects = {}
  for _, p in ipairs(V.known_projects()) do projects[#projects + 1] = V.abbreviate_path(p) end
  core.pick("Forget project", projects, "", function(p)
    V.project_forget(p)
    V.message("Forgot " .. p)
  end)
end)

V.command("recent-files-command", function()
  local files = {}
  for _, f in ipairs(V.recent_files()) do files[#files + 1] = V.abbreviate_path(f) end
  if #files == 0 then return V.message("No recent files") end
  core.pick("Recent files", files, "", function(f) V.find_file(f) end)
end)

V.command("open-config", function() V.find_file(V.config_dir() .. "/init.lua") end)
