;;;; The coding agent
;;
;; A coding agent (Claude Code by default) works in the project, over the
;; Agent Client Protocol: it reads files (from your buffers, saved or not),
;; edits them and runs tools, asking first when it needs permission.
;;
;; The *agent* buffer holds the conversation, and below its rule the prompt
;; you type: an ordinary text area (every Vim key works; the conversation
;; above can be read, searched and yanked, not changed). RET in normal state
;; sends it (C-RET or Cmd-RET from insert state). On a tool call, RET or TAB opens and
;; closes its details; on a file it touched, RET visits it.
;;
;; SPC a a: type to the agent      SPC a s: ask about the line or selection
;; SPC a c: cancel the run         SPC a p: answer a permission it waits on
;; SPC a n: a new conversation     SPC a k: stop the agent
;; SPC a o: show *agent*
;;
;; :agent PROMPT asks from the command line. Another agent: :agent-command
;; CMD, or (set-agent-command "codex-acp") in init.scm,
;; vmac.set_agent_command("codex-acp") in init.lua.

(defcommand (agent-ask)
  (agent-focus-prompt)
  (set! evil-state 'insert))

(defcommand (agent-open) (agent-show))
(defcommand (agent-cancel-run) (agent-cancel))
(defcommand (agent-kill) (agent-stop))
(defcommand (agent-new) (agent-new-session))
(defcommand (agent-send) (agent-activate) (set! evil-state 'normal))
(defcommand (agent-toggle) (agent-toggle-at-point))

;; SPC a s: a question about the selection (or this line), with its code.
(defcommand (agent-ask-selection)
  (let ((bounds (if (visual?)
                    (visual-bounds)
                    (list (line-beginning-position (point)) (line-end-position (point))))))
    (set! evil-state 'normal)
    (read-from-minibuffer "Ask about this: "
      (lambda (question) (agent-ask-about (car bounds) (second bounds) question)))))

;; i a o... in the conversation go to the prompt; in the prompt, as usual.
(define (agent-typing key)
  (lambda ()
    (if (agent-in-prompt?)
        (call-binding (keymap-lookup normal-map key))
        (begin (agent-focus-prompt) (set! evil-state 'insert)))))

(for-each-pair (lambda (k b) (define-key agent-map k b))
  (list (list "RET" 'agent-send) (list "TAB" 'agent-toggle) (list "z a" 'agent-toggle)
        (list "C-c" 'agent-cancel-run)
        (list "i" (agent-typing "i")) (list "a" (agent-typing "a"))
        (list "I" (agent-typing "I")) (list "A" (agent-typing "A"))
        (list "o" (agent-typing "o")) (list "O" (agent-typing "O"))))

(define-key agent-insert-map "C-RET" 'agent-send)
(define-key agent-insert-map "s-RET" 'agent-send)

;; "2. Allow always" -> the second option.
(define (agent-choose-permission item)
  (let ((dot (let loop ((i 0)) (if (equal? (substring item i (+ i 1)) ".") i (loop (+ i 1))))))
    (agent-permit (- (string->number (substring item 0 dot)) 1))))

(defcommand (agent-permission)
  (let ((options (agent-permission-options)))
    (if (null? options)
        (message "The agent isn't waiting for permission")
        (pick (string-append "Allow " (agent-permission-title) "?") options ""
              agent-choose-permission))))

;; Raised when the agent asks; asks at once unless you're mid-prompt.
(define (agent-ask-permission args)
  (if (minibuffer-active?)
      (message (string-append "The agent asks to " (car args) ": SPC a p to answer"))
      (call-binding 'agent-permission)))
