;;; ewm-surface.el --- Surface buffer management for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Surface buffer management for EWM.
;; Provides the major mode for surface buffers and buffer lifecycle management.

;;; Code:

(declare-function ewm-close "ewm")

(defvar ewm--module-mode)
(defvar ewm-input--mode)

(defvar-local ewm-surface-id nil
  "Surface ID for this buffer.")

(defvar-local ewm-surface-app nil
  "Application name (app_id) for this buffer.
Similar to `exwm-class-name'.")

(defvar-local ewm-surface-title nil
  "Window title for this buffer.
Similar to `exwm-title'.")

(defun ewm--kill-buffer-query-function ()
  "Run in `kill-buffer-query-functions' for surface buffers.
Sends close request to compositor and prevents immediate buffer kill."
  (if (not (and ewm-surface-id ewm--module-mode))
      t  ; Not a surface buffer or compositor not running
    ;; Request graceful close via xdg_toplevel.close
    (ewm-close ewm-surface-id)
    nil))

(defun ewm-surface-mode-line-mode ()
  "Return mode-line indicator for current input mode."
  (if (eq ewm-input--mode 'char-mode)
      "[C]"
    "[L]"))

(define-derived-mode ewm-surface-mode fundamental-mode "EWM"
  "Major mode for EWM surface buffers.
\\<ewm-surface-mode-map>
In line-mode (default), keys go to Emacs.
In char-mode, keys go directly to the surface.

\\[ewm-input-char-mode] - switch to char-mode
\\[ewm-input-line-mode] - switch to line-mode
\\[ewm-input-toggle-mode] - toggle input mode"
  (setq buffer-read-only t)
  (setq-local cursor-type nil)
  (setq-local left-fringe-width 0)
  (setq-local right-fringe-width 0)
  (setq-local show-trailing-whitespace nil)
  ;; Set up mode line to show input mode
  (setq mode-name '("EWM" (:eval (ewm-surface-mode-line-mode))))
  ;; Kill buffer -> close window (like EXWM)
  (add-hook 'kill-buffer-query-functions
            #'ewm--kill-buffer-query-function nil t))

;; Keybindings for surface mode
(define-key ewm-surface-mode-map (kbd "C-c C-k") #'ewm-input-char-mode)
(define-key ewm-surface-mode-map (kbd "C-c C-t") #'ewm-input-toggle-mode)

(provide 'ewm-surface)
;;; ewm-surface.el ends here
