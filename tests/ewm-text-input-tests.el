;;; ewm-text-input-tests.el --- ERT tests for EWM text input -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins the Lisp-side text-input invariant: intercepted text is translated in
;; the focused surface buffer, using that buffer's input method and no other
;; buffer's input method.
;;
;; Run:
;;   emacs -Q -batch -L lisp -l tests/ewm-text-input-tests.el \
;;         -f ert-run-tests-batch-and-exit

;;; Code:

(require 'cl-lib)
(require 'ert)

(setq load-prefer-newer t)

(defvar ewm-text-input-tests--focused-id nil)
(defvar ewm-text-input-tests--commits nil)
(defvar ewm-text-input-tests--forwards nil)
(defvar ewm-text-input-tests--intercepts nil)

;; The compositor dynamic module isn't loaded in the test harness; stub
;; the module entry points `ewm-text-input' needs.
(defun ewm-im-commit-module (text surface-id)
  (push (cons surface-id text) ewm-text-input-tests--commits))

(defun ewm-text-input-forward-key-module (&rest args)
  (push args ewm-text-input-tests--forwards))

(defun ewm-text-input-intercept-module (enabled)
  (push enabled ewm-text-input-tests--intercepts))

(defun ewm-get-focused-id ()
  ewm-text-input-tests--focused-id)

(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
(require 'ewm-text-input)

(defvar ewm--surfaces)

(defmacro ewm-text-input-tests--with-state (&rest body)
  "Run BODY with isolated text-input globals."
  (declare (indent 0) (debug t))
  `(let ((old-mode ewm-text-input-mode)
         (ewm--surfaces (make-hash-table))
         (ewm-text-input-activated-hook nil)
         (ewm-text-input-deactivated-hook nil)
         (input-method-activate-hook nil)
         (input-method-deactivate-hook nil)
         (ewm-text-input--client-active nil)
         (ewm-text-input-tests--focused-id nil)
         (ewm-text-input-tests--commits nil)
         (ewm-text-input-tests--forwards nil)
         (ewm-text-input-tests--intercepts nil))
     (unwind-protect
         (progn
           (ewm-text-input-mode -1)
           ,@body)
       (ewm-text-input-mode (if old-mode 1 -1)))))

(defmacro ewm-text-input-tests--with-buffers (buffers &rest body)
  "Bind BUFFERS to temporary buffers while running BODY."
  (declare (indent 1) (debug t))
  `(let ,(mapcar (lambda (name)
                   `(,name (generate-new-buffer ,(format " *%s*" name))))
                 buffers)
     (unwind-protect
         (progn ,@body)
       ,@(mapcar (lambda (name)
                   `(when (buffer-live-p ,name)
                      (kill-buffer ,name)))
                 buffers))))

(defmacro ewm-text-input-tests--with-fake-quail (&rest body)
  "Run BODY with deterministic input-method lookup."
  (declare (indent 0) (debug t))
  `(cl-letf (((symbol-function 'activate-input-method) (lambda (_im) nil))
             ((symbol-function 'quail-lookup-key)
              (lambda (_key)
                (pcase current-input-method
                  ("surface-im" "surface")
                  ("caller-im" "caller")
                  (_ nil)))))
     ,@body))

(ert-deftest ewm-text-input-tests/handle-key-uses-focused-surface-buffer-input-method ()
  "The focused surface buffer's input method performs translation."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface caller)
      (puthash 10 surface ewm--surfaces)
      (setq ewm-text-input-tests--focused-id 10)
      (with-current-buffer surface
        (setq-local current-input-method "surface-im"))
      (with-current-buffer caller
        (setq-local current-input-method "caller-im")
        (ewm-text-input-tests--with-fake-quail
          (ewm--handle-key '((surface_id . 10) (utf8 . "a")))))
      (should (equal ewm-text-input-tests--commits '((10 . "surface")))))))

(ert-deftest ewm-text-input-tests/handle-key-does-not-use-caller-input-method ()
  "A surface with no input method must not inherit the caller buffer's method."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface caller)
      (puthash 10 surface ewm--surfaces)
      (setq ewm-text-input-tests--focused-id 10)
      (with-current-buffer surface
        (setq-local current-input-method nil))
      (with-current-buffer caller
        (setq-local current-input-method "caller-im")
        (ewm-text-input-tests--with-fake-quail
          (ewm--handle-key '((surface_id . 10) (utf8 . "a")))))
      (should (equal ewm-text-input-tests--commits '((10 . "a")))))))

(ert-deftest ewm-text-input-tests/handle-key-without-surface-buffer-forwards ()
  "A key for a surface with no buffer is handed back to the compositor."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (caller)
      (with-current-buffer caller
        (setq-local current-input-method "caller-im")
        (ewm-text-input-tests--with-fake-quail
          (ewm--handle-key '((surface_id . 10) (keycode . 38) (utf8 . "a")))))
      (should-not ewm-text-input-tests--commits)
      (should (equal ewm-text-input-tests--forwards
                     '((10 38 nil nil nil nil)))))))

(ert-deftest ewm-text-input-tests/handle-key-forwards-non-text-key ()
  "A key with no utf8 (Return, arrows) is forwarded, not committed."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (ewm--handle-key '((surface_id . 10) (keycode . 36)))
      (should-not ewm-text-input-tests--commits)
      (should (equal ewm-text-input-tests--forwards
                     '((10 36 nil nil nil nil)))))))

(ert-deftest ewm-text-input-tests/handle-key-forwards-control-text ()
  "Ctrl-modified text is a shortcut, not text to commit."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (ewm--handle-key '((surface_id . 10) (keycode . 54)
                         (utf8 . "c") (ctrl . t)))
      (should-not ewm-text-input-tests--commits)
      (should (equal ewm-text-input-tests--forwards
                     '((10 54 t nil nil nil)))))))

(ert-deftest ewm-text-input-tests/auto-mode-intercepts-only-with-input-method ()
  "Interception follows both client activation and an active input method."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (setq ewm-text-input-tests--focused-id 10)
      (ewm-text-input-auto-mode-enable)
      ;; Active field but no input method: indicator only, no interception.
      (ewm--handle-text-input-activated)
      (should-not ewm-text-input-mode)
      (should (equal ewm-text-input-tests--intercepts '(nil)))
      (should (eq (key-binding (kbd "a")) 'self-insert-command))
      ;; Input method appears: interception turns on.
      (with-current-buffer surface
        (setq-local current-input-method "surface-im"))
      (ewm-text-input--refresh-auto-state)
      (should ewm-text-input-mode)
      (should (equal ewm-text-input-tests--intercepts '(t nil)))
      ;; Field deactivates: interception turns off.
      (ewm--handle-text-input-deactivated)
      (should-not ewm-text-input-mode)
      (ewm-text-input-auto-mode-disable)
      (should-not ewm-text-input-mode)
      (should (equal ewm-text-input-tests--intercepts '(nil nil t nil))))))

(ert-deftest ewm-text-input-tests/auto-mode-ignores-ambient-input-method ()
  "Interception tracks the focused surface's input method, not an ambient one."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (setq ewm-text-input-tests--focused-id 10)
      (ewm-text-input-auto-mode-enable)
      ;; Surface has no input method, but the buffer we activate from does.
      (with-temp-buffer
        (setq-local current-input-method "ambient-im")
        (ewm--handle-text-input-activated))
      (should-not ewm-text-input-mode)
      (should (equal ewm-text-input-tests--intercepts '(nil))))))

(provide 'ewm-text-input-tests)
;;; ewm-text-input-tests.el ends here
