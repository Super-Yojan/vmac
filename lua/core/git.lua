-- Git, the magit way: a status buffer to stage, commit, branch, push and
-- pull from, and in files the lines that changed and who changed them.
--
-- SPC g g opens *git-status* for the current file's repository:
--
--   Head:     main  Fix the parser
--   Upstream: origin/main  ↑1
--
--   Unstaged changes (2)
--   ▸ modified   src/lib.rs
--   ▾ modified   README.md
--     @@ -10,6 +10,7 @@
--     ...
--   Staged changes (1)
--   Recent commits
--
-- In it, on a section, a file or one hunk of it:
--   TAB  fold or unfold it       RET  visit it (the file, at the hunk's line;
--                                     a commit, a stash: show it)
--   s    stage it                u    unstage it
--   x    discard it (asks)       S / U  stage / unstage everything
--        (in visual state, on lines of a hunk: s, u and x do just those)
--   c c  commit  c a  amend  c e  amend without editing the message
--   b b  switch branch  b c  new branch  b k  delete a branch
--   P p  push  F p  pull  f f  fetch    (in the background)
--   z z  stash  z p  pop  z a  apply  z k  drop (the stash under the cursor)
--   l l  log  l a  all branches' log    g r  refresh    q  close
--   @ p  pull requests  @ i  issues  @ n  new pull request  @ v  this
--        branch's pull request (see github.lua)
--   m m  merge a branch           r u  rebase onto the upstream
--
-- On a commit (here, in the log, a commit's page, the blame):
--   A    cherry-pick it          _    revert it (a new commit undoing it)
--   X s / X m / X h   reset to it: soft, mixed, hard (asks)
--   r i  rebase interactively from it: *git-rebase* lists the commits,
--        p r e s f d pick / reword / edit / squash / fixup / drop the one
--        under the cursor, C-j / C-k move it, C-c C-c rebases.
-- When git stops (conflicts, an edit), the status says so: fix and stage,
-- then r r continues, r s skips, r a aborts (a merge: commit with c c).
--
-- A commit's page (*git-show*) is drawn with gpui-kit's Diff, its message
-- above; RET on a change visits it. :git-diff-view-toggle draws it as text.
--
-- The commit message is a buffer (*git-commit*): write it, then C-c C-c
-- (or Z Z) commits and C-c C-k cancels. Lines starting with # are left out.
--
-- In a file (GitLens's share):
--   gutter   green: added, blue: changed, a red notch: lines deleted (all
--            against the index, so staging a change clears its mark)
--   ] c  [ c the next / previous change     SPC g s  stage the change here
--   SPC g r  reset the change here (u undoes it)
--   The cursor's line says who last changed it, and when (SPC g B turns
--   that off); SPC g v shows that commit, SPC g b the whole file's blame.
--   SPC g l  this file's history    SPC g L  the history of this line (or
--            of the selected lines)
--   SPC g o  open the line (or selection) on GitHub/GitLab; SPC g y copies
--            the link
--
-- It's all git commands, run with vmac.git(root, args, stdin), which
-- returns ok, output, errors; vmac.git_async(root, args, tag) runs one in
-- the background and raises core.events["git-done"].

local V = vmac
local G = {}
core.git = G

G.status_buffer = "*git-status*"
G.commit_buffer = "*git-commit*"
G.log_buffer = "*git-log*"
G.show_buffer = "*git-show*"
G.blame_buffer = "*git-blame*"
G.rebase_buffer = "*git-rebase*"
-- A commit's changes drawn with gpui-kit's Diff (side by side or unified,
-- word-level); false: as colored text.
G.diff_view = true

---- Running git

