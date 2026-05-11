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
(declare-function ewm-focus-surface-module "ewm-core" (id))

(defvar ewm--module-mode)
(defvar ewm--surfaces)
(defvar ewm-surface-id)
(defvar ewm-focus-follows-mouse)
(defvar ewm--is-ffm-focus-event)
(defvar ewm--mff-last-window)

(defun ewm--select-frame-set-input-focus (frame &optional norecord)
  "Override skipping `x-focus-frame' so xdg_activation can't echo back."
  (select-frame frame norecord)
  (when-let* (((bound-and-true-p ewm--module-mode))
              (id (frame-parameter frame 'ewm-surface-id)))
    (ewm-focus-surface-module id))
  frame)

(defun ewm--minibuffer-active-p ()
  "Return non-nil if minibuffer is currently active."
  (or (active-minibuffer-window)
      (> (minibuffer-depth) 0)
      (minibufferp)))

(defun ewm--handle-focus (event)
  "Mirror compositor focus EVENT into Emacs selection."
  (unless (ewm--minibuffer-active-p)
    (let* ((id (alist-get 'id event))
           (x (alist-get 'x event))
           (y (alist-get 'y event))
           (ewm--is-ffm-focus-event (and ewm-focus-follows-mouse x y))
           (buf (gethash id ewm--surfaces)))
      (cond
       ((and buf (buffer-live-p buf))
        (if-let ((win (or (cl-find-if #'ewm-input--pointer-in-window-p
                                      (get-buffer-window-list buf nil t))
                          (get-buffer-window buf t))))
            (progn
              (select-frame (window-frame win))
              (select-window win)
              (setq ewm--mff-last-window win))
          (pop-to-buffer-same-window buf)))
       (t
        (when-let* ((frame (cl-find id (frame-list)
                                    :key (lambda (f) (frame-parameter f 'ewm-surface-id)))))
          (select-frame frame)
          (when-let* ((ewm--is-ffm-focus-event)
                      (window (window-at-x-y x y frame))
                      ((not (minibufferp (window-buffer window)))))
            (select-window window)
            (setq ewm--mff-last-window window))))))))

(provide 'ewm-focus)
;;; ewm-focus.el ends here
