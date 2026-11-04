;;; ewm-session-tests.el --- ERT tests for ewm-session -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins the Lisp-only contracts of the session integration: the desktop
;; handler, the placeholder and the placeholder matching of the placement hook.
;; The restore itself runs in the VM lane.
;;
;; Run:
;;   emacs -Q -batch -L lisp -l tests/ewm-session-tests.el \
;;         -f ert-run-tests-batch-and-exit

;;; Code:

(require 'ert)
(require 'cl-lib)

(load (expand-file-name "ewm-test-stubs"
                        (file-name-directory (or load-file-name buffer-file-name)))
      nil t)

(require 'ewm-session)

(defconst ewm-session-tests--launch '(:app "firefox" :command ("firefox") :directory "/tmp/"))

(ert-deftest ewm-session-tests/the-desktop-handler-makes-a-placeholder-once ()
  "The handler of both modes makes a placeholder that names its app and records itself again."
  (let ((buffer (ewm-session--restore-desktop-buffer nil "*ewm:firefox*" ewm-session-tests--launch)))
    (unwind-protect
        (progn
          (should (eq buffer (ewm-session--restore-desktop-buffer nil "*ewm:firefox*" nil)))
          (should-not (ewm-session--restore-desktop-buffer nil "*ewm:none*" nil))
          (should (eq 'ewm-placeholder-mode (buffer-local-value 'major-mode buffer)))
          (with-current-buffer buffer
            (should (equal ewm-session-tests--launch (funcall desktop-save-buffer nil)))
            (ewm-session--draw-placeholder (selected-window))
            (should (string-match-p "firefox not ready" (buffer-string))))
          (dolist (mode '(ewm-surface-mode ewm-placeholder-mode))
            (should (eq #'ewm-session--restore-desktop-buffer
                        (alist-get mode desktop-buffer-mode-handlers)))))
      (kill-buffer buffer))))

(ert-deftest ewm-session-tests/a-surface-finds-its-placeholder-by-token-pid-then-app ()
  "Matching prefers the token, then the pid chain, then the app, shown placeholders first, else none."
  (let* ((ewm--surfaces (make-hash-table :test 'eql))
         (by-token (ewm-session--make-placeholder "by-token" ewm-session-tests--launch))
         (by-pid (ewm-session--make-placeholder "by-pid" ewm-session-tests--launch))
         (by-app (ewm-session--make-placeholder "by-app" ewm-session-tests--launch))
         (shown (ewm-session--make-placeholder "shown" ewm-session-tests--launch))
         (spawned (make-pipe-process :name "ewm-session-token" :noquery t))
         (running (make-process :name "ewm-session-pid" :command '("sleep" "60") :noquery t))
         (surface (ewm--create-surface-buffer 7 "firefox" (process-id running)))
         (other (ewm--create-surface-buffer 8 "firefox" (emacs-pid))))
    (unwind-protect
        (progn
          (process-put spawned 'ewm-launch-token "tok")
          (with-current-buffer by-token (setq ewm-session--process spawned))
          (with-current-buffer by-pid (setq ewm-session--process running))
          (with-current-buffer surface (setq ewm-surface-token "tok"))
          (should (eq by-token (ewm-session--placeholder-for surface)))
          (kill-buffer by-token)
          (should (eq by-pid (ewm-session--placeholder-for surface)))
          (kill-buffer by-pid)
          (with-current-buffer surface (setq ewm-surface-app "chromium"))
          (should-not (ewm-session--place surface))
          (should (buffer-live-p by-app))
          (with-current-buffer surface (setq ewm-surface-app "firefox"))
          ;; The shown placeholder wins over the buried one and shows the surface in its window.
          (save-window-excursion
            (set-window-buffer (selected-window) shown)
            (should (eq shown (ewm-session--placeholder-for surface)))
            (should (ewm-session--place surface))
            (should (eq surface (window-buffer (selected-window))))
            (should-not (buffer-live-p shown))
            (should (buffer-live-p by-app)))
          ;; The buried placeholder claims the next surface without showing it.
          (should (ewm-session--place other))
          (should-not (buffer-live-p by-app))
          (should-not (get-buffer-window other t))
          (should-not (ewm-session--place other)))
      (delete-process spawned)
      (delete-process running)
      (dolist (buffer (list by-token by-pid by-app shown surface other))
        (when (buffer-live-p buffer) (kill-buffer buffer))))))

(provide 'ewm-session-tests)
;;; ewm-session-tests.el ends here
