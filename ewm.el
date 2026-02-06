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

(defgroup ewm nil
  "Emacs Wayland Manager."
  :group 'environment)

(defvar ewm--process nil
  "Network process for compositor connection.")

(defvar ewm--surfaces (make-hash-table :test 'eql)
  "Hash table mapping surface ID to surface info.")

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
      (_ (message "EWM: unknown event type: %s" type)))))

;;; Event handlers

(defcustom ewm-manage-focus-new-surface t
  "Whether to automatically focus new surfaces.
When non-nil, new surface buffers are displayed and selected.
Adapted from EXWM's behavior."
  :type 'boolean
  :group 'ewm)

(defun ewm--handle-new-surface (event)
  "Handle new surface EVENT.
Creates a buffer for the surface and optionally displays it.
Adapted from `exwm-manage--manage-window'."
  (let* ((id (gethash "id" event))
         (app (gethash "app" event))
         (buf (generate-new-buffer (format "*ewm:%s:%d*" app id))))
    (puthash id `(:buffer ,buf :app ,app) ewm--surfaces)
    (with-current-buffer buf
      (ewm-surface-mode)
      (setq-local ewm-surface-id id)
      (setq-local ewm-surface-app app))
    ;; Display the new surface buffer (like EXWM's pop-to-buffer-same-window)
    ;; This triggers buffer-list-update-hook which handles focus
    (when ewm-manage-focus-new-surface
      (pop-to-buffer-same-window buf))
    (message "EWM: new surface %d (%s)" id app)))

(defun ewm--handle-close-surface (event)
  "Handle close surface EVENT.
Kills the surface buffer and focuses Emacs.
Adapted from `exwm-manage--unmanage-window'."
  (let* ((id (gethash "id" event))
         (info (gethash id ewm--surfaces)))
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
    (message "EWM: closed surface %d" id)))

;;; Commands

(defun ewm-layout (id x y w h)
  "Set surface ID position to X Y and size to W H."
  (ewm--send `(:cmd "layout" :id ,id :x ,x :y ,y :w ,w :h ,h)))

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

