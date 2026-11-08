;;;; The key dispatcher
;;
;; Rust calls (dispatch-key key text) for every keystroke. `text` is what the
;; key would type ("" for ESC, RET, C-r). A count typed first (3j, 2dd, d2w)
;; is in `current-count` while the binding runs. A named command runs that
;; many times; a procedure binding runs once and reads the count itself.

(define pending-keys "")
(define pending-count "")
(define current-count 1)
(define count-given #f)

;; (read-char f): the next key is a character for f, not a command (f, t, r).
(define char-reader #f)
(define (read-char f) (set! char-reader f))

;; Called after every key while the minibuffer is open (incremental search).
(define minibuffer-update (lambda () #t))

(define (count-digit? text)
  (and (not (inserting?))
       (equal? pending-keys "")
       ;; On the dashboard, digits open its projects.
       (not (and (dashboard-here?) (not (minibuffer-active?))))
       (or (contains? '("1" "2" "3" "4" "5" "6" "7" "8" "9") text)
           (and (equal? text "0") (not (equal? pending-count ""))))))

(define (repeat n binding)
  (when (> n 0)
    (call-binding binding)
    (repeat (- n 1) binding)))

;;;; Repeating the last change: .
;;
;; Every key is recorded from the start of a command in normal state until
;; the editor is back in normal state with nothing pending. If the buffer
;; changed in between, those keys are the last change, and . types them
;; again: dw, ciwfoo<ESC>, 3x, vjd, d/foo<RET> and so on. A count on .
;; replaces the original one. Undo, redo and : commands aren't changes.

(define recording '())
(define recording-buffer "")
(define recording-revision 0)
(define last-change '())
(define replaying #f)

(define (idle-normal?)
  (and (equal? evil-state 'normal) (equal? pending-keys "") (equal? pending-count "")
       (not char-reader) (not (minibuffer-active?))))

(define (digit-key? k) (contains? '("0" "1" "2" "3" "4" "5" "6" "7" "8" "9") (car k)))

(define (drop-count keys)
  (if (and (not (null? keys)) (digit-key? (car keys))) (drop-count (cdr keys)) keys))

;; Whether recorded keys (in order) are a change . should repeat.
(define (repeatable? keys)
  (let ((first (drop-count keys)))
    (and (not (null? first))
         (not (contains? '("u" "C-r" "." ":") (car (car first)))))))

(define (dispatch-key key text)
  (when (and (not replaying) (idle-normal?))
    (set! recording '())
    (set! recording-buffer (buffer-name))
    (set! recording-revision (buffer-revision)))
  (when (not replaying) (set! recording (cons (list key text) recording)))
  (dispatch-key-1 key text)
  (when (minibuffer-active?) (minibuffer-update))
  (when (and (not replaying) (idle-normal?) (not (null? recording)))
    (let ((keys (reverse recording)))
      (when (and (equal? (buffer-name) recording-buffer)
                 (not (buffer-read-only?))
                 (not (equal? (buffer-revision) recording-revision))
                 (repeatable? keys))
        (set! last-change keys)))
    (set! recording '())))

(define (count-keys n)
  (map (lambda (c) (list (string c) (string c))) (string->list (number->string n))))

(defcommand (repeat-last-change)
  (if (null? last-change)
      (message "No change to repeat")
      (let ((keys (if count-given
                      (append (count-keys current-count) (drop-count last-change))
                      last-change)))
        (set! replaying #t)
        (for-each (lambda (k) (dispatch-key (car k) (second k))) keys)
        (set! replaying #f))))

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
               (set! count-given (not (equal? pending-count "")))
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
              ;; Typing into a terminal: every other key goes to its program.
              ((and (equal? pending-keys "") (terminal-insert?))
               (terminal-send key text))
              ((and (equal? pending-keys "") (inserting?) (not (equal? text "")))
               (insert text)
               (when (and (equal? evil-state 'insert) (not (minibuffer-active?)))
                 (after-self-insert text)))
              (else
               (set! pending-keys "")
               (set! pending-count "")
               (when (equal? evil-state 'operator) (set! evil-state 'normal))
               (message (string-append seq " is undefined")))))))

