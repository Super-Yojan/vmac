-- Coding agents, two ways.
--
-- ACP agents (claude, codex, gemini; built in) answer in vmac's *agent*
-- pane: edits wait for review, it reads your unsaved buffers, @file works.
-- CLI agents run their own interface (Claude Code, Codex, Gemini CLI, pi,
-- aider...) in a terminal beside your code, as in your shell.
--
-- SPC a b picks either; the agent keys work with both:
--   SPC a a   open it (to type to it)      SPC a t   show or hide its pane
--   SPC a s   ask about the line or selection (a CLI agent gets the file and
--             line numbers in one line, and reads the file itself)
--   SPC a e   ask about the file's errors   SPC a h   reopen a past session
--   SPC a k   stop it                       SPC a n   start it afresh
--
-- A CLI agent writes to disk itself: vmac reloads the open files it changes
-- and keeps what they were, so its changes wait for review too (green bands,
-- ga / gx, SPC a r), as an ACP agent's do.
--
-- Add your own in init.lua:
--   vmac.agent_backend("pi", { terminal = "pi" })
--   vmac.agent_backend("aider", { terminal = "aider --no-auto-commits" })
-- `resume` reopens a past session from SPC a h, {id} standing for its id.

vmac.agent_backend("claude-cli", { terminal = "claude", resume = "claude --resume {id}" })
vmac.agent_backend("codex-cli", { terminal = "codex" })
vmac.agent_backend("gemini-cli", { terminal = "gemini" })

-- SPC a t: the CLI agent's pane, shown or hidden; with an ACP agent, *agent*.
vmac.command("agent-pane-toggle", function()
  if vmac.agent_kind() ~= "cli" then
    vmac.agent_show()
    return
  end
  if vmac.agent_cli_here() then
    if not vmac.delete_window() then
      vmac.next_buffer(1)
    end
  else
    vmac.agent_cli_open()
  end
end)
vmac.map("normal", "SPC a t", "agent-pane-toggle")
