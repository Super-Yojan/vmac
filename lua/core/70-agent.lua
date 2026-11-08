-- The coding agent
--
-- A coding agent (Claude Code by default) works in the project, over the
-- Agent Client Protocol: it reads files (from your buffers, saved or not),
-- edits them and runs tools, asking first when it needs permission.
--
-- The *agent* buffer holds the conversation, and below its rule the prompt
-- you type: an ordinary text area (every Vim key works; the conversation
-- above can be read, searched and yanked, not changed). RET in normal state
-- sends it (C-RET or Cmd-RET from insert state). On a tool call, RET or TAB
-- opens and closes its details; on a file it touched, RET visits it.
--
-- When it asks permission (to edit, to run a command, to start coding
-- after a plan), *agent* shows the question, what it's about (the plan, the
-- command, the diff) and the choices: press a choice's number, or RET on
-- it. SPC a p picks from a list instead.
--
-- SPC a a: type to the agent      SPC a s: ask about the line or selection
-- SPC a c: cancel the run         SPC a p: answer a permission it waits on
-- SPC a n: a new conversation     SPC a k: stop the agent
-- SPC a o: show *agent*           SPC a e: ask about this file's problems
-- SPC a h: reopen a past session of this project; SPC a H: of any project.
--   With Claude Code these are all its sessions (the CLI's and the desktop
--   app's too, from ~/.claude/projects); other agents list their own. The
--   agent starts in the session's folder and replays it into *agent*.
--
-- In the prompt, @ at the start of a word picks a project file to attach:
-- @src/lib.rs sends the file's text along (your buffer's, saved or not).
-- Commands the agent runs show in terminals (*agent: cargo test*): the
-- tool call shows their last lines; RET on it opens the terminal.
--
-- Its edits are made at once (so it can build and test them) and saved,
-- and wait for your review: changed lines have a green band, deletions a
-- red rule. ]h and [h move between them, ga accepts the one under the
-- cursor, gx rejects it (puts the old lines back, one u undoes that). SPC a
-- r lists them all in *agent-review* (the same keys, RET goes to one);
-- SPC a A / SPC a X accept / reject them all.
-- vmac.set_agent_review(false) accepts edits as they're made.
--
-- :agent PROMPT asks from the command line. Another agent: :agent-command
-- CMD, or vmac.set_agent_command("codex-acp") in init.lua.

local V = vmac

V.command("agent-ask", function()
  V.agent_focus_prompt()
  core.state = "insert"
end)

V.command("agent-open", function() V.agent_show() end)
V.command("agent-history", function() V.agent_sessions(false) end)
V.command("agent-history-all", function() V.agent_sessions(true) end)

-- Raised with the sessions to pick from: "3. Title · ~/project · 2h ago".
core.events["agent-sessions-event"] = function(items)
  core.pick("Agent sessions", items, "", function(item) V.agent_open_session(core.leading_number(item)) end)
end

V.command("agent-cancel-run", function() V.agent_cancel() end)
V.command("agent-kill", function() V.agent_stop() end)
V.command("agent-new", function() V.agent_new_session() end)
V.command("agent-send", function()
  V.agent_activate()
  core.state = "normal"
end)
V.command("agent-toggle", function() V.agent_toggle_at_point() end)

V.command("review-open", function() V.review_show() end)
V.command("review-accept", function() V.review_decide(true) end)
V.command("review-reject", function() V.review_decide(false) end)
V.command("review-accept-all", function() V.review_decide_all(true, true) end)
V.command("review-reject-all", function() V.review_decide_all(false, true) end)
V.command("review-next", function() V.review_jump(true) end)
V.command("review-previous", function() V.review_jump(false) end)
V.command("review-go", function() V.review_visit() end)

-- q: closes its window, or shows another buffer in the last one.
V.command("review-close", function()
  if not V.delete_window() then V.next_buffer(1) end
end)

core.define_key("review", "RET", "review-go")
core.define_key("review", "q", "review-close")

-- SPC a s: a question about the selection (or this line), with its code.
V.command("agent-ask-selection", function()
  local bounds
  if core.visual() then
    bounds = core.visual_bounds()
  else
    bounds = { core.line_start(), core.line_end() }
  end
  core.state = "normal"
  core.read("Ask about this: ", function(question) V.agent_ask_about(bounds[1], bounds[2], question) end)
end)

-- i a o... in the conversation go to the prompt; in the prompt, as usual.
local function agent_typing(key)
  return function()
    if V.agent_in_prompt() then
      core.call_binding(core.keymaps.normal[key])
    else
      V.agent_focus_prompt()
      core.state = "insert"
    end
  end
end

for key, binding in pairs({ RET = "agent-send", TAB = "agent-toggle", ["z a"] = "agent-toggle", ["C-c"] = "agent-cancel-run" }) do
  core.define_key("agent", key, binding)
end
for _, key in ipairs({ "i", "a", "I", "A", "o", "O" }) do core.define_key("agent", key, agent_typing(key)) end

-- @ at the start of a word: pick a file to attach (ESC just types @).
V.command("agent-attach", function()
  V.insert("@")
  local before = V.point() > 1 and V.char_at(V.point() - 2) or " "
  if core.contains({ " ", "\n", "\t", "" }, before) then
    local root = V.agent_root()
    core.pick("Attach file", V.project_files(root), root, function(path) V.insert(path .. " ") end)
  end
end)

core.define_key("agent_insert", "@", "agent-attach")
core.define_key("agent_insert", "C-RET", "agent-send")
core.define_key("agent_insert", "s-RET", "agent-send")

-- SPC a e: the file's problems, with a question (default: fix them).
V.command("agent-fix-problems", function()
  core.read("Ask about the problems: ", function(question) V.agent_ask_about_problems(question) end)
end)

-- While it waits for permission, 1-9 in *agent* answer with that choice.
for n = 1, 9 do
  core.define_key("agent", tostring(n), function()
    if V.agent_permission_waiting() then V.agent_permit(n - 1) end
  end)
end

V.command("agent-permission", function()
  local options = V.agent_permission_options()
  if #options == 0 then return V.message("The agent isn't waiting for permission") end
  core.pick(V.agent_permission_question(), options, "", function(item) V.agent_permit(core.leading_number(item) - 1) end)
end)

---- Choosing the agent, rules and hooks
--
-- Agents by name: claude (the default), codex and gemini are built in; add
-- your own with vmac.agent_backend("name", { command = "..." }). SPC a b or
-- :agent-use NAME picks one (from the next start; SPC a n restarts).
--
-- Permission rules: before asking you, vmac asks
-- vmac.hooks.agent_permission(title, kind), kind being read, edit, delete,
-- move, search, execute, fetch, think or other. Return "allow" (this once),
-- "always", "reject" or "ask" (or nothing: ask). E.g. to let it read and
-- search freely:
--   vmac.hooks.agent_permission = function(title, kind)
--     if kind == "read" or kind == "search" then return "allow" end
--   end
--
-- Hooks: vmac.hooks.agent_tool_call(title, kind, status) when a tool call
-- starts or changes status (pending, in_progress, completed, failed), and
-- vmac.hooks.agent_turn_end(reason) when the agent finishes (end_turn,
-- cancelled, max_tokens, refusal, error).

-- vmac.agent_backend("codex", { command = "codex-acp" }) (or just the
-- command line) for an ACP agent in *agent*, or { terminal = "pi", resume =
-- "pi --resume {id}" } for a CLI agent in a terminal beside your code; then
-- vmac.agent_use("codex") or SPC a b.
-- With no arguments, the agent in use's name.
local current_backend = V.agent_backend
function V.agent_backend(name, spec)
  if name == nil then return current_backend() end
  if type(spec) == "table" and spec.terminal then
    V.agent_define_cli(name, spec.terminal, spec.resume or "")
    return
  end
  V.agent_define(name, type(spec) == "table" and spec.command or spec)
end

V.command("agent-choose", function()
  core.pick("Agent", V.agent_backends(), "", function(name) V.agent_use(name) end)
end)

local function hook(name, ...)
  local f = V.hooks[name]
  if f then return f(...) end
end

-- Raised when the agent asks: a rule answers, or you do (at once, unless
-- you're mid-prompt).
core.events["agent-ask-permission"] = function(args)
  local title, kind = args[1], args[2]
  local rule = hook("agent_permission", title, kind) or "ask"
  if rule == "allow" and V.agent_permit_kind("allow_once") then return end
  if rule == "always" and V.agent_permit_kind("allow_always") then return end
  if rule == "reject" and V.agent_permit_kind("reject_once") then return end
  if V.minibuffer_active() or core.state == "insert" then
    -- Typing (a prompt, insert state): don't take the cursor away.
    V.message("The agent asks: " .. V.agent_permission_question() .. " (SPC a p, or answer in *agent*)")
  else
    -- Else: *agent* at the question, to read what it's about first.
    V.agent_show_permission()
    V.message(V.agent_permission_question() .. " Read it, then press a choice's number")
  end
end

core.events["agent-tool-event"] = function(args) hook("agent_tool_call", args[2], args[3], args[4]) end
core.events["agent-done-event"] = function(args) hook("agent_turn_end", args[1]) end
