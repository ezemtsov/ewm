;;; ewm-test-stubs.el --- Module stubs for the ERT suites -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; The compositor dynamic module isn't loaded in the test harness. Every
;; module function answers nil until a suite redefines it, and lisp/ goes
;; on the load path.

;;; Code:

;; A stale .elc next to a newer .el would otherwise mask the source we're
;; meant to be testing.
(setq load-prefer-newer t)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
(require 'ewm-module)
(dolist (form (cdr (macroexpand '(ewm-module--declare))))
  (defalias (cadr form) #'ignore))
;; Frames sit where they were made for.
(defun ewm-get-frame-output (surface-id)
  (when-let* ((frame (ewm--frame-by-surface-id surface-id)))
    (frame-parameter frame 'ewm-output)))

(defvar ewm-test--queued nil "Events the stubbed `ewm-drain-events' hands out.")
(defun ewm-drain-events () (prog1 ewm-test--queued (setq ewm-test--queued nil)))

(defun ewm-test--drain (&rest events)
  "Run the event filter over EVENTS, as one compositor wakeup would."
  (setq ewm-test--queued events)
  (ewm--event-filter nil "\0"))
(provide 'ewm-core)

(provide 'ewm-test-stubs)
;;; ewm-test-stubs.el ends here
