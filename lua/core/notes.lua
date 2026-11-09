-- Notes, the org-mode way, in plain Markdown files under ~/notes
-- (vmac.set_notes_dir("~/elsewhere") in init.lua to change it).
--
-- From anywhere:
--   SPC n c   capture a TODO into inbox.md (SPC n C: a plain note)
--   SPC n a   the agenda: open TODOs and dated items across every note
--   SPC n d   today's daily note (daily/2026-11-09.md); SPC n y yesterday's
--   SPC n f   find a note; SPC n i opens the inbox
--
-- In any Markdown buffer (normal state):
--   SPC m t   cycle a heading TODO -> DONE -> plain; tick a "- [ ]" item
--   M-h M-l   promote / demote a heading (with its subtree) or a list item
--   M-k M-j   move a heading's subtree, or a list item, past its sibling
--   SPC m d   insert a date: <2026-11-09 Mon> (asks: empty, +3, +2w, fri, 12-24)
--   SPC m l   insert a [[link]] to another note
--   RET       follow the [[link]] or [text](file.md) under the cursor
--
-- What counts: a TODO is a heading starting "TODO " or an unticked "- [ ]";
-- "DONE " and "- [x]" are done. A date in angle brackets (<2026-11-09 Mon>)
-- puts its line on the agenda; one in square brackets doesn't (captures are
-- stamped that way).
--
-- In *agenda*: RET visits the item, t marks it done (and saves), r
-- refreshes, q goes back.

local V = vmac
local N = {}
core.notes = N

N.dir = "~/notes"
N.agenda_buffer = "*agenda*"
function V.set_notes_dir(dir) N.dir = dir end

local function expand(path)
  if path:sub(1, 1) == "~" then return (os.getenv("HOME") or "") .. path:sub(2) end
  return path
end

function N.root() return (expand(N.dir):gsub("/$", "")) end

local function ensure_dir(dir) if not V.file_exists(dir) then V.create_path(dir .. "/") end end

---- Dates

local day_names = { "Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat" }

function N.stamp(t, with_time) return os.date(with_time and "%Y-%m-%d %a %H:%M" or "%Y-%m-%d %a", t) end

-- Noon on the day, so adding days never trips over daylight saving.
local function day(y, m, d) return os.time({ year = y, month = m, day = d, hour = 12 }) end
local function today()
  local t = os.date("*t")
  return day(t.year, t.month, t.day)
end
local function add_days(t, n)
  local d = os.date("*t", t)
  return day(d.year, d.month, d.day + n)
end

-- "" today, "+3" / "-1" days, "+2w" weeks, "fri" the next Friday,
-- "12-24" or "2026-12-24": a time, or nil.
function N.parse_date(s)
  s = s:lower():gsub("^%s+", ""):gsub("%s+$", "")
  local now = today()
  if s == "" or s == "today" or s == "." then return now end
  if s == "tomorrow" then return add_days(now, 1) end
  if s == "yesterday" then return add_days(now, -1) end
  local sign, n, unit = s:match("^([+-])(%d+)([dw]?)$")
  if sign then
    n = tonumber(n) * (unit == "w" and 7 or 1)
    return add_days(now, sign == "-" and -n or n)
  end
  local y, m, d = s:match("^(%d%d%d%d)-(%d%d?)-(%d%d?)$")
  if y then return day(tonumber(y), tonumber(m), tonumber(d)) end
  m, d = s:match("^(%d%d?)-(%d%d?)$")
  if m then
    local t = day(tonumber(os.date("%Y", now)), tonumber(m), tonumber(d))
    return t < now and day(tonumber(os.date("%Y", now)) + 1, tonumber(m), tonumber(d)) or t
  end
  for i, name in ipairs(day_names) do
    if s:sub(1, 3) == name:lower() and #s >= 3 then
      local ahead = (i - 1 - tonumber(os.date("%w", now))) % 7
      return add_days(now, ahead == 0 and 7 or ahead)
    end
  end
  return nil
