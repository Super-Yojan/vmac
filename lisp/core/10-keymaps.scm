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

;; All next keys after a prefix, as ((next-key label) ...).
;; A sub-prefix gets "+key", a leaf binding gets its command name.
(define (keymap-next-keys keymap prefix)
  (let ((p (string-append prefix " "))
        (plen (+ (string-length prefix) 1))
        (seen (hash)))
    (let loop ((km keymap) (result '()))
      (if (null? km)
          (reverse result)
          (let* ((entry (car km))
                 (key (car entry))
                 (binding (second entry)))
            (if (string-prefix? p key)
                (let* ((rest (substring key plen (string-length key)))
                       (sp (space-index rest))
                       (next (if sp (substring rest 0 sp) rest)))
                  (if (hash-contains? seen next)
                      (loop (cdr km) result)
                      (begin
                        (set! seen (hash-insert seen next #t))
                        (let ((full (string-append prefix " " next)))
                          (loop (cdr km)
                                (cons (list next
                                            (if (keymap-prefix? keymap full)
                                                (string-append "+" next)
                                                (if (symbol? binding)
                                                    (symbol->string binding)
                                                    "...")))
                                      result))))))
                (loop (cdr km) result)))))))

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