-- The repository of the current file (or of the session's folder); in a
-- git buffer, the one it shows.
function G.root()
  local root = G.root_here()
  if root == "" then
    V.message("Not in a git repository")
    return nil
  end
  return root
end

-- The same, quietly: "" outside a repository (for the modeline).
function G.root_here()
  local shown = ({
    [G.status_buffer] = G.status_root, [G.log_buffer] = G.log_root, [G.show_buffer] = G.show_root,
    [G.blame_buffer] = G.blame_root, [G.rebase_buffer] = G.rebase_root, [G.commit_buffer] = G.commit_root,
  })[V.buffer_name()]
  if not shown and core.github and core.github.repo_root and core.github.in_buffer() then shown = core.github.repo_root end
  return shown or V.git_root()
end

-- Runs git: its output, or nil after saying what went wrong.
function G.run(root, args, stdin)
  local ok, out, err = V.git(root, args, stdin)
  if not ok then
    V.message("git " .. (args[1] or "") .. ": " .. (err:match("[^\n]*$") ~= "" and err:match("[^\n]*$") or err))
    return nil
  end
  return out
end

-- Its output whatever happened (for questions that may fail: no upstream...).
function G.ask(root, args)
  local ok, out = V.git(root, args, nil)
  return ok and out or nil, ok
end

local function lines_of(text)
  local out = {}
  for line in (text or ""):gmatch("([^\n]*)\n?") do out[#out + 1] = line end
  if out[#out] == "" then out[#out] = nil end
  return out
end

local function split(s, sep)
  local out = {}
  for piece in (s .. sep):gmatch("(.-)" .. sep) do out[#out + 1] = piece end
  return out
end

-- Background jobs: what to do when each finishes, by tag.
local jobs, next_job = {}, 0
function G.async(root, args, busy, done)
  next_job = next_job + 1
  local tag = tostring(next_job)
  jobs[tag] = done
  V.message(busy)
  V.git_async(root, args, tag)
end

core.events["git-done"] = function(args)
  local tag, ok, out, err = args[1], args[2] == "ok", args[3], args[4]
  local done = jobs[tag]
  jobs[tag] = nil
  if done then done(ok, out, err) end
end

---- A buffer drawn from lines with faces

-- `lines`: { text, faces = { {from, to, face}, ... } } (columns from 0).
-- Writes them into the buffer called `name`, keeping the cursor's line.
local function draw(name, lines, keep_line)
  V.ensure_buffer(name)
  local text, faces, offset = {}, {}, 0
  for i, l in ipairs(lines) do
    text[i] = l.text
    for _, f in ipairs(l.faces or {}) do
      faces[#faces + 1] = { offset + f[1], offset + f[2], f[3] }
    end
    offset = offset + core.length(l.text) + 1
  end
  V.set_buffer_read_only(name, false)
  V.set_buffer_text(name, table.concat(text, "\n") .. "\n")
  V.set_buffer_read_only(name, true)
  for _, f in ipairs(faces) do V.add_face(name, f[1], f[2], f[3]) end
  if keep_line and V.buffer_name() == name then
    V.goto_line_column(math.min(keep_line, #lines), 1)
  end
end

local function line(text, face)
  return { text = text, faces = face and { { 0, core.length(text), face } } or {} }
end

-- A line in pieces: { {text, face}, ... }.
local function pieces(list)
  local text, faces, col = "", {}, 0
  for _, p in ipairs(list) do
    local n = core.length(p[1])
    if p[2] then faces[#faces + 1] = { col, col + n, p[2] } end
    text, col = text .. p[1], col + n
  end
  return { text = text, faces = faces }
end

-- A diff's line, colored: + green, - red, @@ cyan.
local function diff_line(text, indent)
  indent = indent or ""
  local c = text:sub(1, 1)
  local face = (text:sub(1, 2) == "@@" and "cyan") or (c == "+" and "green") or (c == "-" and "red") or nil
  return pieces({ { indent }, { text, face } })
end

---- The status buffer

-- What each line of the status buffer is: { kind = "section" | "file" |
-- "hunk" | "commit" | "stash", section, path, hunk (1-based) }.
G.items = {}
-- Folded sections and unfolded files, by key.
G.folded = { recent = false }
G.open = {}
G.status_root = nil

local labels = {
  M = "modified", A = "new file", D = "deleted", R = "renamed", C = "copied",
  T = "typechange", U = "unmerged",
}

-- git status, read: branch, upstream, ahead, behind and the files.
function G.read_status(root)
  local out = G.run(root, { "status", "--porcelain=v1", "-z", "--branch", "--untracked-files=all" })
  if not out then return nil end
  local s = { untracked = {}, unstaged = {}, staged = {}, unmerged = {} }
  local fields = split(out, "\0")
  local i = 1
  while i <= #fields do
    local f = fields[i]
    if f:sub(1, 3) == "## " then
      local head = f:sub(4)
      s.ahead = tonumber(head:match("ahead (%d+)")) or 0
      s.behind = tonumber(head:match("behind (%d+)")) or 0
      local branch, upstream = head:match("^(.-)%.%.%.(%S+)")
      s.branch = branch or head:match("^No commits yet on (%S+)") or head:match("^(%S+)")
      s.upstream = upstream
      s.unborn = head:match("^No commits yet") ~= nil
    elseif #f > 3 then
      local x, y, path = f:sub(1, 1), f:sub(2, 2), f:sub(4)
      local entry = { path = path, x = x, y = y }
      if x == "R" or x == "C" then
        i = i + 1
        entry.from = fields[i]
      end
      if x == "?" then
        table.insert(s.untracked, entry)
      elseif x == "U" or y == "U" or (x == "A" and y == "A") or (x == "D" and y == "D") then
        table.insert(s.unmerged, entry)
      else
        if x ~= " " then table.insert(s.staged, entry) end
        if y ~= " " then table.insert(s.unstaged, entry) end
      end
    end
    i = i + 1
  end
  return s
end

local function commits(root, args)
  local out = G.ask(root, { "log", "--format=%h%x1f%s%x1f%D", table.unpack(args) })
  local list = {}
  for _, l in ipairs(lines_of(out)) do
    local hash, subject, refs = l:match("^([^\31]*)\31([^\31]*)\31(.*)$")
    if hash then list[#list + 1] = { hash = hash, subject = subject, refs = refs } end
  end
  return list
end

local function commit_line(c, indent)
  local parts = { { indent or "" }, { c.hash, "git-hash" }, { " " } }
  if c.refs and c.refs ~= "" then
    parts[#parts + 1] = { "(" .. c.refs .. ") ", "git-ref" }
  end
  parts[#parts + 1] = { c.subject }
  return pieces(parts)
end

function G.refresh(keep)
  local root = G.status_root
  if not root then return end
  local s = G.read_status(root)
  if not s then return end
  G.status = s
  local out, items = {}, {}
  local function add(l, item)
    out[#out + 1] = l
    items[#out] = item
  end

  local head = s.unborn and "(no commits yet)" or (G.ask(root, { "log", "-1", "--format=%s" }) or ""):gsub("\n$", "")
  add(pieces({ { "Head:     ", "git-label" }, { s.branch or "?", "git-branch" }, { "  " .. head } }))
  if s.upstream then
    local where = {}
    if s.ahead > 0 then where[#where + 1] = "↑" .. s.ahead end
    if s.behind > 0 then where[#where + 1] = "↓" .. s.behind end
    local state = #where > 0 and table.concat(where, " ") or "up to date"
    add(pieces({ { "Upstream: ", "git-label" }, { s.upstream, "git-ref" }, { "  " .. state, "dim" } }))
  end
  local pr = core.github and core.github.status_line(root, s.branch)
  if pr then add(pr) end
  G.op = G.operation(root)
  if G.op then
    local keys = G.op.kind == "merge" and "  c c commit · r a abort" or "  r r continue · r s skip · r a abort"
    add(pieces({ { "Doing:    ", "git-label" }, { G.op.title, "yellow" }, { keys, "dim" } }))
  end
  add(line(""))

  local function section(key, title, n)
    local fold = G.folded[key] and "▸ " or "▾ "
    add(pieces({ { fold .. title, "git-section" }, { " (" .. n .. ")", "dim" } }), { kind = "section", section = key })
    return not G.folded[key]
  end

  local function files(key, title, list)
    if #list == 0 then return end
    if section(key, title, #list) then
      for _, e in ipairs(list) do
        local code = (key == "staged") and e.x or e.y
        local label = key == "untracked" and "" or (labels[code] or code)
        local open_key = key .. ":" .. e.path
        local shown = e.from and (e.from .. " → " .. e.path) or e.path
        local fold = (key ~= "untracked" and key ~= "unmerged") and (G.open[open_key] and "▾ " or "▸ ") or "  "
        add(pieces({ { fold, "dim" }, { string.format("%-11s", label), "git-label" }, { shown } }),
          { kind = "file", section = key, path = e.path })
        if G.open[open_key] and (key == "unstaged" or key == "staged") then
          local hunks = V.git_hunks(root, e.path, key == "staged")
          if #hunks == 0 then add(line("    (no text changes: binary, mode or deleted)", "dim")) end
          for n, h in ipairs(hunks) do
            for j, hl in ipairs(h) do
              add(diff_line(hl, "    "), { kind = "hunk", section = key, path = e.path, hunk = n, row = j, header = h[1] })
            end
          end
        end
      end
    end
    add(line(""))
  end

  files("unmerged", "Unmerged (fix the conflicts, then stage)", s.unmerged)
  files("untracked", "Untracked files", s.untracked)
  files("unstaged", "Unstaged changes", s.unstaged)
  files("staged", "Staged changes", s.staged)

  local stashes = {}
  for _, l in ipairs(lines_of(G.ask(root, { "stash", "list", "--format=%gd%x1f%s" }))) do
    local ref, subject = l:match("^([^\31]*)\31(.*)$")
    if ref then stashes[#stashes + 1] = { ref = ref, subject = subject } end
  end
  if #stashes > 0 then
    if section("stashes", "Stashes", #stashes) then
      for _, st in ipairs(stashes) do
        add(pieces({ { "  " }, { st.ref, "git-hash" }, { " " .. st.subject } }), { kind = "stash", ref = st.ref })
      end
    end
    add(line(""))
  end

  if not s.unborn then
    if s.upstream and s.ahead > 0 then
      local list = commits(root, { s.upstream .. "..HEAD" })
      if section("unpushed", "Unpushed to " .. s.upstream, #list) then
        for _, c in ipairs(list) do add(commit_line(c, "  "), { kind = "commit", hash = c.hash }) end
      end
      add(line(""))
    end
    if s.upstream and s.behind > 0 then
      local list = commits(root, { "HEAD.." .. s.upstream })
      if section("unpulled", "Unpulled from " .. s.upstream, #list) then
        for _, c in ipairs(list) do add(commit_line(c, "  "), { kind = "commit", hash = c.hash }) end
      end
      add(line(""))
    end
    local list = commits(root, { "-n", "10" })
    if section("recent", "Recent commits", #list) then
      for _, c in ipairs(list) do add(commit_line(c, "  "), { kind = "commit", hash = c.hash }) end
    end
    add(line(""))
  end

  add(line("s stage · u unstage · x discard · c c commit · P p push · F p pull · b b branch · l l log · z z stash · TAB fold · RET visit · q quit", "dim"))
  G.items = items
  draw(G.status_buffer, out, keep and V.buffer_name() == G.status_buffer and V.line_number() or nil)
end

-- SPC g g: the status of the current file's repository.
function G.status_open()
  local root = G.root()
  if not root then return end
  if V.buffer_name() ~= G.status_buffer then G.return_to = V.buffer_name() end
  if G.status_root ~= root then G.open = {} end
  G.status_root = root
  V.ensure_buffer(G.status_buffer)
  V.switch_to_buffer(G.status_buffer)
  G.refresh(false)
  -- On the first change, else the top.
  for i = 1, #G.items + 5 do
    local it = G.items[i]
    if it and it.kind == "file" then
      V.goto_line_column(i, 1)
      return
    end
  end
  V.goto_line_column(1, 1)
end

-- Goes back to the buffer the git buffer was opened from.
function G.close()
  if G.return_to and V.switch_to_buffer(G.return_to) then return end
  V.next_buffer(1)
end

function G.item() return G.items[V.line_number()] end

-- Any of the status buffer's files changed: redraw, and let files know.
local function changed()
  V.git_forget()
  if G.status_root and V.buffer_name() == G.status_buffer then G.refresh(true) end
end
G.changed = changed

function G.toggle()
  local it = G.item()
  if not it then return end
  if it.kind == "section" then
    G.folded[it.section] = not G.folded[it.section]
  elseif it.kind == "file" or it.kind == "hunk" then
    local key = it.section .. ":" .. it.path
    G.open[key] = not G.open[key]
    if it.kind == "hunk" then
      -- Folding from inside a hunk: back to its file's line.
      for i = V.line_number(), 1, -1 do
        local up = G.items[i]
        if up and up.kind == "file" and up.path == it.path then
          G.refresh(false)
          V.goto_line_column(i, 1)
          return
        end
      end
    end
  end
  G.refresh(true)
end

-- The paths of a section's files (to act on all of them).
local function section_paths(key)
  local out = {}
  for _, e in ipairs((G.status or {})[key] or {}) do out[#out + 1] = e.path end
  return out
end

local function with_paths(args, paths)
  local all = { table.unpack(args) }
  all[#all + 1] = "--"
  for _, p in ipairs(paths) do all[#all + 1] = p end
  return all
end

-- The first and last lines of the visual selection (leaving visual state).
function G.selected_lines()
  local b = core.visual_bounds()
  local save = V.point()
  V.goto_char(b[1])
  local from = V.line_number()
  V.goto_char(math.max(b[1], b[2] - 1))
  local to = V.line_number()
  V.goto_char(save)
  core.state = "normal"
  return from, to
end

-- The selected rows of one hunk in the status buffer: its item and the rows
-- (1-based, after its @@ line); nil after saying why not.
local function selected_rows()
  local from, to = G.selected_lines()
  local first, rows = nil, {}
  for i = from, to do
    local it = G.items[i]
    if not it or it.kind ~= "hunk" or (first and (it.path ~= first.path or it.hunk ~= first.hunk or it.section ~= first.section)) then
      V.message("Select lines inside one hunk (TAB on a file shows its hunks)")
      return nil
    end
    first = first or it
    if it.row > 1 then rows[#rows + 1] = it.row - 1 end
  end
  return first, rows
end

-- s, u or x on selected lines of a hunk: just those lines.
local function on_lines(action)
  local it, rows = selected_rows()
  if not it then return end
  local wrong = (action == "unstage") ~= (it.section == "staged")
  if wrong then return V.message(action == "unstage" and "Not staged" or "Those lines are staged (u unstages them)") end
  local function go()
    local err = V.git_hunk_apply(G.status_root, it.path, it.hunk, action, rows)
    if err ~= "" then V.message(err) end
    changed()
  end
  if action == "discard" then core.confirm("Discard the selected lines?", go) else go() end
end
G.stage_lines = function() on_lines("stage") end
G.unstage_lines = function() on_lines("unstage") end
G.discard_lines = function() on_lines("discard") end

function G.stage()
  local it, root = G.item(), G.status_root
  if not it then return end
  if it.section == "staged" then return V.message("Already staged (u unstages)") end
  if it.kind == "hunk" then
    local err = V.git_hunk_apply(root, it.path, it.hunk, "stage")
    if err ~= "" then V.message(err) end
  elseif it.kind == "file" then
    G.run(root, with_paths({ "add" }, { it.path }))
  elseif it.kind == "section" and core.contains({ "untracked", "unstaged", "unmerged" }, it.section) then
    G.run(root, with_paths({ "add" }, section_paths(it.section)))
  else
    return
  end
  changed()
end

local function unstage_paths(root, paths)
  if G.status and G.status.unborn then
    return G.run(root, with_paths({ "rm", "--cached", "-q", "-r" }, paths))
  end
  return G.run(root, with_paths({ "reset", "-q", "HEAD" }, paths))
end

function G.unstage()
  local it, root = G.item(), G.status_root
  if not it then return end
  if it.section ~= "staged" then return V.message("Not staged") end
  if it.kind == "hunk" then
    local err = V.git_hunk_apply(root, it.path, it.hunk, "unstage")
    if err ~= "" then V.message(err) end
  elseif it.kind == "file" then
    unstage_paths(root, { it.path })
  elseif it.kind == "section" then
    unstage_paths(root, section_paths("staged"))
  end
  changed()
end

function G.stage_all()
  if G.run(G.status_root, { "add", "-A" }) then changed() end
end

function G.unstage_all()
  if unstage_paths(G.status_root, { "." }) then changed() end
end

function G.discard()
  local it, root = G.item(), G.status_root
  if not it then return end
  if it.section == "staged" then return V.message("Unstage it first (u), then discard") end
  if it.kind == "hunk" and it.section == "unstaged" then
    core.confirm("Discard this change?", function()
      local err = V.git_hunk_apply(root, it.path, it.hunk, "discard")
      if err ~= "" then V.message(err) end
      changed()
    end)
  elseif it.kind == "file" and it.section == "untracked" then
    core.confirm("Delete " .. it.path .. "?", function()
      if G.run(root, { "clean", "-f", "-q", "--", it.path }) then changed() end
    end)
  elseif it.kind == "file" and it.section == "unstaged" then
    core.confirm("Discard the changes to " .. it.path .. "?", function()
      if G.run(root, { "checkout", "-q", "--", it.path }) then changed() end
    end)
  elseif it.kind == "section" and it.section == "unstaged" then
    core.confirm("Discard all unstaged changes?", function()
      if G.run(root, with_paths({ "checkout", "-q" }, section_paths("unstaged"))) then changed() end
    end)
  elseif it.kind == "stash" then
    G.stash_drop()
  end
end

-- RET: a file (at the hunk's line), a commit, a stash.
function G.visit()
  local it, root = G.item(), G.status_root
  if not it then return end
  if it.kind == "section" then return G.toggle() end
  if it.kind == "commit" then return G.show(root, it.hash) end
  if it.kind == "stash" then return G.show(root, it.ref, true) end
  local target = 1
  if it.kind == "hunk" then
    -- The hunk's first line, then a line for each row down to the cursor
    -- that's on the file's side (context and added rows, not removed ones).
    target = math.max(1, tonumber(it.header:match("%+(%d+)")) or 1)
    local all = lines_of(V.buffer_string())
    local here_line = V.line_number()
    for i = here_line - it.row + 2, here_line - 1 do
      if not (all[i] or ""):match("^%s*%-") then target = target + 1 end
    end
  end
  V.find_file(root .. "/" .. it.path)
  V.goto_line_column(target, 1)
end

---- Committing

local commit_amend = false

local commit_help = "# Write the message above. C-c C-c (or Z Z) commits, C-c C-k cancels.\n"
  .. "# Lines starting with # are left out."

function G.commit(amend)
  local root = G.status_root or G.root()
  if not root then return end
  G.commit_root = root
  local s = G.read_status(root)
  if not s then return end
  if not amend and #s.staged == 0 then
    if #s.unstaged == 0 and #s.untracked == 0 then return V.message("Nothing to commit") end
    return core.confirm("Nothing staged: stage everything and commit?", function()
      if G.run(root, { "add", "-A" }) then
        changed()
        G.commit(false)
      end
    end)
  end
  commit_amend = amend
  local message = ""
  if amend then message = (G.ask(root, { "log", "-1", "--format=%B" }) or ""):gsub("%s+$", "") end
  local summary = { "", commit_help, "#", "# On " .. (s.branch or "?") .. (amend and ", amending the last commit" or "") .. ". To commit:" }
  for _, e in ipairs(s.staged) do
    summary[#summary + 1] = "#   " .. string.format("%-11s", labels[e.x] or e.x) .. e.path
  end
  if V.buffer_name() ~= G.commit_buffer and V.buffer_name() ~= G.status_buffer then G.return_to = V.buffer_name() end
  V.ensure_buffer(G.commit_buffer)
  V.set_buffer_read_only(G.commit_buffer, false)
  V.set_buffer_text(G.commit_buffer, message .. "\n" .. table.concat(summary, "\n") .. "\n")
  V.switch_to_buffer(G.commit_buffer)
  -- The help and summary, dim.
  local from = core.length(message) + 1
  V.add_face(G.commit_buffer, from, from + core.length(table.concat(summary, "\n")) + 1, "dim")
  V.goto_line_column(1, 1)
  if message ~= "" then
    V.goto_char(core.length(message:match("^[^\n]*")))
  end
  core.state = "insert"
end

function G.commit_finish()
  local root = G.commit_root
  local kept = {}
  for _, l in ipairs(lines_of(V.buffer_string())) do
    if l:sub(1, 1) ~= "#" then kept[#kept + 1] = l end
  end
  local message = table.concat(kept, "\n"):gsub("^%s+", ""):gsub("%s+$", "")
  if message == "" then return V.message("The message is empty: write one, or C-c C-k to cancel") end
  local args = { "commit", "-q", "-F", "-" }
  if commit_amend then args[#args + 1] = "--amend" end
  local ok, _, err = V.git(root, args, message .. "\n")
  if not ok then return V.message("git commit: " .. (err:match("[^\n]+$") or err)) end
  core.state = "normal"
  V.set_buffer_text(G.commit_buffer, "")
  V.git_forget()
  local head = (G.ask(root, { "log", "-1", "--format=%h %s" }) or ""):gsub("\n$", "")
  if G.status_root == root and V.switch_to_buffer(G.status_buffer) then
    G.refresh(false)
  else
    G.close()
  end
  V.message((commit_amend and "Amended: " or "Committed: ") .. head)
end

function G.commit_cancel()
  core.state = "normal"
  V.set_buffer_text(G.commit_buffer, "")
  if not (G.status_root and V.switch_to_buffer(G.status_buffer)) then G.close() end
  V.message("Not committed")
end

function G.commit_extend()
  local root = G.status_root or G.root()
  if root and G.run(root, { "commit", "-q", "--amend", "--no-edit" }) then
    changed()
    V.message("Amended the last commit")
  end
end

---- Branches

local function branches(root, remotes)
  local refs = { "refs/heads" }
  if remotes then refs[2] = "refs/remotes" end
  local out = G.ask(root, { "for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", table.unpack(refs) })
  local list = {}
  for _, b in ipairs(lines_of(out)) do
    if not b:match("/HEAD$") and b ~= "origin" then list[#list + 1] = b end
  end
  return list
end

function G.switch_branch()
  local root = G.status_root or G.root()
  if not root then return end
  core.pick("Switch to branch", branches(root, true), "", function(b)
    local remote, name = b:match("^([^/]+)/(.+)$")
    local local_branches = branches(root, false)
    local args = { "switch", "-q", b }
    -- A remote branch with no local one: a local branch tracking it.
    if remote and not core.contains(local_branches, b) and not core.contains(local_branches, name) then
      args = { "switch", "-q", "--track", b }
    elseif remote and core.contains(local_branches, name) then
      args = { "switch", "-q", name }
    end
    if G.run(root, args) then
      changed()
      V.message("On " .. V.git_branch(root))
    end
  end)
end

function G.new_branch()
  local root = G.status_root or G.root()
  if not root then return end
  V.prompt("New branch (from " .. V.git_branch(root) .. "): ", function(name)
    if name == "" then return end
    if G.run(root, { "switch", "-q", "-c", name }) then
      changed()
      V.message("On a new branch, " .. name)
    end
  end)
end

function G.delete_branch()
  local root = G.status_root or G.root()
  if not root then return end
  core.pick("Delete branch", branches(root, false), "", function(b)
    core.confirm("Delete the branch " .. b .. "?", function()
      local ok, _, err = V.git(root, { "branch", "-d", b }, nil)
      if ok then
        changed()
        return V.message("Deleted " .. b)
      end
      if err:match("not fully merged") then
        core.confirm(b .. " isn't merged. Delete it anyway?", function()
          if G.run(root, { "branch", "-D", b }) then changed() end
        end)
      else
        V.message(err)
      end
    end)
  end)
end

---- Remotes: in the background, the status redrawn when done

local function last_line(text)
  local last = ""
  for l in (text or ""):gmatch("[^\n]+") do if l:match("%S") then last = l end end
  return last
end

local function remote_job(args, busy, done_message)
  local root = G.status_root or G.root()
  if not root then return end
  G.async(root, args, busy, function(ok, out, err)
    if ok then
      V.message(done_message)
      -- The branch's pull request may have changed (pushed, new checks).
      if core.github then core.github.branch_prs = {} end
    else
      V.message(args[1] .. " failed: " .. last_line(err ~= "" and err or out))
    end
    if V.buffer_name() == G.status_buffer then G.refresh(true) end
  end)
end

function G.push()
  local root = G.status_root or G.root()
  if not root then return end
  local s = G.read_status(root) or {}
  if s.upstream then
    remote_job({ "push" }, "Pushing to " .. s.upstream .. "…", "Pushed")
  else
    -- No upstream yet: origin's branch of the same name.
    remote_job({ "push", "-u", "origin", "HEAD" }, "Pushing " .. (s.branch or "HEAD") .. " to origin…", "Pushed (and tracking origin)")
  end
end

function G.pull() remote_job({ "pull" }, "Pulling…", "Pulled") end
function G.fetch() remote_job({ "fetch", "--all", "--prune" }, "Fetching…", "Fetched") end

---- Stashes

function G.stash()
  local root = G.status_root or G.root()
  if not root then return end
  V.prompt("Stash message: ", function(msg)
    local args = { "stash", "push", "-q", "--include-untracked" }
    if msg ~= "" then
      args[#args + 1] = "-m"
      args[#args + 1] = msg
    end
    if G.run(root, args) then
      changed()
      V.message("Stashed")
    end
  end)
end

local function stash_here()
  local it = G.item()
  return it and it.kind == "stash" and it.ref or "stash@{0}"
end

function G.stash_pop()
  local ref = stash_here()
  if G.run(G.status_root, { "stash", "pop", "-q", ref }) then
    changed()
    V.message("Popped " .. ref)
  end
end

function G.stash_apply()
  local ref = stash_here()
  if G.run(G.status_root, { "stash", "apply", "-q", ref }) then
    changed()
    V.message("Applied " .. ref)
  end
end

function G.stash_drop()
  local ref = stash_here()
  core.confirm("Drop " .. ref .. "?", function()
    if G.run(G.status_root, { "stash", "drop", "-q", ref }) then changed() end
  end)
end

---- The log and commits

G.log_items = {}

-- *git-log*: commits with their graph. `args`: more for git log (--all,
-- -- path...). RET on one shows it.
function G.log(root, args, title)
  root = root or G.root()
  if not root then return end
  local all = { "log", "--graph", "--color=never", "--format=%x1f%h%x1f%D%x1f%s%x1f%an%x1f%ar", "-n", "500" }
  for _, a in ipairs(args or {}) do all[#all + 1] = a end
  local out = G.run(root, all)
  if not out then return end
  local lines, items = { line(title or ("Log of " .. V.git_branch(root)), "git-section"), line("") }, {}
  for _, l in ipairs(lines_of(out)) do
    local graph, hash, refs, subject, author, ago = l:match("^([^\31]*)\31([^\31]*)\31([^\31]*)\31([^\31]*)\31([^\31]*)\31(.*)$")
    if hash then
      local parts = { { graph, "dim" }, { hash, "git-hash" }, { " " } }
      if refs ~= "" then parts[#parts + 1] = { "(" .. refs .. ") ", "git-ref" } end
      parts[#parts + 1] = { subject }
      parts[#parts + 1] = { "  " .. author .. ", " .. ago, "dim" }
      lines[#lines + 1] = pieces(parts)
      items[#lines] = hash
    else
      lines[#lines + 1] = line(l, "dim")
    end
  end
  if #lines == 2 then lines[#lines + 1] = line("No commits", "dim") end
  if not core.contains({ G.log_buffer, G.show_buffer, G.status_buffer }, V.buffer_name()) then G.return_to = V.buffer_name() end
  G.log_root, G.log_items = root, items
  draw(G.log_buffer, lines)
  V.switch_to_buffer(G.log_buffer)
  V.goto_line_column(3, 1)
end

function G.log_visit()
  local hash = G.log_items[V.line_number()]
  if hash then G.show(G.log_root, hash) end
end

-- *git-show*: a commit (or stash): its message, files and diff.
function G.show(root, rev, stash)
  local args = stash and { "stash", "show", "-p", "--stat", "--include-untracked", rev }
    or { "show", "--stat", "--patch", "--format=fuller", "--color=never", rev }
  local out = G.run(root, args)
  if not out then return end
  G.show_text(root, out, rev, true)
end

-- Draws diff-ish text (git show, git log -L) into *git-show*; `as_diff`:
-- drawn with gpui-kit's Diff (one commit's changes), else as text.
function G.show_text(root, out, title, as_diff)
  V.set_buffer_diff(G.show_buffer, as_diff and G.diff_view and out:find("diff %-%-git") ~= nil or false)
  local lines = {}
  for _, l in ipairs(lines_of(out)) do
    if l:match("^commit ") then
      lines[#lines + 1] = pieces({ { "commit ", "git-label" }, { l:sub(8), "git-hash" } })
    elseif l:match("^diff %-%-git") then
      lines[#lines + 1] = line(l, "git-section")
    elseif l:match("^%-%-%- ") or l:match("^%+%+%+ ") or l:match("^index ") then
      lines[#lines + 1] = line(l, "dim")
    elseif l:match("^%w+:%s") and #lines < 8 then
      local key, rest = l:match("^(%w+:)(.*)$")
      lines[#lines + 1] = pieces({ { key, "git-label" }, { rest } })
    else
      lines[#lines + 1] = diff_line(l)
    end
  end
  if not core.contains({ G.log_buffer, G.show_buffer, G.status_buffer }, V.buffer_name()) then G.return_to = V.buffer_name() end
  G.show_root = root
  draw(G.show_buffer, lines)
  V.switch_to_buffer(G.show_buffer)
  V.goto_line_column(1, 1)
  V.message(title or "")
end

-- RET in *git-show* on a diff line: that file, at that line.
function G.show_visit()
  local text = V.buffer_string()
  local all = lines_of(text)
  local here = V.line_number()
  local file, new_line, offset = nil, nil, 0
  for i = here, 1, -1 do
    local l = all[i] or ""
    if not new_line and l:match("^@@") then
      new_line = tonumber(l:match("%+(%d+)")) or 1
      -- Lines after the @@ that count on the new side.
      for j = i + 1, here - 1 do
        local c = (all[j] or ""):sub(1, 1)
        if c ~= "-" then offset = offset + 1 end
      end
    end
    local path = l:match("^%+%+%+ b/(.+)$")
    if path then
      file = path
      break
    end
  end
  if not file then return V.message("Not on a change") end
  V.find_file(G.show_root .. "/" .. file)
  V.goto_line_column((new_line or 1) + offset, 1)
end

---- In files

-- The current file's repository and path in it.
local function here()
  local file = V.buffer_file_name()
  if file == "" then
    V.message("Not a file")
    return nil
  end
  local root = G.root()
  if not root then return nil end
  return root, file:sub(#root + 2)
end

-- SPC g b: the commit that last changed the cursor's line.
function G.blame_show()
  local root = here()
  if not root then return end
  local b = V.git_blame_here()
  if #b == 0 then
    -- Not blamed yet (or blame is off): ask git for this one line.
    local _, rel = here()
    local n = V.line_number()
    local out = G.run(root, { "blame", "--porcelain", "-L", n .. "," .. n, "--contents", "-", "--", rel })
    if not out then return end
    b = { out:match("^(%x+)") }
  end
  if not b[1] or b[1]:match("^0+$") then return V.message("Not committed yet") end
  G.show(root, b[1])
end

function G.blame_toggle()
  V.set_git_blame_inline(not V.git_blame_inline())
  V.message(V.git_blame_inline() and "Blame shown on the cursor's line" or "Blame hidden")
end

-- SPC g l: the file's history (following renames).
function G.file_log()
  local root, rel = here()
  if root then G.log(root, { "--follow", "--", rel }, "History of " .. rel) end
end

-- SPC g L: how the line (or the selected lines) came to be.
function G.line_log()
  local root, rel = here()
  if not root then return end
  local from, to = V.line_number(), V.line_number()
  if core.visual() then
    local b = core.visual_bounds()
    local save = V.point()
    V.goto_char(b[1])
    from = V.line_number()
    V.goto_char(math.max(b[1], b[2] - 1))
    to = V.line_number()
    V.goto_char(save)
    core.state = "normal"
  end
  if V.buffer_modified() then V.message("Unsaved changes aren't in the history; save to include them") end
  local out = G.run(root, { "log", "--color=never", "-L", from .. "," .. to .. ":" .. rel })
  if out then G.show_text(root, out, "History of " .. rel .. ":" .. from .. (to ~= from and ("-" .. to) or "")) end
end

-- SPC g S: stages the whole file (saving it first).
function G.stage_file()
  local root, rel = here()
  if not root then return end
  if V.buffer_modified() then V.save_buffer() end
  if G.run(root, { "add", "--", rel }) then
    V.git_forget()
    V.message("Staged " .. rel)
  end
end

-- The web page of the line (or selection): GitHub, GitLab, Bitbucket...
function G.remote_url()
  local root, rel = here()
  if not root then return nil end
  local remote = (G.ask(root, { "remote", "get-url", "origin" }) or ""):gsub("%s+$", "")
  if remote == "" then
    V.message("No remote called origin")
    return nil
  end
  -- git@host:owner/repo(.git) and ssh:// and https:// all become https.
  local web = remote:gsub("%.git$", "")
  web = web:gsub("^git@([^:]+):", "https://%1/")
  web = web:gsub("^ssh://git@([^/:]+)[:%d]*/", "https://%1/")
  web = web:gsub("^https://[^@/]+@", "https://")
  local commit = (G.ask(root, { "rev-parse", "HEAD" }) or ""):gsub("%s+$", "")
  local from, to = V.line_number(), V.line_number()
  if core.visual() then
    local b = core.visual_bounds()
    local save = V.point()
    V.goto_char(b[1])
    from = V.line_number()
    V.goto_char(math.max(b[1], b[2] - 1))
    to = V.line_number()
    V.goto_char(save)
    core.state = "normal"
  end
  local lines
  if web:match("gitlab") then
    lines = "#L" .. from .. (to ~= from and ("-" .. to) or "")
    return web .. "/-/blob/" .. commit .. "/" .. rel .. lines
  elseif web:match("bitbucket") then
    lines = "#lines-" .. from .. (to ~= from and (":" .. to) or "")
    return web .. "/src/" .. commit .. "/" .. rel .. lines
  end
  lines = "#L" .. from .. (to ~= from and ("-L" .. to) or "")
  return web .. "/blob/" .. commit .. "/" .. rel .. lines
end

function G.browse()
  local url = G.remote_url()
  if url then
    V.exec("open", { url })
    V.message(url)
  end
end

function G.copy_url()
  local url = G.remote_url()
  if url then
    V.clipboard_set(url)
    V.message("Copied " .. url)
  end
end

---- Operations in progress: rebase, merge, cherry-pick, revert

local git_dirs = {}
function G.git_dir(root)
  if not git_dirs[root] then
    local out = G.ask(root, { "rev-parse", "--absolute-git-dir" })
    git_dirs[root] = out and out:gsub("%s+$", "") or (root .. "/.git")
  end
  return git_dirs[root]
end

local function read_file(path)
  local f = io.open(path, "r")
  if not f then return nil end
  local text = f:read("a")
  f:close()
  return (text:gsub("%s+$", ""))
end

-- What git is in the middle of: { kind, title }, or nil.
function G.operation(root)
  local dir = G.git_dir(root)
  local function short(ref) return (ref or ""):gsub("^refs/heads/", ""):sub(1, 40) end
  for _, sub in ipairs({ "rebase-merge", "rebase-apply" }) do
    if V.file_exists(dir .. "/" .. sub) then
      local head = short(read_file(dir .. "/" .. sub .. "/head-name"))
      local onto = (read_file(dir .. "/" .. sub .. "/onto") or ""):sub(1, 7)
      local step, total = read_file(dir .. "/" .. sub .. "/msgnum"), read_file(dir .. "/" .. sub .. "/end")
      local where = step and total and (" (" .. step .. "/" .. total .. ")") or ""
      return { kind = "rebase", title = "Rebasing " .. head .. " onto " .. onto .. where }
    end
  end
  if V.file_exists(dir .. "/MERGE_HEAD") then
    return { kind = "merge", title = "Merging " .. (read_file(dir .. "/MERGE_HEAD") or ""):sub(1, 7) }
  end
  if V.file_exists(dir .. "/CHERRY_PICK_HEAD") then
    return { kind = "cherry-pick", title = "Cherry-picking " .. (read_file(dir .. "/CHERRY_PICK_HEAD") or ""):sub(1, 7) }
  end
  if V.file_exists(dir .. "/REVERT_HEAD") then
    return { kind = "revert", title = "Reverting " .. (read_file(dir .. "/REVERT_HEAD") or ""):sub(1, 7) }
  end
  return nil
end

-- After something that may stop on conflicts: say what to do next.
local function after_operation(root, ok, err, done)
  changed()
  if ok then return V.message(done) end
  local op = G.operation(root)
  if op then
    V.message(op.title .. ": " .. (err:match("CONFLICT[^\n]*") or "stopped") .. " — fix, stage (s), then r r")
  else
    V.message(last_line(err))
  end
end

-- Runs git without an editor (messages are kept as git prepares them).
local function quiet(root, args)
  local all = { "-c", "core.editor=true" }
  for _, a in ipairs(args) do all[#all + 1] = a end
  local ok, out, err = V.git(root, all, nil)
  return ok, (err ~= "" and err or out)
end

-- r r, r s, r a: continue, skip or abort what's in progress.
local function operation_step(step)
  local root = G.status_root or G.root()
  if not root then return end
  local op = G.operation(root)
  if not op then return V.message("Nothing in progress") end
  if op.kind == "merge" then
    if step == "abort" then
      local ok, err = quiet(root, { "merge", "--abort" })
      return after_operation(root, ok, err, "Merge aborted")
    end
    return V.message("Finish the merge by committing (c c), or abort it (r a)")
  end
  local ok, err = quiet(root, { op.kind, "--" .. step })
  after_operation(root, ok, err, ({ continue = "Done: ", skip = "Skipped: ", abort = "Aborted: " })[step] .. op.title:lower())
end
G.continue = function() operation_step("continue") end
G.skip = function() operation_step("skip") end
G.abort = function() operation_step("abort") end

---- Commits: cherry-pick, revert, reset, merge

-- The commit under the cursor: in the status buffer, the log, *git-show*
-- or *git-blame*.
function G.commit_at()
  local name = V.buffer_name()
  if name == G.status_buffer then
    local it = G.item()
    return it and it.kind == "commit" and it.hash or nil, G.status_root
  elseif name == G.log_buffer then
    return G.log_items[V.line_number()], G.log_root
  elseif name == G.show_buffer then
    return V.buffer_string():match("^commit (%x+)"), G.show_root
  elseif name == G.blame_buffer then
    local h = G.blame_items[V.line_number()]
    return h and not h:match("^0+$") and h or nil, G.blame_root
  end
end

local function with_commit(what, fn)
  return function()
    local hash, root = G.commit_at()
    if not hash then return V.message("Put the cursor on a commit to " .. what) end
    fn(root, hash)
  end
end

G.cherry_pick = with_commit("cherry-pick", function(root, hash)
  local ok, err = quiet(root, { "cherry-pick", hash })
  after_operation(root, ok, err, "Picked " .. hash)
end)

G.revert = with_commit("revert", function(root, hash)
  core.confirm("Revert " .. hash .. " (a new commit undoing it)?", function()
    local ok, err = quiet(root, { "revert", "--no-edit", hash })
    after_operation(root, ok, err, "Reverted " .. hash)
  end)
end)

local function reset(mode)
  return with_commit("reset to", function(root, hash)
    local go = function()
      local ok, err = quiet(root, { "reset", "-q", "--" .. mode, hash })
      after_operation(root, ok, err, "Reset (" .. mode .. ") to " .. hash)
    end
    if mode == "hard" then
      core.confirm("Reset hard to " .. hash .. "? Uncommitted changes are lost", go)
    else
      go()
    end
  end)
end
G.reset_soft, G.reset_mixed, G.reset_hard = reset("soft"), reset("mixed"), reset("hard")

function G.merge()
  local root = G.status_root or G.root()
  if not root then return end
  local current = V.git_branch(root)
  local list = {}
  for _, b in ipairs(branches(root, true)) do if b ~= current then list[#list + 1] = b end end
  core.pick("Merge into " .. current, list, "", function(b)
    local ok, err = quiet(root, { "merge", "--no-edit", b })
    after_operation(root, ok, err, "Merged " .. b)
  end)
end

-- r u: rebase onto the upstream.
function G.rebase_upstream()
  local root = G.status_root or G.root()
  if not root then return end
  local ok, err = quiet(root, { "rebase", "@{upstream}" })
  after_operation(root, ok, err, "Rebased onto the upstream")
end

---- Interactive rebase: *git-rebase*

local actions = { p = "pick", r = "reword", e = "edit", s = "squash", f = "fixup", d = "drop" }
local action_faces = { pick = "default", reword = "blue", edit = "yellow", squash = "magenta", fixup = "magenta", drop = "red" }

function G.rebase_draw()
  local out = {}
  for _, t in ipairs(G.todo) do
    local parts = { { string.format("%-7s", t.action), action_faces[t.action] }, { t.hash, "git-hash" }, { " " .. (t.message or t.subject), t.action == "drop" and "dim" or nil } }
    out[#out + 1] = pieces(parts)
  end
  out[#out + 1] = line("")
  out[#out + 1] = line("Rebasing " .. #G.todo .. " commits onto " .. G.rebase_base_label .. ", oldest first.", "dim")
  out[#out + 1] = line("p pick · r reword · e edit (stop there) · s squash · f fixup · d drop", "dim")
  out[#out + 1] = line("C-j / C-k move a commit down / up · C-c C-c (or Z Z) rebases · C-c C-k cancels", "dim")
  draw(G.rebase_buffer, out, V.buffer_name() == G.rebase_buffer and V.line_number() or nil)
end

-- r i: the commits from the one under the cursor to HEAD, to reorder,
-- squash, reword, edit or drop.
G.rebase_interactive = with_commit("rebase from", function(root, hash)
  if not V.git(root, { "merge-base", "--is-ancestor", hash, "HEAD" }, nil) then
    return V.message(hash .. " isn't in the current branch")
  end
  local parent = G.ask(root, { "rev-parse", "--verify", "-q", hash .. "^" })
  local range = parent and (hash .. "^..HEAD") or "HEAD"
  G.rebase_root, G.rebase_onto = root, parent and parent:gsub("%s+$", "") or nil
  G.rebase_base_label = parent and parent:sub(1, 7) or "the root"
  G.todo = {}
  for _, l in ipairs(lines_of(G.ask(root, { "log", "--reverse", "--format=%h%x1f%s", range }))) do
    local h, subject = l:match("^([^\31]*)\31(.*)$")
    if h then G.todo[#G.todo + 1] = { action = "pick", hash = h, subject = subject } end
  end
  if #G.todo == 0 then return V.message("Nothing to rebase") end
  if not core.contains({ G.log_buffer, G.show_buffer, G.status_buffer, G.rebase_buffer }, V.buffer_name()) then G.return_to = V.buffer_name() end
  G.rebase_draw()
  V.switch_to_buffer(G.rebase_buffer)
  V.goto_line_column(1, 1)
end)

local function todo_here()
  local n = V.line_number()
  return G.todo[n] and n or nil
end

local function set_action(key)
  return function()
    local n = todo_here()
    if not n then return end
    local t = G.todo[n]
    t.action = actions[key]
    if key == "r" then
      V.prompt("New message: ", function(msg)
        t.message = msg ~= "" and msg or nil
        if not t.message then t.action = "pick" end
        G.rebase_draw()
      end, t.message or t.subject)
    elseif key == "s" and n == 1 or key == "f" and n == 1 then
      t.action = "pick"
      V.message("The first commit has nothing above it to squash into")
    end
    G.rebase_draw()
    if key ~= "r" then V.goto_line_column(math.min(n + 1, #G.todo), 1) end
  end
end

local function move(by)
  return function()
    local n = todo_here()
    if not n or not G.todo[n + by] then return end
    G.todo[n], G.todo[n + by] = G.todo[n + by], G.todo[n]
    G.rebase_draw()
    V.goto_line_column(n + by, 1)
  end
end

local function temp_file(text)
  local path = os.tmpname()
  local f = io.open(path, "w")
  f:write(text)
  f:close()
  return path
end

local function shell_quote(s) return "'" .. s:gsub("'", "'\\''") .. "'" end

function G.rebase_start()
  local root = G.rebase_root
  local todo = {}
  for _, t in ipairs(G.todo) do
    if t.action == "reword" and t.message then
      -- Reworded without an editor: picked, then amended with the message.
      todo[#todo + 1] = "pick " .. t.hash .. " " .. t.subject
      todo[#todo + 1] = "exec git commit --amend -q -F " .. shell_quote(temp_file(t.message .. "\n"))
    else
      todo[#todo + 1] = t.action .. " " .. t.hash .. " " .. t.subject
    end
  end
  local file = temp_file(table.concat(todo, "\n") .. "\n")
  local args = { "-c", "sequence.editor=cp " .. shell_quote(file), "-c", "core.editor=true", "rebase", "-i" }
  args[#args + 1] = G.rebase_onto or "--root"
  local ok, out, err = V.git(root, args, nil)
  V.set_buffer_text(G.rebase_buffer, "")
  if not (G.status_root == root and V.switch_to_buffer(G.status_buffer)) then G.close() end
  G.status_root = G.status_root or root
  after_operation(root, ok, err ~= "" and err or out, "Rebased " .. #G.todo .. " commits")
  if V.buffer_name() == G.status_buffer then G.refresh(true) end
end

function G.rebase_cancel()
  V.set_buffer_text(G.rebase_buffer, "")
  if not (G.status_root and V.switch_to_buffer(G.status_buffer)) then G.close() end
  V.message("Not rebased")
end

---- Blame, the whole file: *git-blame*

G.blame_items = {}

-- SPC g b: each line with the commit that last changed it.
function G.blame_view()
  local root, rel = here()
  if not root then return end
  local text = V.buffer_string()
  local blame, err = V.git_blame(root, rel, text)
  if err ~= "" then return V.message("git blame: " .. last_line(err)) end
  local out, items, previous = {}, {}, nil
  local width = 0
  for _, b in ipairs(blame) do if b[2] then width = math.max(width, math.min(18, core.length(b[2]))) end end
  local pad = function(s, n)
    s = core.length(s) > n and (s:sub(1, n - 1) .. "…") or s
    return s .. string.rep(" ", n - core.length(s))
  end
  for i, code in ipairs(lines_of(text)) do
    local b = blame[i] or {}
    local parts
    if b[1] and b[1] ~= previous then
      if b[1]:match("^0+$") then
        parts = { { pad("Not committed yet", 8 + width + 15), "dim" } }
      else
        parts = { { b[1]:sub(1, 7), "git-hash" }, { " " .. pad(b[2], width) }, { " " .. pad(b[3], 14), "dim" } }
      end
    else
      parts = { { string.rep(" ", 8 + width + 15) } }
    end
    parts[#parts + 1] = { " │ ", "dim" }
    parts[#parts + 1] = { code }
    out[#out + 1] = pieces(parts)
    items[#out] = b[1]
    previous = b[1]
  end
  local line_now = V.line_number()
  G.blame_file, G.blame_root, G.blame_items = V.buffer_file_name(), root, items
  G.return_to = V.buffer_name()
  draw(G.blame_buffer, out)
  V.switch_to_buffer(G.blame_buffer)
  V.goto_line_column(line_now, 1)
  V.message("RET shows a line's commit · q goes back")
end

function G.blame_visit()
  local hash = G.blame_items[V.line_number()]
  if not hash or hash:match("^0+$") then return V.message("Not committed yet") end
  G.show(G.blame_root, hash)
end

-- q in *git-blame*: back to the file, on the same line.
function G.blame_close()
  local n = V.line_number()
  V.find_file(G.blame_file)
  V.goto_line_column(n, 1)
end

---- Commands and keys

local commands = {
  ["git-status"] = G.status_open,
  ["git-status-refresh"] = function() G.refresh(true) end,
  ["git-toggle"] = G.toggle,
  ["git-visit"] = G.visit,
  ["git-stage"] = G.stage,
  ["git-unstage"] = G.unstage,
  ["git-stage-all"] = G.stage_all,
  ["git-unstage-all"] = G.unstage_all,
  ["git-discard"] = G.discard,
  ["git-close"] = G.close,
  ["git-commit"] = function() G.commit(false) end,
  ["git-commit-amend"] = function() G.commit(true) end,
  ["git-commit-extend"] = G.commit_extend,
  ["git-commit-finish"] = G.commit_finish,
  ["git-commit-cancel"] = G.commit_cancel,
  ["git-branch-switch"] = G.switch_branch,
  ["git-branch-new"] = G.new_branch,
  ["git-branch-delete"] = G.delete_branch,
  ["git-push"] = G.push,
  ["git-pull"] = G.pull,
  ["git-fetch"] = G.fetch,
  ["git-stash"] = G.stash,
  ["git-stash-pop"] = G.stash_pop,
  ["git-stash-apply"] = G.stash_apply,
  ["git-stash-drop"] = G.stash_drop,
  ["git-log"] = function() G.log(G.status_root or G.root(), {}) end,
  ["git-log-all"] = function() G.log(G.status_root or G.root(), { "--all" }, "Log of all branches") end,
  ["git-log-visit"] = G.log_visit,
  ["git-show-visit"] = G.show_visit,
  ["git-blame-show"] = G.blame_show,
  ["git-blame-toggle"] = G.blame_toggle,
  ["git-file-log"] = G.file_log,
  ["git-line-log"] = G.line_log,
  ["git-stage-file"] = G.stage_file,
  ["git-hunk-stage"] = function() V.git_hunk_here("stage") end,
  ["git-hunk-reset"] = function() V.git_hunk_here("reset") end,
  ["git-hunk-next"] = function() V.git_hunk_here("next") end,
  ["git-hunk-previous"] = function() V.git_hunk_here("previous") end,
  ["git-browse"] = G.browse,
  ["git-copy-link"] = G.copy_url,
  ["git-stage-lines"] = G.stage_lines,
  ["git-unstage-lines"] = G.unstage_lines,
  ["git-discard-lines"] = G.discard_lines,
  ["git-continue"] = G.continue,
  ["git-skip"] = G.skip,
  ["git-abort"] = G.abort,
  ["git-cherry-pick"] = G.cherry_pick,
  ["git-revert"] = G.revert,
  ["git-reset-soft"] = G.reset_soft,
  ["git-reset-mixed"] = G.reset_mixed,
  ["git-reset-hard"] = G.reset_hard,
  ["git-merge"] = G.merge,
  ["git-rebase-upstream"] = G.rebase_upstream,
  ["git-rebase-interactive"] = G.rebase_interactive,
  ["git-rebase-start"] = G.rebase_start,
  ["git-rebase-cancel"] = G.rebase_cancel,
  ["git-rebase-down"] = move(1),
  ["git-rebase-up"] = move(-1),
  ["git-blame"] = G.blame_view,
  ["git-blame-visit"] = G.blame_visit,
  ["git-blame-close"] = G.blame_close,
  ["git-diff-view-toggle"] = function()
    G.diff_view = not G.diff_view
    V.message(G.diff_view and "Commits drawn as diffs" or "Commits drawn as text")
  end,
}
for key, action in pairs(actions) do
  commands["git-rebase-" .. action] = set_action(key)
end
for name, fn in pairs(commands) do V.command(name, fn) end

-- Keys like c and x are Vim's in normal state; in git buffers they start
-- a sequence (c c, c a...), showing what can follow, like magit's menus.
local function prefix(key)
  return function()
    core.pending_keys = key
    V.message(key .. "-")
    core.whichkey_open(key)
  end
end

local status = {}
core.keymaps.git_status = status
for key, cmd in pairs({
  TAB = "git-toggle", ["z a"] = "git-stash-apply", RET = "git-visit",
  s = "git-stage", u = "git-unstage", S = "git-stage-all", U = "git-unstage-all", x = "git-discard",
  ["c c"] = "git-commit", ["c a"] = "git-commit-amend", ["c e"] = "git-commit-extend",
  ["b b"] = "git-branch-switch", ["b c"] = "git-branch-new", ["b k"] = "git-branch-delete",
  ["P p"] = "git-push", ["F p"] = "git-pull", ["f f"] = "git-fetch",
  ["z z"] = "git-stash", ["z p"] = "git-stash-pop", ["z k"] = "git-stash-drop",
  ["l l"] = "git-log", ["l a"] = "git-log-all",
  ["g r"] = "git-status-refresh", q = "git-close",
}) do status[key] = cmd end
-- On a commit (here, in the log, *git-show*, *git-blame*): pick, revert,
-- reset to it, rebase from it; and what's in progress.
local on_commits = {
  A = "git-cherry-pick", _ = "git-revert",
  ["X s"] = "git-reset-soft", ["X m"] = "git-reset-mixed", ["X h"] = "git-reset-hard",
  ["r i"] = "git-rebase-interactive", ["r u"] = "git-rebase-upstream",
  ["r r"] = "git-continue", ["r s"] = "git-skip", ["r a"] = "git-abort",
  ["m m"] = "git-merge",
}
for key, cmd in pairs(on_commits) do status[key] = cmd end
for _, key in ipairs({ "c", "b", "P", "F", "f", "z", "l", "X", "r", "m" }) do status[key] = prefix(key) end
core.keymaps.git_status_visual = { s = "git-stage-lines", u = "git-unstage-lines", x = "git-discard-lines" }

local log = { RET = "git-log-visit", q = "git-close", ["l l"] = "git-log", ["l a"] = "git-log-all", l = prefix("l") }
local show = { RET = "git-show-visit", q = "git-close" }
local blame = { RET = "git-blame-visit", q = "git-blame-close" }
for _, map in ipairs({ log, show, blame }) do
  for key, cmd in pairs(on_commits) do map[key] = cmd end
  for _, key in ipairs({ "X", "r", "m" }) do map[key] = prefix(key) end
end
core.keymaps.git_log = log
core.keymaps.git_show = show
core.keymaps.git_blame = blame
local rebase = {
  ["C-j"] = "git-rebase-down", ["C-k"] = "git-rebase-up",
  ["C-c C-c"] = "git-rebase-start", ["Z Z"] = "git-rebase-start", ["C-c C-k"] = "git-rebase-cancel", q = "git-rebase-cancel",
}
for key, action in pairs(actions) do rebase[key] = "git-rebase-" .. action end
core.keymaps.git_rebase = rebase
core.keymaps.git_commit = { ["C-c C-c"] = "git-commit-finish", ["C-c C-k"] = "git-commit-cancel", ["Z Z"] = "git-commit-finish" }
core.keymaps.git_commit_insert = { ["C-c C-c"] = "git-commit-finish", ["C-c C-k"] = "git-commit-cancel" }

core.buffer_keymaps = core.buffer_keymaps or {}
for _, e in ipairs({
  { G.status_buffer, "git_status", nil, "git_status_visual" },
  { G.blame_buffer, "git_blame" },
  { G.rebase_buffer, "git_rebase" },
  { G.log_buffer, "git_log" },
  { G.show_buffer, "git_show" },
  { G.commit_buffer, "git_commit", "git_commit_insert" },
}) do
  core.buffer_keymaps[#core.buffer_keymaps + 1] = e
end

for key, cmd in pairs({
  ["SPC g g"] = "git-status", ["SPC g c"] = "git-commit", ["SPC g b"] = "git-blame", ["SPC g v"] = "git-blame-show",
  ["SPC g B"] = "git-blame-toggle", ["SPC g l"] = "git-file-log", ["SPC g L"] = "git-line-log",
  ["SPC g s"] = "git-hunk-stage", ["SPC g r"] = "git-hunk-reset", ["SPC g S"] = "git-stage-file",
  ["SPC g o"] = "git-browse", ["SPC g y"] = "git-copy-link", ["SPC g C"] = "git-branch-switch",
  ["SPC g P"] = "git-push", ["SPC g F"] = "git-pull", ["SPC g a"] = "git-log-all",
  ["] c"] = "git-hunk-next", ["[ c"] = "git-hunk-previous",
}) do
  core.define_key("normal", key, cmd)
end
for key, cmd in pairs({ ["SPC g L"] = "git-line-log", ["SPC g o"] = "git-browse", ["SPC g y"] = "git-copy-link" }) do
  core.define_key("visual", key, cmd)
end

-- For github.lua and plugins: drawing buffers, reading output, menus.
G.draw, G.pieces, G.line, G.lines_of, G.diff_line, G.prefix, G.last_line = draw, pieces, line, lines_of, diff_line, prefix, last_line
