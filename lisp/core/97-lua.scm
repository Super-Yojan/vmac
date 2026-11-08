;;;; Lua
;;
;; Lua (~/.config/vmac/init.lua, :lua) has the same primitives as Scheme, in
;; the vmac table (buffer-name -> vmac.buffer_name). Its commands and keys
;; land here, so they mix freely with Scheme's: a Lua command runs from any
;; key, SPC :, or (call-binding 'name). Lua reaches Scheme with
;; vmac.scheme(code), which runs right after the current command.

;; vmac.command(name, fn): a command named `name` that runs the Lua function.
(define (register-lua-command! name)
  (register-command! name (lambda () (lua-call (symbol->string name)))))

;; vmac.map(state, keys, command).
(define (lua-define-key state keys command)
  (keymap-define! state keys command))
