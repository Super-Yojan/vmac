;;;; Commands
;;
;; Commands live in a registry, looked up by name on every key. That is what
;; makes redefinition work: Steel resolves calls between Scheme functions when
;; it compiles them, so a plain (define (x) ...) evaluated later would not
;; change what an already-loaded key calls. (defcommand (x) ...) does, for
;; every key bound to 'x.

(define commands (hash))

(define (register-command! name procedure)
  (set! commands (hash-insert commands name procedure)))

(define-syntax defcommand
  (syntax-rules ()
    [(defcommand (name) body ...)
     (begin (define (name) body ...) (register-command! 'name name))]))

(define (command? name) (hash-contains? commands name))

;; Runs a binding: a command name, or a procedure of no arguments.
(define (call-binding binding)
  (cond ((procedure? binding) (binding))
        ((command? binding) ((hash-ref commands binding)))
        (else (message (string-append (symbol->string binding) " is not a command")))))

