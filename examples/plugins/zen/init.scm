;;; zen: SPC t z hides the file tree and other windows, to focus on one.
;;; A Scheme plugin; it uses the core's commands and keymaps like init.scm.

(defcommand (zen-mode)
  (when (sidebar-open?) (sidebar-close))
  (delete-other-windows)
  (recenter "center")
  (message "zen"))

(define-key normal-map "SPC t z" 'zen-mode)
