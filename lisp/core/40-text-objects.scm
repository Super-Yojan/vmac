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

