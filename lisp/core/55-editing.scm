;;;; Insert state

(defcommand (normal-state)
  (set! evil-state 'normal)
  (completion-close)
  (when block-insert (finish-block-insert))
  (when (in-markdown-table?) (align-table 0))
  (when (> (point) (line-start)) (forward-char -1))
  (message ""))
(defcommand (insert-state) (set! evil-state 'insert))
(defcommand (insert-after) (when (< (point) (line-end)) (forward-char 1)) (insert-state))
(defcommand (insert-line-start) (goto-char (first-non-blank)) (insert-state))
(defcommand (insert-line-end) (goto-char (line-end)) (insert-state))
(defcommand (open-below) (goto-char (line-end)) (insert "\n") (insert-state))
(defcommand (open-above) (goto-char (line-start)) (insert "\n") (forward-char -1) (insert-state))

(defcommand (newline) (insert "\n"))
(defcommand (indent) (insert "    "))
(defcommand (delete-backward) (delete-backward-char 1))
(defcommand (delete-forward) (delete-char 1))
(defcommand (char-left) (forward-char -1))
(defcommand (char-right) (forward-char 1))
(defcommand (line-up) (forward-line -1))
(defcommand (line-down-cmd) (forward-line 1))

;;;; Other normal-state commands

(defcommand (delete-char-under)
  (when (< (point) (line-end))
    (register-set (delete-region (point) (+ (point) 1)))
    (clamp-to-line!)))

(define (linewise? text)
  (and (> (string-length text) 0)
       (equal? (substring text (- (string-length text) 1) (string-length text)) "\n")))

(define (without-last-char text) (substring text 0 (- (string-length text) 1)))

;; Pastes the block register as a column, starting at `column` of this line.
(define (paste-block column)
  (let ((first-line (pos->line (point))))
    (let loop ((lines block-register-lines) (line first-line))
      (when (not (null? lines))
        (when (>= line (line-count))
          (goto-char (point-max)) (insert "\n"))
        (let* ((s (line->pos line)) (len (- (line-end-at s) s)))
          (if (< len column)
              (begin (goto-char (+ s len))
                     (insert (string-append (make-string (- column len) #\space) (car lines))))
              (begin (goto-char (+ s column)) (insert (car lines)))))
        (loop (cdr lines) (+ line 1))))
    (goto-char (+ (line->pos first-line) column))))

(defcommand (paste-after)
  (let ((text (register-get)))
    (cond ((block-register?)
           (paste-block (if (empty-line? (point)) 0 (+ 1 (column-at (point))))))
          ((linewise? text)
           (if (< (line-end) (point-max))
               (begin (goto-char (+ (line-end) 1)) (insert text) (forward-line -1))
               (begin (goto-char (line-end)) (insert (string-append "\n" (without-last-char text)))))
           (goto-char (first-non-blank)))
          ((> (string-length text) 0)
           (when (< (point) (line-end)) (forward-char 1))
           (insert text)
           (forward-char -1)))))

(defcommand (paste-before)
  (let ((text (register-get)))
    (cond ((block-register?) (paste-block (column-at (point))))
          ((linewise? text)
           (goto-char (line-start)) (insert text) (forward-line -1) (goto-char (first-non-blank)))
          ((> (string-length text) 0)
           (insert text) (forward-char -1)))))

(defcommand (evil-undo) (when (not (undo)) (message "Already at oldest change")) (clamp-to-line!))
(defcommand (evil-redo) (when (not (redo)) (message "Already at newest change")) (clamp-to-line!))

(defcommand (keyboard-quit)
  (set! pending-keys "")
  (set! pending-count "")
  (set! char-reader #f)
  (set! search-preview #f)
  (set! search-highlight? #f)
  (set! minibuffer-update (lambda () #t))
  (if (picker-active?)
      (close-picker!)
      (when (not (equal? evil-state 'insert)) (set! evil-state 'normal)))
  (when (minibuffer-active?) (minibuffer-close))
  (message "Quit"))


;;;; The system clipboard
;;
;; Yanks and deletes also copy to the system clipboard, and p/P paste what
;; was copied last anywhere: text copied in another app wins over an older
;; yank. (set-clipboard-sync #f) keeps vmac's register to itself. Cmd-V
;; pastes the clipboard at the cursor (normal, insert, prompts, the picker,
;; terminals); Cmd-C copies the visual selection.

(defcommand (clipboard-paste)
  (insert (clipboard-get))
  (when (picker-active?) (picker-filter (minibuffer-contents))))

(defcommand (clipboard-copy)
  ((visual-operator op-yank))
  (clipboard-set (register-get))
  (message "Copied"))
