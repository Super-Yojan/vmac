;;;; Plugins
;;
;; Plugins live in ~/.config/gmacs/plugins: a folder with init.scm and/or
;; init.lua, or a single name.scm / name.lua. They load after the core and
;; before your init files, so your config can change what they set. A plugin
;; that fails to load is skipped (its error shows in the list); the others
;; still load.
;;
;; SPC h p lists them (picking one turns it on or off). :plugin-install URL
;; clones one with git, :plugin-update pulls them all, :plugin-remove NAME
;; deletes one. Editing a plugin's files reloads it.

(defcommand (plugin-manager)
  (let ((items (plugin-list)))
    (if (null? items)
        (message (string-append "No plugins in " (plugin-dir)))
        (pick "Plugins (RET turns one on or off)" items ""
              (lambda (item) (plugin-toggle (substring item 0 (space-index item))))))))

(defcommand (plugin-update-all) (plugin-update))

(define (plugin-remove-asking name)
  (read-from-minibuffer (string-append "Remove the plugin " name "? (y/n) ")
    (lambda (answer)
      (if (contains? '("y" "yes") answer) (plugin-remove name) (message "kept")))))

(keymap-define! 'normal "SPC h p" 'plugin-manager)
