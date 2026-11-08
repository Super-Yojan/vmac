;;;; States: normal, insert, visual, visual-line, operator (after d, c, y...)

(define evil-state 'normal)

(define (visual?) (contains? '(visual visual-line visual-block) evil-state))
;; The minibuffer is for typing, except in a picker, which has its own normal
;; mode (like telescope): ESC there leaves typing, and Vim keys move through
;; the results or edit the query.
(define (picker-normal?) (and (minibuffer-active?) (picker-active?) (not (equal? evil-state 'insert))))

(define (inserting?)
  (or (equal? evil-state 'insert)
      (and (minibuffer-active?) (not (picker-active?)))))

(define picker-normal-map '())
(define tree-map '())
(define tree-buffer "*tree*")

(define (current-keymap)
  (cond ((and (equal? evil-state 'normal) (not (minibuffer-active?)) (equal? (buffer-name) tree-buffer))
         (append tree-map normal-map))
        ((picker-normal?)
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

