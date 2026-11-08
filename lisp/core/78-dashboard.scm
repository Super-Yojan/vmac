;;;; The dashboard and projects
;;
;; vmac opens on a dashboard when started without a file (SPC b h shows it
;; again): what to do first, and the projects you opened lately. Each line's
;; key runs it, as does RET on it; 1-9 open a project.
;;
;; A project is a folder with .git in it. vmac remembers the projects and
;; files you open (~/.local/state/vmac). SPC p p switches project: pick one
;; (those opened before, then repositories found in ~/git, ~/code,
;; ~/projects, ~/src, ~/dev, ~/work), and the file picker opens in it. SPC p
;; a opens any folder by path. SPC f r reopens a recent file. :project PATH
;; does what SPC p a does.
;;
;; Change what it shows by redefining dashboard-banner (lines) and
;; dashboard-items ((key label command) ...), e.g. in init.scm:
;;   (define (dashboard-items)
;;     (cons (list "t" "Terminal" 'terminal-toggle) (default-dashboard-items)))
;; (define dashboard-on-start #f) turns it off at startup.

(define dashboard-on-start #t)

(define (dashboard-banner)
  (list "█   █  █   █   ███    ████"
        "█   █  ██ ██  █   █  █    "
        "█   █  █ █ █  █████  █    "
        " █ █   █   █  █   █  █    "
        "  █    █   █  █   █   ████"))

(define (default-dashboard-items)
  (list (list "f" "Find file" 'find-file-command)
        (list "r" "Recent files" 'recent-files-command)
        (list "p" "Switch project" 'project-switch)
        (list "o" "Open a folder" 'project-add)
        (list "a" "Ask the agent" 'agent-ask)
        (list "c" "Open config" 'open-config)
        (list "q" "Quit" 'quit)))

(define (dashboard-items) (default-dashboard-items))

(defcommand (dashboard)
  ;; Looked up when called (eval), so your redefinitions are the ones used.
  (let ((items ((eval 'dashboard-items))))
    ;; Each item's key runs it here; 1-9 open the projects listed.
    (set! dashboard-map '())
    (for-each (lambda (item)
                (let ((command (third item)))
                  (define-key dashboard-map (car item) (lambda () (call-binding command)))))
              items)
    (for-each (lambda (n)
                (define-key dashboard-map (number->string n)
                  (lambda () (dashboard-open-project (dashboard-project n)))))
              (list 1 2 3 4 5 6 7 8 9))
    (define-key dashboard-map "RET" 'dashboard-run)
    (dashboard-show ((eval 'dashboard-banner))
                    (map (lambda (i) (list (car i) (second i) (symbol->string (third i)))) items))))

(define (dashboard-open-project path)
  (when (not (equal? path "")) (project-switch-to path)))

;; RET: the line's item or project.
(defcommand (dashboard-run)
  (let ((action (dashboard-action)))
    (cond ((equal? action "") #f)
          ((string-prefix? "project:" action)
           (project-switch-to (substring action 8 (string-length action))))
          (else (call-binding (string->symbol action))))))

;; Opens a folder as the project, then the file picker in it.
(define (project-switch-to path)
  (let ((root (project-open path)))
    (when (not (equal? root "")) (find-file-in root))))

(defcommand (project-switch)
  (let ((projects (project-candidates)))
    (if (null? projects)
        (message "No projects yet: SPC p a opens a folder")
        (pick "Switch project" projects "" project-switch-to))))

(defcommand (project-add)
  (read-from-minibuffer "Open folder: " project-switch-to)
  (insert "~/"))

(defcommand (project-forget-command)
  (pick "Forget project" (map abbreviate-path (known-projects)) ""
        (lambda (p) (project-forget p) (message (string-append "Forgot " p)))))

(defcommand (recent-files-command)
  (let ((files (recent-files)))
    (if (null? files)
        (message "No recent files")
        (pick "Recent files" (map abbreviate-path files) ""
              (lambda (f) (find-file f))))))

(defcommand (open-config)
  (let ((dir (config-dir)))
    (find-file (string-append dir (if (file-exists? (string-append dir "/init.lua")) "/init.lua" "/init.scm")))))
