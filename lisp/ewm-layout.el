;;; ewm-layout.el --- Layout management for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Layout management.  Also owns the modal focus contract --
;; `:selected-entry-id' is suppressed while Emacs is reading keys for
;; itself; see `ewm-layout-menu-predicates' and `docs/focus-design.md'.

;;; Code:

(require 'cl-lib)
(require 'ewm-surface)

(declare-function ewm-output-layout "ewm")
(declare-function ewm-floating-layout "ewm")

(defvar ewm--module-mode)
(defvar ewm--floating-frames)

(defgroup ewm-layout nil
  "EWM layout and modal focus contract."
  :group 'ewm)

(defun ewm-layout--reading-key-p ()
  "Non-nil while an overriding keymap is consuming keystrokes."
  (or overriding-terminal-local-map
      overriding-local-map))

(defun ewm-layout--minibuffer-menu-p ()
  "Non-nil while a completion minibuffer is active.
A plain prompt stays unpinned so its value can come from another window."
  (and (active-minibuffer-window) minibuffer-completion-table))

(defcustom ewm-layout-menu-predicates
  '(ewm-layout--minibuffer-menu-p
    ewm-layout--reading-key-p)
  "Predicates that signal Emacs is reading keys for itself.
If any returns non-nil, the next layout's `:selected-entry-id' is
suppressed for every frame."
  :type '(repeat function)
  :group 'ewm-layout)

(defun ewm-layout--menu-active-p ()
  "Non-nil if any `ewm-layout-menu-predicates' entry says so."
  (run-hook-with-args-until-success 'ewm-layout-menu-predicates))

(defconst ewm-layout--frame-focus-id-marker (ash 1 32)
  "Bit 32 marker for frame chrome focus ids.
Entry ids are random in [1, 2^32-1] with bit 32 unset; frame chrome
focus ids are `(logior MARKER SURFACE-ID)' with bit 32 set.  Disjoint ranges
let compositor and lisp derive the same id from surface_id without coordination.")

(defun ewm-layout--fresh-entry-id ()
  "Generate a non-zero random entry id with bit 32 unset.
The bit-32 range is reserved for frame chrome (see
`ewm-layout--frame-focus-id-marker')."
  (1+ (random (1- (ash 1 32)))))

(defun ewm-layout--ensure-entry-id (window)
  "Return a stable entry id for WINDOW."
  (or (window-parameter window 'ewm-entry-id)
      (let ((id (ewm-layout--fresh-entry-id)))
        (set-window-parameter window 'ewm-entry-id id)
        id)))

(defun ewm-layout--ensure-frame-focus-id (frame)
  "Return FRAME's chrome focus id derived from its `ewm-surface-id'.
Computed deterministically so the compositor can construct the same id at
`new_toplevel' without coordination."
  (when-let ((sid (frame-parameter frame 'ewm-surface-id)))
    (logior ewm-layout--frame-focus-id-marker sid)))

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

(defun ewm-layout--build-frame-plist (frame menu-active)
  "Plist describing FRAME for `ewm-output-layout'.
When MENU-ACTIVE is non-nil, `:selected-entry-id' is nil so the
compositor parks keyboard focus on the frame chrome."
  (let (entries)
    (dolist (window (window-list frame 'no-minibuf))
      (when-let* ((entry-id (ewm-layout--ensure-entry-id window))
                  (geometry (ewm-layout--make-output-entry window)))
        (let ((surface-id (ewm-surface-buffer-p (window-buffer window))))
          (push `(,@(when surface-id `(:surface-id ,surface-id))
                  :entry-id ,entry-id
                  :name ,(buffer-name (window-buffer window))
                  ,@geometry)
                entries))))
    (let ((selected-entry-id
           (unless menu-active
             (when-let* ((sel-win (frame-selected-window frame)))
               (unless (window-minibuffer-p sel-win)
                 (ewm-layout--ensure-entry-id sel-win))))))
      (list :name (or (frame-parameter frame 'name) "")
            :surface-id (or (frame-parameter frame 'ewm-surface-id) 0)
            :focus-id (ewm-layout--ensure-frame-focus-id frame)
            :selected-entry-id selected-entry-id
            :width (float (frame-pixel-width frame))
            :height (float (frame-pixel-height frame))
            :entries (vconcat (nreverse entries))))))

(defun ewm-layout--send-layouts ()
  "Send per-output frame layouts; suppress `:selected-entry-id' while modal."
  (let ((menu-active (ewm-layout--menu-active-p)))
    (cl-flet ((send (alist fn)
                (dolist (e alist)
                  (funcall fn (car e)
                           (vconcat (mapcar (lambda (f)
                                              (ewm-layout--build-frame-plist f menu-active))
                                            (cdr e)))))))
      (send ewm--strips #'ewm-output-layout)
      (send ewm--floating-frames #'ewm-floating-layout))))

(defun ewm-layout--refresh ()
  "Send current window layouts to the compositor."
  (when ewm--module-mode
    (ewm-layout--send-layouts)))

(defun ewm-layout--make-output-entry (window)
  "Create an entry geometry plist for WINDOW with frame-relative coordinates.
Returns (:x X :y Y :w W :h H).  Coordinates are relative to the output's
working area; compositor adds output geometry + working area offset."
  (let* ((edges (ewm--window-inside-absolute-pixel-edges window))
         (x (pop edges))
         (y (pop edges))
         (width (- (pop edges) x))
         (height (- (pop edges) y)))
    `(:x ,x :y ,y :w ,width :h ,height)))

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

(defvar ewm-layout--last-menu-active nil
  "Cached previous `ewm-layout--menu-active-p' for edge detection.")

(defvar ewm-layout-menu-deactivated-hook nil
  "Edge-triggered when a menu/minibuffer stops being active.")

(defun ewm-layout--on-post-command ()
  "Refresh layout on a modal-state transition."
  ;; Coerce to t: the predicates return a window or a keymap, not t, and
  ;; `eq' against those would never match across commands.
  (let ((now (and (ewm-layout--menu-active-p) t)))
    (unless (eq now ewm-layout--last-menu-active)
      (setq ewm-layout--last-menu-active now)
      (unless now (run-hooks 'ewm-layout-menu-deactivated-hook))
      (ewm-layout--refresh))))

(defun ewm--enable-layout-sync ()
  "Enable automatic layout sync."
  (setq ewm-layout--last-menu-active (and (ewm-layout--menu-active-p) t))
  (add-hook 'window-configuration-change-hook #'ewm--window-config-change)
  (add-hook 'window-size-change-functions #'ewm--on-window-size-change)
  (add-hook 'window-selection-change-functions #'ewm--on-window-selection-change)
  (add-hook 'window-buffer-change-functions #'ewm--on-window-buffer-change)
  (add-hook 'post-command-hook #'ewm-layout--on-post-command))

(defun ewm--disable-layout-sync ()
  "Disable automatic layout sync."
  (remove-hook 'window-configuration-change-hook #'ewm--window-config-change)
  (remove-hook 'window-size-change-functions #'ewm--on-window-size-change)
  (remove-hook 'window-selection-change-functions #'ewm--on-window-selection-change)
  (remove-hook 'window-buffer-change-functions #'ewm--on-window-buffer-change)
  (remove-hook 'post-command-hook #'ewm-layout--on-post-command))

(provide 'ewm-layout)
;;; ewm-layout.el ends here
