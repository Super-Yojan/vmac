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

