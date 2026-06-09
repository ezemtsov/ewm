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
(ewm-test--stub ewm-init-event-channel (_process) nil)
(ewm-test--stub ewm-focus-target-module (_focus-id) nil)
(ewm-test--stub ewm-prepare-frame-close-module (_surface-id) nil)
(ewm-test--stub ewm-intercept-keys-module (_keys) nil)
(ewm-test--stub ewm-configure-idle-module (_secs _cmd) nil)
(ewm-test--stub ewm-configure-cursor-module (_theme _size) nil)
(ewm-test--stub ewm-configure-cursor-hide-module (_timeout _when-typing) nil)
(ewm-test--stub ewm-set-focus-follows-mouse (_enabled) nil)
(ewm-test--stub ewm-set-unfocused-alpha (_alpha) nil)
(ewm-test--stub ewm-set-animations-enabled (_enabled) nil)
(ewm-test--stub ewm-notify-initialized-module () nil)
(ewm-test--stub ewm-gtk-set-style (_css) nil)
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

(ert-deftest ewm-event-tests/direct-mode-enable-publishes-after-ready-event ()
  "Direct `(ewm-mode 1)' publishes mode state after the ready event."
  (let ((events nil)
        (original-mode (default-value 'ewm-mode))
        (original-global-minor-modes
         (when (boundp 'global-minor-modes) global-minor-modes))
        (original-switch-frame (lookup-key special-event-map [switch-frame]))
        (original-cut-function interprogram-cut-function)
        (original-paste-function interprogram-paste-function)
        (original-saved-cut-function ewm--saved-interprogram-cut-function)
        (original-saved-paste-function ewm--saved-interprogram-paste-function)
        (process-environment (copy-sequence process-environment))
        (ewm--module-mode nil)
        (ewm--compositor-ready nil)
        (ewm--event-process nil)
        (ewm--event-buffer "")
        (ewm-mode-hook nil))
    (cl-letf (((symbol-function 'ewm-running) (lambda () nil))
              ((symbol-function 'ewm-start) (lambda (_theme _size) t))
              ((symbol-function 'accept-process-output)
               (lambda (&rest _)
                 (push (list 'poll ewm-mode ewm--compositor-ready) events)
                 (ewm--event-filter nil "{\"event\":\"ready\"}\n")
                 t))
              ((symbol-function 'run-with-idle-timer) (lambda (&rest _) nil)))
      (unwind-protect
          (progn
            (setq-default ewm-mode nil)
            (setq ewm-mode-hook
                  (list (lambda ()
                          (push (list 'mode ewm-mode ewm--compositor-ready)
                                events))))
            (ewm-mode 1)
            (should (default-value 'ewm-mode))
            (should (equal (nreverse events)
                           '((poll nil nil) (mode t t)))))
        (ewm--destroy-event-channel)
        (ewm--teardown)
        (setq-default ewm-mode original-mode)
        (when (boundp 'global-minor-modes)
          (setq global-minor-modes original-global-minor-modes))
        (define-key special-event-map [switch-frame] original-switch-frame)
        (setq interprogram-cut-function original-cut-function)
        (setq interprogram-paste-function original-paste-function)
        (setq ewm--saved-interprogram-cut-function original-saved-cut-function)
        (setq ewm--saved-interprogram-paste-function original-saved-paste-function)
        (setq ewm--event-process nil)
        (setq ewm--event-buffer "")
        (setq ewm--module-mode nil)
        (setq ewm--compositor-ready nil)))))

(provide 'ewm-event-tests)
;;; ewm-event-tests.el ends here
