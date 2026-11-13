-- Ghost text: completions from a model running beside vmac
-- (docs/design/autocomplete/, src/complete.rs, engine/).
--
-- Off until you turn it on in init.lua:
--   vmac.set_autocomplete({ enabled = true })
-- with the engine installed (on a Mac with Apple silicon):
--   pip install "vmac-complete[mlx] @ git+https://github.com/Super-Yojan/vmac#subdirectory=engine"
--
-- Options, with their defaults:
--   command     = { "vmac-complete" }   the engine and its arguments
--   backend     = "mlx"                 or "fake": no model, to try the keys
--   model       = ""                    LiquidAI/LFM3-1.7B-MLX-4bit
--   layout      = ""                    spm, psm or prefix; "" lets the engine pick
--   debounce_ms = 40                    the pause in typing before it asks
--   stop        = "block"               or "line"
--   max_tokens  = 64
--
-- In insert state, after a pause in typing, a completion shows dimmed after
-- the cursor. TAB takes it (when the LSP popup is closed), M-f takes a
-- word, M-e a line; typing what it says keeps it; anything else drops it.
-- :autocomplete-toggle turns it on and off, :autocomplete-status says how
-- the engine is and what the last completion cost.

local V = vmac

local A = core.autocomplete or {}
core.autocomplete = A
A.config = A.config or {
  enabled = false, command = { "vmac-complete" }, backend = "mlx", model = "", layout = "",
  debounce_ms = 40, stop = "block", max_tokens = 64,
}

local function apply()
  local c = A.config
  V.autocomplete_configure(c.enabled, c.command, c.backend, c.model, c.layout, c.debounce_ms, c.stop, c.max_tokens)
end

function V.set_autocomplete(opts)
  for k, v in pairs(opts or {}) do A.config[k] = v end
  apply()
end

-- After a typed character: keep the ghost text if it was typed through,
-- else ask again (once typing pauses).
local lsp_after_self_insert = core.after_self_insert
function core.after_self_insert(text)
  lsp_after_self_insert(text)
  if not A.config.enabled then return end
  if not V.ghost_type_through(text) then V.autocomplete_request() end
end

local function ask_after(binding)
  return function()
    if type(binding) == "function" then binding() else core.call_binding(binding) end
    V.ghost_dismiss()
    if A.config.enabled then V.autocomplete_request() end
  end
end

local keys = core.completion_insert_keys
local lsp_ret, lsp_del = keys.RET, keys.DEL

-- TAB: the LSP popup's pick when it's open, else the ghost text, else as before.
keys.TAB = function()
  if V.completion_active() then V.completion_accept()
  elseif V.ghost_active() then V.ghost_accept("all")
  else core.call_binding("table-next-cell") end
end
keys["M-f"] = function() if V.ghost_active() then V.ghost_accept("word") end end
keys["M-e"] = function() if V.ghost_active() then V.ghost_accept("line") end end
-- A new line and a backspace ask too.
keys.RET = ask_after(lsp_ret)
keys.DEL = ask_after(lsp_del)

V.command("autocomplete-toggle", function()
  V.set_autocomplete({ enabled = not A.config.enabled })
  V.message("Autocomplete " .. (A.config.enabled and "on" or "off"))
end)

V.command("autocomplete-status", function()
  local stats = V.autocomplete_stats()
  V.message("Autocomplete " .. (A.config.enabled and "on" or "off") .. ": " .. V.autocomplete_status()
    .. (stats ~= "" and ("  " .. stats) or ""))
end)
