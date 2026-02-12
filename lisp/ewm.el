;;; ewm.el --- Emacs Wayland Manager -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;; Package-Requires: ((emacs "28.1") (transient "0.4"))

;;; Commentary:

;; EWM integrates Emacs with a Wayland compositor, providing an EXWM-like
;; experience without the single-threaded limitations.
;;
;; Usage: M-x ewm-start-module
;;   Starts the compositor as a thread within Emacs.
;;
;; Start apps inside the compositor:
;;   WAYLAND_DISPLAY=wayland-ewm foot
;;
;; Surfaces automatically align with the Emacs window displaying their buffer.
;;
;; Input handling (like EXWM):
;;   When viewing a surface buffer, typing goes directly to the surface.
;;   Prefix keys (C-x, M-x, etc.) are intercepted and go to Emacs.

;;; Code:

(require 'cl-lib)
(require 'map)

;; Module function declarations (provided by ewm-core dynamic module)
(declare-function ewm-start "ewm-core")
(declare-function ewm-stop "ewm-core")
(declare-function ewm-running "ewm-core")
(declare-function ewm-pop-event "ewm-core")
(declare-function ewm-layout-module "ewm-core")
(declare-function ewm-views-module "ewm-core")
(declare-function ewm-hide-module "ewm-core")
(declare-function ewm-close-module "ewm-core")
(declare-function ewm-focus-module "ewm-core")
(declare-function ewm-warp-pointer-module "ewm-core")
(declare-function ewm-screenshot-module "ewm-core")
(declare-function ewm-assign-output-module "ewm-core")
(declare-function ewm-prepare-frame-module "ewm-core")
(declare-function ewm-configure-output-module "ewm-core")
(declare-function ewm-get-focused-id "ewm-core")
(declare-function ewm-get-output-offset "ewm-core")
(declare-function ewm-get-state-module "ewm-core")

;;; Dynamic module loading

(defconst ewm--module-path
  (let* ((lisp-dir (file-name-directory (or load-file-name "")))
         (project-dir (file-name-directory (directory-file-name lisp-dir))))
    (expand-file-name "compositor/target/debug/libewm_core.so" project-dir))
  "Path to ewm-core module (debug build relative to ewm.el).")

(defun ewm-load-module ()
  "Load the ewm-core dynamic module.
Tries debug build relative to ewm.el, then EWM_MODULE_PATH env var."
  (interactive)
  (if (featurep 'ewm-core)
      (message "ewm-core already loaded")
    (let ((path (if (file-exists-p ewm--module-path)
                    ewm--module-path
                  (getenv "EWM_MODULE_PATH"))))
      (if (and path (file-exists-p path))
          (condition-case err
              (progn
                (module-load path)
                (message "Loaded ewm-core from %s" path)
                t)
            (error
             (message "Failed to load ewm-core: %s" (error-message-string err))
             nil))
        (message "Module not found at %s" (or path ewm--module-path))
        nil))))

;;; Module mode (compositor runs in-process)

(defvar ewm--module-mode nil
  "Non-nil when running in module mode (compositor in-process).")

(defvar ewm--compositor-ready nil
  "Non-nil when compositor has signaled it is ready.")

(defun ewm--compositor-active-p ()
  "Return non-nil if compositor is active."
  ewm--module-mode)

(defun ewm--sigusr1-handler ()
  "Handle SIGUSR1 signal from compositor.
The compositor sends this signal when events are queued."
  (interactive)
  (ewm--process-pending-events))

(defun ewm--enable-signal-handler ()
  "Enable SIGUSR1 handler for compositor events."
  (define-key special-event-map [sigusr1] #'ewm--sigusr1-handler))

(defun ewm--disable-signal-handler ()
  "Disable SIGUSR1 handler."
  (define-key special-event-map [sigusr1] nil))

(defun ewm--process-pending-events ()
  "Process all pending module events synchronously.
Called by SIGUSR1 handler when compositor queues events."
  (when (and ewm--module-mode
             (fboundp 'ewm-running)
             (ewm-running))
    (while-let ((event (ewm-pop-event)))
      (ewm--handle-event event))))

(defgroup ewm nil
  "Emacs Wayland Manager."
  :group 'environment)

(defcustom ewm-mouse-follows-focus t
  "Whether the mouse pointer follows focus changes.
When non-nil, warps the pointer to the center of the focused window."
  :type 'boolean
  :group 'ewm)

(defvar ewm--mff-last-window nil
  "Last window for mouse-follows-focus, to avoid redundant warps.")

(defvar ewm--surfaces (make-hash-table :test 'eql)
  "Hash table mapping surface ID to buffer.")

(defvar ewm--pending-frame-outputs nil
  "Alist of (output-name . frame) pairs waiting for surface assignment.
When creating frames, we send prepare-frame to compositor, then make-frame.
Compositor assigns the surface to the output and sends \"new\" event with output.
We match by output name to find the corresponding frame.")

(defvar ewm--pending-output-for-next-frame nil
  "Output name for the next frame being created.
Set this before calling `make-frame' to have the on-make-frame hook
register the frame as pending for that output instead of deleting it.")

(defcustom ewm-output-config nil
  "Output configuration alist.
Each entry is (OUTPUT-NAME . PLIST) where PLIST can contain:
  :width    - desired width in pixels
  :height   - desired height in pixels
  :refresh  - desired refresh rate in Hz (optional)
  :x        - horizontal position (optional)
  :y        - vertical position (optional)
  :enabled  - whether output is enabled (default t)

Example:
  \\='((\"DP-1\" :width 2560 :height 1440)
    (\"eDP-1\" :width 1920 :height 1200 :x 0 :y 0))"
  :type '(alist :key-type string :value-type plist)
  :group 'ewm)

;;; Protocol

(defun ewm--handle-event (event)
  "Handle EVENT from compositor (an alist with string keys)."
  (let ((type (map-elt event "event")))
    (pcase type
      ("new" (ewm--handle-new-surface event))
      ("close" (ewm--handle-close-surface event))
      ("title" (ewm--handle-title-update event))
      ("focus" (ewm--handle-focus event))
      ("output_detected" (ewm--handle-output-detected event))
      ("output_disconnected" (ewm--handle-output-disconnected event))
      ("outputs_complete" (ewm--handle-outputs-complete))
      ("ready" (ewm--handle-ready))
      ("text-input-activated" (ewm--handle-text-input-activated))
      ("text-input-deactivated" (ewm--handle-text-input-deactivated))
      ("key" (ewm--handle-key event))
      ("state" (ewm--handle-state event)))))

;;; Event handlers

(defcustom ewm-manage-focus-new-surface t
  "Whether to automatically focus new surfaces.
When non-nil, new surface buffers are displayed and selected.
Adapted from EXWM's behavior."
  :type 'boolean
  :group 'ewm)

(defun ewm--cleanup-orphan-frames ()
  "Delete frames that have no ewm-output assigned."
  (dolist (f (frame-list))
    (unless (frame-parameter f 'ewm-output)
      (ignore-errors (delete-frame f)))))

(defun ewm--assign-pending-frame (id output pending)
  "Assign surface ID to PENDING frame for OUTPUT."
  (let ((frame (cdr pending)))
    (setq ewm--pending-frame-outputs (delete pending ewm--pending-frame-outputs))
    (set-frame-parameter frame 'ewm-output output)
    (set-frame-parameter frame 'ewm-surface-id id)
    (when (null ewm--pending-frame-outputs)
      (ewm--cleanup-orphan-frames))))

(defun ewm--create-surface-buffer (id app output)
  "Create buffer for regular surface ID with APP on OUTPUT."
  (let ((buf (generate-new-buffer (format "*ewm:%s:%d*" app id))))
    (puthash id buf ewm--surfaces)
    (with-current-buffer buf
      (ewm-surface-mode)
      (setq-local ewm-surface-id id)
      (setq-local ewm-surface-app app))
    ;; Display on target frame if configured
    (when ewm-manage-focus-new-surface
      (let ((target-frame (ewm--frame-for-output output)))
        (if target-frame
            (with-selected-frame target-frame
              (pop-to-buffer-same-window buf))
          (pop-to-buffer-same-window buf))))))

(defun ewm--handle-new-surface (event)
  "Handle new surface EVENT.
If there's a pending frame for this output, this is an Emacs frame.
Otherwise, creates a buffer for external surface."
  (pcase-let (((map ("id" id) ("app" app) ("output" output)) event))
    (let ((pending (and output (assoc output ewm--pending-frame-outputs))))
      (if pending
          (ewm--assign-pending-frame id output pending)
        (ewm--create-surface-buffer id app output)))))

(defun ewm--handle-close-surface (event)
  "Handle close surface EVENT.
Kills the surface buffer."
  (pcase-let (((map ("id" id)) event))
    (when-let ((buf (gethash id ewm--surfaces)))
      (when (buffer-live-p buf)
        (with-current-buffer buf
          (remove-hook 'kill-buffer-query-functions
                       #'ewm--kill-buffer-query-function t))
        (kill-buffer buf))
      (remhash id ewm--surfaces))))

(defun ewm--handle-focus (event)
  "Handle focus EVENT from compositor.
Selects the window displaying the surface's buffer."
  (pcase-let (((map ("id" id)) event))
    ;; Select window unless minibuffer is active
    (unless (ewm--minibuffer-active-p)
      (when-let* ((buf (gethash id ewm--surfaces))
                  ((buffer-live-p buf))
                  (win (get-buffer-window buf t)))
        (select-frame-set-input-focus (window-frame win))
        (select-window win)))))

(defcustom ewm-update-title-hook nil
  "Normal hook run when a surface's title is updated.
Similar to `exwm-update-title-hook'.
The current buffer is the surface buffer when this runs."
  :type 'hook
  :group 'ewm)

(defun ewm--handle-title-update (event)
  "Handle title update EVENT.
Updates buffer-local variables and renames the buffer."
  (pcase-let (((map ("id" id) ("app" app) ("title" title)) event))
    (when-let ((buf (gethash id ewm--surfaces)))
      (when (buffer-live-p buf)
        (with-current-buffer buf
          (setq-local ewm-surface-app app)
          (setq-local ewm-surface-title title)
          (ewm--rename-buffer)
          (run-hooks 'ewm-update-title-hook))))))

(defun ewm--handle-output-detected (event)
  "Handle output detected EVENT. Creates a frame if needed."
  (pcase-let (((map ("name" name)) event))
    (unless (ewm--frame-for-output name)
      (ewm--create-frame-for-output name))))

(defun ewm--handle-output-disconnected (event)
  "Handle output disconnected EVENT. Closes its frame."
  (pcase-let (((map ("name" name)) event))
    (when-let ((frame (ewm--frame-for-output name)))
      ;; Move windows to another frame before deletion
      (let ((target-frame (car (cl-remove frame (frame-list)))))
        (when target-frame
          (dolist (window (window-list frame))
            (let ((buf (window-buffer window)))
              (with-selected-frame target-frame
                (switch-to-buffer buf))))))
      (delete-frame frame))))

(defun ewm--rename-buffer ()
  "Rename the current surface buffer based on app and title.
Similar to `exwm-workspace-rename-buffer'."
  (let* ((app (or ewm-surface-app "unknown"))
         (title (or ewm-surface-title ""))
         ;; Use title if available, otherwise just app
         (basename (if (string-empty-p title)
                       (format "ewm:%s" app)
                     (format "ewm:%s" title)))
         (name (format "*%s*" basename))
         (counter 1))
    ;; Handle name conflicts by adding <N> suffix
    (while (and (get-buffer name)
                (not (eq (get-buffer name) (current-buffer))))
      (setq name (format "*%s<%d>*" basename (cl-incf counter))))
    (rename-buffer name)))

(defun ewm--handle-outputs-complete ()
  "Handle outputs_complete event.
Triggered after compositor sends all output_detected events.
Applies user output config and enforces frame-output parity."
  (ewm--apply-output-config)
  (ewm--enforce-frame-output-parity))

(defun ewm--handle-ready ()
  "Handle ready event from compositor.
Signals that the compositor is fully initialized."
  (setq ewm--compositor-ready t))

(defun ewm--handle-state (event)
  "Handle state event from compositor.
Displays the compositor state in a buffer for debugging."
  (let ((json (map-elt event "json")))
    (with-current-buffer (get-buffer-create "*ewm-state*")
      (let ((inhibit-read-only t))
        (erase-buffer)
        (insert json)
        (goto-char (point-min)))
      (when (fboundp 'js-json-mode) (js-json-mode))
      (display-buffer (current-buffer)))))

;;; Commands

(defun ewm-layout (id x y w h)
  "Set surface ID position to X Y and size to W H."
  (ewm-layout-module id x y w h))

(defun ewm-views (id views)
  "Set surface ID to display at multiple VIEWS.
VIEWS is a vector of plists with :x :y :w :h :active keys.
The :active view receives input, others are visual copies."
  (ewm-views-module id views))

(defun ewm-hide (id)
  "Hide surface ID (move offscreen)."
  (ewm-hide-module id))

(defun ewm-close (id)
  "Request surface ID to close gracefully."
  (ewm-close-module id))

(defun ewm-focus (id)
  "Focus surface ID."
  (ewm-focus-module id))

(defun ewm-warp-pointer (x y)
  "Warp pointer to absolute position X, Y."
  (ewm-warp-pointer-module (float x) (float y)))

(defun ewm-screenshot (&optional path)
  "Take a screenshot of the compositor."
  (interactive)
  (ewm-screenshot-module (or path "/tmp/ewm-screenshot.png")))

(defun ewm-show-state ()
  "Request compositor state dump.
State will be displayed in *ewm-state* buffer when received."
  (interactive)
  (ewm-get-state-module)
  (message "Requested compositor state..."))

(defun ewm-configure-output (name &rest args)
  "Configure output NAME with ARGS.
ARGS is a plist with optional keys:
  :x :y - position in global coordinate space
  :enabled - t or nil to enable/disable the output"
  (ewm-configure-output-module
   name
   (plist-get args :x)
   (plist-get args :y)
   (plist-get args :width)
   (plist-get args :height)
   (plist-get args :refresh)
   (if (plist-member args :enabled)
       (plist-get args :enabled)
     :unset)))

(defun ewm-assign-output (id output)
  "Assign surface ID to OUTPUT."
  (ewm-assign-output-module id output))

(defun ewm-prepare-frame (output)
  "Tell compositor to assign next frame to OUTPUT."
  (ewm-prepare-frame-module output))

(defun ewm--get-output-offset (output-name)
  "Return (x . y) offset for OUTPUT-NAME, or (0 . 0) if not found."
  (or (ewm-get-output-offset output-name) '(0 . 0)))

(defun ewm--apply-output-config ()
  "Apply user output configuration from `ewm-output-config'."
  (dolist (config ewm-output-config)
    (let* ((name (car config))
           (props (cdr config))
           (width (plist-get props :width))
           (height (plist-get props :height))
           (refresh (plist-get props :refresh))
           (x (plist-get props :x))
           (y (plist-get props :y)))
      (when (or width height)
        (ewm-configure-output name
                              :width width
                              :height height
                              :refresh refresh
                              :x x
                              :y y)))))

;;; Frame management

(defun ewm--frame-for-output (output-name)
  "Return the frame assigned to OUTPUT-NAME, or nil."
  (cl-find output-name (frame-list)
           :test #'string=
           :key (lambda (f) (frame-parameter f 'ewm-output))))

(defun ewm--create-frame-for-output (output-name)
  "Create a new frame for OUTPUT-NAME.
Sends prepare-frame to compositor and creates a pending frame.
The frame will be fully assigned when the compositor responds."
  (ewm-prepare-frame output-name)
  (setq ewm--pending-output-for-next-frame output-name)
  ;; Use window-system pgtk for fg-daemon mode (no initial display connection)
  (make-frame '((visibility . t) (window-system . pgtk))))

(defun ewm--on-make-frame (frame)
  "Hook for frame creation. Register pending or delete unauthorized."
  (when ewm-mode
    (cond
     ((frame-parameter frame 'ewm-output)
      nil)
     (ewm--pending-output-for-next-frame
      (push (cons ewm--pending-output-for-next-frame frame)
            ewm--pending-frame-outputs)
      (setq ewm--pending-output-for-next-frame nil))
     (t
      (run-at-time 0 nil
                   (lambda ()
                     (ignore-errors (delete-frame frame))))))))

(defun ewm--enforce-frame-output-parity ()
  "Ensure one frame per output. Delete orphans and duplicates."
  (let ((seen (make-hash-table :test 'equal)))
    (dolist (frame (frame-list))
      (let ((output (frame-parameter frame 'ewm-output)))
        (cond
         ((rassq frame ewm--pending-frame-outputs)
          nil)
         ((null output)
          (ignore-errors (delete-frame frame)))
         ((gethash output seen)
          (ignore-errors (delete-frame frame)))
         (t
          (puthash output frame seen)))))))

;;; Public API

(defun ewm--current-vt ()
  "Return the current VT number, or nil if not on a VT."
  (when-let ((active (ignore-errors
                       (string-trim
                        (with-temp-buffer
                          (insert-file-contents "/sys/class/tty/tty0/active")
                          (buffer-string))))))
    (when (string-match "\\`tty\\([0-9]+\\)\\'" active)
      (string-to-number (match-string 1 active)))))

(defun ewm--disable-csd ()
  "Disable client-side decorations and bars for all frames.
Sets frames to undecorated mode and removes bars since EWM manages windows directly."
  ;; Set current frame to undecorated
  (set-frame-parameter nil 'undecorated t)
  ;; Ensure future frames are also undecorated
  (add-to-list 'default-frame-alist '(undecorated . t))
  ;; Disable menu-bar, tool-bar, and tab-bar if enabled
  ;; These add to the Y-offset and must be accounted for
  (when (bound-and-true-p menu-bar-mode)
    (menu-bar-mode -1))
  (when (bound-and-true-p tool-bar-mode)
    (tool-bar-mode -1))
  (when (bound-and-true-p tab-bar-mode)
    (tab-bar-mode -1)))

;;;###autoload
(defun ewm-start-module ()
  "Start EWM in module mode (compositor runs in-process).
This is the primary entry point for using EWM from `emacs --daemon' on TTY.
The compositor runs as a thread within the Emacs process."
  (interactive)
  ;; Load the module if not already loaded
  (unless (featurep 'ewm-core)
    (unless (ewm-load-module)
      (error "Failed to load ewm-core module")))
  ;; Check if already running
  (when (and (fboundp 'ewm-running) (ewm-running))
    (user-error "EWM compositor is already running"))
  ;; Reset state
  (setq ewm--pending-frame-outputs nil)
  (setq ewm--module-mode nil)
  (setq ewm--compositor-ready nil)
  ;; Start the compositor
  (if (ewm-start)
      (progn
        (setq ewm--module-mode t)
        ;; Enable EWM mode first (needed for frame creation hooks)
        (ewm-mode 1)
        ;; Enable signal handler to receive events
        (ewm--enable-signal-handler)
        ;; Wait for compositor ready event (with timeout)
        (let ((timeout 50))  ; 5 seconds max
          (while (and (> timeout 0)
                      (not ewm--compositor-ready))
            (sleep-for 0.1)
            (ewm--process-pending-events)
            (cl-decf timeout))
          (unless ewm--compositor-ready
            (ewm--disable-signal-handler)
            (ewm-mode -1)
            (setq ewm--module-mode nil)
            (error "Compositor failed to become ready")))
        ;; Set environment for Wayland clients
        (let ((socket-name (format "wayland-ewm-vt%d" (ewm--current-vt))))
          (setenv "WAYLAND_DISPLAY" socket-name)
          (setenv "XDG_SESSION_TYPE" "wayland")
          (setenv "GTK_IM_MODULE" "wayland")
          (setenv "QT_IM_MODULE" "wayland")))
    (error "Failed to start compositor")))

(defun ewm-stop-module ()
  "Stop EWM module mode compositor."
  (interactive)
  (when ewm--module-mode
    (ewm--disable-signal-handler)
    (when (and (fboundp 'ewm-stop) (fboundp 'ewm-running) (ewm-running))
      (ewm-stop)
      (let ((timeout 50))
        (while (and (> timeout 0) (ewm-running))
          (sleep-for 0.1)
          (cl-decf timeout))))
    (setq ewm--module-mode nil)
    (setq ewm--compositor-ready nil)
    (ewm-mode -1)))

;;; Global minor mode

(defun ewm--mode-enable ()
  "Enable EWM integration."
  (ewm--disable-csd)
  (ewm--enable-layout-sync)
  (ewm-input--enable)
  (ewm--send-intercept-keys)
  (ewm--send-xkb-config)
  (ewm-text-input-auto-mode-enable)
  (add-hook 'after-make-frame-functions #'ewm--on-make-frame)
  ;; Resend intercept keys after startup to catch late-loaded bindings
  (unless after-init-time
    (add-hook 'emacs-startup-hook #'ewm--send-intercept-keys)))

(defun ewm--mode-disable ()
  "Disable EWM integration."
  (ewm--disable-layout-sync)
  (ewm-input--disable)
  (ewm-text-input-auto-mode-disable)
  (remove-hook 'after-make-frame-functions #'ewm--on-make-frame)
  ;; Stop module mode if active
  (when ewm--module-mode
    (ewm--disable-signal-handler)
    (when (and (fboundp 'ewm-stop) (fboundp 'ewm-running) (ewm-running))
      (ewm-stop))
    (setq ewm--module-mode nil)))

;;;###autoload
(define-minor-mode ewm-mode
  "Global minor mode for EWM compositor integration."
  :global t
  :lighter " EWM"
  :group 'ewm
  (if ewm-mode
      (ewm--mode-enable)
    (ewm--mode-disable)))

;; Load submodules
(require 'ewm-surface)
(require 'ewm-layout)
(require 'ewm-input)
(require 'ewm-text-input)
(require 'ewm-transient)

(provide 'ewm)
;;; ewm.el ends here
