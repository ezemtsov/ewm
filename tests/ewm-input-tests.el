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
(ewm-test--stub ewm-get-focused-id () 2)
(ewm-test--stub ewm-get-focused-frame-id () 2)
(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
(require 'ewm-input)

(defvar ewm-input-tests--focused-id 2)
(defvar ewm-input-tests--focused-frame-id 2)

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
                  (lambda () ,menu-active))
                 ((symbol-function 'ewm-get-focused-id)
                  (lambda () ewm-input-tests--focused-id))
                 ((symbol-function 'ewm-get-focused-frame-id)
                  (lambda () ewm-input-tests--focused-frame-id)))
         ,@body))))

(defmacro ewm-input-tests--should-not-refresh-layout (&rest body)
  "Run BODY and fail if it asks the layout layer to refresh."
  (declare (indent 0))
  `(let (refreshed)
     (cl-letf (((symbol-function 'ewm-layout--refresh)
                (lambda () (setq refreshed t))))
       ,@body)
     (should-not refreshed)))

;;;; Keymap bindings

(ert-deftest ewm-input-tests/super-shift-space-binds-floating-toggle ()
  "Super-Shift-space is the floating detach/attach toggle."
  (should (eq (lookup-key ewm-mode-map (kbd "s-S-SPC"))
              #'ewm-floating-toggle)))

;;;; ewm--event-to-intercept-spec

(ert-deftest ewm-input-tests/spec-plain-key ()
  "Plain character event maps to :key with every flag nil."
  (let ((spec (ewm--event-to-intercept-spec ?a)))
    (should (equal (plist-get spec :key) ?a))
    (should-not (plist-get spec :ctrl))
    (should-not (plist-get spec :alt))
    (should-not (plist-get spec :shift))
    (should-not (plist-get spec :super))
    (should-not (plist-get spec :fullscreen))
    (should (equal (plist-get spec :description) "a"))
    (should (equal (plist-get spec :dispatch) "command"))
    (should-not (plist-get spec :repeat))))

(ert-deftest ewm-input-tests/spec-fullscreen-flag ()
  "`fullscreen=t' lifts :fullscreen in the plist."
  (let ((spec (ewm--event-to-intercept-spec ?a t)))
    (should (eq (plist-get spec :fullscreen) t))))

