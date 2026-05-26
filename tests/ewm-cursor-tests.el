;;; ewm-cursor-tests.el --- ERT tests for cursor config -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins the Lisp-side cursor environment contract.  Auto must restore the
;; ambient theme instead of keeping the last explicit EWM theme.

;;; Code:

(require 'ert)

(setq load-prefer-newer t)

(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
(require 'ewm)

(ert-deftest ewm-cursor-tests/auto-restores-session-theme ()
  (let ((process-environment '("XCURSOR_THEME=AmbientTheme"
                               "XCURSOR_SIZE=16"))
        (ewm--session-cursor-theme "AmbientTheme")
        (ewm-cursor-theme "ExplicitTheme")
        (ewm-cursor-size 24))
    (ewm--apply-cursor-env)
    (should (equal (getenv "XCURSOR_THEME") "ExplicitTheme"))

    (setq ewm-cursor-theme nil)
    (ewm--apply-cursor-env)
    (should (equal (getenv "XCURSOR_THEME") "AmbientTheme"))
    (should (equal (getenv "XCURSOR_SIZE") "24"))))

(ert-deftest ewm-cursor-tests/auto-clears-theme-without-session-theme ()
  (let ((process-environment '("XCURSOR_THEME=ExplicitTheme"
                               "XCURSOR_SIZE=16"))
        (ewm--session-cursor-theme nil)
        (ewm-cursor-theme nil)
        (ewm-cursor-size 24))
    (ewm--apply-cursor-env)
    (should-not (getenv "XCURSOR_THEME"))
    (should (equal (getenv "XCURSOR_SIZE") "24"))))

(provide 'ewm-cursor-tests)
;;; ewm-cursor-tests.el ends here
