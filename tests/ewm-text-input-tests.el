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

;; The compositor dynamic module isn't loaded in the test harness; stub
;; the module entry points `ewm-text-input' needs.
(defun ewm-im-commit-module (text surface-id)
  (push (cons surface-id text) ewm-text-input-tests--commits))

(defun ewm-text-input-intercept-module (_enabled) nil)

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
  `(let ((old-surfaces (and (boundp 'ewm--surfaces) ewm--surfaces))
         (ewm-text-input-tests--focused-id nil)
         (ewm-text-input-tests--commits nil))
     (unwind-protect
         (progn
           (setq ewm--surfaces (make-hash-table))
           ,@body)
       (setq ewm--surfaces old-surfaces))))

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
          (ewm--handle-key '((utf8 . "a")))))
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
          (ewm--handle-key '((utf8 . "a")))))
      (should (equal ewm-text-input-tests--commits '((10 . "a")))))))

(ert-deftest ewm-text-input-tests/handle-key-without-surface-buffer-does-not-commit ()
  "Without a focused surface buffer there is no buffer-local input method."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (caller)
      (setq ewm-text-input-tests--focused-id 10)
      (with-current-buffer caller
        (setq-local current-input-method "caller-im")
        (ewm-text-input-tests--with-fake-quail
          (ewm--handle-key '((utf8 . "a")))))
      (should-not ewm-text-input-tests--commits))))

(provide 'ewm-text-input-tests)
;;; ewm-text-input-tests.el ends here
