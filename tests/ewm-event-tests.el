;;; ewm-event-tests.el --- ERT tests for EWM event pipe -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins compositor event-pipe behavior.  The process filter must not let
;; user cancellation (`quit', normally C-g) escape as an Emacs process-filter
;; error while handling a compositor event.
;;
;; Run:
;;   emacs -Q -batch -L lisp -l tests/ewm-event-tests.el \
;;         -f ert-run-tests-batch-and-exit

;;; Code:

(require 'ert)
(require 'cl-lib)

(setq load-prefer-newer t)

;; The compositor dynamic module isn't loaded in the test harness; stub
;; the module entry points the libs would otherwise pull in.
(defmacro ewm-test--stub (name args &rest body)
  `(unless (fboundp ',name) (defun ,name ,args ,@body)))

(ewm-test--stub ewm-output-layout (_output _frames) nil)
(ewm-test--stub ewm-focus-view-module (_view-id) nil)
(ewm-test--stub ewm-prepare-frame-close-module (_surface-id) nil)
(ewm-test--stub ewm-intercept-keys-module (_keys) nil)
(ewm-test--stub ewm-configure-input-module (_config) nil)
(ewm-test--stub ewm-keyboard-capture-begin-module (_reason) 1)
(ewm-test--stub ewm-keyboard-capture-end-module (_token) t)
(ewm-test--stub ewm-keyboard-redirect-capture-clear-module () nil)
(ewm-test--stub ewm-im-commit-module (_text _surface-id) nil)
(ewm-test--stub ewm-text-input-intercept-module (_enabled) nil)
(ewm-test--stub ewm-get-focused-id () nil)
(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
(require 'ewm)

(ert-deftest ewm-event-tests/filter-consumes-handler-quit ()
  "A handler `quit' must not escape the compositor process filter."
  (let ((ewm--event-buffer "")
        called)
    (cl-letf (((symbol-function 'ewm--handle-event)
               (lambda (_event)
                 (setq called t)
                 (signal 'quit nil))))
      (ewm--event-filter nil "{\"event\":\"ready\"}\n"))
    (should called)
    (should (equal ewm--event-buffer ""))))

(provide 'ewm-event-tests)
;;; ewm-event-tests.el ends here
