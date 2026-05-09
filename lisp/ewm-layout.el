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

(defvar ewm--module-mode)
(defvar ewm--surfaces)
(defvar ewm-surface-id)

(defun ewm-layout--ensure-view-id (window surface-id)
  "Return a stable view id for WINDOW showing SURFACE-ID."
  (let ((stored (window-parameter window 'ewm-view-id))
        (cached (window-parameter window 'ewm-view-surface)))
    (if (and stored (eql cached surface-id))
        stored
      (let ((id (random most-positive-fixnum)))
        (set-window-parameter window 'ewm-view-id id)
        (set-window-parameter window 'ewm-view-surface surface-id)
        id))))

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
Iterates `ewm--strips' so the order is the strip's own; the frame
containing `selected-frame' carries `:active t', the single source of
truth driving compositor strip activation."
  (let ((sel-window (selected-window))
        (sel-frame (selected-frame)))
    (dolist (strip ewm--strips)
      (let ((output (car strip))
            (frames-out nil))
        (dolist (frame (cdr strip))
          (let ((surfaces nil))
            (dolist (window (window-list frame 'no-minibuf))
              (when-let ((id (buffer-local-value 'ewm-surface-id (window-buffer window))))
                (let* ((focused-p (eq window sel-window))
                       (view-id (ewm-layout--ensure-view-id window id))
                       (view (ewm-layout--make-output-view window focused-p)))
                  (push `(:id ,id :view-id ,view-id ,@view) surfaces))))
            (push (list :name (or (frame-parameter frame 'name) "")
                        :surface-id (or (frame-parameter frame 'ewm-surface-id) 0)
                        :width (float (frame-pixel-width frame))
                        :active (if (eq frame sel-frame) t :false)
                        :surfaces (vconcat (nreverse surfaces)))
                  frames-out)))
        (let ((sorted (nreverse frames-out)))
          (ewm-output-layout output (vconcat sorted)))))))

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
  "Re-send layouts when selected window changes.
Compositor focus is driven by user commands (`ewm-frame-select',
windmove, click), not by Emacs-internal selection churn from process
filters like telega."
  (ewm-layout--send-layouts))

(defun ewm--on-window-buffer-change (_frame)
  "Re-send layouts when a buffer changes within a window."
  (ewm-layout--send-layouts))

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
