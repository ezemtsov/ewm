;;; ewm-focus.el --- Focus handling for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Compositor owns focus.  Lisp -> compositor via the overridden
;; `select-frame-set-input-focus'; compositor -> Lisp via Focus events
;; routed to `ewm--handle-focus', which itself is gated by
;; `ewm-layout--menu-active-p' so modal reads aren't disturbed.

;;; Code:

(require 'cl-lib)

(declare-function ewm-focus-view-module "ewm-core" (view-id))
(declare-function ewm-layout--ensure-frame-view-id "ewm-layout" (frame))
(declare-function ewm-layout--menu-active-p "ewm-layout" ())

(defvar ewm--module-mode)
(defvar ewm-focus-follows-mouse)
(defvar ewm--is-ffm-focus-event)
(defvar ewm--mff-last-window)

(defun ewm--select-frame-set-input-focus (frame &optional norecord)
  "Override skipping `x-focus-frame' so xdg_activation can't echo back."
  (select-frame frame norecord)
  (when (bound-and-true-p ewm--module-mode)
    (ewm-focus-view-module (ewm-layout--ensure-frame-view-id frame)))
  frame)

(defun ewm--find-window-by-view-id (view-id)
  "Window whose `ewm-view-id' parameter is VIEW-ID, if any."
  (cl-some
   (lambda (frame)
     (cl-find-if (lambda (w) (eql (window-parameter w 'ewm-view-id) view-id))
                 (window-list frame 'no-minibuf)))
   (frame-list)))

(defun ewm--find-frame-by-view-id (view-id)
  "Frame whose chrome view-id matches VIEW-ID, if any.
Chrome view-ids have `ewm-layout--frame-view-id-marker' set; strip it to
recover the surface-id and look up by frame parameter `ewm-surface-id'."
  (when (and view-id
             (not (zerop (logand view-id ewm-layout--frame-view-id-marker))))
    (let ((sid (logand view-id (1- ewm-layout--frame-view-id-marker))))
      (cl-find sid (frame-list)
               :key (lambda (f) (frame-parameter f 'ewm-surface-id))))))

(defun ewm--handle-focus (event)
  "Mirror compositor focus EVENT into Emacs selection."
  (unless (ewm-layout--menu-active-p)
    (let* ((view-id (alist-get 'view_id event))
           (x (alist-get 'x event))
           (y (alist-get 'y event))
           (ewm--is-ffm-focus-event (and ewm-focus-follows-mouse x y)))
      (cond
       ((when-let ((win (ewm--find-window-by-view-id view-id)))
          (select-frame (window-frame win))
          (select-window win)
          (setq ewm--mff-last-window win)
          t))
       ((when-let ((frame (ewm--find-frame-by-view-id view-id)))
          (select-frame frame)
          (when-let* ((ewm--is-ffm-focus-event)
                      (window (window-at-x-y x y frame))
                      ((not (minibufferp (window-buffer window)))))
            (select-window window)
            (setq ewm--mff-last-window window))
          t))))))

(provide 'ewm-focus)
;;; ewm-focus.el ends here
