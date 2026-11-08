;;;; The minibuffer
;;
;; (read-from-minibuffer prompt callback) opens it; RET calls the callback
;; with what was typed, ESC cancels.

(define minibuffer-callback (lambda (s) s))

(define (read-from-minibuffer prompt callback)
  (set! minibuffer-callback callback)
  (minibuffer-open prompt))

(defcommand (minibuffer-exit)
  (let ((input (minibuffer-contents)))
    (set! minibuffer-update (lambda () #t))
    (minibuffer-close)
    (minibuffer-callback input)))

;; Ex commands, after ":". `:eval EXPR` evaluates Scheme; any command name runs it.
(define (ex-dispatch line)
  (let* ((i (space-index line))
         (name (if i (substring line 0 i) line))
         (arg (if i (substring line (+ i 1) (string-length line)) "")))
    (cond ((equal? name "") #t)
          ((equal? name "w") (save-buffer))
          ((equal? name "q") (quit-window))
          ((contains? '("qa" "qall") name) (quit-editor))
          ((contains? '("wq" "x") name) (begin (save-buffer) (quit-window)))
          ((equal? name "e") (find-file arg))
          ((contains? '("bn" "bnext") name) (next-buffer 1))
          ((contains? '("bp" "bprevious") name) (next-buffer -1))
          ((contains? '("bd" "bdelete") name) (kill-buffer #f))
          ((contains? '("bd!" "bdelete!") name) (kill-buffer #t))
          ((contains? '("ls" "buffers") name) (switch-buffer))
          ((equal? name "b")
           (when (not (switch-to-buffer arg)) (message (string-append "No buffer named " arg))))
          ((contains? '("sp" "split") name)
           (begin (split-window "below") (when (not (equal? arg "")) (find-file arg))))
          ((contains? '("vs" "vsplit") name)
           (begin (split-window "right") (when (not (equal? arg "")) (find-file arg))))
          ((equal? name "new") (begin (split-window "below") (new-buffer "*scratch*")))
          ((equal? name "vnew") (begin (split-window "right") (new-buffer "*scratch*")))
          ((contains? '("on" "only") name) (delete-other-windows))
          ((contains? '("clo" "close") name) (window-close))
          ((equal? name "eval") (eval-string arg))
          ((equal? name "lua") (lua-eval arg))
          ((contains? '("noh" "nohlsearch") name) (no-highlight))
          ((command? (string->symbol name)) (call-binding (string->symbol name)))
          (else (message (string-append "Not an editor command: " name))))))

(defcommand (ex) (read-from-minibuffer ":" ex-dispatch))
(defcommand (eval-expression)
  (read-from-minibuffer "Eval: " (lambda (s) (eval-string s))))
(defcommand (execute-extended-command)
  (read-from-minibuffer "M-x " (lambda (name) (call-binding (string->symbol name)))))
