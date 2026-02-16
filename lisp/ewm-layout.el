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

(defun ewm-layout--refresh (&optional skip-focus-sync)
  "Refresh layout for all surface buffers and optionally sync focus.
When SKIP-FOCUS-SYNC is non-nil, only update views without syncing focus.
This is used by hooks that fire rapidly during startup or resize operations.
Focus sync should only happen through the debounced `ewm-input--sync-focus'."
  (when ewm--module-mode
    ;; Force redisplay to ensure window sizes are current
    (redisplay t)
    ;; Group surface entries by output
    (let ((output-surfaces (make-hash-table :test 'equal))
          (sel-window (selected-window))
          (sel-frame (selected-frame)))
      ;; Ensure every output with a frame gets a key (empty = clear layout)
      (dolist (frame (frame-list))
        (let ((output (frame-parameter frame 'ewm-output)))
          (when output
            (unless (gethash output output-surfaces)
              (puthash output nil output-surfaces))
            (dolist (window (window-list frame 'no-minibuf))
              (let* ((buf (window-buffer window))
                     (id (buffer-local-value 'ewm-surface-id buf)))
                (when id
                  (let ((view (ewm-layout--make-output-view window (eq window sel-window))))
                    (push `(:id ,id ,@view) (gethash output output-surfaces)))))))))
      ;; Send per-output declarations
      (maphash
       (lambda (output entries)
         (ewm-output-layout output (vconcat (nreverse entries))))
       output-surfaces)
      ;; Sync focus only if not skipped and not locked
      (unless (or skip-focus-sync (ewm--focus-locked-p))
        (let* ((sel-buf (window-buffer sel-window))
               (surface-id (buffer-local-value 'ewm-surface-id sel-buf))
               (frame-surface-id (frame-parameter sel-frame 'ewm-surface-id))
               (target-id (or surface-id frame-surface-id)))
          (when (and target-id (not (eq target-id (ewm-get-focused-id))))
            (ewm-focus target-id)))))))

(defun ewm-layout--make-output-view (window active-p)
  "Create a view plist for WINDOW with frame-relative coordinates.
Returns (:x X :y Y :w W :h H :active ACTIVE-P).
Coordinates are relative to the output's working area — the compositor
converts to global positions using output geometry + working area offset."
  (let* ((edges (ewm--window-inside-absolute-pixel-edges window))
         (x (pop edges))
         (y (pop edges))
         (width (- (pop edges) x))
         (height (- (pop edges) y))
         (csd-offset (ewm--frame-y-offset (window-frame window))))
    `(:x ,x :y ,(+ y csd-offset) :w ,width :h ,height
      :active ,(if active-p t :false))))

(defun ewm--window-config-change ()
  "Hook called when window configuration changes.
Updates views only; focus sync happens through debounced path."
  (ewm-layout--refresh 'skip-focus))

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
  (ewm-layout--refresh 'skip-focus))

(defun ewm--on-minibuffer-exit ()
  "Restore focus to previous surface when minibuffer exits."
  (when (and ewm--pre-minibuffer-surface-id (ewm--compositor-active-p))
    (ewm-focus ewm--pre-minibuffer-surface-id)
    (setq ewm--pre-minibuffer-surface-id nil))
  (redisplay t)
  (ewm-layout--refresh 'skip-focus))

(defun ewm--on-window-size-change (_frame)
  "Refresh layout when window sizes change.
Catches minibuffer height changes that window-configuration-change misses.
Updates views only; focus sync happens through debounced path."
  (ewm-layout--refresh 'skip-focus))

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
