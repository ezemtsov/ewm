;;; ewm-text-input.el --- Text input support for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Text input protocol support for EWM.
;; Handles input method integration for Wayland clients.

;;; Code:

(require 'ewm-module)
(ewm-module--declare)
(require 'ewm-surface)

(defcustom ewm-text-input-provider 'emacs
  "Who serves text input for Wayland clients.
`emacs' routes client text fields through Emacs input methods.
`external' leaves the seat to an input method such as fcitx5 or
ibus, which then serves Emacs frames as well."
  :type '(choice (const :tag "Emacs input methods" emacs)
                 (const :tag "External input method (fcitx5, ibus)" external))
  :set (ewm--send-on-set #'ewm-text-input--send-provider)
  :group 'ewm-input-methods)

(defun ewm-text-input--send-provider ()
  "Tell the compositor who serves client text input."
  (ewm-configure-text-input-module ewm-text-input-provider))

(defvar ewm-text-input--composing nil
  "Surface id whose input method is reading keys, else nil.")

(defvar ewm-text-input--raw nil
  "Compositor keys queued for the input method, oldest first.")

(defvar ewm-text-input--preedit ""
  "Text last shown as preedit for the composing field.")

(defvar ewm-text-input-activated-hook nil
  "Hook run when a client text field becomes active.
Use this to enable special input handling modes.")

(defvar ewm-text-input-deactivated-hook nil
  "Hook run when a client text field becomes inactive.")

(defvar ewm-text-input--client-active nil
  "Non-nil while a Wayland client text field is active.")

(defun ewm--handle-text-input-activated ()
  "Note that a client text field gained focus and run the hook."
  (setq ewm-text-input--client-active t)
  (run-hooks 'ewm-text-input-activated-hook))

(defun ewm--handle-text-input-deactivated ()
  "Note that the client text field lost focus, aborting any composition."
  (setq ewm-text-input--client-active nil)
  (when ewm-text-input--composing
    ;; Stop the input method. Its text dies with the field.
    (setq ewm-text-input--composing nil)
    (setq unread-command-events
          (nconc unread-command-events (list '(no-record . ewm-text-input-abort)))))
  (run-hooks 'ewm-text-input-deactivated-hook))

(defun ewm-im-commit (text surface-id)
  "Commit TEXT to the client text field on SURFACE-ID."
  (ewm-im-commit-module text surface-id))

(defun ewm-im-preedit (text surface-id)
  "Show TEXT as the composing text in the client field on SURFACE-ID."
  (ewm-im-preedit-module text surface-id))

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

(define-minor-mode ewm-text-input-mode
  "Mode-line indicator for active Wayland text-input relay."
  :global t
  :group 'ewm-input-methods
  :lighter " IM")

(defun ewm-text-input--active-input-method ()
  "Return the input method of the focused text-input surface, if any."
  (when-let* ((buf (ewm--surface-buffer (ewm-get-focused-id))))
    (buffer-local-value 'current-input-method buf)))

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
  :group 'ewm-input-methods
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

;;; Composition

;; A text key runs the surface buffer's input method in the command loop,
;; exwm-xim style.

(defconst ewm-text-input--keysym-events
  '((#xff08 . backspace) (#xff09 . tab) (#xff0d . return) (#xff1b . escape)
    (#xffff . delete) (#xff50 . home) (#xff51 . left) (#xff52 . up)
    (#xff53 . right) (#xff54 . down) (#xff55 . prior) (#xff56 . next)
    (#xff57 . end) (#xff63 . insert) (#xff8d . kp-enter) (#xfe20 . iso-lefttab))
  "Emacs events for the non-text keysyms an input method may want.")

(defun ewm-text-input--event (key)
  "Return the Emacs event for compositor KEY, or nil for keys Emacs cannot name."
  (let* ((utf8 (alist-get 'utf8 key))
         (base (if utf8
                   (string-to-char utf8)
                 (alist-get (alist-get 'keysym key) ewm-text-input--keysym-events)))
         (mods (append (and (alist-get 'ctrl key) '(control))
                       (and (alist-get 'alt key) '(meta))
                       (and (alist-get 'logo key) '(super))
                       (and (symbolp base) (alist-get 'shift key) '(shift)))))
    (and base (event-convert-list (append mods (list base))))))

(defun ewm-text-input--text-p (event)
  "Non-nil for an EVENT an input method reads as text."
  (and (characterp event) (>= event ?\s) (/= event 127)))

(defun ewm-text-input--pending-p ()
  "Non-nil when a queued text key has yet to start its input method."
  (seq-some (lambda (event) (eq (car-safe event) 'ewm-text-input-key))
            unread-command-events))

(defun ewm--handle-key (event)
  "Queue compositor key EVENT for the surface's input method, else forward it."
  (let* ((surface-id (alist-get 'surface_id event))
         (key (ewm-text-input--event event))
         (surface-buf (ewm--surface-buffer surface-id)))
    (cond
     ;; Keys may arrive before the command loop starts the first composition.
     ((or ewm-text-input--composing (ewm-text-input--pending-p))
      (when key
        (setq ewm-text-input--raw (nconc ewm-text-input--raw (list event)))
        (setq unread-command-events
              (nconc unread-command-events (list (cons 'no-record key))))))
     ((and (ewm-text-input--text-p key) surface-buf)
      (setq unread-command-events
            (nconc unread-command-events
                   (list (list 'ewm-text-input-key surface-id key)))))
     (t (ewm-text-input--forward-key event)))))

(defun ewm-text-input--preedit-sync (_window)
  "Before a redisplay, show the client what the input method composed so far."
  (when-let* ((surface-id ewm-text-input--composing)
              (buf (ewm--surface-buffer surface-id)))
    (let ((text (with-current-buffer buf
                  (buffer-substring-no-properties (point-min) (point-max)))))
      (unless (equal text ewm-text-input--preedit)
        (setq ewm-text-input--preedit text)
        (ewm-im-preedit text surface-id)))))

(defun ewm-text-input--input (surface-id key)
  "Give KEY to SURFACE-ID's input method and commit what it makes of it."
  (when-let* ((buf (ewm--surface-buffer surface-id)))
    (with-current-buffer buf
      (let ((ewm-text-input--composing surface-id)
            (ewm-text-input--preedit "")
            ;; The method's scratch text is preedit, not an insertion to commit.
            (after-change-functions nil)
            ;; Quail may compose under with-silent-modifications, so after-change misses it.
            (pre-redisplay-functions
             (cons #'ewm-text-input--preedit-sync pre-redisplay-functions))
            (text ""))
        ;; A prior composition cut short could have left scratch text.
        (erase-buffer)
        (unwind-protect
            (let ((result (funcall (or input-method-function #'list) key)))
              ;; Nil once the field deactivated under the input method.
              (when ewm-text-input--composing
                (setq text (concat (seq-filter #'characterp result)))))
          ;; A commit replaces the preedit.
          (if (string-empty-p text)
              (unless (string-empty-p ewm-text-input--preedit)
                (ewm-im-preedit "" surface-id))
            (ewm-im-commit text surface-id)))))))

(defun ewm-text-input--raw-key (event)
  "Pop the queued compositor keys up to EVENT's and return that key, or nil."
  (while (and ewm-text-input--raw
              (not (eql (ewm-text-input--event (car ewm-text-input--raw)) event)))
    (pop ewm-text-input--raw))
  (pop ewm-text-input--raw))

(defun ewm-text-input--drain (surface-id)
  "Type the keys the input method handed back into SURFACE-ID's client."
  (while (eq (car-safe (car unread-command-events)) 'no-record)
    (let ((ev (cdr (pop unread-command-events))))
      ;; Abort and C-g are the method's own, with no pressed key to forward.
      (cond ((memq ev '(ewm-text-input-abort ?\C-g)))
            ((ewm-text-input--text-p ev) (ewm-text-input--input surface-id ev))
            (t (when-let* ((raw (ewm-text-input--raw-key ev)))
                 (ewm-text-input--forward-key raw))))))
  (setq ewm-text-input--raw nil))

(defun ewm-text-input--compose ()
  "Run the input method for `last-input-event', then type back what it hands over."
  (interactive)
  (pcase-let ((`(,_ ,surface-id ,key) last-input-event))
    (ewm-text-input--input surface-id key)
    (ewm-text-input--drain surface-id)))

(define-key special-event-map [ewm-text-input-key] #'ewm-text-input--compose)

;;; Edit a field's content in an Emacs buffer.

;; Only for clients that report surrounding text, bounded by its 4000-byte window.

(defvar-local ewm-edit--surface-id nil
  "Surface id of the field this `ewm-edit' buffer replaces.")

;; Survive a major-mode change in the edit buffer.
(put 'ewm-edit--surface-id 'permanent-local t)

(defun ewm-edit--buffer-name (surface-id)
  "Buffer name for editing the field on SURFACE-ID."
  (let ((client (ewm--surface-buffer surface-id)))
    (format "*ewm-edit %s*" (if client (buffer-name client) surface-id))))

(defvar ewm-edit-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "C-c C-c") #'ewm-edit-finish)
    (define-key map (kbd "C-c C-k") #'ewm-edit-cancel)
    map)
  "Keymap for `ewm-edit-mode'.")

(define-minor-mode ewm-edit-mode
  "Minor mode for buffers editing a Wayland field via `ewm-edit'."
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

(defun ewm-edit--take-pending (event)
  "Pop the live edit buffer awaiting EVENT's surface, else nil."
  (let* ((surface-id (alist-get 'surface_id event))
         (buffer (gethash surface-id ewm-edit--pending)))
    (remhash surface-id ewm-edit--pending)
    (and (buffer-live-p buffer) buffer)))

(defun ewm--handle-text-input-replaced (event)
  "Drop the edit buffer whose replace the compositor delivered."
  (when-let* ((buffer (ewm-edit--take-pending event)))
    (kill-buffer buffer)))

(defun ewm--handle-text-input-replace-dropped (event)
  "Re-surface the edit buffer whose replace the compositor dropped."
  (when-let* ((buffer (ewm-edit--take-pending event)))
    (with-current-buffer buffer
      (setq header-line-format
            (substitute-command-keys
             "Replace not applied; edit kept. \\[ewm-edit-finish] retries, \\[ewm-edit-cancel] discards")))
    (pop-to-buffer buffer)))

(provide 'ewm-text-input)
;;; ewm-text-input.el ends here
