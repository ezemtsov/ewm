;;; ewm.el --- Emacs Wayland Manager -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; EWM integrates Emacs with a Wayland compositor, providing an EXWM-like
;; experience without the single-threaded limitations.
;;
;; Usage:
;;   1. Start compositor: cargo run (in ewm-compositor/)
;;   2. Start Emacs inside: WAYLAND_DISPLAY=wayland-ewm emacs -Q -l ewm.el
;;   3. Connect: M-x ewm-connect
;;   4. Start apps: WAYLAND_DISPLAY=wayland-ewm foot
;;   5. Switch to surface buffer: C-x b *ewm:...*
;;
;; Surfaces automatically align with the Emacs window displaying their buffer.

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

(defun ewm--handle-new-surface (event)
  "Handle new surface EVENT."
  (let* ((id (gethash "id" event))
         (app (gethash "app" event))
         (buf (generate-new-buffer (format "*ewm:%s:%d*" app id))))
    (puthash id `(:buffer ,buf :app ,app) ewm--surfaces)
    (with-current-buffer buf
      (ewm-surface-mode)
      (setq-local ewm-surface-id id)
      (setq-local ewm-surface-app app))
    (message "EWM: new surface %d (%s)" id app)))

(defun ewm--handle-close-surface (event)
  "Handle close surface EVENT."
  (let* ((id (gethash "id" event))
         (info (gethash id ewm--surfaces)))
    (when info
      (let ((buf (plist-get info :buffer)))
        (when (buffer-live-p buf)
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
    (setq ewm--process
          (make-network-process
           :name "ewm"
           :buffer (generate-new-buffer " *ewm-input*")
           :family 'local
           :service path
           :filter #'ewm--filter
           :sentinel #'ewm--sentinel))
    (ewm--enable-layout-sync)
    (message "EWM: connected to %s" path)))

(defun ewm-disconnect ()
  "Disconnect from compositor."
  (interactive)
  (ewm--disable-layout-sync)
  (when ewm--process
    (delete-process ewm--process)
    (setq ewm--process nil)
    (message "EWM: disconnected")))

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

(define-derived-mode ewm-surface-mode special-mode "EWM Surface"
  "Major mode for EWM surface buffers."
  (setq buffer-read-only t)
  ;; Kill buffer -> close window (like EXWM)
  (add-hook 'kill-buffer-query-functions
            #'ewm--kill-buffer-query-function nil t))

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
         (info (format "Window edges (absolute): %S
Window edges (relative): %S
Calculated Y offset: %d
  - ewm-csd-height: %d
  - menu-bar: %S
  - tool-bar: %S
  - tab-bar: %S
Frame undecorated: %S
"
                       abs-edges
                       rel-edges
                       y-offset
                       ewm-csd-height
                       (alist-get 'menu-bar-size geometry)
                       (alist-get 'tool-bar-size geometry)
                       (alist-get 'tab-bar-size geometry)
                       (frame-parameter frame 'undecorated))))
    (write-region info nil "/tmp/ewm-debug.txt")
    (message "Debug saved to /tmp/ewm-debug.txt")))

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

(defcustom ewm-csd-height 0
  "Height of client-side decorations in pixels.
Set this if surfaces appear shifted vertically.
GTK CSD headers are typically 35-45 pixels."
  :type 'integer
  :group 'ewm)

(defun ewm--frame-y-offset (&optional frame)
  "Calculate Y offset for FRAME to account for CSD, menu bar, and tool bar."
  (let* ((frame (or frame (selected-frame)))
         (geometry (frame-geometry frame))
         (menu-bar-height (or (cdr (alist-get 'menu-bar-size geometry)) 0))
         (tool-bar-height (or (cdr (alist-get 'tool-bar-size geometry)) 0))
         (tab-bar-height (or (cdr (alist-get 'tab-bar-size geometry)) 0)))
    ;; Add: CSD height + all bars
    (+ ewm-csd-height menu-bar-height tool-bar-height tab-bar-height)))

(defun ewm-layout--show (id &optional window)
  "Show surface ID exactly fit in the Emacs window WINDOW.
Adapted from exwm-layout--show."
  (let* ((edges (ewm--window-inside-absolute-pixel-edges window))
         (x (pop edges))
         (y (pop edges))
         (width (- (pop edges) x))
         (height (- (pop edges) y))
         ;; On PGTK, absolute edges are relative to content area.
         ;; Add frame bars offset to get compositor coordinates.
         (y-offset (ewm--frame-y-offset (window-frame window))))
    (ewm-layout id x (+ y y-offset) width height)))

(defun ewm-layout--refresh ()
  "Refresh layout for all surface buffers.
Shows surfaces that are displayed in windows, hides others.
Only sends layout for the first window displaying each surface.
Adapted from exwm-layout--refresh-workspace."
  (when (and ewm--process (process-live-p ewm--process))
    ;; First pass: find which surfaces are visible and where
    (let ((visible-surfaces (make-hash-table :test 'eql)))
      (dolist (frame (frame-list))
        (dolist (window (window-list frame 'no-minibuf))
          (let* ((buf (window-buffer window))
                 (id (buffer-local-value 'ewm-surface-id buf)))
            ;; Only process each surface once (first window wins)
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
