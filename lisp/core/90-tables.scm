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

