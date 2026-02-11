;;; ewm-transient.el --- Transient interface for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; This library provides a transient interface for EWM (Emacs Wayland
;; Manager).  Use `M-x ewm-transient' or bind it to a key to access
;; the menu.

;;; Code:

(require 'transient)

;;;; Variables

(defvar ewm--process)
(defvar ewm--surfaces)
(defvar ewm--outputs)
(defvar ewm--xkb-current-layout)

;;;; Forward declarations

(declare-function ewm-connect "ewm")
(declare-function ewm-disconnect "ewm")
(declare-function ewm-load-module "ewm")
(declare-function ewm-input-line-mode "ewm")
(declare-function ewm-input-char-mode "ewm")
(declare-function ewm-input-toggle-mode "ewm")
(declare-function ewm-text-input-mode "ewm")
(declare-function ewm-text-input-auto-mode-enable "ewm")
(declare-function ewm-text-input-auto-mode-disable "ewm")
(declare-function ewm-screenshot "ewm")
(declare-function ewm-debug-layout "ewm")
(declare-function ewm-debug-surfaces "ewm")

;;;; Utilities

(defun ewm--connected-p ()
  "Return non-nil if connected to EWM compositor."
  (and (boundp 'ewm--process)
       ewm--process
       (process-live-p ewm--process)))

;;;; Info buffer

(defun ewm-info ()
  "Display detailed EWM status in a buffer."
  (interactive)
  (with-current-buffer (get-buffer-create "*ewm-info*")
    (let ((inhibit-read-only t))
      (erase-buffer)
      (ewm-info--insert-header)
      (ewm-info--insert-connection)
      (when (ewm--connected-p)
        (ewm-info--insert-outputs)
        (ewm-info--insert-surfaces)
        (ewm-info--insert-keyboard))
      (goto-char (point-min)))
    (special-mode)
    (pop-to-buffer (current-buffer))))

(defun ewm-info--insert-header ()
  "Insert header into info buffer."
  (insert (propertize "EWM Status\n" 'face 'bold))
  (insert (make-string 40 ?─) "\n\n"))

(defun ewm-info--insert-connection ()
  "Insert connection status into info buffer."
  (insert (propertize "Connection: " 'face 'bold))
  (insert (if (ewm--connected-p)
              (propertize "Connected\n" 'face 'success)
            (propertize "Disconnected\n" 'face 'error)))
  (insert "\n"))

(defun ewm-info--insert-outputs ()
  "Insert outputs list into info buffer."
  (insert (propertize "Outputs:\n" 'face 'bold))
  (if (and (boundp 'ewm--outputs) ewm--outputs)
      (dolist (output ewm--outputs)
        (insert (format "  %s at (%d, %d)\n"
                        (plist-get output :name)
                        (or (plist-get output :x) 0)
                        (or (plist-get output :y) 0))))
    (insert "  (none)\n"))
  (insert "\n"))

(defun ewm-info--insert-surfaces ()
  "Insert surfaces list into info buffer."
  (insert (propertize "Surfaces:\n" 'face 'bold))
  (if (and (boundp 'ewm--surfaces)
           (> (hash-table-count ewm--surfaces) 0))
      (maphash (lambda (id info)
                 (insert (format "  %d: %s%s\n"
                                 id
                                 (or (plist-get info :app) "?")
                                 (if-let ((title (plist-get info :title)))
                                     (format " - %s" title)
                                   ""))))
               ewm--surfaces)
    (insert "  (none)\n"))
  (insert "\n"))

(defun ewm-info--insert-keyboard ()
  "Insert keyboard layout into info buffer."
  (insert (propertize "Keyboard: " 'face 'bold))
  (insert (or (and (boundp 'ewm--xkb-current-layout)
                   ewm--xkb-current-layout)
              "-"))
  (insert "\n"))

;;;; Transient

(defun ewm-transient--status ()
  "Return connection status for transient display."
  (if (ewm--connected-p)
      (propertize "Connected" 'face 'success)
    (propertize "Disconnected" 'face 'error)))

;;;###autoload (autoload 'ewm-transient "ewm-transient" nil t)
(transient-define-prefix ewm-transient ()
  "Transient menu for EWM."
  [["Status"
    (:info #'ewm-transient--status)
    ("?" "Details" ewm-info)]
   ["Connection"
    ("c" "Connect" ewm-connect)
    ("d" "Disconnect" ewm-disconnect)
    ("m" "Load module" ewm-load-module)]
   ["Actions"
    ("s" "Screenshot" ewm-screenshot)
    ("D l" "Debug layout" ewm-debug-layout)
    ("D s" "Debug surfaces" ewm-debug-surfaces)]
   ["Input"
    ("l" "Line mode" ewm-input-line-mode)
    ("k" "Char mode" ewm-input-char-mode)
    ("t" "Toggle" ewm-input-toggle-mode)]
   ["Text Input"
    ("i" "Toggle" ewm-text-input-mode)
    ("a" "Auto on" ewm-text-input-auto-mode-enable)
    ("A" "Auto off" ewm-text-input-auto-mode-disable)]])

(provide 'ewm-transient)
;;; ewm-transient.el ends here
