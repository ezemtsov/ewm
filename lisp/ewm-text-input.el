;;; ewm-text-input.el --- Text input support for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Text input protocol support for EWM.
;; Handles input method integration for Wayland clients.

;;; Code:

(declare-function ewm-im-commit-module "ewm-core")
(declare-function ewm-text-input-replace-module "ewm-core")
(declare-function ewm-text-input-surrounding-module "ewm-core")
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
  (setq ewm-text-input--client-active t)
  (run-hooks 'ewm-text-input-activated-hook))

(defun ewm--handle-text-input-deactivated ()
  "Handle text-input-deactivated event from compositor.
Called when a client's text field loses focus."
  (setq ewm-text-input--client-active nil)
  (run-hooks 'ewm-text-input-deactivated-hook))

(defun ewm-im-commit (text surface-id)
  "Commit TEXT to the client text field on SURFACE-ID."
  (ewm-im-commit-module text surface-id))

(defun ewm-text-input-replace (text surface-id)
  "Replace the whole content of the field on SURFACE-ID with TEXT."
  (ewm-text-input-replace-module text surface-id))

(defun ewm-text-input-surrounding ()
  "Return the active field's surrounding text, or nil.
Value is (SURFACE-ID TEXT CURSOR ANCHOR); CURSOR/ANCHOR are byte offsets."
  (ewm-text-input-surrounding-module))

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

(defvar ewm-text-input-auto-mode)

(defun ewm-text-input--refresh-auto-state ()
  "Intercept keys only in auto mode with an active field and input method."
  (let ((enabled (and ewm-text-input-auto-mode
                      ewm-text-input--client-active
                      (ewm-text-input--active-input-method))))
    (ewm-text-input-intercept (and enabled t))
    (ewm-text-input-mode (if enabled 1 -1))))

(defun ewm-text-input--input-method-deactivated ()
  "Refresh interception after the current buffer's input method turns off."
  ;; Emacs runs the deactivate hook before clearing this variable.
  (let ((current-input-method nil))
    (ewm-text-input--refresh-auto-state)))

(define-minor-mode ewm-text-input-auto-mode
  "Automatically relay input methods to active Wayland text fields."
  :global t
  :group 'ewm
  (let ((hook-function (if ewm-text-input-auto-mode #'add-hook #'remove-hook)))
    (dolist (hook '(ewm-text-input-activated-hook ewm-text-input-deactivated-hook
                   input-method-activate-hook))
      (funcall hook-function hook #'ewm-text-input--refresh-auto-state))
    (funcall hook-function 'input-method-deactivate-hook
             #'ewm-text-input--input-method-deactivated))
  (ewm-text-input--refresh-auto-state))

(defun ewm-text-input-intercept (enabled)
  "Enable or disable text input key interception."
  (ewm-text-input-intercept-module enabled))

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

;;; Edit a field's content in an Emacs buffer.

;; Only for clients that report surrounding text, bounded by its 4000-byte window.

(defvar-local ewm-edit--surface-id nil
  "Surface id of the field this `ewm-edit' buffer replaces.")

;; Survive a major-mode change in the edit buffer.
(put 'ewm-edit--surface-id 'permanent-local t)

(defun ewm-edit--buffer-name (surface-id)
  "Buffer name for editing the field on SURFACE-ID."
  (let ((client (gethash surface-id ewm--surfaces)))
    (format "*ewm-edit %s*"
            (if (buffer-live-p client) (buffer-name client) surface-id))))

(defvar ewm-edit-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "C-c C-c") #'ewm-edit-finish)
    (define-key map (kbd "C-c C-k") #'ewm-edit-cancel)
    map)
  "Keymap for `ewm-edit-mode'.")

(define-minor-mode ewm-edit-mode
  "Minor mode for buffers editing a Wayland field via `ewm-edit'."
  :init-value nil
  :lighter " ewm-edit")
(put 'ewm-edit-mode 'permanent-local t)

(defvar ewm-edit--pending (make-hash-table :test 'eql)
  "Surface id -> edit buffer awaiting a replace ack from the compositor.")

(defun ewm-edit ()
  "Edit the focused Wayland text field's content in an Emacs buffer."
  (interactive)
  (let* ((surrounding (or (ewm-text-input-surrounding)
                          (user-error "Editing unsupported")))
         (surface-id (nth 0 surrounding))
         (text (nth 1 surrounding))
         (cursor (nth 2 surrounding))
         (buffer (get-buffer-create (ewm-edit--buffer-name surface-id))))
    ;; A late ack for a prior replace must not kill this fresh buffer.
    (remhash surface-id ewm-edit--pending)
    (with-current-buffer buffer
      (erase-buffer)
      (insert text)
      (goto-char (or (ignore-errors (byte-to-position (1+ cursor))) (point-max)))
      (ewm-edit-mode 1)
      (setq ewm-edit--surface-id surface-id)
      (setq header-line-format
            (substitute-command-keys
             "Edit, then \\[ewm-edit-finish] to replace or \\[ewm-edit-cancel] to cancel")))
    (pop-to-buffer buffer)))

(defun ewm-edit-finish ()
  "Send this buffer's contents to replace the originating field."
  (interactive)
  (unless ewm-edit-mode
    (user-error "Not in an ewm-edit buffer"))
  (let ((text (buffer-string))
        (surface-id ewm-edit--surface-id))
    (puthash surface-id (current-buffer) ewm-edit--pending)
    (setq header-line-format "Replacing…")
    ;; Return to the field, keeping the edit buffer until the replace is acked.
    (quit-windows-on (current-buffer))
    (ewm-text-input-replace text surface-id)))

(defun ewm-edit-cancel ()
  "Abandon the edit, leaving the field untouched."
  (interactive)
  (unless ewm-edit-mode
    (user-error "Not in an ewm-edit buffer"))
  (when ewm-edit--surface-id
    (remhash ewm-edit--surface-id ewm-edit--pending))
  (quit-windows-on (current-buffer) t))

(defun ewm--handle-text-input-replaced (event)
  "Drop the edit buffer whose replace the compositor delivered."
  (let* ((surface-id (alist-get 'surface_id event))
         (buffer (gethash surface-id ewm-edit--pending)))
    (remhash surface-id ewm-edit--pending)
    (when (buffer-live-p buffer)
      (kill-buffer buffer))))

(defun ewm--handle-text-input-replace-dropped (event)
  "Re-surface the edit buffer whose replace the compositor dropped."
  (let* ((surface-id (alist-get 'surface_id event))
         (buffer (gethash surface-id ewm-edit--pending)))
    (remhash surface-id ewm-edit--pending)
    (when (buffer-live-p buffer)
      (with-current-buffer buffer
        (setq header-line-format
              (substitute-command-keys
               "Replace not applied; edit kept. \\[ewm-edit-finish] retries, \\[ewm-edit-cancel] discards")))
      (pop-to-buffer buffer))))

(provide 'ewm-text-input)
;;; ewm-text-input.el ends here
