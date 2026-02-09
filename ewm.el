;;; ewm.el --- Emacs Wayland Manager -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; EWM integrates Emacs with a Wayland compositor, providing an EXWM-like
;; experience without the single-threaded limitations.
;;
;; Quick start (compositor spawns Emacs automatically):
;;   EWM_INIT=/path/to/ewm.el ewm
;;
;; Or with custom Emacs args:
;;   EWM_INIT=/path/to/ewm.el ewm -Q --eval "(load-theme 'modus-vivendi)"
;;
;; Manual startup:
;;   1. Start compositor: ewm --no-auto-emacs
;;   2. Start Emacs inside: WAYLAND_DISPLAY=wayland-ewm emacs -l ewm.el -f ewm-connect
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
;;   EWM_INIT  - Path to ewm.el (auto-loads and connects)

;;; Code:

(require 'cl-lib)
(require 'map)

;;; Debug logging

(defvar ewm-debug-log-file "/tmp/ewm-emacs.log"
  "File to write EWM debug logs to.")

(defvar ewm-debug-logging nil
  "When non-nil, write debug messages to `ewm-debug-log-file'.")

(defun ewm-log (format-string &rest args)
  "Log FORMAT-STRING with ARGS to debug file and *Messages*."
  (let ((msg (apply #'format format-string args)))
    (message "EWM: %s" msg)
    (when ewm-debug-logging
      (with-temp-buffer
        (insert (format-time-string "[%Y-%m-%d %H:%M:%S] ")
                msg "\n")
        (write-region (point-min) (point-max) ewm-debug-log-file t 'silent)
        ;; Force sync
        (call-process "sync" nil nil nil)))))

;;; Input state struct (defined early for use in event handlers)

(cl-defstruct (ewm-input-state (:constructor ewm-input-state-create))
  "State for EWM input handling."
  (last-focused-id 1)
  (last-selected-window nil)
  (mff-last-window nil)
  (compositor-focus nil)
  (focus-timer nil)
  (pending-focus-id nil)
  (inhibit-update nil))

(defvar ewm--input-state nil
  "Current input state, or nil if not connected.")

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

(defvar ewm--initial-setup-done nil
  "Non-nil after initial outputs_complete has been processed.
Used to distinguish hotplug events from initial output detection.")

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
      (_ (ewm-log "unknown event type: %s" type)))))

;;; Event handlers

(defcustom ewm-manage-focus-new-surface t
  "Whether to automatically focus new surfaces.
When non-nil, new surface buffers are displayed and selected.
Adapted from EXWM's behavior."
  :type 'boolean
  :group 'ewm)

(defun ewm--handle-new-surface (event)
  "Handle new surface EVENT.
If there's a pending frame for this output, this is an Emacs frame.
Otherwise, creates a buffer for external surface and displays on the target output.
Adapted from `exwm-manage--manage-window'."
  (pcase-let (((map ("id" id) ("app" app) ("output" output)) event))
    (ewm-log "new-surface id=%d app=%s output=%s" id app output)
    (let ((pending (and output (assoc output ewm--pending-frame-outputs))))
      (ewm-log "pending-frame-outputs=%s, found pending=%s"
               ewm--pending-frame-outputs pending)
      (if pending
        ;; Emacs frame assigned to output by compositor
        (let ((frame (cdr pending)))
          (ewm-log "assigning surface %d to pending frame %s for %s" id frame output)
          (setq ewm--pending-frame-outputs
                (delete pending ewm--pending-frame-outputs))
          (set-frame-parameter frame 'ewm-output output)
          (set-frame-parameter frame 'ewm-surface-id id)
          (ewm-log "frame on %s (surface %d) - assignment complete" output id))
      ;; Regular surface - create buffer and display on target output's frame
      (let ((buf (generate-new-buffer (format "*ewm:%s:%d*" app id))))
        (puthash id `(:buffer ,buf :app ,app) ewm--surfaces)
        (with-current-buffer buf
          (ewm-surface-mode)
          (setq-local ewm-surface-id id)
          (setq-local ewm-surface-app app))
        ;; Display the new surface buffer on the frame for target output
        (when ewm-manage-focus-new-surface
          (let ((target-frame (ewm--frame-for-output output)))
            (if target-frame
                (with-selected-frame target-frame
                  (pop-to-buffer-same-window buf))
              (pop-to-buffer-same-window buf))))
        (ewm-log "new surface %d (%s) on %s" id app (or output "current")))))))

