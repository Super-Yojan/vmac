;;;; The modeline
;;
;; A list of (text face) segments. Faces: default, strong, dim, accent,
;; error. Redefine it to change what the bottom bar shows.

(define (state-name)
  (cond ((and (minibuffer-active?) (inserting?)) "INSERT")
        ((equal? evil-state 'insert) "INSERT")
        ((equal? evil-state 'visual) "VISUAL")
        ((equal? evil-state 'visual-line) "V-LINE")
        ((equal? evil-state 'visual-block) "V-BLOCK")
        ((equal? evil-state 'operator) "O-PENDING")
        (else "NORMAL")))

(define (modeline)
  (list (list (state-name) (if (inserting?) 'strong 'accent))
        (list (string-append (buffer-name) (if (buffer-modified?) " ●" "")) 'strong)
        (list (string-append (number->string (line-number)) ":"
                             (number->string (column-number)))
              'dim)
        (list (buffer-language) 'dim)
        (list (diagnostic-summary) 'error)
        (list (lsp-status) 'dim)
        (list (string-append pending-count pending-keys) 'dim)))
