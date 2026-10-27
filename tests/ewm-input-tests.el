;;; ewm-input-tests.el --- ERT tests for EWM intercept-key parsing -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins the Lisp-side half of the K-series contract: reading the flags of
;; `ewm-mode-map' bindings and building the InterceptedKey plists the
;; compositor consumes via the module bridge.  The Rust-side
;; classification of the same plists is pinned by the K1-K4 proptest in
;; `compositor/tests/key_classify_proptest.rs'.
;;
;; Run:
;;   emacs -Q -batch -L lisp -l tests/ewm-input-tests.el \
;;         -f ert-run-tests-batch-and-exit

;;; Code:

(require 'ert)
(require 'ert-x)
(require 'cl-lib)

(load (expand-file-name "ewm-test-stubs"
                        (file-name-directory (or load-file-name buffer-file-name)))
      nil t)
(require 'ewm-input)

(defun ewm-input-tests--under-capture-state (token command menu focused composing keys thunk)
  "Call THUNK with the capture module stubbed, return (CALLS . TOKEN-AFTER).
CALLS are newest first."
  (let ((ewm-input--keyboard-capture-token token)
        (this-command command)
        (prefix-arg nil)
        (ewm-text-input--composing composing)
        calls)
    (cl-letf (((symbol-function 'ewm-keyboard-capture-begin-module)
               (lambda (reason) (push (list :begin reason) calls) 42))
              ((symbol-function 'ewm-keyboard-capture-end-module)
               (lambda (ended-token) (push (list :end ended-token) calls) t))
              ((symbol-function 'ewm-keyboard-redirect-capture-clear-module)
               (lambda () (push :clear-redirect calls) t))
              ((symbol-function 'ewm-layout--menu-active-p) (lambda () menu))
              ((symbol-function 'ewm-get-focused-id) (lambda () focused))
              ((symbol-function 'ewm-get-focused-frame-id) (lambda () 2))
              ((symbol-function 'this-command-keys-vector) (lambda () (key-parse keys))))
      (funcall thunk)
      (cons calls ewm-input--keyboard-capture-token))))

(defmacro ewm-input-tests--should-not-refresh-layout (&rest body)
  "Run BODY and fail if it asks the layout layer to refresh."
  (declare (indent 0))
  `(let (refreshed)
     (cl-letf (((symbol-function 'ewm-layout--refresh)
                (lambda () (setq refreshed t))))
       ,@body)
     (should-not refreshed)))

;;;; Keymap bindings

;;;; ewm--event-to-intercept-spec

(ert-deftest ewm-input-tests/intercept-specs-carry-the-key-its-modifiers-and-flags ()
  "A spec names the key, raises its modifiers as flags and reads the binding's flags."
  (let ((plain (ewm--event-to-intercept-spec ?a (list #'ewm-frame-new))))
    (should (equal (plist-get plain :key) ?a))
    (should (equal (plist-get plain :description) "a"))
    (should (equal (plist-get plain :dispatch) 'command))
    (dolist (flag '(:ctrl :alt :shift :super :fullscreen :repeat))
      (should-not (plist-get plain flag))))
  (let ((spec (ewm--event-to-intercept-spec
               (event-convert-list '(control meta shift super ?a)))))
    (dolist (flag '(:ctrl :alt :shift :super))
      (should (eq (plist-get spec flag) t))))
  (should (equal (plist-get (ewm--event-to-intercept-spec 'XF86AudioMute) :key)
                 "XF86AudioMute"))
  (should (eq (plist-get (ewm--event-to-intercept-spec (aref (kbd "s-<left>") 0)) :repeat) t))
  (should-not (plist-get (ewm--event-to-intercept-spec (aref (kbd "s-f") 0)) :repeat)))

(ert-deftest ewm-input-tests/ewm-key-rejects-bad-shapes ()
  "A command with :intercept, flags without a command and an unknown flag are errors."
  (should-error (ewm-key #'find-file :intercept))
  (should-error (ewm-key :fullscreen))
  (should-error (ewm-key #'find-file :bogus)))

(ert-deftest ewm-input-tests/flagged-bindings-keep-their-meaning ()
  "A flagged command runs as itself, an intercepted key keeps what Emacs gives it."
  (let ((ewm-mode-map (make-sparse-keymap)))
    (keymap-set ewm-mode-map "<f9>" (ewm-key #'find-file :repeat))
    (keymap-set ewm-mode-map "C-x" (ewm-key :intercept :fullscreen))
    (define-minor-mode ewm-input-tests--mode "" :global t :keymap ewm-mode-map)
    (unwind-protect
        (progn
          (ewm-input-tests--mode 1)
          (with-temp-buffer
            (dired-mode)
            (should (eq (key-binding (kbd "<f9>")) #'find-file))
            (should (eq (key-binding (kbd "C-x C-f")) #'find-file))
            (should (eq (key-binding (kbd "C-x C-q")) #'dired-toggle-read-only)))
          (should (equal (ewm--event-entry 'f9) '(find-file :repeat)))
          (should (equal (ewm--event-entry ?\C-x) '(nil :intercept :fullscreen)))
          (let ((spec (ewm--event-to-intercept-spec ?\C-x)))
            (should (equal (plist-get spec :dispatch) 'keyboard))
            (should (eq (plist-get spec :fullscreen) t)))
          (should (eq (plist-get (ewm--event-to-intercept-spec 'f9) :repeat) t)))
      (ewm-input-tests--mode -1))))

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
        (should (equal (plist-get (ewm--event-to-intercept-spec 's-f9) :dispatch)
                       '(action focus-frame :delta 1)))
        (should (equal (plist-get (ewm--event-to-intercept-spec ?\C-\s-7) :dispatch)
                       '(action focus-nth-frame :n 7)))
        (should (eq (plist-get (ewm--event-to-intercept-spec ?a (list #'ewm-frame-new)) :dispatch)
                    'command))
        ;; `ewm-frame-select' bound to a swipe yields no action instead of an error.
        (should-not (ewm--frame-select-action 'swipe-4-up)))
    (define-key ewm-mode-map (kbd "s-<f9>") nil)
    (define-key ewm-mode-map (kbd "s-C-7") nil)))

(ert-deftest ewm-input-tests/xf86-aliases-follow-hardware-bindings ()
  "CamelCase hardware bindings get xkbcommon XF86 aliases, existing translations stay."
  (let ((function-key-map (make-sparse-keymap))
        (ewm-mode-map (make-sparse-keymap)))
    (define-key function-key-map [XF86AudioLowerVolume] [custom-audio-down])
    (define-key ewm-mode-map (kbd "<AudioRaiseVolume>") #'ignore)
    (define-key ewm-mode-map (kbd "<AudioLowerVolume>") #'ignore)
    (define-key ewm-mode-map (kbd "<left>") #'ignore)
    (ewm-input--install-xf86-key-aliases)
    (should (equal (lookup-key function-key-map [XF86AudioRaiseVolume])
                   [AudioRaiseVolume]))
    (should (equal (lookup-key function-key-map [XF86AudioLowerVolume])
                   [custom-audio-down]))
    (should-not (lookup-key function-key-map [XF86left]))))

(ert-deftest ewm-input-tests/send-intercept-keys-has-runtime-dispatch-contract ()
  "The actual module payload keeps M-x keyboard-routed and media keys on their action."
  (let ((ewm-mode-map (make-sparse-keymap))
        captured)
    (keymap-set ewm-mode-map "M-x" (ewm-key :intercept))
    (keymap-set ewm-mode-map "C-u" (ewm-key :intercept))
    (keymap-set ewm-mode-map "<AudioLowerVolume>" (ewm-key #'ewm-volume-lower :fullscreen :repeat))
    (keymap-set ewm-mode-map "<Print>" (ewm-key :intercept :fullscreen))
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
      (should (equal (plist-get mx :dispatch) 'keyboard))
      (should cu)
      (should (equal (plist-get cu :dispatch) 'keyboard))
      (should print)
      (should (eq (plist-get print :fullscreen) t))
      (should audio)
      (should (= (length audio) 1))
      (dolist (spec audio)
        (should (eq (car (plist-get spec :dispatch)) 'action))
        (should (eq (plist-get spec :fullscreen) t))
        (should (eq (plist-get spec :repeat) t))))))

(ert-deftest ewm-input-tests/mode-map-edit-resends-once-idle ()
  "Binding keys in `ewm-mode-map' resends the list once, when Emacs is idle."
  (let ((ewm-mode-map (make-sparse-keymap)) (sent 0))
    (cl-letf (((symbol-function 'ewm-intercept-keys-module)
               (lambda (_) (cl-incf sent))))
      (unwind-protect
          (progn
            (advice-add 'define-key :after #'ewm-input--on-define-key)
            (define-key ewm-mode-map (kbd "s-q") #'kill-buffer)
            (define-key ewm-mode-map (kbd "s-z") #'kill-buffer)
            (should (= sent 0))
            (ert-run-idle-timers)
            (should (= sent 1)))
        (advice-remove 'define-key #'ewm-input--on-define-key)
        (cancel-function-timers #'ewm--send-intercept-keys)
        (setq ewm-input--resend-timer nil)))))

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

(ert-deftest ewm-input-tests/intercepted-commands-queue-in-order-without-touching-focus ()
  "Pipe events append to input already waiting, capture and layout are left to the command."
  (let ((ewm-input--keyboard-capture-token 42)
        (unread-command-events (list (cons t ?a)))
        calls)
    (cl-letf (((symbol-function 'ewm-keyboard-capture-end-module)
               (lambda (token) (push token calls)))
              ((symbol-function 'ewm-keyboard-redirect-capture-clear-module)
               (lambda () (push :clear calls))))
      (ewm-input-tests--should-not-refresh-layout
        (ewm--handle-intercepted-command '((key . "s-2")))
        (ewm--handle-intercepted-command '((key . "s-3")))))
    (should (equal unread-command-events '((t . ?a) (t . ?\s-2) (t . ?\s-3))))
    (should-not calls)
    (should (equal ewm-input--keyboard-capture-token 42))))

;;;; keyboard capture sync

(ert-deftest ewm-input-tests/keyboard-capture-follows-command-loop-reads ()
  "Capture begins for a prefix, a modal read or a keyboard-dispatched command in
an Emacs frame, ends with the read, never borrows an app's focus from the
background, and never asks the layout to refresh."
  (pcase-dolist (`(,case ,token ,command ,menu ,focused ,composing ,keys ,action ,calls ,after)
                 '((an-inactive-sync-clears-the-redirect nil ignore nil 2 nil "" sync (:clear-redirect) nil)
                   (a-focus-change-keeps-capture nil ewm--focus-change nil 2 nil "" sync nil nil)
                   (a-prefix-begins-capture nil nil nil 2 nil "C-x" sync ((:begin "command-loop")) 42)
                   (a-finished-read-ends-capture 42 nil nil 2 nil "g" sync (:clear-redirect (:end 42)) nil)
                   (a-modal-read-begins-capture nil ignore t 2 nil "" sync ((:begin "command-loop")) 42)
                   (a-background-modal-read-is-denied nil ignore t 3 nil "" sync (:clear-redirect) nil)
                   (a-background-read-key-is-denied nil nil nil 3 nil "" on nil nil)
                   (a-command-key-captures-over-an-app nil execute-extended-command nil 3 nil "M-x" on ((:begin "command-loop")) 42)
                   (a-client-field-composition-is-denied nil execute-extended-command nil 2 2 "M-x" on nil nil)
                   (an-intercepted-command-captures nil ewm-launch-app nil 3 nil "s-d" on ((:begin "command-loop")) 42)
                   (a-click-popup-is-left-alone 42 nil nil 2 nil "" popup nil 42)
                   (leaving-the-read-ends-the-token 42 ignore nil 2 nil "" sync (:clear-redirect (:end 42)) nil)
))
    (ert-info ((symbol-name case))
      (ewm-input-tests--should-not-refresh-layout
        (should (equal (ewm-input-tests--under-capture-state
                        token command menu focused composing keys
                        (lambda ()
                          (pcase action
                            ('sync (ewm-input--sync-keyboard-capture))
                            ('on (ewm-input--keyboard-capture-on))
                            ('popup (cl-letf (((symbol-function 'menu-or-popup-active-p)
                                               (lambda () t)))
                                      (ewm-input--with-keyboard-capture #'ignore))))))
                       (cons calls after)))))))

(provide 'ewm-input-tests)
;;; ewm-input-tests.el ends here
