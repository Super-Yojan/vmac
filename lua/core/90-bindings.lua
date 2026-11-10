-- Default bindings.

local K = core.keymaps

local function bind(map, pairs_)
  for _, pair in ipairs(pairs_) do map[pair[1]] = pair[2] end
end

-- Motions move in normal and visual state, and complete an operator.
for _, pair in ipairs(core.motion_keys) do
  local key, motion = pair[1], pair[2]
  K.normal[key] = function() core.move_by(motion) end
  K.visual[key] = function() core.move_by(motion) end
  K.operator[key] = core.operator_motion(motion)
end

-- Operators: start one in normal state, act on the selection in visual.
for _, pair in ipairs(core.operators) do
  local key, op = pair[1], pair[2]
  K.normal[key] = core.start_operator(op)
  K.visual[key] = core.visual_operator(op)
  K.operator[key] = core.operator_lines
end

-- Text objects: after an operator, or to select in visual state.
for _, pair in ipairs(core.object_keys) do
  local key, object = pair[1], pair[2]
  K.operator["i " .. key] = core.operator_object(object, true)
  K.operator["a " .. key] = core.operator_object(object, false)
  K.visual["i " .. key] = core.visual_object(object, true)
  K.visual["a " .. key] = core.visual_object(object, false)
end

-- f t F T ; , / ? * #: the same keys move, extend a selection, or complete
-- an operator; only what is done with the motion differs.
for _, pair in ipairs({ { K.normal, core.move_by }, { K.visual, core.move_by }, { K.operator, core.operator_run } }) do
  local map, then_ = pair[1], pair[2]
  for _, kind in ipairs({ "f", "t", "F", "T" }) do map[kind] = core.find_with(then_)(kind) end
  map[";"] = core.repeat_find(then_, false)
  map[","] = core.repeat_find(then_, true)
  map["/"] = core.search_prompt(true, then_)
  map["?"] = core.search_prompt(false, then_)
  map["*"] = core.search_word(true, then_)
  map["#"] = core.search_word(false, then_)
end

local function shorthand(op, motion)
  return function()
    core.start_operator(op)()
    core.operator_motion(motion)()
  end
end

bind(K.normal, {
  { "i", "insert-state" }, { "a", "insert-after" },
  { "I", "insert-line-start" }, { "A", "insert-line-end" },
  { "o", "open-below" }, { "O", "open-above" },
  { "v", core.enter_visual("visual") }, { "V", core.enter_visual("visual-line") },
  { "C-v", core.enter_visual("visual-block") },
  -- A function binding runs once, whatever the count; it reads the count.
  { ".", function() core.call_binding("repeat-last-change") end },
  { "x", "delete-char-under" },
  { "D", shorthand(core.op_delete, "line-end") },
  { "C", shorthand(core.op_change, "line-end") },
  { "Y", function() core.start_operator(core.op_yank)() core.operator_lines() end },
  { "p", "paste-after" }, { "P", "paste-before" },
  { "u", "evil-undo" }, { "C-r", "evil-redo" }, { "s-v", "clipboard-paste" },
  { ":", "ex" }, { "ESC", "keyboard-quit" },
  -- Doom's leader
  { "SPC :", "execute-extended-command" }, { "SPC ;", "eval-expression" },
  { "SPC f f", "find-file-command" }, { "SPC SPC", "find-file-command" },
  { "SPC f r", "recent-files-command" }, { "SPC b h", "dashboard" },
  { "SPC p p", "project-switch" }, { "SPC p a", "project-add" },
  { "SPC p d", "project-forget-command" }, { "SPC f P", "open-config" },
  { "SPC o p", "tree-toggle" }, { "SPC o P", "tree-open" },
  { "SPC o t", "terminal-toggle" }, { "SPC o T", "terminal-new" },
  { "SPC o n", "terminal-next" }, { "SPC o N", "terminal-previous" },
  { "SPC o l", "terminal-pick" }, { "SPC o k", "terminal-kill" },
  { "SPC h F", "font-picker" }, { "SPC h m", "package-manager" }, { "SPC h t", "theme-picker" },
  { "SPC h p", "plugin-manager" },
  { "SPC m a", "table-align" }, { "SPC m p", "preview" }, { "SPC m s", "pdf-forward-search" }, { "SPC t s", "toggle-pdf-follow" },
  { "SPC t m", "markdown-conceal" },
  -- Language servers
  { "] d", "diagnostic-next" }, { "[ d", "diagnostic-previous" },
  { "g d", "goto-definition" }, { "g r", "find-references" },
  { "SPC c r", "rename-symbol" }, { "SPC c a", "code-actions" },
  { "SPC c f", "format-buffer" }, { "SPC c R", "lsp-restart-command" },
  { "K", "hover-documentation" }, { "C-o", "jump-back-command" },
  { "SPC c x", "diagnostic-list" },
  -- The coding agent
  { "SPC a a", "agent-ask" }, { "SPC a o", "agent-open" },
  { "SPC a c", "agent-cancel-run" }, { "SPC a p", "agent-permission" },
  { "SPC a h", "agent-history" }, { "SPC a H", "agent-history-all" },
  { "SPC a e", "agent-fix-problems" }, { "SPC a b", "agent-choose" },
  { "SPC a r", "review-open" }, { "SPC a A", "review-accept-all" },
  { "SPC a X", "review-reject-all" },
  { "] h", "review-next" }, { "[ h", "review-previous" },
  { "g a", "review-accept" }, { "g x", "review-reject" },
  { "SPC a k", "agent-kill" }, { "SPC a n", "agent-new" }, { "SPC a s", "agent-ask-selection" },
  -- Buffers
  { "SPC b b", "switch-buffer" }, { "SPC ,", "switch-buffer" },
  { "SPC b n", "buffer-next" }, { "SPC b p", "buffer-previous" },
  { "] b", "buffer-next" }, { "[ b", "buffer-previous" },
  { "SPC b k", "buffer-kill" }, { "SPC b d", "buffer-kill" }, { "SPC b N", "buffer-new" },
  -- Windows
  { "C-w v", "split-right" }, { "C-w s", "split-below" },
  { "C-w h", "window-left" }, { "C-w j", "window-down" },
  { "C-w k", "window-up" }, { "C-w l", "window-right" },
  { "C-w C-h", "window-left" }, { "C-w C-j", "window-down" },
  { "C-w C-k", "window-up" }, { "C-w C-l", "window-right" },
  { "C-w w", "window-next" }, { "C-w C-w", "window-next" }, { "C-w W", "window-previous" },
  { "C-w c", "window-close" }, { "C-w q", "quit-window" }, { "C-w o", "window-only" },
  { "C-w =", "window-balance" }, { "SPC w =", "window-balance" },
  { "SPC w v", "split-right" }, { "SPC w s", "split-below" },
  { "SPC w h", "window-left" }, { "SPC w j", "window-down" },
  { "SPC w k", "window-up" }, { "SPC w l", "window-right" },
  { "SPC w w", "window-next" }, { "SPC w d", "window-close" },
  { "SPC w c", "window-close" }, { "SPC w o", "window-only" },
  -- Scrolling
  { "C-d", "scroll-half-down" }, { "C-u", "scroll-half-up" },
  { "C-f", "scroll-page-down" }, { "C-b", "scroll-page-up" },
  { "C-e", "scroll-line-down" }, { "C-y", "scroll-line-up" },
  { "z z", "recenter-center" }, { "z t", "recenter-top" }, { "z b", "recenter-bottom" },
  { "SPC f s", "save" }, { "SPC q q", "quit" },
})

