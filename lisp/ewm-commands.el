;;; ewm-commands.el --- Interactive commands for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; The commands `ewm-mode-map' binds and the ones meant for M-x and
;; emacsclient. Those defined with `ewm--define-action' run in the
;; compositor straight from the key.

;;; Code:

(require 'json)
(require 'windmove)
(require 'ewm-module)
(ewm-module--declare)
(require 'ewm-surface)
(require 'ewm-frame)

(defmacro ewm--define-action (name action docstring)
  "Define command NAME as compositor ACTION, documented by DOCSTRING."
  (declare (indent defun) (doc-string 3))
  `(progn
     (defun ,name ()
       ,docstring
       (interactive)
       (ewm-action-module ',action))
     (function-put ',name 'ewm-action ',action)))

;;; Frames and outputs

(defun ewm-frame-new ()
  "Open a new Emacs frame after the active one on the same output."
  (interactive)
  (let* ((current (ewm--focused-frame))
         (output (ewm--frame-output current)))
    (unless output (user-error "Current frame is on no output"))
    (ewm--create-frame-for-output output current
                                  '((ewm-focus-on-create . t)))))

(defun ewm-frame-close ()
  "Close the focused frame; the compositor refocuses a same-output neighbour."
  (interactive)
  (let ((frame (ewm--focused-frame)))
    ;; Select it first so the delete-frame advice notifies the compositor.
    (unless (eq frame (selected-frame))
      (select-frame frame))
    (delete-frame frame)))

(ewm--define-action ewm-frame-left (focus-frame :delta -1)
  "Slide to the previous frame on the current output's strip.")

(ewm--define-action ewm-frame-right (focus-frame :delta 1)
  "Slide to the next frame on the current output's strip.")

(defun ewm--frame-select-action (event)
  "The `focus-nth-frame' action for the digit key EVENT."
  (list 'focus-nth-frame :n (- (event-basic-type event) ?0)))

(defun ewm-frame-select ()
  "Select the Nth frame (1-based) on the current output, N the digit pressed.
No-op if N is past the number of frames on this output."
  (interactive)
  (ewm-action-module (ewm--frame-select-action last-command-event)))

(function-put 'ewm-frame-select 'ewm-action #'ewm--frame-select-action)

(ewm--define-action ewm-frame-move-left (move-frame :delta -1)
  "Move the selected frame one slot left in the strip.")

(ewm--define-action ewm-frame-move-right (move-frame :delta 1)
  "Move the selected frame one slot right in the strip.")

(ewm--define-action ewm-frame-move-to-next-output move-frame-to-next-output
  "Move the selected frame to the next output in order, wrapping around.")

(ewm--define-action ewm-output-focus-left (focus-output :dx -1 :dy 0)
  "Focus the active frame on the output to the left of the current one.")

(ewm--define-action ewm-output-focus-right (focus-output :dx 1 :dy 0)
  "Focus the active frame on the output to the right of the current one.")

(ewm--define-action ewm-output-focus-up (focus-output :dx 0 :dy -1)
  "Focus the active frame on the output above the current one.")

(ewm--define-action ewm-output-focus-down (focus-output :dx 0 :dy 1)
  "Focus the active frame on the output below the current one.")

(defun ewm-focus-left ()
  "Focus the visible window to the left; cross output at the frame edge."
  (interactive)
  (unless (ignore-errors (windmove-left))
    (ewm-focus-output-direction-module -1 0)))

(defun ewm-focus-right ()
  "Focus the visible window to the right; cross output at the frame edge."
  (interactive)
  (unless (ignore-errors (windmove-right))
    (ewm-focus-output-direction-module 1 0)))

(defun ewm-focus-up ()
  "Focus the visible window above; cross output at the frame edge."
  (interactive)
  (unless (ignore-errors (windmove-up))
    (ewm-focus-output-direction-module 0 -1)))

(defun ewm-focus-down ()
  "Focus the visible window below; cross output at the frame edge."
  (interactive)
  (unless (ignore-errors (windmove-down))
    (ewm-focus-output-direction-module 0 1)))

(defun ewm-floating-toggle ()
  "Toggle the selected buffer between tiled and floating frame placement."
  (interactive)
  (let ((frame (ewm--focused-frame)))
    (if (frame-parameter frame 'ewm-floating)
        (ewm--floating-attach-selected-buffer frame)
      (ewm--floating-detach-selected-buffer frame))))

(ewm--define-action ewm-toggle-fullscreen toggle-fullscreen
  "Toggle fullscreen for the focused view.")

(ewm--define-action ewm-overview-toggle (overview toggle)
  "Toggle the overview: zoom every output out to show its whole strip.
Click a frame to select it and zoom back in.")

(ewm--define-action ewm-overview-open (overview open)
  "Open the overview.")

(ewm--define-action ewm-overview-close (overview close)
  "Close the overview.")

(ewm--define-action ewm-hide-toggle hide-toggle
  "Slide every frame off its output to show what is behind, or bring them back.
Clicking the emptied output also brings them back.")

(defun ewm-workspace-rename (name &optional frame)
  "Rename FRAME's workspace to NAME.
When FRAME is nil, use the selected frame.  An empty NAME clears
the workspace name."
  (interactive (list (read-string "Workspace name: ")))
  (ewm-workspace-rename-module name (frame-parameter frame 'ewm-surface-id)))

(defun ewm-workspace-mark-urgent (&optional frame)
  "Mark FRAME's workspace as urgent.
When FRAME is nil, use the selected frame.  The compositor ignores
the request when the workspace is already focused."
  (interactive)
  (ewm-workspace-mark-urgent-module
   (or (frame-parameter frame 'ewm-surface-id)
       (user-error "Frame has no EWM surface id"))))

;;; Surface buffers

(defun ewm--cycle-surface-buffer (command)
  "Run buffer-navigation COMMAND with a surface-only filter."
  (let* ((skip switch-to-prev-buffer-skip)
         (switch-to-prev-buffer-skip
          (lambda (window buffer bury-or-kill)
            (or (not (ewm-surface-buffer-p buffer))
                (switch-to-prev-buffer-skip-p skip window buffer bury-or-kill)))))
    (funcall command)))

(defun ewm-next-surface-buffer ()
  "Like `next-buffer', but only among surface buffers."
  (interactive)
  (ewm--cycle-surface-buffer #'next-buffer))

(defun ewm-prev-surface-buffer ()
  "Like `previous-buffer', but only among surface buffers."
  (interactive)
  (ewm--cycle-surface-buffer #'previous-buffer))

(defun ewm-get-window-info (&optional id)
  "Return window info alist (id app title pid) for surface ID.
When ID is nil, use the current buffer's surface or compositor focus.
Returns nil when no surface is found."
  (when-let* ((surface-id (or id ewm-surface-id (ewm-get-focused-id)))
              (buf (ewm--surface-buffer surface-id)))
    (list (cons 'id surface-id)
          (cons 'app (buffer-local-value 'ewm-surface-app buf))
          (cons 'title (buffer-local-value 'ewm-surface-title buf))
          (cons 'pid (buffer-local-value 'ewm-surface-pid buf)))))

(defun ewm-get-window-info-json (&optional id)
  "Return `ewm-get-window-info' for ID as JSON, for emacsclient.
Without a surface, describe the selected Emacs frame instead."
  (json-encode (or (ewm-get-window-info id)
                   `((id . 0)
                     (app . "emacs")
                     (title . ,(frame-parameter nil 'name))
                     (pid . ,(emacs-pid))))))

;;; Session

(ewm--define-action ewm-lock-session (spawn :command ("loginctl" "lock-session"))
  "Lock the session via loginctl.")

(ewm--define-action ewm-volume-raise
  (spawn :command ("wpctl" "set-volume" "-l" "1.0" "@DEFAULT_AUDIO_SINK@" "5%+"))
  "Raise the default sink's volume by 5%.")

(ewm--define-action ewm-volume-lower
  (spawn :command ("wpctl" "set-volume" "@DEFAULT_AUDIO_SINK@" "5%-"))
  "Lower the default sink's volume by 5%.")

(ewm--define-action ewm-volume-mute
  (spawn :command ("wpctl" "set-mute" "@DEFAULT_AUDIO_SINK@" "toggle"))
  "Toggle the default sink's mute.")

(ewm--define-action ewm-mic-mute
  (spawn :command ("wpctl" "set-mute" "@DEFAULT_AUDIO_SOURCE@" "toggle"))
  "Toggle the default source's mute.")

(ewm--define-action ewm-brightness-up (spawn :command ("brightnessctl" "set" "5%+"))
  "Raise the screen brightness by 5%.")

(ewm--define-action ewm-brightness-down (spawn :command ("brightnessctl" "set" "5%-"))
  "Lower the screen brightness by 5%.")

(defun ewm-power-off-monitors ()
  "Power off all monitors until a key press, pointer motion or session unlock."
  (interactive)
  (ewm-power-off-monitors-module))

(defun ewm-power-on-monitors ()
  "Power all monitors back on."
  (interactive)
  (ewm-power-on-monitors-module))

(defun ewm-launch-xdg-command (name)
  "Launch XDG app NAME, stripping desktop field codes from its Exec line."
  (let* ((cmd (cdr (assoc name (ewm-list-xdg-apps))))
         (cmd (replace-regexp-in-string "%[uUfFdDnNickvm]" "" cmd))
         (cmd (string-trim cmd)))
    (start-process-shell-command name nil cmd)))

(defun ewm-launch-app ()
  "Launch an XDG desktop application via `completing-read'."
  (interactive)
  (ewm-launch-xdg-command
   (completing-read "Launch app: " (ewm-list-xdg-apps) nil t)))

;;; Diagnostics

(defun ewm-show-state ()
  "Display the compositor state dump in *ewm-state*."
  (interactive)
  (let ((state (or (ewm-get-debug-state-module)
                   (user-error "Timed out waiting for compositor state dump"))))
    (with-current-buffer-window "*ewm-state*" nil nil
      (emacs-lisp-mode)
      (pp state (current-buffer)))))

(defun ewm-list-outputs ()
  "Describe each connected output in *ewm-outputs*, with a config entry."
  (interactive)
  (if (null ewm--output-info)
      (message "No output info available (is the compositor running?)")
    (with-current-buffer-window "*ewm-outputs*" nil nil
      (dolist (entry ewm--output-info)
        (let* ((info        (cdr entry))
               (name        (alist-get 'name      info))
               (mms         (ewm--output-mms info))
               (config-key  (ewm--output-config-key info))
               (x           (or (alist-get 'x         info) 0))
               (y           (or (alist-get 'y         info) 0))
               (scale       (or (alist-get 'scale     info) 1.0))
               (transform   (or (alist-get 'transform info) 0))
               (width-mm    (or (alist-get 'width_mm  info) 0))
               (height-mm   (or (alist-get 'height_mm info) 0))
               (modes       (alist-get 'modes info))
               (cur (seq-find (lambda (m) (alist-get 'current m)) modes))
               (transform-str
                (pcase transform
                  (0 "Normal")   (1 "90°")          (2 "180°")
                  (3 "270°")     (4 "Flipped")       (5 "Flipped+90°")
                  (6 "Flipped+180°") (7 "Flipped+270°")
                  (_ (number-to-string transform)))))
          (insert (format "Output \"%s\" (%s)\n" mms name))
          (if cur
              (insert (format "  Current mode: %dx%d @ %.3f Hz%s\n"
                              (alist-get 'width   cur)
                              (alist-get 'height  cur)
                              (/ (float (alist-get 'refresh cur)) 1000.0)
                              (if (alist-get 'preferred cur) " (preferred)" "")))
            (insert "  Disabled / no current mode\n"))
          (insert (format "  Scale: %g\n" scale))
          (insert (format "  Transform: %s\n" transform-str))
          (insert (format "  Position: %d, %d\n" x y))
          (when (or (> width-mm 0) (> height-mm 0))
            (insert (format "  Physical size: %dx%d mm\n" width-mm height-mm)))
          (when modes
            (insert "  Available modes:\n")
            (seq-do
             (lambda (m)
               (insert
                (format "    %dx%d @ %.3f Hz%s\n"
                        (alist-get 'width  m)
                        (alist-get 'height m)
                        (/ (float (alist-get 'refresh m)) 1000.0)
                        (concat
                         (if (alist-get 'current   m) " (current)"   "")
                         (if (alist-get 'preferred m) " (preferred)" "")))))
             modes))
          (when cur
            (insert "\n  Suggested ewm-output-config entry:\n")
            (insert (format "    (\"%s\" :width %d :height %d :scale %g)\n"
                            config-key
                            (alist-get 'width  cur)
                            (alist-get 'height cur)
                            scale)))
          (insert "\n")))
      (special-mode))))

(defun ewm-debug-mode (&optional enable)
  "Toggle compositor debug logging, on with a positive prefix ENABLE.
Follow the output with `journalctl --user -t ewm -f'."
  (interactive "P")
  (let ((on (ewm-debug-mode-module (and enable (> (prefix-numeric-value enable) 0)))))
    (setq ewm--debug on)
    (message "EWM debug mode: %s" (if on "ENABLED" "disabled"))))

(provide 'ewm-commands)
;;; ewm-commands.el ends here
