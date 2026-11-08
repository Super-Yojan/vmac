;;;; Terminals
;;
;; A terminal is a program on a pseudo-terminal, in a read-only buffer: in
;; normal state its screen is text (motions, search, yank); in insert state
;; every key goes to the program, except ESC (back to normal state). C-\
;; sends the program an ESC. p pastes the yanked text into it.
;;
;; Where it runs is a backend: "local" (built in) or one a plugin adds with
;; gmacs.terminal_backend (herdr's hc, ...). SPC o t opens one below (or
;; closes it), SPC o T in this window; :term CMD runs CMD; :terminal-backend
;; NAME picks the default. With no command a terminal runs the shell: fish
;; when installed, else $SHELL; :terminal-shell PATH (or set-terminal-shell)
;; changes it.

(define terminal-map '())
(define terminal-normal-map '())

(define (terminal-insert?)
  (and (equal? evil-state 'insert) (not (minibuffer-active?)) (terminal-here?)))

(define (terminal-normal?)
  (and (equal? evil-state 'normal) (not (minibuffer-active?)) (terminal-here?)))

(defcommand (terminal-new)
  (when (terminal-open "" "") (set! evil-state 'insert)))

;; SPC o t: a terminal below, reusing the running one; from a terminal, close it.
(defcommand (terminal-toggle)
  (if (terminal-here?)
      (begin (set! evil-state 'normal)
             (when (not (delete-window)) (next-buffer 1)))
      (let ((running (terminal-buffer-name)))
        (split-window "below")
        (if (equal? running "")
            (terminal-open "" "")
            (switch-to-buffer running))
        (set! evil-state 'insert))))

(defcommand (terminal-type) (set! evil-state 'insert))
(defcommand (terminal-paste) (terminal-send "" (register-get)))
(defcommand (terminal-send-escape) (terminal-send "ESC" ""))

(for-each-pair (lambda (k b) (define-key terminal-map k b))
  '(("ESC" normal-state) ("C-\\" terminal-send-escape)))

(for-each-pair (lambda (k b) (define-key terminal-normal-map k b))
  '(("i" terminal-type) ("a" terminal-type) ("I" terminal-type) ("A" terminal-type)
    ("o" terminal-type) ("O" terminal-type) ("p" terminal-paste) ("P" terminal-paste)))
