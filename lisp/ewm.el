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

(unless (featurep 'ewm-core)
  (let ((path (getenv "EWM_MODULE_PATH")))
    (if (and path (file-exists-p path))
        (module-load path)
      (require 'ewm-core))))

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
  "Emacs Wayland Manager - Wayland apps as Emacs buffers."
  :link '(emacs-library-link :tag "Library Source" "ewm.el")
  :group 'environment
  :prefix "ewm-")

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
  :width     - desired width in pixels
  :height    - desired height in pixels
  :refresh   - desired refresh rate in Hz (optional)
  :x         - horizontal position (optional)
  :y         - vertical position (optional)
  :scale     - fractional scale, e.g. 1.5 (optional)
  :transform - transform as integer (optional):
               0=Normal 1=90 2=180 3=270
               4=Flipped 5=Flipped90 6=Flipped180 7=Flipped270
  :enabled   - whether output is enabled (default t)

Configuration is stored in the compositor and re-applied when outputs
reconnect (hot-plug), so values persist across connect/disconnect cycles.

Example:
  \\='((\"DP-1\" :width 2560 :height 1440 :scale 1.5)
    (\"eDP-1\" :width 1920 :height 1200 :x 0 :y 0 :transform 0))"
  :type '(alist :key-type string :value-type plist)
  :set (lambda (sym val)
         (set-default sym val)
         (when (fboundp 'ewm--apply-output-config)
           (ewm--apply-output-config)))
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
      ("output_config_changed" (ewm--handle-output-config-changed event))
      ("output_disconnected" (ewm--handle-output-disconnected event))
      ("outputs_complete" (ewm--handle-outputs-complete))
      ("ready" (ewm--handle-ready))
      ("text-input-activated" (ewm--handle-text-input-activated))
      ("text-input-deactivated" (ewm--handle-text-input-deactivated))
      ("key" (ewm--handle-key event))
      ("state" (ewm--handle-state event))
      ("working_area" (ewm--handle-working-area event))
      ("selection-changed" (ewm--handle-selection-changed event)))))

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
          (pop-to-buffer-same-window buf)))
      ;; Explicitly focus the new surface.
      ;; We can't rely on layout sync because with-selected-frame restores
      ;; the previous frame selection, so selected-window won't be this buffer.
      (ewm-focus id))))

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
Selects the window displaying the surface's buffer, or displays it if hidden."
  (pcase-let (((map ("id" id)) event))
    ;; Select window unless minibuffer is active
    (unless (ewm--minibuffer-active-p)
      (when-let* ((buf (gethash id ewm--surfaces))
                  ((buffer-live-p buf)))
        (let ((win (get-buffer-window buf t)))
          (if win
              ;; Use select-frame instead of select-frame-set-input-focus
              ;; to avoid triggering xdg_activation which would steal focus
              ;; back to Emacs. The compositor already set focus correctly.
              (progn
                (select-frame (window-frame win))
                (select-window win))
            ;; Buffer not visible - display it
            (pop-to-buffer-same-window buf)))))))

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

