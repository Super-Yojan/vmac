;;;; Language servers (LSP)
;;
;; Which server runs for which language: (language command arg ...). A
;; server starts when a file of its language opens, one per project.
;; Errors and warnings are underlined; ]d and [d jump between them, SPC c x
;; lists them. Change this list to use other servers.

(define (lsp-servers)
  (list (list "rust" "rust-analyzer")
        (list "python" "pyright-langserver" "--stdio")
        (list "typescript" "typescript-language-server" "--stdio")
        (list "tsx" "typescript-language-server" "--stdio")
        (list "javascript" "typescript-language-server" "--stdio")
        (list "c" "clangd")
        (list "go" "gopls")))

;; Completion: typing a word asks the server; the popup then narrows as you
;; type. . and : ask afresh (foo. std::); other characters close it.
(define (after-self-insert text)
  (cond ((word-char? text) (if (completion-active?) (completion-refresh) (lsp-complete)))
        ((contains? '("." ":") text) (completion-close) (lsp-complete))
        (else (completion-close))))

;; Insert-state keys that act on the popup while it's open, else as usual.
(define (with-completion when-open otherwise)
  (lambda () (if (completion-active?) (when-open) (call-binding otherwise))))

(define completion-insert-keys
  (list (list "C-n" (with-completion (lambda () (completion-move 1)) 'line-down-cmd))
        (list "C-p" (with-completion (lambda () (completion-move -1)) 'line-up))
        (list "<down>" (with-completion (lambda () (completion-move 1)) 'line-down-cmd))
        (list "<up>" (with-completion (lambda () (completion-move -1)) 'line-up))
        (list "RET" (with-completion (lambda () (completion-accept)) 'newline))
        (list "TAB" (with-completion (lambda () (completion-accept)) 'table-next-cell))
        (list "DEL" (lambda () (delete-backward-char 1) (completion-refresh)))))

;; gr: everywhere it's used, in the picker (straight there if only one).
(define (lsp-show-references items)
  (pick "References" items (project-root) (lambda (item) (goto-location item))))

;; SPC c r: rename everywhere, starting from the current name.
(defcommand (rename-symbol)
  (let ((name (word-at-point)))
    (read-from-minibuffer "Rename to: "
      (lambda (new) (when (not (or (equal? new "") (equal? new name))) (lsp-rename new))))
    (insert name)))

;; SPC c a: fixes and refactorings for the cursor, or the selection.
(defcommand (code-actions)
  (if (visual?)
      (let ((b (visual-bounds)))
        (set! evil-state 'normal)
        (lsp-code-actions (car b) (second b)))
      (lsp-code-actions (point) (point))))

;; "2. Title" -> the second action.
(define (lsp-show-code-actions items)
  (pick "Code actions" items ""
        (lambda (item)
          (let ((dot (let loop ((i 0)) (if (equal? (substring item i (+ i 1)) ".") i (loop (+ i 1))))))
            (apply-code-action (- (string->number (substring item 0 dot)) 1))))))

;; SPC c R or :lsp-restart: stop the language servers and start them again
;; (also those that failed to start, e.g. after installing one).
(defcommand (lsp-restart-command) (lsp-restart))

;; SPC c f: format the file, or the selected lines.
(defcommand (format-buffer)
  (if (visual?)
      (let ((b (visual-bounds)))
        (set! evil-state 'normal)
        (lsp-format-range (car b) (second b)))
      (lsp-format)))

(defcommand (goto-definition) (lsp-goto-definition))
(defcommand (find-references) (lsp-references))
(defcommand (hover-documentation) (lsp-hover))
(defcommand (jump-back-command) (jump-back))

(defcommand (diagnostic-next) (diagnostic-goto 1))
(defcommand (diagnostic-previous) (diagnostic-goto -1))

;; "12:5  error: ..." -> goes to line 12, column 5.
(define (goto-diagnostic-item item)
  (let* ((colon (let loop ((i 0)) (if (equal? (substring item i (+ i 1)) ":") i (loop (+ i 1)))))
         (rest (substring item (+ colon 1) (string-length item)))
         (space (space-index rest)))
    (goto-line-column (string->number (substring item 0 colon))
                      (string->number (substring rest 0 space)))))

(defcommand (diagnostic-list)
  (let ((items (diagnostic-items)))
    (if (null? items)
        (message "No diagnostics")
        (pick "Diagnostics" items "" goto-diagnostic-item))))

