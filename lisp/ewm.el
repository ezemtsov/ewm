;;; ewm.el --- Emacs Wayland Manager -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;; Package-Requires: ((emacs "28.1") (transient "0.4"))

;;; Commentary:

;; EWM integrates Emacs with a Wayland compositor, providing an EXWM-like
;; experience without the single-threaded limitations.
;;
;; Quick start (compositor spawns Emacs automatically):
;;   EWM_INIT=/path/to/lisp/ewm.el ewm
;;
;; Or with custom Emacs args:
;;   EWM_INIT=/path/to/lisp/ewm.el ewm -Q --eval "(load-theme 'modus-vivendi)"
;;
;; Manual startup:
;;   1. Start compositor: ewm --no-auto-emacs
;;   2. Start Emacs inside: WAYLAND_DISPLAY=wayland-ewm emacs -l lisp/ewm.el -f ewm-connect
;;
;; Start apps inside the compositor:
;;   WAYLAND_DISPLAY=wayland-ewm foot
;;
;; Surfaces automatically align with the Emacs window displaying their buffer.
;;
;; Input handling (like EXWM):
;;   When viewing a surface buffer, typing goes directly to the surface.
;;   Prefix keys (C-x, M-x, etc.) are intercepted and go to Emacs.
;;
;; Environment variables:
;;   EWM_EMACS - Path to Emacs binary (default: "emacs")
;;   EWM_INIT  - Path to lisp/ewm.el (auto-loads and connects)

;;; Code:

