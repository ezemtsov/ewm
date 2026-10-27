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

(declare-function ewm--send-config "ewm")

(defcustom ewm-focus-follows-mouse nil
  "Whether moving the mouse pointer should focus applications.
The compositor sends focus events with cursor coordinates; intra-frame
window switching is handled by `ewm--handle-focus' from those events."
  :type 'boolean
  :group 'ewm-input-devices
  :set (ewm--send-on-set #'ewm--send-config))

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

;; Held back like a frame switch until the key sequence in progress ends.
(put 'ewm-focus-change 'event-kind 'switch-frame)
(add-to-list 'selection-inhibit-update-commands 'ewm--focus-change)

(defvar ewm--focus-change-last-command nil
  "`last-command' before the latest `ewm--focus-change'.")

(defun ewm--focus-change ()
  "Do nothing, so a compositor focus change ends in a command boundary."
  (interactive)
  (setq ewm--focus-change-last-command last-command)
  (add-hook 'pre-command-hook #'ewm--restore-last-command)
  (setq prefix-arg current-prefix-arg))

(defun ewm--restore-last-command ()
  "Give the next command the `last-command' from before the focus change."
  (remove-hook 'pre-command-hook #'ewm--restore-last-command)
  (setq last-command ewm--focus-change-last-command))

(defun ewm--unread-focus-change ()
  "Queue an `ewm-focus-change' event behind the keys already unread."
  (remove-hook 'post-command-hook #'ewm--unread-focus-change)
  (setq unread-command-events
        (append unread-command-events (list (list 'ewm-focus-change (selected-frame))))))

(defun ewm--queue-focus-change ()
  "Make the selection change a command boundary, as a frame switch is."
  ;; A macro would record the event, `read-event' would return it.
  (cond (defining-kbd-macro)
        (this-command (add-hook 'post-command-hook #'ewm--unread-focus-change 90))
        (t (ewm--unread-focus-change))))

(defun ewm--handle-focus (event)
  "Mirror compositor focus EVENT into Emacs selection."
  (unless (ewm-layout--menu-active-p)
    (let* ((focus-id (alist-get 'focus_id event))
           (x (alist-get 'x event))
           (y (alist-get 'y event))
           (pointer (and (alist-get 'pointer event) x y)))
      (if-let* ((win (ewm--find-window-by-entry-id focus-id)))
          (progn
            (select-frame (window-frame win))
            (select-window win)
            (setq ewm--mff-last-window win))
        (when-let* ((frame (ewm--find-frame-by-focus-id focus-id)))
          (select-frame frame)
          (when (and ewm-focus-follows-mouse pointer)
            (when-let* ((window (ewm--window-at-logical frame x y))
                        ((not (minibufferp (window-buffer window)))))
              (select-window window)))
          ;; Anchor only on pointer focus; keyboard events must leave it stale so MFF warps.
          (when pointer
            (setq ewm--mff-last-window (selected-window))))))))

(provide 'ewm-focus)
;;; ewm-focus.el ends here
