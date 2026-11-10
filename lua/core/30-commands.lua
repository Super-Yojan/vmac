-- The picker, : commands, scrolling, the mouse, buffers and windows, fonts
-- and terminals.

local V = vmac

---- The picker (telescope-style)
--
-- core.pick(title, items, preview_root, on_pick) shows items filtered
-- fuzzily as you type, in a panel in the middle of the window. C-n/C-p,
-- C-j/C-k or the arrows move; RET calls on_pick with the item; ESC closes.
-- When preview_root is a folder, items are paths in it and the selected one
-- is previewed. Rust does the filtering; what to list and what picking does
-- is up to the caller.

-- The buffer's state before the picker opened, to go back to.
local picker_return_state = "normal"

function core.close_picker()
  if V.picker_active() then
    V.picker_close()
    core.state = picker_return_state
  end
end

function core.pick(title, items, preview_root, on_pick)
  picker_return_state = core.state == "insert" and "insert" or "normal"
  V.picker_open(title, items, preview_root or "")
  core.state = "insert"
  core.minibuffer_update = function() V.picker_filter(V.minibuffer_contents()) end
  core.read("> ", function()
    local choice = V.picker_selection()
    core.close_picker()
    if choice == "" then
      V.message("No match")
    else
      on_pick(choice)
    end
  end)
end

-- vmac.pick(title, items, fn, root): the fuzzy picker over items; fn gets
-- the one picked. With root (a folder), items are paths in it, previewed.
function V.pick(title, items, fn, root) core.pick(title, items, root, fn) end

function core.find_file_in(root)
  core.pick("Find file", V.project_files(root), root, function(path) V.find_file(root .. "/" .. path) end)
end

V.command("find-file-command", function() core.find_file_in(V.project_root()) end)

V.command("picker-next", function() V.picker_move(1) end)
V.command("picker-previous", function() V.picker_move(-1) end)
V.command("picker-first", function() V.picker_select(0) end)
V.command("picker-last", function() V.picker_select(-1) end)
V.command("picker-page-down", function() V.picker_move(10) end)
V.command("picker-page-up", function() V.picker_move(-10) end)

-- ESC while typing: in a picker, go to its normal mode; elsewhere, cancel.
V.command("minibuffer-escape", function()
  core.call_binding(V.picker_active() and "normal-state" or "keyboard-quit")
end)
V.command("save", function() V.save_buffer() end)
V.command("quit", function() V.quit_editor() end)

---- : commands
--
-- Each name in core.ex_commands runs with what follows it (`:e FILE` gets
-- "FILE"); any other command's name runs that command. Add your own:
--   core.ex_commands["hello"] = function(arg) vmac.message("hi " .. arg) end

local function or_say(say, set)
  return function(a)
    if a == "" then V.message(say()) else set(a) end
  end
end

local function split(where)
  return function(arg)
    V.split_window(where)
    if arg ~= "" then V.find_file(arg) end
  end
end

local function alias(names, fn)
  for _, name in ipairs(names) do core.ex_commands[name] = fn end
end

