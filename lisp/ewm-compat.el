;;; ewm-compat.el --- Obsolete EWM options kept working -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; The options `ewm-mode-map' replaced. When EWM starts, and whenever one
;; of them is set, each is turned into the bindings that replaced it and
;; the lines that do the same from an init file are printed once.

;;; Code:

(require 'ewm-input)

(defcustom ewm-intercept-prefixes nil
  "Keys that reach Emacs while a surface has focus.
Each is a key or (key :fullscreen). Bind the key in `ewm-mode-map' with
(ewm-key :intercept) instead."
  :type '(repeat (choice ewm-key (list ewm-key (const :fullscreen))))
  :set (ewm--send-on-set #'ewm-compat--apply)
  :group 'ewm-keybindings)
(make-obsolete-variable 'ewm-intercept-prefixes "(ewm-key :intercept) in `ewm-mode-map'." "2026-10")

(defcustom ewm-repeatable-commands nil
  "Commands whose intercepted key bindings repeat while held.
Bind the key in `ewm-mode-map' with (ewm-key COMMAND :repeat) instead."
  :type '(repeat function)
  :set (ewm--send-on-set #'ewm-compat--apply)
  :group 'ewm-keybindings)
(make-obsolete-variable 'ewm-repeatable-commands "(ewm-key COMMAND :repeat) in `ewm-mode-map'." "2026-10")

(defun ewm-compat--parse-intercept-entry (entry)
  "Parse ENTRY from `ewm-intercept-prefixes' into (event . fullscreen-p), or nil."
  (let* ((fullscreen (and (listp entry) (memq :fullscreen entry)))
         (event (ewm--key-event (if fullscreen (car entry) entry))))
    (and event (cons event (and fullscreen t)))))

(defun ewm-compat--flag (event command flag)
  "Give EVENT's binding FLAG, binding COMMAND when it has none, return EVENT."
  (pcase-let ((`(,bound . ,flags) (ewm--event-entry event)))
    (unless (memq flag flags)
      (let ((command (or command bound))
            (flags (append flags (list flag))))
        (keymap-set ewm-mode-map (key-description (vector event))
                    (apply #'ewm-key (if command (cons command flags) flags))))
      event)))

(defun ewm-compat--line (event)
  "The `keymap-set' line that makes EVENT's binding in `ewm-mode-map'."
  (pcase-let ((`(,command . ,flags) (ewm--event-entry event)))
    (format "(keymap-set ewm-mode-map %S (ewm-key %s))"
            (key-description (vector event))
            (mapconcat (lambda (x) (format "%s" x))
                       (if command (cons (format "#'%s" command) flags) flags) " "))))

(defun ewm-compat--apply ()
  "Bind in `ewm-mode-map' what the obsolete variables ask for, and say so once."
  (let (events)
    (pcase-dolist (`(,event . ,fullscreen)
                   (seq-keep #'ewm-compat--parse-intercept-entry ewm-intercept-prefixes))
      (let ((command (car (ewm--event-entry event))))
        (unless (keymapp command)
          (unless command (push (ewm-compat--flag event nil :intercept) events))
          (when fullscreen (push (ewm-compat--flag event command :fullscreen) events)))))
    (dolist (command ewm-repeatable-commands)
      (dolist (key (where-is-internal command (list ewm-mode-map)))
        (when (length= key 1)
          (push (ewm-compat--flag (aref key 0) command :repeat) events))))
    (when-let* ((events (seq-uniq (delq nil (nreverse events)))))
      (display-warning
       'ewm (concat "ewm-intercept-prefixes and ewm-repeatable-commands are obsolete, "
                    "the same bindings in ewm-mode-map:\n  "
                    (mapconcat #'ewm-compat--line events "\n  "))))))

(provide 'ewm-compat)
;;; ewm-compat.el ends here
