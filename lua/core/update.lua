-- Updating vmac
--
-- vmac checks for a new release once a day and says when one is out (it
-- never downloads one you didn't ask for). SPC h u or :update installs it:
-- the release for your system, checked against its SHA256SUMS and put in
-- place of the old (a vmac built with `cargo install` is rebuilt by cargo,
-- in a terminal). :update-check checks now.
--
-- Once a newer vmac is installed (by vmac or otherwise), the vmac running
-- waits until nothing would be lost by restarting (no unsaved files, no
-- agent at work, no command running in a terminal), then asks whether to
-- restart onto it. Yes reopens your sessions, their projects and files, on
-- the new vmac; :restart-server asks again later (:restart-server! restarts
-- even with something unsaved or running). Servers on other machines
-- (`vmac connect`) are offered the same once a newer vmac connects to them.
--
-- vmac.set_update_check(false) in init.lua turns the daily check off
-- (VMAC_NO_UPDATE=1 in the environment does too). vmac.version() says
-- which vmac this is.

local V = vmac

-- The newer release out, and the newer vmac waiting to be restarted onto.
core.update_out = nil
core.update_waiting = nil

core.events["update-out"] = function(args)
  core.update_out = args[1]
  V.message("vmac " .. args[1] .. " is out (this is " .. V.version() .. "): SPC h u installs it")
end

core.events["update-latest"] = function(args)
  V.message("vmac " .. args[1] .. " is the latest")
end

core.events["update-failed"] = function(args)
  V.message("Update: " .. args[1])
end

-- A vmac built with cargo: cargo rebuilds it, where you can watch.
core.events["update-run"] = function(args)
  local words = {}
  for word in args[1]:gmatch("%S+") do words[#words + 1] = word end
  local program = table.remove(words, 1)
  if V.terminal_run("*vmac update*", program, words, os.getenv("HOME") or ".") then
    V.message("Building the new vmac with cargo; vmac offers to restart once it's installed")
  end
end

core.events["update-installed"] = function(args)
  core.update_out = nil
  V.message("vmac " .. args[1] .. " is installed")
end

core.events["update-waiting"] = function(args)
  core.update_waiting = args[1]
  V.message("vmac " .. args[1] .. " is installed: it'll offer to restart once nothing's busy (" .. args[2] .. ")")
end

core.events["update-ready"] = function(args)
  core.update_waiting = nil
  core.confirm("vmac " .. args[1] .. " is installed. Restart onto it now (your sessions reopen)?", function()
    V.restart_server(false)
  end, "Not restarted: :restart-server when you're ready")
end

-- In the modeline while a restart waits for something to finish.
function core.update_segment()
  if not core.update_waiting then return "" end
  return "↻ " .. core.update_waiting
end

V.command("update", function() V.update() end)
V.command("update-check", function() V.update_check() end)
V.command("restart-server", function() V.restart_server(false) end)
core.ex_commands["update"] = function() V.update() end
core.ex_commands["update-check"] = function() V.update_check() end
core.ex_commands["restart-server"] = function() V.restart_server(false) end
core.ex_commands["restart-server!"] = function() V.restart_server(true) end
core.ex_commands["version"] = function() V.message("vmac " .. V.version()) end

-- On the dashboard while a release is out.
local default_items = core.default_dashboard_items
function core.default_dashboard_items()
  local items = default_items()
  if core.update_out then
    table.insert(items, 1, { "u", "Update vmac to " .. core.update_out, "update" })
  end
  return items
end