(defun ewm-input-tests--go () (interactive))
(function-put 'ewm-input-tests--go 'ewm-action '(focus-frame :delta 1))
(defun ewm-input-tests--nth () (interactive))
(function-put 'ewm-input-tests--nth 'ewm-action #'ewm--frame-select-action)

(ert-deftest ewm-input-tests/spec-action ()
  "A command's `ewm-action' rides the spec, a function of the key when it is one."
  (define-key ewm-mode-map (kbd "s-<f9>") #'ewm-input-tests--go)
  (define-key ewm-mode-map (kbd "s-C-7") #'ewm-input-tests--nth)
  (unwind-protect
      (progn
        (should (equal (plist-get (ewm--event-to-intercept-spec 's-f9) :action)
                       '(focus-frame :delta 1)))
        (should (equal (plist-get (ewm--event-to-intercept-spec ?\C-\s-7) :action)
                       '(focus-nth-frame :n 7)))
        (should-not (plist-get (ewm--event-to-intercept-spec ?a) :action)))
    (define-key ewm-mode-map (kbd "s-<f9>") nil)
    (define-key ewm-mode-map (kbd "s-C-7") nil)))

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
    (should-not (plist-get one-shot :repeat))))

(ert-deftest ewm-input-tests/xf86-aliases-follow-gdk-style-bindings ()
  "CamelCase hardware bindings get xkbcommon XF86 aliases."
  (let ((function-key-map (make-sparse-keymap))
        (ewm-mode-map (make-sparse-keymap)))
    (define-key ewm-mode-map (kbd "<AudioRaiseVolume>") #'ignore)
    (define-key ewm-mode-map (kbd "<left>") #'ignore)
    (ewm-input--install-xf86-key-aliases)
    (should (equal (lookup-key function-key-map [XF86AudioRaiseVolume])
                   [AudioRaiseVolume]))
    (should-not (lookup-key function-key-map [XF86left]))))

(ert-deftest ewm-input-tests/xf86-aliases-preserve-existing-translations ()
  "Existing function-key translations are not overwritten."
  (let ((function-key-map (make-sparse-keymap))
        (ewm-mode-map (make-sparse-keymap)))
    (define-key function-key-map [XF86AudioRaiseVolume] [custom-audio-up])
    (define-key ewm-mode-map (kbd "<AudioRaiseVolume>") #'ignore)
    (ewm-input--install-xf86-key-aliases)
    (should (equal (lookup-key function-key-map [XF86AudioRaiseVolume])
                   [custom-audio-up]))))

(ert-deftest ewm-input-tests/send-intercept-keys-has-runtime-dispatch-contract ()
  "The actual module payload keeps M-x keyboard-routed and media command-routed."
  (let ((ewm--module-mode t)
        (ewm-intercept-prefixes
         '(?\M-x ?\C-u ("<AudioLowerVolume>" :fullscreen)
           ("<Print>" :fullscreen) "<Print>"))
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

(ert-deftest ewm-input-tests/key-event-names-one-key ()
  "`ewm--key-event' returns the one event a description names, else nil."
  (should (eq (ewm--key-event "<Print>") 'Print))
  (should-not (ewm--key-event "C-x C-f"))
  (should-not (ewm--key-event "")))

(ert-deftest ewm-input-tests/intercepted-command-uses-active-keymap ()
  "Queued commands respect normal key lookup and command hooks."
  (let ((overriding-terminal-local-map (make-sparse-keymap))
        (unread-command-events nil)
        (pre-command-hook nil)
        (post-command-hook nil)
        seen)
    (define-key overriding-terminal-local-map (kbd "s-1")
                (lambda () (interactive) (push last-command-event seen)))
    (add-hook 'post-command-hook
              (lambda () (when (equal last-command-event ?\s-1)
                           (push 'post seen))))
    (ewm--handle-intercepted-command '((key . "s-1")))
    (should-not seen)
    (execute-kbd-macro [])
    (should (equal seen '(post ?\s-1)))))

(ert-deftest ewm-input-tests/intercepted-command-retains-event-order ()
  "Multiple pipe events append to input already waiting for Emacs."
  (let ((unread-command-events (list (cons t ?a))))
    (ewm--handle-intercepted-command '((key . "s-2")))
    (ewm--handle-intercepted-command '((key . "s-3")))
    (should (equal unread-command-events '((t . ?a) (t . ?\s-2) (t . ?\s-3))))))

(ert-deftest ewm-input-tests/intercepted-command-does-not-change-focus ()
  "Queueing input leaves capture and layout changes to command processing."
  (let ((ewm--module-mode t)
        (ewm-input--keyboard-capture-token 42)
        (unread-command-events nil)
        calls)
    (cl-letf (((symbol-function 'ewm-keyboard-capture-end-module)
               (lambda (token) (push token calls)))
              ((symbol-function 'ewm-keyboard-redirect-capture-clear-module)
               (lambda () (push :clear calls))))
      (ewm-input-tests--should-not-refresh-layout
        (ewm--handle-intercepted-command '((key . "s-3")))))
    (should-not calls)
    (should (equal ewm-input--keyboard-capture-token 42))))

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
    (cl-letf (((symbol-function 'this-command-keys-vector)
               (lambda () (kbd "C-x"))))
      (ewm-input--sync-keyboard-capture))
    (should (equal calls '((:begin "command-loop"))))
    (should (equal ewm-input--keyboard-capture-token 42))))

(ert-deftest ewm-input-tests/keyboard-capture-sync-ends-non-prefix-null-command ()
  "A completed non-prefix read must not keep command-loop capture alive."
  (ewm-input-tests--with-capture-state (42 nil)
    (cl-letf (((symbol-function 'this-command-keys-vector)
               (lambda () [?g])))
      (ewm-input--sync-keyboard-capture))
    (should (equal calls '(:clear-redirect (:end 42))))
    (should-not ewm-input--keyboard-capture-token)))

(ert-deftest ewm-input-tests/keyboard-capture-sync-begins-modal-token ()
  "Shared modal state begins compositor keyboard capture."
  (ewm-input-tests--with-capture-state (nil 'ignore t)
    (ewm-input--sync-keyboard-capture)
    (should (equal calls '((:begin "command-loop"))))
    (should (equal ewm-input--keyboard-capture-token 42))))

(ert-deftest ewm-input-tests/keyboard-capture-sync-denies-background-modal ()
  "App-focused background modal input must not borrow keyboard focus."
  (ewm-input-tests--with-capture-state (nil 'ignore t)
    (let ((ewm-input-tests--focused-id 3)
          (ewm-input-tests--focused-frame-id 2))
      (ewm-input--sync-keyboard-capture))
    (should (equal calls '(:clear-redirect)))
    (should-not ewm-input--keyboard-capture-token)))

(ert-deftest ewm-input-tests/keyboard-capture-on-denies-background-read ()
  "read-key advice must not capture app focus without command provenance."
  (ewm-input-tests--with-capture-state (nil nil)
    (let ((ewm-input-tests--focused-id 3)
          (ewm-input-tests--focused-frame-id 2))
      (cl-letf (((symbol-function 'this-command-keys-vector)
                 (lambda () [])))
        (ewm-input--keyboard-capture-on)))
    (should-not calls)
    (should-not ewm-input--keyboard-capture-token)))

(ert-deftest ewm-input-tests/keyboard-capture-on-allows-command-key ()
  "Keyboard-dispatched commands like app-focused M-x may capture immediately."
  (ewm-input-tests--with-capture-state (nil 'execute-extended-command)
    (let ((ewm-input-tests--focused-id 3)
          (ewm-input-tests--focused-frame-id 2))
      (cl-letf (((symbol-function 'this-command-keys-vector)
                 (lambda () (kbd "M-x"))))
        (ewm-input--keyboard-capture-on)))
    (should (equal calls '((:begin "command-loop"))))
    (should (equal ewm-input--keyboard-capture-token 42))))

(ert-deftest ewm-input-tests/keyboard-capture-on-denies-client-field-composition ()
  "An input method reading for a client field must not pull the keyboard to Emacs."
  (ewm-input-tests--with-capture-state (nil 'execute-extended-command)
    (let ((ewm-input-tests--focused-id 2)
          (ewm-input-tests--focused-frame-id 2)
          (ewm-text-input--composing 2))
      (cl-letf (((symbol-function 'this-command-keys-vector)
                 (lambda () (kbd "M-x"))))
        (ewm-input--keyboard-capture-on)))
    (should-not calls)
    (should-not ewm-input--keyboard-capture-token)))

(ert-deftest ewm-input-tests/keyboard-capture-on-allows-intercepted-command ()
  "A queued EWM command has normal command-key provenance for capture."
  (ewm-input-tests--with-capture-state (nil 'ewm-launch-app)
    (let ((ewm-input-tests--focused-id 3)
          (ewm-input-tests--focused-frame-id 2))
      (cl-letf (((symbol-function 'this-command-keys-vector)
                 (lambda () (kbd "s-d"))))
        (ewm-input--keyboard-capture-on)))
    (should (equal calls '((:begin "command-loop"))))
    (should (equal ewm-input--keyboard-capture-token 42))))

(ert-deftest ewm-input-tests/keyboard-capture-leaves-click-popup-alone ()
  "A click's popup menu neither begins nor ends capture."
  (ewm-input-tests--with-capture-state (42 nil)
    (cl-letf (((symbol-function 'menu-or-popup-active-p) (lambda () t)))
      (ewm-input--with-keyboard-capture #'ignore))
    (should-not calls)))

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

(ert-deftest ewm-input-tests/in-selected-window-defers-while-composing ()
  "While a client field composes, the advice must not re-route to another buffer."
  (let ((ewm-text-input--composing 7) called)
    (with-temp-buffer                     ; the window's buffer
      (let ((win-buf (current-buffer)))
        (with-temp-buffer                 ; a buffer distinct from the window's
          (cl-letf (((symbol-function 'window-buffer) (lambda (&optional _) win-buf)))
            (ewm-input--in-selected-window (lambda (k) (setq called k) (list k)) ?a)))))
    (should (eq called ?a))))

(ert-deftest ewm-input-tests/in-selected-window-routes-to-window-buffer ()
  "With no composition and a stale current-buffer, use the selected window's method."
  (let ((ewm-text-input--composing nil) routed)
    (with-temp-buffer                     ; the window's buffer
      (let ((win-buf (current-buffer)))
        (setq-local current-input-method "x")
        (setq-local input-method-function
                    (lambda (k) (setq routed (cons k (current-buffer))) (list k)))
        (with-temp-buffer                 ; a stale current buffer
          (cl-letf (((symbol-function 'window-buffer) (lambda (&optional _) win-buf)))
            (ewm-input--in-selected-window
             (lambda (_k) (error "must not translate in the wrong buffer")) ?z)))
        (should (equal routed (cons ?z win-buf)))))))

(provide 'ewm-input-tests)
;;; ewm-input-tests.el ends here
