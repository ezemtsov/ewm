;;; ewm-focus.el --- Focus handling for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Compositor owns focus.  Lisp -> compositor via the overridden
;; `select-frame-set-input-focus'; compositor -> Lisp via Focus events
;; routed to `ewm--handle-focus', which itself is gated by
;; `ewm-layout--menu-active-p' so modal reads aren't disturbed.

;;; Code:

(require 'ewm-module)
(ewm-module--declare)

(declare-function ewm-layout--ensure-frame-focus-id "ewm-layout" (frame))
(declare-function ewm-layout--menu-active-p "ewm-layout" ())
(declare-function ewm--frame-by-surface-id "ewm" (surface-id))

(defvar ewm--module-mode)
(defvar ewm-focus-follows-mouse)
(defvar ewm--is-ffm-focus-event)
(defvar ewm--is-pointer-focus-event)
(defvar ewm--mff-last-window)

(defun ewm--select-frame-set-input-focus (frame &optional norecord)
  "Override skipping `x-focus-frame' so xdg_activation can't echo back."
  (select-frame frame norecord)
  (when (bound-and-true-p ewm--module-mode)
    ;; Queue focus for an in-flight frame until its surface arrives.
    (if-let* ((focus-id (ewm-layout--ensure-frame-focus-id frame)))
        (ewm-focus-target-module focus-id)
      (set-frame-parameter frame 'ewm-focus-on-create t)))
  frame)

(defun ewm--find-window-by-entry-id (entry-id)
  "Window whose `ewm-entry-id' parameter is ENTRY-ID, if any."
  (get-window-with-predicate
   (lambda (window) (eql (window-parameter window 'ewm-entry-id) entry-id))
   'no-minibuf t))

(defun ewm--find-frame-by-focus-id (focus-id)
  "Frame whose chrome focus id matches FOCUS-ID, if any.
Chrome focus ids have `ewm-layout--frame-focus-id-marker' set; strip it to
recover the surface-id."
  (when (and focus-id
             (not (zerop (logand focus-id ewm-layout--frame-focus-id-marker))))
    (ewm--frame-by-surface-id
     (logand focus-id (1- ewm-layout--frame-focus-id-marker)))))

(defun ewm--handle-focus (event)
  "Mirror compositor focus EVENT into Emacs selection."
  (unless (ewm-layout--menu-active-p)
    (let* ((focus-id (alist-get 'focus_id event))
           (x (alist-get 'x event))
           (y (alist-get 'y event))
           (pointer (alist-get 'pointer event))
           (ewm--is-pointer-focus-event (and pointer x y))
           (ewm--is-ffm-focus-event
            (and ewm-focus-follows-mouse ewm--is-pointer-focus-event)))
      (cond
       ((when-let ((win (ewm--find-window-by-entry-id focus-id)))
          (select-frame (window-frame win))
          (select-window win)
          (setq ewm--mff-last-window win)
          t))
       ((when-let ((frame (ewm--find-frame-by-focus-id focus-id)))
          (select-frame frame)
          (when ewm--is-ffm-focus-event
            (when-let* ((window (window-at-x-y x y frame))
                        ((not (minibufferp (window-buffer window)))))
              (select-window window)))
          ;; Anchor only on pointer focus; keyboard events must leave it stale so MFF warps.
          (when ewm--is-pointer-focus-event
            (setq ewm--mff-last-window (selected-window)))
          t))))))

(provide 'ewm-focus)
;;; ewm-focus.el ends here
