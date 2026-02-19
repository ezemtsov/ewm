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
  "Build and send per-output layout declarations.
Groups surface entries by output and sends them to the compositor.
The `focused' flag marks the selected-window entry for each surface.
The compositor uses it for focus routing and popup placement.
The compositor independently computes which entry is `primary'
\(largest area) for configure size and rendering decisions."
  (let ((output-surfaces (make-hash-table :test 'equal))
        (window-entries nil)
        (sel-window (selected-window)))
    ;; Collect entries
    (dolist (frame (frame-list))
      (let ((output (frame-parameter frame 'ewm-output)))
        (when output
          (unless (gethash output output-surfaces)
            (puthash output nil output-surfaces))
          (dolist (window (window-list frame 'no-minibuf))
            (let ((id (buffer-local-value 'ewm-surface-id (window-buffer window))))
              (when id
                (push (list output id window) window-entries)))))))
    ;; Build entries — mark selected-window as focused
    (pcase-dolist (`(,output ,id ,window) (nreverse window-entries))
      (let* ((focused-p (eq window sel-window))
             (view (ewm-layout--make-output-view window focused-p)))
        (push `(:id ,id ,@view) (gethash output output-surfaces))))
    ;; Send per-output declarations with tab state
    (maphash
     (lambda (output entries)
       (ewm-output-layout output
                          (vconcat (nreverse entries))
                          (ewm-layout--collect-tabs output)))
     output-surfaces)))

(defun ewm-layout--refresh ()
  "Send current window layouts to the compositor."
  (when ewm--module-mode
    (ewm-layout--send-layouts)))

(defun ewm-layout--make-output-view (window focused-p)
  "Create a view plist for WINDOW with frame-relative coordinates.
Returns (:x X :y Y :w W :h H :focused FOCUSED-P).
Coordinates are relative to the output's working area — the compositor
converts to global positions using output geometry + working area offset."
  (let* ((edges (ewm--window-inside-absolute-pixel-edges window))
         (x (pop edges))
         (y (pop edges))
         (width (- (pop edges) x))
         (height (- (pop edges) y)))
    `(:x ,x :y ,y :w ,width :h ,height
      :focused ,(if focused-p t :false))))

(defun ewm--window-config-change ()
  "Hook called when window configuration changes."
  (ewm-layout--refresh))

(defun ewm--on-minibuffer-setup ()
  "Refresh layout and resolve focus when minibuffer activates."
  (ewm-layout--refresh)
  (ewm--sync-focus))

(defun ewm--on-minibuffer-exit ()
  "Refresh layout and resolve focus when minibuffer exits."
  (ewm-layout--refresh)
  (ewm--sync-focus))

(defun ewm--on-window-size-change (_frame)
  "Refresh layout when window sizes change.
Catches minibuffer height changes that window-configuration-change misses."
  (ewm-layout--refresh))

(defun ewm--on-window-selection-change (_frame)
  "Update layouts and resolve focus when selected window changes.
Primary flag depends on selected-window, so re-send layouts."
  (ewm-layout--send-layouts)
  (ewm--sync-focus))

(defun ewm-layout--collect-tabs (output)
  "Collect tab state for OUTPUT as a vector of plists.
Each element is a plist with :name and :active keys."
  (let ((frame (cl-find-if (lambda (f)
                             (equal (frame-parameter f 'ewm-output) output))
                           (frame-list))))
    (if frame
        (let ((tabs (funcall tab-bar-tabs-function frame)))
          (vconcat
           (mapcar (lambda (tab)
                     (let ((name (alist-get 'name tab)))
                       (list :name (or name "")
                             :active (if (eq (car tab) 'current-tab) t :false))))
                   tabs)))
      (vector))))

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
  (add-hook 'window-buffer-change-functions #'ewm--on-window-buffer-change)
  (add-hook 'minibuffer-setup-hook #'ewm--on-minibuffer-setup)
  (add-hook 'minibuffer-exit-hook #'ewm--on-minibuffer-exit))

(defun ewm--disable-layout-sync ()
  "Disable automatic layout sync."
  (remove-hook 'window-configuration-change-hook #'ewm--window-config-change)
  (remove-hook 'window-size-change-functions #'ewm--on-window-size-change)
  (remove-hook 'window-selection-change-functions #'ewm--on-window-selection-change)
  (remove-hook 'window-buffer-change-functions #'ewm--on-window-buffer-change)
  (remove-hook 'minibuffer-setup-hook #'ewm--on-minibuffer-setup)
  (remove-hook 'minibuffer-exit-hook #'ewm--on-minibuffer-exit))

(provide 'ewm-layout)
;;; ewm-layout.el ends here
