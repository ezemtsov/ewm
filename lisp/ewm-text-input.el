;;; ewm-text-input.el --- Text input support for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Text input protocol support for EWM.
;; Handles input method integration for Wayland clients.

;;; Code:

(declare-function ewm-im-commit-module "ewm-core")
(declare-function ewm-text-input-intercept-module "ewm-core")
(declare-function ewm-get-focused-id "ewm-core")

(defvar ewm--surfaces)

(defvar ewm-text-input-activated-hook nil
  "Hook run when a client text field becomes active.
Use this to enable special input handling modes.")

(defvar ewm-text-input-deactivated-hook nil
  "Hook run when a client text field becomes inactive.")

(defun ewm--handle-text-input-activated ()
  "Handle text-input-activated event from compositor.
Called when a client's text field gains focus."
  (run-hooks 'ewm-text-input-activated-hook))

(defun ewm--handle-text-input-deactivated ()
  "Handle text-input-deactivated event from compositor.
Called when a client's text field loses focus."
  (run-hooks 'ewm-text-input-deactivated-hook))

(defun ewm-im-commit (text)
  "Commit TEXT to the currently focused client text field."
  (ewm-im-commit-module text))

(defvar ewm-text-input-method nil
  "Input method to use for text input translation.
When nil, uses `current-input-method' or `default-input-method'.")

(defun ewm-text-input--translate-char (char &optional input-method)
  "Translate CHAR through INPUT-METHOD if provided.
If INPUT-METHOD is nil, uses `ewm-text-input-method' or `current-input-method'.
For quail-based input methods, looks up the translation directly."
  (let ((im (or input-method
                ewm-text-input-method
                current-input-method)))
    (if (and im (fboundp 'quail-lookup-key))
        (let ((current-input-method im))
          (activate-input-method im)
          (let ((result (quail-lookup-key (string char))))
            (cond
             ((and (consp result) (integerp (car result)))
              ;; Quail returns (charcode) - convert to string
              (string (car result)))
             ((stringp result) result)
             (t (string char)))))
      (string char))))

(defun ewm-text-input--self-insert ()
  "Handle self-insert when text input mode is active.
Sends the typed character to the client via commit_string,
applying input method translation if active."
  (interactive)
  (let* ((char last-command-event)
         (translated (ewm-text-input--translate-char char)))
    (when (stringp translated)
      (ewm-im-commit translated))))

(defvar ewm-text-input-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map [remap self-insert-command] #'ewm-text-input--self-insert)
    map)
  "Keymap for `ewm-text-input-mode'.")

(define-minor-mode ewm-text-input-mode
  "Minor mode for typing in client text fields.
When enabled, regular keystrokes are sent to the focused client
text field via input method commit_string, while Emacs commands
like C-x and M-x continue to work normally.

Input method translations (e.g., russian-computer) are applied."
  :lighter " TxtIn"
  :keymap ewm-text-input-mode-map)

(defun ewm-text-input-auto-mode-enable ()
  "Enable automatic text input mode switching.
Text input mode will be enabled/disabled automatically when
client text fields gain/lose focus."
  (interactive)
  (add-hook 'ewm-text-input-activated-hook #'ewm-text-input--auto-enable)
  (add-hook 'ewm-text-input-deactivated-hook #'ewm-text-input--auto-disable))

(defun ewm-text-input-auto-mode-disable ()
  "Disable automatic text input mode switching."
  (interactive)
  (remove-hook 'ewm-text-input-activated-hook #'ewm-text-input--auto-enable)
  (remove-hook 'ewm-text-input-deactivated-hook #'ewm-text-input--auto-disable)
  (ewm-text-input-mode -1))

(defun ewm-text-input-intercept (enabled)
  "Enable or disable text input key interception."
  (ewm-text-input-intercept-module (if (eq enabled :false) nil enabled)))

(defun ewm--handle-key (event)
  "Handle key event from compositor.
Called when text-input-intercept is enabled and a printable key is pressed."
  (pcase-let (((map ("utf8" utf8)) event))
    (when utf8
      ;; Get input method from the focused surface buffer
      (let* ((surface-buf (ewm--focused-surface-buffer))
             (im (when surface-buf
                   (buffer-local-value 'current-input-method surface-buf)))
             (translated (ewm-text-input--translate-char (string-to-char utf8) im)))
        (ewm-im-commit translated)))))

(defun ewm--focused-surface-buffer ()
  "Return the buffer displaying the currently focused surface."
  (gethash (ewm-get-focused-id) ewm--surfaces))

(defun ewm-text-input--auto-enable ()
  "Enable text input mode when a client text field is activated."
  (ewm-text-input-intercept t)
  (ewm-text-input-mode 1))

(defun ewm-text-input--auto-disable ()
  "Disable text input mode when a client text field is deactivated."
  (ewm-text-input-intercept :false)
  (ewm-text-input-mode -1))

(provide 'ewm-text-input)
;;; ewm-text-input.el ends here
