;;; ewm-layout.el --- Layout management for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Layout management.  Also owns the modal focus contract --
;; `:selected-entry-id' is suppressed while Emacs is reading keys for
;; itself; see `ewm-layout-menu-predicates' and `docs/focus-design.md'.

;;; Code:

(require 'ewm-module)
(ewm-module--declare)
(require 'ewm-surface)

(defun ewm-layout--reading-key-p ()
  "Non-nil while an overriding keymap is consuming keystrokes."
  (or overriding-terminal-local-map
      overriding-local-map))

(defun ewm-layout--minibuffer-menu-p ()
  "Non-nil while a completion minibuffer is active.
A plain prompt stays unpinned so its value can come from another window."
  (and (active-minibuffer-window) minibuffer-completion-table))

(defun ewm-layout--command-popup-p ()
  "Non-nil while a command shows a popup menu."
  (and this-command (menu-or-popup-active-p)))

(defun ewm-layout--click-popup-p ()
  "Non-nil while a click shows a popup menu, read with no command to end it."
  (and (null this-command) (menu-or-popup-active-p)))

(defvar ewm-layout-menu-predicates
  '(ewm-layout--minibuffer-menu-p
    ewm-layout--reading-key-p
    ewm-layout--command-popup-p)
  "Predicates that signal Emacs is reading keys for itself.
If any returns non-nil, the next layout's `:selected-entry-id' is
suppressed for every frame.")

(defvar ewm-layout--last-menu-active nil
  "Cached previous `ewm-layout--menu-active-p' for edge detection.")

(defun ewm-layout--menu-active-p ()
  "Non-nil if any `ewm-layout-menu-predicates' entry says so."
  (and (run-hook-with-args-until-success 'ewm-layout-menu-predicates)
       ;; Latched, a command's popup is gone by `post-command-hook'.
       (setq ewm-layout--last-menu-active t)))

(defconst ewm-layout--frame-focus-id-marker (ash 1 32)
  "Bit 32 marks the focus id of a frame's own surface.
Entry ids are random in [1, 2^32-1] with bit 32 unset, a frame's focus id is
`(logior MARKER SURFACE-ID)'. Disjoint ranges let the compositor and Lisp
derive the same id from the surface id without coordination.")

(defun ewm-layout--fresh-entry-id ()
  "Generate a non-zero random entry id with bit 32 unset."
  (1+ (random (1- (ash 1 32)))))

(defun ewm-layout--ensure-entry-id (window)
  "Return a stable entry id for WINDOW."
  (or (window-parameter window 'ewm-entry-id)
      (set-window-parameter window 'ewm-entry-id (ewm-layout--fresh-entry-id))))

(defun ewm--find-window-by-entry-id (entry-id)
  "Window whose `ewm-entry-id' parameter is ENTRY-ID, if any."
  (get-window-with-predicate
   (lambda (window) (eql (window-parameter window 'ewm-entry-id) entry-id))
   'no-minibuf t))

(defun ewm-layout--ensure-frame-focus-id (frame)
  "FRAME's focus id, derived from its surface id as the compositor does."
  (when-let* ((sid (frame-parameter frame 'ewm-surface-id)))
    (logior ewm-layout--frame-focus-id-marker sid)))

(defun ewm--window-body-pixel-rect (window)
  "Return WINDOW's text area as frame-relative (X Y WIDTH HEIGHT) pixels."
  (pcase-let ((`(,left ,top ,right ,bottom) (window-inside-pixel-edges window)))
    (list left
          ;; Before Emacs 31, body edges omitted the tab-line offset.
          (+ top (if (< emacs-major-version 31) (window-tab-line-height window) 0))
          (- right left)
          (- bottom top))))

(defun ewm-layout--filler-p (frame)
  "Non-nil while FRAME is EWM's filler nobody used.
A used one becomes the user's for good."
  (when (eq (frame-parameter frame 'ewm-owner) 'compositor)
    (let ((window (frame-root-window frame)))
      ;; Splitting makes the root window internal.
      (when (or (not (window-live-p window)) (window-prev-buffers window))
        (set-frame-parameter frame 'ewm-owner 'user)))
    (eq (frame-parameter frame 'ewm-owner) 'compositor)))

(defun ewm-layout--build-frame-plist (frame menu-active)
  "Plist describing FRAME for `ewm-layout'.
When MENU-ACTIVE is non-nil, `:selected-entry-id' is nil so the
compositor parks keyboard focus on the frame itself."
  (let (entries)
    (dolist (window (window-list frame 'no-minibuf))
      (pcase-let ((`(,x ,y ,w ,h) (ewm--window-body-pixel-rect window))
                  (surface-id (ewm-surface-buffer-p (window-buffer window))))
        (push `(,@(when surface-id `(:surface-id ,surface-id))
                :entry-id ,(ewm-layout--ensure-entry-id window)
                :name ,(buffer-name (window-buffer window))
                :x ,x :y ,y :w ,w :h ,h)
              entries)))
    (let ((selected-entry-id
           (unless menu-active
             (when-let* ((sel-win (frame-selected-window frame)))
               (unless (window-minibuffer-p sel-win)
                 (ewm-layout--ensure-entry-id sel-win))))))
      `(:name ,(or (frame-parameter frame 'name) "")
        :surface-id ,(frame-parameter frame 'ewm-surface-id)
        ,@(when (ewm-layout--filler-p frame) '(:owner compositor))
        :focus-id ,(ewm-layout--ensure-frame-focus-id frame)
        :selected-entry-id ,selected-entry-id
        :width ,(float (frame-pixel-width frame))
        :height ,(float (frame-pixel-height frame))
        :entries ,(vconcat (nreverse entries))))))

(defun ewm-layout (frames)
  "Declare the contents of every Emacs frame. The compositor owns placement.
FRAMES is a vector of `ewm-layout--build-frame-plist' plists."
  (ewm-layout-module frames))

(defun ewm-layout--send-layouts ()
  "Declare every placed frame, suppressing `:selected-entry-id' while modal."
  (let ((menu-active (ewm-layout--menu-active-p)))
    (ewm-layout (vconcat (seq-keep (lambda (frame)
                                     (and (frame-parameter frame 'ewm-surface-id)
                                          (ewm-layout--build-frame-plist frame menu-active)))
                                   (frame-list))))))

(defun ewm-layout--refresh ()
  "Send the current window layout to the compositor."
  (when ewm--module-mode
    ;; An error here would drop us from `window-state-change-hook' for good.
    (condition-case err
        (ewm-layout--send-layouts)
      (error (message "EWM: layout sync error: %s" (error-message-string err))))))

(defvar ewm-layout-menu-deactivated-hook nil
  "Edge-triggered when a menu/minibuffer stops being active.")

(defun ewm-layout--on-post-command ()
  "Refresh layout on a modal-state transition."
  (let* ((was ewm-layout--last-menu-active)
         (now (ewm-layout--menu-active-p)))
    (unless (eq now was)
      (setq ewm-layout--last-menu-active now)
      (unless now (run-hooks 'ewm-layout-menu-deactivated-hook))
      (ewm-layout--refresh))))

(defun ewm--enable-layout-sync ()
  "Enable automatic layout sync."
  (setq ewm-layout--last-menu-active (ewm-layout--menu-active-p))
  (add-hook 'window-state-change-hook #'ewm-layout--refresh)
  (add-hook 'post-command-hook #'ewm-layout--on-post-command))

(defun ewm--disable-layout-sync ()
  "Disable automatic layout sync."
  (remove-hook 'window-state-change-hook #'ewm-layout--refresh)
  (remove-hook 'post-command-hook #'ewm-layout--on-post-command))

(provide 'ewm-layout)
;;; ewm-layout.el ends here
