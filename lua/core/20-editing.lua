-- Operators (d c y > <), visual mode (v V C-v, block insert), insert
-- state, pasting, undo, and the system clipboard.

local V = vmac

---- Operators
--
-- An operator is a function (start, end, type) acting on a range: d, c, y,
-- >, <. Press it, then a motion or text object (dw, ci", y$), or press it
-- twice for whole lines (dd, 3yy, >>), or select first in visual mode.

local function linewise_range(start, stop)
  local s, e = core.line_start_at(start), core.line_end_at(stop)
  if e < V.point_max() then return s, e + 1 end
  if s > 0 then return s - 1, e end
  return s, e
end

local function range_text(start, stop, kind)
  if kind == "linewise" then
    return V.buffer_substring(core.line_start_at(start), core.line_end_at(stop)) .. "\n"
  end
  return V.buffer_substring(start, stop)
end

local function delete_range(start, stop, kind)
  V.register_set(range_text(start, stop, kind))
  local a, b = start, stop
  if kind == "linewise" then a, b = linewise_range(start, stop) end
  V.delete_region(a, b)
  V.goto_char(a)
  if kind == "linewise" then V.goto_char(core.first_non_blank()) else core.clamp_to_line() end
end

function core.op_delete(start, stop, kind)
  if kind == "block" then
    core.delete_block(core.block_ranges())
    core.goto_block_top_left()
    core.clamp_to_line()
  else
    delete_range(start, stop, kind)
  end
end

function core.op_change(start, stop, kind)
  if kind == "block" then
    local top, left, targets = core.block_top(), core.block_left(), core.block_lines_reaching(core.block_left())
    core.delete_block(core.block_ranges())
    core.start_block_insert(left, top, targets, false, false)
    return
  end
  V.register_set(range_text(start, stop, kind))
  if kind == "linewise" then
    V.delete_region(core.first_non_blank_at(start), core.line_end_at(stop))
    V.goto_char(core.first_non_blank_at(start))
  else
    V.delete_region(start, stop)
    V.goto_char(start)
  end
  core.state = "insert"
end

function core.op_yank(start, stop, kind)
  if kind == "block" then
    core.yank_block(core.block_ranges())
    core.goto_block_top_left()
    V.message("yanked a block")
    return
  end
  V.register_set(range_text(start, stop, kind))
  if kind ~= "linewise" then V.goto_char(start) end
  V.message(kind == "linewise" and "yanked lines" or "yanked")
end

local function shift_lines(start, stop, f)
  local first = V.pos_to_line(start)
  for line = V.pos_to_line(stop), first, -1 do
    V.goto_char(V.line_to_pos(line))
    f()
  end
  V.goto_char(core.first_non_blank_at(V.line_to_pos(first)))
end

function core.op_indent(start, stop)
  shift_lines(start, stop, function()
    if not core.empty_line(V.point()) then V.insert("    ") end
  end)
end

function core.op_outdent(start, stop)
  shift_lines(start, stop, function()
    local n = core.first_non_blank() - core.line_start()
    V.delete_region(core.line_start(), core.line_start() + math.min(4, n))
  end)
end

core.operators = {
  { "d", core.op_delete }, { "c", core.op_change }, { "y", core.op_yank },
  { ">", core.op_indent }, { "<", core.op_outdent },
}

core.pending_operator = nil
core.operator_count = 1
core.operator_count_given = false

function core.start_operator(op)
  return function()
    core.pending_operator = op
    core.operator_count = core.count
    core.operator_count_given = core.count_given
    core.state = "operator"
  end
end

local function finish_operator(start, stop, kind)
  core.state = "normal"
  core.pending_operator(start, stop, kind)
end

function core.operator_motion(motion)
  return function()
    local n = core.operator_count * core.count
    local start = V.point()
    local kind = core.motion_type(motion)
    -- cw changes to the end of the word, like ce.
    if core.pending_operator == core.op_change then
      if motion == "word-forward" then motion = "word-end"
      elseif motion == "WORD-forward" then motion = "WORD-end" end
    end
    if motion == "word-end" or motion == "WORD-end" then kind = "inclusive" end
    -- A count before the operator or the motion: d5G and 5dG both to line 5.
    local target = core.motion_target(motion, start, n, core.count_given or core.operator_count_given)
    -- dw on a line's last word stops at the line's end.
    if target and kind == "exclusive" and (motion == "word-forward" or motion == "WORD-forward")
      and V.pos_to_line(target) > V.pos_to_line(start) then
      target = math.max(start, core.line_end_at(start))
    end
    if not target then
      core.state = "normal"
      return
    end
    local lo, hi = math.min(start, target), math.max(start, target)
    finish_operator(lo, kind == "inclusive" and hi + 1 or hi, kind)
  end
end

function core.operator_run(motion) core.operator_motion(motion)() end

function core.operator_object(object, inner)
  return function()
    local range = object(V.point(), inner)
    if range then
      finish_operator(range[1], range[2], range[3] or "exclusive")
    else
      core.state = "normal"
      V.message("no text object here")
    end
  end
end

function core.operator_lines()
  local n = core.operator_count * core.count
  finish_operator(V.point(), V.line_to_pos(V.pos_to_line(V.point()) + n - 1), "linewise")
end

---- Visual mode
--
-- The selection runs from core.visual_anchor to the cursor, both included.
-- Rust draws whatever core.selection() returns.

core.visual_anchor = 0

-- Visual block (C-v): the rectangle between the anchor and the cursor,
-- columns included. After $ it reaches each line's end.
core.block_to_eol = false

function core.block_top() return math.min(V.pos_to_line(core.visual_anchor), V.pos_to_line(V.point())) end
function core.block_bottom() return math.max(V.pos_to_line(core.visual_anchor), V.pos_to_line(V.point())) end
function core.block_left() return math.min(core.column_at(core.visual_anchor), core.column_at(V.point())) end
function core.block_right() return 1 + math.max(core.column_at(core.visual_anchor), core.column_at(V.point())) end

local function line_length(line)
  local s = V.line_to_pos(line)
  return core.line_end_at(s) - s
end

-- One {start, end} per line of the block; empty on lines too short to reach it.
function core.block_ranges()
  local out = {}
  for line = core.block_top(), core.block_bottom() do
    local s = V.line_to_pos(line)
    local len = core.line_end_at(s) - s
    local a = s + math.min(core.block_left(), len)
    local b = core.block_to_eol and s + len or s + math.min(core.block_right(), len)
    out[#out + 1] = { a, math.max(a, b) }
  end
  return out
end

-- {start, end, type} of the selection.
function core.visual_bounds()
  local lo, hi = math.min(core.visual_anchor, V.point()), math.max(core.visual_anchor, V.point())
  if core.state == "visual-line" then return { core.line_start_at(lo), core.line_end_at(hi), "linewise" } end
  if core.state == "visual-block" then
    return { V.line_to_pos(core.block_top()), V.line_to_pos(core.block_bottom()), "block" }
  end
  return { lo, math.min(V.point_max(), hi + 1), "exclusive" }
end

-- The selected ranges {{start, end}, ...}, for drawing.
function core.selection()
  if not core.visual() then return {} end
  local b = core.visual_bounds()
  if b[3] == "linewise" then return { { b[1], math.min(V.point_max(), b[2] + 1) } } end
  if b[3] == "block" then return core.block_ranges() end
  return { { b[1], b[2] } }
end

-- The block register: a yanked or deleted block, pasted back as a column.
-- It is the register only while the register still holds that text.
core.block_register_lines = nil
local block_register_text = ""

local function is_block_register()
  return core.block_register_lines ~= nil and V.register_get() == block_register_text
end

function core.yank_block(ranges)
  local lines = {}
  for _, r in ipairs(ranges) do lines[#lines + 1] = V.buffer_substring(r[1], r[2]) end
  core.block_register_lines = lines
  block_register_text = table.concat(lines, "\n")
  V.register_set(block_register_text)
end

function core.goto_block_top_left()
  local s = V.line_to_pos(core.block_top())
  V.goto_char(s + math.min(core.block_left(), core.line_end_at(s) - s))
end

function core.delete_block(ranges)
  core.yank_block(ranges)
  for i = #ranges, 1, -1 do V.delete_region(ranges[i][1], ranges[i][2]) end
end

-- I, A and c in a block: type on the first line; ESC copies what was typed
-- to the block's other lines, at the same column. Which lines get it is
-- decided when typing starts: for I and c the lines that reach into the
-- block, for A every line (short ones padded with spaces).
core.block_insert = nil

-- Lines after the first of the block that reach past `column`.
function core.block_lines_reaching(column)
  local out = {}
  for line = core.block_top() + 1, core.block_bottom() do
    if line_length(line) > column then out[#out + 1] = line end
  end
  return out
end

local function block_lines_after_top()
  local out = {}
  for line = core.block_top() + 1, core.block_bottom() do out[#out + 1] = line end
  return out
end

function core.start_block_insert(column, top, targets, to_eol, pad)
  local s = V.line_to_pos(top)
  local len = core.line_end_at(s) - s
  if to_eol then
    V.goto_char(s + len)
  elseif len < column then
    V.goto_char(s + len)
    if pad then V.insert(string.rep(" ", column - len)) end
  else
    V.goto_char(s + column)
  end
  core.block_insert = { column = column, top = top, targets = targets, to_eol = to_eol, pad = pad, start = V.point() }
  core.state = "insert"
end

local function finish_block_insert()
  local b = core.block_insert
  core.block_insert = nil
  local typed = ""
  if V.point() >= b.start and V.pos_to_line(V.point()) == b.top then typed = V.buffer_substring(b.start, V.point()) end
  if typed ~= "" then
    for _, line in ipairs(b.targets) do
      local s = V.line_to_pos(line)
      local len = core.line_end_at(s) - s
      if b.to_eol then
        V.goto_char(s + len)
        V.insert(typed)
      elseif len >= b.column then
        V.goto_char(s + b.column)
        V.insert(typed)
      elseif b.pad then
        V.goto_char(s + len)
        V.insert(string.rep(" ", b.column - len) .. typed)
      end
    end
  end
  V.goto_char(b.start)
end

V.command("block-insert-start", function()
  if core.state == "visual-block" then
    core.start_block_insert(core.block_left(), core.block_top(), core.block_lines_reaching(core.block_left()), false, false)
  else
    core.state = "normal"
    V.goto_char(math.min(core.visual_anchor, V.point()))
    core.call_binding("insert-state")
  end
end)

V.command("block-append", function()
  if core.state == "visual-block" then
    core.start_block_insert(core.block_right(), core.block_top(), block_lines_after_top(), core.block_to_eol, true)
  else
    core.state = "normal"
    V.goto_char(math.min(V.point_max(), 1 + math.max(core.visual_anchor, V.point())))
    core.call_binding("insert-state")
  end
end)

-- O in a block: the cursor goes to the other side, same line.
V.command("visual-swap-columns", function()
  if core.state ~= "visual-block" then return core.call_binding("visual-swap-ends") end
  local ac, pc = core.column_at(core.visual_anchor), core.column_at(V.point())
  local a_line, p_line = core.line_start_at(core.visual_anchor), core.line_start_at(V.point())
  core.visual_anchor = a_line + math.min(pc, core.line_end_at(a_line) - a_line)
  V.goto_char(p_line + math.min(ac, core.line_end_at(p_line) - p_line))
end)

function core.enter_visual(state)
  return function()
    if core.state == state then return core.call_binding("exit-visual") end
    if not core.visual() then core.visual_anchor = V.point() end
    core.block_to_eol = false
    core.state = state
  end
end

V.command("exit-visual", function()
  core.state = "normal"
  core.clamp_to_line()
end)

V.command("visual-swap-ends", function()
  local p = V.point()
  V.goto_char(core.visual_anchor)
  core.visual_anchor = p
end)

function core.visual_operator(op)
  return function()
    local b = core.visual_bounds()
    core.state = "normal"
    op(b[1], b[2], b[3])
  end
end

function core.visual_object(object, inner)
  return function()
    local range = object(V.point(), inner)
    if not range then return end
    if range[3] == "linewise" then core.state = "visual-line" end
    core.visual_anchor = range[1]
    V.goto_char(math.max(range[1], range[2] - 1))
  end
end

V.command("visual-paste", function()
  local text, b = V.register_get(), core.visual_bounds()
  core.state = "normal"
  core.op_delete(b[1], b[2], b[3])
  V.register_set(text)
  core.call_binding("paste-before")
end)

---- Insert state

V.command("normal-state", function()
  core.state = "normal"
  V.completion_close()
  if core.block_insert then finish_block_insert() end
  if core.in_markdown_table() then V.align_table(0) end
  if V.point() > core.line_start() then V.forward_char(-1) end
  V.message("")
end)
V.command("insert-state", function() core.state = "insert" end)
V.command("insert-after", function()
  if V.point() < core.line_end() then V.forward_char(1) end
  core.state = "insert"
end)
V.command("insert-line-start", function()
  V.goto_char(core.first_non_blank())
  core.state = "insert"
end)
V.command("insert-line-end", function()
  V.goto_char(core.line_end())
  core.state = "insert"
end)
V.command("open-below", function()
  V.goto_char(core.line_end())
  V.insert("\n")
  core.state = "insert"
end)
V.command("open-above", function()
  V.goto_char(core.line_start())
  V.insert("\n")
  V.forward_char(-1)
  core.state = "insert"
end)

V.command("newline", function() V.insert("\n") end)
V.command("indent", function() V.insert("    ") end)
V.command("delete-backward", function() V.delete_backward_char(1) end)
V.command("delete-forward", function() V.delete_char(1) end)
V.command("char-left", function() V.forward_char(-1) end)
V.command("char-right", function() V.forward_char(1) end)
V.command("line-up", function() V.forward_line(-1) end)
V.command("line-down-cmd", function() V.forward_line(1) end)

---- Other normal-state commands

V.command("delete-char-under", function()
  if V.point() < core.line_end() then
    V.register_set(V.delete_region(V.point(), V.point() + 1))
    core.clamp_to_line()
  end
end)

local function linewise(text) return text:sub(-1) == "\n" end

-- Pastes the block register as a column, starting at `column` of this line.
local function paste_block(column)
  local first_line = V.pos_to_line(V.point())
  for i, text in ipairs(core.block_register_lines) do
    local line = first_line + i - 1
    if line >= V.line_count() then
      V.goto_char(V.point_max())
      V.insert("\n")
    end
    local s = V.line_to_pos(line)
    local len = core.line_end_at(s) - s
    if len < column then
      V.goto_char(s + len)
      V.insert(string.rep(" ", column - len) .. text)
    else
      V.goto_char(s + column)
      V.insert(text)
    end
  end
  V.goto_char(V.line_to_pos(first_line) + column)
end

V.command("paste-after", function()
  local text = V.register_get()
  if is_block_register() then
    paste_block(core.empty_line(V.point()) and 0 or 1 + core.column_at(V.point()))
  elseif linewise(text) then
    if core.line_end() < V.point_max() then
      V.goto_char(core.line_end() + 1)
      V.insert(text)
      V.forward_line(-1)
    else
      V.goto_char(core.line_end())
      V.insert("\n" .. text:sub(1, -2))
    end
    V.goto_char(core.first_non_blank())
  elseif #text > 0 then
    if V.point() < core.line_end() then V.forward_char(1) end
    V.insert(text)
    V.forward_char(-1)
  end
end)

V.command("paste-before", function()
  local text = V.register_get()
  if is_block_register() then
    paste_block(core.column_at(V.point()))
  elseif linewise(text) then
    V.goto_char(core.line_start())
    V.insert(text)
    V.forward_line(-1)
    V.goto_char(core.first_non_blank())
  elseif #text > 0 then
    V.insert(text)
    V.forward_char(-1)
  end
end)

V.command("evil-undo", function()
  if not V.undo() then V.message("Already at oldest change") end
  core.clamp_to_line()
end)
V.command("evil-redo", function()
  if not V.redo() then V.message("Already at newest change") end
  core.clamp_to_line()
end)

V.command("keyboard-quit", function()
  core.pending_keys = ""
  core.pending_count = ""
  core.char_reader = nil
  core.search_preview = nil
  core.search_highlighted = false
  core.minibuffer_update = function() end
  if V.picker_active() then
    core.close_picker()
  elseif core.state ~= "insert" then
    core.state = "normal"
  end
  if V.minibuffer_active() then V.minibuffer_close() end
  V.message("Quit")
end)

---- The system clipboard
--
-- Yanks and deletes also copy to the system clipboard, and p/P paste what
-- was copied last anywhere: text copied in another app wins over an older
-- yank. vmac.set_clipboard_sync(false) keeps vmac's register to itself.
-- Cmd-V pastes the clipboard at the cursor (normal, insert, prompts, the
-- picker, terminals); Cmd-C copies the visual selection.

V.command("clipboard-paste", function()
  V.insert(V.clipboard_get())
  if V.picker_active() then V.picker_filter(V.minibuffer_contents()) end
end)

V.command("clipboard-copy", function()
  core.visual_operator(core.op_yank)()
  V.clipboard_set(V.register_get())
  V.message("Copied")
end)
