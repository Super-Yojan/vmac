;;;; The picker (telescope-style)
;;
;; (pick title items preview-root on-pick) shows items filtered fuzzily as
;; you type, in a panel in the middle of the window. C-n/C-p, C-j/C-k or the
;; arrows move; RET calls on-pick with the item; ESC closes. When
;; preview-root is a folder, items are paths in it and the selected one is
;; previewed. Rust does the filtering; what to list and what picking does is
;; up to the caller.

;; The buffer's state before the picker opened, to go back to.
(define picker-return-state 'normal)

(define (close-picker!)
  (when (picker-active?)
    (picker-close)
    (set! evil-state picker-return-state)))

(define (pick title items preview-root on-pick)
  (set! picker-return-state (if (equal? evil-state 'insert) 'insert 'normal))
  (picker-open title items preview-root)
  (set! evil-state 'insert)
  (set! minibuffer-update (lambda () (picker-filter (minibuffer-contents))))
  (read-from-minibuffer "> "
    (lambda (query)
      (let ((choice (picker-selection)))
        (close-picker!)
        (if (equal? choice "")
            (message "No match")
            (on-pick choice))))))

(defcommand (find-file-command)
  (let ((root (project-root)))
    (pick "Find file" (project-files root) root
          (lambda (path) (find-file (string-append root "/" path))))))

(defcommand (picker-next) (picker-move 1))
(defcommand (picker-previous) (picker-move -1))
(defcommand (picker-first) (picker-select 0))
(defcommand (picker-last) (picker-select -1))
(defcommand (picker-page-down) (picker-move 10))
(defcommand (picker-page-up) (picker-move -10))

;; ESC while typing: in a picker, go to its normal mode; elsewhere, cancel.
(defcommand (minibuffer-escape)
  (if (picker-active?) (normal-state) (keyboard-quit)))
(defcommand (save) (save-buffer))
(defcommand (quit) (quit-editor))

