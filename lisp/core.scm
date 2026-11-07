;;; core.scm — gmacs's editor, in Scheme. Modal, evil-style.
;;;
;;; Rust provides primitives (insert, char-at, char-class, delete-region,
;;; undo, find-file, message, minibuffer-open, ...). Everything here — the
;;; states, motions, operators, text objects, every command, the key
;;; dispatcher, the keymaps, the modeline — can be redefined while gmacs
;;; runs: edit this file or ~/.config/gmacs/init.scm and save, or evaluate
;;; code with SPC ; or :eval.

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

;;;; Keymaps
;;
;; A keymap is a list of (key-sequence binding) entries, in Emacs key
;; notation: "g g", "SPC f f", "C-r", "ESC", "RET". Each state has its own.

(define normal-map '())
(define visual-map '())
(define operator-map '())
(define insert-map '())
(define minibuffer-map '())

(define (keymap-remove keymap key)
  (cond ((null? keymap) '())
        ((equal? (car (car keymap)) key) (keymap-remove (cdr keymap) key))
        (else (cons (car keymap) (keymap-remove (cdr keymap) key)))))

(define (keymap-set keymap key binding)
  (cons (list key binding) (keymap-remove keymap key)))

(define (keymap-lookup keymap key)
  (cond ((null? keymap) #f)
        ((equal? (car (car keymap)) key) (second (car keymap)))
        (else (keymap-lookup (cdr keymap) key))))

;; Whether some binding continues the sequence.
(define (keymap-prefix? keymap seq)
  (let ((p (string-append seq " ")))
    (cond ((null? keymap) #f)
          ((string-prefix? p (car (car keymap))) #t)
          (else (keymap-prefix? (cdr keymap) seq)))))

;; (define-key normal-map "Q" 'some-command). The keymap variables are
;; replaced, not mutated, so define-key is a macro that set!s the variable.
(define-syntax define-key
  (syntax-rules ()
    [(define-key keymap key binding)
     (set! keymap (keymap-set keymap key binding))]))

(define (for-each-pair f pairs)
  (when (not (null? pairs))
    (f (car (car pairs)) (second (car pairs)))
    (for-each-pair f (cdr pairs))))

;;;; States: normal, insert, visual, visual-line, operator (after d, c, y...)

(define evil-state 'normal)

(define (visual?) (contains? '(visual visual-line) evil-state))
;; The minibuffer is for typing, except in a picker, which has its own normal
;; mode (like telescope): ESC there leaves typing, and Vim keys move through
;; the results or edit the query.
(define (picker-normal?) (and (minibuffer-active?) (picker-active?) (not (equal? evil-state 'insert))))

(define (inserting?)
  (or (equal? evil-state 'insert)
      (and (minibuffer-active?) (not (picker-active?)))))

(define picker-normal-map '())

(define (current-keymap)
  (cond ((picker-normal?)
         (cond ((visual?) visual-map)
               ((equal? evil-state 'operator) operator-map)
               (else (append picker-normal-map normal-map))))
        ((minibuffer-active?) minibuffer-map)
        ((equal? evil-state 'insert) insert-map)
        ((visual?) visual-map)
        ((equal? evil-state 'operator) operator-map)
        (else normal-map)))

;; Rust asks this for the cursor: a block, or a bar when typing.
(define (cursor-shape) (if (inserting?) 'bar 'box))

;;;; The key dispatcher
;;
;; Rust calls (dispatch-key key text) for every keystroke. `text` is what the
;; key would type ("" for ESC, RET, C-r). A count typed first (3j, 2dd, d2w)
;; is in `current-count` while the binding runs. A named command runs that
;; many times; a procedure binding runs once and reads the count itself.

(define pending-keys "")
(define pending-count "")
(define current-count 1)

;; (read-char f): the next key is a character for f, not a command (f, t, r).
(define char-reader #f)
(define (read-char f) (set! char-reader f))

;; Called after every key while the minibuffer is open (incremental search).
(define minibuffer-update (lambda () #t))

(define (count-digit? text)
  (and (not (inserting?))
       (equal? pending-keys "")
       (or (contains? '("1" "2" "3" "4" "5" "6" "7" "8" "9") text)
           (and (equal? text "0") (not (equal? pending-count ""))))))

(define (repeat n binding)
  (when (> n 0)
    (call-binding binding)
    (repeat (- n 1) binding)))

(define (dispatch-key key text)
  (dispatch-key-1 key text)
  (when (minibuffer-active?) (minibuffer-update)))

(define (dispatch-key-1 key text)
  (cond
    (char-reader
     (let ((f char-reader))
       (set! char-reader #f)
       (cond ((not (equal? text "")) (f text))
             ((equal? key "ESC") (keyboard-quit))
             (else (when (equal? evil-state 'operator) (set! evil-state 'normal))
                   (message (string-append key " is not a character"))))))
    (else (dispatch-binding key text))))

(define (dispatch-binding key text)
  (if (count-digit? text)
      (set! pending-count (string-append pending-count text))
      (let* ((seq (if (equal? pending-keys "") key (string-append pending-keys " " key)))
             (keymap (current-keymap))
             (binding (keymap-lookup keymap seq)))
        (cond (binding
               (set! current-count
                     (if (equal? pending-count "") 1 (string->number pending-count)))
               ;; The "SPC f-" shown while a sequence was pending goes away.
               (when (not (equal? pending-keys "")) (message ""))
               (set! pending-keys "")
               (set! pending-count "")
               (when (equal? evil-state 'normal) (undo-boundary))
               (if (procedure? binding) (binding) (repeat current-count binding)))
              ((keymap-prefix? keymap seq)
               (set! pending-keys seq)
               (message (string-append seq "-")))
              ((and (equal? pending-keys "") (inserting?) (not (equal? text "")))
               (insert text))
              (else
               (set! pending-keys "")
               (set! pending-count "")
               (when (equal? evil-state 'operator) (set! evil-state 'normal))
               (message (string-append seq " is undefined")))))))

;;;; Lines

(define (line-start-at pos) (line-beginning-position pos))
(define (line-end-at pos) (line-end-position pos))
(define (line-start) (line-start-at (point)))
(define (line-end) (line-end-at (point)))
(define (last-char-at pos) (max (line-start-at pos) (- (line-end-at pos) 1)))
(define (last-char-of-line) (last-char-at (point)))
(define (clamp-to-line!) (when (> (point) (last-char-of-line)) (goto-char (last-char-of-line))))

(define (first-non-blank-at pos)
  (let loop ((p (line-start-at pos)))
    (if (and (< p (line-end-at pos)) (equal? (char-class p) "space")) (loop (+ p 1)) p)))
(define (first-non-blank) (first-non-blank-at (point)))

(define (empty-line? pos) (equal? (line-start-at pos) (line-end-at pos)))

;;;; Motions
;;
;; A motion is (type step): step takes a position and returns where one
;; repetition lands. The type says what an operator covers: exclusive (up
;; to the target), inclusive (the target too) or linewise (whole lines).
;; Define one with (define-motion name type step), bind it in motion-keys,
;; and it works for moving, in visual mode and after an operator (d, c, y).

(define motions (hash))

(define (define-motion name type step)
  (set! motions (hash-insert motions name (list type step))))

(define (motion-type name) (car (hash-ref motions name)))

;; Where n repetitions land, or #f when the motion can't go (f with no such
;; character, a search with no match).
(define (motion-target name pos n)
  (let ((step (second (hash-ref motions name))))
    (let loop ((p pos) (i n))
      (cond ((not p) #f)
            ((> i 0) (loop (step p) (- i 1)))
            (else p)))))

;; Words: "w" stops where the character class changes (word, punctuation);
;; "W" only at whitespace.
(define (same-word-class? a b big)
  (if big
      (and (not (blank? a)) (not (blank? b)))
      (equal? (char-class a) (char-class b))))

(define (word-forward pos big)
  (let skip-word ((p pos))
    (if (and (< (+ p 1) (point-max)) (not (blank? p)) (same-word-class? p (+ p 1) big))
        (skip-word (+ p 1))
        (let skip-blank ((q (if (blank? p) p (+ p 1))))
          (if (and (< q (point-max)) (blank? q) (not (empty-line? q)))
              (skip-blank (+ q 1))
              (min q (point-max)))))))

(define (word-backward pos big)
  (let skip-blank ((p (max 0 (- pos 1))))
    (if (and (> p 0) (blank? p))
        (skip-blank (- p 1))
        (let skip-word ((q p))
          (if (and (> q 0) (not (blank? (- q 1))) (same-word-class? (- q 1) q big))
              (skip-word (- q 1))
              q)))))

(define (word-end pos big)
  (let skip-blank ((p (min (+ pos 1) (max 0 (- (point-max) 1)))))
    (if (and (< (+ p 1) (point-max)) (blank? p))
        (skip-blank (+ p 1))
        (let skip-word ((q p))
          (if (and (< (+ q 1) (point-max)) (same-word-class? q (+ q 1) big))
              (skip-word (+ q 1))
              q)))))

(define (line-down pos n)
  (let ((line (min (- (line-count) 1) (+ (pos->line pos) n))))
    (line->pos line)))

(define-motion 'left 'exclusive (lambda (p) (if (> p (line-start-at p)) (- p 1) p)))
(define-motion 'right 'exclusive (lambda (p) (if (< p (line-end-at p)) (+ p 1) p)))
(define-motion 'down 'linewise (lambda (p) (line-down p 1)))
(define-motion 'up 'linewise (lambda (p) (line->pos (max 0 (- (pos->line p) 1)))))
(define-motion 'line-start 'exclusive (lambda (p) (line-start-at p)))
(define-motion 'first-non-blank 'exclusive (lambda (p) (first-non-blank-at p)))
(define-motion 'line-end 'inclusive (lambda (p) (last-char-at p)))
(define-motion 'word-forward 'exclusive (lambda (p) (word-forward p #f)))
(define-motion 'WORD-forward 'exclusive (lambda (p) (word-forward p #t)))
(define-motion 'word-backward 'exclusive (lambda (p) (word-backward p #f)))
(define-motion 'WORD-backward 'exclusive (lambda (p) (word-backward p #t)))
(define-motion 'word-end 'inclusive (lambda (p) (word-end p #f)))
(define-motion 'WORD-end 'inclusive (lambda (p) (word-end p #t)))
(define-motion 'buffer-start 'linewise (lambda (p) 0))
(define-motion 'buffer-end 'linewise (lambda (p) (line->pos (- (line-count) 1))))

(define motion-keys
  '(("h" left) ("l" right) ("j" down) ("k" up)
    ("<left>" left) ("<right>" right) ("<down>" down) ("<up>" up)
    ("0" line-start) ("^" first-non-blank) ("$" line-end)
    ("w" word-forward) ("W" WORD-forward) ("b" word-backward) ("B" WORD-backward)
    ("e" word-end) ("E" WORD-end) ("g g" buffer-start) ("G" buffer-end)
    ("n" search-next) ("N" search-previous)))

;; Moving the cursor. j and k keep the column (the buffer remembers it), and
;; outside visual mode the cursor stays on a character, never past the line.
(define (move-by motion)
  (cond ((equal? motion 'down) (forward-line current-count))
        ((equal? motion 'up) (forward-line (- 0 current-count)))
        ((equal? motion 'buffer-end)
         (goto-char (first-non-blank-at (motion-target motion (point) 1))))
        ((equal? motion 'buffer-start) (goto-char 0))
        (else (let ((target (motion-target motion (point) current-count)))
                (when target (goto-char target)))))
  (when (not (visual?)) (clamp-to-line!)))

;;;; Finding a character: f t F T, repeated with ; and ,
;;
;; On the current line. f and F land on the character, t and T just before
;; it. They read the character after the key, then act as the motion
;; 'find-char, so they work after operators (dt), cf,) and in visual mode.

(define (find-on-line c from step)
  (let loop ((p from))
    (cond ((or (< p (line-start-at from)) (>= p (line-end-at from))) #f)
          ((equal? (char-at p) c) p)
          (else (loop (+ p step))))))

;; kind is "f", "t", "F" or "T". `again` (from ; and ,) makes t and T skip
;; a match right next to the cursor, so repeating them moves on.
(define (find-step kind c again)
  (lambda (p)
    (cond ((equal? kind "f") (find-on-line c (+ p 1) 1))
          ((equal? kind "F") (find-on-line c (- p 1) -1))
          ((equal? kind "t")
           (let ((q (find-on-line c (+ p (if again 2 1)) 1))) (and q (- q 1))))
          (else
           (let ((q (find-on-line c (- p (if again 2 1)) -1))) (and q (+ q 1)))))))

(define (find-type kind) (if (contains? '("f" "t") kind) 'inclusive 'exclusive))

(define last-find #f)

(define (opposite-find kind)
  (cond ((equal? kind "f") "F") ((equal? kind "F") "f")
        ((equal? kind "t") "T") (else "t")))

;; Makes 'find-char mean this find, then runs it as `then` would any motion.
(define (run-find kind c again then)
  (define-motion 'find-char (find-type kind) (find-step kind c again))
  (then 'find-char))

(define (find-with then)
  (lambda (kind)
    (lambda ()
      (let ((count current-count))
        (read-char (lambda (c)
                     (set! last-find (list kind c))
                     (set! current-count count)
                     (run-find kind c #f then)))))))

(define (repeat-find then opposite)
  (lambda ()
    (if last-find
        (let ((kind (if opposite (opposite-find (car last-find)) (car last-find))))
          (run-find kind (second last-find) #t then))
        (message "No previous f or t"))))

(define (operator-run motion) ((operator-motion motion)))

;;;; Search: / ? n N * #
;;
;; Patterns are regexes, case-insensitive unless they have a capital
;; (smartcase), searched across the buffer with wrap-around. n and N are
;; motions, so d/foo, dn and v/foo work. Matches are highlighted until ESC
;; or :noh.

(define search-pattern "")
(define search-forward? #t)
(define search-highlight? #f)
(define search-preview #f)

;; The next match start after pos in a direction, wrapping; #f with none.
(define (search-from pos pattern forward)
  (if forward
      (let ((p (search-forward pattern (+ pos 1))))
        (cond ((>= p 0) p)
              (else (let ((w (search-forward pattern 0)))
                      (and (>= w 0) (begin (message "search wrapped") w))))))
      (let ((p (search-backward pattern pos)))
        (cond ((>= p 0) p)
              (else (let ((w (search-backward pattern (point-max))))
                      (and (>= w 0) (begin (message "search wrapped") w))))))))

(define (search-step forward-wanted)
  (lambda (p)
    (if (equal? search-pattern "")
        (begin (message "No previous search") #f)
        (let ((found (search-from p search-pattern (equal? forward-wanted search-forward?))))
          (when (not found) (message (string-append "Pattern not found: " search-pattern)))
          found))))

(define-motion 'search-next 'exclusive (search-step #t))
(define-motion 'search-previous 'exclusive (search-step #f))

;; What the window highlights: what's being typed after / or ?, else the
;; last pattern until ESC or :noh.
(define (search-highlight)
  (cond (search-preview search-preview)
        (search-highlight? search-pattern)
        (else "")))

;; / and ?: read a pattern (highlighting as you type), then run `then` with
;; the 'search-next motion.
(define (search-prompt forward then)
  (lambda ()
    (let ((count current-count))
      (set! minibuffer-update (lambda () (set! search-preview (minibuffer-contents))))
      (read-from-minibuffer (if forward "/" "?")
        (lambda (pattern)
          (set! search-preview #f)
          (when (not (equal? pattern "")) (set! search-pattern pattern))
          (set! search-forward? forward)
          (set! search-highlight? #t)
          (set! current-count count)
          (then 'search-next))))))

;; * and #: the word under the cursor, as a whole word.
(define (search-word forward then)
  (lambda ()
    (let ((range ((word-object #f) (point) #t)))
      (if (and range (not (blank? (point))))
          (begin
            (set! search-pattern
                  (string-append "\\b" (regex-quote (buffer-substring (car range) (second range))) "\\b"))
            (set! search-forward? forward)
            (set! search-highlight? #t)
            (goto-char (car range))
            (then 'search-next))
          (message "No word under cursor")))))

(defcommand (no-highlight) (set! search-highlight? #f))

;;;; Text objects
;;
;; A text object is a procedure (pos inner?) -> (start end) or #f, end
;; exclusive. Bind it in object-keys: "i" + key is inner, "a" + key around.

(define (extend-left pos same?)
  (if (and (> pos 0) (same? (- pos 1))) (extend-left (- pos 1) same?) pos))
(define (extend-right pos same?)
  (if (and (< pos (point-max)) (same? pos)) (extend-right (+ pos 1) same?) pos))

(define (word-object big)
  (lambda (pos inner)
    (if (or (equal? (char-class pos) "eof") (equal? (char-class pos) "newline"))
        #f
        (let* ((same? (lambda (p) (if (blank? pos)
                                      (equal? (char-class p) "space")
                                      (same-word-class? p pos big))))
               (start (extend-left pos same?))
               (end (extend-right pos same?))
               (space? (lambda (p) (equal? (char-class p) "space"))))
          (cond (inner (list start end))
                ;; "a": the word with the spaces after it, else before it.
                ((blank? pos) (list start (extend-right end (lambda (p) (same-word-class? p end big)))))
                ((> (extend-right end space?) end) (list start (extend-right end space?)))
                (else (list (extend-left start space?) end)))))))

;; Brackets: the nearest pair around pos, counting nesting.
(define (bracket-object open close)
  (lambda (pos inner)
    (let ((start
           (let back ((p pos) (depth 0))
             (cond ((< p 0) #f)
                   ((and (equal? (char-at p) open) (= depth 0)) p)
                   ((equal? (char-at p) open) (back (- p 1) (- depth 1)))
                   ((and (equal? (char-at p) close) (not (= p pos))) (back (- p 1) (+ depth 1)))
                   (else (back (- p 1) depth))))))
      (and start
           (let ((end
                  (let fwd ((p (+ start 1)) (depth 0))
                    (cond ((>= p (point-max)) #f)
                          ((and (equal? (char-at p) close) (= depth 0)) p)
                          ((equal? (char-at p) close) (fwd (+ p 1) (- depth 1)))
                          ((equal? (char-at p) open) (fwd (+ p 1) (+ depth 1)))
                          (else (fwd (+ p 1) depth))))))
             (and end (if inner (list (+ start 1) end) (list start (+ end 1)))))))))

;; Quotes, on the current line: the pair around pos, else the next pair.
(define (quote-object q)
  (lambda (pos inner)
    (let find ((p (line-start-at pos)) (open #f))
      (cond ((>= p (line-end-at pos)) #f)
            ((not (equal? (char-at p) q)) (find (+ p 1) open))
            ((not open) (find (+ p 1) p))
            ((>= p pos) (if inner (list (+ open 1) p) (list open (+ p 1))))
            (else (find (+ p 1) #f))))))

;; Paragraphs: the run of non-empty (or empty) lines around pos; "a" adds
;; the empty lines after it.
(define (paragraph-object pos inner)
  (let* ((kind (empty-line? pos))
         (same? (lambda (line) (equal? (empty-line? (line->pos line)) kind)))
         (first (let up ((l (pos->line pos)))
                  (if (and (> l 0) (same? (- l 1))) (up (- l 1)) l)))
         (last (let down ((l (pos->line pos)))
                 (if (and (< l (- (line-count) 1)) (same? (+ l 1))) (down (+ l 1)) l)))
         (last (if inner
                   last
                   (let down ((l last))
                     (if (and (< l (- (line-count) 1))
                              (not (equal? (empty-line? (line->pos (+ l 1))) kind)))
                         (down (+ l 1))
                         l)))))
    (list (line->pos first) (line-end-at (line->pos last)) 'linewise)))

(define object-keys
  (list (list "w" (word-object #f)) (list "W" (word-object #t))
        (list "(" (bracket-object "(" ")")) (list ")" (bracket-object "(" ")"))
        (list "b" (bracket-object "(" ")"))
        (list "[" (bracket-object "[" "]")) (list "]" (bracket-object "[" "]"))
        (list "{" (bracket-object "{" "}")) (list "}" (bracket-object "{" "}"))
        (list "B" (bracket-object "{" "}"))
        (list "<" (bracket-object "<" ">")) (list ">" (bracket-object "<" ">"))
        (list "\"" (quote-object "\"")) (list "'" (quote-object "'"))
        (list "`" (quote-object "`"))
        (list "p" paragraph-object)))

;;;; Operators
;;
;; An operator is a procedure (start end type) acting on a range: d, c, y,
;; >, <. Press it, then a motion or text object (dw, ci", y$), or press it
;; twice for whole lines (dd, 3yy, >>), or select first in visual mode.

(define (linewise-range start end)
  (let ((s (line-start-at start)) (e (line-end-at end)))
    (cond ((< e (point-max)) (list s (+ e 1)))
          ((> s 0) (list (- s 1) e))
          (else (list s e)))))

(define (range-text start end type)
  (if (equal? type 'linewise)
      (string-append (buffer-substring (line-start-at start) (line-end-at end)) "\n")
      (buffer-substring start end)))

(define (op-delete start end type)
  (register-set (range-text start end type))
  (let ((r (if (equal? type 'linewise) (linewise-range start end) (list start end))))
    (delete-region (car r) (second r))
    (goto-char (car r))
    (if (equal? type 'linewise) (goto-char (first-non-blank)) (clamp-to-line!))))

(define (op-change start end type)
  (register-set (range-text start end type))
  (if (equal? type 'linewise)
      (begin (delete-region (first-non-blank-at start) (line-end-at end))
             (goto-char (first-non-blank-at start)))
      (begin (delete-region start end) (goto-char start)))
  (set! evil-state 'insert))

(define (op-yank start end type)
  (register-set (range-text start end type))
  (goto-char (if (equal? type 'linewise) (point) start))
  (message (if (equal? type 'linewise) "yanked lines" "yanked")))

(define (shift-lines start end f)
  (let loop ((line (pos->line end)))
    (when (>= line (pos->line start))
      (goto-char (line->pos line))
      (f)
      (loop (- line 1))))
  (goto-char (first-non-blank-at (line->pos (pos->line start)))))

(define (op-indent start end type)
  (shift-lines start end (lambda () (when (not (empty-line? (point))) (insert "    ")))))

(define (op-outdent start end type)
  (shift-lines start end
               (lambda ()
                 (let ((n (- (first-non-blank) (line-start))))
                   (delete-region (line-start) (+ (line-start) (min 4 n)))))))

(define operators
  (list (list "d" op-delete) (list "c" op-change) (list "y" op-yank)
        (list ">" op-indent) (list "<" op-outdent)))

(define pending-operator #f)
(define pending-operator-key "")
(define operator-count 1)

(define (start-operator key op)
  (lambda ()
    (set! pending-operator op)
    (set! pending-operator-key key)
    (set! operator-count current-count)
    (set! evil-state 'operator)))

(define (finish-operator start end type)
  (set! evil-state 'normal)
  (pending-operator start end type))

(define (operator-motion motion)
  (lambda ()
    (let* ((n (* operator-count current-count))
           (start (point))
           (type (motion-type motion))
           ;; cw changes to the end of the word, like ce.
           (motion (cond ((and (equal? pending-operator op-change) (equal? motion 'word-forward)) 'word-end)
                         ((and (equal? pending-operator op-change) (equal? motion 'WORD-forward)) 'WORD-end)
                         (else motion)))
           (type (if (contains? '(word-end WORD-end) motion) 'inclusive type))
           (target (motion-target motion start n))
           ;; dw on a line's last word stops at the line's end.
           (target (if (and target
                            (equal? type 'exclusive)
                            (contains? '(word-forward WORD-forward) motion)
                            (> (pos->line target) (pos->line start)))
                       (max start (line-end-at start))
                       target))
           (lo (and target (min start target)))
           (hi (and target (max start target))))
      (if target
          (finish-operator lo (if (equal? type 'inclusive) (+ hi 1) hi) type)
          (set! evil-state 'normal)))))

(define (operator-object object inner)
  (lambda ()
    (let ((range (object (point) inner)))
      (if range
          (finish-operator (car range) (second range)
                           (if (null? (cdr (cdr range))) 'exclusive (third range)))
          (begin (set! evil-state 'normal) (message "no text object here"))))))

(define (operator-lines)
  (let ((n (* operator-count current-count)))
    (finish-operator (point) (line->pos (+ (pos->line (point)) (- n 1))) 'linewise)))

;;;; Visual mode
;;
;; The selection runs from `visual-anchor` to the cursor, both included.
;; Rust draws whatever (selection) returns.

(define visual-anchor 0)

(define (visual-bounds)
  (let ((lo (min visual-anchor (point))) (hi (max visual-anchor (point))))
    (if (equal? evil-state 'visual-line)
        (list (line-start-at lo) (line-end-at hi) 'linewise)
        (list lo (min (point-max) (+ hi 1)) 'exclusive))))

(define (selection)
  (if (visual?)
      (let ((b (visual-bounds)))
        (if (equal? (third b) 'linewise)
            (list (car b) (min (point-max) (+ (second b) 1)))
            (list (car b) (second b))))
      '()))

(define (enter-visual state)
  (lambda ()
    (if (equal? evil-state state)
        (exit-visual)
        (begin (when (not (visual?)) (set! visual-anchor (point)))
               (set! evil-state state)))))

(defcommand (exit-visual) (set! evil-state 'normal) (clamp-to-line!))

(defcommand (visual-swap-ends)
  (let ((p (point))) (goto-char visual-anchor) (set! visual-anchor p)))

(define (visual-operator op)
  (lambda ()
    (let ((b (visual-bounds)))
      (set! evil-state 'normal)
      (op (car b) (second b) (third b)))))

(define (visual-object object inner)
  (lambda ()
    (let ((range (object (point) inner)))
      (when range
        (when (and (not (null? (cdr (cdr range)))) (equal? (third range) 'linewise))
          (set! evil-state 'visual-line))
        (set! visual-anchor (car range))
        (goto-char (max (car range) (- (second range) 1)))))))

(defcommand (visual-paste)
  (let ((text (register-get)) (b (visual-bounds)))
    (set! evil-state 'normal)
    (op-delete (car b) (second b) (third b))
    (register-set text)
    (paste-before)))

;;;; Insert state

(defcommand (normal-state)
  (set! evil-state 'normal)
  (when (> (point) (line-start)) (forward-char -1))
  (message ""))
(defcommand (insert-state) (set! evil-state 'insert))
(defcommand (insert-after) (when (< (point) (line-end)) (forward-char 1)) (insert-state))
(defcommand (insert-line-start) (goto-char (first-non-blank)) (insert-state))
(defcommand (insert-line-end) (goto-char (line-end)) (insert-state))
(defcommand (open-below) (goto-char (line-end)) (insert "\n") (insert-state))
(defcommand (open-above) (goto-char (line-start)) (insert "\n") (forward-char -1) (insert-state))

(defcommand (newline) (insert "\n"))
(defcommand (indent) (insert "    "))
(defcommand (delete-backward) (delete-backward-char 1))
(defcommand (delete-forward) (delete-char 1))
(defcommand (char-left) (forward-char -1))
(defcommand (char-right) (forward-char 1))
(defcommand (line-up) (forward-line -1))
(defcommand (line-down-cmd) (forward-line 1))

;;;; Other normal-state commands

(defcommand (delete-char-under)
  (when (< (point) (line-end))
    (register-set (delete-region (point) (+ (point) 1)))
    (clamp-to-line!)))

(define (linewise? text)
  (and (> (string-length text) 0)
       (equal? (substring text (- (string-length text) 1) (string-length text)) "\n")))

(define (without-last-char text) (substring text 0 (- (string-length text) 1)))

(defcommand (paste-after)
  (let ((text (register-get)))
    (cond ((linewise? text)
           (if (< (line-end) (point-max))
               (begin (goto-char (+ (line-end) 1)) (insert text) (forward-line -1))
               (begin (goto-char (line-end)) (insert (string-append "\n" (without-last-char text)))))
           (goto-char (first-non-blank)))
          ((> (string-length text) 0)
           (when (< (point) (line-end)) (forward-char 1))
           (insert text)
           (forward-char -1)))))

(defcommand (paste-before)
  (let ((text (register-get)))
    (cond ((linewise? text)
           (goto-char (line-start)) (insert text) (forward-line -1) (goto-char (first-non-blank)))
          ((> (string-length text) 0)
           (insert text) (forward-char -1)))))

(defcommand (evil-undo) (when (not (undo)) (message "Already at oldest change")) (clamp-to-line!))
(defcommand (evil-redo) (when (not (redo)) (message "Already at newest change")) (clamp-to-line!))

(defcommand (keyboard-quit)
  (set! pending-keys "")
  (set! pending-count "")
  (set! char-reader #f)
  (set! search-preview #f)
  (set! search-highlight? #f)
  (set! minibuffer-update (lambda () #t))
  (if (picker-active?)
      (close-picker!)
      (when (not (equal? evil-state 'insert)) (set! evil-state 'normal)))
  (when (minibuffer-active?) (minibuffer-close))
  (message "Quit"))

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
          ((contains? '("noh" "nohlsearch") name) (no-highlight))
          ((command? (string->symbol name)) (call-binding (string->symbol name)))
          (else (message (string-append "Not an editor command: " name))))))

(defcommand (ex) (read-from-minibuffer ":" ex-dispatch))
(defcommand (eval-expression)
  (read-from-minibuffer "Eval: " (lambda (s) (eval-string s))))
(defcommand (execute-extended-command)
  (read-from-minibuffer "M-x " (lambda (name) (call-binding (string->symbol name)))))
;;;; The picker (telescope-style)
;;
;; (pick title items preview-root on-pick) shows items filtered fuzzily as
;; you type, in a panel in the middle of the window. C-n/C-p, C-j/C-k or the
;; arrows move; RET calls on-pick with the item; ESC closes. When
;; preview-root is a folder, items are paths in it and the selected one is
;; previewed. Rust does the filtering; what to list and what picking does is
;; up to the caller.

;; The buffer's state before the picker opened, to go back to.
(define picker-return-state 'normal)

(define (close-picker!)
  (when (picker-active?)
    (picker-close)
    (set! evil-state picker-return-state)))

(define (pick title items preview-root on-pick)
  (set! picker-return-state (if (equal? evil-state 'insert) 'insert 'normal))
  (picker-open title items preview-root)
  (set! evil-state 'insert)
  (set! minibuffer-update (lambda () (picker-filter (minibuffer-contents))))
  (read-from-minibuffer "> "
    (lambda (query)
      (let ((choice (picker-selection)))
        (close-picker!)
        (if (equal? choice "")
            (message "No match")
            (on-pick choice))))))

(defcommand (find-file-command)
  (let ((root (project-root)))
    (pick "Find file" (project-files root) root
          (lambda (path) (find-file (string-append root "/" path))))))

(defcommand (picker-next) (picker-move 1))
(defcommand (picker-previous) (picker-move -1))
(defcommand (picker-first) (picker-select 0))
(defcommand (picker-last) (picker-select -1))
(defcommand (picker-page-down) (picker-move 10))
(defcommand (picker-page-up) (picker-move -10))

;; ESC while typing: in a picker, go to its normal mode; elsewhere, cancel.
(defcommand (minibuffer-escape)
  (if (picker-active?) (normal-state) (keyboard-quit)))
(defcommand (save) (save-buffer))
(defcommand (quit) (quit-editor))

;;;; Scrolling: C-d C-u C-f C-b C-e C-y zz zt zb

;; Keeps the cursor inside the window after it scrolls.
(define (keep-cursor-in-window)
  (let* ((top (window-top-line))
         (bottom (+ top (- (window-height) 1)))
         (line (pos->line (point))))
    (cond ((< line top) (goto-char (first-non-blank-at (line->pos top))))
          ((> line bottom) (goto-char (first-non-blank-at (line->pos bottom)))))))

;; Scrolls the window and moves the cursor the same number of lines.
(define (scroll-with-cursor n)
  (scroll-window n)
  (forward-line n)
  (when (not (visual?)) (clamp-to-line!)))

(defcommand (scroll-half-down) (scroll-with-cursor (max 1 (quotient (window-height) 2))))
(defcommand (scroll-half-up) (scroll-with-cursor (- 0 (max 1 (quotient (window-height) 2)))))
(defcommand (scroll-page-down) (scroll-with-cursor (max 1 (- (window-height) 2))))
(defcommand (scroll-page-up) (scroll-with-cursor (- 0 (max 1 (- (window-height) 2)))))
(defcommand (scroll-line-down) (scroll-window 1) (keep-cursor-in-window))
(defcommand (scroll-line-up) (scroll-window -1) (keep-cursor-in-window))
(defcommand (recenter-center) (recenter "center"))
(defcommand (recenter-top) (recenter "top"))
(defcommand (recenter-bottom) (recenter "bottom"))

;;;; Buffers and windows
;;
;; Every open file is a buffer. Windows show buffers; split them with C-w v
;; (beside) and C-w s (below), move with C-w h/j/k/l, close with C-w c.
;; Each window keeps its own cursor, even on the same buffer.

(defcommand (switch-buffer)
  (pick "Switch buffer" (buffer-list) "" (lambda (name) (switch-to-buffer name))))
(defcommand (buffer-next) (next-buffer 1))
(defcommand (buffer-previous) (next-buffer -1))
(defcommand (buffer-kill) (kill-buffer #f))
(defcommand (buffer-kill-force) (kill-buffer #t))
(defcommand (buffer-new) (new-buffer "*scratch*"))

(defcommand (split-right) (split-window "right"))
(defcommand (split-below) (split-window "below"))
(defcommand (window-left) (select-window-in-direction "left"))
(defcommand (window-right) (select-window-in-direction "right"))
(defcommand (window-up) (select-window-in-direction "up"))
(defcommand (window-down) (select-window-in-direction "down"))
(defcommand (window-next) (other-window 1))
(defcommand (window-previous) (other-window -1))
(defcommand (window-close)
  (when (not (delete-window)) (message "Can't close the last window")))
(defcommand (window-only) (delete-other-windows))

;; :q closes the window, and quits from the last one.
(defcommand (quit-window) (when (not (delete-window)) (quit-editor)))

;;;; Default bindings

;; Motions move in normal and visual state, and complete an operator.
(for-each-pair (lambda (key motion)
                 (define-key normal-map key (lambda () (move-by motion)))
                 (define-key visual-map key (lambda () (move-by motion)))
                 (define-key operator-map key (operator-motion motion)))
               motion-keys)

;; Operators: start one in normal state, act on the selection in visual.
(for-each-pair (lambda (key op)
                 (define-key normal-map key (start-operator key op))
                 (define-key visual-map key (visual-operator op))
                 (define-key operator-map key (lambda () (operator-lines))))
               operators)

;; Text objects: after an operator, or to select in visual state.
(for-each-pair (lambda (key object)
                 (define-key operator-map (string-append "i " key) (operator-object object #t))
                 (define-key operator-map (string-append "a " key) (operator-object object #f))
                 (define-key visual-map (string-append "i " key) (visual-object object #t))
                 (define-key visual-map (string-append "a " key) (visual-object object #f)))
               object-keys)

;; f t F T ; , / ? * #: the same keys move, extend a selection, or complete
;; an operator; only what is done with the motion differs.
(define (keymap-define! name key binding)
  (cond ((equal? name 'normal) (define-key normal-map key binding))
        ((equal? name 'visual) (define-key visual-map key binding))
        ((equal? name 'operator) (define-key operator-map key binding))
        ((equal? name 'insert) (define-key insert-map key binding))
        (else (define-key minibuffer-map key binding))))

(for-each (lambda (pair)
            (let ((keymap-name (car pair)) (then (second pair)))
              (for-each (lambda (kind)
                          (keymap-define! keymap-name kind ((find-with then) kind)))
                        '("f" "t" "F" "T"))
              (keymap-define! keymap-name ";" (repeat-find then #f))
              (keymap-define! keymap-name "," (repeat-find then #t))
              (keymap-define! keymap-name "/" (search-prompt #t then))
              (keymap-define! keymap-name "?" (search-prompt #f then))
              (keymap-define! keymap-name "*" (search-word #t then))
              (keymap-define! keymap-name "#" (search-word #f then))))
          (list (list 'normal move-by) (list 'visual move-by) (list 'operator operator-run)))

(define (shorthand op motion) (lambda () ((start-operator "" op)) ((operator-motion motion))))

(for-each-pair (lambda (k b) (define-key normal-map k b))
  (list (list "i" 'insert-state) (list "a" 'insert-after)
        (list "I" 'insert-line-start) (list "A" 'insert-line-end)
        (list "o" 'open-below) (list "O" 'open-above)
        (list "v" (enter-visual 'visual)) (list "V" (enter-visual 'visual-line))
        (list "x" 'delete-char-under)
        (list "D" (shorthand op-delete 'line-end))
        (list "C" (shorthand op-change 'line-end))
        (list "Y" (lambda () ((start-operator "y" op-yank)) (operator-lines)))
        (list "p" 'paste-after) (list "P" 'paste-before)
        (list "u" 'evil-undo) (list "C-r" 'evil-redo)
        (list ":" 'ex) (list "ESC" 'keyboard-quit)
        ;; Doom's leader
        (list "SPC :" 'execute-extended-command) (list "SPC ;" 'eval-expression)
        (list "SPC f f" 'find-file-command) (list "SPC SPC" 'find-file-command)
        ;; Buffers
        (list "SPC b b" 'switch-buffer) (list "SPC ," 'switch-buffer)
        (list "SPC b n" 'buffer-next) (list "SPC b p" 'buffer-previous)
        (list "] b" 'buffer-next) (list "[ b" 'buffer-previous)
        (list "SPC b k" 'buffer-kill) (list "SPC b d" 'buffer-kill) (list "SPC b N" 'buffer-new)
        ;; Windows
        (list "C-w v" 'split-right) (list "C-w s" 'split-below)
        (list "C-w h" 'window-left) (list "C-w j" 'window-down)
        (list "C-w k" 'window-up) (list "C-w l" 'window-right)
        (list "C-w C-h" 'window-left) (list "C-w C-j" 'window-down)
        (list "C-w C-k" 'window-up) (list "C-w C-l" 'window-right)
        (list "C-w w" 'window-next) (list "C-w C-w" 'window-next) (list "C-w W" 'window-previous)
        (list "C-w c" 'window-close) (list "C-w q" 'quit-window) (list "C-w o" 'window-only)
        (list "SPC w v" 'split-right) (list "SPC w s" 'split-below)
        (list "SPC w h" 'window-left) (list "SPC w j" 'window-down)
        (list "SPC w k" 'window-up) (list "SPC w l" 'window-right)
        (list "SPC w w" 'window-next) (list "SPC w d" 'window-close)
        (list "SPC w c" 'window-close) (list "SPC w o" 'window-only)
        ;; Scrolling
        (list "C-d" 'scroll-half-down) (list "C-u" 'scroll-half-up)
        (list "C-f" 'scroll-page-down) (list "C-b" 'scroll-page-up)
        (list "C-e" 'scroll-line-down) (list "C-y" 'scroll-line-up)
        (list "z z" 'recenter-center) (list "z t" 'recenter-top) (list "z b" 'recenter-bottom) (list "SPC f s" 'save) (list "SPC q q" 'quit)))

(for-each-pair (lambda (k b) (define-key visual-map k b))
  (list (list "v" (enter-visual 'visual)) (list "V" (enter-visual 'visual-line))
        (list "o" 'visual-swap-ends) (list "x" (visual-operator op-delete))
        (list "p" 'visual-paste) (list "ESC" 'exit-visual) (list ":" 'ex)
        (list "C-d" 'scroll-half-down) (list "C-u" 'scroll-half-up)
        (list "C-f" 'scroll-page-down) (list "C-b" 'scroll-page-up)
        (list "z z" 'recenter-center)))

(define-key operator-map "ESC" 'keyboard-quit)

(for-each-pair (lambda (k b) (define-key insert-map k b))
  '(("ESC" normal-state) ("RET" newline) ("TAB" indent)
    ("DEL" delete-backward) ("<delete>" delete-forward)
    ("<left>" char-left) ("<right>" char-right) ("<down>" line-down-cmd) ("<up>" line-up)))

(for-each-pair (lambda (k b) (define-key minibuffer-map k b))
  '(("RET" minibuffer-exit) ("ESC" minibuffer-escape)
    ("DEL" delete-backward) ("<left>" char-left) ("<right>" char-right)
    ("C-n" picker-next) ("C-j" picker-next) ("<down>" picker-next)
    ("C-p" picker-previous) ("C-k" picker-previous) ("<up>" picker-previous)))

(for-each-pair (lambda (k b) (define-key picker-normal-map k b))
  (list (list "j" (lambda () (picker-move current-count)))
        (list "k" (lambda () (picker-move (- 0 current-count))))
        (list "<down>" (lambda () (picker-move current-count)))
        (list "<up>" (lambda () (picker-move (- 0 current-count))))
        (list "C-n" 'picker-next) (list "C-p" 'picker-previous)
        (list "C-j" 'picker-next) (list "C-k" 'picker-previous)
        (list "g g" 'picker-first) (list "G" 'picker-last)
        (list "C-d" 'picker-page-down) (list "C-u" 'picker-page-up)
        (list "RET" 'minibuffer-exit) (list "q" 'keyboard-quit) (list "ESC" 'keyboard-quit)))

;;;; Syntax highlighting
;;
;; tree-sitter names what each piece of code is (keyword, string,
;; function.method, ...). This says what each name looks like: a face, which
;; is a theme color (red green blue yellow magenta cyan) or default, strong,
;; dim, accent, error. A name not listed uses its parent's entry
;; (function.method -> function), else plain text.

(define (syntax-theme)
  (list (list "keyword" 'magenta) (list "operator" 'dim)
        (list "string" 'green) (list "string.special" 'cyan) (list "escape" 'cyan)
        (list "comment" 'dim)
        (list "function" 'blue) (list "function.macro" 'magenta) (list "constructor" 'yellow)
        (list "type" 'yellow) (list "type.builtin" 'yellow)
        (list "constant" 'cyan) (list "constant.builtin" 'cyan) (list "number" 'cyan)
        (list "boolean" 'cyan) (list "attribute" 'cyan) (list "label" 'cyan)
        (list "property" 'default) (list "variable.builtin" 'red)
        (list "punctuation" 'dim) (list "tag" 'red) (list "embedded" 'default)))

;;;; The modeline
;;
;; A list of (text face) segments. Faces: default, strong, dim, accent,
;; error. Redefine it to change what the bottom bar shows.

(define (state-name)
  (cond ((and (minibuffer-active?) (inserting?)) "INSERT")
        ((equal? evil-state 'insert) "INSERT")
        ((equal? evil-state 'visual) "VISUAL")
        ((equal? evil-state 'visual-line) "V-LINE")
        ((equal? evil-state 'operator) "O-PENDING")
        (else "NORMAL")))

(define (modeline)
  (list (list (state-name) (if (inserting?) 'strong 'accent))
        (list (string-append (buffer-name) (if (buffer-modified?) " ●" "")) 'strong)
        (list (string-append (number->string (line-number)) ":"
                             (number->string (column-number)))
              'dim)
        (list (buffer-language) 'dim)
        (list (string-append pending-count pending-keys) 'dim)))
