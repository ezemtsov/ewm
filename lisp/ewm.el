;;; ewm.el --- Emacs Wayland Manager -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;; Package-Requires: ((emacs "29.1"))

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
(require 'ewm-module)
(ewm-module--declare)
(require 'ewm-surface)
(require 'ewm-layout)
(require 'ewm-frame)
(require 'ewm-focus)
(require 'ewm-text-input)
(require 'ewm-commands)
(require 'ewm-input)

(unless (featurep 'ewm-core)
  (let ((path (getenv "EWM_MODULE_PATH")))
    (if (and path (file-exists-p path))
        (module-load path)
      (require 'ewm-core))))

;;; Event channel

(defvar ewm--compositor-ready nil
  "Non-nil when compositor has signaled it is ready.")

(defconst ewm--compositor-ready-timeout 30.0
  "Seconds to wait for the compositor to signal readiness on start.")

(defconst ewm--compositor-stop-timeout 5.0
  "Seconds to wait for the compositor to halt on stop.")

(defconst ewm--compositor-poll-interval 0.1
  "Seconds between polls of the compositor's running flag during shutdown.")

(defvar ewm--event-process nil
  "Pipe process receiving compositor events via open_channel.")

(defvar ewm--draining-events nil
  "Non-nil while `ewm--event-filter' dispatches the queue.")

(defun ewm--event-filter (_proc _output)
  "Dispatch the events the compositor queued; the pipe only carries a wakeup."
  ;; A nested filter (a handler waited for output) must not jump the queue.
  (unless ewm--draining-events
    (let ((ewm--draining-events t))
      (while-let ((events (ewm-drain-events)))
        (dolist (event events)
          (condition-case dispatch-err
              (ewm--handle-event event)
            (quit
             (setq quit-flag nil)
             (ewm--log "Event handler quit while handling %S" (car (ensure-list event))))
            ;; A user-error is expected feedback, not a handler fault.
            (user-error
             (message "%s" (error-message-string dispatch-err)))
            (error
             (message "EWM: event handler error: %s" dispatch-err))))))))

(defun ewm--init-event-channel ()
  "Create a pipe process and register it as the compositor event channel."
  (ewm--destroy-event-channel)
  (with-temp-buffer
    (setq ewm--event-process
          (make-pipe-process :name "ewm-events" :buffer (current-buffer) :noquery t
                             :filter #'ewm--event-filter))
    (set-process-buffer ewm--event-process nil))
  (ewm-init-event-channel ewm--event-process))

(defun ewm--destroy-event-channel ()
  "Tear down the event pipe process."
  (when (and ewm--event-process (process-live-p ewm--event-process))
    (delete-process ewm--event-process))
  (setq ewm--event-process nil))

;;; Options

(defcustom ewm-animations-enabled t
  "Whether cross-frame slide and surface open-fade animations play.
Set to nil to snap in-flight animations to target and skip easing on
new ones.  Toggling is live -- the compositor flips the shared
animation clock and the next frame settles."
  :type 'boolean
  :group 'ewm-appearance
  :set (ewm--send-on-set #'ewm--send-animations-enabled))

(defun ewm--send-animations-enabled ()
  "Send `ewm-animations-enabled' to the compositor."
  (ewm-set-animations-enabled ewm-animations-enabled))

(defcustom ewm-unfocused-alpha 1.0
  "Alpha multiplier for unfocused application toplevels.
A float in 0.0..1.0; 1.0 (default) disables the effect.  Lower values
dim non-focused windows so the focused one stands out.  Emacs frames
and layer surfaces (panels, lock screen) are never dimmed."
  :type 'float
  :group 'ewm-appearance
  :set (ewm--send-on-set #'ewm--send-unfocused-alpha))

(defun ewm--send-unfocused-alpha ()
  "Send `ewm-unfocused-alpha' to the compositor."
  (ewm-set-unfocused-alpha (float ewm-unfocused-alpha)))

(defcustom ewm-overview-zoom 0.5
  "Scale of each output's content in the overview.
A float above 0.0 and at most 0.75; smaller values show more of the strip."
  :type 'float
  :group 'ewm-appearance
  :set (ewm--send-on-set #'ewm--send-overview-config))

(defcustom ewm-overview-backdrop-color "#262626"
  "Color behind the zoomed-out output in the overview.
A color name or #RRGGBB spec; unknown names fall back to the default."
  :type 'color
  :group 'ewm-appearance
  :set (ewm--send-on-set #'ewm--send-overview-config))

(defun ewm--color-rgb (color)
  "COLOR as red, green and blue in 0.0..1.0, or nil for an unknown spec."
  ;; `tty-color-standard-values' parses names and hex specs without a display.
  (when-let* ((rgb (tty-color-standard-values (tty-color-canonicalize color))))
    (mapcar (lambda (c) (/ c 65535.0)) rgb)))

(defun ewm--send-overview-config ()
  "Send overview zoom and backdrop color to the compositor."
  (let ((rgb (or (ewm--color-rgb ewm-overview-backdrop-color)
                 (ewm--color-rgb
                  (eval (car (get 'ewm-overview-backdrop-color 'standard-value)) t)))))
    (apply #'ewm-configure-overview-module (float ewm-overview-zoom) rgb)))

(defcustom ewm-blur t
  "Whether to blur behind surfaces that ask for it.
Clients request blur through the ext-background-effect protocol and
the blurred area follows the region they set."
  :type 'boolean
  :group 'ewm-appearance
  :set (ewm--send-on-set #'ewm--send-blur-config))

(defcustom ewm-blur-passes 3
  "Number of blur passes, 1 to 31. More passes blur further."
  :type 'natnum
  :group 'ewm-appearance
  :set (ewm--send-on-set #'ewm--send-blur-config))

(defcustom ewm-blur-offset 3.0
  "Sampling offset per blur pass, in pixels."
  :type 'float
  :group 'ewm-appearance
  :set (ewm--send-on-set #'ewm--send-blur-config))

(defcustom ewm-blur-noise 0.02
  "Noise added to blurred areas; 0.0 disables it."
  :type 'float
  :group 'ewm-appearance
  :set (ewm--send-on-set #'ewm--send-blur-config))

(defcustom ewm-blur-saturation 1.5
  "Saturation multiplier for blurred areas; 1.0 keeps colors as they are."
  :type 'float
  :group 'ewm-appearance
  :set (ewm--send-on-set #'ewm--send-blur-config))

(defun ewm--send-blur-config ()
  "Send blur settings to the compositor."
  (ewm-configure-blur-module ewm-blur (truncate ewm-blur-passes)
                             (float ewm-blur-offset) (float ewm-blur-noise)
                             (float ewm-blur-saturation)))

(defcustom ewm-idle nil
  "Idle timeout and action.
nil disables idle timeout.  An integer means blank monitors after
that many seconds.  A cons (SECONDS . COMMAND) runs COMMAND as a
shell command after SECONDS of inactivity.

Examples:
  nil          ; disabled
  300          ; blank after 5 minutes
  (300 . \"swaylock -f -c 333333\")  ; lock after 5 minutes"
  :type '(choice (const :tag "Disabled" nil)
                 (integer :tag "Blank after N seconds")
                 (cons :tag "Run command after N seconds"
                       (integer :tag "Seconds")
                       (string :tag "Shell command")))
  :set (ewm--send-on-set #'ewm--send-idle-config)
  :group 'ewm-output)

(defvar ewm-idle-hook nil
  "Hook run when idle state changes.
Called with one argument: t when entering idle, nil when waking.")

(defun ewm--send-idle-config ()
  "Send idle timeout configuration to compositor."
  (pcase ewm-idle
    ('nil (ewm-configure-idle-module nil nil))
    ((pred integerp)
     (ewm-configure-idle-module ewm-idle "blank"))
    (`(,(and (pred integerp) secs) . ,(and (pred stringp) cmd))
     (ewm-configure-idle-module secs cmd))))

(defvar ewm-idle-inhibited nil
  "Whether idle is manually inhibited from Emacs.
When non-nil, the idle timeout and idle notifications (swayidle) are
suppressed.  This overrides all other idle inhibition sources.
Use `ewm-idle-inhibit-mode' to toggle interactively.")

(define-minor-mode ewm-idle-inhibit-mode
  "Inhibit idle timeout and notifications.
While enabled, the screen won't blank and swayidle won't fire regardless
of inactivity.  Useful for video playback or presentations."
  :global t
  :variable ewm-idle-inhibited
  (when ewm--module-mode
    (ewm-set-idle-inhibited ewm-idle-inhibited)))

(define-minor-mode ewm-insomnia-mode
  "Keep the system awake with the lid closed.
The panel turns off, windows are asked to stop drawing and other sleep
requests are refused.  The screen turns sepia as a reminder."
  :global t
  :group 'ewm
  (when ewm--module-mode
    (ewm--send-power-config))
  (when (called-interactively-p 'any)
    (message "Insomnia mode: %s" (if ewm-insomnia-mode "on" "off"))))

(defun ewm--send-power-config ()
  "Send `ewm-insomnia-mode' to the compositor."
  (ewm-configure-power-module (not ewm-insomnia-mode) ewm-insomnia-mode))

(defcustom ewm-cursor-theme nil
  "Xcursor theme used by EWM and clients launched from it.
When nil, EWM picks a loadable theme from the session environment."
  :type '(choice (const :tag "Auto" nil)
                 (string :tag "Theme"))
  :set (ewm--send-on-set #'ewm--send-cursor-config)
  :group 'ewm-appearance)

(defvar ewm--session-cursor-theme (getenv "XCURSOR_THEME")
  "Initial session XCURSOR_THEME value from before EWM applied its config.")

(defcustom ewm-cursor-size
  (or (when-let* ((size (getenv "XCURSOR_SIZE"))
                  (parsed (string-to-number size)))
        (and (> parsed 0) parsed))
      24)
  "Xcursor size used by EWM and clients launched from it."
  :type '(integer :tag "Pixels")
  :set (ewm--send-on-set #'ewm--send-cursor-config)
  :group 'ewm-appearance)

(defun ewm--cursor-theme ()
  "Return the configured EWM cursor theme, or nil for automatic selection."
  (and (stringp ewm-cursor-theme)
       (not (string-empty-p ewm-cursor-theme))
       ewm-cursor-theme))

(defun ewm--cursor-size ()
  "Return the resolved EWM cursor size."
  (if (and (integerp ewm-cursor-size)
           (> ewm-cursor-size 0)
           (<= ewm-cursor-size 255))
      ewm-cursor-size
    24))

(defun ewm--apply-cursor-env ()
  "Apply EWM's cursor config to Emacs child process environment."
  (setenv "XCURSOR_THEME" (or (ewm--cursor-theme)
                              ewm--session-cursor-theme))
  (setenv "XCURSOR_SIZE" (number-to-string (ewm--cursor-size))))

(defun ewm--send-cursor-config ()
  "Send cursor theme and size configuration to compositor."
  (ewm--apply-cursor-env)
  (ewm-configure-cursor-module (ewm--cursor-theme) (ewm--cursor-size)))

(defcustom ewm-cursor-auto-hide 5
  "Hide the mouse cursor after this many seconds of pointer inactivity.
nil disables auto-hide.  Only pointer motion and button events restore
the cursor; keyboard input and scrolling do not.

Examples:
  nil   ; disabled
  5     ; hide after 5 seconds of no pointer activity"
  :type '(choice (const :tag "Disabled" nil)
                 (integer :tag "Hide after N seconds"))
  :set (ewm--send-on-set #'ewm--send-cursor-hide-config)
  :group 'ewm-input-devices)

(defcustom ewm-cursor-hide-when-typing nil
  "When non-nil, hide the mouse cursor on key press.
Pointer motion restores it.  Independent from `ewm-cursor-auto-hide';
both can be enabled together."
  :type 'boolean
  :set (ewm--send-on-set #'ewm--send-cursor-hide-config)
  :group 'ewm-input-devices)

(defun ewm--send-cursor-hide-config ()
  "Send cursor auto-hide configuration to compositor."
  (ewm-configure-cursor-hide-module
   (and (integerp ewm-cursor-auto-hide) ewm-cursor-auto-hide)
   ewm-cursor-hide-when-typing))

;;; Events

(defun ewm--handle-event (event)
  "Dispatch EVENT: a variant symbol, or one consed onto its alist body."
  (let* ((event (ensure-list event))
         (body (cdr event)))
    (pcase (car event)
      ('new (ewm--handle-new-surface body))
      ('mapped (ewm--handle-mapped-surface body))
      ('close (ewm--handle-close-surface body))
      ('minimize (ewm--handle-minimize-surface body))
      ('activate_surface (ewm--handle-activate-surface body))
      ('title (ewm--handle-title-update body))
      ('focus (ewm--handle-focus body))
      ('output_detected (ewm--handle-output-detected body))
      ('output_config_changed (ewm--handle-output-config-changed body))
      ('output_disconnected (ewm--handle-output-disconnected body))
      ('outputs_complete (ewm--handle-outputs-complete))
      ('ready (ewm--handle-ready))
      ('text-input-activated (ewm--handle-text-input-activated))
      ('text-input-deactivated (ewm--handle-text-input-deactivated))
      ('text-input-replaced (ewm--handle-text-input-replaced body))
      ('text-input-replace-dropped (ewm--handle-text-input-replace-dropped body))
      ('key (ewm--handle-key body))
      ('intercepted-command (ewm--handle-intercepted-command body))
      ('working_area (ewm--handle-working-area body))
      ('selection-changed (ewm--handle-selection-changed body))
      ('bell (ewm--handle-bell body))
      ('idle_state_changed (ewm--handle-idle-state-changed body))
      ('environment (ewm--handle-environment body))
      ('frame_shown (ewm--gtk-frame-updates body #'ewm-gtk-thaw-frame-updates))
      ('frame_hidden (ewm--gtk-frame-updates body #'ewm-gtk-freeze-frame-updates))
      ('overview (ewm--handle-overview body))
      ('hide (ewm--handle-hide body)))))

(defun ewm--assign-surface-to-frame (id seq)
  "Pair surface ID with the frame created as SEQ."
  (when-let* ((frame (cl-find seq (frame-list)
                              :key (lambda (f) (frame-parameter f 'ewm-frame-seq)))))
    (set-frame-parameter frame 'ewm-surface-id id)
    ;; PGTK only flushes Cairo to the wl_surface for the selected frame;
    ;; without this the first commit is transparent until something else
    ;; triggers a redisplay (bug#53232).
    (with-selected-frame frame (redisplay t))
    (when (frame-parameter frame 'ewm-focus-on-create)
      (set-frame-parameter frame 'ewm-focus-on-create nil)
      (select-frame-set-input-focus frame))
    (ewm-layout--refresh)
    frame))

(defun ewm--create-surface-buffer (id app pid)
  "Create the buffer for surface ID (APP, client PID); placed later on map."
  (let ((buf (generate-new-buffer (format "*ewm:%s:%d*" app id))))
    (puthash id buf ewm--surfaces)
    (with-current-buffer buf
      (ewm-surface-mode)
      (setq-local ewm-surface-id id)
      (setq-local ewm-surface-app app)
      (setq-local ewm-surface-pid pid)
      (ewm--update-app))))

(defun ewm--handle-new-surface (event)
  "Handle new surface EVENT.
An Emacs frame surface carries the seq of the frame it belongs to and is
paired with it. Any other surface gets a buffer."
  (let* ((id (alist-get 'id event))
         (app (alist-get 'app event))
         (seq (alist-get 'seq event))
         (pid (alist-get 'pid event))
         (frame (and seq (ewm--assign-surface-to-frame id seq))))
    (ewm--log "handle-new-surface: id=%s app=%s seq=%s frame=%s"
              id app seq (or frame "no-match"))
    (unless frame
      (ewm--create-surface-buffer id app pid))))

(defvar ewm--pending-placements nil
  "Deferred `ewm--place-surface-buffer' arg lists, flushed when menus close.")

(defun ewm--handle-mapped-surface (event)
  "Display the surface buffer for EVENT now that it has mapped."
  (ewm--with-surface-buffer-from-event event
    (when-let* ((width (alist-get 'width event))
                (height (alist-get 'height event)))
      (setq-local ewm-surface-size (cons width height)))
    (ewm--place-or-defer (current-buffer)
                         (alist-get 'open_floating event)
                         (alist-get 'output event)
                         (alist-get 'frame_surface_id event)
                         (alist-get 'activate event))))

(defun ewm--place-or-defer (buffer floating output host-id activate)
  "Place BUFFER now, or once the active menu closes."
  (let ((args (list buffer floating output host-id activate)))
    (if (ewm-layout--menu-active-p)
        (push args ewm--pending-placements)
      (apply #'ewm--place-surface-buffer args))))

(defun ewm--flush-pending-placements ()
  "Place surfaces that mapped while a menu was active."
  (let ((pending (nreverse ewm--pending-placements)))
    (setq ewm--pending-placements nil)
    (dolist (args pending)
      (when (buffer-live-p (car args))
        (apply #'ewm--place-surface-buffer args)))))

(defun ewm--place-surface-buffer (buffer floating output host-id activate)
  "Display BUFFER on OUTPUT: floating in a new frame, else tiled in HOST-ID.
A float is selected only when ACTIVATE."
  (let ((host (or (ewm--frame-by-surface-id host-id)
                  (ewm--surface-target-frame-for-output output))))
    (when host (select-frame host))
    (cond
     (floating
      (let ((action `(ewm-display-buffer-floating (ewm-output . ,output))))
        (if activate
            (pop-to-buffer buffer action)
          (display-buffer buffer action))))
     (host
      (pop-to-buffer-same-window buffer)
      (ewm-layout--refresh)))))

(defun ewm--handle-close-surface (event)
  "Kill the surface buffer for EVENT, or delete a frame the compositor dropped."
  (let* ((id (alist-get 'id event))
         (buf (gethash id ewm--surfaces))
         (frame (ewm--frame-by-surface-id id)))
    (ewm--log "handle-close-surface: id=%d kind=%s" id
              (cond (buf "buffer") (frame "frame-orphan") (t "unknown")))
    (cond
     (buf
      (remhash id ewm--surfaces)
      (when (buffer-live-p buf)
        ;; Let Emacs retire single-window floats even with buffer history.
        (dolist (window (get-buffer-window-list buf nil t))
          (when (and (frame-parameter (window-frame window) 'ewm-floating)
                     (not (window-parent window)))
            (set-window-dedicated-p window t)))
        (save-selected-window
          (quit-windows-on buf t))
        ;; Emacs 30 leaves an undisplayed buffer alive.
        (when (buffer-live-p buf) (kill-buffer buf))))
     (frame
      (ignore-errors (delete-frame frame t))))))

(defun ewm--handle-minimize-surface (event)
  "Bury the surface buffer for EVENT so the layout hides it."
  (ewm--with-surface-buffer-from-event event
    (bury-buffer)))

(defun ewm--handle-activate-surface (event)
  "Show the surface named by activation EVENT tiled on the focused output."
  (ewm--with-surface-buffer-from-event event
    (ewm--place-or-defer (current-buffer) nil
                         (ewm--frame-output (ewm--focused-frame))
                         nil t)))

(defcustom ewm-bell-function #'ewm-bell-ding
  "Function called when a client rings the system bell.
It receives the surface buffer, or nil when the bell names no known surface.
The default rings the Emacs bell, so `visible-bell' and `ring-bell-function'
apply; nil ignores the bell."
  :type '(choice (const :tag "Ignore" nil) function)
  :group 'ewm)

(defun ewm-bell-ding (_source)
  "Ring the Emacs bell."
  (ding t))

(defun ewm--handle-bell (event)
  "Call `ewm-bell-function' with the surface buffer named by EVENT, if any."
  (when ewm-bell-function
    (funcall ewm-bell-function (ewm--surface-buffer (alist-get 'id event)))))

(defun ewm--handle-environment (event)
  "Export the variables of EVENT so child processes inherit them."
  (dolist (pair (alist-get 'vars event))
    (setenv (symbol-name (car pair)) (cdr pair))))

(defun ewm--handle-ready ()
  "Note that the compositor is fully initialized."
  (setq ewm--compositor-ready t))

(defun ewm--handle-working-area (event)
  "Log the working area EVENT a panel's exclusive zone produced."
  ;; Frames resize through their own configure, which refreshes the layout.
  (ewm--log "Working area for %s: %dx%d+%d+%d"
            (alist-get 'output event) (alist-get 'width event)
            (alist-get 'height event) (alist-get 'x event)
            (alist-get 'y event)))

(defun ewm--handle-idle-state-changed (event)
  "Handle idle_state_changed EVENT from compositor."
  (let ((idle (alist-get 'idle event)))
    (run-hook-with-args 'ewm-idle-hook idle)))

(defvar ewm-overview-hook nil
  "Functions called with non-nil when the overview opens and nil when it closes.")

(defun ewm--handle-overview (event)
  "Handle overview EVENT from compositor."
  (run-hook-with-args 'ewm-overview-hook (alist-get 'open event)))

(defvar ewm-hide-hook nil
  "Functions called with non-nil when frames slide away and nil when they return.")

(defun ewm--handle-hide (event)
  "Handle hide EVENT from compositor."
  (run-hook-with-args 'ewm-hide-hook (alist-get 'hidden event)))

;;; Surface metadata

(defcustom ewm-update-app-hook nil
  "Normal hook run when a surface's app id is set or changes.
Similar to `exwm-update-class-hook'. The current buffer is the surface
buffer when this runs. See `ewm-surface-emulate-keys' for per-app keys."
  :type 'hook
  :group 'ewm)

(defcustom ewm-update-title-hook nil
  "Normal hook run when a surface's title is updated.
Similar to `exwm-update-title-hook'.
The current buffer is the surface buffer when this runs."
  :type 'hook
  :group 'ewm)

(defun ewm--update-app ()
  "Run `ewm-update-app-hook' from a fresh table, resending keys if one was local."
  (let ((local (local-variable-p 'ewm-surface-emulate-keys)))
    (kill-local-variable 'ewm-surface-emulate-keys)
    (save-current-buffer (run-hooks 'ewm-update-app-hook))
    (when (or local (local-variable-p 'ewm-surface-emulate-keys))
      (ewm--send-intercept-keys))))

(defun ewm--handle-title-update (event)
  "Refresh app/title on the surface buffer for EVENT."
  (let ((app (alist-get 'app event))
        (title (alist-get 'title event)))
    (ewm--with-surface-buffer-from-event event
      ;; No app_id from the client keeps the comm-derived name.
      (let ((app-changed (not (member app (list nil "" ewm-surface-app)))))
        (when app-changed (setq-local ewm-surface-app app))
        (setq-local ewm-surface-title title)
        (ewm--rename-buffer)
        (when app-changed (ewm--update-app))
        (run-hooks 'ewm-update-title-hook)))))

(defun ewm--rename-buffer ()
  "Rename the current surface buffer based on app and title.
Similar to `exwm-workspace-rename-buffer'."
  (let* ((app (or ewm-surface-app "unknown"))
         (title (or ewm-surface-title ""))
         (name (format "*ewm:%s*" (if (string-empty-p title) app title))))
    (rename-buffer name t)))

;;; Shell CWD tracking

(defconst ewm--cwd-ancestor-walk-max 10
  "Maximum depth when walking a shell's /proc PPid chain to find its surface.")

(defun ewm--cwd-ancestors (pid)
  "Return PID and its ancestors on the compositor host."
  (let ((default-directory "/")
        (ancestors nil)
        (current pid)
        (depth 0))
    (while (and current (< depth ewm--cwd-ancestor-walk-max))
      (push current ancestors)
      (let ((ppid (alist-get 'ppid (process-attributes current))))
        (setq current (and ppid (> ppid 1) ppid)))
      (cl-incf depth))
    ancestors))

(defun ewm--cwd-find-buffer (shell-pid)
  "Find the surface buffer whose PID is an ancestor of SHELL-PID."
  (let ((ancestors (ewm--cwd-ancestors shell-pid)))
    (cl-loop for buf being the hash-values of ewm--surfaces
             when (and (buffer-live-p buf)
                       (memq (buffer-local-value 'ewm-surface-pid buf) ancestors))
             return buf)))

(defun ewm--report-cwd (shell-pid cwd)
  "Report that shell with SHELL-PID has changed directory to CWD.
Called by shell hooks via emacsclient."
  (when-let* ((buf (ewm--cwd-find-buffer shell-pid)))
    (with-current-buffer buf
      (setq default-directory (file-name-as-directory cwd)))))

;;; Outputs

(defun ewm--handle-output-detected (event)
  "Record a newly detected output."
  (let ((name (alist-get 'name event)))
    (setf (alist-get name ewm--output-info nil nil #'string=) event)
    (ewm-layout--refresh)))

(defvar ewm-output-config-changed-hook nil
  "Hook run when output configuration is applied.
Each function receives the event alist with symbol keys:
  name, width, height, refresh, x, y, scale, transform.")

(defun ewm--handle-output-config-changed (event)
  "Patch `ewm--output-info' from EVENT and run the config-changed hook."
  (when-let* ((info (alist-get (alist-get 'name event) ewm--output-info nil nil #'string=)))
    (dolist (key '(scale transform x y))
      (setf (alist-get key info) (alist-get key event)))
    (seq-doseq (m (alist-get 'modes info))
      (setf (alist-get 'current m)
            (seq-every-p (lambda (key) (= (alist-get key m) (alist-get key event)))
                         '(width height refresh)))))
  (run-hook-with-args 'ewm-output-config-changed-hook event))

(defun ewm--handle-output-disconnected (event)
  "Record that an output disconnected. The compositor moves its frames."
  (let ((name (alist-get 'name event)))
    (setq ewm--output-info
          (assoc-delete-all name ewm--output-info #'string=))
    (ewm-layout--refresh)))

(defvar ewm--late-frames-replayed nil
  "Non-nil once frames created after the ready event asked for a focus replay.")

(defun ewm--apply-user-config (name thunk)
  "Call THUNK, reporting an error in the NAME option as a warning."
  (condition-case err
      (funcall thunk)
    (error (display-warning 'ewm (format "%s: %s" name (error-message-string err))))))

(defun ewm--handle-outputs-complete ()
  "Apply output config, then create any missing per-output frames."
  (ewm--apply-user-config "ewm-output-config" #'ewm--apply-output-config)
  ;; For pgtk, CSS goes in before any frame is created so the GTK widget
  ;; background is set on the first wl_surface commit.
  (ewm--apply-background)
  (let (created)
    (dolist (output (ewm-get-active-outputs))
      (unless (or (ewm--strip-frames output) (ewm--frame-in-flight-on-p output))
        (ewm--create-frame-for-output output nil '((ewm-owner . compositor)))
        (setq created t)))
    (ewm--delete-stray-frames)
    (ewm-layout--refresh)
    ;; Frames created after the ready event missed the replay `ewm-start-module'
    ;; asks for, and PGTK drops the enter they get before their first roundtrip.
    ;; Once only: the replay costs a leave/enter for whatever holds focus.
    (when (and created ewm--compositor-ready (not ewm--late-frames-replayed))
      (setq ewm--late-frames-replayed t)
      (run-with-idle-timer 0 nil #'ewm-notify-initialized-module))))

;;; Clipboard integration

(defun ewm--handle-selection-changed (event)
  "Handle selection-changed EVENT from compositor.
Push the text onto the kill ring."
  (let ((text (alist-get 'text event)))
    (when (and text (not (string-empty-p text))
               (not (equal text (car kill-ring))))
      (let ((interprogram-cut-function nil)
            (interprogram-paste-function nil))
        (kill-new text)))))

(defun ewm--interprogram-cut-function (text)
  "Send TEXT to Wayland clipboard via compositor."
  (ewm-set-selection-module text))

;;; Strip-visibility freeze/thaw
;;
;; The compositor emits frame_shown/frame_hidden when an emacs frame's
;; strip-active status changes.  We forward to GTK's update-freeze so
;; pgtk's per-tick Cairo paint skips hidden frames.

(defun ewm--gtk-frame-updates (event fn)
  "Call FN with the GTK window id of EVENT's frame, when GTK-backed."
  (when-let* (((ewm--gtk-backed-p))
              (frame (ewm--frame-by-surface-id (alist-get 'id event)))
              (wid (frame-parameter frame 'window-id)))
    (ignore-errors (funcall fn wid))))

;;; Session

(defun ewm--disable-csd ()
  "Undecorate frames and drop bars that would skew the Y-offset."
  (set-frame-parameter nil 'undecorated t)
  (add-to-list 'default-frame-alist '(undecorated . t))
  (when menu-bar-mode (menu-bar-mode -1))
  (when tool-bar-mode (tool-bar-mode -1))
  (when scroll-bar-mode (scroll-bar-mode -1)))

;;;###autoload
(defun ewm-start-module ()
  "Start EWM in module mode (compositor runs in-process).
This is the primary entry point for using EWM from `emacs --daemon' on TTY.
The compositor runs as a thread within the Emacs process."
  (interactive)
  (ewm-mode 1))

(defun ewm--start-compositor ()
  "Start the compositor and wait for readiness, cleaning up on failure."
  (when (ewm-running)
    (user-error "EWM compositor is already running"))
  (setq ewm--module-mode nil
        ewm--compositor-ready nil)
  (let (started)
    (unwind-protect
        (progn
          (ewm--apply-cursor-env)
          ;; Register the event pipe before starting the compositor.
          (ewm--init-event-channel)
          (unless (ewm-start (ewm--cursor-theme) (ewm--cursor-size))
            (error "Failed to start compositor"))
          (setq ewm--module-mode t)
          (ewm--setup)
          (with-timeout (ewm--compositor-ready-timeout
                         (error "Compositor failed to become ready"))
            (while (not ewm--compositor-ready)
              (accept-process-output ewm--event-process)))
          ;; Replay the first keyboard enter after init (PGTK drops it early).
          (if after-init-time
              (run-with-idle-timer 0 nil #'ewm-notify-initialized-module)
            (add-hook 'emacs-startup-hook #'ewm-notify-initialized-module))
          ;; Graphical frames now have the real theme background.
          (ewm--apply-background)
          (setq started t))
      (unless started
        (ewm--stop-compositor)
        (ewm--destroy-event-channel)
        (ewm--teardown)
        (setq ewm--module-mode nil
              ewm--compositor-ready nil)))))

(defun ewm--stop-compositor ()
  "Request graceful compositor shutdown."
  (when (ewm-running)
    (ewm-stop)
    (with-timeout (ewm--compositor-stop-timeout nil)
      (while (ewm-running)
        (sleep-for ewm--compositor-poll-interval)))))

(defun ewm--apply-background (&rest _)
  "Push the default face background to the compositor and GTK."
  (let* ((live (face-attribute 'default :background))
         (bg (and (stringp live) (not (string-prefix-p "unspecified" live)) live)))
    (when-let* ((rgb (and bg (ewm--color-rgb bg))))
      (apply #'ewm-configure-background-module rgb))
    (when (ewm--gtk-backed-p)
      ;; Black before the first graphical frame, when the face is unspecified (bug#53232).
      (condition-case err
          (ewm-gtk-set-style (format "window { background-color: %s; }" (or bg "#000000")))
        (error (message "EWM: ewm-gtk-set-style failed: %s" err))))))

;;; Global minor mode

(defun ewm--setup ()
  "Install Emacs-side state before compositor startup events run."
  (ewm--disable-csd)
  (ewm--enable-layout-sync)
  (add-hook 'ewm-layout-menu-deactivated-hook #'ewm--flush-pending-placements)
  (advice-add 'select-frame-set-input-focus :override
              #'ewm--select-frame-set-input-focus)
  (ewm-input--enable)
  (if after-init-time
      (ewm--send-intercept-keys)
    (add-hook 'after-init-hook #'ewm--send-intercept-keys))
  (ewm--apply-user-config "ewm-input-config" #'ewm--send-input-config)
  (ewm--send-idle-config)
  (ewm--send-power-config)
  (ewm--send-cursor-config)
  (ewm--send-cursor-hide-config)
  (ewm-text-input--send-provider)
  (ewm--send-unfocused-alpha)
  (ewm--send-animations-enabled)
  (setopt ewm-focus-follows-mouse (or ewm-focus-follows-mouse (and mouse-autoselect-window t)))
  (ewm--send-overview-config)
  (ewm--send-blur-config)
  (ewm-text-input-auto-mode 1)
  (ewm--enable-process-advice)
  (add-function :override interprogram-cut-function #'ewm--interprogram-cut-function)
  (add-function :override interprogram-paste-function #'ignore)
  (add-hook 'after-make-frame-functions #'ewm--on-make-frame)
  (add-hook 'enable-theme-functions #'ewm--apply-background)
  (advice-add 'delete-frame :around #'ewm--prevent-frame-deletion)
  (add-hook 'kill-emacs-hook #'ewm--stop-compositor t))

(defun ewm--teardown ()
  "Remove Emacs-side state installed by `ewm--setup'."
  (ewm--disable-layout-sync)
  (remove-hook 'ewm-layout-menu-deactivated-hook #'ewm--flush-pending-placements)
  (advice-remove 'select-frame-set-input-focus
                 #'ewm--select-frame-set-input-focus)
  (when (memq #'ewm-input--sync-keyboard-capture post-command-hook)
    (ewm-input--disable))
  (when ewm-text-input-auto-mode
    (ewm-text-input-auto-mode -1))
  (ewm--disable-process-advice)
  (remove-function interprogram-cut-function #'ewm--interprogram-cut-function)
  (remove-function interprogram-paste-function #'ignore)
  (remove-hook 'after-make-frame-functions #'ewm--on-make-frame)
  (remove-hook 'after-init-hook #'ewm--send-intercept-keys)
  (remove-hook 'emacs-startup-hook #'ewm-notify-initialized-module)
  (remove-hook 'enable-theme-functions #'ewm--apply-background)
  (advice-remove 'delete-frame #'ewm--prevent-frame-deletion)
  (remove-hook 'kill-emacs-hook #'ewm--stop-compositor))

;;;###autoload
(defcustom ewm-mode nil
  "Non-nil when the EWM compositor session is ready.
Use the `ewm-mode' command to change this variable."
  :type 'boolean
  :set #'custom-set-minor-mode
  :initialize #'custom-initialize-default
  :group 'ewm)

;;;###autoload
(define-minor-mode ewm-mode
  "Global minor mode for a ready EWM compositor session."
  :global t
  :lighter " ewm"
  :keymap ewm-mode-map
  :group 'ewm
  ;; The setter completes before Emacs updates mode lists and runs hooks.
  :variable ((default-value 'ewm-mode)
             . (lambda (enabled)
                 (if enabled
                     (unless (default-value 'ewm-mode)
                       (ewm--start-compositor))
                   (when ewm--module-mode
                     (user-error "EWM cannot be disabled; exit Emacs to end the session")))
                 (setq-default ewm-mode enabled))))

;;; Process spawning with activation tokens

(defun ewm--inject-activation-token (orig-fun &rest args)
  "Advice to inject XDG_ACTIVATION_TOKEN into spawned processes.
This allows spawned GUI applications to request focus via xdg_activation."
  (let* ((token (and ewm-mode (ewm-create-activation-token)))
         (process-environment
          (if token
              (cons (format "XDG_ACTIVATION_TOKEN=%s" token)
                    (cons (format "DESKTOP_STARTUP_ID=%s" token)
                          process-environment))
            process-environment)))
    (apply orig-fun args)))

(defun ewm--enable-process-advice ()
  "Enable automatic activation token injection for spawned processes."
  ;; `start-process' and the shell helpers all call `make-process'.
  (advice-add 'make-process :around #'ewm--inject-activation-token))

(defun ewm--disable-process-advice ()
  "Disable automatic activation token injection."
  (advice-remove 'make-process #'ewm--inject-activation-token))

(provide 'ewm)
;;; ewm.el ends here
