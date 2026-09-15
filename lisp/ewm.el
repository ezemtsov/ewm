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

(unless (featurep 'ewm-core)
  (let ((path (getenv "EWM_MODULE_PATH")))
    (if (and path (file-exists-p path))
        (module-load path)
      (require 'ewm-core))))

;; Functions provided by the ewm-core dynamic module
(declare-function ewm-init-event-channel "ewm-core")
(declare-function ewm-drain-events "ewm-core")
(declare-function ewm-set-selection-module "ewm-core")
(declare-function ewm-output-layout-module "ewm-core")
(declare-function ewm-floating-layout-module "ewm-core")
(declare-function ewm-close-module "ewm-core")
(declare-function ewm-warp-pointer-module "ewm-core")
(declare-function ewm-get-debug-state-module "ewm-core")
(declare-function ewm-debug-mode-module "ewm-core")
(declare-function ewm-configure-output-module "ewm-core")
(declare-function ewm-prepare-frame-module "ewm-core")
(declare-function ewm-prepare-floating-frame-module "ewm-core")
(declare-function ewm-prepare-frame-close-module "ewm-core")
(declare-function ewm-move-floating-frame-module "ewm-core")
(declare-function ewm-resize-floating-frame-module "ewm-core")
(declare-function ewm-workspace-rename-module "ewm-core")
(declare-function ewm-workspace-mark-urgent-module "ewm-core")
(declare-function ewm-toggle-fullscreen-module "ewm-core")
(declare-function ewm-get-active-outputs "ewm-core")
(declare-function ewm-get-output-origin "ewm-core")
(declare-function ewm-get-frame-origin "ewm-core")
(declare-function ewm-entry-under-floating-center-module "ewm-core")
(declare-function ewm--find-window-by-entry-id "ewm-focus")
(declare-function ewm-configure-cursor-module "ewm-core")
(declare-function ewm-start "ewm-core" (cursor-theme cursor-size))
(declare-function ewm-list-xdg-apps "ewm-core")
(declare-function ewm-set-focus-follows-mouse "ewm-core")
(declare-function ewm-set-unfocused-alpha "ewm-core")
(declare-function ewm-set-animations-enabled "ewm-core")
(declare-function ewm-notify-initialized-module "ewm-core")
(declare-function ewm-focus-output-direction-module "ewm-core")
(declare-function ewm-get-focused-frame-id "ewm-core")
(declare-function ewm-get-active-frame-id "ewm-core")
(declare-function ewm--handle-intercepted-command "ewm-input" (event))

;;; Logging

(defvar ewm--debug nil
  "Non-nil when EWM debug mode is active.")

(defun ewm--log (format-string &rest args)
  "Log a message to the *ewm-log* buffer when debug mode is active.
FORMAT-STRING and ARGS are passed to `format'."
  (when ewm--debug
    (let ((buf (get-buffer-create "*ewm-log*")))
      (with-current-buffer buf
        (goto-char (point-max))
        (insert (format-time-string "[%H:%M:%S.%3N] ")
                (apply #'format format-string args) "\n")))))

;;; Module mode (compositor runs in-process)

(defvar ewm--module-mode nil
  "Non-nil when running in module mode (compositor in-process).")

(defvar ewm--compositor-ready nil
  "Non-nil when compositor has signaled it is ready.")

(defconst ewm--compositor-ready-timeout 30.0
  "Seconds to wait for the compositor to signal readiness on start.")

(defconst ewm--compositor-stop-timeout 5.0
  "Seconds to wait for the compositor to halt on stop.")

(defconst ewm--compositor-poll-interval 0.1
  "Seconds between polls of the compositor's running flag during shutdown.")

(defun ewm--compositor-active-p ()
  "Return non-nil if compositor is active."
  ewm--module-mode)

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

(defcustom ewm-animations-enabled t
  "Whether cross-frame slide and surface open-fade animations play.
Set to nil to snap in-flight animations to target and skip easing on
new ones.  Toggling is live -- the compositor flips the shared
`AnimationsClock' and the next frame settles."
  :type 'boolean
  :group 'ewm
  :set (lambda (sym val)
         (set-default sym val)
         (when (fboundp 'ewm-set-animations-enabled)
           (ewm-set-animations-enabled val))))

(defvar ewm--mff-last-window nil
  "Last window for mouse-follows-focus, to avoid redundant warps.")

(defcustom ewm-focus-follows-mouse nil
  "Whether moving the mouse pointer should focus applications.
The compositor sends focus events with cursor coordinates; intra-frame
window switching is handled by `ewm--handle-focus' from those events."
  :type 'boolean
  :group 'ewm
  :set (lambda (sym val)
         (set-default sym val)
         (setq focus-follows-mouse (and val t))
         (when (fboundp 'ewm-set-focus-follows-mouse)
           (ewm-set-focus-follows-mouse val))))

(defcustom ewm-unfocused-alpha 1.0
  "Alpha multiplier for unfocused application toplevels.
A float in 0.0..1.0; 1.0 (default) disables the effect.  Lower values
dim non-focused windows so the focused one stands out.  Emacs frames
and layer surfaces (panels, lock screen) are never dimmed."
  :type 'float
  :group 'ewm
  :set (lambda (sym val)
         (set-default sym val)
         (when (fboundp 'ewm-set-unfocused-alpha)
           (ewm-set-unfocused-alpha (float val)))))

(defvar ewm--is-ffm-focus-event nil
  "Whether the current event was triggered by focus follows mouse.
Let-bound in `ewm--handle-focus' to gate the Emacs-window hover branch.")

(defvar ewm--is-pointer-focus-event nil
  "Whether the current focus event came from pointer interaction.")

(defvar ewm--surfaces (make-hash-table :test 'eql)
  "Hash table mapping surface ID to buffer.")

(defvar ewm--strips nil
  "Alist of (OUTPUT-NAME . FRAMES) -- per-output, left-to-right strip order.
Single source of truth for frame ordering on each output's strip.")

(defvar ewm--floating-frames nil
  "Alist of (OUTPUT-NAME . FRAMES) for floating Emacs frames.")

(defvar ewm--frame-seq 0
  "Monotonic counter advanced for each EWM-managed frame creation.
`ewm--assign-surface-to-frame' uses it to pair `new' events with
in-flight frames in creation order.")


(defvar ewm--output-info nil
  "Alist of (CONNECTOR-NAME . EVENT-ALIST) for currently connected outputs.
Each value is the full output_detected event alist, including make, model,
serial, modes, scale, position and physical size.
Updated on output_detected and output_disconnected events.")

(defcustom ewm-output-config nil
  "Output configuration alist.
Each entry is (OUTPUT-NAME . PLIST) where PLIST can contain:
  :width     - desired width in pixels
  :height    - desired height in pixels
  :refresh   - desired refresh rate in Hz (optional)
  :custom    - when non-nil, generate a CVT mode for :width/:height/:refresh
               instead of matching an advertised mode.  Use this for
               resolutions the monitor does not advertise (e.g. a lower
               resolution on a HiDPI panel for games).  :refresh is required.
  :modeline  - explicit X11 modeline string, takes precedence over the mode
               fields when set:
               \"CLOCK HDISP HSS HSE HTOTAL VDISP VSS VSE VTOTAL +hsync +vsync\"
  :x         - horizontal position (optional)
  :y         - vertical position (optional)
  :scale     - fractional scale, e.g. 1.5 (optional)
  :transform - transform as integer (optional):
               0=Normal 1=90 2=180 3=270
               4=Flipped 5=Flipped90 6=Flipped180 7=Flipped270
  :enabled   - whether output is enabled (default t)

Custom modes are not guaranteed to work: the compositor asks the display to
run a mode its manufacturer did not advertise, and the kernel may reject it.

OUTPUT-NAME can be either a connector name (e.g. \"DP-1\") or a
Make-Model-Serial string (e.g. \"Dell U2722D ABC123\").  The MMS form is
preferred because it survives connector re-numbering when cables are
replugged.  Use `ewm-list-outputs' to see both forms for each connected
output and copy a ready-to-use config entry.

Configuration is stored in the compositor and re-applied when outputs
reconnect (hot-plug), so values persist across connect/disconnect cycles.

Example:
  \\='((\"Dell U2722D ABC123\" :width 2560 :height 1440 :scale 1.5)
    (\"eDP-1\" :width 1920 :height 1200 :x 0 :y 0 :transform 0)
    (\"eDP-1\" :width 1024 :height 640 :refresh 60 :custom t))"
  :type '(alist :key-type string :value-type plist)
  :set (lambda (sym val)
         (set-default sym val)
         (when (fboundp 'ewm--apply-output-config)
           (ewm--apply-output-config)))
  :group 'ewm)

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
  :set (lambda (sym val)
         (set-default sym val)
         (when (fboundp 'ewm--send-idle-config)
           (ewm--send-idle-config)))
  :group 'ewm)

(defvar ewm-idle-hook nil
  "Hook run when idle state changes.
Called with one argument: t when entering idle, nil when waking.")

(defcustom ewm-cursor-auto-hide 5
  "Hide the mouse cursor after this many seconds of pointer inactivity.
nil disables auto-hide.  Only pointer motion and button events restore
the cursor; keyboard input and scrolling do not.

Examples:
  nil   ; disabled
  5     ; hide after 5 seconds of no pointer activity"
  :type '(choice (const :tag "Disabled" nil)
                 (integer :tag "Hide after N seconds"))
  :set (lambda (sym val)
         (set-default sym val)
         (when (fboundp 'ewm--send-cursor-hide-config)
           (ewm--send-cursor-hide-config)))
  :group 'ewm)

(defcustom ewm-cursor-theme nil
  "Xcursor theme used by EWM and clients launched from it.
When nil, EWM picks a loadable theme from the session environment."
  :type '(choice (const :tag "Auto" nil)
                 (string :tag "Theme"))
  :set (lambda (sym val)
         (set-default sym val)
         (when (fboundp 'ewm--send-cursor-config)
           (ewm--send-cursor-config)))
  :group 'ewm)

(defvar ewm--session-cursor-theme (getenv "XCURSOR_THEME")
  "Initial session XCURSOR_THEME value from before EWM applied its config.")

(defcustom ewm-cursor-size
  (or (when-let* ((size (getenv "XCURSOR_SIZE"))
                  (parsed (string-to-number size)))
        (and (> parsed 0) parsed))
      24)
  "Xcursor size used by EWM and clients launched from it."
  :type '(integer :tag "Pixels")
  :set (lambda (sym val)
         (set-default sym val)
         (when (fboundp 'ewm--send-cursor-config)
           (ewm--send-cursor-config)))
  :group 'ewm)

(defcustom ewm-cursor-hide-when-typing nil
  "When non-nil, hide the mouse cursor on key press.
Pointer motion restores it.  Independent from `ewm-cursor-auto-hide';
both can be enabled together."
  :type 'boolean
  :set (lambda (sym val)
         (set-default sym val)
         (when (fboundp 'ewm--send-cursor-hide-config)
           (ewm--send-cursor-hide-config)))
  :group 'ewm)

(defvar ewm-idle-inhibited nil
  "Whether idle is manually inhibited from Emacs.
When non-nil, the idle timeout and idle notifications (swayidle) are
suppressed.  This overrides all other idle inhibition sources.
Use `ewm-idle-inhibit-mode' to toggle interactively.")

;; Load submodules (before event handlers that reference them)
(require 'ewm-surface)
(require 'ewm-focus)
(require 'ewm-layout)
(require 'ewm-input)
(require 'ewm-text-input)

;;; Protocol

(defun ewm--handle-event (event)
  "Dispatch EVENT: a variant symbol, or one consed onto its alist body."
  (let* ((event (ensure-list event))
         (body (cdr event)))
    (pcase (car event)
      ('new (ewm--handle-new-surface body))
      ('mapped (ewm--handle-mapped-surface body))
      ('close (ewm--handle-close-surface body))
      ('minimize (ewm--handle-minimize-surface body))
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
      ('activate_workspace (ewm--handle-activate-workspace body))
      ('maximize_request (ewm--handle-maximize-request body))
      ('unmaximize_request (ewm--handle-unmaximize-request body))
      ('bell (ewm--handle-bell body))
      ('idle_state_changed (ewm--handle-idle-state-changed body))
      ('environment (ewm--handle-environment body))
      ('frame_shown (ewm--gtk-frame-updates body #'ewm-gtk-thaw-frame-updates))
      ('frame_hidden (ewm--gtk-frame-updates body #'ewm-gtk-freeze-frame-updates))
      ('frame_moved (ewm--handle-frame-moved body)))))

;;; Event handlers

(defun ewm--assign-surface-to-frame (id output)
  "Pair surface ID with the oldest in-flight frame on OUTPUT.
Picks lowest `ewm-frame-seq' so sids align with PGTK wl_surface creation
order, which `ewm-frame-new' may have reordered in the strip."
  (when-let* ((frame (car (seq-sort-by
                           (lambda (f) (or (frame-parameter f 'ewm-frame-seq) 0))
                           #'<
                           (cl-remove-if-not
                            (lambda (f)
                              (and (ewm--frame-in-flight-p f)
                                   (equal (frame-parameter f 'ewm-output) output)))
                            (frame-list))))))
    (set-frame-parameter frame 'ewm-surface-id id)
    (unless (frame-parameter frame 'ewm-home-output)
      (set-frame-parameter frame 'ewm-home-output output))
    ;; PGTK only flushes Cairo to the wl_surface for the selected frame;
    ;; without this the first commit is transparent until something else
    ;; triggers a redisplay (bug#53232).
    (with-selected-frame frame (redisplay t))
    (when (frame-parameter frame 'ewm-focus-on-create)
      (set-frame-parameter frame 'ewm-focus-on-create nil)
      (select-frame-set-input-focus frame))
    (ewm-layout--refresh)
    frame))

(defun ewm--frame-by-surface-id (surface-id)
  "Return the Emacs frame whose `ewm-surface-id' parameter is SURFACE-ID."
  (and surface-id
       (cl-find surface-id (frame-list)
                :key (lambda (f) (frame-parameter f 'ewm-surface-id)))))

(defun ewm--create-surface-buffer (id app pid)
  "Create the buffer for surface ID (APP, client PID); placed later on map."
  (let ((buf (generate-new-buffer (format "*ewm:%s:%d*" app id))))
    (puthash id buf ewm--surfaces)
    (with-current-buffer buf
      (ewm-surface-mode)
      (setq-local ewm-surface-id id)
      (setq-local ewm-surface-app app)
      (setq-local ewm-surface-pid pid))))

(defun ewm--handle-new-surface (event)
  "Handle new surface EVENT.
An Emacs frame surface carries its output; if an in-flight frame there
matches it is paired, otherwise a surface buffer is created."
  (let ((id (alist-get 'id event))
        (app (alist-get 'app event))
        (output (alist-get 'output event))
        (pid (alist-get 'pid event)))
    (let ((frame (and output (ewm--assign-surface-to-frame id output))))
      (ewm--log "handle-new-surface: id=%s app=%s output=%s frame=%s"
                id app output (or frame "no-match"))
      (unless frame
        (ewm--create-surface-buffer id app pid)))))

(defvar ewm--pending-placements nil
  "Deferred `ewm--place-surface-buffer' arg lists, flushed when menus close.")

(defun ewm--handle-mapped-surface (event)
  "Display the surface buffer for EVENT now that it has mapped."
  (when-let* ((id (alist-get 'id event))
              (buffer (gethash id ewm--surfaces))
              ((buffer-live-p buffer)))
    (when-let* ((width (alist-get 'width event))
                (height (alist-get 'height event)))
      (with-current-buffer buffer
        (setq-local ewm-surface-size (cons width height))))
    (let ((args (list buffer
                      (alist-get 'open_floating event)
                      (alist-get 'output event)
                      (alist-get 'frame_surface_id event))))
      ;; A menu is up: defer so placement neither disrupts nor is dropped by it.
      (if (ewm-layout--menu-active-p)
          (push args ewm--pending-placements)
        (apply #'ewm--place-surface-buffer args)))))

(defun ewm--flush-pending-placements ()
  "Place surfaces that mapped while a menu was active."
  (let ((pending (nreverse ewm--pending-placements)))
    (setq ewm--pending-placements nil)
    (dolist (args pending)
      (when (buffer-live-p (car args))
        (apply #'ewm--place-surface-buffer args)))))

(defun ewm--place-surface-buffer (buffer floating output host-id)
  "Display BUFFER on OUTPUT: floating in a new frame, else tiled in HOST-ID."
  (let ((host (or (ewm--frame-by-surface-id host-id)
                  (ewm--surface-target-frame-for-output output))))
    (when host (select-frame host))
    (cond
     (floating
      (pop-to-buffer buffer `(ewm-display-buffer-floating (ewm-output . ,output))))
     (host
      (pop-to-buffer-same-window buffer)
      (ewm-layout--refresh)))))

(defun ewm--handle-close-surface (event)
  "Handle close surface EVENT.
Quits the surface buffer's windows and kills it, or deletes a frame that died
without lisp asking (e.g. compositor-driven destroy)."
  (let* ((id (alist-get 'id event))
         (buf (gethash id ewm--surfaces))
         (frame (ewm--frame-by-surface-id id)))
    (ewm--log "handle-close-surface: id=%d kind=%s" id
              (cond (buf "buffer") (frame "frame-orphan") (t "unknown")))
    (cond
     (buf
      (remhash id ewm--surfaces)
      (when (buffer-live-p buf)
        (with-current-buffer buf
          (remove-hook 'kill-buffer-query-functions
                       #'ewm--kill-buffer-query-function t))
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

(defun ewm--handle-maximize-request (_event)
  "Handle maximize request EVENT.
No-op: tiling layout already manages window sizes."
  nil)

(defun ewm--handle-unmaximize-request (_event)
  "Handle unmaximize request EVENT.
No-op: tiling layout has no maximized state to restore."
  nil)

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
    (let ((buf (gethash (alist-get 'id event) ewm--surfaces)))
      (funcall ewm-bell-function (and (buffer-live-p buf) buf)))))

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

(defcustom ewm-update-title-hook nil
  "Normal hook run when a surface's title is updated.
Similar to `exwm-update-title-hook'.
The current buffer is the surface buffer when this runs."
  :type 'hook
  :group 'ewm)

(defun ewm--handle-title-update (event)
  "Refresh app/title on the surface buffer for EVENT."
  (let ((app (alist-get 'app event))
        (title (alist-get 'title event)))
    (ewm--with-surface-buffer-from-event event
      ;; No app_id from the client; keep the comm-derived name.
      (unless (member app '(nil ""))
        (setq-local ewm-surface-app app))
      (setq-local ewm-surface-title title)
      (ewm--rename-buffer)
      (run-hooks 'ewm-update-title-hook))))

(defun ewm--handle-output-detected (event)
  "Record a newly detected output."
  (let ((name (alist-get 'name event)))
    (setf (alist-get name ewm--output-info nil nil #'string=) event)
    (ewm-layout--refresh)))

(defun ewm--handle-output-config-changed (event)
  "Handle output config changed EVENT.
Patches `ewm--output-info' with the new values, then runs
`ewm-output-config-changed-hook'."
  (let ((name (alist-get 'name event)))
    (when-let* ((info (alist-get name ewm--output-info nil nil #'string=)))
      ;; Patch scalar fields
      (setf (alist-get 'scale info)     (alist-get 'scale event))
      (setf (alist-get 'transform info) (alist-get 'transform event))
      (setf (alist-get 'x info)         (alist-get 'x event))
      (setf (alist-get 'y info)         (alist-get 'y event))
      ;; Update current flag on modes to match the new active mode
      (let ((new-w (alist-get 'width event))
            (new-h (alist-get 'height event))
            (new-r (alist-get 'refresh event)))
        (seq-doseq (m (alist-get 'modes info))
          (setf (alist-get 'current m)
                (and (= (alist-get 'width m) new-w)
                     (= (alist-get 'height m) new-h)
                     (= (alist-get 'refresh m) new-r))))))
    (run-hook-with-args 'ewm-output-config-changed-hook event)))

(defvar ewm-output-config-changed-hook nil
  "Hook run when output configuration is applied.
Each function receives the event alist with symbol keys:
  name, width, height, refresh, x, y, scale, transform.")

(defun ewm--handle-output-disconnected (event)
  "Record that an output disconnected.
Frame migration is compositor-owned and arrives as `frame_moved' events."
  (let ((name (alist-get 'name event)))
    (setq ewm--output-info
          (assoc-delete-all name ewm--output-info #'string=))
    (ewm-layout--refresh)))

(defun ewm--rename-buffer ()
  "Rename the current surface buffer based on app and title.
Similar to `exwm-workspace-rename-buffer'."
  (let* ((app (or ewm-surface-app "unknown"))
         (title (or ewm-surface-title ""))
         (name (format "*ewm:%s*" (if (string-empty-p title) app title))))
    (rename-buffer name t)))

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
  (ewm--apply-gtk-bg-css)
  (let (created)
    (dolist (output (ewm-get-active-outputs))
      (unless (ewm--strip-frames output)
        (ewm--create-frame-for-output output)
        (setq created t)))
    (ewm--enforce-frame-output-parity)
    (ewm-layout--refresh)
    ;; Frames created after the ready event missed the replay `ewm-start-module'
    ;; asks for, and PGTK drops the enter they get before their first roundtrip.
    ;; Once only: the replay costs a leave/enter for whatever holds focus.
    (when (and created ewm--compositor-ready (not ewm--late-frames-replayed))
      (setq ewm--late-frames-replayed t)
      (run-with-idle-timer 0 nil #'ewm-notify-initialized-module))))

(defun ewm--handle-environment (event)
  "Handle environment event from compositor.
Sets Emacs's `process-environment' so child processes inherit these vars."
  (dolist (pair (alist-get 'vars event))
    (setenv (symbol-name (car pair)) (cdr pair))))

(defun ewm--handle-ready ()
  "Handle ready event from compositor.
Signals that the compositor is fully initialized."
  (setq ewm--compositor-ready t))

(defun ewm--handle-working-area (event)
  "Handle working area change EVENT.
Called when layer-shell surfaces (panels) claim exclusive zones,
changing the available area for Emacs frames."
  ;; Frame resize happens automatically via Wayland configure event.
  ;; Layout refresh is triggered by window-size-change-functions when
  ;; the frame actually resizes.
  (ewm--log "Working area for %s: %dx%d+%d+%d"
            (alist-get 'output event) (alist-get 'width event)
            (alist-get 'height event) (alist-get 'x event)
            (alist-get 'y event)))

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
  (when (ewm--compositor-active-p)
    (ewm-set-selection-module text)))

(defun ewm--handle-activate-workspace (event)
  "Handle activate_workspace EVENT from compositor.
Select the Nth Emacs frame on the requested output (1-based index)."
  (unless (ewm-layout--menu-active-p)
    (let* ((output (alist-get 'output event))
           (frame-index (alist-get 'frame_index event))
           (frames (ewm--strip-frames output))
           (target (nth (1- frame-index) frames)))
      (when target
        (select-frame-set-input-focus target)))))

;;; Strip-visibility freeze/thaw
;;
;; The compositor emits frame_shown/frame_hidden when an emacs frame's
;; strip-active status changes.  We forward to GTK's update-freeze so
;; pgtk's per-tick Cairo paint skips hidden frames.

(declare-function ewm-gtk-freeze-frame-updates "ewm-core")
(declare-function ewm-gtk-thaw-frame-updates "ewm-core")

(defun ewm-output-active-frame (output)
  "Return the strip frame shown on OUTPUT, or its first frame."
  (let ((frames (ewm--strip-frames output))
        (active (and output
                     (ewm--frame-by-surface-id (ewm-get-active-frame-id output)))))
    ;; `ewm--strips' can lag the compositor by an event.
    (if (memq active frames) active (car frames))))

(defun ewm--gtk-frame-updates (event fn)
  "Call FN with the GTK window id of EVENT's frame, when GTK-backed."
  (when-let* (((ewm--gtk-backed-p))
              (frame (ewm--frame-by-surface-id (alist-get 'id event)))
              (wid (frame-parameter frame 'window-id)))
    (ignore-errors (funcall fn wid))))

(defun ewm--handle-frame-moved (event)
  "Mirror the compositor-authoritative output for an Emacs frame."
  (let ((output (alist-get 'output event))
        (frame (ewm--frame-by-surface-id (alist-get 'id event))))
    (when frame
      (set-frame-parameter frame 'ewm-home-output (alist-get 'home_output event))
      (unless (equal (frame-parameter frame 'ewm-output) output)
        (ewm--move-frame-to-output frame output)
        (ewm-layout--refresh)))))

;;; Idle timeout

(defun ewm--handle-idle-state-changed (event)
  "Handle idle_state_changed EVENT from compositor."
  (let ((idle (alist-get 'idle event)))
    (run-hook-with-args 'ewm-idle-hook idle)))

(defun ewm--send-idle-config ()
  "Send idle timeout configuration to compositor."
  (when (ewm--compositor-active-p)
    (pcase ewm-idle
      ('nil (ewm-configure-idle-module nil nil))
      ((pred integerp)
       (ewm-configure-idle-module ewm-idle "blank"))
      (`(,(and (pred integerp) secs) . ,(and (pred stringp) cmd))
       (ewm-configure-idle-module secs cmd)))))

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
  (when (ewm--compositor-active-p)
    (ewm-configure-cursor-module (ewm--cursor-theme) (ewm--cursor-size))))

(defun ewm--send-cursor-hide-config ()
  "Send cursor auto-hide configuration to compositor.
Uses `bound-and-true-p' for the inputs so this is safe to call from a
`:set' that fires before the other defcustom has been evaluated."
  (when (ewm--compositor-active-p)
    (ewm-configure-cursor-hide-module
     (let ((v (bound-and-true-p ewm-cursor-auto-hide)))
       (and (integerp v) v))
     (bound-and-true-p ewm-cursor-hide-when-typing))))

(define-minor-mode ewm-idle-inhibit-mode
  "Inhibit idle timeout and notifications.
While enabled, the screen won't blank and swayidle won't fire regardless
of inactivity.  Useful for video playback or presentations."
  :global t
  :variable ewm-idle-inhibited
  (when (ewm--compositor-active-p)
    (ewm-set-idle-inhibited (if ewm-idle-inhibited 1 nil))))

;;; Commands

(defun ewm-lock-session ()
  "Lock the session via loginctl."
  (interactive)
  (start-process "loginctl" nil "loginctl" "lock-session"))

(defun ewm--cycle-surface-buffer (command)
  "Run buffer-navigation COMMAND with a surface-only filter."
  (let* ((skip switch-to-prev-buffer-skip)
         (switch-to-prev-buffer-skip
          (lambda (window buffer bury-or-kill)
            (or (not (ewm-surface-buffer-p buffer))
                (switch-to-prev-buffer-skip-p skip window buffer bury-or-kill)))))
    (funcall command)))

(defun ewm-next-surface-buffer ()
  "Like `next-buffer', but only among surface buffers."
  (interactive)
  (ewm--cycle-surface-buffer #'next-buffer))

(defun ewm-prev-surface-buffer ()
  "Like `previous-buffer', but only among surface buffers."
  (interactive)
  (ewm--cycle-surface-buffer #'previous-buffer))

(defun ewm-toggle-fullscreen ()
  "Toggle fullscreen for the current layout view."
  (interactive)
  (when-let* ((entry-id (window-parameter nil 'ewm-entry-id)))
    (ewm-toggle-fullscreen-module entry-id)))

(defun ewm-get-window-info (&optional id)
  "Return window info alist (id app title pid) for surface ID.
When ID is nil, use the current buffer's surface or compositor focus.
Returns nil when no surface is found."
  (when-let* ((surface-id (or id ewm-surface-id (ewm-get-focused-id)))
              (buf (gethash surface-id ewm--surfaces)))
    (list (cons 'id surface-id)
          (cons 'app (buffer-local-value 'ewm-surface-app buf))
          (cons 'title (buffer-local-value 'ewm-surface-title buf))
          (cons 'pid (buffer-local-value 'ewm-surface-pid buf)))))

(defun ewm-get-window-info-json (&optional id)
  "Return window info as a JSON string.
Calls `ewm-get-window-info' and serializes the result.
When ID is nil, use the current buffer's surface or compositor focus.
Returns fallback Emacs frame info when no surface is found.

For use with `emacsclient -e \\='(ewm-get-window-info-json)'."
  (json-encode (or (ewm-get-window-info id)
                   `((id . 0)
                     (app . "emacs")
                     (title . ,(frame-parameter nil 'name))
                     (pid . ,(emacs-pid))))))

(defun ewm-output-layout (output frames)
  "Set OUTPUT's frame strip to FRAMES.
FRAMES is a vector of `ewm-layout--build-frame-plist' plists, left to right."
  (ewm-output-layout-module output frames))

(defun ewm-floating-layout (output frames)
  "Set declarative contents for OUTPUT's floating Emacs frames."
  (ewm-floating-layout-module output frames))

(defun ewm-workspace-rename (name &optional frame)
  "Rename FRAME's workspace to NAME.
When FRAME is nil, use the selected frame.  An empty NAME clears
the workspace name."
  (interactive (list (read-string "Workspace name: ")))
  (let* ((frame (or frame (selected-frame)))
         (frame-id (frame-parameter frame 'ewm-surface-id)))
    (ewm-workspace-rename-module name frame-id)))

(defun ewm-workspace-mark-urgent (&optional frame)
  "Mark FRAME's workspace as urgent.
When FRAME is nil, use the selected frame.  The compositor ignores
the request when the workspace is already focused."
  (interactive)
  (let* ((frame (or frame (selected-frame)))
         (frame-id (frame-parameter frame 'ewm-surface-id)))
    (unless frame-id
      (user-error "Frame has no EWM surface id"))
    (ewm-workspace-mark-urgent-module frame-id)))

(defun ewm-close (id)
  "Request surface ID to close gracefully."
  (ewm-close-module id))

(defun ewm-warp-pointer (x y)
  "Warp pointer to absolute position X, Y."
  (ewm-warp-pointer-module x y))

(defun ewm-show-state ()
  "Display the compositor state dump in *ewm-state*."
  (interactive)
  (let ((state (or (ewm-get-debug-state-module)
                   (user-error "Timed out waiting for compositor state dump"))))
    (with-current-buffer-window "*ewm-state*" nil nil
      (emacs-lisp-mode)
      (pp state (current-buffer)))))

(defun ewm-list-outputs ()
  "Display connected outputs with properties and ewm-output-config suggestions.
Shows connector name, Make-Model-Serial, current mode, scale, position,
physical size, all available modes, and a ready-to-use config entry.

Output is written to the *ewm-outputs* buffer.  The suggested config key
uses the Make-Model-Serial form when make/model are known so that the entry
survives connector re-numbering when cables are replugged."
  (interactive)
  (if (null ewm--output-info)
      (message "No output info available (is the compositor running?)")
    (with-current-buffer-window "*ewm-outputs*" nil nil
      (dolist (entry ewm--output-info)
        (let* ((info        (cdr entry))
               (name        (alist-get 'name      info))
               (mms         (ewm--output-mms info))
               (config-key  (ewm--output-config-key info))
               (x           (or (alist-get 'x         info) 0))
               (y           (or (alist-get 'y         info) 0))
               (scale       (or (alist-get 'scale     info) 1.0))
               (transform   (or (alist-get 'transform info) 0))
               (width-mm    (or (alist-get 'width_mm  info) 0))
               (height-mm   (or (alist-get 'height_mm info) 0))
               (modes       (alist-get 'modes info))
               (cur (when modes
                      (seq-find (lambda (m) (alist-get 'current m)) modes)))
               (transform-str
                (pcase transform
                  (0 "Normal")   (1 "90°")          (2 "180°")
                  (3 "270°")     (4 "Flipped")       (5 "Flipped+90°")
                  (6 "Flipped+180°") (7 "Flipped+270°")
                  (_ (number-to-string transform)))))
          (insert (format "Output \"%s\" (%s)\n" mms name))
          (if cur
              (insert (format "  Current mode: %dx%d @ %.3f Hz%s\n"
                              (alist-get 'width   cur)
                              (alist-get 'height  cur)
                              (/ (float (alist-get 'refresh cur)) 1000.0)
                              (if (alist-get 'preferred cur) " (preferred)" "")))
            (insert "  Disabled / no current mode\n"))
          (insert (format "  Scale: %g\n" scale))
          (insert (format "  Transform: %s\n" transform-str))
          (insert (format "  Position: %d, %d\n" x y))
          (when (or (> width-mm 0) (> height-mm 0))
            (insert (format "  Physical size: %dx%d mm\n" width-mm height-mm)))
          (when modes
            (insert "  Available modes:\n")
            (seq-do
             (lambda (m)
               (insert
                (format "    %dx%d @ %.3f Hz%s\n"
                        (alist-get 'width  m)
                        (alist-get 'height m)
                        (/ (float (alist-get 'refresh m)) 1000.0)
                        (concat
                         (if (alist-get 'current   m) " (current)"   "")
                         (if (alist-get 'preferred m) " (preferred)" "")))))
             modes))
          (when cur
            (insert "\n  Suggested ewm-output-config entry:\n")
            (insert (format "    (\"%s\" :width %d :height %d :scale %g)\n"
                            config-key
                            (alist-get 'width  cur)
                            (alist-get 'height cur)
                            scale)))
          (insert "\n")))
      (special-mode))))

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
    (setq ewm--debug new-state)
    (message "EWM debug mode: %s" (if new-state "ENABLED" "disabled"))))

(defun ewm-configure-output (name &rest args)
  "Configure output NAME with ARGS.
ARGS is a plist with optional keys:
  :x :y       - position in global coordinate space
  :width :height :refresh - video mode
  :custom     - when non-nil, generate a CVT mode for :width/:height/:refresh
                instead of matching an advertised mode (:refresh required)
  :modeline   - X11 modeline string, takes precedence over the mode fields:
                \"CLOCK HDISP HSS HSE HTOTAL VDISP VSS VSE VTOTAL +hsync +vsync\"
  :scale      - fractional scale (e.g. 1.5)
  :transform  - transform integer (0=Normal, 1=90, 2=180, 3=270, ...)
  :enabled    - t or nil to enable/disable the output"
  (ewm-configure-output-module name args))

(defun ewm-prepare-frame (output)
  "Tell compositor to assign next frame to OUTPUT."
  (ewm-prepare-frame-module output))

(defun ewm-prepare-floating-frame (output &optional x y)
  "Tell compositor to assign next frame as floating on OUTPUT at top-left X/Y."
  (ewm-prepare-floating-frame-module output x y))

(defun ewm--get-output-origin (output-name)
  "Return (x . y) origin for OUTPUT-NAME, or (0 . 0) if not found."
  (or (ewm-get-output-origin output-name) '(0 . 0)))

(defun ewm--get-frame-origin (frame)
  "Return global origin for FRAME, falling back to its output origin."
  (or (when-let ((id (frame-parameter frame 'ewm-surface-id)))
        (and (fboundp 'ewm-get-frame-origin)
             (ewm-get-frame-origin id)))
      (ewm--get-output-origin (frame-parameter frame 'ewm-output))))

(defun ewm--output-mms (info)
  "Return \"Make Model Serial\" string for output INFO event alist.
Uses \"Unknown\" for any component absent from EDID so config keys
written from `ewm-list-outputs' round-trip correctly."
  (let* ((make   (or (alist-get 'make   info) "Unknown"))
         (model  (or (alist-get 'model  info) "Unknown"))
         (serial (or (alist-get 'serial info) ""))
         (serial-str (if (string-empty-p serial) "Unknown" serial)))
    (format "%s %s %s" make model serial-str)))

(defun ewm--output-config-key (info)
  "Return the recommended ewm-output-config key for output INFO.
Uses the Make-Model-Serial string when make/model are known (survives
connector re-numbering after cable changes), otherwise the connector name."
  (let ((make  (or (alist-get 'make  info) "Unknown"))
        (model (or (alist-get 'model info) "Unknown")))
    (if (and (string= make "Unknown") (string= model "Unknown"))
        (alist-get 'name info)
      (ewm--output-mms info))))

(defun ewm--find-connector-for-config (config-name)
  "Resolve CONFIG-NAME to a live connector name.
CONFIG-NAME may be a connector name (e.g. \"DP-1\") or a Make-Model-Serial
string (e.g. \"Dell U2722D ABC123\").  Tries exact connector match first,
then case-insensitive MMS match against `ewm--output-info'.
Returns CONFIG-NAME itself when no live output matches, so the compositor
still stores the config for future hot-plug."
  (or (car (assoc config-name ewm--output-info))
      (car (cl-find config-name ewm--output-info
                    :key (lambda (entry) (ewm--output-mms (cdr entry)))
                    :test #'string-equal-ignore-case))
      config-name))

(defun ewm--apply-output-config ()
  "Apply user output configuration from `ewm-output-config'.
Config keys may be connector names (\"DP-1\") or Make-Model-Serial strings
(\"Dell U2722D ABC123\"); the latter are resolved to the live connector name
via `ewm--find-connector-for-config' before sending to the compositor."
  (dolist (config ewm-output-config)
    (let ((props (cdr config)))
      (when (or (plist-member props :enabled)
                (cl-some (lambda (key) (plist-get props key))
                         '(:width :height :refresh :modeline :scale :transform :x :y)))
        (apply #'ewm-configure-output
               (ewm--find-connector-for-config (car config)) props)))))

;;; Frame management

(defconst ewm--managed-window-systems '(pgtk pwayl)
  "GUI window systems whose frames EWM binds to outputs.
EWM can host either a pgtk or a native-Wayland (+ Skia) Emacs build.")

(defun ewm--gtk-backed-p ()
  "Non-nil when EWM's host Emacs frames are backed by GTK.
The native Wayland backend has no GTK widgets or style context to mutate."
  (eq (ignore-errors (ewm--window-system)) 'pgtk))

(defun ewm--window-system ()
  "Return the window system to create EWM output frames with.
Picks whichever GUI backend this Emacs was built with (pgtk or the
native Wayland backend)."
  (cond ((featurep 'pgtk) 'pgtk)
        ((featurep 'pwayl) 'pwayl)
        (t (error "EWM requires a pgtk or native-Wayland Emacs build"))))

(defun ewm--output-managed-p (frame)
  "Return non-nil if FRAME is bound to a compositor output."
  (memq (framep-on-display frame) ewm--managed-window-systems))

(defun ewm--surface-target-frame-for-output (output-name)
  "Return the best tiled fallback frame for a new surface on OUTPUT-NAME."
  (or (let ((frame (ignore-errors (ewm--focused-frame))))
        (and frame
             (not (frame-parameter frame 'ewm-floating))
             (equal (frame-parameter frame 'ewm-output) output-name)
             frame))
      (ewm-output-active-frame output-name)))

(defun ewm--frame-in-flight-p (frame)
  "Return non-nil when FRAME has an output assignment but no surface id yet."
  (and (frame-parameter frame 'ewm-output)
       (null (frame-parameter frame 'ewm-surface-id))))

(defun ewm--strip-frames (output)
  "Return OUTPUT's strip frame list (left-to-right)."
  (alist-get output ewm--strips nil nil #'equal))

(defun ewm--strip-set (output frames)
  "Replace OUTPUT's strip with FRAMES, dropping the entry when empty."
  (setf (alist-get output ewm--strips nil t #'equal) frames))

(defun ewm--floating-frames (output)
  "Return OUTPUT's floating frame list, top-to-bottom."
  (alist-get output ewm--floating-frames nil nil #'equal))

(defun ewm--floating-set (output frames)
  "Replace OUTPUT's floating frame list, dropping the entry when empty."
  (setf (alist-get output ewm--floating-frames nil t #'equal) frames))

(defun ewm--floating-insert (frame output)
  "Insert FRAME at the top of OUTPUT's floating frame stack."
  (ewm--floating-set output (cons frame (delq frame (ewm--floating-frames output)))))

(defun ewm--floating-remove (frame)
  "Drop FRAME from its floating output stack."
  (when-let* ((output (frame-parameter frame 'ewm-output)))
    (ewm--floating-set output (delq frame (ewm--floating-frames output)))))

(defun ewm--strip-insert (frame output &optional after)
  "Insert FRAME into OUTPUT's strip after frame AFTER, or at the end."
  (let* ((frames (ewm--strip-frames output))
         (pos (and after (cl-position after frames)))
         (at (if pos (1+ pos) (length frames))))
    (ewm--strip-set output
                    (append (cl-subseq frames 0 at)
                            (list frame)
                            (cl-subseq frames at)))))

(defun ewm--strip-remove (frame)
  "Drop FRAME from its strip."
  (when-let* ((output (frame-parameter frame 'ewm-output)))
    (ewm--strip-set output (delq frame (ewm--strip-frames output)))))

(defun ewm--strip-swap (frame delta)
  "Swap FRAME with its DELTA neighbor; return that neighbor or nil at the edge."
  (when-let* ((output (frame-parameter frame 'ewm-output))
              (frames (ewm--strip-frames output))
              (idx (cl-position frame frames))
              (target-idx (+ idx delta))
              ((>= target-idx 0))
              ((< target-idx (length frames))))
    (cl-rotatef (nth idx frames) (nth target-idx frames))
    (nth target-idx frames)))

(defun ewm--move-frame-to-output (frame output)
  "Move FRAME to OUTPUT's strip, updating param and membership in lockstep.
Order matters: `ewm--strip-remove' reads `ewm-output' to find the source
strip, so the parameter must change between the remove and the insert."
  (ewm--strip-remove frame)
  (ewm--floating-remove frame)
  (set-frame-parameter frame 'ewm-output output)
  (if (frame-parameter frame 'ewm-floating)
      (ewm--floating-insert frame output)
    (ewm--strip-insert frame output)))

(defun ewm--make-output-frame (output-name extra-params)
  "Make a managed frame on OUTPUT-NAME carrying EXTRA-PARAMS.
EXTRA-PARAMS win over the defaults, so a caller can override `visibility'."
  (make-frame `(,@extra-params
                (visibility . t)
                (window-system . ,(ewm--window-system))
                (ewm-output . ,output-name)
                (ewm-frame-seq . ,(cl-incf ewm--frame-seq)))))

(defun ewm--create-frame-for-output (output-name &optional after-frame extra-params)
  "Create a frame on OUTPUT-NAME inserted after AFTER-FRAME (or appended)."
  (ewm-prepare-frame output-name)
  (let ((frame (ewm--make-output-frame output-name extra-params)))
    (ewm--strip-insert frame output-name after-frame)
    frame))

(defun ewm--create-floating-frame-for-output (output-name &optional extra-params pos)
  "Create a floating frame on OUTPUT-NAME, optionally at POS (X . Y) pixels."
  (ewm-prepare-floating-frame output-name (car pos) (cdr pos))
  (let ((frame (ewm--make-output-frame
                output-name
                `((ewm-floating . t) ,@extra-params (width . 80) (height . 24)))))
    ;; `quit-window' would iconify otherwise, which EWM frames cannot do.
    (set-frame-parameter frame 'auto-hide-function (lambda () (delete-frame frame)))
    (ewm--floating-insert frame output-name)
    frame))

(defun ewm--on-make-frame (frame)
  "Hook for frame creation. Drop frames not assigned to an output."
  (when (and (ewm--output-managed-p frame)
             (not (frame-parameter frame 'ewm-output)))
    (ewm--log "on-make-frame: frame=%s action=schedule-delete" frame)
    (run-at-time 0 nil (lambda () (ignore-errors (delete-frame frame))))))

(defun ewm--on-delete-frame (frame &rest _)
  "Hook for `delete-frame-functions': drop FRAME from its EWM placement."
  (if (frame-parameter frame 'ewm-floating)
      (ewm--floating-remove frame)
    (ewm--strip-remove frame)))

(defun ewm--prevent-frame-deletion (orig-fun &optional frame force)
  "Around advice for `delete-frame': refuse to delete the last frame of
an output unless FORCE.

When deletion proceeds, pass the captured frame to ORIG-FUN explicitly:
interactive `delete-frame' passes nil, and nil means \"delete the
selected frame\", which can change during pgtk's internal close handling."
  (let* ((f (or frame (selected-frame)))
         (output (frame-parameter f 'ewm-output))
         (alone (and output
                     (not (frame-parameter f 'ewm-floating))
                     (<= (length (ewm--strip-frames output)) 1))))
    (cond
     ((and alone (not force))
      (message "Cannot delete the only frame on output %s" output))
     (t
      (when (eq f (selected-frame))
        (when-let ((id (frame-parameter f 'ewm-surface-id)))
          (ewm-prepare-frame-close-module id)))
      (funcall orig-fun f force)))))

(defun ewm-frame-new ()
  "Open a new Emacs frame after the active one on the same output."
  (interactive)
  (let* ((current (ewm--focused-frame))
         (output (frame-parameter current 'ewm-output)))
    (unless output (user-error "Current frame has no ewm-output"))
    (ewm--create-frame-for-output output current
                                  '((ewm-focus-on-create . t)))))

(defun ewm--floating-return-frame (floating-frame)
  "Return the tiled frame that should receive FLOATING-FRAME's buffer."
  (or (when-let* ((origin-id (frame-parameter floating-frame
                                              'ewm-floating-origin-frame-id))
                  (origin (ewm--frame-by-surface-id origin-id))
                  ((not (frame-parameter origin 'ewm-floating))))
        origin)
      (ewm-output-active-frame (frame-parameter floating-frame 'ewm-output))))

(defun ewm--fit-floating-frame (frame width height)
  "Size FRAME so its client window body is WIDTH×HEIGHT logical pixels."
  (let ((rect (ewm--window-body-pixel-rect (frame-selected-window frame)))
        (area (frame-monitor-workarea frame)))
    (cl-flet ((axis (want limit text body)
                (round (+ (min want limit) (- text body)))))
      (set-frame-size frame
                      (axis width (nth 2 area) (frame-text-width frame) (nth 2 rect))
                      (axis height (nth 3 area) (frame-text-height frame) (nth 3 rect))
                      t))))

(defun ewm--make-floating-frame (alist)
  "Make a floating frame from ALIST and `pop-up-frame-alist', or nil."
  (when-let* ((output (or (alist-get 'ewm-output alist)
                          (frame-parameter nil 'ewm-output))))
    (let ((origin (frame-parameter nil 'ewm-surface-id)))
      (ewm--create-floating-frame-for-output
       output
       ;; Stay hidden until fitted so only the final size is ever rendered.
       `((visibility . nil)
         ,@(and origin `((ewm-floating-origin-frame-id . ,origin)))
         ,@pop-up-frame-alist)
       (alist-get 'ewm-position alist)))))

(defvar ewm--inhibit-floating nil
  "Non-nil makes `ewm-display-buffer-floating' decline, so attach cannot re-float.")

(defun ewm-display-buffer-floating (buffer alist)
  "Display BUFFER in a new floating frame; a `display-buffer' action function.
Looks come from `pop-up-frame-parameters'.  The frame is fitted to `ewm-size'
(client body W . H) in ALIST, else to the buffer's `ewm-surface-size'.  EWM's
own placements add `ewm-output' and `ewm-position' (top-left X . Y)."
  (unless ewm--inhibit-floating
    (let ((pop-up-frame-function (lambda () (ewm--make-floating-frame alist))))
      ;; Fit after the user's window parameters land, then show.
      (when-let* ((window (display-buffer-pop-up-frame
                           buffer (cons '(inhibit-switch-frame . t) alist)))
                  (frame (window-frame window)))
        (when-let* ((size (or (alist-get 'ewm-size alist)
                              (buffer-local-value 'ewm-surface-size buffer))))
          (ewm--fit-floating-frame frame (car size) (cdr size)))
        (make-frame-visible frame)
        window))))

(defvar ewm-floating-popout-scale 0.9
  "Fraction of the source window a popped-out floating frame occupies.")

(defun ewm--floating-detach-selected-buffer (frame)
  "Detach FRAME's selected buffer into a floating frame centered over its tile."
  (let* ((source-window (frame-selected-window frame))
         (rect (ewm--window-body-pixel-rect source-window))
         (w (nth 2 rect))
         (h (nth 3 rect))
         (fw (round (* w ewm-floating-popout-scale)))
         (fh (round (* h ewm-floating-popout-scale))))
    (unless (frame-parameter frame 'ewm-output)
      (user-error "Current frame has no ewm-output"))
    (when (window-minibuffer-p source-window)
      (user-error "Cannot detach minibuffer window"))
    (select-frame frame)
    ;; Float positions are output-relative.
    (pcase-let ((`(,fx . ,fy) (ewm--get-frame-origin frame))
                (`(,ox . ,oy) (ewm--get-output-origin
                               (frame-parameter frame 'ewm-output))))
      (pop-to-buffer (window-buffer source-window)
                     `(ewm-display-buffer-floating
                       (inhibit-same-window . t)
                       (ewm-size . (,fw . ,fh))
                       (ewm-position . (,(+ (nth 0 rect) (- fx ox) (/ (- w fw) 2))
                                        . ,(+ (nth 1 rect) (- fy oy) (/ (- h fh) 2)))))))
    ;; Reclaim the tile once the buffer has left for a float.
    (when (and (window-live-p source-window)
               (frame-parameter (window-frame (selected-window)) 'ewm-floating))
      (save-selected-window
        (if (eq t (window-deletable-p source-window))
            (delete-window source-window)
          (quit-window nil source-window))))))

(defun ewm--floating-attach-selected-buffer (floating-frame)
  "Absorb FLOATING-FRAME's buffer into the tiled window under its center."
  (let* ((buffer (window-buffer (frame-selected-window floating-frame)))
         (sid (frame-parameter floating-frame 'ewm-surface-id))
         (entry-id (and sid (ewm-entry-under-floating-center-module sid)))
         (window (and entry-id (ewm--find-window-by-entry-id entry-id)))
         (fallback (unless window (ewm--floating-return-frame floating-frame))))
    (unless (or window fallback)
      (user-error "No tiled frame on output %s"
                  (frame-parameter floating-frame 'ewm-output)))
    (delete-frame floating-frame t)
    ;; pop-to-buffer lets Emacs reuse or split the target window on its own.
    (if (window-live-p window)
        (select-window window)
      (select-frame-set-input-focus fallback))
    (let ((ewm--inhibit-floating t))
      (pop-to-buffer buffer))
    (ewm-layout--refresh)))

(defun ewm-floating-toggle ()
  "Toggle the selected buffer between tiled and floating frame placement."
  (interactive)
  (let ((frame (ewm--focused-frame)))
    (if (frame-parameter frame 'ewm-floating)
        (ewm--floating-attach-selected-buffer frame)
      (ewm--floating-detach-selected-buffer frame))))


(defun ewm-frame-close ()
  "Close the focused frame; the compositor refocuses a same-output neighbour."
  (interactive)
  (let ((frame (ewm--focused-frame)))
    ;; Select it first so the delete-frame advice notifies the compositor.
    (unless (eq frame (selected-frame))
      (select-frame frame))
    (delete-frame frame)))

(defun ewm--current-frame ()
  "Frame the user is visually on -- compositor-authoritative.
Emacs's `selected-frame' can drift via `display-buffer'/`select-window'."
  (when-let* ((id (ewm-get-focused-frame-id))
              ((> id 0)))
    (ewm--frame-by-surface-id id)))

(defun ewm--focused-frame ()
  "Frame the user is on per the compositor, falling back to `selected-frame'."
  (or (ewm--current-frame) (selected-frame)))

(defun ewm--frame-shift-on-output (delta)
  "Move strip focus by DELTA on the current output (no wrap-around).
Returns the target frame on success, nil at the strip edge.  Anchors at
the focused frame when it is in the strip, else the shown frame, so
navigation works while a float holds focus."
  (let* ((focused (ewm--focused-frame))
         (output (frame-parameter focused 'ewm-output))
         (frames (ewm--strip-frames output))
         (current (if (memq focused frames) focused (ewm-output-active-frame output)))
         (idx (cl-position current frames))
         (target-idx (and idx (+ idx delta))))
    (when (and target-idx (>= target-idx 0) (< target-idx (length frames)))
      (let ((target (nth target-idx frames)))
        (select-frame-set-input-focus target)
        target))))

(defun ewm-frame-left ()
  "Slide to the previous frame on the current output's strip."
  (interactive)
  (ewm--frame-shift-on-output -1))

(defun ewm-frame-right ()
  "Slide to the next frame on the current output's strip."
  (interactive)
  (ewm--frame-shift-on-output 1))

(defun ewm-frame-select ()
  "Select the Nth frame (1-based) on the current output.
Reads the digit from `last-command-event'.  No-op if N is past the
number of frames on this output."
  (interactive)
  (let* ((pressed (- (event-basic-type last-command-event) ?0))
         (current (ewm--focused-frame))
         (output (frame-parameter current 'ewm-output))
         (frames (ewm--strip-frames output))
         (target (and (>= pressed 1) (<= pressed (length frames))
                      (nth (1- pressed) frames))))
    (when target
      (select-frame-set-input-focus target))))

(defun ewm--frame-swap-on-output (delta)
  "Swap the focused frame with its DELTA neighbor in the strip.
Returns the neighbor on success, nil at the strip edge."
  (when-let* ((target (ewm--strip-swap (ewm--focused-frame) delta)))
    (ewm-layout--refresh)
    target))

(defun ewm-frame-move-left ()
  "Swap the selected frame with its left neighbor in the strip."
  (interactive)
  (ewm--frame-swap-on-output -1))

(defun ewm-frame-move-right ()
  "Swap the selected frame with its right neighbor in the strip."
  (interactive)
  (ewm--frame-swap-on-output 1))

(defun ewm-output-focus-left ()
  "Focus the active frame on the output to the left of the current one."
  (interactive)
  (ewm-focus-output-direction-module -1 0))

(defun ewm-output-focus-right ()
  "Focus the active frame on the output to the right of the current one."
  (interactive)
  (ewm-focus-output-direction-module 1 0))

(defun ewm-output-focus-up ()
  "Focus the active frame on the output above the current one."
  (interactive)
  (ewm-focus-output-direction-module 0 -1))

(defun ewm-output-focus-down ()
  "Focus the active frame on the output below the current one."
  (interactive)
  (ewm-focus-output-direction-module 0 1))

(defun ewm-focus-left ()
  "Focus the visible window to the left; cross output at the frame edge."
  (interactive)
  (unless (ignore-errors (windmove-left))
    (ewm-focus-output-direction-module -1 0)))

(defun ewm-focus-right ()
  "Focus the visible window to the right; cross output at the frame edge."
  (interactive)
  (unless (ignore-errors (windmove-right))
    (ewm-focus-output-direction-module 1 0)))

(defun ewm-focus-up ()
  "Focus the visible window above; cross output at the frame edge."
  (interactive)
  (unless (ignore-errors (windmove-up))
    (ewm-focus-output-direction-module 0 -1)))

(defun ewm-focus-down ()
  "Focus the visible window below; cross output at the frame edge."
  (interactive)
  (unless (ignore-errors (windmove-down))
    (ewm-focus-output-direction-module 0 1)))

(defun ewm--enforce-frame-output-parity ()
  "Delete orphan frames.
Frames may live on a strip multiple-deep; the duplicate-frame rule
that one-output-one-frame implied is gone -- the user explicitly
manages the strip via `ewm-frame-new'/`ewm-frame-close'."
  (dolist (frame (frame-list))
    (when (and (ewm--output-managed-p frame)
               (not (ewm--frame-in-flight-p frame))
               (null (frame-parameter frame 'ewm-output)))
      (ignore-errors (delete-frame frame)))))

;;; Public API

(defun ewm--disable-csd ()
  "Undecorate frames and drop bars that would skew the Y-offset."
  (set-frame-parameter nil 'undecorated t)
  (add-to-list 'default-frame-alist '(undecorated . t))
  (when (bound-and-true-p menu-bar-mode) (menu-bar-mode -1))
  (when (bound-and-true-p tool-bar-mode) (tool-bar-mode -1))
  (when (bound-and-true-p scroll-bar-mode) (scroll-bar-mode -1)))

;;;###autoload
(defun ewm-start-module ()
  "Start EWM in module mode (compositor runs in-process).
This is the primary entry point for using EWM from `emacs --daemon' on TTY.
The compositor runs as a thread within the Emacs process."
  (interactive)
  (ewm-mode 1))

(defun ewm--start-compositor ()
  "Start the compositor and wait for readiness, cleaning up on failure."
  (when (and (fboundp 'ewm-running) (ewm-running))
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
          (ewm--apply-gtk-bg-css)
          (setq started t))
      (unless started
        (ewm--stop-compositor)
        (ewm--destroy-event-channel)
        (ewm--teardown)
        (setq ewm--module-mode nil
              ewm--compositor-ready nil)))))

(defun ewm--stop-compositor ()
  "Request graceful compositor shutdown."
  (when (and (fboundp 'ewm-stop) (fboundp 'ewm-running) (ewm-running))
    (ewm-stop)
    (with-timeout (ewm--compositor-stop-timeout nil)
      (while (ewm-running)
        (sleep-for ewm--compositor-poll-interval)))))

(declare-function ewm-gtk-set-style "ewm-core")

(defun ewm--apply-gtk-bg-css (&rest _)
  "Push the default face background to GTK; falls back to black pre-graphical (bug#53232)."
  (when (ewm--gtk-backed-p)
    (let* ((live (face-attribute 'default :background))
           (bg (if (and (stringp live) (not (string-prefix-p "unspecified" live)))
                   live
                 "#000000")))
      (condition-case err
          (ewm-gtk-set-style (format "window { background-color: %s; }" bg))
        (error (message "EWM: ewm-gtk-set-style failed: %s" err))))))

;;; Process spawning with activation tokens

(defun ewm--inject-activation-token (orig-fun &rest args)
  "Advice to inject XDG_ACTIVATION_TOKEN into spawned processes.
This allows spawned GUI applications to request focus via xdg_activation."
  (let* ((token (and ewm-mode (fboundp 'ewm-create-activation-token)
                     (ewm-create-activation-token)))
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

;;; Application launcher

(defun ewm-launch-xdg-command (name)
  "Launch XDG app NAME, stripping desktop field codes from its Exec line."
  (let* ((cmd (cdr (assoc name (ewm-list-xdg-apps))))
         (cmd (replace-regexp-in-string "%[uUfFdDnNickvm]" "" cmd))
         (cmd (string-trim cmd)))
    (start-process-shell-command name nil cmd)))

(defun ewm-launch-app ()
  "Launch an XDG desktop application via `completing-read'."
  (interactive)
  (ewm-launch-xdg-command
   (completing-read "Launch app: " (ewm-list-xdg-apps) nil t)))

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
  (ewm--send-cursor-config)
  (ewm--send-cursor-hide-config)
  (ewm-set-unfocused-alpha (float ewm-unfocused-alpha))
  (ewm-set-animations-enabled ewm-animations-enabled)
  (when (and (not ewm-focus-follows-mouse) mouse-autoselect-window)
    (customize-set-variable 'ewm-focus-follows-mouse t))
  (ewm-text-input-auto-mode 1)
  (ewm--enable-process-advice)
  (add-function :override interprogram-cut-function #'ewm--interprogram-cut-function)
  (add-function :override interprogram-paste-function #'ignore)
  (add-hook 'after-make-frame-functions #'ewm--on-make-frame)
  (add-hook 'delete-frame-functions #'ewm--on-delete-frame)
  (when (ewm--gtk-backed-p)
    (add-hook 'enable-theme-functions #'ewm--apply-gtk-bg-css))
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
  (remove-hook 'delete-frame-functions #'ewm--on-delete-frame)
  (remove-hook 'after-init-hook #'ewm--send-intercept-keys)
  (remove-hook 'emacs-startup-hook #'ewm-notify-initialized-module)
  (remove-hook 'enable-theme-functions #'ewm--apply-gtk-bg-css)
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

(provide 'ewm)
;;; ewm.el ends here
