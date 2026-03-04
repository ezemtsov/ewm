;;; ewm-surface.el --- Surface buffer management for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Surface buffer management for EWM.
;; Provides the major mode for surface buffers and buffer lifecycle management.

;;; Code:

(declare-function ewm-close "ewm")
(declare-function ewm-im-commit "ewm-text-input")

(defvar ewm--module-mode)
(defvar-local ewm-surface-id nil
  "Surface ID for this buffer.")

(defvar-local ewm-surface-app nil
  "Application name (app_id) for this buffer.
Similar to `exwm-class-name'.")

(defvar-local ewm-surface-title nil
  "Window title for this buffer.
Similar to `exwm-title'.")

(defvar-local ewm-surface-pid nil
  "Client PID for this surface.")

(defvar-local ewm-surface-fullscreen nil
  "Non-nil when this surface is in fullscreen mode.")

(defun ewm--kill-buffer-query-function ()
  "Run in `kill-buffer-query-functions' for surface buffers.
Sends close request to compositor and prevents immediate buffer kill."
  (if (not (and ewm-surface-id ewm--module-mode))
      t  ; Not a surface buffer or compositor not running
    ;; Request graceful close via xdg_toplevel.close
    (ewm-close ewm-surface-id)
    nil))

(defun ewm-surface--after-change (beg end old-len)
  "Intercept insertions and route through im-commit.
Pure insertions are captured, removed from the buffer, and
forwarded to the client text field via `ewm-im-commit'."
  (when (and (> end beg) (zerop old-len))
    (let ((text (buffer-substring-no-properties beg end))
          (inhibit-modification-hooks t)
          (buffer-undo-list t))
      (delete-region beg end)
      (ewm-im-commit text ewm-surface-id))))

(define-derived-mode ewm-surface-mode fundamental-mode "Surface"
  "Major mode for EWM surface buffers."
  ;; Surface buffers are visual proxies — no editable text content.
  ;; Insertions are intercepted and routed to the client text field.
  (buffer-disable-undo)
  (add-hook 'after-change-functions #'ewm-surface--after-change nil t)
  (setq-local cursor-type nil)
  (setq-local left-fringe-width 0)
  (setq-local right-fringe-width 0)
  (setq-local show-trailing-whitespace nil)
  ;; Kill buffer -> close window (like EXWM)
  (add-hook 'kill-buffer-query-functions
            #'ewm--kill-buffer-query-function nil t))

(provide 'ewm-surface)
;;; ewm-surface.el ends here
