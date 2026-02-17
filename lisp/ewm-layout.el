;;; ewm-layout.el --- Layout management for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Layout management for EWM, adapted from EXWM's exwm-layout.el.
;; Handles surface positioning and window synchronization.

;;; Code:

(require 'cl-lib)

(declare-function ewm-output-layout "ewm")
(declare-function ewm-focus "ewm")
(declare-function ewm--compositor-active-p "ewm")
(declare-function ewm-get-focused-id "ewm-core")
(declare-function ewm-in-prefix-sequence-p "ewm-core")

(defvar ewm--module-mode)
(defvar ewm--surfaces)
(defvar ewm-surface-id)

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

(defun ewm--frame-y-offset (&optional _frame)
  "Return Y offset for CSD (always 0 with no decorations).
Internal bars are already reflected in `window-inside-absolute-pixel-edges'."
  0)

(defun ewm--focus-locked-p ()
  "Return non-nil if focus should not be synced to surfaces.
This covers various Emacs states where focus needs to stay on Emacs:
- Minibuffer is active
- In a prefix key sequence (compositor flag)
- Emacs is reading a key sequence (overriding-terminal-local-map)
- Universal argument pending (prefix-arg)"
  (or (active-minibuffer-window)
      (> (minibuffer-depth) 0)
      prefix-arg
      (ewm-in-prefix-sequence-p)
      ;; Emacs sets this during key sequence reading (isearch, read-key, etc.)
      (and overriding-terminal-local-map
           (keymapp overriding-terminal-local-map))))

(defun ewm-layout--send-layouts ()
  "Build and send per-output layout declarations.
Groups surface entries by output and sends them to the compositor.
The `primary' flag controls configure + direct rendering vs stretching.
A surface is primary when it appears in only one window, or when it
appears in multiple windows and this entry is the selected one."
  (let ((output-surfaces (make-hash-table :test 'equal))
        (surface-counts (make-hash-table :test 'eql))
        (window-entries nil)
        (sel-window (selected-window)))
    ;; Collect entries and count per-surface occurrences
    (dolist (frame (frame-list))
      (let ((output (frame-parameter frame 'ewm-output)))
        (when output
          (unless (gethash output output-surfaces)
            (puthash output nil output-surfaces))
          (dolist (window (window-list frame 'no-minibuf))
            (let ((id (buffer-local-value 'ewm-surface-id (window-buffer window))))
              (when id
                (puthash id (1+ (gethash id surface-counts 0)) surface-counts)
                (push (list output id window) window-entries)))))))
    ;; Build entries with correct primary flags
    (pcase-dolist (`(,output ,id ,window) (nreverse window-entries))
      ;; Primary when sole view of this surface, or selected among multiple
      (let* ((primary-p (or (= 1 (gethash id surface-counts 1))
                            (eq window sel-window)))
             (view (ewm-layout--make-output-view window primary-p)))
        (push `(:id ,id ,@view) (gethash output output-surfaces))))
    ;; Send per-output declarations
    (maphash
     (lambda (output entries)
       (ewm-output-layout output (vconcat (nreverse entries))))
     output-surfaces)))

(defun ewm-layout--refresh ()
  "Force redisplay to ensure window sizes are current, then send layouts.
Focus sync happens through the debounced `ewm-input--sync-focus'."
  (when ewm--module-mode
    (redisplay t)
    (ewm-layout--send-layouts)))

(defun ewm-layout--make-output-view (window primary-p)
  "Create a view plist for WINDOW with frame-relative coordinates.
Returns (:x X :y Y :w W :h H :primary PRIMARY-P).
Coordinates are relative to the output's working area — the compositor
converts to global positions using output geometry + working area offset."
  (let* ((edges (ewm--window-inside-absolute-pixel-edges window))
         (x (pop edges))
         (y (pop edges))
         (width (- (pop edges) x))
         (height (- (pop edges) y))
         (csd-offset (ewm--frame-y-offset (window-frame window))))
    `(:x ,x :y ,(+ y csd-offset) :w ,width :h ,height
      :primary ,(if primary-p t :false))))

(defun ewm--window-config-change ()
  "Hook called when window configuration changes.
Updates views only; focus sync happens through debounced path."
  (ewm-layout--refresh))

(defvar ewm--pre-minibuffer-surface-id nil
  "Surface ID that was focused before minibuffer opened.")

(defun ewm--minibuffer-active-p ()
  "Return non-nil if minibuffer is currently active.
More reliable than tracking with hooks since it checks actual state."
  (or (active-minibuffer-window)
      (> (minibuffer-depth) 0)
      ;; Also check if current buffer is a minibuffer
      (minibufferp)))

(defun ewm--on-minibuffer-setup ()
  "Focus Emacs frame when minibuffer activates.
Saves previous surface to restore on exit."
  (setq ewm--pre-minibuffer-surface-id (ewm-get-focused-id))
  (when-let ((frame-surface-id (frame-parameter (selected-frame) 'ewm-surface-id)))
    (ewm-focus frame-surface-id))
  (ewm-layout--refresh))

(defun ewm--on-minibuffer-exit ()
  "Restore focus to previous surface when minibuffer exits."
  (when (and ewm--pre-minibuffer-surface-id (ewm--compositor-active-p))
    (ewm-focus ewm--pre-minibuffer-surface-id)
    (setq ewm--pre-minibuffer-surface-id nil))
  (ewm-layout--refresh))

(defun ewm--on-window-size-change (_frame)
  "Refresh layout when window sizes change.
Catches minibuffer height changes that window-configuration-change misses.
Updates views only; focus sync happens through debounced path."
  (ewm-layout--refresh))

(defun ewm--on-window-selection-change (_frame)
  "Update layouts when selected window changes.
Primary flag depends on selected-window, so re-send layouts."
  (ewm-layout--send-layouts))

(defun ewm--enable-layout-sync ()
  "Enable automatic layout sync."
  (add-hook 'window-configuration-change-hook #'ewm--window-config-change)
  (add-hook 'window-size-change-functions #'ewm--on-window-size-change)
  (add-hook 'window-selection-change-functions #'ewm--on-window-selection-change)
  (add-hook 'minibuffer-setup-hook #'ewm--on-minibuffer-setup)
  (add-hook 'minibuffer-exit-hook #'ewm--on-minibuffer-exit))

(defun ewm--disable-layout-sync ()
  "Disable automatic layout sync."
  (remove-hook 'window-configuration-change-hook #'ewm--window-config-change)
  (remove-hook 'window-size-change-functions #'ewm--on-window-size-change)
  (remove-hook 'window-selection-change-functions #'ewm--on-window-selection-change)
  (remove-hook 'minibuffer-setup-hook #'ewm--on-minibuffer-setup)
  (remove-hook 'minibuffer-exit-hook #'ewm--on-minibuffer-exit))

(provide 'ewm-layout)
;;; ewm-layout.el ends here
