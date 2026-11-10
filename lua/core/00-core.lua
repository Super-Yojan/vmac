-- vmac's core, in Lua. Modal, evil-style.
--
-- The files in lua/core/ load in name order, one area each: this one (the
-- key dispatcher, keymaps, states and commands), motions, operators, the
-- minibuffer and picker, windows, the file tree, LSP, the agent... Rust
-- provides primitives in the `vmac` table (vmac.insert, vmac.char_at,
-- vmac.delete_region, vmac.find_file, vmac.message, ...); everything here
-- can be changed while vmac runs: edit these files or ~/.config/vmac/init.lua
-- and save, or evaluate Lua with SPC ; or :lua.
--
-- The core's state and functions are in the global `core` table
-- (core.state, core.keymaps.normal, core.modeline, ...): redefine any of
-- them in init.lua. Functions are looked up when called, so a redefinition
-- takes effect at once.

local V = vmac
core = {}

---- Small helpers

function core.contains(items, x)
  for _, item in ipairs(items) do
    if item == x then return true end
  end
  return false
end

function core.starts_with(s, prefix) return s:sub(1, #prefix) == prefix end

-- Characters, not bytes (the buffer counts characters).
function core.length(s) return utf8.len(s) or #s end

function core.blank(pos)
  local class = V.char_class(pos)
  return class == "space" or class == "newline"
end

---- Commands
--
-- Commands live in a registry, looked up by name on every key, so
-- vmac.command("x", fn) again changes what every key bound to "x" does.

V.commands = V.commands or {}
core.commands = V.commands

-- A command callable by name from anywhere (keys, SPC :, M-x, Lua).
function V.command(name, fn) core.commands[name] = fn end

function core.is_command(name) return core.commands[name] ~= nil end

-- Runs a binding: a command's name, or a function of no arguments.
function core.call_binding(binding)
  if type(binding) == "function" then return binding() end
  local command = core.commands[binding]
  if command then return command() end
  V.message(tostring(binding) .. " is not a command")
end

-- Runs a command right after this one (by name).
function V.run(name) core.call_binding(name) end

---- Keymaps
--
-- A keymap maps key sequences in Emacs notation ("g g", "SPC f f", "C-r",
-- "ESC", "RET") to a binding. Each state has its own; some buffers (the
-- file tree, *agent*, the dashboard...) have one in front of normal state's.

core.keymaps = {
  normal = {}, visual = {}, operator = {}, insert = {}, minibuffer = {},
  picker_normal = {}, tree = {}, agent = {}, agent_insert = {}, review = {},
  dashboard = {}, terminal = {}, terminal_normal = {}, media = {},
  device = {}, device_insert = {},
}

-- core.define_key("normal", "Q", "some-command") or a keymap table.
function core.define_key(map, key, binding)
  if type(map) == "string" then
    local name = map:gsub("-", "_")
    map = core.keymaps[name]
    if not map then
      V.message("No keymap named " .. name)
      return
    end
  end
  map[key] = binding
end

-- Binds keys ("SPC i d") in a state (normal, insert, visual, operator,
-- minibuffer, terminal, terminal-normal, review, agent, tree, dashboard)
-- to a command's name or a function.
function V.map(state, keys, command) core.define_key(state, keys, command) end

-- The first binding of `seq` in a chain of keymaps.
function core.lookup(chain, seq)
  for _, map in ipairs(chain) do
    local binding = map[seq]
    if binding ~= nil then return binding end
  end
  return nil
end

-- Whether some binding continues the sequence.
function core.is_prefix(chain, seq)
  local p = seq .. " "
  for _, map in ipairs(chain) do
    for key in pairs(map) do
      if core.starts_with(key, p) then return true end
    end
  end
  return false
end

-- All next keys after a prefix, as {{next, label}, ...}: a sub-prefix gets
-- "+key", a binding its command's name ("..." for a function).
function core.next_keys(chain, prefix)
  local p, seen, out = prefix .. " ", {}, {}
  for _, map in ipairs(chain) do
    for key in pairs(map) do
      if core.starts_with(key, p) then
        local next = key:sub(#p + 1):match("^[^ ]+")
        if next and not seen[next] then
          seen[next] = true
          local full = prefix .. " " .. next
          local label
          if core.is_prefix(chain, full) then
            label = "+" .. next
          else
            local binding = core.lookup(chain, full)
            label = type(binding) == "string" and binding or "..."
          end
          out[#out + 1] = { next, label }
        end
      end
    end
  end
  table.sort(out, function(a, b)
    local x, y = a[1]:lower(), b[1]:lower()
    if x == y then return a[1] < b[1] end
    return x < y
  end)
  return out
end

---- States: normal, insert, visual, visual-line, visual-block, operator
-- (after d, c, y...)

core.state = "normal"

function core.visual()
  return core.state == "visual" or core.state == "visual-line" or core.state == "visual-block"
end

-- The minibuffer is for typing, except in a picker, which has its own
-- normal mode (like telescope): ESC there leaves typing, and Vim keys move
-- through the results or edit the query.
function core.picker_normal()
  return V.minibuffer_active() and V.picker_active() and core.state ~= "insert"
end

function core.inserting()
  return core.state == "insert" or (V.minibuffer_active() and not V.picker_active())
end

core.tree_buffer = "*tree*"

function core.terminal_insert()
  return core.state == "insert" and not V.minibuffer_active() and V.terminal_here()
end

function core.terminal_normal()
  return core.state == "normal" and not V.minibuffer_active() and V.terminal_here()
end

-- Typing into a device's screen (mobile.lua): keys go to the device.
function core.device_insert()
  return core.state == "insert" and not V.minibuffer_active() and V.device_here()
end
-- What typing a key into a device does (mobile.lua sets it).
function core.device_type(_key, _text) end

-- Buffers with keymaps of their own, by name: { name, normal state's
-- keymap, insert state's, visual state's } (the last two optional), e.g.
-- { "*git-status*", "git_status" }. The name can be a function instead,
-- asked of the current buffer (Markdown files get notes' keys that way).
core.buffer_keymaps = core.buffer_keymaps or {}

function core.buffer_keymap(mode)
  local name = V.buffer_name()
  local slot = ({ normal = 2, insert = 3, visual = 4 })[mode or "normal"]
  for _, e in ipairs(core.buffer_keymaps) do
    local match = type(e[1]) == "function" and e[1]() or e[1] == name
    if match and e[slot] then return core.keymaps[e[slot]] end
  end
end

-- The keymaps in effect, first match winning.
function core.current_keymaps()
  local k = core.keymaps
  local plain = not V.minibuffer_active()
  local normal = core.state == "normal" and plain
  if core.terminal_insert() then return { k.terminal } end
  if core.terminal_normal() then return { k.terminal_normal, k.normal } end
  if core.device_insert() then return { k.device_insert } end
  if normal and V.device_here() then return { k.device, k.normal } end
  if normal and V.buffer_name() == core.tree_buffer then return { k.tree, k.normal } end
  if normal and V.dashboard_here() then return { k.dashboard, k.normal } end
  if normal and V.review_here() then return { k.review, k.normal } end
  if normal and V.media_here() then return { k.media, k.normal } end
  local own = (normal and core.buffer_keymap("normal")) or (core.state == "insert" and plain and core.buffer_keymap("insert"))
  if own then return { own, core.state == "insert" and k.insert or k.normal } end
  local own_visual = plain and core.visual() and core.buffer_keymap("visual")
  if own_visual then return { own_visual, k.visual } end
  if normal and V.agent_here() then return { k.agent, k.normal } end
  if core.state == "insert" and plain and V.agent_here() then return { k.agent_insert, k.insert } end
  if core.picker_normal() then
    if core.visual() then return { k.visual } end
    if core.state == "operator" then return { k.operator } end
    return { k.picker_normal, k.normal }
  end
  if V.minibuffer_active() then return { k.minibuffer } end
  if core.state == "insert" then return { k.insert } end
  if core.visual() then return { k.visual } end
  if core.state == "operator" then return { k.operator } end
  return { k.normal }
end

-- Rust asks this for Markdown: as typed while you type or select in it,
-- rendered (or its markup hidden) otherwise.
function core.markdown_raw()
  return (core.state == "insert" or core.visual()) and not V.minibuffer_active()
end

-- Rust asks this for the cursor: a block, or a bar when typing.
function core.cursor_shape() return core.inserting() and "bar" or "box" end

---- The key dispatcher
--
-- Rust calls core.dispatch_key(key, text) for every keystroke. `text` is
-- what the key would type ("" for ESC, RET, C-r). A count typed first (3j,
-- 2dd, d2w) is in core.count while the binding runs. A named command runs
-- that many times; a function binding runs once and reads the count itself.

core.pending_keys = ""
core.pending_count = ""
core.count = 1
core.count_given = false

-- core.read_char(f): the next key is a character for f, not a command (f, t, r).
core.char_reader = nil
function core.read_char(f) core.char_reader = f end

-- Called after every key while the minibuffer is open (incremental search).
core.minibuffer_update = function() end

local digits = { ["1"] = true, ["2"] = true, ["3"] = true, ["4"] = true, ["5"] = true,
                 ["6"] = true, ["7"] = true, ["8"] = true, ["9"] = true }

function core.count_digit(text)
  if core.inserting() or core.pending_keys ~= "" then return false end
  -- On the dashboard, digits open its projects.
  if V.dashboard_here() and not V.minibuffer_active() then return false end
  -- In *agent*, while it waits for permission, digits answer it.
  if V.agent_here() and V.agent_permission_waiting() and not V.minibuffer_active() then return false end
  return digits[text] or (text == "0" and core.pending_count ~= "")
end

---- Repeating the last change: .
--
-- Every key is recorded from the start of a command in normal state until
-- the editor is back in normal state with nothing pending. If the buffer
-- changed in between, those keys are the last change, and . types them
-- again: dw, ciwfoo<ESC>, 3x, vjd, d/foo<RET> and so on. A count on .
-- replaces the original one. Undo, redo and : commands aren't changes.

local recording, recording_buffer, recording_revision = {}, "", 0
core.last_change = {}
local replaying = false

function core.idle_normal()
  return core.state == "normal" and core.pending_keys == "" and core.pending_count == ""
    and not core.char_reader and not V.minibuffer_active()
end

local function drop_count(keys)
  local i = 1
  while keys[i] and (digits[keys[i][1]] or keys[i][1] == "0") do i = i + 1 end
  return { table.unpack(keys, i) }
end

-- Whether recorded keys (in order) are a change . should repeat.
local function repeatable(keys)
  local first = drop_count(keys)[1]
  return first ~= nil and not core.contains({ "u", "C-r", ".", ":" }, first[1])
end

function core.dispatch_key(key, text)
  if not replaying and core.idle_normal() then
    recording = {}
    recording_buffer = V.buffer_name()
    recording_revision = V.buffer_revision()
  end
  if not replaying then recording[#recording + 1] = { key, text } end
  core.dispatch_key_1(key, text)
  if V.minibuffer_active() then core.minibuffer_update() end
  if not replaying and core.idle_normal() and #recording > 0 then
    if V.buffer_name() == recording_buffer and not V.buffer_read_only()
      and V.buffer_revision() ~= recording_revision and repeatable(recording) then
      core.last_change = recording
    end
    recording = {}
  end
end

V.command("repeat-last-change", function()
  if #core.last_change == 0 then
    V.message("No change to repeat")
    return
  end
  local keys = core.last_change
  if core.count_given then
    keys = {}
    for c in tostring(core.count):gmatch(".") do keys[#keys + 1] = { c, c } end
    for _, k in ipairs(drop_count(core.last_change)) do keys[#keys + 1] = k end
  end
  replaying = true
  for _, k in ipairs(keys) do core.dispatch_key(k[1], k[2]) end
  replaying = false
end)

function core.dispatch_key_1(key, text)
  if core.char_reader then
    local f = core.char_reader
    core.char_reader = nil
    if text ~= "" then
      f(text)
    elseif key == "ESC" then
      core.call_binding("keyboard-quit")
    else
      if core.state == "operator" then core.state = "normal" end
      V.message(key .. " is not a character")
    end
    return
  end
  core.dispatch_binding(key, text)
end

function core.dispatch_binding(key, text)
  if core.count_digit(text) then
    core.pending_count = core.pending_count .. text
    return
  end
  local seq = core.pending_keys == "" and key or (core.pending_keys .. " " .. key)
  local chain = core.current_keymaps()
  local binding = core.lookup(chain, seq)
  if binding ~= nil then
    core.count_given = core.pending_count ~= ""
    core.count = core.count_given and tonumber(core.pending_count) or 1
    -- The "SPC f-" shown while a sequence was pending goes away.
    if core.pending_keys ~= "" then V.message("") end
    core.pending_keys = ""
    core.pending_count = ""
    if core.state == "normal" then V.undo_boundary() end
    if type(binding) == "function" then
      binding()
    else
      for _ = 1, core.count do core.call_binding(binding) end
    end
  elseif core.is_prefix(chain, seq) then
    core.pending_keys = seq
    V.message(seq .. "-")
    core.whichkey_open(seq)
  elseif core.pending_keys == "" and core.terminal_insert() then
    -- Typing into a terminal: every other key goes to its program.
    V.terminal_send(key, text)
  elseif core.pending_keys == "" and core.device_insert() then
    core.device_type(key, text)
  elseif core.pending_keys == "" and core.inserting() and text ~= "" then
    V.insert(text)
    if core.state == "insert" and not V.minibuffer_active() then core.after_self_insert(text) end
  else
    core.pending_keys = ""
    core.pending_count = ""
    if core.state == "operator" then core.state = "normal" end
    V.message(seq .. " is undefined")
  end
end

---- Which-key: the keys that can follow a pending prefix.

function core.whichkey_open(prefix)
  local flat = {}
  for _, pair in ipairs(core.next_keys(core.current_keymaps(), prefix)) do
    flat[#flat + 1] = pair[1]
    flat[#flat + 1] = pair[2]
  end
  if #flat > 0 then V.whichkey_show(flat) end
end

---- The minibuffer
--
-- core.read(prompt, callback) opens it; RET calls the callback with what
-- was typed, ESC cancels.

core.minibuffer_callback = function() end

function core.read(prompt, callback)
  core.minibuffer_callback = callback
  V.minibuffer_open(prompt)
end

V.command("minibuffer-exit", function()
  local input = V.minibuffer_contents()
  core.minibuffer_update = function() end
  V.minibuffer_close()
  core.minibuffer_callback(input)
end)

-- vmac.prompt(label, fn, initial): asks for a line in the minibuffer and
-- calls fn with it (when you press RET; ESC drops it).
function V.prompt(label, fn, initial)
  core.read(label, fn)
  if initial and initial ~= "" then V.insert(initial) end
end

---- Ticks: about ten times a second, Rust calls core.tick, which calls each
-- function in core.tick_hooks, by name, so a redefinition replaces it.
-- One returns true when it changed something to draw. Keep them cheap.

core.tick_hooks = core.tick_hooks or {}

function core.tick()
  local changed = false
  for _, hook in pairs(core.tick_hooks) do
    if hook() then changed = true end
  end
  return changed
end

---- Events Rust raises (server replies, the agent asking...): each calls
-- core.events[name] with a list of strings.

core.events = {}

-- ("lua-event" hook arg ...) calls vmac.hooks[hook](arg, ...).
core.events["lua-event"] = function(args)
  local hook = V.hooks[args[1]]
  if hook then hook(table.unpack(args, 2)) end
end

-- Functions in vmac.hooks replace the core's: modeline, lsp_servers,
-- syntax_theme; and some are called on events (agent_permission,
-- agent_tool_call, agent_turn_end, review_comment_at).
V.hooks = V.hooks or {}

---- Evaluating Lua: SPC ; and :lua show what an expression is.

function core.eval(code)
  local chunk = load("return " .. code, "eval") or load(code, "eval")
  if not chunk then
    local _, err = load(code, "eval")
    V.message("lua: " .. tostring(err))
    return
  end
  local ok, value = pcall(chunk)
  if not ok then
    V.message("lua: " .. tostring(value):match("^[^\n]*"))
  elseif value ~= nil then
    V.message(tostring(value))
  end
end

-- print shows in the echo area.
function print(...)
  local parts = {}
  for i = 1, select("#", ...) do parts[#parts + 1] = tostring(select(i, ...)) end
  V.message(table.concat(parts, " "))
end

-- Puts the editor in a state ("normal", "insert"...).
function core.enter_state(state) core.state = state end
