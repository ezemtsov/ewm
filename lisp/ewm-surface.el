;;; ewm-surface.el --- Surface buffer management for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Surface buffer management for EWM.
;; Provides the major mode for surface buffers and buffer lifecycle management,
;; and holds the session state every other file reads.

;;; Code:

(require 'cl-lib)
(require 'ewm-module)
(ewm-module--declare)

;;; Customization groups

(defgroup ewm nil
  "Support for Wayland apps as Emacs buffers."
  :tag "EWM"
  :link '(emacs-library-link :tag "Library Source" "ewm.el")
  :group 'environment
  :prefix "ewm-")

(defgroup ewm-keybindings nil
  "Key interception, prefix keys and per-app keys."
  :link '(url-link "https://codeberg.org/ezemtsov/ewm/wiki/Keybindings")
  :group 'ewm
  :prefix "ewm-")

(defgroup ewm-input-methods nil
  "Text input in Wayland clients."
  :link '(url-link "https://codeberg.org/ezemtsov/ewm/wiki/Input-Methods")
  :group 'ewm
  :prefix "ewm-")

(defgroup ewm-input-devices nil
  "Device settings, cursor hiding and pointer focus."
  :link '(url-link "https://codeberg.org/ezemtsov/ewm/wiki/Input-Devices")
  :group 'ewm
  :prefix "ewm-")

(defgroup ewm-output nil
  "Display resolution, scaling, positioning and idle."
  :link '(url-link "https://codeberg.org/ezemtsov/ewm/wiki/Output")
  :group 'ewm
  :prefix "ewm-")

(defgroup ewm-appearance nil
  "Dimming, animations, overview, blur and cursor theme."
  :link '(url-link "https://codeberg.org/ezemtsov/ewm/wiki/Appearance")
  :group 'ewm
  :prefix "ewm-")

;;; Session state

(defvar ewm--module-mode nil
  "Non-nil when running in module mode (compositor in-process).")

(defun ewm--send-on-set (send)
  "A setter storing the value, then calling SEND once the compositor runs."
  (lambda (sym val)
    (set-default sym val)
    (when ewm--module-mode (funcall send))))

(defvar ewm--surfaces (make-hash-table :test 'eql)
  "Hash table mapping surface ID to buffer.")

(defvar ewm--debug nil
  "Non-nil when EWM debug mode is active.")

(defun ewm--log (format-string &rest args)
  "Log FORMAT-STRING formatted with ARGS to *ewm-log* in debug mode."
  (when ewm--debug
    (let ((buf (get-buffer-create "*ewm-log*")))
      (with-current-buffer buf
        (goto-char (point-max))
        (insert (format-time-string "[%H:%M:%S.%3N] ")
                (apply #'format format-string args) "\n")))))

;;; Surface buffers

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

(defvar-local ewm-surface-size nil
  "Client's natural size (W . H) in logical pixels at map, or nil.")

(defun ewm-buffer-name (app title _id)
  "Name the surface after TITLE, or APP while it has none."
  (format "*ewm:%s*" (if (member title '(nil "")) app title)))

(defcustom ewm-buffer-name-function #'ewm-buffer-name
  "Function returning a surface buffer name from app id, title and surface id.
Called with a nil title at creation and again on every app id or title change.
The current buffer is unspecified."
  :type 'function
  :group 'ewm)

(defun ewm-surface-buffer-p (buffer)
  "If BUFFER is an EWM surface buffer, return its surface id, else nil."
  (and buffer (buffer-local-value 'ewm-surface-id buffer)))

;;;###autoload
(cl-defun ewm-surface-match (&key app title)
  "Return a `display-buffer-alist' condition matching APP and TITLE regexps.
A nil regexp matches anything; matching is case-sensitive."
  (lambda (buffer &optional _action)
    (with-current-buffer buffer
      (let ((case-fold-search nil))
        (and ewm-surface-id
             (or (null app) (string-match-p app (or ewm-surface-app "")))
             (or (null title) (string-match-p title (or ewm-surface-title ""))))))))

(defun ewm--surface-buffer (id)
  "Live buffer of surface ID, else nil."
  (when-let* ((buf (gethash id ewm--surfaces)) ((buffer-live-p buf))) buf))

(defmacro ewm--with-surface-buffer-from-event (event &rest body)
  "Run BODY in the surface buffer for EVENT's `id', when live; else no-op.
Absorbs the race where the surface vanishes between the compositor
queueing the event and Emacs handling it."
  (declare (indent 1) (debug t))
  `(when-let* ((buf (ewm--surface-buffer (alist-get 'id ,event))))
     (with-current-buffer buf ,@body)))

(defun ewm-close (id)
  "Request surface ID to close gracefully."
  (ewm-close-module id))

(defun ewm--kill-buffer-query-function ()
  "Ask the compositor to close a live surface instead of killing its buffer."
  (if (and ewm-surface-id ewm--module-mode (gethash ewm-surface-id ewm--surfaces))
      (progn (ewm-close ewm-surface-id) nil)
    t))

(defun ewm-surface--after-change (beg end old-len)
  "Intercept insertions and route through im-commit.
Pure insertions are captured, removed from the buffer, and
forwarded to the client text field via `ewm-im-commit-module'."
  (when (and (> end beg) (zerop old-len))
    (let ((text (buffer-substring-no-properties beg end))
          (inhibit-modification-hooks t)
          (buffer-undo-list t))
      (delete-region beg end)
      (ewm-im-commit-module text ewm-surface-id))))

(define-derived-mode ewm-surface-mode fundamental-mode "Surface"
  "Major mode for EWM surface buffers."
  ;; Surface buffers are visual proxies, not editable text content.
  (buffer-disable-undo)
  (add-hook 'after-change-functions #'ewm-surface--after-change nil t)
  (setq-local cursor-type nil)
  (setq-local left-fringe-width 0)
  (setq-local right-fringe-width 0)
  (setq-local show-trailing-whitespace nil)
  (add-hook 'kill-buffer-query-functions
            #'ewm--kill-buffer-query-function nil t))

(provide 'ewm-surface)
;;; ewm-surface.el ends here
