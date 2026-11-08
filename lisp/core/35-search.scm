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

