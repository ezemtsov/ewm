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
(require 'cl-lib)

;; A stale .elc next to a newer .el would otherwise mask the source we're
;; meant to be testing.
(setq load-prefer-newer t)

;; The compositor dynamic module isn't loaded in the test harness; stub
;; the module entry points the libs would otherwise pull in.
(defmacro ewm-test--stub (name args &rest body)
  `(unless (fboundp ',name) (defun ,name ,args ,@body)))

(ewm-test--stub ewm-intercept-keys-module (_keys) nil)
(ewm-test--stub ewm-keyboard-capture-begin-module (_reason) 1)
(ewm-test--stub ewm-keyboard-capture-end-module (_token) t)
(ewm-test--stub ewm-keyboard-redirect-capture-clear-module () nil)
(ewm-test--stub ewm-layout--menu-active-p () nil)
(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
(require 'ewm-input)

(defvar ewm--module-mode)
(defvar ewm-input--keyboard-capture-token)

(defmacro ewm-input-tests--with-capture-state (state &rest body)
  "Run BODY with capture module calls recorded in `calls'."
  (declare (indent 1))
  (cl-destructuring-bind (token command &optional menu-active) state
    `(let ((ewm--module-mode t)
           (ewm-input--keyboard-capture-token ,token)
           (this-command ,command)
           (prefix-arg nil)
           calls)
       (cl-letf (((symbol-function 'ewm-keyboard-capture-begin-module)
                  (lambda (reason) (push (list :begin reason) calls) 42))
                 ((symbol-function 'ewm-keyboard-capture-end-module)
                  (lambda (ended-token) (push (list :end ended-token) calls) t))
                 ((symbol-function 'ewm-keyboard-redirect-capture-clear-module)
                  (lambda () (push :clear-redirect calls) t))
                 ((symbol-function 'ewm-layout--menu-active-p)
                  (lambda () ,menu-active)))
         ,@body))))

(defmacro ewm-input-tests--should-not-refresh-layout (&rest body)
  "Run BODY and fail if it asks the layout layer to refresh."
  (declare (indent 0))
  `(let (refreshed)
     (cl-letf (((symbol-function 'ewm-layout--refresh)
                (lambda () (setq refreshed t))))
       ,@body)
     (should-not refreshed)))

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
    (should (equal (plist-get spec :description) "a"))
    (should (equal (plist-get spec :dispatch) "command"))
    (should (eq (plist-get spec :repeat) :false))
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

(ert-deftest ewm-input-tests/spec-keymap-prefix-dispatch ()
  "Keymap prefixes request keyboard dispatch."
  (let ((spec (ewm--event-to-intercept-spec ?\C-x)))
    (should (equal (plist-get spec :dispatch) "keyboard"))))

(ert-deftest ewm-input-tests/intercept-prefix-command-uses-keyboard-dispatch ()
  "Plain intercept-prefix commands like M-x enter the Emacs keyboard path."
  (should (equal (ewm--event-dispatch ?\M-x t) "keyboard"))
  (should (equal (ewm--event-dispatch ?\C-u t) "keyboard")))

(ert-deftest ewm-input-tests/mode-map-command-uses-command-dispatch ()
  "`ewm-mode-map' commands stay on command dispatch, even if fullscreen-lifted."
  (let ((event (aref (key-parse "<AudioLowerVolume>") 0)))
    (should (equal (ewm--event-dispatch event t) "command"))))

(ert-deftest ewm-input-tests/repeatable-mode-command-sets-repeat ()
  "Only explicitly repeatable commands request compositor command repeat."
  (let ((repeatable (ewm--event-to-intercept-spec (aref (kbd "s-<left>") 0)))
        (one-shot (ewm--event-to-intercept-spec (aref (kbd "s-f") 0))))
    (should (eq (plist-get repeatable :repeat) t))
    (should (eq (plist-get one-shot :repeat) :false))))

(ert-deftest ewm-input-tests/send-intercept-keys-has-runtime-dispatch-contract ()
  "The actual module payload keeps M-x keyboard-routed and media command-routed."
  (let ((ewm-intercept-prefixes
         '(?\M-x ?\C-u ("<AudioLowerVolume>" :fullscreen)
           ("<Print>" :fullscreen) "<Print>"))
        (ewm-surface-emulate-keys nil)
        captured)
    (cl-letf (((symbol-function 'ewm-intercept-keys-module)
               (lambda (keys) (setq captured keys))))
      (ewm--send-intercept-keys))
    (let* ((specs (append captured nil))
           (by-desc (lambda (description)
                      (cl-remove-if-not
                       (lambda (spec)
                         (equal (plist-get spec :description) description))
                       specs)))
           (mx (car (funcall by-desc "M-x")))
           (cu (car (funcall by-desc "C-u")))
           (audio (funcall by-desc "<AudioLowerVolume>"))
           (print (car (funcall by-desc "<Print>"))))
      (should mx)
      (should (equal (plist-get mx :dispatch) "keyboard"))
      (should cu)
      (should (equal (plist-get cu :dispatch) "keyboard"))
      (should print)
      (should (eq (plist-get print :fullscreen) t))
      (should audio)
      (should (= (length audio) 1))
      (dolist (spec audio)
        (should (equal (plist-get spec :dispatch) "command"))
        (should (eq (plist-get spec :fullscreen) t))
        (should (eq (plist-get spec :repeat) t))))))

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

(ert-deftest ewm-input-tests/intercepted-command-uses-ewm-mode-map-binding ()
  "Command events invoke the `ewm-mode-map' binding, not ambient maps."
  (let ((ewm-mode-map (copy-keymap ewm-mode-map))
        (ewm--module-mode nil)
        (ewm-input--keyboard-capture-token nil)
        (ambient-map (make-sparse-keymap))
        seen
        ambient-hit)
    (define-key ewm-mode-map (kbd "s-1")
                (lambda ()
                  (interactive)
                  (setq seen last-command-event)))
    (define-key ambient-map (kbd "s-1")
                (lambda ()
                  (interactive)
                  (setq ambient-hit t)))
    (let ((overriding-terminal-local-map ambient-map))
      (ewm--handle-intercepted-command '((key . "s-1"))))
    (should (equal seen ?\s-1))
    (should-not ambient-hit)))

(ert-deftest ewm-input-tests/intercepted-command-syncs-keyboard-capture ()
  "Command-event dispatch updates capture state after the command returns."
  (let ((ewm-mode-map (copy-keymap ewm-mode-map))
        synced)
    (define-key ewm-mode-map (kbd "s-2")
                (lambda () (interactive)))
    (cl-letf (((symbol-function 'ewm-input--sync-keyboard-capture)
               (lambda () (setq synced t))))
      (ewm--handle-intercepted-command '((key . "s-2"))))
    (should synced)))

(ert-deftest ewm-input-tests/intercepted-command-cleanup-does-not-refresh-layout ()
  "Command dispatch may sync capture, but must not send layout updates."
  (let ((ewm-mode-map (copy-keymap ewm-mode-map))
        (ewm--module-mode t)
        (ewm-input--keyboard-capture-token 42)
        calls)
    (define-key ewm-mode-map (kbd "s-3")
                (lambda () (interactive)))
    (cl-letf (((symbol-function 'ewm-keyboard-capture-end-module)
               (lambda (token) (push (list :end token) calls) t))
              ((symbol-function 'ewm-keyboard-redirect-capture-clear-module)
               (lambda () (push :clear-redirect calls) t))
              ((symbol-function 'ewm-layout--menu-active-p)
               (lambda () nil)))
      (ewm-input-tests--should-not-refresh-layout
        (ewm--handle-intercepted-command '((key . "s-3")))))
    (should (equal calls '(:clear-redirect (:end 42))))
    (should-not ewm-input--keyboard-capture-token)))

;;;; keyboard capture sync

(ert-deftest ewm-input-tests/keyboard-capture-sync-inactive-clears-redirect-capture ()
  "Inactive sync clears compositor-owned keyboard-redirect capture."
  (ewm-input-tests--with-capture-state (nil 'ignore)
    (ewm-input--sync-keyboard-capture)
    (should (equal calls '(:clear-redirect)))
    (should-not ewm-input--keyboard-capture-token)))

(ert-deftest ewm-input-tests/keyboard-capture-sync-begins-prefix-token ()
  "A keymap prefix command begins compositor keyboard capture."
  (ewm-input-tests--with-capture-state (nil nil)
    (ewm-input--sync-keyboard-capture)
    (should (equal calls '((:begin "command-loop"))))
    (should (equal ewm-input--keyboard-capture-token 42))))

(ert-deftest ewm-input-tests/keyboard-capture-sync-begins-modal-token ()
  "Shared modal state begins compositor keyboard capture."
  (ewm-input-tests--with-capture-state (nil 'ignore t)
    (ewm-input--sync-keyboard-capture)
    (should (equal calls '((:begin "command-loop"))))
    (should (equal ewm-input--keyboard-capture-token 42))))

(ert-deftest ewm-input-tests/keyboard-capture-sync-ends-current-token ()
  "Leaving command-loop input ends exactly the active capture token."
  (ewm-input-tests--with-capture-state (42 'ignore)
    (ewm-input--sync-keyboard-capture)
    (should (equal calls '(:clear-redirect (:end 42))))
    (should-not ewm-input--keyboard-capture-token)))

(ert-deftest ewm-input-tests/keyboard-capture-sync-does-not-refresh-layout ()
  "Keyboard-capture leases are focus overlays, not layout updates.
Covers inactive cleanup, capture begin, and capture end so broadening any
capture transition back into a layout send fails here."
  (ewm-input-tests--with-capture-state (nil 'ignore)
    (ewm-input-tests--should-not-refresh-layout
      (ewm-input--sync-keyboard-capture)))
  (ewm-input-tests--with-capture-state (nil nil)
    (ewm-input-tests--should-not-refresh-layout
      (ewm-input--sync-keyboard-capture)))
  (ewm-input-tests--with-capture-state (nil 'ignore t)
    (ewm-input-tests--should-not-refresh-layout
      (ewm-input--sync-keyboard-capture)))
  (ewm-input-tests--with-capture-state (42 'ignore)
    (ewm-input-tests--should-not-refresh-layout
      (ewm-input--sync-keyboard-capture))))

(ert-deftest ewm-input-tests/keyboard-capture-force-disable-clears-redirect-capture ()
  "Forced disable clears compositor-owned keyboard-redirect capture."
  (ewm-input-tests--with-capture-state (nil 'ignore)
    (ewm-input--set-keyboard-capture nil t)
    (should (equal calls '(:clear-redirect)))
    (should-not ewm-input--keyboard-capture-token)))

(provide 'ewm-input-tests)
;;; ewm-input-tests.el ends here
