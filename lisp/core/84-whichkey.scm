;;;; Which-key: show available keys when a prefix is pending.

(define (whichkey-open prefix)
  (let* ((keymap (current-keymap))
         (next (keymap-next-keys keymap prefix))
         (flat (apply append (map (lambda (pair) (list (car pair) (second pair))) next))))
    (when (not (null? flat))
      (whichkey-show flat))))