core.ex_commands = {}
alias({ "w" }, function() V.save_buffer() end)
alias({ "q" }, function() core.call_binding("quit-window") end)
alias({ "qa", "qall" }, function() V.quit_editor() end)
alias({ "wq", "x" }, function() V.save_buffer() core.call_binding("quit-window") end)
alias({ "e" }, function(arg) V.find_file(arg) end)
alias({ "bn", "bnext" }, function() V.next_buffer(1) end)
alias({ "bp", "bprevious" }, function() V.next_buffer(-1) end)
alias({ "bd", "bdelete" }, function() V.kill_buffer(false) end)
alias({ "bd!", "bdelete!" }, function() V.kill_buffer(true) end)
alias({ "ls", "buffers" }, function() core.call_binding("switch-buffer") end)
alias({ "b" }, function(arg)
  if not V.switch_to_buffer(arg) then V.message("No buffer named " .. arg) end
end)
alias({ "sp", "split" }, split("below"))
alias({ "vs", "vsplit" }, split("right"))
alias({ "new" }, function() core.choose_new("below") end)
alias({ "vnew" }, function() core.choose_new("right") end)
alias({ "on", "only" }, function() V.delete_other_windows() end)
alias({ "clo", "close" }, function() core.call_binding("window-close") end)
alias({ "lua", "eval" }, function(arg) core.eval(arg) end)
alias({ "term", "terminal" }, function(arg)
  if V.terminal_open("", arg) then core.state = "insert" end
end)
alias({ "font" }, or_say(function()
  return "font: " .. (V.font() == "" and "the theme's" or V.font())
end, V.set_font))
alias({ "fonts" }, function() core.call_binding("font-picker") end)
alias({ "theme" }, or_say(function() return "theme: " .. V.theme_name() .. " (" .. V.theme() .. ")" end, V.set_theme))
alias({ "terminal-shell" }, or_say(function() return "terminals run " .. V.terminal_shell() end, function(a)
  V.set_terminal_shell(a)
  V.message("new terminals run " .. a)
end))
alias({ "terminal-backend" }, or_say(function() return "terminals run on: " .. V.terminal_backend() end, function(a)
  V.set_terminal_backend(a)
  V.message("new terminals run on: " .. a)
end))
alias({ "lsp-restart", "LspRestart" }, function() V.lsp_restart() end)
alias({ "project" }, function(arg)
  if arg == "" then core.call_binding("project-switch") else core.project_switch_to(arg) end
end)
alias({ "dashboard" }, function() core.call_binding("dashboard") end)
alias({ "agent" }, function(arg)
  if arg == "" then
    core.call_binding("agent-ask")
  else
    V.agent_show()
    V.agent_prompt(arg)
  end
end)
alias({ "agent-sessions", "sessions" }, function(arg) V.agent_sessions(arg == "all") end)
alias({ "agent-use" }, function(arg)
  if arg == "" then core.call_binding("agent-choose") else V.agent_use(arg) end
end)
alias({ "agent-command" }, or_say(function() return "the agent runs: " .. V.agent_command() end, V.set_agent_command))
alias({ "plugins" }, function() core.call_binding("plugin-manager") end)
alias({ "packages", "Mason" }, function() core.call_binding("package-manager") end)
alias({ "install" }, function(arg)
  if arg == "" then core.call_binding("package-manager") else V.package_install(arg) end
end)
alias({ "uninstall" }, function(arg) V.package_uninstall(arg) end)
alias({ "auto-install" }, function(arg)
  if arg ~= "" then V.set_auto_install(core.contains({ "on", "yes", "true" }, arg)) end
  V.message("auto-install: " .. (V.auto_install() and "on" or "off"))
end)
alias({ "plugin-install" }, function(arg) V.plugin_install(arg) end)
alias({ "plugin-update" }, function() V.plugin_update() end)
alias({ "plugin-remove" }, function(arg) core.plugin_remove_asking(arg) end)
alias({ "noh", "nohlsearch" }, function() core.call_binding("no-highlight") end)

function core.ex_dispatch(line)
  local name, arg = line:match("^(%S*)%s?(.*)$")
  if name == "" then return end
  local ex = core.ex_commands[name]
  if ex then
    ex(arg)
  elseif core.is_command(name) then
    core.call_binding(name)
  else
    V.message("Not an editor command: " .. name)
  end
end

V.command("ex", function() core.read(":", core.ex_dispatch) end)
V.command("eval-expression", function() core.read("Lua: ", core.eval) end)
V.command("execute-extended-command", function()
  core.read("M-x ", function(name) core.call_binding(name) end)
end)

---- Scrolling: C-d C-u C-f C-b C-e C-y zz zt zb

-- Keeps the cursor inside the window after it scrolls.
local function keep_cursor_in_window()
  local top = V.window_top_line()
  local bottom = top + V.window_height() - 1
  local line = V.pos_to_line(V.point())
  if line < top then
    V.goto_char(core.first_non_blank_at(V.line_to_pos(top)))
  elseif line > bottom then
    V.goto_char(core.first_non_blank_at(V.line_to_pos(bottom)))
  end
end

-- Scrolls the window and moves the cursor the same number of lines.
local function scroll_with_cursor(n)
  V.scroll_window(n)
  V.forward_line(n)
  if not core.visual() then core.clamp_to_line() end
end

