;;; ewm-text-input-tests.el --- ERT tests for EWM text input -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins the Lisp-side text-input invariants: intercepted text runs the
;; surface buffer's own input method, multi-key sequences compose into one
;; commit with the composing text mirrored as preedit, and keys the input
;; method hands back are typed into the client.
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
(defvar ewm-text-input-tests--replaces nil)
(defvar ewm-text-input-tests--surrounding nil)
(defvar ewm-text-input-tests--preedits nil)

;; The compositor dynamic module isn't loaded in the test harness; stub
;; the module entry points `ewm-text-input' needs.
(defun ewm-im-commit-module (text surface-id)
  (push (cons surface-id text) ewm-text-input-tests--commits))

(defun ewm-im-preedit-module (text surface-id)
  (push (cons surface-id text) ewm-text-input-tests--preedits))

(defun ewm-text-input-replace-module (text surface-id)
  (push (cons surface-id text) ewm-text-input-tests--replaces))

(defun ewm-text-input-surrounding-module ()
  ewm-text-input-tests--surrounding)

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

(defmacro ewm-text-input-tests--with-state (&rest body)
  "Run BODY with isolated text-input globals."
  (declare (indent 0) (debug t))
  `(let ((old-mode ewm-text-input-mode)
         (ewm-text-input-auto-mode nil)
         (ewm--surfaces (make-hash-table))
         (ewm-text-input-activated-hook nil)
         (ewm-text-input-deactivated-hook nil)
         (input-method-activate-hook nil)
         (input-method-deactivate-hook nil)
         (ewm-text-input--client-active nil)
         (ewm-text-input-tests--focused-id nil)
         (ewm-text-input-tests--commits nil)
         (ewm-text-input-tests--forwards nil)
         (ewm-text-input-tests--intercepts nil)
         (ewm-text-input-tests--replaces nil)
         (ewm-text-input-tests--surrounding nil)
         (ewm-text-input-tests--preedits nil)
         (ewm-text-input--raw nil)
         (unread-command-events nil)
         (ewm-edit--pending (make-hash-table :test 'eql)))
     (unwind-protect
         (progn
           (ewm-text-input-mode -1)
           ,@body)
       (ewm-text-input-auto-mode -1)
       (ewm-text-input-mode (if old-mode 1 -1))
       ;; Sweep edit buffers left awaiting an ack.
       (dolist (buf (buffer-list))
         (when (string-prefix-p "*ewm-edit " (buffer-name buf))
           (kill-buffer buf))))))

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

(defun ewm-text-input-tests--key (key &optional keycode)
  "A compositor key event on surface 10: KEY is utf8 text or a keysym."
  `((surface_id . 10) (keycode . ,(or keycode 0))
    ,(if (stringp key) (cons 'utf8 key) (cons 'keysym key))))

;; `--type' drives the real input method through `ewm-text-input--compose', so
;; EVERY assertion below runs genuine quail translation, not a stub. LATER keys
;; are queued the way `ewm--handle-key' queues them and the input method reads
;; them through a wrapped `read-key-sequence' that mirrors preedit before each
;; read. The one thing batch cannot reproduce is quail's interactive candidate
;; loop (overriding-terminal-local-map across blocking reads): a multi-candidate
;; method like chinese-py selects differently here than in a live command loop,
;; so candidate selection is validated live, not here. czech-prog-2 exercises
;; all of EWM's own code (compose, preedit, backspace, hand-back, keycode
;; replay, multibyte commit via é/ě). Candidate choice is quail's, not EWM's.
(defun ewm-text-input-tests--type (event &rest later)
  "Run the surface input method over compositor EVENT then LATER keys.
LATER are queued as `ewm--handle-key' would while composing. The input method
reads them, and preedit is mirrored before each read as redisplay would."
  (ewm--handle-key event)
  (let ((ewm-text-input--composing (alist-get 'surface_id event)))
    (mapc #'ewm--handle-key later))
  (let* ((last-input-event (pop unread-command-events))
         (inhibit-message t)
         (read (symbol-function 'read-key-sequence)))
    (cl-letf (((symbol-function 'read-key-sequence)
               (lambda (&rest args)
                 (run-hook-with-args 'pre-redisplay-functions (selected-window))
                 (apply read args))))
      (ewm-text-input--compose))))

(defun ewm-text-input-tests--czech (buffer)
  "Activate the Czech programmer input method in BUFFER."
  (with-current-buffer buffer
    (activate-input-method "czech-prog-2")))

(ert-deftest ewm-text-input-tests/multi-key-sequence-commits-once ()
  "The surface's quail sequence shows its keys as preedit and commits one char.
The surface buffer's own change hook, which commits insertions when Emacs
holds the keyboard, must not see the method's scratch text."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface caller)
      (puthash 10 surface ewm--surfaces)
      (ewm-text-input-tests--czech surface)
      (with-current-buffer surface
        (add-hook 'after-change-functions
                  (lambda (&rest _) (push (cons 10 "hook") ewm-text-input-tests--commits))
                  nil t))
      (with-current-buffer caller
        (ewm-text-input-tests--type (ewm-text-input-tests--key "+")
                                    (ewm-text-input-tests--key "+")
                                    (ewm-text-input-tests--key "e")))
      (should (equal (reverse ewm-text-input-tests--preedits)
                     '((10 . "+") (10 . "++"))))
      (should (equal ewm-text-input-tests--commits '((10 . "ě"))))
      (should-not unread-command-events)
      (should-not ewm-text-input--raw))))

(ert-deftest ewm-text-input-tests/stale-buffer-text-does-not-leak-into-preedit ()
  "Scratch text a cut-short composition left behind is cleared before the next."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (with-current-buffer surface (insert "junk"))
      (ewm-text-input-tests--czech surface)
      (ewm-text-input-tests--type (ewm-text-input-tests--key "+")
                                  (ewm-text-input-tests--key "e"))
      (should (equal ewm-text-input-tests--commits '((10 . "é"))))
      (should-not (seq-find (lambda (p) (string-match-p "junk" (cdr p)))
                            ewm-text-input-tests--preedits)))))

(ert-deftest ewm-text-input-tests/backspace-feeds-the-input-method ()
  "Backspace while composing edits the sequence instead of reaching the client."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (ewm-text-input-tests--czech surface)
      (ewm-text-input-tests--type (ewm-text-input-tests--key "+")
                                  (ewm-text-input-tests--key "+")
                                  (ewm-text-input-tests--key #xff08)
                                  (ewm-text-input-tests--key "e"))
      (should (equal ewm-text-input-tests--commits '((10 . "é"))))
      (should-not ewm-text-input-tests--forwards))))

(ert-deftest ewm-text-input-tests/keys-the-input-method-hands-back-are-typed ()
  "Text ending a sequence goes through the method again, other keys are replayed."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (ewm-text-input-tests--czech surface)
      (ewm-text-input-tests--type (ewm-text-input-tests--key "+")
                                  (ewm-text-input-tests--key "x")
                                  (ewm-text-input-tests--key #xff0d 36))
      (should (equal (reverse ewm-text-input-tests--commits)
                     '((10 . "+") (10 . "x"))))
      (should (equal ewm-text-input-tests--forwards '((10 36 nil nil nil nil))))
      (should-not unread-command-events))))

(ert-deftest ewm-text-input-tests/composing-keys-are-queued-for-the-input-method ()
  "While the input method reads, the filter queues keys as Emacs events.
A keysym Emacs cannot name is dropped mid-composition, not forwarded; and a
drained abort leaves the raw queue intact so later keys still replay by keycode."
  (ewm-text-input-tests--with-state
    (let ((ewm-text-input--composing 10))
      (ewm--handle-key (ewm-text-input-tests--key #xff08))
      (ewm--handle-key '((surface_id . 10) (utf8 . "f") (ctrl . t)))
      (ewm--handle-key (ewm-text-input-tests--key "e"))
      ;; F1 (#xffbe) is not in the keysym table and has no utf8: unnameable.
      (ewm--handle-key (ewm-text-input-tests--key #xffbe 67))
      (should (equal unread-command-events
                     '((no-record . backspace) (no-record . ?\C-f) (no-record . ?e))))
      ;; The raw compositor events are kept in order so hand-backs replay by
      ;; keycode; the unnameable F1 is dropped, not queued.
      (should (equal ewm-text-input--raw
                     (list (ewm-text-input-tests--key #xff08)
                           '((surface_id . 10) (utf8 . "f") (ctrl . t))
                           (ewm-text-input-tests--key "e"))))
      (should-not ewm-text-input-tests--forwards)
      ;; An abort the method pushes back must not consume the raw queue: the
      ;; backspace queued after it still forwards by keycode.
      (push '(no-record . ewm-text-input-abort) unread-command-events)
      (ewm-text-input--drain 10)
      (should (member '(10 0 nil nil nil nil) ewm-text-input-tests--forwards))
      (should-not ewm-text-input--raw))))

(ert-deftest ewm-text-input-tests/burst-keys-wait-for-the-first ()
  "Keys arriving before the command loop ran are read by the first key's method."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (ewm-text-input-tests--czech surface)
      (ewm--handle-key (ewm-text-input-tests--key "+"))
      (ewm--handle-key (ewm-text-input-tests--key "e"))
      (should (equal unread-command-events
                     '((ewm-text-input-key 10 ?+) (no-record . ?e))))
      (let ((last-input-event (pop unread-command-events))
            (inhibit-message t))
        (ewm-text-input--compose))
      (should (equal ewm-text-input-tests--commits '((10 . "é"))))
      (should-not unread-command-events))))

(ert-deftest ewm-text-input-tests/deactivation-drops-the-composition ()
  "A field going away stops the input method and clears its preedit."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (ewm-text-input-tests--czech surface)
      ;; Quail's first read is the key itself, so deactivate on the second.
      (cl-letf* ((reads 0)
                 (read (symbol-function 'read-key-sequence))
                 ((symbol-function 'read-key-sequence)
                  (lambda (&rest args)
                    (when (> (cl-incf reads) 1)
                      (ewm--handle-text-input-deactivated))
                    (apply read args))))
        (ewm-text-input-tests--type (ewm-text-input-tests--key "+")))
      (should-not ewm-text-input-tests--commits)
      (should (equal (reverse ewm-text-input-tests--preedits)
                     '((10 . "+") (10 . ""))))
      (should-not unread-command-events))))

(ert-deftest ewm-text-input-tests/handle-key-does-not-use-caller-input-method ()
  "A surface with no input method must not inherit the caller buffer's method."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface caller)
      (puthash 10 surface ewm--surfaces)
      (ewm-text-input-tests--czech caller)
      (with-current-buffer caller
        (ewm-text-input-tests--type (ewm-text-input-tests--key "+")))
      (should (equal ewm-text-input-tests--commits '((10 . "+"))))
      (should-not ewm-text-input-tests--preedits))))

(ert-deftest ewm-text-input-tests/handle-key-without-surface-buffer-forwards ()
  "A key for a surface with no buffer is handed back to the compositor."
  (ewm-text-input-tests--with-state
    (ewm--handle-key (ewm-text-input-tests--key "a" 38))
    (should-not unread-command-events)
    (should (equal ewm-text-input-tests--forwards
                   '((10 38 nil nil nil nil))))))

(ert-deftest ewm-text-input-tests/handle-key-forwards-non-text-key ()
  "A key with no utf8 (Return, arrows) is forwarded, not committed."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (ewm--handle-key (ewm-text-input-tests--key #xff0d 36))
      (should-not unread-command-events)
      (should (equal ewm-text-input-tests--forwards
                     '((10 36 nil nil nil nil)))))))

(ert-deftest ewm-text-input-tests/handle-key-forwards-control-text ()
  "Ctrl-modified text is a shortcut, not text to commit."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (ewm--handle-key '((surface_id . 10) (keycode . 54)
                         (utf8 . "c") (ctrl . t)))
      (should-not unread-command-events)
      (should (equal ewm-text-input-tests--forwards
                     '((10 54 t nil nil nil)))))))

(ert-deftest ewm-text-input-tests/auto-mode-intercepts-only-with-input-method ()
  "Interception follows both client activation and an active input method."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (setq ewm-text-input-tests--focused-id 10)
      (ewm-text-input-auto-mode 1)
      ;; Active field but no input method: indicator only, no interception.
      (ewm--handle-text-input-activated)
      (should-not ewm-text-input-mode)
      (should (equal ewm-text-input-tests--intercepts '(nil nil)))
      (should (eq (key-binding (kbd "a")) 'self-insert-command))
      ;; Input method appears: interception turns on.
      (with-current-buffer surface
        (setq-local current-input-method "surface-im"))
      (ewm-text-input--refresh-auto-state)
      (should ewm-text-input-mode)
      (should (equal ewm-text-input-tests--intercepts '(t nil nil)))
      ;; Field deactivates: interception turns off.
      (ewm--handle-text-input-deactivated)
      (should-not ewm-text-input-mode)
      (ewm-text-input-auto-mode -1)
      (should-not ewm-text-input-mode)
      (should (equal ewm-text-input-tests--intercepts '(nil nil t nil nil))))))

(ert-deftest ewm-text-input-tests/auto-mode-ignores-ambient-input-method ()
  "Interception tracks the focused surface's input method, not an ambient one."
  (ewm-text-input-tests--with-state
    (ewm-text-input-tests--with-buffers (surface)
      (puthash 10 surface ewm--surfaces)
      (setq ewm-text-input-tests--focused-id 10)
      (ewm-text-input-auto-mode 1)
      ;; Surface has no input method, but the buffer we activate from does.
      (with-temp-buffer
        (setq-local current-input-method "ambient-im")
        (ewm--handle-text-input-activated))
      (should-not ewm-text-input-mode)
      (should (equal ewm-text-input-tests--intercepts '(nil nil))))))

(ert-deftest ewm-text-input-tests/ewm-edit-refuses-without-surrounding ()
  "`ewm-edit' errors cleanly when no field reports surrounding text."
  (ewm-text-input-tests--with-state
    (setq ewm-text-input-tests--surrounding nil)
    (should-error (ewm-edit) :type 'user-error)))

(ert-deftest ewm-text-input-tests/ewm-edit-finish-replaces-edited-text ()
  "Finishing sends the replace and keeps the buffer until the compositor acks."
  (ewm-text-input-tests--with-state
    (cl-letf (((symbol-function 'set-window-configuration) (lambda (&rest _) nil)))
      (setq ewm-text-input-tests--surrounding '(10 "old" 3 3))
      (ewm-edit)
      (with-current-buffer (ewm-edit--buffer-name 10)
        (should (string= (buffer-string) "old"))
        (should ewm-edit-mode)
        (erase-buffer)
        (insert "new text")
        (ewm-edit-finish))
      (should (equal ewm-text-input-tests--replaces '((10 . "new text"))))
      (should (buffer-live-p (get-buffer (ewm-edit--buffer-name 10))))
      (ewm--handle-text-input-replaced '((surface_id . 10)))
      (should-not (get-buffer (ewm-edit--buffer-name 10))))))

(ert-deftest ewm-text-input-tests/ewm-edit-dropped-replace-preserves-buffer ()
  "A dropped replace re-surfaces the edit buffer instead of losing the edit."
  (ewm-text-input-tests--with-state
    (cl-letf (((symbol-function 'set-window-configuration) (lambda (&rest _) nil))
              ((symbol-function 'pop-to-buffer) (lambda (&rest _) nil)))
      (setq ewm-text-input-tests--surrounding '(10 "old" 3 3))
      (ewm-edit)
      (with-current-buffer (ewm-edit--buffer-name 10)
        (erase-buffer)
        (insert "kept")
        (ewm-edit-finish))
      (ewm--handle-text-input-replace-dropped '((surface_id . 10)))
      (let ((buf (get-buffer (ewm-edit--buffer-name 10))))
        (should (buffer-live-p buf))
        (should (string= (with-current-buffer buf (buffer-string)) "kept"))))))

(ert-deftest ewm-text-input-tests/ewm-edit-survives-major-mode-change ()
  "Switching major mode in the edit buffer keeps the edit committable."
  (ewm-text-input-tests--with-state
    (cl-letf (((symbol-function 'set-window-configuration) (lambda (&rest _) nil)))
      (setq ewm-text-input-tests--surrounding '(10 "old" 3 3))
      (ewm-edit)
      (with-current-buffer (ewm-edit--buffer-name 10)
        (text-mode)                      ; triggers kill-all-local-variables
        (should ewm-edit-mode)           ; permanent-local kept the mode on
        (should (eq ewm-edit--surface-id 10))
        (erase-buffer)
        (insert "changed")
        (ewm-edit-finish))
      (should (equal ewm-text-input-tests--replaces '((10 . "changed")))))))

(ert-deftest ewm-text-input-tests/ewm-edit-reinvoke-survives-stale-ack ()
  "Re-editing a field with a pending replace survives the prior replace's ack."
  (ewm-text-input-tests--with-state
    (cl-letf (((symbol-function 'set-window-configuration) (lambda (&rest _) nil)))
      (setq ewm-text-input-tests--surrounding '(10 "old" 3 3))
      (ewm-edit)
      (with-current-buffer (ewm-edit--buffer-name 10)
        (erase-buffer) (insert "first") (ewm-edit-finish))
      ;; Re-edit the same field before its ack; the buffer is reused.
      (ewm-edit)
      (with-current-buffer (ewm-edit--buffer-name 10)
        (erase-buffer) (insert "second"))
      ;; The prior replace's ack must not kill the re-opened edit.
      (ewm--handle-text-input-replaced '((surface_id . 10)))
      (let ((buf (get-buffer (ewm-edit--buffer-name 10))))
        (should (buffer-live-p buf))
        (should (string= (with-current-buffer buf (buffer-string)) "second"))))))

(ert-deftest ewm-text-input-tests/ewm-edit-cancel-leaves-field ()
  "Cancelling closes the buffer without replacing the field."
  (ewm-text-input-tests--with-state
    (cl-letf (((symbol-function 'set-window-configuration) (lambda (&rest _) nil)))
      (setq ewm-text-input-tests--surrounding '(10 "old" 3 3))
      (ewm-edit)
      (with-current-buffer (ewm-edit--buffer-name 10)
        (erase-buffer)
        (insert "discard me")
        (ewm-edit-cancel))
      (should-not ewm-text-input-tests--replaces)
      (should-not (get-buffer (ewm-edit--buffer-name 10))))))

(provide 'ewm-text-input-tests)
;;; ewm-text-input-tests.el ends here
