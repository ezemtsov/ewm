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

(ewm-test--stub ewm-layout (_frames) nil)
(ewm-test--stub ewm-get-strip-frame-ids (_output) nil)
(ewm-test--stub ewm-get-frame-output (surface-id)
  (seq-some (lambda (frame)
              (and (eql (frame-parameter frame 'ewm-surface-id) surface-id)
                   (frame-parameter frame 'ewm-output)))
            (frame-list)))
(ewm-test--stub ewm-init-event-channel (_process) nil)
(defvar ewm-test--queued nil "Events the stubbed `ewm-drain-events' hands out.")
(ewm-test--stub ewm-drain-events () (prog1 ewm-test--queued (setq ewm-test--queued nil)))
(ewm-test--stub ewm-focus-target-module (_focus-id) nil)
(ewm-test--stub ewm-prepare-frame-close-module (_surface-id) nil)
(ewm-test--stub ewm-intercept-keys-module (_keys) nil)
(ewm-test--stub ewm-configure-module (_config) nil)
(ewm-test--stub ewm-notify-initialized-module () nil)
(ewm-test--stub ewm-gtk-set-style (_css) nil)
(ewm-test--stub ewm-keyboard-capture-begin-module (_reason) 1)
(ewm-test--stub ewm-keyboard-capture-end-module (_token) t)
(ewm-test--stub ewm-keyboard-redirect-capture-clear-module () nil)
(ewm-test--stub ewm-im-commit-module (_text _surface-id) nil)
(ewm-test--stub ewm-im-preedit-module (_text _surface-id) nil)
(ewm-test--stub ewm-text-input-intercept-module (_enabled) nil)
(ewm-test--stub ewm-configure-text-input-module (_provider) nil)
(ewm-test--stub ewm-get-focused-id () nil)
(ewm-test--stub ewm-get-focused-frame-id () 0)
(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
(require 'ewm)
(require 'cus-edit)
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

(ert-deftest ewm-event-tests/no-event-is-dropped ()
  "Every variant the module can hand out reaches one handler with its body."
  (let* (;; Layout notifications have no Lisp side yet, tiling ignores maximize.
         (unhandled '(layouts layout-switched maximize_request unmaximize_request))
         (handlers (cons 'ewm--gtk-frame-updates
                         (remq 'ewm--handle-event
                               (apropos-internal "\\`ewm--handle-[a-z-]+\\'" #'fboundp))))
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
            (let ((kind (car (ensure-list event))))
              (ert-info ((symbol-name kind))
                (if (memq kind unhandled)
                    (should-not calls)
                  (should (= (length calls) 1))
                  (should (equal (cadar calls) (cdr (ensure-list event)))))))))
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

(ert-deftest ewm-event-tests/close-of-a-frame-deletes-it ()
  "A close naming a frame's own surface deletes the frame."
  (let* ((frame (selected-frame))
         (surface-id (frame-parameter frame 'ewm-surface-id))
         (ewm--surfaces (make-hash-table :test 'eql))
         deleted)
    (cl-letf (((symbol-function 'delete-frame)
               (lambda (f &optional _force) (push f deleted))))
      (unwind-protect
          (progn
            (set-frame-parameter frame 'ewm-surface-id 5)
            (ewm--handle-close-surface '((id . 5)))
            (should (equal deleted (list frame))))
        (set-frame-parameter frame 'ewm-surface-id surface-id)))))

(ert-deftest ewm-event-tests/filler-becomes-the-users-once-it-shows-more ()
  "The layout declares a filler compositor-owned until it shows more than it was made with."
  (let* ((frame (selected-frame))
         (window (frame-root-window frame))
         (owner (frame-parameter frame 'ewm-owner))
         (prev (window-prev-buffers window)))
    (unwind-protect
        (progn
          (set-frame-parameter frame 'ewm-owner 'compositor)
          (set-window-prev-buffers window nil)
          (should (eq (plist-get (ewm-layout--build-frame-plist frame nil) :owner)
                      'compositor))
          (set-window-prev-buffers window (list (list (current-buffer) 1 1)))
          (should-not (plist-get (ewm-layout--build-frame-plist frame nil) :owner))
          (should (eq (frame-parameter frame 'ewm-owner) 'user)))
      (set-frame-parameter frame 'ewm-owner owner)
      (set-window-prev-buffers window prev))))

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

(ert-deftest ewm-event-tests/defaults-match-custom-types ()
  "Every ewm option's default matches its Customize type."
  (should-not
   (seq-remove (lambda (sym)
                 (widget-apply (widget-convert (custom-variable-type sym))
                               :match (custom--standard-value sym)))
               (apropos-internal "\\`ewm-" #'custom-variable-p))))

(ert-deftest ewm-event-tests/setup-resends-every-option ()
  "Startup sends whatever the option setters send."
  (let* ((options (seq-remove (lambda (sym) (memq (get sym 'custom-set) '(nil custom-set-minor-mode)))
                              (apropos-internal "\\`ewm-" #'custom-variable-p)))
         (module-functions (apropos-internal "\\`ewm-\\(set-\\|.*-module\\'\\)" #'fboundp))
         (ewm--module-mode t)
         calls)
    (unwind-protect
        (progn
          (dolist (fn module-functions)
            (advice-add fn :before (lambda (&rest _) (cl-pushnew fn calls)) `((name . ,fn))))
          (dolist (sym options)
            (funcall (get sym 'custom-set) sym (default-value sym)))
          (let ((by-setters calls))
            (setq calls nil)
            (ewm--setup)
            (should-not (cl-set-difference by-setters calls))))
      (ewm--teardown)
      (dolist (fn module-functions)
        (advice-remove fn fn)))))

(ert-deftest ewm-event-tests/rejected-config-section-is-reported ()
  "A section the compositor rejects is reported once by name, nothing is resent."
  (let ((ewm--module-mode t) sent warnings)
    (cl-letf (((symbol-function 'ewm-configure-module)
               (lambda (config) (push config sent) '(("input" "unknown field `tapp'"))))
              ((symbol-function 'display-warning)
               (lambda (_type message &rest _) (push message warnings))))
      (ewm--send-config))
    (should (= (length sent) 1))
    (should (equal (length warnings) 1))
    (should (string-match-p "input.*tapp" (car warnings)))))

(provide 'ewm-event-tests)
;;; ewm-event-tests.el ends here
