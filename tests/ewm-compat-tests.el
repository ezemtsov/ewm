;;; ewm-compat-tests.el --- ERT tests for the obsolete EWM options -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Code:

(require 'ert)
(require 'cl-lib)

(load (expand-file-name "ewm-test-stubs"
                        (file-name-directory (or load-file-name buffer-file-name)))
      nil t)
(require 'ewm-compat)

(ert-deftest ewm-compat-tests/obsolete-variables-become-bindings-and-say-so-once ()
  "The obsolete variables turn into `ewm-mode-map' bindings, malformed entries are skipped, and the lines are printed once."
  (let ((ewm-mode-map (make-sparse-keymap))
        (ewm-intercept-prefixes '(?\M-x ("<Print>" :fullscreen) (?\s-f :fullscreen) (?\C-o :fullscreen)
                                  "<MonBrightnessUp>" (?\s-q :other) [bogus]))
        (ewm-repeatable-commands '(kill-buffer ewm-frame-left))
        warnings)
    (keymap-set ewm-mode-map "s-q" #'kill-buffer)
    (keymap-set ewm-mode-map "s-f" (ewm-key #'ewm-toggle-fullscreen :repeat))
    (keymap-set ewm-mode-map "C-o a" #'ignore)
    (cl-letf (((symbol-function 'display-warning)
               (lambda (_type message &rest _) (push message warnings))))
      (ewm-compat--apply)
      (ewm-compat--apply))
    (should (equal (ewm--event-entry ?\M-x) '(nil :intercept)))
    (should (equal (ewm--event-entry 'Print) '(nil :intercept :fullscreen)))
    (should (equal (ewm--event-entry ?\s-f) '(ewm-toggle-fullscreen :repeat :fullscreen)))
    (should (equal (ewm--event-entry ?\s-q) '(kill-buffer :repeat)))
    (should (equal (ewm--event-entry 'MonBrightnessUp) '(nil :intercept)))
    (should (keymapp (keymap-lookup ewm-mode-map "C-o")))
    (should (equal warnings
                   (list (concat "ewm-intercept-prefixes and ewm-repeatable-commands are obsolete, "
                                 "the same bindings in ewm-mode-map:\n  "
                                 "(keymap-set ewm-mode-map \"M-x\" (ewm-key :intercept))\n  "
                                 "(keymap-set ewm-mode-map \"<Print>\" (ewm-key :intercept :fullscreen))\n  "
                                 "(keymap-set ewm-mode-map \"s-f\" (ewm-key #'ewm-toggle-fullscreen :repeat :fullscreen))\n  "
                                 "(keymap-set ewm-mode-map \"<MonBrightnessUp>\" (ewm-key :intercept))\n  "
                                 "(keymap-set ewm-mode-map \"s-q\" (ewm-key #'kill-buffer :repeat))"))))))

(provide 'ewm-compat-tests)
;;; ewm-compat-tests.el ends here
