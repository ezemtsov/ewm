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
(require 'ewm-layout)
(require 'ewm-frame)

(defcustom ewm-focus-follows-mouse nil
  "Whether moving the mouse pointer should focus applications.
The compositor sends focus events with cursor coordinates; intra-frame
window switching is handled by `ewm--handle-focus' from those events."
  :type 'boolean
  :group 'ewm-input-devices
  :set (ewm--send-on-set #'ewm--send-focus-follows-mouse))

(defun ewm--send-focus-follows-mouse ()
  "Mirror `ewm-focus-follows-mouse' into Emacs and the compositor."
  (setq focus-follows-mouse (and ewm-focus-follows-mouse t))
  (ewm-set-focus-follows-mouse ewm-focus-follows-mouse))

(defvar ewm--mff-last-window nil
  "Last window for mouse-follows-focus, to avoid redundant warps.")

(defun ewm--select-frame-set-input-focus (frame &optional norecord)
  "Override skipping `x-focus-frame' so xdg_activation can't echo back."
  (select-frame frame norecord)
  (when ewm--module-mode
    ;; Queue focus for an in-flight frame until its surface arrives.
    (if-let* ((focus-id (ewm-layout--ensure-frame-focus-id frame)))
        (ewm-focus-target-module focus-id)
      (set-frame-parameter frame 'ewm-focus-on-create t)))
  frame)

(defun ewm--find-frame-by-focus-id (focus-id)
  "Frame whose own-surface focus id is FOCUS-ID, if any."
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
           (pointer (and (alist-get 'pointer event) x y))
           (ffm (and ewm-focus-follows-mouse pointer)))
      (cond
       ((when-let* ((win (ewm--find-window-by-entry-id focus-id)))
          (select-frame (window-frame win))
          (select-window win)
          (setq ewm--mff-last-window win)
          t))
       ((when-let* ((frame (ewm--find-frame-by-focus-id focus-id)))
          (select-frame frame)
          (when ffm
            (when-let* ((window (ewm--window-at-logical frame x y))
                        ((not (minibufferp (window-buffer window)))))
              (select-window window)))
          ;; Anchor only on pointer focus; keyboard events must leave it stale so MFF warps.
          (when pointer
            (setq ewm--mff-last-window (selected-window)))
          t))))))

(provide 'ewm-focus)
;;; ewm-focus.el ends here