end

-- The first active date on a line, <2026-11-09 ...>: its time and text.
local function active_date(line)
  local y, m, d = line:match("<(%d%d%d%d)-(%d%d)-(%d%d)[^>]*>")
  if not y then return nil end
  return day(tonumber(y), tonumber(m), tonumber(d))
end

---- Lines of the current buffer

local function buffer_lines()
  local out = {}
  for l in (V.buffer_string() .. "\n"):gmatch("([^\n]*)\n") do out[#out + 1] = l end
  return out
end

-- Which lines are inside ``` or ~~~ fences (their # aren't headings).
local function fenced(ls)
  local inside, out = false, {}
  for i, l in ipairs(ls) do
    if l:match("^%s*```") or l:match("^%s*~~~") then
      out[i] = true
      inside = not inside
    else
      out[i] = inside
    end
  end
  return out
end

local function blank(l) return l:match("^%s*$") ~= nil end
local function indent_of(l) return #l:match("^%s*") end

local function heading_level(ls, f, i)
  if f[i] or not ls[i] then return nil end
  local h = ls[i]:match("^(#+)%s") or ls[i]:match("^(#+)$")
  return h and #h <= 6 and #h or nil
end

local function item_indent(ls, f, i)
  if f[i] or not ls[i] then return nil end
  local ind = ls[i]:match("^(%s*)[-*+]%s") or ls[i]:match("^(%s*)%d+[.)]%s")
  return ind and #ind or nil
end

-- The heading or list item starting at line i and what belongs to it:
-- { kind, level, s, e } with e its last non-blank line (blank lines after
-- it separate it from the next and stay put when things move).
local function block_at(ls, f, i)
  local lv = heading_level(ls, f, i)
  if lv then
    local j = i + 1
    while j <= #ls do
      local l2 = heading_level(ls, f, j)
      if l2 and l2 <= lv then break end
      j = j + 1
    end
    local e = j - 1
    while e > i and blank(ls[e]) do e = e - 1 end
    return { kind = "heading", level = lv, s = i, e = e }
  end
  local ind = item_indent(ls, f, i)
  if ind then
    local j, last = i + 1, i
    while j <= #ls do
      if blank(ls[j]) then
        j = j + 1
      elseif indent_of(ls[j]) > ind and not heading_level(ls, f, j) then
        last, j = j, j + 1
      else
        break
      end
    end
    return { kind = "item", level = ind, s = i, e = last }
  end
  return nil
end

-- The block the cursor's line belongs to: the line's own, else the
-- heading above it (so M-j in a section's text moves the section).
local function block_here(ls, f)
  local line = V.line_number()
  local b = block_at(ls, f, line)
  if b then return b end
  for k = line - 1, 1, -1 do
    if heading_level(ls, f, k) then return block_at(ls, f, k) end
  end
  return nil
end

local function cursor_column() return V.point() - V.line_to_pos(V.line_number() - 1) end

-- Replaces lines s..e (1-based) with `new`, as one undo step.
local function replace_lines(s, e, new)
  local a = V.line_to_pos(s - 1)
  local b = V.line_end_position(V.line_to_pos(e - 1))
  V.undo_boundary()
  V.delete_region(a, b)
  V.goto_char(a)
  V.insert(table.concat(new, "\n"))
  V.undo_boundary()
end

local function slice(ls, s, e)
  local out = {}
  for i = s, e do out[#out + 1] = ls[i] end
  return out
end

local function markdown_here() return V.buffer_language() == "markdown" end

local function in_markdown(fn)
  return function()
    if not markdown_here() then return V.message("Not a Markdown buffer") end
    if V.buffer_read_only() then return V.message("Buffer is read-only") end
    fn()
  end
end

---- TODO

-- The line, cycled: "# TODO x" -> "# DONE x" -> "# x" -> "# TODO x";
-- "- [ ] x" <-> "- [x] x"; "- x" -> "- [ ] x". Nil if it's neither.
function N.cycle_line(l)
  local hashes, rest = l:match("^(#+%s+)(.*)$")
  if hashes then
    if rest:match("^TODO%s") or rest == "TODO" then return hashes .. "DONE" .. rest:sub(5) end
    if rest:match("^DONE%s") or rest == "DONE" then return hashes .. rest:gsub("^DONE%s*", "") end
    return hashes .. "TODO " .. rest
  end
  local lead, box, after = l:match("^(%s*[-*+]%s+)%[([ xX])%](.*)$")
  if lead then return lead .. (box == " " and "[x]" or "[ ]") .. after end
  lead, after = l:match("^(%s*[-*+]%s+)(.*)$")
  if lead then return lead .. "[ ] " .. after end
  return nil
end

function N.cycle_todo()
  local line, col = V.line_number(), cursor_column()
  local ls = buffer_lines()
  local f = fenced(ls)
  if f[line] then return V.message("Inside a code block") end
  local new = N.cycle_line(ls[line])
  if not new then return V.message("Not a heading or list item") end
  replace_lines(line, line, { new })
  V.goto_line_column(line, math.min(col, core.length(new)) + 1)
end

---- Moving and promoting

local function shift(by)
  return function()
    local ls = buffer_lines()
    local f = fenced(ls)
    local b = block_here(ls, f)
    if not b then return V.message("Not on a heading or list item") end
    local line, col = V.line_number(), cursor_column()
    local new = slice(ls, b.s, b.e)
    if b.kind == "heading" then
      for i = b.s, b.e do
        local lv = heading_level(ls, f, i)
        if lv and (lv + by < 1 or lv + by > 6) then return V.message("Headings go from # to ######") end
      end
      for i = b.s, b.e do
        if heading_level(ls, f, i) then
          new[i - b.s + 1] = by > 0 and ("#" .. ls[i]) or ls[i]:sub(2)
        end
      end
    else
      if by < 0 and b.level == 0 then return V.message("Already at the top level") end
      for i = b.s, b.e do
        local l = ls[i]
        if not blank(l) then new[i - b.s + 1] = by > 0 and ("  " .. l) or l:gsub("^%s%s?", "") end
      end
    end
    replace_lines(b.s, b.e, new)
    if line >= b.s and line <= b.e then
      local moved = (b.kind == "heading" and 1 or 2) * by
      V.goto_line_column(line, math.max(col + moved, 0) + 1)
    else
      V.goto_line_column(line, col + 1)
    end
  end
end

-- The sibling before or after block b (same kind and level, same parent).
local function sibling(ls, f, b, dir)
  if dir < 0 then
    for k = b.s - 1, 1, -1 do
      if b.kind == "heading" then
        local lv = heading_level(ls, f, k)
        if lv == b.level then return block_at(ls, f, k) end
        if lv and lv < b.level then return nil end
      elseif not blank(ls[k]) then
        local ind = item_indent(ls, f, k)
        if ind == b.level then return block_at(ls, f, k) end
        if indent_of(ls[k]) <= b.level or heading_level(ls, f, k) then return nil end
      end
    end
    return nil
  end
  local j = b.e + 1
  while j <= #ls and blank(ls[j]) do j = j + 1 end
  if j > #ls then return nil end
  local lv = b.kind == "heading" and heading_level(ls, f, j) or item_indent(ls, f, j)
  if lv ~= b.level or (b.kind == "item" and heading_level(ls, f, j)) then return nil end
  local other = block_at(ls, f, j)
  return other and other.kind == b.kind and other or nil
end

local function move(dir)
  return function()
    local ls = buffer_lines()
    local f = fenced(ls)
    local b = block_here(ls, f)
    if not b then return V.message("Not on a heading or list item") end
    local other = sibling(ls, f, b, dir)
    if not other then return V.message(dir < 0 and "Nothing above to move past" or "Nothing below to move past") end
    local first, second = b, other
    if dir < 0 then first, second = other, b end
    local new = slice(ls, second.s, second.e)
    for _, l in ipairs(slice(ls, first.e + 1, second.s - 1)) do new[#new + 1] = l end
    for _, l in ipairs(slice(ls, first.s, first.e)) do new[#new + 1] = l end
    local line, col = V.line_number(), cursor_column()
    local offset = line - b.s
    replace_lines(first.s, second.e, new)
    local start = dir < 0 and first.s or (first.s + (second.e - second.s + 1) + (second.s - first.e - 1))
    V.goto_line_column(start + offset, col + 1)
  end
end

---- Files

-- Every note: paths relative to the notes folder.
function N.files()
  local root = N.root()
  if not V.file_exists(root) then return {} end
  local out = {}
  for _, rel in ipairs(V.project_files(root)) do
    if rel:match("%.md$") or rel:match("%.markdown$") then out[#out + 1] = rel end
  end
  return out
end

local function read_file(path)
  local fh = io.open(path, "r")
  if not fh then return nil end
  local text = fh:read("a")
  fh:close()
  return text
end

-- Is a buffer (maybe) visiting this file? By name, so it can say yes
-- wrongly; then the change goes through the buffer, which is still right.
local function maybe_open(path) return core.contains(V.buffer_list(), core.basename(path)) end

-- Adds a line to the end of a note: on disk, or through its buffer when
-- it's open (so the buffer and the file don't disagree).
function N.append(path, text, title)
  ensure_dir(core.dirname(path))
  if not maybe_open(path) then
    local old = read_file(path) or ""
    local prefix = old == "" and title and ("# " .. title .. "\n\n") or ((old ~= "" and old:sub(-1) ~= "\n") and "\n" or "")
    local fh = io.open(path, "a")
    if not fh then return V.message("Can't write " .. path) end
    fh:write(prefix .. text .. "\n")
    fh:close()
    return true
  end
  local back = V.buffer_name()
  V.find_file(path)
  local old = V.buffer_string()
  local prefix = old == "" and title and ("# " .. title .. "\n\n") or ((old ~= "" and old:sub(-1) ~= "\n") and "\n" or "")
  V.goto_char(V.point_max())
  V.insert(prefix .. text .. "\n")
  V.save_buffer()
  if back ~= V.buffer_name() then V.switch_to_buffer(back) end
  return true
end

function N.inbox() return N.root() .. "/inbox.md" end

local function capture(todo)
  return function()
    core.read(todo and "Capture TODO: " or "Capture note: ", function(text)
      if text == "" then return end
      local line = (todo and "- [ ] " or "- ") .. text .. " [" .. N.stamp(os.time(), true) .. "]"
      if N.append(N.inbox(), line, "Inbox") then V.message("Captured to inbox.md") end
    end)
  end
end

-- Opens a note, starting it with a heading when it's new.
local function open_note(path, title)
  ensure_dir(core.dirname(path))
  local fresh = not V.file_exists(path)
  V.find_file(path)
  if fresh and V.buffer_string() == "" and title then
    V.insert("# " .. title .. "\n\n")
    V.save_buffer()
  end
end

local function daily(offset)
  return function()
    local t = add_days(today(), offset)
    open_note(N.root() .. "/daily/" .. os.date("%Y-%m-%d", t) .. ".md", os.date("%A, %d %B %Y", t))
  end
end

---- Links

-- The note a [[name]] means: one named so anywhere in the notes (any
-- case), else name.md beside the current file, else a new one in the notes.
function N.resolve(name)
  name = name:gsub("|.*$", ""):gsub("#.*$", ""):gsub("^%s+", ""):gsub("%s+$", "")
  local want = name:lower():gsub("%.md$", "")
  local root = N.root()
  for _, rel in ipairs(N.files()) do
    local base = core.basename(rel):lower():gsub("%.md$", ""):gsub("%.markdown$", "")
    if base == want or rel:lower():gsub("%.md$", "") == want then return root .. "/" .. rel end
  end
  local file = V.buffer_file_name()
  if file ~= "" then
    local beside = core.dirname(file) .. "/" .. name .. (name:match("%.md$") and "" or ".md")
    if V.file_exists(beside) then return beside end
  end
  return root .. "/" .. name .. (name:match("%.md$") and "" or ".md"), name
end

-- The link under the cursor: ("wiki", name) or ("file", path), or nil.
local function link_here()
  local line = V.line_number()
  local a = V.line_to_pos(line - 1)
  local text = V.buffer_substring(a, V.line_end_position(a))
  local col = V.point() - a + 1
  -- Columns are characters; Lua's find counts bytes.
  local function covers(s, e)
    local cs = utf8.len(text:sub(1, s - 1)) + 1
    local ce = utf8.len(text:sub(1, e))
    return col >= cs and col <= ce
  end
  local init = 1
  while true do
    local s, e, name = text:find("%[%[([^%]]+)%]%]", init)
    if not s then break end
    if covers(s, e) then return "wiki", name end
    init = e + 1
  end
  init = 1
  while true do
    local s, e, target = text:find("%[[^%]]*%]%(([^)%s]+)[^)]*%)", init)
    if not s then break end
    if covers(s, e) then return "file", target end
    init = e + 1
  end
  return nil
end

function N.follow()
  local kind, target = link_here()
  if kind == "wiki" then
    local path, new_title = N.resolve(target)
    open_note(path, new_title)
    return true
  elseif kind == "file" then
    if target:match("^%a[%w+.-]*://") or target:match("^mailto:") then
      V.message(target)
      return true
    end
    target = target:gsub("#.*$", "")
    if target == "" then return true end
    local file = V.buffer_file_name()
    local path = target:sub(1, 1) == "/" and target or ((file ~= "" and core.dirname(file) or N.root()) .. "/" .. target)
    V.find_file(path)
    return true
  end
  return false
end

function N.insert_link()
  local files = N.files()
  if #files == 0 then return V.message("No notes in " .. N.dir .. " yet") end
  local was_insert = core.state == "insert"
  V.pick("Link to note", files, function(rel)
    local name = core.basename(rel):gsub("%.md$", "")
    V.insert("[[" .. name .. "]]")
    if was_insert then core.state = "insert" end
  end, N.root())
end

---- The agenda

-- Everything the agenda shows, read from the notes on disk: { path, line,
-- text, todo (open TODO), date (a time or nil) }.
function N.collect()
  local root, items = N.root(), {}
  for _, rel in ipairs(N.files()) do
    local text = read_file(root .. "/" .. rel)
    if text then
      local ls = {}
      for l in (text .. "\n"):gmatch("([^\n]*)\n") do ls[#ls + 1] = l end
      local f = fenced(ls)
      for i, l in ipairs(ls) do
        if not f[i] then
          local heading = l:match("^#+%s+(.*)$")
          local box, item = l:match("^%s*[-*+]%s+%[([ xX])%]%s*(.*)$")
          local done = (heading and heading:match("^DONE%f[%W]")) or (box and box ~= " ")
          local todo = not done and ((heading and heading:match("^TODO%f[%W]")) or box == " ")
          local date = active_date(l)
          if not done and (todo or date) then
            local shown = heading or item or l:gsub("^%s*[-*+]%s+", "")
            shown = shown:gsub("^TODO%s*", ""):gsub("%s*<[^>]*>", ""):gsub("%s*%[%d%d%d%d%-[^%]]*%]", "")
            items[#items + 1] = { path = root .. "/" .. rel, rel = rel, line = i, text = shown, todo = todo and true or false, date = date }
          end
        end
      end
    end
  end
  return items
end

N.items = {}

function N.agenda_draw(keep)
  local now = today()
  local overdue, today_list, upcoming, undated = {}, {}, {}, {}
  for _, it in ipairs(N.collect()) do
    if it.date then
      if it.date < now then
        if it.todo then overdue[#overdue + 1] = it end
      elseif it.date == now then
        today_list[#today_list + 1] = it
      else
        upcoming[#upcoming + 1] = it
      end
    else
      undated[#undated + 1] = it
    end
  end
  local by_date = function(a, b) return a.date < b.date or (a.date == b.date and a.rel < b.rel) end
  table.sort(overdue, by_date)
  table.sort(upcoming, by_date)

  local lines, faces, items = {}, {}, {}
  local offset = 0
  local function add(text, spans, item)
    lines[#lines + 1] = text
    for _, s in ipairs(spans or {}) do faces[#faces + 1] = { offset + s[1], offset + s[2], s[3] } end
    items[#lines] = item
    offset = offset + core.length(text) + 1
  end
  local title = "Agenda for " .. os.date("%A, %d %B %Y", now)
  add(title, { { 0, core.length(title), "accent" } })
  add("")
  local function section(name, list, date_face)
    if #list == 0 then return end
    add(name, { { 0, core.length(name), "blue" } })
    for _, it in ipairs(list) do
      local date = it.date and (N.stamp(it.date) .. "  ") or ""
      local mark = it.todo and "TODO " or ""
      local left = "  " .. date .. mark .. it.text
      local text = left .. "  " .. it.rel
      local spans = {}
      local c = 2
      if date ~= "" then spans[#spans + 1] = { c, c + core.length(date), date_face } end
      c = c + core.length(date)
      if mark ~= "" then spans[#spans + 1] = { c, c + 4, "red" } end
      spans[#spans + 1] = { core.length(left) + 2, core.length(text), "dim" }
      add(text, spans, it)
    end
    add("")
  end
  section("Overdue", overdue, "red")
  section("Today", today_list, "green")
  section("Upcoming", upcoming, "cyan")
  section("To do", undated, "dim")
  if #overdue + #today_list + #upcoming + #undated == 0 then
    add("Nothing to do. Capture with SPC n c, or date a line with SPC m d.", { { 0, 9999, "dim" } })
  end
  N.items = items
  V.ensure_buffer(N.agenda_buffer)
  V.set_buffer_read_only(N.agenda_buffer, false)
  V.set_buffer_text(N.agenda_buffer, table.concat(lines, "\n") .. "\n")
  V.set_buffer_read_only(N.agenda_buffer, true)
  for _, f in ipairs(faces) do V.add_face(N.agenda_buffer, f[1], math.min(f[2], offset), f[3]) end
  if V.buffer_name() == N.agenda_buffer then
    local line = keep or 1
    if not keep then
      for i = 1, #lines do
        if items[i] then
          line = i
          break
        end
      end
    end
    V.goto_line_column(math.min(line, #lines), 3)
  end
end

function N.agenda()
  if V.buffer_name() ~= N.agenda_buffer then N.return_to = V.buffer_name() end
  V.ensure_buffer(N.agenda_buffer)
  V.switch_to_buffer(N.agenda_buffer)
  core.state = "normal"
  N.agenda_draw()
end

local function visit(it)
  V.find_file(it.path)
  V.goto_line_column(it.line, 1)
  V.goto_char(core.first_non_blank())
end

function N.agenda_visit()
  local it = N.items[V.line_number()]
  if it then visit(it) end
end

function N.agenda_done()
  local it = N.items[V.line_number()]
  if not it then return end
  local keep = V.line_number()
  visit(it)
  local l = V.buffer_substring(V.line_to_pos(it.line - 1), V.line_end_position(V.line_to_pos(it.line - 1)))
  local new = l:match("^#+%s+TODO") and N.cycle_line(l)
    or l:match("^%s*[-*+]%s+%[ %]") and N.cycle_line(l)
    or l:match("^#+%s") and l:gsub("^(#+%s+)", "%1DONE ", 1)
    or l:match("^%s*[-*+]%s") and l:gsub("^(%s*[-*+]%s+)", "%1[x] ", 1)
    or nil
  if new then
    replace_lines(it.line, it.line, { new })
    V.save_buffer()
  end
  V.switch_to_buffer(N.agenda_buffer)
  N.agenda_draw(keep)
  V.message(new and ("Done: " .. it.text) or "Can't mark that line done; edit it with RET")
end

function N.agenda_close()
  if N.return_to and V.switch_to_buffer(N.return_to) then return end
  V.next_buffer(1)
end

---- Commands and keys

local function find_note()
  local files = N.files()
  if #files == 0 then return V.message("No notes in " .. N.dir .. " yet: SPC n d starts today's") end
  V.pick("Note", files, function(rel) V.find_file(N.root() .. "/" .. rel) end, N.root())
end

local function insert_date()
  core.read("Date (empty today, +3, +2w, fri, 12-24): ", function(s)
    local t = N.parse_date(s)
    if not t then return V.message("Not a date: " .. s) end
    V.insert("<" .. N.stamp(t) .. ">")
  end)
end

for name, fn in pairs({
  ["notes-capture"] = capture(true),
  ["notes-capture-note"] = capture(false),
  ["notes-agenda"] = N.agenda,
  ["notes-today"] = daily(0),
  ["notes-yesterday"] = daily(-1),
  ["notes-find"] = find_note,
  ["notes-inbox"] = function() open_note(N.inbox(), "Inbox") end,
  ["notes-todo-cycle"] = in_markdown(N.cycle_todo),
  ["notes-promote"] = in_markdown(shift(-1)),
  ["notes-demote"] = in_markdown(shift(1)),
  ["notes-move-up"] = in_markdown(move(-1)),
  ["notes-move-down"] = in_markdown(move(1)),
  ["notes-insert-date"] = in_markdown(insert_date),
  ["notes-insert-link"] = in_markdown(N.insert_link),
  ["notes-follow-link"] = function()
    if not N.follow() then
      local fallback = core.keymaps.normal["RET"]
      if fallback then core.call_binding(fallback) end
    end
  end,
  ["notes-agenda-visit"] = N.agenda_visit,
  ["notes-agenda-done"] = N.agenda_done,
  ["notes-agenda-refresh"] = function() N.agenda_draw(V.line_number()) end,
  ["notes-agenda-close"] = N.agenda_close,
}) do
  V.command(name, fn)
end

for key, cmd in pairs({
  ["SPC n c"] = "notes-capture", ["SPC n C"] = "notes-capture-note", ["SPC n a"] = "notes-agenda",
  ["SPC n d"] = "notes-today", ["SPC n y"] = "notes-yesterday", ["SPC n f"] = "notes-find",
  ["SPC n i"] = "notes-inbox",
}) do
  core.define_key("normal", key, cmd)
end

-- In Markdown buffers, in front of normal state's keys.
core.keymaps.notes = {
  ["SPC m t"] = "notes-todo-cycle", ["SPC m d"] = "notes-insert-date", ["SPC m l"] = "notes-insert-link",
  ["M-h"] = "notes-promote", ["M-l"] = "notes-demote", ["M-k"] = "notes-move-up", ["M-j"] = "notes-move-down",
  ["RET"] = "notes-follow-link",
}
core.keymaps.notes_agenda = {
  RET = "notes-agenda-visit", o = "notes-agenda-visit", t = "notes-agenda-done",
  r = "notes-agenda-refresh", ["g r"] = "notes-agenda-refresh", q = "notes-agenda-close",
}

core.buffer_keymaps = core.buffer_keymaps or {}
table.insert(core.buffer_keymaps, { N.agenda_buffer, "notes_agenda" })
table.insert(core.buffer_keymaps, { function() return markdown_here() and V.buffer_file_name() ~= "" end, "notes" })