local function half() return math.max(1, V.window_height() // 2) end
local function page() return math.max(1, V.window_height() - 2) end
V.command("scroll-half-down", function() scroll_with_cursor(half()) end)
V.command("scroll-half-up", function() scroll_with_cursor(-half()) end)
V.command("scroll-page-down", function() scroll_with_cursor(page()) end)
V.command("scroll-page-up", function() scroll_with_cursor(-page()) end)
V.command("scroll-line-down", function() V.scroll_window(1) keep_cursor_in_window() end)
V.command("scroll-line-up", function() V.scroll_window(-1) keep_cursor_in_window() end)
V.command("recenter-center", function() V.recenter("center") end)
V.command("recenter-top", function() V.recenter("top") end)
V.command("recenter-bottom", function() V.recenter("bottom") end)

---- The mouse
--
-- Rust selects the window under a click, then calls these with the buffer
-- position clicked (the wheel scrolls without them). A click puts the
-- cursor there (and leaves visual state); a double click selects the word,
-- or opens the file tree's entry; a triple click selects the line. Dragging
-- selects, as v does. Programs in terminals that ask for the mouse get it
-- instead; hold shift to select their text.

function core.mouse_click(pos, clicks)
  V.completion_close()
  -- A click cancels a key sequence half typed (SPC ...).
  core.pending_keys = ""
  core.pending_count = ""
  if core.visual() then core.state = "normal" end
  V.goto_char(pos)
  if core.inserting() then
    -- Typing goes on where it was clicked.
  elseif clicks == 2 and V.buffer_name() == core.tree_buffer then
    core.call_binding("tree-activate")
  elseif clicks == 2 then
    for _, k in ipairs({ "v", "i", "w" }) do core.dispatch_key(k, k) end
  elseif clicks >= 3 then
    core.dispatch_key("V", "V")
  else
    core.clamp_to_line()
  end
  -- Keys typed for it (v i w) open which-key on the i; close it, and any open before.
  V.whichkey_hide()
  V.message("")
end

function core.mouse_drag(pos)
  if not core.visual() then
    V.completion_close()
    core.state = "visual"
    core.visual_anchor = V.point()
  end
  V.goto_char(pos)
end

---- Buffers and windows
--
-- Every open file is a buffer. Windows show buffers; split them with C-w v
-- (beside) and C-w s (below), move with C-w h/j/k/l, close with C-w c.
-- Each window keeps its own cursor, even on the same buffer.

V.command("switch-buffer", function()
  core.pick("Switch buffer", V.buffer_list(), "", function(name) V.switch_to_buffer(name) end)
end)
V.command("buffer-next", function() V.next_buffer(1) end)
V.command("buffer-previous", function() V.next_buffer(-1) end)
V.command("buffer-kill", function() V.kill_buffer(false) end)
V.command("buffer-kill-force", function() V.kill_buffer(true) end)
V.command("buffer-new", function() core.choose_new() end)

-- A new buffer asks what it's for: a terminal, an empty buffer to edit, or
-- a file. `side` ("below", "right") splits the window first, once chosen.
core.new_choices = { "Terminal", "Editor", "Open file" }
function core.choose_new(side)
  core.pick("New", core.new_choices, "", function(choice)
    if side then V.split_window(side) end
    if choice == "Terminal" then
      if V.terminal_open("", "") then core.state = "insert" end
    elseif choice == "Open file" then
      core.call_binding("find-file-command")
    else
      V.new_buffer("*scratch*")
      core.state = "normal"
    end
  end)
end

V.command("split-right", function() V.split_window("right") end)
V.command("split-below", function() V.split_window("below") end)
V.command("window-left", function() V.select_window_in_direction("left") end)
V.command("window-right", function() V.select_window_in_direction("right") end)
V.command("window-up", function() V.select_window_in_direction("up") end)
V.command("window-down", function() V.select_window_in_direction("down") end)
V.command("window-next", function() V.other_window(1) end)
V.command("window-previous", function() V.other_window(-1) end)
V.command("window-close", function()
  if not V.delete_window() then V.message("Can't close the last window") end
end)
V.command("window-only", function() V.delete_other_windows() end)
-- Drag a border between windows with the mouse to size them; C-w = (or
-- SPC w =) makes them all the same size again.
V.command("window-balance", function() V.balance_windows() end)

-- :q closes the window, and quits from the last one.
V.command("quit-window", function()
  if not V.delete_window() then V.quit_editor() end
end)

---- Fonts
--
-- The editor's font starts as an installed Nerd Font (for the icons eza,
-- starship and others print), else the theme's. :font NAME or
-- vmac.set_font("...") changes it; :fonts or SPC h F picks from those
-- installed.

V.command("font-picker", function()
  core.pick("Font", V.font_list(), "", function(name) V.set_font(name) end)
end)