(require 'cl-lib)
(require 'map)

;; Module function declarations (provided by ewm-core dynamic module)
(declare-function ewm-start "ewm-core")
(declare-function ewm-stop "ewm-core")
(declare-function ewm-running "ewm-core")
(declare-function ewm-pop-event "ewm-core")
(declare-function ewm-drain-events "ewm-core")
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
(declare-function ewm-intercept-keys-module "ewm-core")
(declare-function ewm-im-commit-module "ewm-core")
(declare-function ewm-text-input-intercept-module "ewm-core")
(declare-function ewm-configure-xkb-module "ewm-core")
(declare-function ewm-switch-layout-module "ewm-core")
(declare-function ewm-get-layouts-module "ewm-core")

;;; Dynamic module loading

(defconst ewm--dir
  (file-name-directory
   (directory-file-name
    (file-name-directory (or load-file-name ""))))
  "Root directory of the EWM project.
When loaded from lisp/, this resolves to the parent directory.")

(defun ewm--find-module-dir ()
  "Find the directory containing libewm_core.so.
Prefers release build over debug build."
  (or (getenv "EWM_MODULE_PATH")
      ;; Development: prefer release, fall back to debug
      (let ((release-dir (expand-file-name "compositor/target/release" ewm--dir))
            (debug-dir (expand-file-name "compositor/target/debug" ewm--dir)))
        (cond
         ((file-exists-p (expand-file-name "libewm_core.so" release-dir))
          release-dir)
         ((file-exists-p (expand-file-name "libewm_core.so" debug-dir))
          debug-dir)))
      ;; Installed: same directory as ewm.el
      ewm--dir))

(defcustom ewm-module-dir nil
  "Directory containing the ewm-core dynamic module.
If nil, automatically detected (preferring release over debug).
Set EWM_MODULE_PATH environment variable to override."
  :type '(choice (const :tag "Auto-detect" nil)
                 directory)
  :group 'ewm)

(defvar ewm--loaded-module-path nil
  "Path to the currently loaded ewm-core module.
Used to detect debug/release build mismatches.")

(defun ewm-load-module ()
  "Load the ewm-core dynamic module.
Uses `ewm-module-dir' if set, otherwise auto-detects (preferring release).
Returns t if loaded successfully, nil otherwise."
  (interactive)
  (if (featurep 'ewm-core)
      (progn (message "ewm-core already loaded from %s" ewm--loaded-module-path) t)
    (let* ((dir (or ewm-module-dir (ewm--find-module-dir)))
           (module-path (expand-file-name "libewm_core.so" dir))
           (is-debug (string-match-p "/debug/" module-path)))
      (if (not (file-exists-p module-path))
          (progn
            (message "Module not found: %s" module-path)
            nil)
        (condition-case err
            (progn
              (module-load module-path)
              (setq ewm--loaded-module-path module-path)
              (message "Loaded ewm-core (%s build) from %s"
                       (if is-debug "debug" "release")
                       module-path)
              ;; Warn prominently if debug build is loaded
              (when is-debug
                (display-warning 'ewm
                  (format "Loaded DEBUG build of ewm-core from:\n%s\n\nIf you're developing, rebuild with 'cargo build' (not --release).\nRestart Emacs after rebuilding to load the new module."
                          module-path)
                  :warning))
              t)
          (error
           (message "Failed to load ewm-core: %s" (error-message-string err))
           nil))))))

(defun ewm-module-info ()
  "Display information about the loaded ewm-core module."
  (interactive)
  (if ewm--loaded-module-path
      (let* ((is-debug (string-match-p "/debug/" ewm--loaded-module-path))
             (mtime (file-attribute-modification-time
                     (file-attributes ewm--loaded-module-path)))
             (mtime-str (format-time-string "%Y-%m-%d %H:%M:%S" mtime)))
        (message "ewm-core: %s build, loaded from %s (built %s)"
                 (if is-debug "DEBUG" "RELEASE")
                 ewm--loaded-module-path
                 mtime-str))
    (message "ewm-core module not loaded")))

;;; Module mode (compositor runs in-process)

(defvar ewm--module-mode nil
  "Non-nil when running in module mode (compositor in-process).")

(defvar ewm--module-timer nil
  "Timer for polling module events.")

(defconst ewm--module-poll-interval 0.016
  "Interval for polling module events (16ms = ~60Hz).")

(defun ewm--compositor-active-p ()
  "Return non-nil if compositor is active (module or IPC mode)."
  (or ewm--module-mode
      (and ewm--process (process-live-p ewm--process))))

(defun ewm--start-module-polling ()
  "Start timer-based event polling for module mode."
  (ewm--stop-module-polling)
  (setq ewm--module-timer
        (run-with-timer 0 ewm--module-poll-interval #'ewm--poll-module-events)))

(defun ewm--stop-module-polling ()
  "Stop module event polling timer."
  (when ewm--module-timer
    (cancel-timer ewm--module-timer)
    (setq ewm--module-timer nil)))

(defun ewm--poll-module-events ()
  "Poll and process events from the module.
Called periodically by `ewm--module-timer'."
  (when (and ewm--module-mode
             (fboundp 'ewm-running)
             (ewm-running))
    ;; Process all pending events
    (while-let ((event (ewm-pop-event)))
      (ewm--handle-module-event event))
    ;; Drain the notification pipe
    (when (fboundp 'ewm-drain-events)
      (ewm-drain-events))))

(defun ewm--alist-to-hash (alist)
  "Convert ALIST to a hash table for compatibility with existing handlers.
The handlers expect hash tables with string keys."
  (let ((hash (make-hash-table :test 'equal)))
    (dolist (pair alist)
      (let ((key (car pair)))
        ;; Key might be a symbol or string depending on source
        (puthash (if (symbolp key) (symbol-name key) key)
                 (cdr pair) hash)))
    hash))

(defun ewm--handle-module-event (event)
  "Handle EVENT from the module.
EVENT is an alist, which we convert to a hash table for the existing handlers."
  (when event
    (let ((hash (ewm--alist-to-hash event)))
      (ewm--handle-event hash))))

;;; Debug logging

(defvar ewm-debug-log-file "/tmp/ewm-emacs.log"
  "File to write EWM debug logs to.")

(defvar ewm-debug-logging nil
  "When non-nil, write debug messages to `ewm-debug-log-file'.")

(defvar ewm-debug-focus nil
  "When non-nil, log detailed focus change tracing.")

(defun ewm--log-to-buffer (msg)
  "Append MSG to *ewm-info* buffer."
  (let ((buf (get-buffer-create "*ewm-info*")))
    (with-current-buffer buf
      (goto-char (point-max))
      (insert (format-time-string "[%H:%M:%S] ") msg "\n"))))

(defun ewm-log (format-string &rest args)
  "Log FORMAT-STRING with ARGS to *ewm-info* buffer."
  (let ((msg (apply #'format format-string args)))
    (ewm--log-to-buffer msg)
    (when ewm-debug-logging
      (with-temp-buffer
        (insert (format-time-string "[%Y-%m-%d %H:%M:%S] ")
                msg "\n")
        (write-region (point-min) (point-max) ewm-debug-log-file t 'silent)))))

(defun ewm--focus-log (source format-string &rest args)
  "Log focus-related message from SOURCE with FORMAT-STRING and ARGS.
Only logs when `ewm-debug-focus' is non-nil."
  (when ewm-debug-focus
    (let ((msg (format "FOCUS [%s]: %s" source (apply #'format format-string args))))
      (ewm--log-to-buffer msg))))

;;; Input state struct (defined early for use in event handlers)

(cl-defstruct (ewm-input-state (:constructor ewm-input-state-create))
  "State for EWM input handling."
  (last-focused-id 1)
  (last-selected-window nil)
  (mff-last-window nil)
  (compositor-focus nil)
  (focus-timer nil)
  (pending-focus-id nil)
  (inhibit-update nil)
  (inhibit-timer nil))

(defvar ewm--input-state nil
  "Current input state, or nil if not connected.")

(defconst ewm--focus-inhibit-delay 0.05
  "Seconds to inhibit focus updates after compositor-initiated focus change.
Prevents feedback loops when compositor redirects focus during key interception.")

(defun ewm-input--set-inhibit ()
  "Set focus inhibit flag and schedule its clearing.
Cancels any existing inhibit timer to ensure correct timing."
  (when-let ((state ewm--input-state))
    (when-let ((timer (ewm-input-state-inhibit-timer state)))
      (cancel-timer timer))
    (setf (ewm-input-state-inhibit-update state) t)
    (setf (ewm-input-state-inhibit-timer state)
          (run-with-timer ewm--focus-inhibit-delay nil
                          (lambda ()
                            (when ewm--input-state
                              (setf (ewm-input-state-inhibit-update ewm--input-state) nil)
                              (setf (ewm-input-state-inhibit-timer ewm--input-state) nil)))))))

(defgroup ewm nil
  "Emacs Wayland Manager."
  :group 'environment)

(defcustom ewm-mouse-follows-focus t
  "Whether the mouse pointer follows focus changes.
When non-nil, warps the pointer to the center of the focused window."
  :type 'boolean
  :group 'ewm)

(defvar ewm--process nil
  "Network process for compositor connection.")

(defvar ewm--surfaces (make-hash-table :test 'eql)
  "Hash table mapping surface ID to surface info.")

(defvar ewm--outputs nil
  "List of detected outputs.
Each output is a plist with keys:
  :name - connector name (e.g., \"HDMI-A-1\")
  :make - manufacturer
  :model - model name
  :width-mm - physical width in mm
  :height-mm - physical height in mm
  :x - position x
  :y - position y
  :modes - list of available modes")


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

(defun ewm--send (cmd)
  "Send CMD as JSON to compositor."
  (when (and ewm--process (process-live-p ewm--process))
    (let ((json (json-serialize cmd)))
      (process-send-string ewm--process (concat json "\n")))))

(defun ewm--handle-event (event)
  "Handle EVENT from compositor."
  (let ((type (gethash "event" event)))
    (pcase type
      ("new" (ewm--handle-new-surface event))
      ("close" (ewm--handle-close-surface event))
      ("title" (ewm--handle-title-update event))
      ("focus" (ewm--handle-focus event))
      ("output_detected" (ewm--handle-output-detected event))
      ("output_disconnected" (ewm--handle-output-disconnected event))
      ("outputs_complete" (ewm--handle-outputs-complete))
      ("layouts" (ewm--handle-layouts event))
      ("layout-switched" (ewm--handle-layout-switched event))
      ("text-input-activated" (ewm--handle-text-input-activated))
      ("text-input-deactivated" (ewm--handle-text-input-deactivated))
      ("key" (ewm--handle-key event))
      (_ (ewm-log "unknown event type: %s" type)))))

;;; Event handlers

(defcustom ewm-manage-focus-new-surface t
  "Whether to automatically focus new surfaces.
When non-nil, new surface buffers are displayed and selected.
Adapted from EXWM's behavior."
  :type 'boolean
  :group 'ewm)

(defun ewm--cleanup-orphan-frames ()
  "Delete frames that have no ewm-output assigned.
Uses ignore-errors since Emacs won't delete the last visible frame."
  (ewm-log "cleaning up orphan frames")
  (dolist (f (frame-list))
    (unless (frame-parameter f 'ewm-output)
      (ewm-log "deleting orphan frame %s" f)
      (ignore-errors (delete-frame f)))))

(defun ewm--assign-pending-frame (id output pending)
  "Assign surface ID to PENDING frame for OUTPUT."
  (let ((frame (cdr pending)))
    (ewm-log "assigning surface %d to pending frame %s for %s" id frame output)
    (setq ewm--pending-frame-outputs (delete pending ewm--pending-frame-outputs))
    (set-frame-parameter frame 'ewm-output output)
    (set-frame-parameter frame 'ewm-surface-id id)
    (ewm-log "frame on %s (surface %d) - assignment complete" output id)
    ;; Clean up orphans once all pending frames are assigned
    (when (null ewm--pending-frame-outputs)
      (ewm--cleanup-orphan-frames))))

(defun ewm--create-surface-buffer (id app output)
  "Create buffer for regular surface ID with APP on OUTPUT."
  (let ((buf (generate-new-buffer (format "*ewm:%s:%d*" app id))))
    (puthash id `(:buffer ,buf :app ,app) ewm--surfaces)
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
          (pop-to-buffer-same-window buf))))
    (ewm-log "new surface %d (%s) on %s" id app (or output "current"))))

(defun ewm--handle-new-surface (event)
  "Handle new surface EVENT.
If there's a pending frame for this output, this is an Emacs frame.
Otherwise, creates a buffer for external surface."
  (pcase-let (((map ("id" id) ("app" app) ("output" output)) event))
    (ewm-log "new-surface id=%d app=%s output=%s" id app output)
    (let ((pending (and output (assoc output ewm--pending-frame-outputs))))
      (ewm-log "pending-frame-outputs=%s, found pending=%s"
               ewm--pending-frame-outputs pending)
      (if pending
          (ewm--assign-pending-frame id output pending)
        (ewm--create-surface-buffer id app output)))))

(defun ewm--handle-close-surface (event)
  "Handle close surface EVENT.
Kills the surface buffer and focuses Emacs."
  (pcase-let (((map ("id" id)) event))
    (let ((info (gethash id ewm--surfaces)))
      (when info
        (let ((buf (plist-get info :buffer)))
          (when (buffer-live-p buf)
            (with-current-buffer buf
              (remove-hook 'kill-buffer-query-functions
                           #'ewm--kill-buffer-query-function t))
            (kill-buffer buf)))
        (remhash id ewm--surfaces)))
    (ewm-log "closed surface %d" id)))

(defun ewm--handle-focus (event)
  "Handle focus EVENT from compositor.
Updates focus tracking and selects the window displaying the surface's buffer."
  (pcase-let (((map ("id" id)) event))
    (ewm--focus-log "compositor" "received focus event for id=%d" id)
    ;; Always update last-focused-id to match compositor's actual focus.
    ;; This is critical when compositor redirects focus (e.g., during prefix
    ;; key interception) so EWM's tracking stays in sync.
    (when ewm--input-state
      (setf (ewm-input-state-last-focused-id ewm--input-state) id)
      ;; Cancel any pending focus timer to prevent feedback loop.
      ;; When compositor redirects focus (e.g., C-x intercept), a focus timer
      ;; for the old surface may already be scheduled.
      (when-let ((timer (ewm-input-state-focus-timer ewm--input-state)))
        (cancel-timer timer)
        (setf (ewm-input-state-focus-timer ewm--input-state) nil)
        (setf (ewm-input-state-pending-focus-id ewm--input-state) nil))
      ;; Inhibit new focus updates briefly.
      (ewm-input--set-inhibit))
    (let ((info (gethash id ewm--surfaces)))
      (when info
        (let ((buf (plist-get info :buffer)))
          (when (buffer-live-p buf)
            ;; Find a window showing this buffer and select it
            (let ((win (get-buffer-window buf t)))
              (when win
                (ewm--focus-log "compositor" "selecting window for %s (id=%d)"
                                (buffer-name buf) id)
                ;; Suppress mouse-follows-focus since this came from a click
                (when ewm--input-state
                  (setf (ewm-input-state-compositor-focus ewm--input-state) t))
                (unwind-protect
                    (progn
                      (select-frame-set-input-focus (window-frame win))
                      (select-window win))
                  (when ewm--input-state
                    (setf (ewm-input-state-compositor-focus ewm--input-state) nil)))))))))))

(defcustom ewm-update-title-hook nil
  "Normal hook run when a surface's title is updated.
Similar to `exwm-update-title-hook'.
The current buffer is the surface buffer when this runs."
  :type 'hook
  :group 'ewm)

(defun ewm--handle-title-update (event)
  "Handle title update EVENT.
Updates buffer-local variables and renames the buffer.
Adapted from EXWM's title update mechanism."
  (pcase-let (((map ("id" id) ("app" app) ("title" title)) event))
    (when-let ((info (gethash id ewm--surfaces)))
      (let ((buf (plist-get info :buffer)))
        (when (buffer-live-p buf)
          (with-current-buffer buf
            ;; Update buffer-local variables
            (setq-local ewm-surface-app app)
            (setq-local ewm-surface-title title)
            ;; Rename buffer based on app and title
            (ewm--rename-buffer)
            ;; Run user hooks for customization
            (run-hooks 'ewm-update-title-hook))
          ;; Update cached info
          (puthash id `(:buffer ,buf :app ,app :title ,title) ewm--surfaces))))))

(defun ewm--handle-output-detected (event)
  "Handle output detected EVENT.
Adds the output to `ewm--outputs' and creates a frame if needed."
  (pcase-let (((map ("name" name) ("make" make) ("model" model)
                    ("width_mm" width-mm) ("height_mm" height-mm)
                    ("x" x) ("y" y) ("modes" modes)) event))
    (let* (;; Convert modes from hash tables to plists
           (mode-plists (mapcar (lambda (m)
                                  (pcase-let (((map ("width" width) ("height" height)
                                                    ("refresh" refresh) ("preferred" preferred)) m))
                                    (list :width width
                                          :height height
                                          :refresh refresh
                                          :preferred preferred)))
                                (append modes nil)))
           (output-plist (list :name name
                               :make make
                               :model model
                               :width-mm width-mm
                               :height-mm height-mm
                               :x x
                               :y y
                               :modes mode-plists)))
      ;; Remove existing entry with same name (update case)
      (setq ewm--outputs (cl-remove-if (lambda (o) (equal (plist-get o :name) name))
                                       ewm--outputs))
      ;; Add new output
      (push output-plist ewm--outputs)
      (ewm-log "output detected: %s at (%d, %d)" name x y)
      ;; Create frame if this output doesn't have one yet
      (unless (ewm--frame-for-output name)
        (ewm--create-frame-for-output name)))))

(defun ewm--handle-output-disconnected (event)
  "Handle output disconnected EVENT.
Removes the output from `ewm--outputs' and closes its frame."
  (pcase-let (((map ("name" name)) event))
    (setq ewm--outputs (cl-remove-if (lambda (o) (equal (plist-get o :name) name))
                                     ewm--outputs))
    ;; Find and delete frame for this output
    (when-let ((frame (ewm--frame-for-output name)))
      ;; Move windows to another frame before deletion
      (let ((target-frame (car (cl-remove frame (frame-list)))))
        (when target-frame
          (dolist (window (window-list frame))
            (let ((buf (window-buffer window)))
              (with-selected-frame target-frame
                (switch-to-buffer buf))))))
      (delete-frame frame))
    (ewm-log "output disconnected: %s" name)))

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

;;; Commands (dual-mode: IPC or module)

(defun ewm-layout (id x y w h)
  "Set surface ID position to X Y and size to W H."
  (if ewm--module-mode
      (ewm-layout-module id x y w h)
    (ewm--send `(:cmd "layout" :id ,id :x ,x :y ,y :w ,w :h ,h))))

(defun ewm-views (id views)
  "Set surface ID to display at multiple VIEWS.
VIEWS is a vector of plists with :x :y :w :h :active keys.
The :active view receives input, others are visual copies."
  (if ewm--module-mode
      (ewm-views-module id views)
    (ewm--send `(:cmd "views" :id ,id :views ,views))))

(defun ewm-hide (id)
  "Hide surface ID (move offscreen)."
  (if ewm--module-mode
      (ewm-hide-module id)
    (ewm--send `(:cmd "hide" :id ,id))))

(defun ewm-close (id)
  "Request surface ID to close gracefully.
Sends xdg_toplevel.close to the client."
  (if ewm--module-mode
      (ewm-close-module id)
    (ewm--send `(:cmd "close" :id ,id))))

(defun ewm-focus (id)
  "Focus surface ID."
  (if ewm--module-mode
      (ewm-focus-module id)
    (ewm--send `(:cmd "focus" :id ,id))))

(defun ewm-warp-pointer (x y)
  "Warp pointer to absolute position X, Y."
  (if ewm--module-mode
      (ewm-warp-pointer-module (float x) (float y))
    (ewm--send `(:cmd "warp-pointer" :x ,x :y ,y))))

(defun ewm-screenshot (&optional path)
  "Take a screenshot of the compositor.
Saves to PATH, or /tmp/ewm-screenshot.png by default."
  (interactive)
  (let ((target (or path "/tmp/ewm-screenshot.png")))
    (if ewm--module-mode
        (ewm-screenshot-module target)
      (ewm--send `(:cmd "screenshot" :path ,target)))
    (ewm-log "screenshot requested -> %s" target)))

(defun ewm-configure-output (name &rest args)
  "Configure output NAME with ARGS.
ARGS is a plist with optional keys:
  :x :y - position in global coordinate space
  :enabled - t or nil to enable/disable the output
Example:
  (ewm-configure-output \"DP-1\" :x 1920 :y 0)
  (ewm-configure-output \"DP-1\" :enabled nil)"
  (if ewm--module-mode
      (ewm-configure-output-module
       name
       (plist-get args :x)
       (plist-get args :y)
       (plist-get args :width)
       (plist-get args :height)
       (plist-get args :refresh)
       (if (plist-member args :enabled)
           (plist-get args :enabled)
         :unset))
    (let ((cmd `(:cmd "configure-output" :name ,name)))
      (while args
        (let ((key (pop args))
              (val (pop args)))
          (setq cmd (plist-put cmd key val))))
      (ewm--send cmd))))

(defun ewm-assign-output (id output)
  "Assign surface ID to OUTPUT.
The surface will be positioned and sized to fill the output.
Example:
  (ewm-assign-output 1 \"DP-1\")"
  (if ewm--module-mode
      (ewm-assign-output-module id output)
    (ewm--send `(:cmd "assign-output" :id ,id :output ,output))))

(defun ewm-prepare-frame (output)
  "Tell compositor to assign next frame to OUTPUT.
Call this before `make-frame' to have the compositor automatically
assign the new frame's surface to the specified output."
  (if ewm--module-mode
      (ewm-prepare-frame-module output)
    (ewm--send `(:cmd "prepare-frame" :output ,output))))

(defun ewm--get-output-offset (output-name)
  "Return (x . y) offset for OUTPUT-NAME, or (0 . 0) if not found."
  (let ((output (cl-find output-name ewm--outputs
                         :test #'string= :key (lambda (o) (plist-get o :name)))))
    (if output
        (cons (or (plist-get output :x) 0)
              (or (plist-get output :y) 0))
      (cons 0 0))))

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
        (ewm-log "configuring %s to %dx%d" name width height)
        (ewm-configure-output name
                              :width width
                              :height height
                              :refresh refresh
                              :x x
                              :y y)))))

(defun ewm--handle-outputs-complete ()
  "Handle outputs_complete event.
Triggered after compositor sends all output_detected events.
Applies user output config and enforces frame-output parity."
  (ewm-log "all outputs received (%d)" (length ewm--outputs))
  ;; Apply user output configuration (mode changes, positioning)
  (ewm--apply-output-config)
  ;; Enforce 1:1 output-frame relationship
  (ewm--enforce-frame-output-parity))

(defun ewm--handle-layouts (event)
  "Handle layouts EVENT from compositor.
Updates internal tracking of available layouts."
  (pcase-let (((map ("layouts" layouts) ("current" current)) event))
    (setq ewm--xkb-layouts-configured (append layouts nil))
    (setq ewm--xkb-current-layout (nth current ewm--xkb-layouts-configured))
    (ewm-log "layouts configured: %s, current: %s"
             ewm--xkb-layouts-configured ewm--xkb-current-layout)))

(defun ewm--handle-layout-switched (event)
  "Handle layout-switched EVENT from compositor."
  (pcase-let (((map ("layout" layout) ("index" _index)) event))
    (setq ewm--xkb-current-layout layout)
    (ewm-log "layout switched to: %s" layout)))

;;; Text Input Support (EXWM-XIM equivalent)

(defvar ewm-text-input-active nil
  "Non-nil when a client text field is active and expecting input.")

(defun ewm--handle-text-input-activated ()
  "Handle text-input-activated event from compositor.
Called when a client's text field gains focus."
  (ewm-log "text input activated")
  (setq ewm-text-input-active t)
  (run-hooks 'ewm-text-input-activated-hook))

(defun ewm--handle-text-input-deactivated ()
  "Handle text-input-deactivated event from compositor.
Called when a client's text field loses focus."
  (ewm-log "text input deactivated")
  (setq ewm-text-input-active nil)
  (run-hooks 'ewm-text-input-deactivated-hook))

(defvar ewm-text-input-activated-hook nil
  "Hook run when a client text field becomes active.
Use this to enable special input handling modes.")

(defvar ewm-text-input-deactivated-hook nil
  "Hook run when a client text field becomes inactive.")

(defun ewm-im-commit (text)
  "Commit TEXT to the currently focused client text field.
This is the core function for input method support - any text passed here
will be inserted into the client's text field (e.g., Firefox URL bar)."
  (if ewm--module-mode
      (ewm-im-commit-module text)
    (ewm--send `(:cmd "im-commit" :text ,text))))

(defvar ewm-text-input-method nil
  "Input method to use for text input translation.
When nil, uses `current-input-method' or `default-input-method'.")

(defun ewm-text-input--translate-char (char &optional input-method)
  "Translate CHAR through INPUT-METHOD if provided.
If INPUT-METHOD is nil, uses `ewm-text-input-method' or `current-input-method'.
For quail-based input methods, looks up the translation directly."
  (let ((im (or input-method
                ewm-text-input-method
                current-input-method)))
    (if (and im (fboundp 'quail-lookup-key))
        (let ((current-input-method im))
          (activate-input-method im)
          (let ((result (quail-lookup-key (string char))))
            (cond
             ((and (consp result) (integerp (car result)))
              ;; Quail returns (charcode) - convert to string
              (string (car result)))
             ((stringp result) result)
             (t (string char)))))
      (string char))))

(defun ewm-text-input--self-insert ()
  "Handle self-insert when text input mode is active.
Sends the typed character to the client via commit_string,
applying input method translation if active."
  (interactive)
  (let* ((char last-command-event)
         (translated (ewm-text-input--translate-char char)))
    (when (stringp translated)
      (ewm-im-commit translated))))

(defvar ewm-text-input-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map [remap self-insert-command] #'ewm-text-input--self-insert)
    map)
  "Keymap for `ewm-text-input-mode'.")

(define-minor-mode ewm-text-input-mode
  "Minor mode for typing in client text fields.
When enabled, regular keystrokes are sent to the focused client
text field via input method commit_string, while Emacs commands
like C-x and M-x continue to work normally.

Input method translations (e.g., russian-computer) are applied."
  :lighter " TxtIn"
  :keymap ewm-text-input-mode-map)

(defun ewm-text-input-auto-mode-enable ()
  "Enable automatic text input mode switching.
Text input mode will be enabled/disabled automatically when
client text fields gain/lose focus."
  (interactive)
  (add-hook 'ewm-text-input-activated-hook #'ewm-text-input--auto-enable)
  (add-hook 'ewm-text-input-deactivated-hook #'ewm-text-input--auto-disable))

(defun ewm-text-input-auto-mode-disable ()
  "Disable automatic text input mode switching."
  (interactive)
  (remove-hook 'ewm-text-input-activated-hook #'ewm-text-input--auto-enable)
  (remove-hook 'ewm-text-input-deactivated-hook #'ewm-text-input--auto-disable)
  (ewm-text-input-mode -1))

(defun ewm-text-input-intercept (enabled)
  "Enable or disable text input key interception.
When ENABLED, the compositor sends all printable keys to Emacs
instead of the focused surface. Emacs translates via input method
and sends back via `ewm-im-commit'."
  (if ewm--module-mode
      (ewm-text-input-intercept-module (if (eq enabled :false) nil enabled))
    (ewm--send `(:cmd "text-input-intercept" :enabled ,enabled))))

(defun ewm--handle-key (event)
  "Handle key event from compositor.
Called when text-input-intercept is enabled and a printable key is pressed."
  (pcase-let (((map ("utf8" utf8)) event))
    (when utf8
      ;; Get input method from the focused surface buffer
      (let* ((surface-buf (ewm--focused-surface-buffer))
             (im (when surface-buf
                   (buffer-local-value 'current-input-method surface-buf)))
             (translated (ewm-text-input--translate-char (string-to-char utf8) im)))
        (ewm-im-commit translated)))))

(defun ewm--focused-surface-buffer ()
  "Return the buffer displaying the currently focused surface."
  (when-let ((state ewm--input-state))
    (let ((focused-id (ewm-input-state-last-focused-id state)))
      (cl-find-if (lambda (buf)
                    (eq (buffer-local-value 'ewm-surface-id buf) focused-id))
                  (buffer-list)))))

(defun ewm-text-input--auto-enable ()
  "Enable text input mode when a client text field is activated."
  (ewm-text-input-intercept t)
  (ewm-text-input-mode 1))

(defun ewm-text-input--auto-disable ()
  "Disable text input mode when a client text field is deactivated."
  (ewm-text-input-intercept :false)
  (ewm-text-input-mode -1))

(defun ewm--frame-for-output (output-name)
  "Return the frame assigned to OUTPUT-NAME, or nil."
  (cl-find output-name (frame-list)
           :test #'string=
           :key (lambda (f) (frame-parameter f 'ewm-output))))

(defun ewm--create-frame-for-output (output-name)
  "Create a new frame for OUTPUT-NAME.
Sends prepare-frame to compositor and creates a pending frame.
The frame will be fully assigned when the compositor responds."
  (ewm-log "creating frame for %s" output-name)
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

;;; Process handling

(defun ewm--filter (proc string)
  "Process filter for PROC receiving STRING."
  (let ((buf (process-buffer proc)))
    (with-current-buffer buf
      (goto-char (point-max))
      (insert string)
      ;; Process complete lines
      (goto-char (point-min))
      (while (search-forward "\n" nil t)
        (let* ((line (buffer-substring (point-min) (1- (point))))
               (event (condition-case err
                          (json-parse-string line)
                        (error
                         (ewm-log "JSON parse error: %s" err)
                         nil))))
          (delete-region (point-min) (point))
          (when event
            (ewm--handle-event event)))))))

(defun ewm--sentinel (proc event)
  "Process sentinel for PROC with EVENT."
  (ewm-log "connection %s" (string-trim event))
  (when (not (process-live-p proc))
    (setq ewm--process nil)))

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

(defun ewm--vt-suffix ()
  "Return VT-specific suffix for socket names, e.g., \"-vt3\"."
  (if-let ((vt (ewm--current-vt)))
      (format "-vt%d" vt)
    ""))

(defun ewm--default-socket-path ()
  "Return the default IPC socket path.
Socket name is automatically derived from current VT number,
e.g., on VT3: \"ewm-vt3.sock\"."
  (let* ((runtime-dir (or (getenv "XDG_RUNTIME_DIR") "/tmp"))
         (socket-name (format "ewm%s.sock" (ewm--vt-suffix))))
    (expand-file-name socket-name runtime-dir)))

(defun ewm-connect (&optional socket-path)
  "Connect to compositor at SOCKET-PATH.
Default path is automatically derived from current VT (e.g., ewm-vt3.sock).
Safe to call unconditionally - returns nil with a message if connection fails."
  (interactive)
  (let ((path (or socket-path (ewm--default-socket-path))))
    (when (and ewm--process (process-live-p ewm--process))
      (delete-process ewm--process))
    ;; Reset outputs and pending assignments
    (setq ewm--outputs nil)
    (setq ewm--pending-frame-outputs nil)
    (condition-case nil
        (progn
          (setq ewm--process
                (make-network-process
                 :name "ewm"
                 :buffer (generate-new-buffer " *ewm-input*")
                 :family 'local
                 :service path
                 :filter #'ewm--filter
                 :sentinel #'ewm--sentinel))
          ;; Connection succeeded - enable EWM mode
          (ewm-mode 1)
          ;; Frame setup is triggered by outputs_complete event from compositor
          (ewm-log "connected to %s" path))
      (file-error
       (ewm-log "connection failed (not running inside EWM?)")))))

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
    (tab-bar-mode -1))
  ;; With no decorations and no bars, CSD height is 0
  (setq ewm-csd-height 0))

(defun ewm-disconnect ()
  "Disconnect from compositor."
  (interactive)
  (ewm-mode -1)
  (setq ewm-text-input-active nil)
  (when ewm--process
    (delete-process ewm--process)
    (setq ewm--process nil)
    (ewm-log "disconnected")))

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
  (setq ewm--outputs nil)
  (setq ewm--pending-frame-outputs nil)
  (setq ewm--module-mode nil)  ; Will be set to t after successful start
  ;; Start the compositor
  (ewm-log "Starting compositor in module mode...")
  (if (ewm-start)
      (progn
        (setq ewm--module-mode t)
        ;; Wait briefly for compositor to initialize
        (sleep-for 0.5)
        ;; Set environment for Wayland clients
        (let ((socket-name (format "wayland-ewm-vt%d" (ewm--current-vt))))
          (setenv "WAYLAND_DISPLAY" socket-name)
          (setenv "XDG_SESSION_TYPE" "wayland")
          (ewm-log "Set WAYLAND_DISPLAY=%s" socket-name))
        ;; Start event polling
        (ewm--start-module-polling)
        ;; Enable EWM mode (input handling, layout sync, etc.)
        (ewm-mode 1)
        (ewm-log "EWM module mode started"))
    (error "Failed to start compositor")))

(defun ewm-stop-module ()
  "Stop EWM module mode compositor."
  (interactive)
  (when ewm--module-mode
    ;; Stop event polling first
    (ewm--stop-module-polling)
    ;; Request compositor stop
    (when (and (fboundp 'ewm-stop) (fboundp 'ewm-running) (ewm-running))
      (ewm-stop)
      ;; Wait briefly for compositor to stop
      (let ((timeout 50))  ; 5 seconds max
        (while (and (> timeout 0) (ewm-running))
          (sleep-for 0.1)
          (cl-decf timeout))))
    ;; Clean up
    (setq ewm--module-mode nil)
    (ewm-mode -1)
    (ewm-log "EWM module mode stopped")))

;;; Input handling
;;
;; EWM automatically intercepts keys based on two settings:
;;
;; 1. `ewm-intercept-prefixes' - Keys that start command sequences (C-x, M-x)
;; 2. `ewm-intercept-modifiers' - Modifiers whose bindings are scanned from
;;    Emacs keymaps (default: super)
;;
;; Unlike EXWM, you don't need a separate `exwm-input-global-keys'.
;; Just use normal `global-set-key' or use-package :bind, and EWM
;; will automatically intercept keys with the configured modifiers.
;;
;; Example:
;;   (global-set-key (kbd "s-d") 'consult-buffer)
;;   (global-set-key (kbd "s-<left>") 'windmove-left)
;;
;; These bindings work both in EWM and regular Emacs sessions.

(defgroup ewm-input nil
  "EWM input handling."
  :group 'ewm)

;;; Keyboard layout integration

(defcustom ewm-xkb-layouts '("us")
  "List of XKB layout names to configure in the compositor.
These layouts will be available for switching via `set-input-method'.
Example: \\='(\"us\" \"ru\" \"no\")"
  :type '(repeat string)
  :group 'ewm-input)

(defcustom ewm-xkb-options nil
  "XKB options string for the compositor.
Example: \"ctrl:nocaps,grp:alt_shift_toggle\""
  :type '(choice (const nil) string)
  :group 'ewm-input)

(defvar ewm--xkb-layouts-configured nil
  "List of layout names currently configured in compositor.")

(defvar ewm--xkb-current-layout nil
  "Current XKB layout name in compositor.")

(defvar-local ewm-input--mode 'line-mode
  "Current input mode: `line-mode' or `char-mode'.")

(defcustom ewm-intercept-prefixes
  '(?\C-x ?\C-u ?\C-h ?\M-x ?\M-` ?\M-& ?\M-:)
  "Prefix keys that always go to Emacs.
These are keys that start command sequences.
Can be character literals (e.g., ?\\C-x) or strings (e.g., \"C-x\").
Adapted from `exwm-input-prefix-keys'."
  :type '(repeat (choice character string))
  :group 'ewm-input)

(defcustom ewm-intercept-modifiers
  '(super)
  "Modifiers whose key bindings are auto-detected from Emacs keymaps.
EWM scans `global-map' for keys with these modifiers and intercepts them.
This means you can use normal `global-set-key' for bindings like s-d, s-left.

Valid values: control, meta, super, hyper, shift, alt.
Default is (super) to intercept all Super-key bindings."
  :type '(repeat symbol)
  :group 'ewm-input)

(defcustom ewm-input-simulation-keys
  '(([?\C-g] . [escape]))
  "Simulation keys for translating Emacs keys to application keys.
Each element is (EMACS-KEY . APP-KEY) where both are key vectors.
These bindings are active in `ewm-surface-mode' buffers, translating
familiar Emacs keys to their application equivalents.

Default: C-g sends Escape (quit/cancel in most applications).

Adapted from `exwm-input-simulation-keys'."
  :type '(alist :key-type key-sequence :value-type key-sequence)
  :group 'ewm-input)

(defcustom ewm-input-line-mode-passthrough nil
  "If non-nil, pass all keys through to surface in line-mode.
Effectively makes line-mode behave like char-mode."
  :type 'boolean
  :group 'ewm-input)

(defun ewm-input--update-mode (mode)
  "Update input mode to MODE for current buffer.
MODE is either `line-mode' or `char-mode'.

In EXWM, line-mode still gives the X window keyboard focus but intercepts
keys via XGrabKey.  In EWM/Wayland, we achieve similar behavior by:
- Line-mode: surface has focus, compositor intercepts prefix keys
- Char-mode: surface has focus, no interception (same as line-mode for now)

Both modes keep focus on the surface so typing works immediately."
  (when-let ((state ewm--input-state))
    (when ewm-surface-id
      (setq ewm-input--mode mode)
      ;; Always focus the surface - this matches EXWM behavior where the X window
      ;; has focus even in line-mode (EXWM intercepts via XGrabKey)
      (setf (ewm-input-state-last-focused-id state) ewm-surface-id)
      (ewm-focus ewm-surface-id)
      (force-mode-line-update))))

(defun ewm-input-char-mode ()
  "Switch to char-mode: keys go directly to surface.
Press a prefix key to return to line-mode."
  (interactive)
  (ewm-input--update-mode 'char-mode)
  (ewm-log "char-mode"))

(defun ewm-input-line-mode ()
  "Switch to line-mode: keys go to Emacs."
  (interactive)
  (ewm-input--update-mode 'line-mode)
  (ewm-log "line-mode"))

(defun ewm-input-toggle-mode ()
  "Toggle between line-mode and char-mode."
  (interactive)
  (ewm-input--update-mode
   (if (eq ewm-input--mode 'char-mode) 'line-mode 'char-mode))
  (ewm-log "%s" ewm-input--mode))

(defun ewm-input-send-key (key)
  "Send KEY to the current surface.
KEY should be a key sequence."
  (interactive "kKey: ")
  (when ewm-surface-id
    (ewm--send `(:cmd "key" :id ,ewm-surface-id :key ,(key-description key)))))

;; Internal variable for skipping window change updates
(defvar ewm-input--skip-window-change nil
  "Non-nil to skip window change handlers.
Used when buffer/window changes are expected and focus should not change.")

(defun ewm-input--focus-debounced (id)
  "Request focus on ID with debouncing to prevent focus loops."
  (when-let ((state ewm--input-state))
    (if (ewm-input-state-inhibit-update state)
        (ewm--focus-log "debounced" "INHIBITED id=%d (inhibit-update is set)" id)
      (ewm--focus-log "debounced" "queuing id=%d (pending=%s, timer=%s)"
                      id
                      (ewm-input-state-pending-focus-id state)
                      (if (ewm-input-state-focus-timer state) "active" "none"))
      (setf (ewm-input-state-pending-focus-id state) id)
      (unless (ewm-input-state-focus-timer state)
        (setf (ewm-input-state-focus-timer state)
              (run-with-timer 0.01 nil #'ewm-input--focus-commit))))))

(defun ewm-input--warp-pointer-to-window (window)
  "Warp pointer to center of WINDOW.
Does nothing if pointer is already inside the window or if it's a minibuffer."
  (unless (minibufferp (window-buffer window))
    (let* ((frame (window-frame window))
           (output (frame-parameter frame 'ewm-output))
           (output-offset (ewm--get-output-offset output))
           (edges (window-inside-pixel-edges window))
           (x (+ (car output-offset) (/ (+ (nth 0 edges) (nth 2 edges)) 2)))
           (y (+ (cdr output-offset) (/ (+ (nth 1 edges) (nth 3 edges)) 2))))
      (ewm-warp-pointer (float x) (float y)))))

(defun ewm-input--mouse-triggered-p ()
  "Return non-nil if current focus change was triggered by mouse."
  (or (mouse-event-p last-input-event)
      (eq this-command 'handle-select-window)
      (and ewm--input-state
           (ewm-input-state-compositor-focus ewm--input-state))))

(defun ewm-input--on-select-window (window &optional norecord)
  "Advice for `select-window' to implement mouse-follows-focus.
Warps pointer to WINDOW unless NORECORD is non-nil, the window hasn't
changed, or the focus change was triggered by a mouse event."
  (when-let ((state ewm--input-state))
    (when (and ewm-mouse-follows-focus
               (not norecord)
               (not (eq window (ewm-input-state-mff-last-window state)))
               (not (ewm-input--mouse-triggered-p)))
      (setf (ewm-input-state-mff-last-window state) window)
      (ewm-input--warp-pointer-to-window window))))

(defun ewm-input--on-select-frame (frame &optional _norecord)
  "Advice for `select-frame-set-input-focus' to implement mouse-follows-focus.
Warps pointer to the selected window on FRAME unless triggered by mouse."
  (when-let ((state ewm--input-state))
    (when (and ewm-mouse-follows-focus
               (not (ewm-input--mouse-triggered-p)))
      (let ((window (frame-selected-window frame)))
        (unless (eq window (ewm-input-state-mff-last-window state))
          (setf (ewm-input-state-mff-last-window state) window)
          (ewm-input--warp-pointer-to-window window))))))

(defun ewm-input--focus-commit ()
  "Commit pending focus change."
  (when-let ((state ewm--input-state))
    (setf (ewm-input-state-focus-timer state) nil)
    (let ((id (ewm-input-state-pending-focus-id state)))
      (setf (ewm-input-state-pending-focus-id state) nil)
      (cond
       ((not id)
        (ewm--focus-log "commit" "no pending id"))
       ((eq id (ewm-input-state-last-focused-id state))
        (ewm--focus-log "commit" "skipping id=%d (same as last)" id))
       (t
        (ewm--focus-log "commit" "FOCUSING id=%d (was %d)"
                        id (ewm-input-state-last-focused-id state))
        (setf (ewm-input-state-last-focused-id state) id)
        (ewm-input--set-inhibit)
        (ewm-focus id))))))

(defun ewm-input--on-window-buffer-change (frame)
  "Handle window buffer changes on FRAME.
Called via `window-buffer-change-functions' when a window's buffer changes.
Unlike `buffer-list-update-hook', this does NOT fire for buffer renames.

When viewing a surface buffer, the surface has keyboard focus so that
typing works immediately.  When viewing a non-surface buffer, the
frame's Emacs surface has focus."
  (when (and (ewm--compositor-active-p)
             (not ewm-input--skip-window-change)
             (not (active-minibuffer-window))
             (eq frame (selected-frame)))
    (let* ((win (frame-selected-window frame))
           (buf (window-buffer win))
           (id (buffer-local-value 'ewm-surface-id buf))
           (frame-surface-id (frame-parameter frame 'ewm-surface-id))
           (target-id (or id frame-surface-id 1)))
      (ewm--focus-log "window-buffer" "buf=%s id=%s frame-id=%s -> target=%d"
                      (buffer-name buf) id frame-surface-id target-id)
      (ewm-input--focus-debounced target-id))))

(defun ewm-input--on-window-selection-change (frame)
  "Handle window selection changes on FRAME.
Called via `window-selection-change-functions' when selected window changes."
  (when (and (ewm--compositor-active-p)
             (not ewm-input--skip-window-change)
             (not (active-minibuffer-window))
             (eq frame (selected-frame)))
    (let* ((win (frame-selected-window frame))
           (buf (window-buffer win))
           (id (buffer-local-value 'ewm-surface-id buf))
           (frame-surface-id (frame-parameter frame 'ewm-surface-id))
           (target-id (or id frame-surface-id 1)))
      (ewm--focus-log "window-selection" "buf=%s id=%s frame-id=%s -> target=%d"
                      (buffer-name buf) id frame-surface-id target-id)
      (ewm-input--focus-debounced target-id))))

(defun ewm-input--on-post-command ()
  "Hook called after each command.
Re-focuses the surface if we're in a surface buffer.
Also updates multi-view layout when selected window changes."
  (when-let ((state ewm--input-state))
    (when (and (ewm--compositor-active-p)
               (not (active-minibuffer-window)))
      ;; Use selected window's buffer, not current-buffer, to avoid spurious
      ;; focus changes when current-buffer differs from displayed buffer
      (let* ((current-window (selected-window))
             (buf (window-buffer current-window))
             (id (buffer-local-value 'ewm-surface-id buf)))
        ;; Check if selected window changed (important for multi-view input routing)
        (unless (eq current-window (ewm-input-state-last-selected-window state))
          (setf (ewm-input-state-last-selected-window state) current-window)
          ;; Refresh layout so the new window's view becomes active for input
          (ewm-layout--refresh))
        ;; Focus the surface for keyboard input
        (when id
          (ewm--focus-log "post-command" "buf=%s id=%d cmd=%s"
                          (buffer-name buf) id this-command)
          (ewm-input--focus-debounced id))))))

(defun ewm-input--on-focus-change ()
  "Handle frame focus changes.
Called via `after-focus-change-function' to sync compositor focus.
Prefers focusing the surface shown in the frame's selected window,
falling back to the frame's own surface for Emacs buffers."
  (when (and (ewm--compositor-active-p)
             (not (active-minibuffer-window)))
    (let* ((frame (selected-frame))
           (win (frame-selected-window frame))
           (buf (window-buffer win))
           (surface-id (buffer-local-value 'ewm-surface-id buf))
           (frame-surface-id (frame-parameter frame 'ewm-surface-id))
           (target-id (or surface-id frame-surface-id)))
      (when target-id
        (ewm--focus-log "focus-change" "frame=%s surface-id=%s frame-id=%s -> target=%s"
                        (frame-parameter frame 'name) surface-id frame-surface-id target-id)
        (ewm-input--focus-debounced target-id)))))

(defun ewm-input--enable ()
  "Enable EWM input handling."
  (setq ewm--input-state
        (ewm-input-state-create
         :last-focused-id 1
         :last-selected-window (selected-window)
         :mff-last-window (selected-window)))
  ;; Use window-specific hooks instead of buffer-list-update-hook
  ;; to avoid spurious triggers from buffer renames (e.g., vterm title updates)
  (add-hook 'window-buffer-change-functions #'ewm-input--on-window-buffer-change)
  (add-hook 'window-selection-change-functions #'ewm-input--on-window-selection-change)
  (add-hook 'post-command-hook #'ewm-input--on-post-command)
  (add-function :after after-focus-change-function #'ewm-input--on-focus-change)
  (advice-add 'select-window :after #'ewm-input--on-select-window)
  (advice-add 'select-frame-set-input-focus :after #'ewm-input--on-select-frame))

(defun ewm-input--disable ()
  "Disable EWM input handling."
  (setq ewm--input-state nil)
  (remove-hook 'window-buffer-change-functions #'ewm-input--on-window-buffer-change)
  (remove-hook 'window-selection-change-functions #'ewm-input--on-window-selection-change)
  (remove-hook 'post-command-hook #'ewm-input--on-post-command)
  (remove-function after-focus-change-function #'ewm-input--on-focus-change)
  (advice-remove 'select-window #'ewm-input--on-select-window)
  (advice-remove 'select-frame-set-input-focus #'ewm-input--on-select-frame))

(defun ewm--event-to-intercept-spec (event)
  "Convert EVENT to an intercept specification for the compositor.
Returns a plist with :key (integer or string) and modifier flags."
  (let* ((mods (event-modifiers event))
         (base (event-basic-type event))
         ;; base is either an integer (ASCII) or a symbol (special key)
         (key-value (cond
                     ((integerp base) base)
                     ((symbolp base) (symbol-name base))
                     (t nil))))
    (when key-value
      `(:key ,key-value
        :ctrl ,(if (memq 'control mods) t :false)
        :alt ,(if (memq 'meta mods) t :false)
        :shift ,(if (memq 'shift mods) t :false)
        :super ,(if (memq 'super mods) t :false)))))

(defun ewm--scan-keymap-for-modifiers (keymap modifiers)
  "Scan KEYMAP for keys that have any of MODIFIERS.
Returns a list of intercept specs."
  (let ((specs '()))
    (map-keymap
     (lambda (event binding)
       (when (and binding
                  (not (eq binding 'undefined))
                  ;; Check if event has any of the target modifiers
                  (let ((event-mods (event-modifiers event)))
                    (cl-intersection event-mods modifiers)))
         (when-let ((spec (ewm--event-to-intercept-spec event)))
           (push spec specs))))
     keymap)
    specs))

(defun ewm--send-intercept-keys ()
  "Send intercepted keys configuration to compositor.
Scans Emacs keymaps for keys matching `ewm-intercept-modifiers',
and adds `ewm-intercept-prefixes'.
This allows normal `global-set-key' bindings to work with EWM."
  (let ((specs '())
        (seen (make-hash-table :test 'equal)))
    ;; Add prefix keys first
    (dolist (key ewm-intercept-prefixes)
      ;; Handle both character literals (integers) and strings
      (let ((event (cond
                    ((integerp key) key)
                    ((stringp key) (aref (key-parse key) 0))
                    (t nil))))
        (when event
          (when-let ((spec (ewm--event-to-intercept-spec event)))
            (let ((spec-key (format "%S" spec)))
              (unless (gethash spec-key seen)
                (puthash spec-key t seen)
                (push spec specs)))))))
    ;; Scan global-map for keys with configured modifiers
    (when ewm-intercept-modifiers
      (dolist (spec (ewm--scan-keymap-for-modifiers
                     (current-global-map) ewm-intercept-modifiers))
        (let ((spec-key (format "%S" spec)))
          (unless (gethash spec-key seen)
            (puthash spec-key t seen)
            (push spec specs)))))
    ;; Send to compositor
    (ewm-log "Intercepting %d keys" (length specs))
    (let ((keys-vec (vconcat (nreverse specs))))
      (if ewm--module-mode
          (ewm-intercept-keys-module keys-vec)
        (ewm--send `(:cmd "intercept-keys" :keys ,keys-vec))))))

;;; Surface mode

(defvar-local ewm-surface-id nil
  "Surface ID for this buffer.")

(defvar-local ewm-surface-app nil
  "Application name (app_id) for this buffer.
Similar to `exwm-class-name'.")

(defvar-local ewm-surface-title nil
  "Window title for this buffer.
Similar to `exwm-title'.")

(defun ewm--kill-buffer-query-function ()
  "Run in `kill-buffer-query-functions' for surface buffers.
Sends close request to compositor and prevents immediate buffer kill.
Buffer will be killed when compositor confirms surface closed.
Adapted from exwm-manage--kill-buffer-query-function."
  (if (not ewm-surface-id)
      t  ; Not a surface buffer, allow kill
    (if (not (or ewm--module-mode
                 (and ewm--process (process-live-p ewm--process))))
        t  ; No connection and not in module mode, allow kill
      ;; Request graceful close via xdg_toplevel.close
      (ewm-close ewm-surface-id)
      ;; Don't kill buffer now; wait for compositor's "close" event
      nil)))

(defun ewm-surface-mode-line-mode ()
  "Return mode-line indicator for current input mode."
  (if (eq ewm-input--mode 'char-mode)
      "[C]"
    "[L]"))

(define-derived-mode ewm-surface-mode fundamental-mode "EWM"
  "Major mode for EWM surface buffers.
\\<ewm-surface-mode-map>
In line-mode (default), keys go to Emacs.
In char-mode, keys go directly to the surface.

\\[ewm-input-char-mode] - switch to char-mode
\\[ewm-input-line-mode] - switch to line-mode
\\[ewm-input-toggle-mode] - toggle input mode"
  (setq buffer-read-only t)
  (setq-local cursor-type nil)
  ;; Set up mode line to show input mode
  (setq mode-name '("EWM" (:eval (ewm-surface-mode-line-mode))))
  ;; Kill buffer -> close window (like EXWM)
  (add-hook 'kill-buffer-query-functions
            #'ewm--kill-buffer-query-function nil t))

;; Keybindings for surface mode (adapted from exwm-input.el)
(define-key ewm-surface-mode-map (kbd "C-c C-k") #'ewm-input-char-mode)
(define-key ewm-surface-mode-map (kbd "C-c C-t") #'ewm-input-toggle-mode)

(defun ewm-input--simulation-key-command (target-key)
  "Return a command that sends TARGET-KEY to the current surface."
  (lambda ()
    (interactive)
    (ewm-input-send-key target-key)))

(defun ewm-input--setup-simulation-keys ()
  "Set up simulation key bindings in `ewm-surface-mode-map'.
Binds keys from `ewm-input-simulation-keys' to send their
translated equivalents to the surface."
  (dolist (mapping ewm-input-simulation-keys)
    (let ((source-key (car mapping))
          (target-key (cdr mapping)))
      (define-key ewm-surface-mode-map source-key
                  (ewm-input--simulation-key-command target-key)))))

;; Apply simulation keys when the mode map is available
(ewm-input--setup-simulation-keys)

;;; Debug

(defun ewm-debug-layout ()
  "Show layout debug info for current window.
Also writes to /tmp/ewm-debug.txt for easy access."
  (interactive)
  (let* ((window (selected-window))
         (frame (selected-frame))
         (geometry (frame-geometry frame))
         (abs-edges (window-inside-absolute-pixel-edges window))
         (rel-edges (window-inside-pixel-edges window))
         (y-offset (ewm--frame-y-offset frame))
         (frame-pos (frame-position frame))
         (frame-outer (alist-get 'outer-edges geometry))
         (frame-inner (alist-get 'inner-edges geometry))
         (info (format "Window edges (absolute): %S
Window edges (relative): %S
Calculated Y offset: %d
  - ewm-csd-height: %d
  - menu-bar: %S
  - tool-bar: %S
  - tab-bar: %S
Frame position: %S
Frame outer-edges: %S
Frame inner-edges: %S
Frame undecorated: %S
"
                       abs-edges
                       rel-edges
                       y-offset
                       ewm-csd-height
                       (alist-get 'menu-bar-size geometry)
                       (alist-get 'tool-bar-size geometry)
                       (alist-get 'tab-bar-size geometry)
                       frame-pos
                       frame-outer
                       frame-inner
                       (frame-parameter frame 'undecorated))))
    (write-region info nil "/tmp/ewm-debug.txt")
    (message "Debug saved to /tmp/ewm-debug.txt")))

(defun ewm-debug-surfaces ()
  "Show which window each surface is mapped to.
Writes to /tmp/ewm-surfaces.txt for debugging."
  (interactive)
  (let ((info "=== Surface Layout Debug ===\n\n")
        (selected (selected-window)))
    ;; Show selected window
    (setq info (concat info (format "Selected window: %s\n\n" selected)))
    ;; Show all windows and their buffers
    (setq info (concat info "Windows:\n"))
    (dolist (window (window-list nil 'no-minibuf))
      (let* ((buf (window-buffer window))
             (id (buffer-local-value 'ewm-surface-id buf))
             (edges (window-inside-absolute-pixel-edges window)))
        (setq info (concat info (format "  %s%s: %s%s\n    edges: %S\n"
                                        window
                                        (if (eq window selected) " *SELECTED*" "")
                                        (buffer-name buf)
                                        (if id (format " [surface %d]" id) "")
                                        edges)))))
    ;; Show registered surfaces
    (setq info (concat info "\nRegistered surfaces:\n"))
    (maphash (lambda (id surface-info)
               (let ((buf (plist-get surface-info :buffer)))
                 (setq info (concat info (format "  ID %d: %s (live: %s)\n"
                                                 id
                                                 (buffer-name buf)
                                                 (buffer-live-p buf))))))
             ewm--surfaces)
    (write-region info nil "/tmp/ewm-surfaces.txt")
    (message "Debug written to /tmp/ewm-surfaces.txt")))

;;; Layout (adapted from EXWM's exwm-layout.el and exwm-core.el)

;; Compatibility wrapper for window-inside-absolute-pixel-edges
;; Fixes tab-line handling for Emacs < 31 (from exwm-core.el)
(defalias 'ewm--window-inside-absolute-pixel-edges
  (if (< emacs-major-version 31)
      (lambda (&optional window)
        "Return absolute pixel edges of WINDOW's text area.
This version correctly handles tab-lines on Emacs prior to v31."
        (let* ((window (window-normalize-window window t))
               (edges (window-inside-absolute-pixel-edges window))
               (tab-line-height (window-tab-line-height window)))
          (cl-incf (elt edges 1) tab-line-height)
          (cl-incf (elt edges 3) tab-line-height)
          edges))
    #'window-inside-absolute-pixel-edges)
  "Return inner absolute pixel edges of WINDOW, handling tab-lines correctly.")

(defvar ewm-csd-height nil
  "Height of client-side decorations in pixels.
Auto-detected on connect, or set manually before connecting.")

(defun ewm--detect-csd-height ()
  "Detect the CSD (title bar) height for the current frame.
Returns the height in pixels."
  (let* ((frame (selected-frame))
         (geometry (frame-geometry frame)))
    ;; If frame is undecorated, no CSD
    (if (frame-parameter frame 'undecorated)
        0
      ;; Try to get the title bar size from frame geometry
      (let ((title-bar-size (alist-get 'title-bar-size geometry)))
        (if (and title-bar-size (> (cdr title-bar-size) 0))
            (cdr title-bar-size)
          ;; Fallback: try to detect from frame edges
          (let ((outer (alist-get 'outer-edges geometry))
                (inner (alist-get 'inner-edges geometry)))
            (if (and outer inner)
                (- (cadr inner) (cadr outer))
              ;; Last resort: assume standard GTK title bar
              37)))))))

(defun ewm--frame-y-offset (&optional _frame)
  "Calculate Y offset to account for CSD only.
Internal bars (menu-bar, tool-bar, tab-bar) are already reflected in
`window-inside-absolute-pixel-edges', so we only add CSD height here.
The FRAME argument is kept for API compatibility but not used."
  (or ewm-csd-height 0))

(defun ewm-layout--show (id &optional window)
  "Show surface ID exactly fit in the Emacs window WINDOW.
Adapted from exwm-layout--show."
  (let* ((frame (window-frame window))
         (output-offset (ewm--get-output-offset (frame-parameter frame 'ewm-output)))
         (edges (ewm--window-inside-absolute-pixel-edges window))
         (x (pop edges))
         (y (pop edges))
         (width (- (pop edges) x))
         (height (- (pop edges) y))
         (csd-offset (ewm--frame-y-offset frame))
         (final-x (+ x (car output-offset)))
         (final-y (+ y csd-offset (cdr output-offset))))
    (ewm-layout id final-x final-y width height)))

(defun ewm-layout--refresh ()
  "Refresh layout for all surface buffers.
Collects all windows showing each surface and sends multi-view commands.
Supports displaying the same surface in multiple windows (true multi-view).
Adapted from exwm-layout--refresh-workspace."
  (when (or ewm--module-mode
            (and ewm--process (process-live-p ewm--process)))
    ;; Force redisplay to ensure window sizes are current
    (redisplay t)
    ;; Build a hash table: surface-id -> list of (window . active-p)
    (let ((surface-windows (make-hash-table :test 'eql))
          (sel-window (selected-window)))
      ;; Collect all windows showing each surface
      (dolist (frame (frame-list))
        (dolist (window (window-list frame 'no-minibuf))
          (let* ((buf (window-buffer window))
                 (id (buffer-local-value 'ewm-surface-id buf)))
            (when id
              (let ((active-p (eq window sel-window))
                    (existing (gethash id surface-windows)))
                (puthash id (cons (cons window active-p) existing) surface-windows))))))
      ;; Send views or hide for each surface
      (maphash
       (lambda (id _info)
         (let ((windows (gethash id surface-windows)))
           (if windows
               ;; Surface is visible in one or more windows - send views
               (let ((views (mapcar
                             (lambda (win-pair)
                               (let* ((window (car win-pair))
                                      (active-p (cdr win-pair)))
                                 (ewm-layout--make-view window active-p)))
                             windows)))
                 (ewm-views id (vconcat views)))
             ;; Surface not visible - hide it
             (ewm-hide id))))
       ewm--surfaces))))

(defun ewm-layout--make-view (window active-p)
  "Create a view plist for WINDOW with ACTIVE-P flag."
  (let* ((frame (window-frame window))
         (output-offset (ewm--get-output-offset (frame-parameter frame 'ewm-output)))
         (edges (ewm--window-inside-absolute-pixel-edges window))
         (x (pop edges))
         (y (pop edges))
         (width (- (pop edges) x))
         (height (- (pop edges) y))
         (csd-offset (ewm--frame-y-offset frame))
         (final-x (+ x (car output-offset)))
         (final-y (+ y csd-offset (cdr output-offset))))
    `(:x ,final-x :y ,final-y :w ,width :h ,height :active ,(if active-p t :false))))

(defun ewm--window-config-change ()
  "Hook called when window configuration changes."
  (ewm-layout--refresh))

(defvar ewm--pre-minibuffer-surface-id nil
  "Surface ID that was focused before minibuffer opened.")

(defun ewm--on-minibuffer-setup ()
  "Save focused surface and refresh layout when minibuffer activates."
  (when-let ((state ewm--input-state))
    (setq ewm--pre-minibuffer-surface-id
          (ewm-input-state-last-focused-id state)))
  (run-with-timer 0.05 nil #'ewm--refresh-with-redisplay))

(defun ewm--on-minibuffer-exit ()
  "Restore focus and refresh layout when minibuffer exits."
  (run-with-timer 0.05 nil #'ewm--refresh-with-redisplay)
  (when (and ewm--pre-minibuffer-surface-id
             (ewm--compositor-active-p))
    (ewm-focus ewm--pre-minibuffer-surface-id)
    (setq ewm--pre-minibuffer-surface-id nil)))

(defun ewm--refresh-with-redisplay ()
  "Force redisplay then refresh layout.
Ensures window edges are current before calculating positions."
  (redisplay t)
  (ewm-layout--refresh))

(defun ewm--on-window-size-change (_frame)
  "Refresh layout when window sizes change.
Catches minibuffer height changes that window-configuration-change misses."
  (ewm-layout--refresh))

(defun ewm--enable-layout-sync ()
  "Enable automatic layout sync."
  (add-hook 'window-configuration-change-hook #'ewm--window-config-change)
  (add-hook 'window-size-change-functions #'ewm--on-window-size-change)
  (add-hook 'minibuffer-setup-hook #'ewm--on-minibuffer-setup)
  (add-hook 'minibuffer-exit-hook #'ewm--on-minibuffer-exit))

(defun ewm--disable-layout-sync ()
  "Disable automatic layout sync."
  (remove-hook 'window-configuration-change-hook #'ewm--window-config-change)
  (remove-hook 'window-size-change-functions #'ewm--on-window-size-change)
  (remove-hook 'minibuffer-setup-hook #'ewm--on-minibuffer-setup)
  (remove-hook 'minibuffer-exit-hook #'ewm--on-minibuffer-exit))

;;; Keyboard layout sync

(defun ewm--send-xkb-config ()
  "Send XKB configuration to compositor.
Configures keyboard layouts from `ewm-xkb-layouts' and options from
`ewm-xkb-options'."
  (when (or ewm--module-mode
            (and ewm--process (process-live-p ewm--process)))
    (let ((layouts-str (string-join ewm-xkb-layouts ",")))
      (if ewm--module-mode
          (ewm-configure-xkb-module layouts-str ewm-xkb-options)
        (let ((cmd (if ewm-xkb-options
                       `(:cmd "configure-xkb"
                         :layouts ,layouts-str
                         :options ,ewm-xkb-options)
                     `(:cmd "configure-xkb"
                       :layouts ,layouts-str))))
          (ewm--send cmd)))
      (ewm-log "sent XKB config: layouts=%s options=%s"
               layouts-str ewm-xkb-options))))

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
    (ewm--stop-module-polling)
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

(require 'ewm-transient)

(provide 'ewm)
;;; ewm.el ends here
