;;; ewm-transient.el --- Transient interface for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Transient menu for EWM (Emacs Wayland Manager).
;;
;; Use `M-x ewm-transient' to access the control panel.
;; Bind it to a key like `s-c' for quick access:
;;
;;   (global-set-key (kbd "s-c") #'ewm-transient)

;;; Code:

(require 'transient)

;;;; Forward declarations

(declare-function ewm-start-module "ewm")
(declare-function ewm-stop-module "ewm")
(declare-function ewm-show-state "ewm")
(declare-function ewm-debug-mode "ewm")
(declare-function ewm-screenshot "ewm")
(declare-function ewm-running "ewm-core")

(defvar ewm--module-mode)
(defvar ewm--surfaces)

;;;; Status functions

(defun ewm-transient--running-p ()
  "Return non-nil if EWM compositor is running."
  (and (boundp 'ewm--module-mode)
       ewm--module-mode
       (fboundp 'ewm-running)
       (ewm-running)))

(defun ewm-transient--status ()
  "Return status string for transient header."
  (if (ewm-transient--running-p)
      (propertize "Running" 'face 'success)
    (propertize "Stopped" 'face 'shadow)))

(defun ewm-transient--surface-count ()
  "Return number of managed surfaces."
  (if (and (boundp 'ewm--surfaces) (hash-table-p ewm--surfaces))
      (hash-table-count ewm--surfaces)
    0))

;;;; Transient

;;;###autoload
(transient-define-prefix ewm-transient ()
  "Control panel for EWM (Emacs Wayland Manager)."
  [:description
   (lambda ()
     (format "EWM: %s  |  Surfaces: %d"
             (ewm-transient--status)
             (ewm-transient--surface-count)))]
  [["Compositor"
    ("s" "Start" ewm-start-module :inapt-if ewm-transient--running-p)
    ("q" "Stop" ewm-stop-module :inapt-if-not ewm-transient--running-p)
    ("r" "Restart" (lambda () (interactive)
                     (when (ewm-transient--running-p) (ewm-stop-module))
                     (sit-for 0.5)
                     (ewm-start-module)))]
   ["Actions"
    ("S" "Screenshot" ewm-screenshot)
    ("i" "Inspect state" ewm-show-state)
    ("d" "Debug mode" ewm-debug-mode)]])

(provide 'ewm-transient)
;;; ewm-transient.el ends here
