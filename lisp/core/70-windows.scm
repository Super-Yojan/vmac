;;;; Scrolling: C-d C-u C-f C-b C-e C-y zz zt zb

;; Keeps the cursor inside the window after it scrolls.
(define (keep-cursor-in-window)
  (let* ((top (window-top-line))
         (bottom (+ top (- (window-height) 1)))
         (line (pos->line (point))))
    (cond ((< line top) (goto-char (first-non-blank-at (line->pos top))))
          ((> line bottom) (goto-char (first-non-blank-at (line->pos bottom)))))))

;; Scrolls the window and moves the cursor the same number of lines.
(define (scroll-with-cursor n)
  (scroll-window n)
  (forward-line n)
  (when (not (visual?)) (clamp-to-line!)))

(defcommand (scroll-half-down) (scroll-with-cursor (max 1 (quotient (window-height) 2))))
(defcommand (scroll-half-up) (scroll-with-cursor (- 0 (max 1 (quotient (window-height) 2)))))
(defcommand (scroll-page-down) (scroll-with-cursor (max 1 (- (window-height) 2))))
(defcommand (scroll-page-up) (scroll-with-cursor (- 0 (max 1 (- (window-height) 2)))))
(defcommand (scroll-line-down) (scroll-window 1) (keep-cursor-in-window))
(defcommand (scroll-line-up) (scroll-window -1) (keep-cursor-in-window))
(defcommand (recenter-center) (recenter "center"))
(defcommand (recenter-top) (recenter "top"))
(defcommand (recenter-bottom) (recenter "bottom"))

;;;; Buffers and windows
;;
;; Every open file is a buffer. Windows show buffers; split them with C-w v
;; (beside) and C-w s (below), move with C-w h/j/k/l, close with C-w c.
;; Each window keeps its own cursor, even on the same buffer.

(defcommand (switch-buffer)
  (pick "Switch buffer" (buffer-list) "" (lambda (name) (switch-to-buffer name))))
(defcommand (buffer-next) (next-buffer 1))
(defcommand (buffer-previous) (next-buffer -1))
(defcommand (buffer-kill) (kill-buffer #f))
(defcommand (buffer-kill-force) (kill-buffer #t))
(defcommand (buffer-new) (new-buffer "*scratch*"))

(defcommand (split-right) (split-window "right"))
(defcommand (split-below) (split-window "below"))
(defcommand (window-left) (select-window-in-direction "left"))
(defcommand (window-right) (select-window-in-direction "right"))
(defcommand (window-up) (select-window-in-direction "up"))
(defcommand (window-down) (select-window-in-direction "down"))
(defcommand (window-next) (other-window 1))
(defcommand (window-previous) (other-window -1))
(defcommand (window-close)
  (when (not (delete-window)) (message "Can't close the last window")))
(defcommand (window-only) (delete-other-windows))

;; :q closes the window, and quits from the last one.
(defcommand (quit-window) (when (not (delete-window)) (quit-editor)))