---- Terminals
--
-- A terminal is a program on a pseudo-terminal, in a read-only buffer: in
-- normal state its screen is text (motions, search, yank); in insert state
-- every key goes to the program, except ESC (back to normal state). C-\
-- sends the program an ESC. p pastes the yanked text into it.
--
-- Where it runs is a backend: "local" (built in) or one a plugin adds with
-- vmac.terminal_backend (herdr's hc, ...). SPC o t opens one below (or
-- closes it), SPC o T in this window; :term CMD runs CMD; :terminal-backend
-- NAME picks the default. With no command a terminal runs the shell: fish
-- when installed, else $SHELL; :terminal-shell PATH (or
-- vmac.set_terminal_shell) changes it.

-- Terminal backends: where a terminal's program runs, beside the built-in
-- "local" PTY. A backend is a table of functions:
--   start(command, cwd, rows, cols)   (command "" means a shell) returns
--     a handle, then vmac polls send and read; or
--     { handle = h, command = "tool attach h" }: vmac runs that command on
--     a local PTY, which carries keys, screen and size (tmux, ssh, hc...)
--   send(handle, text, key)   text it types, or a key name (enter, ctrl+c, up)
--   read(handle) -> the screen as text
--   alive(handle) -> bool     (optional)
--   kill(handle)              (optional; also called for attached terminals)
V.terminal_backends = {}
function V.terminal_backend(name, backend) V.terminal_backends[name] = backend end

V.command("terminal-new", function()
  if V.terminal_open("", "") then core.state = "insert" end
end)

-- SPC o t: a terminal below, reusing the running one; from a terminal, close it.
V.command("terminal-toggle", function()
  if V.terminal_here() then
    core.state = "normal"
    if not V.delete_window() then V.next_buffer(1) end
    return
  end
  local running = V.terminal_buffer_name()
  V.split_window("below")
  if running == "" then V.terminal_open("", "") else V.switch_to_buffer(running) end
  core.state = "insert"
end)

-- Like tmux windows: open as many as you like and move between them in a
-- window. SPC o T (s-t) opens another, SPC o n / SPC o N (s-] / s-[, or gt
-- / gT in normal state) go to the next / previous, SPC o l picks one,
-- SPC o k ends this one, :terminal-rename NAME names it. A shell's buffer
-- closes when it exits; the mode line shows which terminal (2/3) it is.
-- Going to another terminal keeps the state: typing stays typing.
local function cycle(n)
  if not V.next_terminal(n) then V.message("No terminals (SPC o T opens one)") end
end
V.command("terminal-next", function() cycle(1) end)
V.command("terminal-previous", function() cycle(-1) end)
V.command("terminal-pick", function()
  local names = V.terminal_list()
  if #names == 0 then
    V.message("No terminals (SPC o T opens one)")
    return
  end
  core.pick("Terminal", names, "", function(name)
    V.switch_to_buffer(name)
    core.state = "insert"
  end)
end)
V.command("terminal-kill", function()
  if not V.close_terminal() then V.message("Not a terminal") return end
  if V.terminal_here() then core.state = "insert" else core.state = "normal" end
end)
alias({ "terminal-rename" }, function(name)
  if not V.terminal_here() then V.message("Not a terminal") return end
  if name == "" then V.message("Usage: :terminal-rename NAME") return end
  if V.rename_buffer(name) then V.message("terminal named " .. name) else V.message(name .. " is taken") end
end)

V.command("terminal-type", function() core.state = "insert" end)
V.command("terminal-paste", function() V.terminal_send("", V.register_get()) end)
V.command("terminal-send-escape", function() V.terminal_send("ESC", "") end)
V.command("terminal-paste-clipboard", function() V.terminal_send("", V.clipboard_get()) end)

for key, command in pairs({ ESC = "normal-state", ["C-\\"] = "terminal-send-escape", ["s-v"] = "terminal-paste-clipboard",
    ["s-t"] = "terminal-new", ["s-]"] = "terminal-next", ["s-}"] = "terminal-next",
    ["s-["] = "terminal-previous", ["s-{"] = "terminal-previous" }) do
  core.define_key("terminal", key, command)
  if key:sub(1, 2) == "s-" and key ~= "s-v" then core.define_key("terminal_normal", key, command) end
end
core.define_key("terminal_normal", "g t", "terminal-next")
core.define_key("terminal_normal", "g T", "terminal-previous")
for _, key in ipairs({ "i", "a", "I", "A", "o", "O" }) do core.define_key("terminal_normal", key, "terminal-type") end
core.define_key("terminal_normal", "p", "terminal-paste")
core.define_key("terminal_normal", "P", "terminal-paste")
