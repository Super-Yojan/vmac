;;;; The file tree (SPC o p)
;;
;; A sidebar at the left edge showing the project's folders. It is an
;; ordinary (read-only) buffer, so every motion works in it; its own keys
;; are in tree-map. Folders hidden by .gitignore don't show.

(define tree-root "")
(define tree-expanded '())
;; What each line shows, in order: (path folder? depth). Line 0 is the root.
(define tree-entries '())

(define (path-join dir name) (string-append dir "/" name))

(define (basename path)
  (let loop ((i (- (string-length path) 1)))
    (cond ((< i 0) path)
          ((equal? (substring path i (+ i 1)) "/") (substring path (+ i 1) (string-length path)))
          (else (loop (- i 1))))))

(define (dirname path)
  (let loop ((i (- (string-length path) 1)))
    (cond ((<= i 0) "/")
          ((equal? (substring path i (+ i 1)) "/") (substring path 0 i))
          (else (loop (- i 1))))))

(define (ends-with-slash? s)
  (and (> (string-length s) 0)
       (equal? (substring s (- (string-length s) 1) (string-length s)) "/")))

(define (without-slash s) (if (ends-with-slash? s) (substring s 0 (- (string-length s) 1)) s))

(define (expanded? dir) (contains? tree-expanded dir))
(define (expand! dir) (when (not (expanded? dir)) (set! tree-expanded (cons dir tree-expanded))))
(define (collapse! dir) (set! tree-expanded (filter (lambda (d) (not (equal? d dir))) tree-expanded)))

;; Entries under dir, recursing into expanded folders.
(define (tree-collect dir depth)
  (let loop ((names (directory-entries dir)) (out '()))
    (if (null? names)
        (reverse out)
        (let* ((name (car names))
               (folder (ends-with-slash? name))
               (path (path-join dir (without-slash name)))
               (entry (list path folder depth))
               (below (if (and folder (expanded? path)) (tree-collect path (+ depth 1)) '())))
          (loop (cdr names) (append (reverse below) (cons entry out)))))))

;; Faces by file extension, so the tree isn't one color.
(define (file-face name)
  (let ((ext (let loop ((i (- (string-length name) 1)))
               (cond ((< i 0) "")
                     ((equal? (substring name i (+ i 1)) ".") (substring name (+ i 1) (string-length name)))
                     (else (loop (- i 1)))))))
    (cond ((contains? '("rs" "py" "js" "ts" "tsx" "go" "c" "h" "scm" "sh") ext) 'default)
          ((contains? '("md" "txt") ext) 'cyan)
          ((contains? '("toml" "json" "yaml" "yml" "lock") ext) 'yellow)
          (else 'default))))

;; Redraws the tree buffer, keeping the cursor on the same path.
(define (tree-render)
  (let ((here (tree-entry-at-point)))
    (set! tree-entries (cons (list tree-root #t -1) (tree-collect tree-root 0)))
    (let loop ((entries (cdr tree-entries))
               (text (string-append (basename tree-root) "\n"))
               (faces (list (list 0 (string-length (basename tree-root)) 'accent))))
      (if (null? entries)
          (begin
            (set-buffer-text tree-buffer (substring text 0 (- (string-length text) 1)))
            (for-each (lambda (f) (add-face tree-buffer (car f) (second f) (symbol->string (third f))))
                      faces)
            (when here (tree-goto-path (car here))))
          (let* ((e (car entries)) (path (car e)) (folder (second e)) (depth (third e))
                 (indent (make-string (* 2 depth) #\space))
                 (icon (cond ((not folder) "  ") ((expanded? path) "▾ ") (else "▸ ")))
                 (name (basename path))
                 (start (+ (string-length text) (string-length indent)))
                 (name-start (+ start (string-length icon)))
                 (line (string-append indent icon name (if folder "/" "") "\n")))
            (loop (cdr entries)
                  (string-append text line)
                  (append (if folder
                              (list (list start name-start 'dim)
                                    (list name-start (+ name-start (string-length name) 1) 'blue))
                              (list (list name-start (+ name-start (string-length name)) (file-face name))))
                          faces)))))))

(define (tree-entry-at-point)
  (let ((line (- (line-number) 1)))
    (and (equal? (buffer-name) tree-buffer)
         (< line (length tree-entries))
         (list-ref tree-entries line))))

(define (tree-goto-path path)
  (let loop ((entries tree-entries) (line 0))
    (cond ((null? entries) #f)
          ((equal? (car (car entries)) path)
           (goto-char (first-non-blank-at (line->pos line))) #t)
          (else (loop (cdr entries) (+ line 1))))))

;; Expands every folder between the root and a file, so it shows.
(define (tree-reveal file)
  (when (string-prefix? (string-append tree-root "/") file)
    (let loop ((dir (dirname file)))
      (when (and (string-prefix? tree-root dir) (not (equal? dir tree-root)))
        (expand! dir)
        (loop (dirname dir))))))

(defcommand (tree-open)
  (let ((file (buffer-file-name)) (root (project-root)))
    (when (not (equal? root tree-root))
      (set! tree-root root)
      (set! tree-expanded '()))
    (ensure-buffer tree-buffer)
    (set-buffer-read-only tree-buffer #t)
    (sidebar-open tree-buffer)
    (set! evil-state 'normal)
    (tree-reveal file)
    (tree-render)
    (when (not (tree-goto-path file)) (goto-char 0))))

(defcommand (tree-close) (sidebar-close))

(defcommand (tree-toggle) (if (sidebar-open?) (tree-close) (tree-open)))

;; RET, o: a folder opens or closes; a file opens in the main window.
(defcommand (tree-activate)
  (let ((e (tree-entry-at-point)))
    (cond ((not e) #f)
          ((equal? (third e) -1) (tree-refresh))
          ((second e)
           (if (expanded? (car e)) (collapse! (car e)) (expand! (car e)))
           (tree-render))
          (else (select-main-window) (find-file (car e))))))

;; l: open a folder (or a file); h: close the folder, else go to its parent.
(defcommand (tree-expand)
  (let ((e (tree-entry-at-point)))
    (when e
      (if (and (second e) (not (expanded? (car e))))
          (begin (expand! (car e)) (tree-render))
          (tree-activate)))))

(defcommand (tree-collapse)
  (let ((e (tree-entry-at-point)))
    (when (and e (not (equal? (third e) -1)))
      (if (and (second e) (expanded? (car e)))
          (begin (collapse! (car e)) (tree-render))
          (tree-goto-path (dirname (car e)))))))

(defcommand (tree-refresh) (tree-render) (message "tree refreshed"))

;; The folder new things go in: the folder on this line, else the line's folder.
(define (tree-target-dir)
  (let ((e (tree-entry-at-point)))
    (cond ((not e) tree-root)
          ((second e) (car e))
          (else (dirname (car e))))))

(defcommand (tree-add)
  (let ((dir (tree-target-dir)))
    (read-from-minibuffer (string-append "New file in " (basename dir) "/ (end with / for a folder): ")
      (lambda (name)
        (when (not (equal? name ""))
          (let ((path (path-join dir name)))
            (when (create-path path)
              (expand! dir)
              (tree-render)
              (tree-goto-path (without-slash path))
              (message (string-append "created " name)))))))))

(defcommand (tree-rename)
  (let ((e (tree-entry-at-point)))
    (when (and e (not (equal? (third e) -1)))
      (read-from-minibuffer (string-append "Rename " (basename (car e)) " to: ")
        (lambda (name)
          (when (not (equal? name ""))
            (let ((to (path-join (dirname (car e)) name)))
              (when (rename-path (car e) to)
                (tree-render)
                (tree-goto-path to)
                (message (string-append "renamed to " name))))))))))

(defcommand (tree-delete)
  (let ((e (tree-entry-at-point)))
    (when (and e (not (equal? (third e) -1)))
      (read-from-minibuffer (string-append "Delete " (basename (car e)) (if (second e) "/ and everything in it" "") "? (y/n) ")
        (lambda (answer)
          (if (contains? '("y" "yes") answer)
              (when (delete-path (car e))
                (collapse! (car e))
                (tree-render)
                (message (string-append "deleted " (basename (car e)))))
              (message "not deleted")))))))

(for-each-pair (lambda (k b) (define-key tree-map k b))
  (list (list "RET" 'tree-activate) (list "o" 'tree-activate) (list "TAB" 'tree-activate)
        (list "l" 'tree-expand) (list "h" 'tree-collapse)
        (list "a" 'tree-add) (list "r" 'tree-rename) (list "d" 'tree-delete)
        (list "R" 'tree-refresh) (list "q" 'tree-close)))

