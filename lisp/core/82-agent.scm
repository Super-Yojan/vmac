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
;; When it asks permission (to edit, to run a command, to start coding
;; after a plan), *agent* shows the question, what it's about (the plan, the
;; command, the diff) and the choices: press a choice's number, or RET on
;; it. SPC a p picks from a list instead.
;;
;; SPC a a: type to the agent      SPC a s: ask about the line or selection
;; SPC a c: cancel the run         SPC a p: answer a permission it waits on
;; SPC a n: a new conversation     SPC a k: stop the agent
;; SPC a o: show *agent*            SPC a e: ask about this file's problems
;; SPC a h: reopen a past session of this project; SPC a H: of any project.
;;   With Claude Code these are all its sessions (the CLI's and the desktop
;;   app's too, from ~/.claude/projects); other agents list their own. The
;;   agent starts in the session's folder and replays it into *agent*.
;;
;; In the prompt, @ at the start of a word picks a project file to attach:
;; @src/lib.rs sends the file's text along (your buffer's, saved or not).
;; Commands the agent runs show in terminals (*agent: cargo test*): the
;; tool call shows their last lines; RET on it opens the terminal.
;;
;; Its edits are made at once (so it can build and test them) and saved,
;; and wait for your review: changed lines have a green band, deletions a
;; red rule. ]h and [h move between them, ga accepts the one under the
;; cursor, gx rejects it (puts the old lines back, one u undoes that). SPC a
;; r lists them all in *agent-review* (the same keys, RET goes to one);
;; SPC a A / SPC a X accept / reject them all. (set-agent-review #f), or
;; vmac.set_agent_review(false), accepts edits as they're made.
;;
;; :agent PROMPT asks from the command line. Another agent: :agent-command
;; CMD, or (set-agent-command "codex-acp") in init.scm,
;; vmac.set_agent_command("codex-acp") in init.lua.

(defcommand (agent-ask)
  (agent-focus-prompt)
  (set! evil-state 'insert))

(defcommand (agent-open) (agent-show))
(defcommand (agent-history) (agent-sessions #f))
(defcommand (agent-history-all) (agent-sessions #t))

;; Raised with the sessions to pick from: "3. Title · ~/project · 2h ago".
(define (agent-sessions-event items)
  (pick "Agent sessions" items ""
        (lambda (item)
          (let ((dot (let loop ((i 0)) (if (equal? (substring item i (+ i 1)) ".") i (loop (+ i 1))))))
            (agent-open-session (string->number (substring item 0 dot)))))))
(defcommand (agent-cancel-run) (agent-cancel))
(defcommand (agent-kill) (agent-stop))
(defcommand (agent-new) (agent-new-session))
(defcommand (agent-send) (agent-activate) (set! evil-state 'normal))
(defcommand (agent-toggle) (agent-toggle-at-point))

(defcommand (review-open) (review-show))
(defcommand (review-accept) (review-decide #t))
(defcommand (review-reject) (review-decide #f))
(defcommand (review-accept-all) (review-decide-all #t #t))
(defcommand (review-reject-all) (review-decide-all #f #t))
(defcommand (review-next) (review-jump #t))
(defcommand (review-previous) (review-jump #f))
(defcommand (review-go) (review-visit))

(for-each-pair (lambda (k b) (define-key review-map k b))
  (list (list "RET" 'review-go) (list "q" 'review-close)))

;; q: closes its window, or shows another buffer in the last one.
(defcommand (review-close) (when (not (delete-window)) (next-buffer 1)))

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

;; @ at the start of a word: pick a file to attach (ESC just types @).
(defcommand (agent-attach)
  (insert "@")
  (let ((before (if (> (point) 1) (char-at (- (point) 2)) " ")))
    (when (contains? '(" " "\n" "\t" "") before)
      (let ((root (agent-root)))
        (pick "Attach file" (project-files root) root
              (lambda (path) (insert path) (insert " ")))))))

(define-key agent-insert-map "@" 'agent-attach)

;; SPC a e: the file's problems, with a question (default: fix them).
(defcommand (agent-fix-problems)
  (read-from-minibuffer "Ask about the problems: "
    (lambda (question) (agent-ask-about-problems question))))

;; While it waits for permission, 1-9 in *agent* answer with that choice.
(for-each (lambda (n)
            (define-key agent-map (number->string n)
              (lambda () (when (agent-permission-waiting?) (agent-permit (- n 1))))))
          (list 1 2 3 4 5 6 7 8 9))

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
        (pick (agent-permission-question) options ""
              agent-choose-permission))))

;;;; Choosing the agent, rules and hooks
;;
;; Agents by name: claude (the default), codex and gemini are built in; add
;; your own with (agent-define "name" "command args"), or in Lua
;; vmac.agent_backend("name", { command = "..." }). SPC a b or :agent-use
;; NAME picks one (from the next start; SPC a n restarts).
;;
;; Permission rules: before asking you, vmac asks (agent-permission-rule
;; title kind), kind being read, edit, delete, move, search, execute,
;; fetch, think or other. Return "allow" (this once), "always", "reject" or
;; "ask". The default asks; e.g. to let it read and search freely:
;;   (define (agent-permission-rule title kind)
;;     (if (contains? '("read" "search") kind) "allow" "ask"))
;; In Lua, vmac.hooks.agent_permission = function(title, kind) ... end
;; decides first (return nil to leave it to Scheme).
;;
;; Hooks: (agent-on-tool-call title kind status) when a tool call starts or
;; changes status (pending, in_progress, completed, failed), and
;; (agent-on-turn-end reason) when the agent finishes (end_turn, cancelled,
;; max_tokens, refusal, error). Lua: vmac.hooks.agent_tool_call and
;; vmac.hooks.agent_turn_end, with the same arguments.

(define (agent-permission-rule title kind) "ask")
(define (agent-on-tool-call title kind status) #t)
(define (agent-on-turn-end reason) #t)

(defcommand (agent-choose)
  (pick "Agent" (agent-backends) "" (lambda (name) (agent-use name))))

;; The rule for a permission: Lua's hook, else Scheme's.
(define (agent-rule title kind)
  (let ((lua (lua-hook "agent_permission" (list title kind))))
    ;; Looked up when called (eval), so your redefinition is the one used.
    (if (equal? lua "") ((eval 'agent-permission-rule) title kind) lua)))

;; Raised when the agent asks: a rule answers, or you do (at once, unless
;; you're mid-prompt).
(define (agent-ask-permission args)
  (let* ((title (car args))
         (kind (second args))
         (rule (agent-rule title kind)))
    (cond ((and (equal? rule "allow") (agent-permit-kind "allow_once")) #t)
          ((and (equal? rule "always") (agent-permit-kind "allow_always")) #t)
          ((and (equal? rule "reject") (agent-permit-kind "reject_once")) #t)
          ;; Typing (a prompt, insert state): don't take the cursor away.
          ((or (minibuffer-active?) (equal? evil-state 'insert))
           (message (string-append "The agent asks: " (agent-permission-question) " (SPC a p, or answer in *agent*)")))
          ;; Else: *agent* at the question, to read what it's about first.
          (else
           (agent-show-permission)
           (message (string-append (agent-permission-question) " Read it, then press a choice's number"))))))

(define (agent-tool-event args)
  (let ((title (second args)) (kind (third args)) (status (list-ref args 3)))
    (lua-hook "agent_tool_call" (list title kind status))
    ((eval 'agent-on-tool-call) title kind status)))

(define (agent-done-event args)
  (lua-hook "agent_turn_end" args)
  ((eval 'agent-on-turn-end) (car args)))
