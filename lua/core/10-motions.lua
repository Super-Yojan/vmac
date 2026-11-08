-- Motions, finding a character (f t F T ; ,), search (/ ? n N * #) and
-- text objects (iw, a", i(, ip...).

local V = vmac

---- Lines

function core.line_start_at(pos) return V.line_beginning_position(pos) end
function core.line_end_at(pos) return V.line_end_position(pos) end
function core.line_start() return core.line_start_at(V.point()) end
function core.line_end() return core.line_end_at(V.point()) end
function core.last_char_at(pos) return math.max(core.line_start_at(pos), core.line_end_at(pos) - 1) end
function core.clamp_to_line()
  local last = core.last_char_at(V.point())
  if V.point() > last then V.goto_char(last) end
end

function core.first_non_blank_at(pos)
  local p, e = core.line_start_at(pos), core.line_end_at(pos)
  while p < e and V.char_class(p) == "space" do p = p + 1 end
  return p
end
function core.first_non_blank() return core.first_non_blank_at(V.point()) end

function core.empty_line(pos) return core.line_start_at(pos) == core.line_end_at(pos) end

function core.column_at(pos) return pos - core.line_start_at(pos) end

---- Motions
--
-- A motion has a type and a step: step takes a position and returns where
-- one repetition lands (nil: it can't go). The type says what an operator
-- covers: exclusive (up to the target), inclusive (the target too) or
-- linewise (whole lines). core.define_motion(name, type, step), then bind
-- it in core.motion_keys, and it works for moving, in visual mode and after
-- an operator (d, c, y).

core.motions = {}

function core.define_motion(name, kind, step) core.motions[name] = { kind, step } end

function core.motion_type(name) return core.motions[name][1] end

-- Where n repetitions land, or nil when the motion can't go (f with no
-- such character, a search with no match).
function core.motion_target(name, pos, n)
  local step = core.motions[name][2]
  local p = pos
  for _ = 1, n do
    if not p then return nil end
    p = step(p)
  end
  return p
end

-- Words: "w" stops where the character class changes (word, punctuation);
-- "W" only at whitespace.
function core.same_word_class(a, b, big)
  if big then return not core.blank(a) and not core.blank(b) end
  return V.char_class(a) == V.char_class(b)
end

local function word_forward(pos, big)
  local max = V.point_max()
  local p = pos
  while p + 1 < max and not core.blank(p) and core.same_word_class(p, p + 1, big) do p = p + 1 end
  local q = core.blank(p) and p or p + 1
  while q < max and core.blank(q) and not core.empty_line(q) do q = q + 1 end
  return math.min(q, max)
end

local function word_backward(pos, big)
  local p = math.max(0, pos - 1)
  while p > 0 and core.blank(p) do p = p - 1 end
  local q = p
  while q > 0 and not core.blank(q - 1) and core.same_word_class(q - 1, q, big) do q = q - 1 end
  return q
end

local function word_end(pos, big)
  local max = V.point_max()
  local p = math.min(pos + 1, math.max(0, max - 1))
  while p + 1 < max and core.blank(p) do p = p + 1 end
  local q = p
  while q + 1 < max and core.same_word_class(q, q + 1, big) do q = q + 1 end
  return q
end

local function line_down(pos, n)
  return V.line_to_pos(math.min(V.line_count() - 1, V.pos_to_line(pos) + n))
end

local M = core.define_motion
M("left", "exclusive", function(p) return p > core.line_start_at(p) and p - 1 or p end)
M("right", "exclusive", function(p) return p < core.line_end_at(p) and p + 1 or p end)
M("down", "linewise", function(p) return line_down(p, 1) end)
M("up", "linewise", function(p) return V.line_to_pos(math.max(0, V.pos_to_line(p) - 1)) end)
M("line-start", "exclusive", function(p) return core.line_start_at(p) end)
M("first-non-blank", "exclusive", function(p) return core.first_non_blank_at(p) end)
M("line-end", "inclusive", function(p) return core.last_char_at(p) end)
M("word-forward", "exclusive", function(p) return word_forward(p, false) end)
M("WORD-forward", "exclusive", function(p) return word_forward(p, true) end)
M("word-backward", "exclusive", function(p) return word_backward(p, false) end)
M("WORD-backward", "exclusive", function(p) return word_backward(p, true) end)
M("word-end", "inclusive", function(p) return word_end(p, false) end)
M("WORD-end", "inclusive", function(p) return word_end(p, true) end)
M("buffer-start", "linewise", function() return 0 end)
M("buffer-end", "linewise", function() return V.line_to_pos(V.line_count() - 1) end)

core.motion_keys = {
  { "h", "left" }, { "l", "right" }, { "j", "down" }, { "k", "up" },
  { "<left>", "left" }, { "<right>", "right" }, { "<down>", "down" }, { "<up>", "up" },
  { "0", "line-start" }, { "^", "first-non-blank" }, { "$", "line-end" },
  { "w", "word-forward" }, { "W", "WORD-forward" }, { "b", "word-backward" }, { "B", "WORD-backward" },
  { "e", "word-end" }, { "E", "WORD-end" }, { "g g", "buffer-start" }, { "G", "buffer-end" },
  { "n", "search-next" }, { "N", "search-previous" },
}

-- Moving the cursor. j and k keep the column (the buffer remembers it), and
-- outside visual mode the cursor stays on a character, never past the line.
function core.move_by(motion)
  if core.state == "visual-block" then
    if motion == "line-end" then
      core.block_to_eol = true
    elseif not core.contains({ "down", "up", "buffer-start", "buffer-end" }, motion) then
      core.block_to_eol = false
    end
  end
  if motion == "down" then
    V.forward_line(core.count)
  elseif motion == "up" then
    V.forward_line(-core.count)
  elseif motion == "buffer-end" then
    V.goto_char(core.first_non_blank_at(core.motion_target(motion, V.point(), 1)))
  elseif motion == "buffer-start" then
    V.goto_char(0)
  else
    local target = core.motion_target(motion, V.point(), core.count)
    if target then V.goto_char(target) end
  end
  if not core.visual() then core.clamp_to_line() end
end

---- Finding a character: f t F T, repeated with ; and ,
--
-- On the current line. f and F land on the character, t and T just before
-- it. They read the character after the key, then act as the motion
-- "find-char", so they work after operators (dt), cf,) and in visual mode.

local function find_on_line(c, from, step)
  local s, e = core.line_start_at(from), core.line_end_at(from)
  local p = from
  while p >= s and p < e do
    if V.char_at(p) == c then return p end
    p = p + step
  end
  return nil
end

-- kind is "f", "t", "F" or "T". `again` (from ; and ,) makes t and T skip
-- a match right next to the cursor, so repeating them moves on.
local function find_step(kind, c, again)
  local skip = again and 2 or 1
  return function(p)
    if kind == "f" then return find_on_line(c, p + 1, 1) end
    if kind == "F" then return find_on_line(c, p - 1, -1) end
    if kind == "t" then
      local q = find_on_line(c, p + skip, 1)
      return q and q - 1
    end
    local q = find_on_line(c, p - skip, -1)
    return q and q + 1
  end
end

local function find_type(kind) return (kind == "f" or kind == "t") and "inclusive" or "exclusive" end

core.last_find = nil

local opposite = { f = "F", F = "f", t = "T", T = "t" }

-- Makes "find-char" mean this find, then runs it as `then_` would any motion.
local function run_find(kind, c, again, then_)
  core.define_motion("find-char", find_type(kind), find_step(kind, c, again))
  then_("find-char")
end

function core.find_with(then_)
  return function(kind)
    return function()
      local count = core.count
      core.read_char(function(c)
        core.last_find = { kind, c }
        core.count = count
        run_find(kind, c, false, then_)
      end)
    end
  end
end

function core.repeat_find(then_, reverse)
  return function()
    if not core.last_find then
      V.message("No previous f or t")
      return
    end
    local kind = core.last_find[1]
    if reverse then kind = opposite[kind] end
    run_find(kind, core.last_find[2], true, then_)
  end
end

---- Search: / ? n N * #
--
-- Patterns are regexes, case-insensitive unless they have a capital
-- (smartcase), searched across the buffer with wrap-around. n and N are
-- motions, so d/foo, dn and v/foo work. Matches are highlighted until ESC
-- or :noh.

core.search_pattern = ""
core.search_forward = true
core.search_highlighted = false
core.search_preview = nil

-- The next match start after pos in a direction, wrapping; nil with none.
local function search_from(pos, pattern, forward)
  if forward then
    local p = V.search_forward(pattern, pos + 1)
    if p >= 0 then return p end
    local w = V.search_forward(pattern, 0)
    if w >= 0 then V.message("search wrapped") return w end
  else
    local p = V.search_backward(pattern, pos)
    if p >= 0 then return p end
    local w = V.search_backward(pattern, V.point_max())
    if w >= 0 then V.message("search wrapped") return w end
  end
  return nil
end

local function search_step(forward_wanted)
  return function(p)
    if core.search_pattern == "" then
      V.message("No previous search")
      return nil
    end
    local found = search_from(p, core.search_pattern, forward_wanted == core.search_forward)
    if not found then V.message("Pattern not found: " .. core.search_pattern) end
    return found
  end
end

core.define_motion("search-next", "exclusive", search_step(true))
core.define_motion("search-previous", "exclusive", search_step(false))

-- What the window highlights: what's being typed after / or ?, else the
-- last pattern until ESC or :noh.
function core.search_highlight()
  if core.search_preview then return core.search_preview end
  if core.search_highlighted then return core.search_pattern end
  return ""
end

-- / and ?: read a pattern (highlighting as you type), then run `then_`
-- with the "search-next" motion.
function core.search_prompt(forward, then_)
  return function()
    local count = core.count
    core.minibuffer_update = function() core.search_preview = V.minibuffer_contents() end
    core.read(forward and "/" or "?", function(pattern)
      core.search_preview = nil
      if pattern ~= "" then core.search_pattern = pattern end
      core.search_forward = forward
      core.search_highlighted = true
      core.count = count
      then_("search-next")
    end)
  end
end

-- * and #: the word under the cursor, as a whole word.
function core.search_word(forward, then_)
  return function()
    local range = core.word_object(false)(V.point(), true)
    if not range or core.blank(V.point()) then
      V.message("No word under cursor")
      return
    end
    core.search_pattern = "\\b" .. V.regex_quote(V.buffer_substring(range[1], range[2])) .. "\\b"
    core.search_forward = forward
    core.search_highlighted = true
    V.goto_char(range[1])
    then_("search-next")
  end
end

V.command("no-highlight", function() core.search_highlighted = false end)

---- Text objects
--
-- A text object is a function (pos, inner) -> {start, end[, type]} or nil,
-- end exclusive. Bind it in core.object_keys: "i" + key is inner, "a" +
-- key around.

local function extend_left(pos, same)
  while pos > 0 and same(pos - 1) do pos = pos - 1 end
  return pos
end
local function extend_right(pos, same)
  local max = V.point_max()
  while pos < max and same(pos) do pos = pos + 1 end
  return pos
end

function core.word_object(big)
  return function(pos, inner)
    local class = V.char_class(pos)
    if class == "eof" or class == "newline" then return nil end
    local blank = core.blank(pos)
    local function same(p)
      if blank then return V.char_class(p) == "space" end
      return core.same_word_class(p, pos, big)
    end
    local function space(p) return V.char_class(p) == "space" end
    local start, stop = extend_left(pos, same), extend_right(pos, same)
    if inner then return { start, stop } end
    -- "a": the word with the spaces after it, else before it.
    if blank then return { start, extend_right(stop, function(p) return core.same_word_class(p, stop, big) end) } end
    if extend_right(stop, space) > stop then return { start, extend_right(stop, space) } end
    return { extend_left(start, space), stop }
  end
end

-- Brackets: the nearest pair around pos, counting nesting.
function core.bracket_object(open, close)
  return function(pos, inner)
    local start
    local p, depth = pos, 0
    while p >= 0 do
      local c = V.char_at(p)
      if c == open and depth == 0 then start = p break end
      if c == open then depth = depth - 1
      elseif c == close and p ~= pos then depth = depth + 1 end
      p = p - 1
    end
    if not start then return nil end
    local stop
    local max = V.point_max()
    p, depth = start + 1, 0
    while p < max do
      local c = V.char_at(p)
      if c == close and depth == 0 then stop = p break end
      if c == close then depth = depth - 1
      elseif c == open then depth = depth + 1 end
      p = p + 1
    end
    if not stop then return nil end
    if inner then return { start + 1, stop } end
    return { start, stop + 1 }
  end
end

-- Quotes, on the current line: the pair around pos, else the next pair.
function core.quote_object(q)
  return function(pos, inner)
    local open
    local e = core.line_end_at(pos)
    local p = core.line_start_at(pos)
    while p < e do
      if V.char_at(p) == q then
        if not open then
          open = p
        elseif p >= pos then
          if inner then return { open + 1, p } end
          return { open, p + 1 }
        else
          open = nil
        end
      end
      p = p + 1
    end
    return nil
  end
end

-- Paragraphs: the run of non-empty (or empty) lines around pos; "a" adds
-- the empty lines after it.
function core.paragraph_object(pos, inner)
  local kind = core.empty_line(pos)
  local function same(line) return core.empty_line(V.line_to_pos(line)) == kind end
  local last_line = V.line_count() - 1
  local first = V.pos_to_line(pos)
  while first > 0 and same(first - 1) do first = first - 1 end
  local last = V.pos_to_line(pos)
  while last < last_line and same(last + 1) do last = last + 1 end
  if not inner then
    while last < last_line and core.empty_line(V.line_to_pos(last + 1)) ~= kind do last = last + 1 end
  end
  return { V.line_to_pos(first), core.line_end_at(V.line_to_pos(last)), "linewise" }
end

local paren, square, brace, angle =
  core.bracket_object("(", ")"), core.bracket_object("[", "]"), core.bracket_object("{", "}"), core.bracket_object("<", ">")
core.object_keys = {
  { "w", core.word_object(false) }, { "W", core.word_object(true) },
  { "(", paren }, { ")", paren }, { "b", paren },
  { "[", square }, { "]", square },
  { "{", brace }, { "}", brace }, { "B", brace },
  { "<", angle }, { ">", angle },
  { '"', core.quote_object('"') }, { "'", core.quote_object("'") }, { "`", core.quote_object("`") },
  { "p", core.paragraph_object },
}
