;;; ewm-focus.el --- Focus management for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Bidirectional focus synchronization between Emacs and the compositor.
;;
;; Emacs -> compositor:
;;   ewm--focus-target  (compute target)
;;   ewm--sync-focus    (send if changed)
;;   ewm-focus          (module call)
;;
;; Compositor -> Emacs:
;;   ewm--handle-focus  (select window for clicked surface)

;;; Code:

(require 'cl-lib)

(declare-function ewm-focus-module "ewm-core")
(declare-function ewm-get-focused-id "ewm-core")
(declare-function ewm-in-prefix-sequence-p "ewm-core")
(declare-function ewm-input--pointer-in-window-p "ewm-input")

(defvar ewm--module-mode)
(defvar ewm--surfaces)
(defvar ewm-surface-id)
(defvar ewm-focus-follows-mouse)
(defvar ewm--is-ffm-focus-event)
(defvar ewm--mff-last-window)

(defun ewm--minibuffer-active-p ()
  "Return non-nil if minibuffer is currently active."
  (or (active-minibuffer-window)
      (> (minibuffer-depth) 0)
      (minibufferp)))

;;; Emacs -> compositor

(defun ewm-focus (id)
  "Focus surface ID in the compositor."
  (ewm-focus-module id))

(defun ewm--focus-target ()
  "Compute the surface ID that should have compositor focus, or nil.
Returns nil when focus should not change (prefix sequence, key reading).
Returns the frame's surface ID when the minibuffer is active or a
non-surface buffer is selected.  Returns the buffer's surface ID when
a surface buffer is selected."
  (cond
   ((or prefix-arg
        (ewm-in-prefix-sequence-p)
        (and overriding-terminal-local-map
             (keymapp overriding-terminal-local-map)))
    nil)
   ((ewm--minibuffer-active-p)
    (frame-parameter (selected-frame) 'ewm-surface-id))
   (t
    (or (buffer-local-value 'ewm-surface-id
                            (window-buffer (selected-window)))
        (frame-parameter (selected-frame) 'ewm-surface-id)))))

(defun ewm--sync-focus ()
  "Sync compositor focus from Emacs state.
Calls `ewm-focus' only when the target differs from current focus."
  (when ewm--module-mode
    (when-let* ((target (ewm--focus-target)))
      (unless (eq target (ewm-get-focused-id))
        (ewm-focus target)))))

;;; Compositor -> Emacs

(defun ewm--handle-focus (event)
  "Handle focus EVENT from compositor.
Selects the window displaying the surface's buffer, or displays it if hidden.
When multiple windows show the same buffer, picks the one under the pointer."
  (let* ((id (alist-get 'id event))
         (x (alist-get 'x event))
         (y (alist-get 'y event))
         (ewm--is-ffm-focus-event (and ewm-focus-follows-mouse x y)))
    ;; handle wayland surfaces
    (if-let* (((not (ewm--minibuffer-active-p)))
              (buf (gethash id ewm--surfaces))
              ((buffer-live-p buf)))
        (let ((win (or (cl-find-if #'ewm-input--pointer-in-window-p
                                   (get-buffer-window-list buf nil t))
                       (get-buffer-window buf t))))
          (if win
              ;; select-frame (not select-frame-set-input-focus) to avoid
              ;; xdg_activation stealing focus back from the compositor.
              (progn
                (select-frame (window-frame win))
                (select-window win)
                (setq ewm--mff-last-window win))
            (pop-to-buffer-same-window buf)))
      ;; handle emacs windows with mouse hover
      (when (and ewm--is-ffm-focus-event (not (ewm--minibuffer-active-p)))
        (when-let* ((frame (cl-find id (frame-list)
                                    :key (lambda (f) (frame-parameter f 'ewm-surface-id))))
                    (window (window-at-x-y x y frame)))
          (unless (minibufferp (window-buffer window))
            (select-frame frame)
            (select-window window)
            (setq ewm--mff-last-window window)))))))

(provide 'ewm-focus)
;;; ewm-focus.el ends here
