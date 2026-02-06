;;; ewm-test.el --- Debug snapshots for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Capture screenshots with layout debug info for manual inspection.
;;
;; Usage:
;;   1. Load: (load "ewm-test.el")
;;   2. Display a surface buffer
;;   3. Run: M-x ewm-test-snapshot
;;   4. Check /tmp/ewm-snapshot.png and /tmp/ewm-snapshot.txt

;;; Code:

(require 'ewm)

(defun ewm-test-snapshot ()
  "Capture a screenshot and layout debug info for manual inspection.
Saves screenshot to /tmp/ewm-snapshot.png and debug info to /tmp/ewm-snapshot.txt."
  (interactive)
  (let ((screenshot-path "/tmp/ewm-snapshot.png")
        (debug-path "/tmp/ewm-snapshot.txt"))
    ;; Collect debug info
    (with-temp-buffer
      (insert (format "=== EWM Layout Snapshot ===\n"))
      (insert (format "Timestamp: %s\n\n" (current-time-string)))
      ;; Frame info
      (let* ((frame (selected-frame))
             (geometry (frame-geometry frame)))
        (insert (format "Frame:\n"))
        (insert (format "  undecorated: %S\n" (frame-parameter frame 'undecorated)))
        (insert (format "  menu-bar-lines: %S\n" (frame-parameter frame 'menu-bar-lines)))
        (insert (format "  menu-bar-size: %S\n" (alist-get 'menu-bar-size geometry)))
        (insert (format "  tool-bar-size: %S\n" (alist-get 'tool-bar-size geometry)))
        (insert (format "  tab-bar-size: %S\n" (alist-get 'tab-bar-size geometry)))
        (insert (format "  ewm-csd-height: %S\n" ewm-csd-height))
        (insert (format "  y-offset: %d\n\n" (ewm--frame-y-offset frame))))
      ;; Window info
      (insert "Windows:\n")
      (dolist (window (window-list nil 'no-minibuf))
        (let* ((buf (window-buffer window))
               (id (buffer-local-value 'ewm-surface-id buf))
               (edges (ewm--window-inside-absolute-pixel-edges window))
               (selected (eq window (selected-window))))
          (insert (format "  %s%s\n" (buffer-name buf) (if selected " [SELECTED]" "")))
          (when id
            (insert (format "    surface-id: %d\n" id)))
          (insert (format "    edges: %S\n" edges))
          (when id
            (let* ((x (nth 0 edges))
                   (y (nth 1 edges))
                   (w (- (nth 2 edges) x))
                   (h (- (nth 3 edges) y))
                   (y-offset (ewm--frame-y-offset (window-frame window)))
                   (final-y (+ y y-offset)))
              (insert (format "    sent to compositor: (%d, %d) %dx%d\n" x final-y w h))))))
      (write-region (point-min) (point-max) debug-path))
    ;; Take screenshot
    (ewm-screenshot screenshot-path)
    (message "Snapshot saved: %s and %s" screenshot-path debug-path)))

(provide 'ewm-test)
;;; ewm-test.el ends here
