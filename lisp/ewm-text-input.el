;;; ewm-text-input.el --- Text input support for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Text input protocol support for EWM.
;; Handles input method integration for Wayland clients.

;;; Code:

(declare-function ewm-im-commit-module "ewm-core")
(declare-function ewm-text-input-forward-key-module "ewm-core")
(declare-function ewm-text-input-intercept-module "ewm-core")
(declare-function ewm-get-focused-id "ewm-core")

(defvar ewm--surfaces)

(defvar ewm-text-input-activated-hook nil
  "Hook run when a client text field becomes active.
Use this to enable special input handling modes.")

(defvar ewm-text-input-deactivated-hook nil
  "Hook run when a client text field becomes inactive.")

(defvar ewm-text-input--client-active nil
  "Non-nil while a Wayland client text field is active.")

(defun ewm--handle-text-input-activated ()
  "Handle text-input-activated event from compositor.
Called when a client's text field gains focus."
  (run-hooks 'ewm-text-input-activated-hook))

(defun ewm--handle-text-input-deactivated ()
  "Handle text-input-deactivated event from compositor.
Called when a client's text field loses focus."
  (run-hooks 'ewm-text-input-deactivated-hook))

(defun ewm-im-commit (text surface-id)
  "Commit TEXT to the client text field on SURFACE-ID."
  (ewm-im-commit-module text surface-id))

(defun ewm-text-input--forward-key (event)
  "Hand EVENT's key back to the compositor."
  (let ((surface-id (alist-get 'surface_id event))
        (keycode (alist-get 'keycode event)))
    (when (and (numberp surface-id) (numberp keycode))
      (ewm-text-input-forward-key-module
       surface-id keycode
       (alist-get 'ctrl event) (alist-get 'alt event)
       (alist-get 'shift event) (alist-get 'logo event)))))

(defun ewm-text-input--translate-char (char &optional input-method)
  "Translate CHAR through INPUT-METHOD if provided.
If INPUT-METHOD is nil, uses the current buffer's `current-input-method'.
For quail-based input methods, looks up the translation directly."
  (let ((im (or input-method current-input-method)))
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

(define-minor-mode ewm-text-input-mode
  "Mode-line indicator for active Wayland text-input relay."
  :global t
  :lighter " IM")

(defun ewm-text-input--active-input-method ()
  "Return the input method of the focused text-input surface, if any."
  (when-let* ((focused-id (ewm-get-focused-id))
              ((numberp focused-id))
              (surface-buf (gethash focused-id ewm--surfaces))
              ((buffer-live-p surface-buf)))
    (buffer-local-value 'current-input-method surface-buf)))

(defun ewm-text-input--refresh-auto-state ()
  "Intercept keys only while a client field is active and an IM is set."
  (let ((enabled (and ewm-text-input--client-active
                      (ewm-text-input--active-input-method))))
    (ewm-text-input-intercept (if enabled t :false))
    (ewm-text-input-mode (if enabled 1 -1))))

(defun ewm-text-input-auto-mode-enable ()
  "Enable automatic text input mode switching.
Text input mode will be enabled/disabled automatically when
client text fields gain/lose focus."
  (interactive)
  (add-hook 'ewm-text-input-activated-hook #'ewm-text-input--auto-enable)
  (add-hook 'ewm-text-input-deactivated-hook #'ewm-text-input--auto-disable)
  (add-hook 'input-method-activate-hook #'ewm-text-input--refresh-auto-state)
  (add-hook 'input-method-deactivate-hook #'ewm-text-input--refresh-auto-state))

(defun ewm-text-input-auto-mode-disable ()
  "Disable automatic text input mode switching."
  (interactive)
  (remove-hook 'ewm-text-input-activated-hook #'ewm-text-input--auto-enable)
  (remove-hook 'ewm-text-input-deactivated-hook #'ewm-text-input--auto-disable)
  (remove-hook 'input-method-activate-hook #'ewm-text-input--refresh-auto-state)
  (remove-hook 'input-method-deactivate-hook #'ewm-text-input--refresh-auto-state)
  (setq ewm-text-input--client-active nil)
  (ewm-text-input-intercept :false)
  (ewm-text-input-mode -1))

(defun ewm-text-input-intercept (enabled)
  "Enable or disable text input key interception."
  (ewm-text-input-intercept-module (if (eq enabled :false) nil enabled)))

(defun ewm--handle-key (event)
  "Handle key EVENT from the compositor during text-input interception.
Commit the IM translation to the client, else forward the key unchanged."
  (let* ((surface-id (alist-get 'surface_id event))
         (utf8 (alist-get 'utf8 event))
         (shortcut (or (alist-get 'ctrl event)
                       (alist-get 'alt event)
                       (alist-get 'logo event)))
         (surface-buf (and (numberp surface-id) (> surface-id 0)
                           (gethash surface-id ewm--surfaces))))
    (if (and utf8 (not shortcut) (buffer-live-p surface-buf))
        (with-current-buffer surface-buf
          (ewm-im-commit (ewm-text-input--translate-char (string-to-char utf8))
                         surface-id))
      (ewm-text-input--forward-key event))))

(defun ewm-text-input--auto-enable ()
  "Enable text input mode when a client text field is activated."
  (setq ewm-text-input--client-active t)
  (ewm-text-input--refresh-auto-state))

(defun ewm-text-input--auto-disable ()
  "Disable text input mode when a client text field is deactivated."
  (setq ewm-text-input--client-active nil)
  (ewm-text-input--refresh-auto-state))

(provide 'ewm-text-input)
;;; ewm-text-input.el ends here
