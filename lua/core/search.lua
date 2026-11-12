-- Searching the project with ripgrep
--
--   SPC s p, SPC /   search the project for a pattern (a regex)
--   SPC *            search the project for the word at the cursor
--   :rg PATTERN      the same, from the : line (:grep too)
--
-- Every match is a line in the picker, `path:line:col  text`, with the
-- file previewed around it; typing narrows them, RET opens the file there.
-- It runs rg (ripgrep: smart case, .gitignore respected, hidden files
-- skipped), and grep -rn when rg isn't installed. In init.lua:
--   vmac.set_search_args({ "--hidden", "--glob", "!.git" })
-- passes more arguments to rg.

local V = vmac

local S = { program = "rg", extra_args = {}, max_results = 5000, next_job = 0, jobs = {} }
core.search = S

function V.set_search_args(args) S.extra_args = args or {} end

-- vmac.spawn's replies are shared: ours by tag, the rest to whoever was
-- listening before.
local previous_spawn_done = core.events["spawn-done"]
core.events["spawn-done"] = function(args)
  local done = S.jobs[args[1]]
  if done then
    S.jobs[args[1]] = nil
    return done(args[2] == "ok", args[3], args[4])
  end
  if previous_spawn_done then return previous_spawn_done(args) end
end

local function spawn(dir, program, args, done)
  S.next_job = S.next_job + 1
  local tag = "search:" .. S.next_job
  S.jobs[tag] = done
  V.spawn(dir, program, args, nil, tag)
end

function S.rg_args(pattern)
  local args = { "--vimgrep", "--smart-case", "--max-columns", "300", "--max-columns-preview" }
  for _, a in ipairs(S.extra_args) do args[#args + 1] = a end
  args[#args + 1] = "-e"
  args[#args + 1] = pattern
  args[#args + 1] = "."
  return args
end

function S.grep_args(pattern)
  return { "-rnIE", "--exclude-dir=.git", "--exclude-dir=node_modules", "--exclude-dir=target", "-e", pattern, "." }
end

-- rg's `./src/a.rs:12:5:text` (or grep's `./src/a.rs:12:text`) as a picker
-- item, `src/a.rs:12:5  text`; nil for a line that isn't a match.
function S.item(line)
  local path, l, c, text = line:match("^(.-):(%d+):(%d+):(.*)$")
  if not path then
    path, l, text = line:match("^(.-):(%d+):(.*)$")
    c = "1"
  end
  if not path then return nil end
  path = path:gsub("^%./", "")
  text = text:gsub("^%s+", ""):gsub("%s+$", "")
  return path .. ":" .. l .. ":" .. c .. "  " .. text
end

function S.items(output)
  local items = {}
  for line in output:gmatch("[^\n]+") do
    local item = S.item(line)
    if item then
      items[#items + 1] = item
      if #items >= S.max_results then break end
    end
  end
  return items
end

local function show(root, pattern, output)
  local items = S.items(output)
  if #items == 0 then return V.message("No matches for " .. pattern) end
  local title = "Search: " .. pattern .. " (" .. #items .. (#items >= S.max_results and "+" or "") .. ")"
  core.pick(title, items, root, function(item) V.goto_location(root .. "/" .. item) end)
end

-- Searches the project (or `root`) for a regex and shows the matches.
function S.run(pattern, root)
  if pattern == "" then return end
  root = root or V.project_root()
  V.message("Searching for " .. pattern .. "...")
  spawn(root, S.program, S.rg_args(pattern), function(ok, out, err)
    -- rg exits 1 for no matches, 2 for errors (it still prints what it found).
    if out ~= "" then return show(root, pattern, out) end
    if err:sub(1, #S.program + 2) == S.program .. ": " and (err:match("No such file") or err:match("not found")) then
      -- No ripgrep here: grep does.
      return spawn(root, "grep", S.grep_args(pattern), function(_, gout, gerr)
        if gout ~= "" then return show(root, pattern, gout) end
        V.message(gerr ~= "" and ("grep: " .. gerr:match("[^\n]*")) or ("No matches for " .. pattern))
      end)
    end
    if not ok and err ~= "" then return V.message(err:match("[^\n]*")) end
    V.message("No matches for " .. pattern)
  end)
end

-- vmac.search_project(pattern, root): the same, from Lua.
function V.search_project(pattern, root) S.run(pattern, root) end

V.command("search-project", function()
  core.read("Search project: ", function(pattern) S.run(pattern) end)
end)

V.command("search-project-word", function()
  local word = V.word_at_point()
  if word == "" then return V.message("No word here") end
  S.run("\\b" .. word .. "\\b")
end)

core.ex_commands["rg"] = function(arg) S.run(arg) end
core.ex_commands["grep"] = function(arg) S.run(arg) end

V.map("normal", "SPC s p", "search-project")
V.map("normal", "SPC /", "search-project")
V.map("normal", "SPC *", "search-project-word")
