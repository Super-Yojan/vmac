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
  (if (equal? type 'block)
      (begin (delete-block! (block-ranges)) (goto-block-top-left) (clamp-to-line!))
      (op-delete-range start end type)))

(define (op-delete-range start end type)
  (register-set (range-text start end type))
  (let ((r (if (equal? type 'linewise) (linewise-range start end) (list start end))))
    (delete-region (car r) (second r))
    (goto-char (car r))
    (if (equal? type 'linewise) (goto-char (first-non-blank)) (clamp-to-line!))))

(define (op-change start end type)
  (if (equal? type 'block)
      (let ((top (block-top)) (left (block-left)) (targets (block-lines-reaching (block-left))))
        (delete-block! (block-ranges))
        (start-block-insert left top targets #f #f))
      (op-change-range start end type)))

(define (op-change-range start end type)
  (register-set (range-text start end type))
  (if (equal? type 'linewise)
      (begin (delete-region (first-non-blank-at start) (line-end-at end))
             (goto-char (first-non-blank-at start)))
      (begin (delete-region start end) (goto-char start)))
  (set! evil-state 'insert))

(define (op-yank start end type)
  (if (equal? type 'block)
      (begin (yank-block! (block-ranges)) (goto-block-top-left) (message "yanked a block"))
      (op-yank-range start end type)))

(define (op-yank-range start end type)
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