(defun ewm-screenshot (&optional path)
  "Take a screenshot of the compositor.
Saves to PATH, or /tmp/ewm-screenshot.png by default."
  (interactive)
  (let ((target (or path "/tmp/ewm-screenshot.png")))
    (ewm--send `(:cmd "screenshot" :path ,target))
    (message "EWM: screenshot requested -> %s" target)))

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
                         (message "EWM: JSON parse error: %s" err)
                         nil))))
          (delete-region (point-min) (point))
          (when event
            (ewm--handle-event event)))))))

(defun ewm--sentinel (proc event)
  "Process sentinel for PROC with EVENT."
  (message "EWM: connection %s" (string-trim event))
  (when (not (process-live-p proc))
    (setq ewm--process nil)))

;;; Public API

(defun ewm-connect (&optional socket-path)
  "Connect to compositor at SOCKET-PATH (default /tmp/ewm.sock)."
  (interactive)
  (when (and ewm--process (process-live-p ewm--process))
    (delete-process ewm--process))
  (let ((path (or socket-path "/tmp/ewm.sock")))
    ;; Always detect CSD height on connect
    (setq ewm-csd-height (ewm--detect-csd-height))
    (message "EWM: detected CSD height: %d" ewm-csd-height)
    (setq ewm--process
          (make-network-process
           :name "ewm"
           :buffer (generate-new-buffer " *ewm-input*")
           :family 'local
           :service path
           :filter #'ewm--filter
           :sentinel #'ewm--sentinel))
    (ewm--enable-layout-sync)
    (ewm-input--enable)
    ;; Send prefix keys to compositor for char-mode handling
    (ewm--send-prefix-keys)
    (message "EWM: connected to %s" path)))

(defun ewm-disconnect ()
  "Disconnect from compositor."
  (interactive)
  (ewm--disable-layout-sync)
  (ewm-input--disable)
  (when ewm--process
    (delete-process ewm--process)
    (setq ewm--process nil)
    (message "EWM: disconnected")))

;;; Input handling (adapted from exwm-input.el)
;;
;; In EXWM, line-mode intercepts all keys via XGrabKey and decides whether
;; each key goes to Emacs or the X window.  Keys without Emacs bindings
;; are replayed to the X window, so regular typing works immediately.
;;
;; EWM achieves similar behavior by:
;; - Always keeping keyboard focus on the viewed surface
;; - Regular typing goes directly to the surface (like EXWM)
;; - Prefix keys (C-x, M-x, etc.) are intercepted by the compositor and
;;   redirected to Emacs, enabling Emacs commands while viewing a surface
;;
;; Customize `ewm-input-prefix-keys' to add more keys that should go to Emacs.

(defgroup ewm-input nil
  "EWM input handling."
  :group 'ewm)

(defvar-local ewm-input--mode 'line-mode
  "Current input mode: `line-mode' or `char-mode'.")

(defcustom ewm-input-prefix-keys
  '(?\C-x ?\C-u ?\C-h ?\M-x ?\M-` ?\M-& ?\M-:)
  "Keys that always go to Emacs, even in char-mode.
These keys switch keyboard focus back to Emacs.
Adapted from `exwm-input-prefix-keys'."
  :type '(repeat character)
  :group 'ewm-input)

(defcustom ewm-input-simulation-keys nil
  "Simulation keys for translating Emacs keys to application keys.
Each element is (EMACS-KEY . APP-KEY).
In line-mode, when EMACS-KEY is pressed in a surface buffer,
APP-KEY is sent to the surface.
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
  (when ewm-surface-id
    (setq ewm-input--mode mode)
    ;; Always focus the surface - this matches EXWM behavior where the X window
    ;; has focus even in line-mode (EXWM intercepts via XGrabKey)
    (setq ewm-input--last-focused-id ewm-surface-id)
    (ewm-focus ewm-surface-id)
    (force-mode-line-update)))

(defun ewm-input-char-mode ()
  "Switch to char-mode: keys go directly to surface.
Press a prefix key to return to line-mode."
  (interactive)
  (ewm-input--update-mode 'char-mode)
  (message "EWM: char-mode"))

(defun ewm-input-line-mode ()
  "Switch to line-mode: keys go to Emacs."
  (interactive)
  (ewm-input--update-mode 'line-mode)
  (message "EWM: line-mode"))

(defun ewm-input-toggle-mode ()
  "Toggle between line-mode and char-mode."
  (interactive)
  (ewm-input--update-mode
   (if (eq ewm-input--mode 'char-mode) 'line-mode 'char-mode))
  (message "EWM: %s" ewm-input--mode))

(defun ewm-input-send-key (key)
  "Send KEY to the current surface.
KEY should be a key sequence."
  (interactive "kKey: ")
  (when ewm-surface-id
    (ewm--send `(:cmd "key" :id ,ewm-surface-id :key ,(key-description key)))))

;; Internal variables for focus tracking
(defvar ewm-input--skip-buffer-list-update nil
  "Non-nil to skip `ewm-input--on-buffer-list-update'.
Used when buffer changes are expected and focus should not change.")

(defvar ewm-input--last-focused-id nil
  "Last surface ID that was focused.
Used to avoid sending redundant focus commands.")

(defun ewm-input--on-buffer-list-update ()
  "Hook called when buffer list changes.
Updates keyboard focus based on current buffer.
Adapted from `exwm-input--on-buffer-list-update'.

Like EXWM, when viewing a surface buffer, the surface has keyboard focus
so that typing works immediately.  When viewing a non-surface buffer,
Emacs (surface 1) has focus."
  (when (and ewm--process
             (process-live-p ewm--process)
             (not ewm-input--skip-buffer-list-update)
             ;; Don't process during minibuffer operations
             (not (minibufferp)))
    (let* ((buf (current-buffer))
           (id (buffer-local-value 'ewm-surface-id buf))
           ;; Surface buffer: focus the surface (like EXWM)
           ;; Non-surface buffer: focus Emacs
           (target-id (or id 1)))
      ;; Only send focus command if target changed
      (unless (eq target-id ewm-input--last-focused-id)
        (setq ewm-input--last-focused-id target-id)
        (ewm-focus target-id)))))

(defun ewm-input--on-post-command ()
  "Hook called after each command.
Re-focuses the surface if we're in a surface buffer.
This handles the case where the compositor intercepted a prefix key
and temporarily redirected focus to Emacs."
  (when (and ewm--process
             (process-live-p ewm--process)
             (not (minibufferp)))
    (let ((id (buffer-local-value 'ewm-surface-id (current-buffer))))
      (when id
        ;; We're in a surface buffer - ensure focus is on the surface
        ;; Reset last-focused-id to force the focus command
        (setq ewm-input--last-focused-id id)
        (ewm-focus id)))))

(defun ewm-input--enable ()
  "Enable EWM input handling."
  (setq ewm-input--last-focused-id 1)  ; Start with Emacs focused
  (add-hook 'buffer-list-update-hook #'ewm-input--on-buffer-list-update)
  (add-hook 'post-command-hook #'ewm-input--on-post-command))

(defun ewm-input--disable ()
  "Disable EWM input handling."
  (setq ewm-input--last-focused-id nil)
  (remove-hook 'buffer-list-update-hook #'ewm-input--on-buffer-list-update)
  (remove-hook 'post-command-hook #'ewm-input--on-post-command))

(defun ewm--send-prefix-keys ()
  "Send prefix keys configuration to compositor.
Compositor uses these to switch focus back to Emacs in char-mode."
  ;; Convert prefix keys to string descriptions for the compositor.
  ;; Handle both character codes (integers) and string formats.
  ;; Use vconcat to create a vector, which json-serialize treats as an array.
  (let ((keys (vconcat (mapcar (lambda (key)
                                 (if (stringp key)
                                     key  ; Already a string description
                                   (single-key-description key)))
                               ewm-input-prefix-keys))))
    (ewm--send `(:cmd "prefix-keys" :keys ,keys))))

;;; Surface mode

(defvar-local ewm-surface-id nil
  "Surface ID for this buffer.")

(defvar-local ewm-surface-app nil
  "Application name for this buffer.")

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

(require 'cl-lib)

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

(defun ewm--frame-y-offset (&optional frame)
  "Calculate Y offset for FRAME to account for CSD, menu bar, and tool bar."
  (let* ((frame (or frame (selected-frame)))
         (geometry (frame-geometry frame))
         (csd-height (or ewm-csd-height 0))
         ;; Menu bar: use frame-geometry, but fall back to checking menu-bar-mode
         (menu-bar-from-geom (or (cdr (alist-get 'menu-bar-size geometry)) 0))
         (menu-bar-height (if (> menu-bar-from-geom 0)
                              menu-bar-from-geom
                            ;; PGTK may not report menu bar in geometry
                            ;; Check if menu-bar-mode is enabled and estimate height
                            (if (and (frame-parameter frame 'menu-bar-lines)
                                     (> (frame-parameter frame 'menu-bar-lines) 0))
                                30  ; Typical GTK menu bar height
                              0)))
         (tool-bar-height (or (cdr (alist-get 'tool-bar-size geometry)) 0))
         (tab-bar-height (or (cdr (alist-get 'tab-bar-size geometry)) 0)))
    ;; Add: CSD height + all bars
    (+ csd-height menu-bar-height tool-bar-height tab-bar-height)))

(defun ewm-layout--show (id &optional window)
  "Show surface ID exactly fit in the Emacs window WINDOW.
Adapted from exwm-layout--show."
  (let* ((edges (ewm--window-inside-absolute-pixel-edges window))
         (x (pop edges))
         (y (pop edges))
         (width (- (pop edges) x))
         (height (- (pop edges) y))
         (y-offset (ewm--frame-y-offset (window-frame window)))
         ;; Final coordinates: add bar offset since absolute edges are
         ;; relative to content area, not the compositor output
         (final-y (+ y y-offset)))
    ;; Log debug info
    (let* ((frame (window-frame window))
           (geometry (frame-geometry frame)))
      (with-temp-buffer
        (insert (format "=== Layout Debug ===\n"))
        (insert (format "Surface ID: %d\n" id))
        (insert (format "Window: %s\n" window))
        (insert (format "Absolute edges: (%d %d %d %d)\n" x y (+ x width) (+ y height)))
        (insert (format "Calculated: x=%d y=%d w=%d h=%d\n" x y width height))
        (insert (format "CSD height: %s\n" (or ewm-csd-height 0)))
        (insert (format "menu-bar-lines: %s\n" (frame-parameter frame 'menu-bar-lines)))
        (insert (format "menu-bar-size from geometry: %s\n" (alist-get 'menu-bar-size geometry)))
        (insert (format "tool-bar-size from geometry: %s\n" (alist-get 'tool-bar-size geometry)))
        (insert (format "Y-offset (total): %d\n" y-offset))
        (insert (format "Sending: (%d, %d) %dx%d\n" x final-y width height))
        (write-region (point-min) (point-max) "/tmp/ewm-layout.txt")))
    (ewm-layout id x final-y width height)))

(defun ewm-layout--refresh ()
  "Refresh layout for all surface buffers.
Shows surfaces that are displayed in windows, hides others.
Prioritizes selected window, then uses first window found.
Adapted from exwm-layout--refresh-workspace."
  (when (and ewm--process (process-live-p ewm--process))
    ;; First pass: find which surfaces are visible and where
    (let ((visible-surfaces (make-hash-table :test 'eql)))
      ;; Check selected window first (priority for focused surface)
      (let* ((sel-window (selected-window))
             (sel-buf (window-buffer sel-window))
             (sel-id (buffer-local-value 'ewm-surface-id sel-buf)))
        (when sel-id
          (puthash sel-id sel-window visible-surfaces)))
      ;; Then check all other windows
      (dolist (frame (frame-list))
        (dolist (window (window-list frame 'no-minibuf))
          (let* ((buf (window-buffer window))
                 (id (buffer-local-value 'ewm-surface-id buf)))
            ;; Only process each surface once (selected window already handled)
            (when (and id (not (gethash id visible-surfaces)))
              (puthash id window visible-surfaces)))))
      ;; Second pass: show visible surfaces, hide others
      (maphash
       (lambda (id info)
         (let ((window (gethash id visible-surfaces)))
           (if window
               (ewm-layout--show id window)
             (ewm-hide id))))
       ewm--surfaces))))

(defun ewm--window-config-change ()
  "Hook called when window configuration changes."
  (ewm-layout--refresh))

(defun ewm--enable-layout-sync ()
  "Enable automatic layout sync."
  (add-hook 'window-configuration-change-hook #'ewm--window-config-change))

(defun ewm--disable-layout-sync ()
  "Disable automatic layout sync."
  (remove-hook 'window-configuration-change-hook #'ewm--window-config-change))

(provide 'ewm)
;;; ewm.el ends here
