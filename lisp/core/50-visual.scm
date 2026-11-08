;;;; Visual mode
;;
;; The selection runs from `visual-anchor` to the cursor, both included.
;; Rust draws whatever (selection) returns.

(define visual-anchor 0)

;; Visual block (C-v): the rectangle between the anchor and the cursor,
;; columns included. After $ it reaches each line's end.
(define block-to-eol #f)

(define (column-at pos) (- pos (line-start-at pos)))
(define (block-top) (min (pos->line visual-anchor) (pos->line (point))))
(define (block-bottom) (max (pos->line visual-anchor) (pos->line (point))))
(define (block-left) (min (column-at visual-anchor) (column-at (point))))
(define (block-right) (+ 1 (max (column-at visual-anchor) (column-at (point)))))

;; One (start end) per line of the block; empty on lines too short to reach it.
(define (block-ranges)
  (let loop ((line (block-bottom)) (out '()))
    (if (< line (block-top))
        out
        (let* ((s (line->pos line))
               (len (- (line-end-at s) s))
               (a (+ s (min (block-left) len)))
               (b (if block-to-eol (+ s len) (+ s (min (block-right) len)))))
          (loop (- line 1) (cons (list a (max a b)) out))))))

(define (visual-bounds)
  (let ((lo (min visual-anchor (point))) (hi (max visual-anchor (point))))
    (cond ((equal? evil-state 'visual-line) (list (line-start-at lo) (line-end-at hi) 'linewise))
          ((equal? evil-state 'visual-block) (list (line->pos (block-top)) (line->pos (block-bottom)) 'block))
          (else (list lo (min (point-max) (+ hi 1)) 'exclusive)))))

(define (selection)
  (if (visual?)
      (let ((b (visual-bounds)))
        (cond ((equal? (third b) 'linewise) (list (car b) (min (point-max) (+ (second b) 1))))
              ((equal? (third b) 'block) (block-ranges))
              (else (list (car b) (second b)))))
      '()))

;; The block register: a yanked or deleted block, pasted back as a column.
;; It is the register only while the register still holds that text.
(define block-register-lines #f)
(define block-register-text "")

(define (join-lines lines)
  (cond ((null? lines) "")
        ((null? (cdr lines)) (car lines))
        (else (string-append (car lines) "\n" (join-lines (cdr lines))))))

(define (block-register?)
  (and block-register-lines (equal? (register-get) block-register-text)))

(define (yank-block! ranges)
  (let ((lines (map (lambda (r) (buffer-substring (car r) (second r))) ranges)))
    (set! block-register-lines lines)
    (set! block-register-text (join-lines lines))
    (register-set block-register-text)))

(define (goto-block-top-left)
  (let* ((s (line->pos (block-top))) (len (- (line-end-at s) s)))
    (goto-char (+ s (min (block-left) len)))))

(define (delete-block! ranges)
  (yank-block! ranges)
  (for-each (lambda (r) (delete-region (car r) (second r))) (reverse ranges)))

;; I, A and c in a block: type on the first line; ESC copies what was typed
;; to the block's other lines, at the same column. Which lines get it is
;; decided when typing starts: for I and c the lines that reach into the
;; block, for A every line (short ones padded with spaces).
(define block-insert #f)

(define (line-length line) (let ((s (line->pos line))) (- (line-end-at s) s)))

;; Lines after the first of the block that reach past `column`.
(define (block-lines-reaching column)
  (let loop ((line (block-bottom)) (out '()))
    (if (<= line (block-top))
        out
        (loop (- line 1) (if (> (line-length line) column) (cons line out) out)))))

(define (block-lines-after-top)
  (let loop ((line (block-bottom)) (out '()))
    (if (<= line (block-top)) out (loop (- line 1) (cons line out)))))

(define (start-block-insert column top targets to-eol pad)
  (let* ((s (line->pos top)) (len (- (line-end-at s) s)))
    (cond (to-eol (goto-char (+ s len)))
          ((< len column) (goto-char (+ s len))
                          (when pad (insert (make-string (- column len) #\space))))
          (else (goto-char (+ s column))))
    (set! block-insert (list column top targets to-eol pad (point)))
    (set! evil-state 'insert)))

(define (finish-block-insert)
  (let* ((column (car block-insert)) (top (second block-insert))
         (targets (third block-insert))
         (to-eol (list-ref block-insert 3)) (pad (list-ref block-insert 4))
         (start (list-ref block-insert 5))
         (typed (if (and (>= (point) start) (= (pos->line (point)) top))
                    (buffer-substring start (point))
                    "")))
    (set! block-insert #f)
    (when (not (equal? typed ""))
      (for-each
       (lambda (line)
         (let* ((s (line->pos line)) (len (- (line-end-at s) s)))
           (cond (to-eol (goto-char (+ s len)) (insert typed))
                 ((>= len column) (goto-char (+ s column)) (insert typed))
                 (pad (goto-char (+ s len))
                      (insert (string-append (make-string (- column len) #\space) typed))))))
       targets))
    (goto-char start)))

(defcommand (block-insert-start)
  (if (equal? evil-state 'visual-block)
      (start-block-insert (block-left) (block-top) (block-lines-reaching (block-left)) #f #f)
      (begin (set! evil-state 'normal) (goto-char (min visual-anchor (point))) (insert-state))))

(defcommand (block-append)
  (if (equal? evil-state 'visual-block)
      (start-block-insert (block-right) (block-top) (block-lines-after-top) block-to-eol #t)
      (begin (set! evil-state 'normal)
             (goto-char (min (point-max) (+ 1 (max visual-anchor (point)))))
             (insert-state))))

;; O in a block: the cursor goes to the other side, same line.
(defcommand (visual-swap-columns)
  (if (equal? evil-state 'visual-block)
      (let* ((ac (column-at visual-anchor)) (pc (column-at (point)))
             (a-line (line-start-at visual-anchor)) (p-line (line-start-at (point))))
        (set! visual-anchor (+ a-line (min pc (- (line-end-at a-line) a-line))))
        (goto-char (+ p-line (min ac (- (line-end-at p-line) p-line)))))
      (visual-swap-ends)))

(define (enter-visual state)
  (lambda ()
    (if (equal? evil-state state)
        (exit-visual)
        (begin (when (not (visual?)) (set! visual-anchor (point)))
               (set! block-to-eol #f)
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

