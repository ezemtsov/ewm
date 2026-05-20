;;; ewm-fullscreen-tests.el --- ERT tests for fullscreen policy -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins the one-fullscreen-per-frame policy on the lisp side: a new
;; fullscreen request must clear the `ewm-fullscreen' flag on any other
;; window that shares an emacs frame with the new target's windows.
;; Frames are independent — different frames (e.g. different EWM columns)
;; can each carry their own fullscreen state.
;;
;; Run:
;;   emacs -Q -batch -L lisp -l tests/ewm-fullscreen-tests.el \
;;         -f ert-run-tests-batch-and-exit

;;; Code:

(require 'ert)
(require 'cl-lib)

(setq load-prefer-newer t)

(unless (fboundp 'ewm-output-layout)
  (defun ewm-output-layout (_output _frames) nil))
(unless (fboundp 'ewm-focus-view-module)
  (defun ewm-focus-view-module (_view-id) nil))
(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
(require 'ewm)

(defmacro ewm-fullscreen-tests--with-two-windows (var-a var-b &rest body)
  "Bind VAR-A and VAR-B to two windows of the selected frame showing
distinct surface buffers, run BODY, then clean up."
  (declare (indent 2))
  `(let* ((buf-a (generate-new-buffer " *fs-test-a*"))
          (buf-b (generate-new-buffer " *fs-test-b*"))
          (orig-buf (window-buffer))
          (,var-a (selected-window))
          (,var-b nil))
     (unwind-protect
         (progn
           (set-window-buffer ,var-a buf-a)
           (setq ,var-b (split-window-right))
           (set-window-buffer ,var-b buf-b)
           ,@body)
       (when (and ,var-b (window-live-p ,var-b))
         (delete-window ,var-b))
       (set-window-buffer (selected-window) orig-buf)
       (when (buffer-live-p buf-a) (kill-buffer buf-a))
       (when (buffer-live-p buf-b) (kill-buffer buf-b)))))

(ert-deftest ewm-fullscreen-tests/new-fullscreen-clears-previous-in-same-frame ()
  "A new fullscreen target unfullscreens the previously-fullscreen window
in the same emacs frame."
  (ewm-fullscreen-tests--with-two-windows win-a win-b
    (set-window-parameter win-a 'ewm-fullscreen t)
    (ewm--clear-other-fullscreens (window-buffer win-b))
    (should (null (window-parameter win-a 'ewm-fullscreen)))))

(ert-deftest ewm-fullscreen-tests/clear-leaves-target-untouched ()
  "ewm--clear-other-fullscreens does not touch windows showing the kept buffer."
  (ewm-fullscreen-tests--with-two-windows win-a win-b
    (set-window-parameter win-b 'ewm-fullscreen t)
    (ewm--clear-other-fullscreens (window-buffer win-b))
    (should (eq (window-parameter win-b 'ewm-fullscreen) t))))

(ert-deftest ewm-fullscreen-tests/clear-noop-when-kept-buffer-has-no-windows ()
  "Calling the clear with a buffer that has no windows collects no frames
and leaves every existing flag in place."
  (ewm-fullscreen-tests--with-two-windows win-a win-b
    (set-window-parameter win-a 'ewm-fullscreen t)
    (let ((orphan (generate-new-buffer " *fs-test-orphan*")))
      (unwind-protect
          (progn
            (ewm--clear-other-fullscreens orphan)
            (should (eq (window-parameter win-a 'ewm-fullscreen) t)))
        (kill-buffer orphan)))))

(provide 'ewm-fullscreen-tests)
;;; ewm-fullscreen-tests.el ends here
