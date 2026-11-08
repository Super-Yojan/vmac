;;;; Lua
;;
;; Lua (~/.config/gmacs/init.lua, :lua) has the same primitives as Scheme, in
;; the gmacs table (buffer-name -> gmacs.buffer_name). Its commands and keys
;; land here, so they mix freely with Scheme's: a Lua command runs from any
;; key, SPC :, or (call-binding 'name). Lua reaches Scheme with
;; gmacs.scheme(code), which runs right after the current command.

;; gmacs.command(name, fn): a command named `name` that runs the Lua function.
(define (register-lua-command! name)
  (register-command! name (lambda () (lua-call (symbol->string name)))))

;; gmacs.map(state, keys, command).
(define (lua-define-key state keys command)
  (keymap-define! state keys command))
