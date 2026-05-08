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

(unless (featurep 'ewm-core)
  (let ((path (getenv "EWM_MODULE_PATH")))
    (if (and path (file-exists-p path))
        (module-load path)
      (require 'ewm-core))))

;; Functions provided by the ewm-core dynamic module
(declare-function ewm-init-event-channel "ewm-core")
(declare-function ewm-set-selection-module "ewm-core")
(declare-function ewm-output-layout-module "ewm-core")
(declare-function ewm-close-module "ewm-core")
(declare-function ewm-warp-pointer-module "ewm-core")
(declare-function ewm-screenshot-module "ewm-core")
(declare-function ewm-get-debug-state-module "ewm-core")
(declare-function ewm-debug-mode-module "ewm-core")
(declare-function ewm-configure-output-module "ewm-core")
(declare-function ewm-prepare-frame-module "ewm-core")
(declare-function ewm-get-active-outputs "ewm-core")
(declare-function ewm-get-output-origin "ewm-core")
(declare-function ewm-start "ewm-core")
(declare-function ewm-list-xdg-apps "ewm-core")
(declare-function ewm-set-focus-follows-mouse "ewm-core")
(declare-function ewm-set-unfocused-alpha "ewm-core")
(declare-function ewm-notify-initialized-module "ewm-core")

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
        (insert (format-time-string "[%H:%M:%S] ")
                (apply #'format format-string args) "\n")))))

;;; Module mode (compositor runs in-process)

(defvar ewm--module-mode nil
  "Non-nil when running in module mode (compositor in-process).")

(defvar ewm--compositor-ready nil
  "Non-nil when compositor has signaled it is ready.")

(defun ewm--compositor-active-p ()
  "Return non-nil if compositor is active."
  ewm--module-mode)

(defvar ewm--event-process nil
  "Pipe process receiving compositor events via open_channel.")

(defvar ewm--event-buffer ""
  "Accumulator for partial JSON lines from the event pipe.")

(defun ewm--event-filter (_proc output)
  "Process newline-delimited JSON events from the compositor pipe."
  (setq ewm--event-buffer (concat ewm--event-buffer output))
  (while (string-match "\n" ewm--event-buffer)
    (let ((line (substring ewm--event-buffer 0 (match-beginning 0))))
      (setq ewm--event-buffer (substring ewm--event-buffer (match-end 0)))
      (when (> (length line) 0)
        (condition-case err
            (let ((event (json-parse-string line :object-type 'alist :false-object nil)))
              (ewm--handle-event event))
          (error (message "EWM: event parse error: %s" err)))))))

(defun ewm--init-event-channel ()
  "Create a pipe process and register it as the compositor event channel."
  (ewm--destroy-event-channel)
  (setq ewm--event-buffer "")
  (setq ewm--event-process
        (make-pipe-process :name "ewm-events"
                           :noquery t
                           :filter #'ewm--event-filter))
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
  "Whether cross-frame slide animations play during navigation.
Set to nil to disable all strip-transition animations.  Bind
dynamically via `let' to suppress animations for a single operation
without changing the global preference."
  :type 'boolean
  :group 'ewm)

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
  :x         - horizontal position (optional)
  :y         - vertical position (optional)
  :scale     - fractional scale, e.g. 1.5 (optional)
  :transform - transform as integer (optional):
               0=Normal 1=90 2=180 3=270
               4=Flipped 5=Flipped90 6=Flipped180 7=Flipped270
  :enabled   - whether output is enabled (default t)

OUTPUT-NAME can be either a connector name (e.g. \"DP-1\") or a
Make-Model-Serial string (e.g. \"Dell U2722D ABC123\").  The MMS form is
preferred because it survives connector re-numbering when cables are
replugged.  Use `ewm-list-outputs' to see both forms for each connected
output and copy a ready-to-use config entry.

Configuration is stored in the compositor and re-applied when outputs
reconnect (hot-plug), so values persist across connect/disconnect cycles.

Example:
  \\='((\"Dell U2722D ABC123\" :width 2560 :height 1440 :scale 1.5)
    (\"eDP-1\" :width 1920 :height 1200 :x 0 :y 0 :transform 0))"
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
(require 'ewm-transient)

;;; Protocol

(defun ewm--handle-event (event)
  "Handle EVENT from compositor (an alist with symbol keys)."
  (let ((type (alist-get 'event event)))
    (pcase type
      ("new" (ewm--handle-new-surface event))
      ("close" (ewm--handle-close-surface event))
      ("minimize" (ewm--handle-minimize-surface event))
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
      ("debug_state" (ewm--handle-debug-state event))
      ("working_area" (ewm--handle-working-area event))
      ("selection-changed" (ewm--handle-selection-changed event))
      ("activate_workspace" (ewm--handle-activate-workspace event))
      ("fullscreen_request" (ewm--handle-fullscreen-request event))
      ("unfullscreen_request" (ewm--handle-unfullscreen-request event))
      ("maximize_request" (ewm--handle-maximize-request event))
      ("unmaximize_request" (ewm--handle-unmaximize-request event))
      ("idle_state_changed" (ewm--handle-idle-state-changed event))
      ("environment" (ewm--handle-environment event)))))

;;; Event handlers

(defun ewm--cleanup-orphan-frames ()
  "Delete frames that have no ewm-output assigned."
  (dolist (f (frame-list))
    (when (and (ewm--output-managed-p f)
               (not (frame-parameter f 'ewm-output)))
      (ignore-errors (delete-frame f)))))

(defun ewm--assign-pending-frame (id output pending)
  "Assign surface ID to PENDING frame for OUTPUT."
  (let ((frame (cdr pending)))
    (setq ewm--pending-frame-outputs (delete pending ewm--pending-frame-outputs))
    (set-frame-parameter frame 'ewm-output output)
    (set-frame-parameter frame 'ewm-surface-id id)
    (unless (frame-parameter frame 'ewm-strip-index)
      (set-frame-parameter frame 'ewm-strip-index
                           (ewm--next-strip-index output)))
    ;; PGTK workaround: redisplay only flushes the Cairo context to the
    ;; Wayland surface for the selected frame.  Without this, the initial
    ;; surface commit is transparent until an external event arrives.
    (with-selected-frame frame (redisplay t))
    (when (frame-parameter frame 'ewm-focus-on-create)
      (set-frame-parameter frame 'ewm-focus-on-create nil)
      (ewm-focus-frame id))
    ;; Layout hooks are no-ops when FRAME was already selected; push it.
    (ewm-layout--refresh)
    (when (null ewm--pending-frame-outputs)
      (ewm--cleanup-orphan-frames))))

(defun ewm--frame-by-surface-id (surface-id)
  "Return the Emacs frame whose `ewm-surface-id' parameter is SURFACE-ID."
  (and surface-id
       (cl-find surface-id (frame-list)
                :key (lambda (f) (frame-parameter f 'ewm-surface-id)))))

(defun ewm--create-surface-buffer (id app output frame-surface-id pid)
  "Create buffer for surface ID (APP on OUTPUT, PID) and display it.
FRAME-SURFACE-ID, when non-nil, is the Emacs frame the compositor
chose to host this surface (parent's frame, or the active frame on
the surface's output).  Falls back to first match on OUTPUT."
  (let ((buf (generate-new-buffer (format "*ewm:%s:%d*" app id))))
    (puthash id buf ewm--surfaces)
    (with-current-buffer buf
      (ewm-surface-mode)
      (setq-local ewm-surface-id id)
      (setq-local ewm-surface-app app)
      (setq-local ewm-surface-pid pid))
    (unless (ewm--minibuffer-active-p)
      (when-let ((target-frame (or (ewm--frame-by-surface-id frame-surface-id)
                                   (ewm--frame-for-output output))))
        (select-frame target-frame))
      (pop-to-buffer-same-window buf))))

(defun ewm--handle-new-surface (event)
  "Handle new surface EVENT.
If there's a pending frame for this output, this is an Emacs frame.
Otherwise, creates a buffer for external surface."
  (let ((id (alist-get 'id event))
        (app (alist-get 'app event))
        (output (alist-get 'output event))
        (frame-surface-id (alist-get 'frame_surface_id event))
        (pid (alist-get 'pid event)))
    (let ((pending (and output (assoc output ewm--pending-frame-outputs))))
      (if pending
          (ewm--assign-pending-frame id output pending)
        (ewm--create-surface-buffer id app output frame-surface-id pid)))))

(defun ewm--handle-close-surface (event)
  "Handle close surface EVENT.
Kills the surface buffer."
  (let ((id (alist-get 'id event)))
    (when-let* ((buf (gethash id ewm--surfaces)))
      (when (buffer-live-p buf)
        (with-current-buffer buf
          (remove-hook 'kill-buffer-query-functions
                       #'ewm--kill-buffer-query-function t))
        (kill-buffer buf))
      (remhash id ewm--surfaces))))

(defun ewm--handle-minimize-surface (event)
  "Handle minimize surface EVENT.
Buries the surface buffer, causing it to be hidden by the layout system."
  (let ((id (alist-get 'id event)))
    (when-let* ((buf (gethash id ewm--surfaces)))
      (when (buffer-live-p buf)
        (with-current-buffer buf
          (bury-buffer))))))

(defun ewm--handle-maximize-request (_event)
  "Handle maximize request EVENT.
No-op: tiling layout already manages window sizes."
  nil)

(defun ewm--handle-unmaximize-request (_event)
  "Handle unmaximize request EVENT.
No-op: tiling layout has no maximized state to restore."
  nil)

;;; Shell CWD tracking

(defun ewm--cwd-ancestors (pid)
  "Return list of ancestor PIDs for PID by walking /proc PPid chain."
  (let ((ancestors nil)
        (current pid)
        (depth 0))
    (while (and current (< depth 10))
      (push current ancestors)
      (let ((status-file (format "/proc/%d/status" current)))
        (setq current
              (when (file-readable-p status-file)
                (with-temp-buffer
                  (insert-file-contents status-file)
                  (when (re-search-forward "^PPid:\\s-*\\([0-9]+\\)" nil t)
                    (let ((ppid (string-to-number (match-string 1))))
                      (and (> ppid 1) ppid)))))))
      (cl-incf depth))
    ancestors))

(defun ewm--cwd-find-buffer (shell-pid)
  "Find the surface buffer whose PID is an ancestor of SHELL-PID."
  (let ((ancestors (ewm--cwd-ancestors shell-pid))
        (found nil))
    (maphash (lambda (_id buf)
               (when (and (not found)
                          (buffer-live-p buf)
                          (buffer-local-value 'ewm-surface-pid buf)
                          (memq (buffer-local-value 'ewm-surface-pid buf)
                                ancestors))
                 (setq found buf)))
             ewm--surfaces)
    found))

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
  "Handle title update EVENT.
Updates buffer-local variables and renames the buffer."
  (let ((id (alist-get 'id event))
        (app (alist-get 'app event))
        (title (alist-get 'title event)))
    (when-let* ((buf (gethash id ewm--surfaces)))
      (when (buffer-live-p buf)
        (with-current-buffer buf
          (setq-local ewm-surface-app app)
          (setq-local ewm-surface-title title)
          (ewm--rename-buffer)
          (run-hooks 'ewm-update-title-hook))))))

(defun ewm--handle-output-detected (event)
  "Handle output detected EVENT. Creates a frame if needed."
  (let ((name (alist-get 'name event)))
    (setf (alist-get name ewm--output-info nil nil #'string=) event)
    (unless (ewm--frame-for-output name)
      (ewm--create-frame-for-output name))))

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
                     (= (alist-get 'refresh m) new-r)))))))
  (run-hook-with-args 'ewm-output-config-changed-hook event))

(defvar ewm-output-config-changed-hook nil
  "Hook run when output configuration is applied.
Each function receives the event alist with symbol keys:
  name, width, height, refresh, x, y, scale, transform.")

(defun ewm--handle-output-disconnected (event)
  "Handle output disconnected EVENT.
Deletes the frame for that output. Surface buffers remain in Emacs but are
hidden by the compositor since no windows display them."
  (let ((name (alist-get 'name event)))
    (setq ewm--output-info
          (assoc-delete-all name ewm--output-info #'string=))
    (when-let* ((frame (ewm--frame-for-output name)))
      (delete-frame frame t))))

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
Sent after startup enumeration, hotplug, and session resume.
Applies user output config, enforces frame-output parity, then
re-syncs layout and focus with the compositor."
  (ewm--apply-output-config)
  (ewm--enforce-frame-output-parity)
  (ewm-layout--refresh)
  (ewm--sync-focus))

(defun ewm--handle-environment (event)
  "Handle environment event from compositor.
Sets Emacs's `process-environment' so child processes inherit these vars."
  (dolist (pair (alist-get 'vars event))
    (setenv (symbol-name (car pair)) (cdr pair))))

(defun ewm--handle-ready ()
  "Handle ready event from compositor.
Signals that the compositor is fully initialized."
  (setq ewm--compositor-ready t))

(defun ewm--handle-debug-state (event)
  "Handle debug_state event from compositor.
Displays the compositor state in a buffer for debugging."
  (let ((json (alist-get 'json event)))
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
  ;; Frame resize happens automatically via Wayland configure event.
  ;; Layout refresh is triggered by window-size-change-functions when
  ;; the frame actually resizes.
  (ewm--log "Working area for %s: %dx%d+%d+%d"
            (alist-get 'output event) (alist-get 'width event)
            (alist-get 'height event) (alist-get 'x event)
            (alist-get 'y event)))

;;; Clipboard integration

(defvar ewm--last-selection nil
  "Last selection text received from compositor, to avoid echo.")

(defvar ewm--saved-interprogram-cut-function nil
  "Saved value of `interprogram-cut-function' before EWM override.")

(defvar ewm--saved-interprogram-paste-function nil
  "Saved value of `interprogram-paste-function' before EWM override.")

(defun ewm--handle-selection-changed (event)
  "Handle selection-changed EVENT from compositor.
Push the text onto the kill ring."
  (let ((text (alist-get 'text event)))
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

(defun ewm--handle-activate-workspace (event)
  "Handle activate_workspace EVENT from compositor.
Select the Nth Emacs frame on the requested output (1-based index)."
  (unless (ewm--minibuffer-active-p)
    (let* ((output (alist-get 'output event))
           (frame-index (alist-get 'frame_index event))
           (frames (cl-remove-if-not
                    (lambda (f) (equal (frame-parameter f 'ewm-output) output))
                    (frame-list)))
           (target (nth (1- frame-index) frames)))
      (when target
        (select-frame-set-input-focus target)))))

(defun ewm--set-fullscreen-windows (buf value)
  "Set fullscreen window parameter to VALUE on all windows showing BUF."
  (dolist (win (get-buffer-window-list buf nil t))
    (set-window-parameter win 'ewm-fullscreen value)))

(defun ewm--handle-fullscreen-request (event)
  "Handle fullscreen request EVENT: set fullscreen flag and refresh layout.
Sets on all windows showing the surface."
  (let ((id (alist-get 'id event)))
    (when-let* ((buf (gethash id ewm--surfaces))
                (win (get-buffer-window buf t)))
      (ewm--set-fullscreen-windows buf t)
      (ewm-layout--refresh))))

(defun ewm--handle-unfullscreen-request (event)
  "Handle unfullscreen request EVENT: clear fullscreen flag and refresh layout.
Clears on all windows showing the surface."
  (let ((id (alist-get 'id event)))
    (when-let* ((buf (gethash id ewm--surfaces))
                (win (get-buffer-window buf t)))
      (ewm--set-fullscreen-windows buf nil)
      (ewm-layout--refresh))))

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

(defun ewm--send-cursor-hide-config ()
  "Send cursor auto-hide configuration to compositor.
Uses `bound-and-true-p' for the inputs so this is safe to call from a
`:set' that fires before the other defcustom has been evaluated."
  (when (ewm--compositor-active-p)
    (ewm-configure-cursor-hide-module
     (let ((v (bound-and-true-p ewm-cursor-auto-hide)))
       (and (integerp v) v))
     (bound-and-true-p ewm-cursor-hide-when-typing))))

(defun ewm-idle-inhibit (&optional arg)
  "Manually inhibit idle timeout and idle notifications.
With prefix ARG, turn inhibition on.  Without ARG, toggle.
When idle is inhibited, the screen won't blank and swayidle won't
fire regardless of inactivity.  Useful when watching a video or
giving a presentation.

See also `ewm-idle-inhibit-mode' for a minor mode version."
  (interactive "P")
  (let ((new-state (if arg
                       t
                     (not ewm-idle-inhibited))))
    (setq ewm-idle-inhibited new-state)
    (when (ewm--compositor-active-p)
      (ewm-set-idle-inhibited (if new-state 1 nil)))
    (message "Idle inhibition %s" (if new-state "enabled" "disabled"))))

(define-minor-mode ewm-idle-inhibit-mode
  "Minor mode to inhibit idle timeout and notifications.
When enabled, the screen won't blank and swayidle won't fire
regardless of inactivity.  Useful when watching a video or
giving a presentation.

This is the minor-mode equivalent of `ewm-idle-inhibit'."
  :global t
  :variable ewm-idle-inhibited
  (when (ewm--compositor-active-p)
    (ewm-set-idle-inhibited (if ewm-idle-inhibited 1 nil))))

;;; Commands

(defun ewm-lock-session ()
  "Lock the session via loginctl."
  (interactive)
  (start-process "loginctl" nil "loginctl" "lock-session"))

(defun ewm--sorted-surface-buffers ()
  "Return live surface buffers sorted by surface ID."
  (let ((bufs (cl-loop for buf being the hash-values of ewm--surfaces
                       when (buffer-live-p buf) collect buf)))
    (sort bufs (lambda (a b)
                 (< (buffer-local-value 'ewm-surface-id a)
                    (buffer-local-value 'ewm-surface-id b))))))

(defun ewm--cycle-surface-buffer (delta)
  "Switch to surface buffer DELTA steps away from current."
  (let ((bufs (ewm--sorted-surface-buffers))
        (cur (current-buffer)))
    (when (cdr bufs)
      (let* ((pos (cl-position cur bufs))
             (next (nth (if pos (mod (+ pos delta) (length bufs)) 0) bufs)))
        (switch-to-buffer next))))
  (ewm--sync-focus))

(defun ewm-next-surface-buffer ()
  "Switch to the next surface buffer in the current window."
  (interactive)
  (ewm--cycle-surface-buffer 1))

(defun ewm-prev-surface-buffer ()
  "Switch to the previous surface buffer in the current window."
  (interactive)
  (ewm--cycle-surface-buffer -1))

(defun ewm-toggle-fullscreen ()
  "Toggle fullscreen for the surface in the current window."
  (interactive)
  (when ewm-surface-id
    (set-window-parameter nil 'ewm-fullscreen
                          (not (window-parameter nil 'ewm-fullscreen)))
    (ewm-layout--refresh)))

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
  "Set declarative layout for OUTPUT as a nested frame strip.
FRAMES is a vector of plists describing the Emacs frame strip on the
output, left-to-right.  Each frame plist has:
  :name NAME :surface-id ID :width FLOAT :strip-index N :surfaces SURFACES
where SURFACES is a vector of plists `:id :x :y :w :h :focused :fullscreen'.
Surface coordinates are relative to the owning frame's working area."
  (ewm-output-layout-module output frames))

(defun ewm-close (id)
  "Request surface ID to close gracefully."
  (ewm-close-module id))

(defun ewm-warp-pointer (x y)
  "Warp pointer to absolute position X, Y."
  (ewm-warp-pointer-module x y))

(defun ewm-screenshot (&optional path)
  "Take a screenshot of the compositor."
  (interactive)
  (ewm-screenshot-module (or path "/tmp/ewm-screenshot.png")))

(defun ewm-show-state ()
  "Request compositor state dump.
State will be displayed in *ewm-state* buffer when received."
  (interactive)
  (ewm-get-debug-state-module)
  (ewm--log "Requested compositor state..."))

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
    (with-current-buffer (get-buffer-create "*ewm-outputs*")
      (let ((inhibit-read-only t))
        (erase-buffer)
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
              (insert (format "\n  Suggested ewm-output-config entry:\n"))
              (insert (format "    (\"%s\" :width %d :height %d :scale %g)\n"
                              config-key
                              (alist-get 'width  cur)
                              (alist-get 'height cur)
                              scale)))
            (insert "\n")))
        (goto-char (point-min)))
      (display-buffer (current-buffer)))))

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

(defun ewm-prepare-frame (output)
  "Tell compositor to assign next frame to OUTPUT."
  (ewm-prepare-frame-module output))

(defun ewm--get-output-origin (output-name)
  "Return (x . y) origin for OUTPUT-NAME, or (0 . 0) if not found."
  (or (ewm-get-output-origin output-name) '(0 . 0)))

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
  (let ((result nil))
    (dolist (entry ewm--output-info)
      (unless result
        (let ((connector (car entry))
              (info      (cdr entry)))
          (when (or (string= config-name connector)
                    (string-equal-ignore-case config-name
                                              (ewm--output-mms info)))
            (setq result connector)))))
    (or result config-name)))

(defun ewm--apply-output-config ()
  "Apply user output configuration from `ewm-output-config'.
Config keys may be connector names (\"DP-1\") or Make-Model-Serial strings
(\"Dell U2722D ABC123\"); the latter are resolved to the live connector name
via `ewm--find-connector-for-config' before sending to the compositor."
  (dolist (config ewm-output-config)
    (let* ((config-name (car config))
           (props       (cdr config))
           (connector   (ewm--find-connector-for-config config-name))
           (width     (plist-get props :width))
           (height    (plist-get props :height))
           (refresh   (plist-get props :refresh))
           (x         (plist-get props :x))
           (y         (plist-get props :y))
           (scale     (plist-get props :scale))
           (transform (plist-get props :transform)))
      (let ((has-enabled (plist-member props :enabled))
            (enabled     (plist-get props :enabled)))
        (when (or width height refresh scale transform x y has-enabled)
          (apply #'ewm-configure-output connector
                 (append
                  (when width     (list :width width))
                  (when height    (list :height height))
                  (when refresh   (list :refresh refresh))
                  (when x         (list :x x))
                  (when y         (list :y y))
                  (when scale     (list :scale scale))
                  (when transform (list :transform transform))
                  (when has-enabled (list :enabled enabled)))))))))

;;; Frame management

(defun ewm--output-managed-p (frame)
  "Return non-nil if FRAME is bound to a compositor output.
Only output-bound frames participate in frame-output parity.
Excludes TTY frames and (in the future) floating frames."
  (eq (framep-on-display frame) 'pgtk))

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
    (ewm--fixup-display-customs frame)
    (cond
     ((not (ewm--output-managed-p frame))
      nil)
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

(defun ewm--prevent-frame-deletion (orig-fun &optional frame force)
  "Around advice for `delete-frame' to protect output frames.
Prevents deletion of the last frame for an active output unless
FORCE is non-nil.
Frames for disconnected outputs are always allowed to be deleted."
  (let* ((f (or frame (selected-frame)))
         (output (frame-parameter f 'ewm-output)))
    (if (and output (not force)
             (member output (ewm-get-active-outputs))
             (not (cl-some (lambda (other)
                             (and (not (eq other f))
                                  (ewm--output-managed-p other)
                                  (string= output
                                           (frame-parameter other 'ewm-output))))
                           (frame-list))))
        (message "Cannot delete the only frame on output %s" output)
      (funcall orig-fun frame force))))

(defun ewm--frames-for-output (output)
  "Return frames assigned to OUTPUT in left-to-right strip order.
Sort key is `ewm-strip-index'; frames missing it sort first
(usually the first frame ever made for the output)."
  (sort
   (cl-remove-if-not
    (lambda (f) (equal (frame-parameter f 'ewm-output) output))
    (frame-list))
   (lambda (a b)
     (< (or (frame-parameter a 'ewm-strip-index) -1)
        (or (frame-parameter b 'ewm-strip-index) -1)))))

(defun ewm--next-strip-index (output)
  "Return the next free strip index for OUTPUT (one past the max)."
  (1+ (apply #'max -1
             (delq nil
                   (mapcar (lambda (f) (frame-parameter f 'ewm-strip-index))
                           (cl-remove-if-not
                            (lambda (f)
                              (equal (frame-parameter f 'ewm-output) output))
                            (frame-list)))))))

(defun ewm-frame-new ()
  "Append a new Emacs frame to the right of the current output's strip
and focus it once the compositor confirms its surface."
  (interactive)
  (let ((output (frame-parameter (selected-frame) 'ewm-output)))
    (unless output
      (user-error "Current frame has no ewm-output"))
    (let* ((idx (ewm--next-strip-index output))
           (new-frame (ewm--create-frame-for-output output)))
      (set-frame-parameter new-frame 'ewm-strip-index idx)
      (set-frame-parameter new-frame 'ewm-focus-on-create t))))

(defun ewm-frame-close ()
  "Close the selected frame.
The last frame on an active output cannot be deleted; this is
enforced by `ewm--prevent-frame-deletion'."
  (interactive)
  (delete-frame))

(defun ewm--frame-shift-on-output (delta)
  "Move strip focus by DELTA on the current output (no wrap-around).
Returns the target frame on success, nil at the strip edge."
  (let* ((current (selected-frame))
         (output (frame-parameter current 'ewm-output))
         (frames (ewm--frames-for-output output))
         (idx (cl-position current frames))
         (target-idx (and idx (+ idx delta))))
    (when (and target-idx (>= target-idx 0) (< target-idx (length frames)))
      (let ((target (nth target-idx frames)))
        (select-frame-set-input-focus target)
        target))))

(defun ewm--focus-edge-window (direction)
  "Select the leftmost (`left') or rightmost (`right') window of the frame.
Picks by window x-edge in a single pass so only one `select-window'
call happens -- a no-op when the saved selected window already is the
outmost one."
  (select-window
   (car (seq-sort-by (lambda (w) (car (window-edges w)))
                     (if (eq direction 'left) #'< #'>)
                     (window-list nil 'no-minibuf)))))

(defun ewm-windmove-left ()
  "Move to the Emacs window to the left, or slide to the previous frame.
When `windmove-left' would error and a previous frame exists on the
output's strip, slide to it and focus its leftmost window so that
pressing `s-<left>' again continues straight into the frame before it.

This \"outmost\" landing optimizes cross-frame traversal over
window-by-window inspection.  Use `s-<right>' to step back."
  (interactive)
  (condition-case err
      (windmove-left)
    (user-error
     (if (ewm--frame-shift-on-output -1)
         (ewm--focus-edge-window 'left)
       (signal (car err) (cdr err))))))

(defun ewm-windmove-right ()
  "Move to the Emacs window to the right, or slide to the next frame.
When `windmove-right' would error and a next frame exists on the
output's strip, slide to it and focus its rightmost window so that
pressing `s-<right>' again continues straight into the frame after it."
  (interactive)
  (condition-case err
      (windmove-right)
    (user-error
     (if (ewm--frame-shift-on-output 1)
         (ewm--focus-edge-window 'right)
       (signal (car err) (cdr err))))))

(defun ewm-frame-next ()
  "Switch to the next frame on the current output.  Linear strip, no wrap."
  (interactive)
  (ewm--frame-shift-on-output 1))

(defun ewm-frame-previous ()
  "Switch to the previous frame on the current output.  Linear strip, no wrap."
  (interactive)
  (ewm--frame-shift-on-output -1))

(defun ewm-frame-select ()
  "Select the Nth frame (1-based) on the current output.
Reads the digit from `last-command-event'.  No-op if N is past the
number of frames on this output."
  (interactive)
  (let* ((pressed (- (event-basic-type last-command-event) ?0))
         (output (frame-parameter (selected-frame) 'ewm-output))
         (frames (ewm--frames-for-output output))
         (target (and (>= pressed 1) (<= pressed (length frames))
                      (nth (1- pressed) frames))))
    (when target
      (select-frame-set-input-focus target))))

(defun ewm--frame-swap-on-output (delta)
  "Swap the active frame's strip position with its DELTA neighbor.
Returns the neighbor on success, nil at the strip edge.  Focus stays
on the same surface; the compositor snaps view_offset rather than
animating, so the strip rearranges around the user without scrolling."
  (let* ((current (selected-frame))
         (output (frame-parameter current 'ewm-output))
         (frames (ewm--frames-for-output output))
         (idx (cl-position current frames))
         (target-idx (and idx (+ idx delta)))
         (target (and target-idx (>= target-idx 0) (< target-idx (length frames))
                      (nth target-idx frames))))
    (when target
      (let ((cur-idx (frame-parameter current 'ewm-strip-index))
            (tgt-idx (frame-parameter target 'ewm-strip-index)))
        (set-frame-parameter current 'ewm-strip-index tgt-idx)
        (set-frame-parameter target 'ewm-strip-index cur-idx)
        (ewm-layout--refresh)
        target))))

(defun ewm-frame-move-left ()
  "Swap the selected frame with its left neighbor in the strip."
  (interactive)
  (ewm--frame-swap-on-output -1))

(defun ewm-frame-move-right ()
  "Swap the selected frame with its right neighbor in the strip."
  (interactive)
  (ewm--frame-swap-on-output 1))

(defun ewm--enforce-frame-output-parity ()
  "Delete orphan/stale frames but keep N frames per output.
Frames may live on a strip multiple-deep; the duplicate-frame rule
that one-output-one-frame implied is gone -- the user explicitly
manages the strip via `ewm-frame-new'/`ewm-frame-close'."
  (let ((active-outputs (ewm-get-active-outputs)))
    (dolist (frame (frame-list))
      (let ((output (frame-parameter frame 'ewm-output)))
        (cond
         ((not (ewm--output-managed-p frame))
          nil)
         ((rassq frame ewm--pending-frame-outputs)
          nil)
         ((null output)
          (ignore-errors (delete-frame frame)))
         ((not (member output active-outputs))
          (ignore-errors (delete-frame frame))))))))

;;; Public API

(defun ewm--disable-csd ()
  "Disable client-side decorations and bars for all frames.
Sets frames to undecorated mode and removes bars since EWM manages
windows directly."
  ;; Set current frame to undecorated
  (set-frame-parameter nil 'undecorated t)
  ;; Ensure future frames are also undecorated
  (add-to-list 'default-frame-alist '(undecorated . t))
  ;; Disable menu-bar, tool-bar, and scroll-bar if enabled.
  ;; These add to the Y-offset and must be accounted for.
  (when (bound-and-true-p menu-bar-mode)
    (menu-bar-mode -1))
  (when (bound-and-true-p tool-bar-mode)
    (tool-bar-mode -1))
  (when (bound-and-true-p scroll-bar-mode)
    (scroll-bar-mode -1)))

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
  ;; Set up pipe channel before starting compositor so no events are lost
  (ewm--init-event-channel)
  (if (ewm-start)
      (progn
        (setq ewm--module-mode t)
        ;; Enable EWM mode (needed for frame creation hooks)
        (ewm-mode 1)
        ;; Wait for compositor ready event (with timeout)
        (let ((timeout 300))  ; 30 seconds max
          (while (and (> timeout 0)
                      (not ewm--compositor-ready))
            (accept-process-output ewm--event-process 0.1)
            (cl-decf timeout))
          (unless ewm--compositor-ready
            (ewm--destroy-event-channel)
            (ewm-mode -1)
            (setq ewm--module-mode nil)
            (error "Compositor failed to become ready")))
        ;; Tell the compositor when init is fully done so it can replay the
        ;; first wl_keyboard.enter (PGTK drops it pre-roundtrip).
        (if after-init-time
            (run-with-idle-timer 0 nil #'ewm-notify-initialized-module)
          (add-hook 'emacs-startup-hook #'ewm-notify-initialized-module))
        ;; Color the GTK widget background so new frames don't flash white
        ;; before redisplay paints. Re-apply on theme change.
        (ewm--apply-gtk-bg-css)
        (add-hook 'enable-theme-functions #'ewm--apply-gtk-bg-css))
    (ewm--destroy-event-channel)
    (error "Failed to start compositor")))

(defun ewm-stop-module ()
  "Stop EWM module mode compositor."
  (interactive)
  (when ewm--module-mode
    (when (and (fboundp 'ewm-stop) (fboundp 'ewm-running) (ewm-running))
      (ewm-stop)
      (let ((timeout 50))
        (while (and (> timeout 0) (ewm-running))
          (sleep-for 0.1)
          (cl-decf timeout))))
    (ewm--destroy-event-channel)
    (setq ewm--module-mode nil)
    (setq ewm--compositor-ready nil)
    (setq ewm--output-info nil)
    (ewm-mode -1)))

(declare-function ewm-gtk-set-style "ewm-core")

(defun ewm--apply-gtk-bg-css (&rest _)
  "Push the default face background to the GTK widget for new frames.
Without this, pgtk's first commit renders the widget's white default
before redisplay paints (bug#53232)."
  (when-let* ((bg (face-attribute 'default :background))
              ((stringp bg)))
    (ignore-errors
      (ewm-gtk-set-style (format "window { background-color: %s; }" bg)))))

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
Only async functions need tokens. `call-process' is synchronous and
used for CLI tools (git, grep, etc.) that never consume activation tokens.")

(defun ewm--enable-process-advice ()
  "Enable automatic activation token injection for spawned processes."
  (dolist (fn ewm--process-functions)
    (advice-add fn :around #'ewm--inject-activation-token)))

(defun ewm--disable-process-advice ()
  "Disable automatic activation token injection."
  (dolist (fn ewm--process-functions)
    (advice-remove fn #'ewm--inject-activation-token)))

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
  (let* ((apps (ewm-list-xdg-apps))
         (names (mapcar #'car apps))
         (name (completing-read "Launch app: " names nil t)))
    (ewm-launch-xdg-command name)))

(defun ewm--fixup-display-customs (frame)
  "Re-evaluate defcustom defaults that got nil during headless init.
EWM starts Emacs as a foreground daemon, so display-probing functions
return nil when init.el loads packages.  Re-evaluate affected defaults
now that a graphical FRAME exists."
  (when (display-graphic-p frame)
    (let ((probe-re (regexp-opt '("image-transforms-p"
                                  "face-valid-attribute-values"
                                  "display-graphic-p"
                                  "display-images-p"))))
      (mapatoms
       (lambda (sym)
         (when-let* ((std (get sym 'standard-value))
                     (form (car-safe std)))
           (when (and (boundp sym)
                      (null (symbol-value sym))
                      (not (get sym 'saved-value))
                      (not (get sym 'customized-value))
                      (string-match-p probe-re (prin1-to-string form)))
             (custom-reevaluate-setting sym))))))))


;;; Global minor mode

(defun ewm--mode-enable ()
  "Enable EWM integration."
  (ewm--disable-csd)
  (ewm--enable-layout-sync)
  (ewm-input--enable)
  (if after-init-time
      (ewm--send-intercept-keys)
    (add-hook 'after-init-hook #'ewm--send-intercept-keys))
  (ewm--send-input-config)
  (ewm--send-idle-config)
  (ewm--send-cursor-hide-config)
  (ewm-set-unfocused-alpha (float ewm-unfocused-alpha))
  (when (and (not ewm-focus-follows-mouse) mouse-autoselect-window)
    (customize-set-variable 'ewm-focus-follows-mouse t))
  (ewm-text-input-auto-mode-enable)
  (ewm--enable-process-advice)
  (setq ewm--saved-interprogram-cut-function interprogram-cut-function)
  (setq interprogram-cut-function #'ewm--interprogram-cut-function)
  (setq ewm--saved-interprogram-paste-function interprogram-paste-function)
  (setq interprogram-paste-function nil)
  (add-hook 'after-make-frame-functions #'ewm--on-make-frame)
  (advice-add 'delete-frame :around #'ewm--prevent-frame-deletion)
  (add-hook 'kill-emacs-hook #'ewm--kill-emacs-hook))

(defun ewm--mode-disable ()
  "Disable EWM integration."
  (ewm--disable-layout-sync)
  (ewm-input--disable)
  (ewm-text-input-auto-mode-disable)
  (ewm--disable-process-advice)
  (setq interprogram-cut-function ewm--saved-interprogram-cut-function)
  (setq interprogram-paste-function ewm--saved-interprogram-paste-function)
  (remove-hook 'after-make-frame-functions #'ewm--on-make-frame)
  (advice-remove 'delete-frame #'ewm--prevent-frame-deletion)
  (remove-hook 'kill-emacs-hook #'ewm--kill-emacs-hook)
  ;; Stop module mode if active
  (when ewm--module-mode
    (when (and (fboundp 'ewm-stop) (fboundp 'ewm-running) (ewm-running))
      (ewm-stop))
    (ewm--destroy-event-channel)
    (setq ewm--module-mode nil)))

;;;###autoload
(define-minor-mode ewm-mode
  "Global minor mode for EWM compositor integration."
  :global t
  :lighter " ewm"
  :keymap ewm-mode-map
  :group 'ewm
  (if ewm-mode
      (ewm--mode-enable)
    (ewm--mode-disable)))

(provide 'ewm)
;;; ewm.el ends here
