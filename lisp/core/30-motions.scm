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
  (when (equal? evil-state 'visual-block)
    (cond ((equal? motion 'line-end) (set! block-to-eol #t))
          ((not (contains? '(down up buffer-start buffer-end) motion)) (set! block-to-eol #f))))
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

