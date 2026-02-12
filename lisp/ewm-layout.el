;;; ewm-layout.el --- Layout management for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Layout management for EWM, adapted from EXWM's exwm-layout.el.
;; Handles surface positioning and window synchronization.

;;; Code:

(require 'cl-lib)

(declare-function ewm-views "ewm")
(declare-function ewm-hide "ewm")
(declare-function ewm-focus "ewm")
(declare-function ewm--get-output-offset "ewm")
(declare-function ewm--compositor-active-p "ewm")
(declare-function ewm-get-focused-id "ewm-core")

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

(defun ewm-layout--refresh ()
  "Refresh layout for all surface buffers and sync focus."
  (when ewm--module-mode
    ;; Force redisplay to ensure window sizes are current
    (redisplay t)
    ;; Build a hash table: surface-id -> list of (window . active-p)
    (let ((surface-windows (make-hash-table :test 'eql))
          (sel-window (selected-window))
          (sel-frame (selected-frame)))
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
      ;; Deduplication happens in compositor (single source of truth)
      (maphash
       (lambda (id _info)
         (let ((windows (gethash id surface-windows)))
           (if windows
               ;; Surface is visible - send views
               (let ((views (mapcar
                             (lambda (win-pair)
                               (ewm-layout--make-view (car win-pair) (cdr win-pair)))
                             windows)))
                 (ewm-views id (vconcat views)))
             ;; Surface not visible - hide it
             (ewm-hide id))))
       ewm--surfaces)
      ;; Sync focus (unless minibuffer is active)
      (unless (ewm--minibuffer-active-p)
        (let* ((sel-buf (window-buffer sel-window))
               (surface-id (buffer-local-value 'ewm-surface-id sel-buf))
               (frame-surface-id (frame-parameter sel-frame 'ewm-surface-id))
               (target-id (or surface-id frame-surface-id)))
          (when (and target-id (not (eq target-id (ewm-get-focused-id))))
            (ewm-focus target-id)))))))

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
  (redisplay t)
  (ewm-layout--refresh))

(defun ewm--on-minibuffer-exit ()
  "Restore focus to previous surface when minibuffer exits."
  (when (and ewm--pre-minibuffer-surface-id (ewm--compositor-active-p))
    (ewm-focus ewm--pre-minibuffer-surface-id)
    (setq ewm--pre-minibuffer-surface-id nil))
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

(provide 'ewm-layout)
;;; ewm-layout.el ends here
