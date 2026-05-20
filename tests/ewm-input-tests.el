;;; ewm-input-tests.el --- ERT tests for EWM intercept-key parsing -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins the Lisp-side half of the K-series contract: parsing
;; `ewm-intercept-prefixes' entries and building the InterceptedKey
;; plists the compositor consumes via the module bridge.  The Rust-side
;; classification of the same plists is pinned by the K1-K4 proptest in
;; `compositor/tests/key_classify_proptest.rs'.
;;
;; Run:
;;   emacs -Q -batch -L lisp -l tests/ewm-input-tests.el \
;;         -f ert-run-tests-batch-and-exit

;;; Code:

(require 'ert)

;; A stale .elc next to a newer .el would otherwise mask the source we're
;; meant to be testing.
(setq load-prefer-newer t)

;; The compositor dynamic module isn't loaded in the test harness; stub
;; the module entry points the libs would otherwise pull in.
(unless (fboundp 'ewm-set-intercepted-keys-module)
  (defun ewm-set-intercepted-keys-module (_keys) nil))
(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
(require 'ewm-input)

;;;; ewm--event-to-intercept-spec

(ert-deftest ewm-input-tests/spec-plain-key ()
  "Plain character event maps to :key and :fullscreen :false."
  (let ((spec (ewm--event-to-intercept-spec ?a)))
    (should (equal (plist-get spec :key) ?a))
    (should (eq (plist-get spec :ctrl) :false))
    (should (eq (plist-get spec :alt) :false))
    (should (eq (plist-get spec :shift) :false))
    (should (eq (plist-get spec :super) :false))
    (should (eq (plist-get spec :fullscreen) :false))
    (should-not (plist-get spec :emulate))))

(ert-deftest ewm-input-tests/spec-fullscreen-flag ()
  "`fullscreen=t' lifts :fullscreen in the plist."
  (let ((spec (ewm--event-to-intercept-spec ?a t)))
    (should (eq (plist-get spec :fullscreen) t))))

(ert-deftest ewm-input-tests/spec-modifiers ()
  "Ctrl/Meta/Shift/Super are surfaced as boolean flags."
  (let ((spec (ewm--event-to-intercept-spec
               (event-convert-list '(control meta shift super ?a)))))
    (should (eq (plist-get spec :ctrl) t))
    (should (eq (plist-get spec :alt) t))
    (should (eq (plist-get spec :shift) t))
    (should (eq (plist-get spec :super) t))))

(ert-deftest ewm-input-tests/spec-symbol-base ()
  "Special keys (symbol base) carry their name as :key."
  (let ((spec (ewm--event-to-intercept-spec 'XF86AudioMute)))
    (should (equal (plist-get spec :key) "XF86AudioMute"))))

(ert-deftest ewm-input-tests/spec-emulate-appended ()
  "`emulate' arg appears as a trailing :emulate entry."
  (let ((spec (ewm--event-to-intercept-spec ?c nil "ctrl")))
    (should (equal (plist-get spec :emulate) "ctrl"))))

;;;; ewm--parse-intercept-entry

(ert-deftest ewm-input-tests/parse-integer-entry ()
  "Bare character → (event . nil), no fullscreen."
  (should (equal (ewm--parse-intercept-entry ?\C-x) (cons ?\C-x nil))))

(ert-deftest ewm-input-tests/parse-string-entry ()
  "String entry is parsed via `key-parse', no fullscreen."
  (let ((parsed (ewm--parse-intercept-entry "<MonBrightnessUp>")))
    (should (equal (car parsed) (aref (key-parse "<MonBrightnessUp>") 0)))
    (should (null (cdr parsed)))))

(ert-deftest ewm-input-tests/parse-list-with-fullscreen ()
  "List containing `:fullscreen' carries the flag."
  (should (equal (ewm--parse-intercept-entry '(?\s-f :fullscreen))
                 (cons ?\s-f t))))

(ert-deftest ewm-input-tests/parse-list-string-with-fullscreen ()
  "String entry inside a `:fullscreen' list still carries the flag."
  (let ((parsed (ewm--parse-intercept-entry '("<MonBrightnessUp>" :fullscreen))))
    (should (equal (car parsed) (aref (key-parse "<MonBrightnessUp>") 0)))
    (should (eq (cdr parsed) t))))

(ert-deftest ewm-input-tests/parse-list-without-fullscreen-rejected ()
  "Lists without `:fullscreen' aren't recognised — return nil."
  (should (null (ewm--parse-intercept-entry '(?\s-f :other)))))

(ert-deftest ewm-input-tests/parse-invalid-returns-nil ()
  "Unknown shapes (number list, vector) return nil."
  (should (null (ewm--parse-intercept-entry [some vector])))
  (should (null (ewm--parse-intercept-entry 'a-symbol))))

(provide 'ewm-input-tests)
;;; ewm-input-tests.el ends here
