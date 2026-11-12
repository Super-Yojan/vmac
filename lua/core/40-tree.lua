-- The file tree (SPC o p)
--
-- A sidebar at the left edge showing the project's folders. It is an
-- ordinary (read-only) buffer, so every motion works in it; its own keys
-- are in core.keymaps.tree. Folders hidden by .gitignore don't show.

local V = vmac

local tree = { root = "", expanded = {}, entries = {} }
core.tree = tree
-- What each line shows, in order: {path, folder, depth}. Line 1 is the root (depth -1).

function core.basename(path) return path:match("([^/]*)$") end

function core.dirname(path)
  local dir = path:match("^(.*)/[^/]*$")
  if not dir or dir == "" then return "/" end
  return dir
end

local function without_slash(s) return (s:gsub("/$", "")) end

local function expanded(dir) return tree.expanded[dir] == true end
local function expand(dir) tree.expanded[dir] = true end
local function collapse(dir) tree.expanded[dir] = nil end

-- Entries under dir, recursing into expanded folders.
local function collect(dir, depth, out)
  for _, name in ipairs(V.directory_entries(dir)) do
    local folder = name:sub(-1) == "/"
    local path = dir .. "/" .. without_slash(name)
    out[#out + 1] = { path, folder, depth }
    if folder and expanded(path) then collect(path, depth + 1, out) end
  end
  return out
end

-- Faces by file extension, so the tree isn't one color.
function core.file_face(name)
  local ext = name:match("%.([^.]*)$") or ""
  if core.contains({ "md", "txt" }, ext) then return "cyan" end
  if core.contains({ "toml", "json", "yaml", "yml", "lock" }, ext) then return "yellow" end
  return "default"
end

local function entry_at_point()
  local line = V.line_number()
  if V.buffer_name() ~= core.tree_buffer then return nil end
  return tree.entries[line]
end

local function goto_path(path)
  for line, e in ipairs(tree.entries) do
    if e[1] == path then
      V.goto_char(core.first_non_blank_at(V.line_to_pos(line - 1)))
      return true
    end
  end
  return false
end

-- What git says changed, by absolute path: a letter per file (M, A, D,
-- R, ?, U) and a dot on each folder holding one. Empty outside a repository.
local function git_marks()
  local marks = {}
  if not core.git then return marks end
  -- The repository's root spelled the tree's way (--show-toplevel resolves
  -- symlinks, so its paths might not match the tree's): the tree's root
  -- less its place in the repository.
  local prefix = core.git.ask(tree.root, { "rev-parse", "--show-prefix" })
  if not prefix then return marks end
  prefix = without_slash(prefix:gsub("%s+$", ""))
  local root = tree.root
  if prefix ~= "" then root = root:sub(1, #root - #prefix - 1) end
  local s = core.git.read_status(root)
  if not s then return marks end
  local function mark(list, letter)
    for _, e in ipairs(list) do
      local path = without_slash(root .. "/" .. e.path)
      if not marks[path] then marks[path] = letter(e) end
      local dir = core.dirname(path)
      while core.starts_with(dir, root) and not marks[dir] do
        marks[dir] = "•"
        dir = core.dirname(dir)
      end
    end
  end
  -- First wins: conflicts, then what's still unstaged, then staged.
  mark(s.unmerged, function() return "U" end)
  mark(s.unstaged, function(e) return e.y end)
  mark(s.staged, function(e) return e.x end)
  mark(s.untracked, function() return "?" end)
  return marks
end

local mark_faces = { M = "yellow", T = "yellow", ["•"] = "yellow", A = "green", ["?"] = "green", R = "green", C = "green", D = "red", U = "red" }

-- Redraws the tree buffer, keeping the cursor on the same path.
local function render()
  local here = entry_at_point()
  tree.entries = collect(tree.root, 0, { { tree.root, true, -1 } })
  local root_name = core.basename(tree.root)
  local lines = { root_name }
  local faces = { { 0, core.length(root_name), "accent" } }
  local offset = core.length(root_name) + 1
  local marks = git_marks()
  for i = 2, #tree.entries do
    local path, folder, depth = table.unpack(tree.entries[i])
    local indent = string.rep(" ", 2 * depth)
    local icon = not folder and "  " or (expanded(path) and "▾ " or "▸ ")
    local name = core.basename(path)
    local start = offset + core.length(indent)
    local name_start = start + core.length(icon)
    local mark = marks[path]
    local name_end = name_start + core.length(name) + (folder and 1 or 0)
    local line = indent .. icon .. name .. (folder and "/" or "") .. (mark and "  " .. mark or "")
    if folder then
      faces[#faces + 1] = { start, name_start, "dim" }
      faces[#faces + 1] = { name_start, name_end, "blue" }
    else
      faces[#faces + 1] = { name_start, name_end, mark and mark_faces[mark] or core.file_face(name) }
    end
    if mark then
      faces[#faces + 1] = { name_end + 2, name_end + 2 + core.length(mark), mark_faces[mark] or "yellow" }
    end
    lines[#lines + 1] = line
    offset = offset + core.length(line) + 1
  end
  V.set_buffer_text(core.tree_buffer, table.concat(lines, "\n"))
  for _, f in ipairs(faces) do V.add_face(core.tree_buffer, f[1], f[2], f[3]) end
  if here then goto_path(here[1]) end
end
tree.render = render

-- Expands every folder between the root and a file, so it shows.
local function reveal(file)
  if not core.starts_with(file, tree.root .. "/") then return end
  local dir = core.dirname(file)
  while core.starts_with(dir, tree.root) and dir ~= tree.root do
    expand(dir)
    dir = core.dirname(dir)
  end
end

V.command("tree-open", function()
  local file, root = V.buffer_file_name(), V.project_root()
  if root ~= tree.root then
    tree.root = root
    tree.expanded = {}
  end
  V.ensure_buffer(core.tree_buffer)
  V.set_buffer_read_only(core.tree_buffer, true)
  V.sidebar_open(core.tree_buffer)
  core.state = "normal"
  reveal(file)
  render()
  if not goto_path(file) then V.goto_char(0) end
  if not tree.hinted then
    tree.hinted = true
    V.message("a new file  r rename  d delete  R refresh  q close")
  end
end)

V.command("tree-close", function() V.sidebar_close() end)

V.command("tree-toggle", function()
  core.call_binding(V.sidebar_shown() and "tree-close" or "tree-open")
end)

-- RET, o: a folder opens or closes; a file opens in the main window.
V.command("tree-activate", function()
  local e = entry_at_point()
  if not e then return end
  if e[3] == -1 then
    core.call_binding("tree-refresh")
  elseif e[2] then
    if expanded(e[1]) then collapse(e[1]) else expand(e[1]) end
    render()
  else
    V.select_main_window()
    V.find_file(e[1])
  end
end)

-- l: open a folder (or a file); h: close the folder, else go to its parent.
V.command("tree-expand", function()
  local e = entry_at_point()
  if not e then return end
  if e[2] and not expanded(e[1]) then
    expand(e[1])
    render()
  else
    core.call_binding("tree-activate")
  end
end)

V.command("tree-collapse", function()
  local e = entry_at_point()
  if not e or e[3] == -1 then return end
  if e[2] and expanded(e[1]) then
    collapse(e[1])
    render()
  else
    goto_path(core.dirname(e[1]))
  end
end)

V.command("tree-refresh", function()
  render()
  V.message("tree refreshed")
end)

-- The folder new things go in: the folder on this line, else the line's folder.
local function target_dir()
  local e = entry_at_point()
  if not e then return tree.root end
  if e[2] then return e[1] end
  return core.dirname(e[1])
end

-- a: a new file (or folder, ending in /) in the folder at the cursor;
-- "src/x.rs" makes the folders on the way. The file opens.
V.command("tree-add", function()
  local dir = target_dir()
  core.read("New file in " .. core.basename(dir) .. "/ (end with / for a folder): ", function(name)
    if name == "" then return end
    local path = dir .. "/" .. name
    if V.create_path(path) then
      expand(dir)
      reveal(without_slash(path))
      render()
      goto_path(without_slash(path))
      if name:sub(-1) == "/" then
        V.message("created " .. name)
      else
        -- A new file opens in the main window, ready to type in.
        V.select_main_window()
        V.find_file(path)
        V.message("created " .. name)
      end
    end
  end)
end)

V.command("tree-rename", function()
  local e = entry_at_point()
  if not e or e[3] == -1 then return end
  core.read("Rename " .. core.basename(e[1]) .. " to: ", function(name)
    if name == "" then return end
    local to = core.dirname(e[1]) .. "/" .. name
    if V.rename_path(e[1], to) then
      render()
      goto_path(to)
      V.message("renamed to " .. name)
    end
  end)
end)

V.command("tree-delete", function()
  local e = entry_at_point()
  if not e or e[3] == -1 then return end
  local what = core.basename(e[1]) .. (e[2] and "/ and everything in it" or "")
  core.read("Delete " .. what .. "? (y/n) ", function(answer)
    if not core.contains({ "y", "yes" }, answer) then return V.message("not deleted") end
    if V.delete_path(e[1]) then
      collapse(e[1])
      render()
      V.message("deleted " .. core.basename(e[1]))
    end
  end)
end)

for key, command in pairs({
  RET = "tree-activate", o = "tree-activate", TAB = "tree-activate",
  l = "tree-expand", h = "tree-collapse",
  a = "tree-add", r = "tree-rename", d = "tree-delete",
  R = "tree-refresh", q = "tree-close",
}) do
  core.define_key("tree", key, command)
end