(defun ewm--handle-close-surface (event)
  "Handle close surface EVENT.
Kills the surface buffer and focuses Emacs.
Adapted from `exwm-manage--unmanage-window'."
  (pcase-let (((map ("id" id)) event))
    (let ((info (gethash id ewm--surfaces)))
      (when info
      (let ((buf (plist-get info :buffer)))
        (when (buffer-live-p buf)
          ;; Remove the buffer-local kill query function that would block the kill,
          ;; since we're handling a close event from compositor (surface already closed)
          (with-current-buffer buf
            (remove-hook 'kill-buffer-query-functions
                         #'ewm--kill-buffer-query-function t))
          (kill-buffer buf)))
        (remhash id ewm--surfaces))
      (ewm-log "closed surface %d" id))))

(defun ewm--handle-focus (event)
  "Handle focus EVENT from compositor.
Selects the window displaying the focused surface's buffer."
  (pcase-let (((map ("id" id)) event))
    (let ((info (gethash id ewm--surfaces)))
      (when info
        (let ((buf (plist-get info :buffer)))
          (when (buffer-live-p buf)
            ;; Find a window showing this buffer and select it
            (let ((win (get-buffer-window buf t)))
              (when win
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
Adds the output to `ewm--outputs' and creates a frame for hotplugged monitors."
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
                               :modes mode-plists))
           ;; Check if this is a hotplug (output not already known)
           (is-hotplug (not (cl-find name ewm--outputs
                                     :test #'string= :key (lambda (o) (plist-get o :name))))))
      ;; Remove existing entry with same name (update case)
      (setq ewm--outputs (cl-remove-if (lambda (o) (equal (plist-get o :name) name))
                                       ewm--outputs))
      ;; Add new output
      (push output-plist ewm--outputs)
      (ewm-log "output detected: %s at (%d, %d)" name x y)
      ;; Create frame for hotplugged output (after initial setup)
      (when (and is-hotplug ewm--initial-setup-done)
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

;;; Commands

(defun ewm-layout (id x y w h)
  "Set surface ID position to X Y and size to W H."
  (ewm--send `(:cmd "layout" :id ,id :x ,x :y ,y :w ,w :h ,h)))

(defun ewm-views (id views)
  "Set surface ID to display at multiple VIEWS.
VIEWS is a vector of plists with :x :y :w :h :active keys.
The :active view receives input, others are visual copies."
  (ewm--send `(:cmd "views" :id ,id :views ,views)))

(defun ewm-hide (id)
  "Hide surface ID (move offscreen)."
  (ewm--send `(:cmd "hide" :id ,id)))

(defun ewm-close (id)
  "Request surface ID to close gracefully.
Sends xdg_toplevel.close to the client."
  (ewm--send `(:cmd "close" :id ,id)))

(defun ewm-focus (id)
  "Focus surface ID."
  (ewm--send `(:cmd "focus" :id ,id)))

(defun ewm-warp-pointer (x y)
  "Warp pointer to absolute position X, Y."
  (ewm--send `(:cmd "warp-pointer" :x ,x :y ,y)))

(defun ewm-screenshot (&optional path)
  "Take a screenshot of the compositor.
Saves to PATH, or /tmp/ewm-screenshot.png by default."
  (interactive)
  (let ((target (or path "/tmp/ewm-screenshot.png")))
    (ewm--send `(:cmd "screenshot" :path ,target))
    (ewm-log "screenshot requested -> %s" target)))

(defun ewm-configure-output (name &rest args)
  "Configure output NAME with ARGS.
ARGS is a plist with optional keys:
  :x :y - position in global coordinate space
  :enabled - t or nil to enable/disable the output
Example:
  (ewm-configure-output \"DP-1\" :x 1920 :y 0)
  (ewm-configure-output \"DP-1\" :enabled nil)"
  (let ((cmd `(:cmd "configure-output" :name ,name)))
    (while args
      (let ((key (pop args))
            (val (pop args)))
        (setq cmd (plist-put cmd key val))))
    (ewm--send cmd)))

(defun ewm-assign-output (id output)
  "Assign surface ID to OUTPUT.
The surface will be positioned and sized to fill the output.
Example:
  (ewm-assign-output 1 \"DP-1\")"
  (ewm--send `(:cmd "assign-output" :id ,id :output ,output)))

(defun ewm-prepare-frame (output)
  "Tell compositor to assign next frame to OUTPUT.
Call this before `make-frame' to have the compositor automatically
assign the new frame's surface to the specified output."
  (ewm--send `(:cmd "prepare-frame" :output ,output)))

(defun ewm--get-primary-output ()
  "Return the primary output name.
Primary is the output at position (0, 0), or the first output."
  (ewm-log "get-primary-output: outputs=%S" ewm--outputs)
  (let ((found (cl-find-if (lambda (o)
                             (let ((x (plist-get o :x))
                                   (y (plist-get o :y)))
                               (ewm-log "  checking output %s: x=%S (type %s), y=%S (type %s)"
                                        (plist-get o :name) x (type-of x) y (type-of y))
                               (and (eql 0 x) (eql 0 y))))
                           ewm--outputs)))
    (or (plist-get found :name)
        (plist-get (car ewm--outputs) :name))))

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
Applies user output config, then sets up frames."
  (ewm-log "all outputs received (%d)" (length ewm--outputs))
  ;; Apply user output configuration first (mode changes, positioning)
  (ewm--apply-output-config)
  ;; Then set up frames
  (when ewm-auto-setup-frames
    (ewm--setup-frames-per-output))
  ;; Enforce 1:1 output-frame relationship
  (ewm--enforce-frame-output-parity)
  ;; Mark initial setup as complete (for hotplug detection)
  (setq ewm--initial-setup-done t))

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
  (make-frame '((visibility . t))))

(defun ewm--setup-frames-per-output ()
  "Create one frame per output and assign accordingly.
The initial frame (surface 1) is assigned to the primary output.
Additional frames are created for other outputs.
Skips outputs that already have a frame assigned."
  (ewm-log "setup-frames-per-output called, %d outputs" (length ewm--outputs))
  (when ewm--outputs
    (let* ((primary (ewm--get-primary-output))
           (initial-frame (selected-frame)))
      (ewm-log "primary output: %s, initial frame: %s" primary initial-frame)
      ;; Assign initial frame to primary output (if not already assigned)
      (if (ewm--frame-for-output primary)
          (ewm-log "primary %s already has frame" primary)
        (ewm-assign-output 1 primary)
        (set-frame-parameter initial-frame 'ewm-output primary)
        (set-frame-parameter initial-frame 'ewm-surface-id 1)
        (ewm-log "assigned initial frame to %s (surface 1)" primary))
      ;; Create frames for other outputs
      (dolist (output ewm--outputs)
        (let ((output-name (plist-get output :name)))
          (unless (or (string= output-name primary)
                      (ewm--frame-for-output output-name))
            (ewm--create-frame-for-output output-name)))))))

(defun ewm--on-make-frame (frame)
  "Enforce 1:1 output-frame relationship.
Delete FRAME unless it's managed by EWM (has output or is pending assignment)."
  (when ewm-mode
    (let ((has-output (frame-parameter frame 'ewm-output))
          (pending-output ewm--pending-output-for-next-frame))
      (cond
       ;; Already has output - keep it
       (has-output
        (ewm-log "on-make-frame %s already has output %s" frame has-output))
       ;; Being created for a specific output - register as pending
       (pending-output
        (ewm-log "on-make-frame %s registering as pending for %s" frame pending-output)
        (push (cons pending-output frame) ewm--pending-frame-outputs)
        (setq ewm--pending-output-for-next-frame nil))
       ;; Unauthorized frame - delete it
       (t
        (ewm-log "on-make-frame %s has no output, deleting" frame)
        ;; Defer deletion to avoid issues during frame creation
        (run-at-time 0 nil #'delete-frame frame))))))

(defun ewm--enforce-frame-output-parity ()
  "Ensure exactly one frame per output.
Deletes frames without outputs and duplicate frames on the same output.
Skips frames that are pending assignment (waiting for compositor response)."
  (ewm-log "enforce-frame-output-parity, %d frames, %d outputs, %d pending"
           (length (frame-list)) (length ewm--outputs)
           (length ewm--pending-frame-outputs))
  (let ((seen-outputs (make-hash-table :test 'equal))
        (deleted-count 0))
    (dolist (frame (frame-list))
      (let ((output (frame-parameter frame 'ewm-output))
            (is-pending (rassq frame ewm--pending-frame-outputs)))
        (ewm-log "checking frame %s, output=%s, pending=%s"
                 frame output (if is-pending (car is-pending) nil))
        (cond
         ;; Pending assignment - skip (waiting for compositor)
         (is-pending
          (ewm-log "skipping pending frame %s for %s" frame (car is-pending)))
         ;; No output and not pending - delete
         ((null output)
          (ewm-log "deleting orphan frame %s (no output)" frame)
          (cl-incf deleted-count)
          (delete-frame frame))
         ;; Duplicate - delete
         ((gethash output seen-outputs)
          (ewm-log "deleting duplicate frame %s on %s" frame output)
          (cl-incf deleted-count)
          (delete-frame frame))
         ;; First for this output - keep
         (t
          (ewm-log "keeping frame %s for %s" frame output)
          (puthash output frame seen-outputs)))))
    (ewm-log "enforce complete, deleted %d, kept %d"
             deleted-count (hash-table-count seen-outputs))))

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

(defun ewm--default-socket-path ()
  "Return the default IPC socket path.
Uses $XDG_RUNTIME_DIR/ewm.sock if available, otherwise /tmp/ewm.sock."
  (let ((runtime-dir (getenv "XDG_RUNTIME_DIR")))
    (if runtime-dir
        (expand-file-name "ewm.sock" runtime-dir)
      "/tmp/ewm.sock")))

(defcustom ewm-auto-setup-frames t
  "Whether to automatically create one frame per output on connect."
  :type 'boolean
  :group 'ewm)

(defun ewm-connect (&optional socket-path)
  "Connect to compositor at SOCKET-PATH.
Default is $XDG_RUNTIME_DIR/ewm.sock.
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
  (when ewm--process
    (delete-process ewm--process)
    (setq ewm--process nil)
    (ewm-log "disconnected")))

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

(defcustom ewm-input-simulation-keys nil
  "Simulation keys for translating Emacs keys to application keys.
Each element is (EMACS-KEY . APP-KEY).
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

;; Internal variable for skipping buffer list updates
(defvar ewm-input--skip-buffer-list-update nil
  "Non-nil to skip `ewm-input--on-buffer-list-update'.
Used when buffer changes are expected and focus should not change.")

(defun ewm-input--focus-debounced (id)
  "Request focus on ID with debouncing to prevent focus loops."
  (when-let ((state ewm--input-state))
    (unless (ewm-input-state-inhibit-update state)
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
      (when (and id (not (eq id (ewm-input-state-last-focused-id state))))
        (setf (ewm-input-state-last-focused-id state) id)
        ;; Inhibit focus updates while compositor processes our request
        (setf (ewm-input-state-inhibit-update state) t)
        (ewm-focus id)
        ;; Re-enable after a short delay to let focus events settle
        (run-with-timer 0.05 nil
                        (lambda ()
                          (when ewm--input-state
                            (setf (ewm-input-state-inhibit-update ewm--input-state) nil))))))))

(defun ewm-input--on-buffer-list-update ()
  "Hook called when buffer list changes.
Updates keyboard focus based on selected window's buffer.
Adapted from `exwm-input--on-buffer-list-update'.

Like EXWM, when viewing a surface buffer, the surface has keyboard focus
so that typing works immediately.  When viewing a non-surface buffer,
Emacs (surface 1) has focus."
  (when (and ewm--process
             (process-live-p ewm--process)
             (not ewm-input--skip-buffer-list-update)
             ;; Don't process during minibuffer operations (e.g., consult preview)
             (not (active-minibuffer-window)))
    ;; Use selected window's buffer, not current-buffer, to avoid spurious
    ;; focus changes during internal buffer operations (e.g., vterm output)
    (let* ((buf (window-buffer (selected-window)))
           (id (buffer-local-value 'ewm-surface-id buf))
           ;; Surface buffer: focus the surface (like EXWM)
           ;; Non-surface buffer: focus the current frame's surface
           (frame-surface-id (frame-parameter nil 'ewm-surface-id))
           (target-id (or id frame-surface-id 1)))
      (ewm-input--focus-debounced target-id))))

(defun ewm-input--on-post-command ()
  "Hook called after each command.
Re-focuses the surface if we're in a surface buffer.
Also updates multi-view layout when selected window changes."
  (when-let ((state ewm--input-state))
    (when (and ewm--process
               (process-live-p ewm--process)
               (not (active-minibuffer-window)))
      (let ((id (buffer-local-value 'ewm-surface-id (current-buffer)))
            (current-window (selected-window)))
        ;; Check if selected window changed (important for multi-view input routing)
        (unless (eq current-window (ewm-input-state-last-selected-window state))
          (setf (ewm-input-state-last-selected-window state) current-window)
          ;; Refresh layout so the new window's view becomes active for input
          (ewm-layout--refresh))
        ;; Focus the surface for keyboard input
        (when id
          (ewm-input--focus-debounced id))))))

(defun ewm-input--on-focus-change ()
  "Handle frame focus changes.
Called via `after-focus-change-function' to sync compositor focus."
  (when (and ewm--process (process-live-p ewm--process))
    (let* ((frame (selected-frame))
           (frame-surface-id (frame-parameter frame 'ewm-surface-id)))
      (when frame-surface-id
        (ewm-input--focus-debounced frame-surface-id)))))

(defun ewm-input--enable ()
  "Enable EWM input handling."
  (setq ewm--input-state
        (ewm-input-state-create
         :last-focused-id 1
         :last-selected-window (selected-window)
         :mff-last-window (selected-window)))
  (add-hook 'buffer-list-update-hook #'ewm-input--on-buffer-list-update)
  (add-hook 'post-command-hook #'ewm-input--on-post-command)
  (add-function :after after-focus-change-function #'ewm-input--on-focus-change)
  (advice-add 'select-window :after #'ewm-input--on-select-window)
  (advice-add 'select-frame-set-input-focus :after #'ewm-input--on-select-frame))

(defun ewm-input--disable ()
  "Disable EWM input handling."
  (setq ewm--input-state nil)
  (remove-hook 'buffer-list-update-hook #'ewm-input--on-buffer-list-update)
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
    (ewm--send `(:cmd "intercept-keys" :keys ,(vconcat (nreverse specs))))))

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
    (if (not (and ewm--process (process-live-p ewm--process)))
        t  ; No connection, allow kill
      ;; Request graceful close via xdg_toplevel.close
      (ewm-close ewm-surface-id)
      ;; Don't kill buffer now; wait for compositor's "close" event
      nil)))

(defun ewm-surface-mode-line-mode ()
  "Return mode-line indicator for current input mode."
  (if (eq ewm-input--mode 'char-mode)
      "[C]"
    "[L]"))

(define-derived-mode ewm-surface-mode special-mode "EWM"
  "Major mode for EWM surface buffers.
\\<ewm-surface-mode-map>
In line-mode (default), keys go to Emacs.
In char-mode, keys go directly to the surface.

\\[ewm-input-char-mode] - switch to char-mode
\\[ewm-input-line-mode] - switch to line-mode
\\[ewm-input-toggle-mode] - toggle input mode"
  (setq buffer-read-only t)
  ;; Set up mode line to show input mode
  (setq mode-name '("EWM" (:eval (ewm-surface-mode-line-mode))))
  ;; Kill buffer -> close window (like EXWM)
  (add-hook 'kill-buffer-query-functions
            #'ewm--kill-buffer-query-function nil t))

;; Keybindings for surface mode (adapted from exwm-input.el)
(define-key ewm-surface-mode-map (kbd "C-c C-k") #'ewm-input-char-mode)
(define-key ewm-surface-mode-map (kbd "C-c C-t") #'ewm-input-toggle-mode)

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
  (when (and ewm--process (process-live-p ewm--process))
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

(defun ewm--on-minibuffer-setup ()
  "Refresh layout when minibuffer activates.
Defers refresh to allow minibuffer to settle."
  (run-with-timer 0.05 nil #'ewm--refresh-with-redisplay))

(defun ewm--on-minibuffer-exit ()
  "Refresh layout when minibuffer exits."
  (run-with-timer 0.05 nil #'ewm--refresh-with-redisplay))

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

;;; Global minor mode

(defun ewm--mode-enable ()
  "Enable EWM integration."
  (ewm--disable-csd)
  (ewm--enable-layout-sync)
  (ewm-input--enable)
  (ewm--send-intercept-keys)
  (add-hook 'after-make-frame-functions #'ewm--on-make-frame))

(defun ewm--mode-disable ()
  "Disable EWM integration."
  (ewm--disable-layout-sync)
  (ewm-input--disable)
  (remove-hook 'after-make-frame-functions #'ewm--on-make-frame))

;;;###autoload
(define-minor-mode ewm-mode
  "Global minor mode for EWM compositor integration."
  :global t
  :lighter " EWM"
  :group 'ewm
  (if ewm-mode
      (ewm--mode-enable)
    (ewm--mode-disable)))

(provide 'ewm)
;;; ewm.el ends here
