;;; ewm-focus.el --- Focus handling for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Compositor owns focus.  Lisp -> compositor via the overridden
;; `select-frame-set-input-focus'; compositor -> Lisp via Focus events
;; routed to `ewm--handle-focus'.  Layouts never carry focus, so
;; programmatic `select-frame' from process filters etc. cannot steal it.

;;; Code:

(require 'cl-lib)

(declare-function ewm-input--pointer-in-window-p "ewm-input")
(declare-function ewm-focus-view-module "ewm-core" (view-id))
(declare-function ewm-layout--ensure-frame-view-id "ewm-layout" (frame))

(defvar ewm--module-mode)
(defvar ewm--surfaces)
(defvar ewm-surface-id)
(defvar ewm-focus-follows-mouse)
(defvar ewm--is-ffm-focus-event)
(defvar ewm--mff-last-window)

(defun ewm--select-frame-set-input-focus (frame &optional norecord)
  "Override skipping `x-focus-frame' so xdg_activation can't echo back."
  (select-frame frame norecord)
  (when (bound-and-true-p ewm--module-mode)
    (ewm-focus-view-module (ewm-layout--ensure-frame-view-id frame)))
  frame)

(defun ewm--minibuffer-active-p ()
  "Return non-nil if minibuffer is currently active."
  (or (active-minibuffer-window)
      (> (minibuffer-depth) 0)
      (minibufferp)))

(defun ewm--find-window-by-view-id (view-id)
  "Window whose `ewm-view-id' parameter is VIEW-ID, if any."
  (cl-some
   (lambda (frame)
     (cl-find-if (lambda (w) (eql (window-parameter w 'ewm-view-id) view-id))
                 (window-list frame 'no-minibuf)))
   (frame-list)))

(defun ewm--find-frame-by-view-id (view-id)
  "Frame whose `ewm-frame-view-id' parameter is VIEW-ID, if any."
  (cl-find view-id (frame-list)
           :key (lambda (f) (frame-parameter f 'ewm-frame-view-id))))

(defun ewm--handle-focus (event)
  "Mirror compositor focus EVENT into Emacs selection."
  (unless (ewm--minibuffer-active-p)
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
