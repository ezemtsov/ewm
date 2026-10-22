;;; ewm-test-stubs.el --- Module stubs for the input ERT suites -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; The compositor dynamic module isn't loaded in the test harness. This
;; stubs the module entry points the input libraries pull in and puts
;; lisp/ on the load path, for the suites that load through `ewm-input'.

;;; Code:

;; A stale .elc next to a newer .el would otherwise mask the source we're
;; meant to be testing.
(setq load-prefer-newer t)

(defmacro ewm-test--stub (name args &rest body)
  `(unless (fboundp ',name) (defun ,name ,args ,@body)))

(ewm-test--stub ewm-intercept-keys-module (_keys) nil)
(ewm-test--stub ewm-keyboard-capture-begin-module (_reason) 1)
(ewm-test--stub ewm-keyboard-capture-end-module (_token) t)
(ewm-test--stub ewm-keyboard-redirect-capture-clear-module () nil)
(ewm-test--stub ewm-get-focused-id () 2)
(ewm-test--stub ewm-get-focused-frame-id () 2)
(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))

(provide 'ewm-test-stubs)
;;; ewm-test-stubs.el ends here
