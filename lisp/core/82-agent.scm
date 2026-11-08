;;;; The coding agent
;;
;; A coding agent (Claude Code by default) works in the project, over the
;; Agent Client Protocol: it reads files (from your buffers, saved or not),
;; edits them and runs tools, asking first when it needs permission. Its
;; conversation is in the *agent* buffer.
;;
;; SPC a a asks it something (opening *agent* on the right), SPC a c cancels
;; the run, SPC a p answers a permission it's waiting on, SPC a k stops it.
;; :agent PROMPT asks from the command line. Another agent: :agent-command
;; CMD, or (set-agent-command "codex-acp") in init.scm,
;; vmac.set_agent_command("codex-acp") in init.lua.

(defcommand (agent-ask)
  (agent-show)
  (read-from-minibuffer "Ask the agent: "
    (lambda (text) (when (not (equal? text "")) (agent-prompt text)))))

(defcommand (agent-open) (agent-show))
(defcommand (agent-cancel-run) (agent-cancel))
(defcommand (agent-kill) (agent-stop))

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
