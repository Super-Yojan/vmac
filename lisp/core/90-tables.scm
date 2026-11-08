;;;; Markdown tables, like Org mode
;;
;; Leaving insert state in a table aligns it (columns padded to their widest
;; cell, the |---| row redrawn); TAB and S-TAB move between cells, aligning
;; as they go, and TAB in the last cell adds a row. SPC m a aligns on demand.

(define (in-markdown-table?)
  (and (not (minibuffer-active?)) (equal? (buffer-language) "markdown") (table-at-point?)))

(defcommand (table-next-cell) (if (in-markdown-table?) (align-table 1) (indent)))
(defcommand (table-previous-cell) (when (in-markdown-table?) (align-table -1)))
(defcommand (table-align)
  (when (not (and (in-markdown-table?) (align-table 0))) (message "Not in a table")))

(for-each-pair (lambda (k b) (define-key insert-map k b)) completion-insert-keys)


;;;; Viewing Markdown
;;
;; Outside insert and visual state, a Markdown file is drawn like the
;; preview, by gpui-kit: headings, code blocks, tables, images and HTML
;; rendered. Motions still move the cursor through the text: the view
;; scrolls to its line and marks it. i, a, o... (or v) show the text as
;; typed, to edit. SPC t m cycles: rendered -> markup hidden in the text
;; (headings without #, code without backticks, still the editor's own
;; drawing) -> as typed. In init.scm: (set-markdown-view "hidden"), or
;; "rendered" or "raw".
;;
;; SPC m p opens a preview beside it instead, following the file you're in
;; as you type; again closes it. (set-markdown-preview #t) opens it from
;; the start.

(defcommand (markdown-preview) (markdown-preview-toggle))
(defcommand (markdown-conceal) (markdown-view-cycle))
