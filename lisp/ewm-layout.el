;;; ewm-layout.el --- Layout management for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Layout management for EWM, adapted from EXWM's exwm-layout.el.
;; Handles surface positioning and window synchronization.

;;; Code:

(require 'cl-lib)

(declare-function ewm-output-layout "ewm")
(declare-function ewm--compositor-active-p "ewm")
(declare-function ewm--sync-focus "ewm-focus")

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

(defun ewm-layout--send-layouts ()
  "Build and send per-output layout declarations as nested frame strips.
For each output, emits the strip in `ewm-strip-index' ascending order.
Each frame plist carries :name :surface-id :active :width :surfaces;
each surface plist holds :id :x :y :w :h :focused :fullscreen.
The compositor uses :focused for input/popup routing, :surface-id to
position each Emacs frame xdg_toplevel at its strip column, and
independently computes :primary across outputs."
  (let ((output-frames (make-hash-table :test 'equal))
        (sel-window (selected-window))
        (sel-frame (selected-frame)))
    (dolist (frame (frame-list))
      ;; In-flight frames (no strip-index yet) would land at strip
      ;; position 0; skip until the compositor assigns one.
      (when-let ((output (frame-parameter frame 'ewm-output))
                 (strip-index (frame-parameter frame 'ewm-strip-index)))
        (let ((surfaces nil))
          (dolist (window (window-list frame 'no-minibuf))
            (when-let ((id (buffer-local-value 'ewm-surface-id (window-buffer window))))
              (let* ((focused-p (eq window sel-window))
                     (view (ewm-layout--make-output-view window focused-p)))
                (push `(:id ,id ,@view) surfaces))))
          (let ((frame-plist
                 (list :name (or (frame-parameter frame 'name) "")
                       :surface-id (or (frame-parameter frame 'ewm-surface-id) 0)
                       :active (if (eq frame sel-frame) t :false)
                       :width (float (frame-pixel-width frame))
                       :strip-index strip-index
                       :surfaces (vconcat (nreverse surfaces)))))
            (puthash output
                     (cons frame-plist (gethash output output-frames))
                     output-frames)))))
    (maphash
     (lambda (output frames)
       (let ((sorted (sort frames
                           (lambda (a b)
                             (< (plist-get a :strip-index)
                                (plist-get b :strip-index))))))
         (ewm-output-layout output (vconcat sorted))))
     output-frames)))

(defun ewm-layout--refresh ()
  "Send current window layouts to the compositor."
  (when ewm--module-mode
    (ewm-layout--send-layouts)))

(defun ewm-layout--make-output-view (window focused-p)
  "Create a view plist for WINDOW with frame-relative coordinates.
Returns (:x X :y Y :w W :h H :focused FOCUSED-P :fullscreen FS).
Coordinates are relative to the output's working area. The compositor
converts to global positions using output geometry + working area offset."
  (let* ((edges (ewm--window-inside-absolute-pixel-edges window))
         (x (pop edges))
         (y (pop edges))
         (width (- (pop edges) x))
         (height (- (pop edges) y))
         (fullscreen (window-parameter window 'ewm-fullscreen)))
    `(:x ,x :y ,y :w ,width :h ,height
         :focused ,(if focused-p t :false)
         :fullscreen ,(if fullscreen t :false))))

(defun ewm--window-config-change ()
  "Hook called when window configuration changes."
  (ewm-layout--refresh))

(defun ewm--on-window-size-change (_frame)
  "Refresh layout when window sizes change.
Catches minibuffer height changes that window-configuration-change misses."
  (ewm-layout--refresh))

(defun ewm--on-window-selection-change (_frame)
  "Update layouts and resolve focus when selected window changes.
Primary flag depends on selected-window, so re-send layouts."
  (ewm-layout--send-layouts)
  (ewm--sync-focus))

(defun ewm--on-window-buffer-change (_frame)
  "Resolve focus when a buffer changes within a window.
Catches `switch-to-buffer' and similar operations that change the displayed
buffer without changing the selected window."
  (ewm--sync-focus))

(defun ewm--enable-layout-sync ()
  "Enable automatic layout sync."
  (add-hook 'window-configuration-change-hook #'ewm--window-config-change)
  (add-hook 'window-size-change-functions #'ewm--on-window-size-change)
  (add-hook 'window-selection-change-functions #'ewm--on-window-selection-change)
  (add-hook 'window-buffer-change-functions #'ewm--on-window-buffer-change))

(defun ewm--disable-layout-sync ()
  "Disable automatic layout sync."
  (remove-hook 'window-configuration-change-hook #'ewm--window-config-change)
  (remove-hook 'window-size-change-functions #'ewm--on-window-size-change)
  (remove-hook 'window-selection-change-functions #'ewm--on-window-selection-change)
  (remove-hook 'window-buffer-change-functions #'ewm--on-window-buffer-change))

(provide 'ewm-layout)
;;; ewm-layout.el ends here
