;;; ewm-event-tests.el --- ERT tests for EWM event pipe -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins compositor event-channel behavior. The pipe only wakes the filter,
;; which drains the module's queue; a handler `quit' (normally C-g) must not
;; escape as an Emacs process-filter error.
;;
;; Run:
;;   emacs -Q -batch -L lisp -l tests/ewm-event-tests.el \
;;         -f ert-run-tests-batch-and-exit

;;; Code:

(require 'ert)
(require 'cl-lib)

(setq load-prefer-newer t)

;; The compositor dynamic module isn't loaded in the test harness; stub
;; the module entry points the libs would otherwise pull in.
(defmacro ewm-test--stub (name args &rest body)
  `(unless (fboundp ',name) (defun ,name ,args ,@body)))

(ewm-test--stub ewm-output-layout (_output _frames) nil)
(ewm-test--stub ewm-init-event-channel (_process) nil)
(defvar ewm-test--queued nil "Events the stubbed `ewm-drain-events' hands out.")
(ewm-test--stub ewm-drain-events () (prog1 ewm-test--queued (setq ewm-test--queued nil)))
(ewm-test--stub ewm-focus-target-module (_focus-id) nil)
(ewm-test--stub ewm-prepare-frame-close-module (_surface-id) nil)
(ewm-test--stub ewm-intercept-keys-module (_keys) nil)
(ewm-test--stub ewm-configure-idle-module (_secs _cmd) nil)
(ewm-test--stub ewm-configure-cursor-module (_theme _size) nil)
(ewm-test--stub ewm-configure-cursor-hide-module (_timeout _when-typing) nil)
(ewm-test--stub ewm-set-focus-follows-mouse (_enabled) nil)
(ewm-test--stub ewm-set-unfocused-alpha (_alpha) nil)
(ewm-test--stub ewm-set-animations-enabled (_enabled) nil)
(ewm-test--stub ewm-configure-overview-module (_zoom _r _g _b) nil)
(ewm-test--stub ewm-configure-blur-module (_on _passes _offset _noise _saturation) nil)
(ewm-test--stub ewm-notify-initialized-module () nil)
(ewm-test--stub ewm-gtk-set-style (_css) nil)
(ewm-test--stub ewm-configure-input-module (_config) nil)
(ewm-test--stub ewm-keyboard-capture-begin-module (_reason) 1)
(ewm-test--stub ewm-keyboard-capture-end-module (_token) t)
(ewm-test--stub ewm-keyboard-redirect-capture-clear-module () nil)
(ewm-test--stub ewm-im-commit-module (_text _surface-id) nil)
(ewm-test--stub ewm-text-input-intercept-module (_enabled) nil)
(ewm-test--stub ewm-get-focused-id () nil)
(ewm-test--stub ewm-get-focused-frame-id () 0)
(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
(require 'ewm)
;; One of each event, printed by the module's serializer.
(load (expand-file-name "fixtures/every-event"
                        (file-name-directory (or load-file-name buffer-file-name)))
      nil t)

(ert-deftest ewm-event-tests/activation-shows-hidden-surface-buffer ()
  "Activating a surface no window shows puts its buffer back in the layout."
  (let ((ewm--surfaces (make-hash-table :test 'eql))
        (ewm--module-mode nil)
        (buf (generate-new-buffer " *ewm-test-activation*")))
    (unwind-protect
        (save-window-excursion
          (puthash 42 buf ewm--surfaces)
          (with-current-buffer buf
            (ewm-surface-mode)
            (setq-local ewm-surface-id 42))
          (ewm--handle-event '(activate_surface (id . 42)))
          (should (eq (window-buffer (selected-window)) buf)))
      (kill-buffer buf))))

(ert-deftest ewm-event-tests/every-event-reaches-its-handler ()
  "Every variant the module can hand out dispatches to its handler with its body."
  (let* ((table '((new . ewm--handle-new-surface)
                  (mapped . ewm--handle-mapped-surface)
                  (close . ewm--handle-close-surface)
                  (minimize . ewm--handle-minimize-surface)
                  (activate_surface . ewm--handle-activate-surface)
                  (title . ewm--handle-title-update)
                  (focus . ewm--handle-focus)
                  (output_detected . ewm--handle-output-detected)
                  (output_config_changed . ewm--handle-output-config-changed)
                  (output_disconnected . ewm--handle-output-disconnected)
                  (outputs_complete . ewm--handle-outputs-complete)
                  (ready . ewm--handle-ready)
                  (text-input-activated . ewm--handle-text-input-activated)
                  (text-input-deactivated . ewm--handle-text-input-deactivated)
                  (text-input-replaced . ewm--handle-text-input-replaced)
                  (text-input-replace-dropped . ewm--handle-text-input-replace-dropped)
                  (key . ewm--handle-key)
                  (intercepted-command . ewm--handle-intercepted-command)
                  (working_area . ewm--handle-working-area)
                  (selection-changed . ewm--handle-selection-changed)
                  (activate_workspace . ewm--handle-activate-workspace)
                  (maximize_request . ewm--handle-maximize-request)
                  (unmaximize_request . ewm--handle-unmaximize-request)
                  (bell . ewm--handle-bell)
                  (idle_state_changed . ewm--handle-idle-state-changed)
                  (environment . ewm--handle-environment)
                  (frame_shown . ewm--gtk-frame-updates)
                  (frame_hidden . ewm--gtk-frame-updates)
                  (frame_moved . ewm--handle-frame-moved)
                  (overview . ewm--handle-overview)))
         ;; Layout notifications have no Lisp side yet.
         (unhandled '(layouts layout-switched))
         (handlers (delete-dups (mapcar #'cdr table)))
         (kinds (mapcar (lambda (event) (car (ensure-list event))) ewm-test-every-event))
         calls)
    (should-not (cl-set-difference unhandled kinds))
    (unwind-protect
        (progn
          (dolist (handler handlers)
            (advice-add handler :override
                        (lambda (&rest args) (push (cons handler args) calls))
                        `((name . ,handler))))
          (dolist (event ewm-test-every-event)
            (setq calls nil)
            (ewm--handle-event event)
            (let* ((kind (car (ensure-list event)))
                   (body (cdr (ensure-list event)))
                   (expected (alist-get kind table)))
              (if (memq kind unhandled)
                  (should-not calls)
                (should (= (length calls) 1))
                (should (eq (caar calls) expected))
                (should (equal (cadar calls) body))
                (pcase kind
                  ('frame_shown (should (eq (caddar calls) #'ewm-gtk-thaw-frame-updates)))
                  ('frame_hidden (should (eq (caddar calls) #'ewm-gtk-freeze-frame-updates))))))))
      (dolist (handler handlers)
        (advice-remove handler handler)))))

(ert-deftest ewm-event-tests/filter-consumes-handler-quit ()
  "A handler `quit' must not escape the compositor process filter."
  (let ((ewm-test--queued '(ready))
        called)
    (cl-letf (((symbol-function 'ewm--handle-event)
               (lambda (_event)
                 (setq called t)
                 (signal 'quit nil))))
      (ewm--event-filter nil "\0"))
    (should called)
    (should (null ewm-test--queued))))

(ert-deftest ewm-event-tests/direct-mode-enable-publishes-after-ready-event ()
  "Direct `(ewm-mode 1)' publishes mode state after the ready event."
  (let ((events nil)
        (original-mode (default-value 'ewm-mode))
        (original-global-minor-modes
         (when (boundp 'global-minor-modes) global-minor-modes))
        (original-switch-frame (lookup-key special-event-map [switch-frame]))
        (original-cut-function interprogram-cut-function)
        (original-paste-function interprogram-paste-function)
        (process-environment (copy-sequence process-environment))
        (ewm--module-mode nil)
        (ewm--compositor-ready nil)
        (ewm--event-process nil)
        (ewm-mode-hook nil))
    (cl-letf (((symbol-function 'ewm-running) (lambda () nil))
              ((symbol-function 'ewm-start) (lambda (_theme _size) t))
              ((symbol-function 'accept-process-output)
               (lambda (&rest _)
                 (push (list 'poll ewm-mode ewm--compositor-ready) events)
                 (setq ewm-test--queued '(ready))
                 (ewm--event-filter nil "\0")
                 t))
              ((symbol-function 'run-with-idle-timer) (lambda (&rest _) nil)))
      (unwind-protect
          (progn
            (setq-default ewm-mode nil)
            (setq ewm-mode-hook
                  (list (lambda ()
                          (push (list 'mode ewm-mode ewm--compositor-ready)
                                events))))
            (ewm-mode 1)
            (should (default-value 'ewm-mode))
            (should (equal (nreverse events)
                           '((poll nil nil) (mode t t)))))
        (ewm--destroy-event-channel)
        (ewm--teardown)
        (setq-default ewm-mode original-mode)
        (when (boundp 'global-minor-modes)
          (setq global-minor-modes original-global-minor-modes))
        (define-key special-event-map [switch-frame] original-switch-frame)
        (setq interprogram-cut-function original-cut-function)
        (setq interprogram-paste-function original-paste-function)
        (setq ewm--event-process nil)
        (setq ewm--module-mode nil)
        (setq ewm--compositor-ready nil)))))

(ert-deftest ewm-event-tests/bell-resolves-surface-buffer ()
  "A bell passes its live surface buffer to `ewm-bell-function', else nil."
  (let ((ewm--surfaces (make-hash-table :test 'eql))
        rung)
    (with-temp-buffer
      (puthash 10 (current-buffer) ewm--surfaces)
      (let ((ewm-bell-function (lambda (source) (push source rung))))
        (ewm--handle-event '(bell (id . 10)))
        (ewm--handle-event '(bell (id . 11)))
        (ewm--handle-event '(bell))
        (ewm--handle-event 'bell))
      (let ((ewm-bell-function nil))
        (ewm--handle-event '(bell (id . 10))))
      (should (equal rung (list nil nil nil (current-buffer)))))))

(provide 'ewm-event-tests)
;;; ewm-event-tests.el ends here