(defun ewm--handle-output-config-changed (event)
  "Handle output config changed EVENT.
Runs `ewm-output-config-changed-hook' with the applied configuration."
  (run-hook-with-args 'ewm-output-config-changed-hook event))

(defvar ewm-output-config-changed-hook nil
  "Hook run when output configuration is applied.
Each function receives the event alist with keys:
  \"name\", \"width\", \"height\", \"refresh\", \"x\", \"y\", \"scale\", \"transform\".")

(defun ewm--handle-output-disconnected (event)
  "Handle output disconnected EVENT.
Deletes the frame for that output. Surface buffers remain in Emacs but are
hidden by the compositor since no windows display them."
  (pcase-let (((map ("name" name)) event))
    (when-let ((frame (ewm--frame-for-output name)))
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

(defun ewm--handle-working-area (event)
  "Handle working area change EVENT.
Called when layer-shell surfaces (panels) claim exclusive zones,
changing the available area for Emacs frames."
  (pcase-let (((map ("output" output) ("x" x) ("y" y)
                     ("width" width) ("height" height)) event))
    (message "Working area for %s: %dx%d+%d+%d" output width height x y)
    ;; Frame resize happens automatically via Wayland configure event.
    ;; Layout refresh is triggered by window-size-change-functions when
    ;; the frame actually resizes. Don't call ewm-layout--refresh directly
    ;; here to avoid focus loops during startup when multiple working area
    ;; events arrive in quick succession.
    ))

;;; Clipboard integration

(defvar ewm--last-selection nil
  "Last selection text received from compositor, to avoid echo.")

(defvar ewm--saved-interprogram-cut-function nil
  "Saved value of `interprogram-cut-function' before EWM override.")

(defun ewm--handle-selection-changed (event)
  "Handle selection-changed EVENT from compositor.
Push the text onto the kill ring."
  (let ((text (map-elt event "text")))
    (when (and text (not (string-empty-p text))
                (not (equal text (car kill-ring))))
      (setq ewm--last-selection text)
      (kill-new text))))

(defun ewm--interprogram-cut-function (text)
  "Send TEXT to Wayland clipboard via compositor."
  (when (and (ewm--compositor-active-p)
             (not (equal text ewm--last-selection)))
    (setq ewm--last-selection text)
    (ewm-set-selection-module text)))

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

(defun ewm-output-layout (output surfaces)
  "Set declarative layout for OUTPUT.
SURFACES is a vector of plists with :id :x :y :w :h :active keys.
Coordinates are relative to the output's working area."
  (ewm-output-layout-module output surfaces))

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

(defun ewm-debug-mode (&optional enable)
  "Toggle compositor debug mode for verbose logging.
With prefix arg, ENABLE if positive, disable if zero or negative.
Without prefix arg, toggles the current state.

When debug mode is enabled:
- Focus changes are logged with source tracking
- Command queue contents are shown in state dump
- More verbose trace logging is active

Check `journalctl --user -t ewm -f' to see debug output."
  (interactive "P")
  (let ((new-state
         (if enable
             (ewm-debug-mode-module (> (prefix-numeric-value enable) 0))
           (ewm-debug-mode-module nil))))
    (message "EWM debug mode: %s" (if new-state "ENABLED" "disabled"))))

(defun ewm-configure-output (name &rest args)
  "Configure output NAME with ARGS.
ARGS is a plist with optional keys:
  :x :y       - position in global coordinate space
  :width :height :refresh - video mode
  :scale      - fractional scale (e.g. 1.5)
  :transform  - transform integer (0=Normal, 1=90, 2=180, 3=270, ...)
  :enabled    - t or nil to enable/disable the output"
  (ewm-configure-output-module
   name
   (plist-get args :x)
   (plist-get args :y)
   (plist-get args :width)
   (plist-get args :height)
   (plist-get args :refresh)
   (plist-get args :scale)
   (plist-get args :transform)
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
           (y (plist-get props :y))
           (scale (plist-get props :scale))
           (transform (plist-get props :transform)))
      (when (or width height scale transform x y)
        (ewm-configure-output name
                              :width width
                              :height height
                              :refresh refresh
                              :x x
                              :y y
                              :scale scale
                              :transform transform)))))

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

(defun ewm--kill-emacs-hook ()
  "Stop compositor gracefully before Emacs exits.
Ensures the compositor thread is cleanly shut down when Emacs terminates."
  (when ewm--module-mode
    (ewm-stop-module)))

;;; Process spawning with activation tokens

(defun ewm--inject-activation-token (orig-fun &rest args)
  "Advice to inject XDG_ACTIVATION_TOKEN into spawned processes.
This allows spawned GUI applications to request focus via xdg_activation."
  (if (and ewm-mode (fboundp 'ewm-create-activation-token))
      (let ((token (ewm-create-activation-token)))
        (if token
            (let ((process-environment
                   (cons (format "XDG_ACTIVATION_TOKEN=%s" token)
                         (cons (format "DESKTOP_STARTUP_ID=%s" token)
                               process-environment))))
              (apply orig-fun args))
          ;; Token creation failed, proceed without it
          (apply orig-fun args)))
    ;; EWM not active, proceed normally
    (apply orig-fun args)))

(defconst ewm--process-functions '(start-process make-process)
  "Process-spawning functions to advise for activation token injection.
Only async functions need tokens — `call-process' is synchronous and
used for CLI tools (git, grep, etc.) that never consume activation tokens.")

(defun ewm--enable-process-advice ()
  "Enable automatic activation token injection for spawned processes."
  (dolist (fn ewm--process-functions)
    (advice-add fn :around #'ewm--inject-activation-token)))

(defun ewm--disable-process-advice ()
  "Disable automatic activation token injection."
  (dolist (fn ewm--process-functions)
    (advice-remove fn #'ewm--inject-activation-token)))

;;; Global minor mode

(defun ewm--mode-enable ()
  "Enable EWM integration."
  (ewm--disable-csd)
  (ewm--enable-layout-sync)
  (ewm-input--enable)
  (ewm--send-intercept-keys)
  (ewm--send-xkb-config)
  (ewm-text-input-auto-mode-enable)
  (ewm--enable-process-advice)
  (setq ewm--saved-interprogram-cut-function interprogram-cut-function)
  (setq interprogram-cut-function #'ewm--interprogram-cut-function)
  (add-hook 'after-make-frame-functions #'ewm--on-make-frame)
  (add-hook 'kill-emacs-hook #'ewm--kill-emacs-hook)
  ;; Resend intercept keys after startup to catch late-loaded bindings
  (unless after-init-time
    (add-hook 'emacs-startup-hook #'ewm--send-intercept-keys)))

(defun ewm--mode-disable ()
  "Disable EWM integration."
  (ewm--disable-layout-sync)
  (ewm-input--disable)
  (ewm-text-input-auto-mode-disable)
  (ewm--disable-process-advice)
  (setq interprogram-cut-function ewm--saved-interprogram-cut-function)
  (remove-hook 'after-make-frame-functions #'ewm--on-make-frame)
  (remove-hook 'kill-emacs-hook #'ewm--kill-emacs-hook)
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