bind(K.visual, {
  { "v", core.enter_visual("visual") }, { "V", core.enter_visual("visual-line") },
  { "C-v", core.enter_visual("visual-block") },
  { "o", "visual-swap-ends" }, { "O", "visual-swap-columns" },
  { "I", "block-insert-start" }, { "A", "block-append" },
  { "x", core.visual_operator(core.op_delete) },
  { "p", "visual-paste" }, { "ESC", "exit-visual" }, { ":", "ex" },
  { "C-d", "scroll-half-down" }, { "C-u", "scroll-half-up" },
  { "C-f", "scroll-page-down" }, { "C-b", "scroll-page-up" },
  { "z z", "recenter-center" }, { "SPC c a", "code-actions" },
  { "SPC c f", "format-buffer" }, { "SPC a s", "agent-ask-selection" },
  { "s-c", "clipboard-copy" },
})

K.operator.ESC = "keyboard-quit"

bind(K.insert, {
  { "ESC", "normal-state" }, { "RET", "newline" }, { "TAB", "table-next-cell" }, { "S-TAB", "table-previous-cell" },
  { "DEL", "delete-backward" }, { "<delete>", "delete-forward" },
  { "<left>", "char-left" }, { "<right>", "char-right" }, { "<down>", "line-down-cmd" }, { "<up>", "line-up" },
  { "s-v", "clipboard-paste" },
})
-- Completion's keys, over the ones above while its popup is open.
for key, binding in pairs(core.completion_insert_keys) do K.insert[key] = binding end

bind(K.minibuffer, {
  { "RET", "minibuffer-exit" }, { "ESC", "minibuffer-escape" },
  { "DEL", "delete-backward" }, { "<left>", "char-left" }, { "<right>", "char-right" },
  { "C-n", "picker-next" }, { "C-j", "picker-next" }, { "<down>", "picker-next" },
  { "C-p", "picker-previous" }, { "C-k", "picker-previous" }, { "<up>", "picker-previous" },
  { "s-v", "clipboard-paste" },
})

bind(K.picker_normal, {
  { "j", function() vmac.picker_move(core.count) end },
  { "k", function() vmac.picker_move(-core.count) end },
  { "<down>", function() vmac.picker_move(core.count) end },
  { "<up>", function() vmac.picker_move(-core.count) end },
  { "C-n", "picker-next" }, { "C-p", "picker-previous" },
  { "C-j", "picker-next" }, { "C-k", "picker-previous" },
  { "g g", "picker-first" }, { "G", "picker-last" },
  { "C-d", "picker-page-down" }, { "C-u", "picker-page-up" },
  { "RET", "minibuffer-exit" }, { "q", "keyboard-quit" }, { "ESC", "keyboard-quit" },
})
