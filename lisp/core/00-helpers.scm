;;; vmac's core, in Scheme. Modal, evil-style.
;;;
;;; The files in lisp/core/ load as one program, in name order (so a function
;;; may call one defined in a later file), one file per area: keymaps, the
;;; dispatcher, motions, operators, the picker, the file tree, LSP, ...
;;;
;;; Rust provides primitives (insert, char-at, char-class, delete-region,
;;; undo, find-file, message, minibuffer-open, ...). Everything here can be
;;; redefined while vmac runs: edit these files or ~/.config/vmac/init.scm
;;; and save, or evaluate code with SPC ; or :eval. Lua can extend it too
;;; (~/.config/vmac/init.lua, :lua).

;;;; Small helpers

(define (string-prefix? prefix s)
  (and (>= (string-length s) (string-length prefix))
       (equal? (substring s 0 (string-length prefix)) prefix)))

(define (contains? items x)
  (cond ((null? items) #f)
        ((equal? (car items) x) #t)
        (else (contains? (cdr items) x))))

(define (second xs) (car (cdr xs)))
(define (third xs) (car (cdr (cdr xs))))

;; Index of the first space in s, or #f.
(define (space-index s)
  (let loop ((i 0))
    (cond ((>= i (string-length s)) #f)
          ((equal? (substring s i (+ i 1)) " ") i)
          (else (loop (+ i 1))))))

(define (blank? pos) (contains? '("space" "newline") (char-class pos)))

