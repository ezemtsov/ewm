;;; ewm-session.el --- EWM frames and surfaces under desktop-save-mode -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; `desktop-save-mode' saves and restores an EWM session like any other.
;; The frame parameters are the layout: frameset saves the output, home,
;; strip position and float geometry of every frame, refreshed from the
;; compositor at save, and a restored frame is placed from them when its
;; surface arrives. A surface buffer is the launch record of its
;; application for desktop.el and bookmark.el alike. A relaunched
;; application maps into the window of its placeholder, found by the
;; activation token of its spawn, or stays buried as it was.

;;; Code:

(require 'cl-lib)
(require 'desktop)
(require 'bookmark)
(require 'ewm-module)
(ewm-module--declare)
(require 'ewm)

(defvar-local ewm-session--launch-record nil
  "Launch record of a placeholder, or of a surface buffer as of its map.")

(defvar-local ewm-session--process nil
  "Relaunched process a placeholder buffer waits for.")

;;; Frame parameters

(defun ewm-session--filter-placement (current _filtered parameters saving)
  "CURRENT as the compositor has it when SAVING, PARAMETERS name the frame.
For a float, ewm-position and ewm-size are the frame parameters with the
surface origin on its output and the surface size, not the body keys of a
`display-buffer' action."
  (let* ((id (alist-get 'ewm-surface-id parameters))
         (output (and saving id (ewm-get-frame-output id))))
    (if (not output)
        current
      (pcase (car current)
        ('ewm-output (cons 'ewm-output output))
        ('ewm-home (cons 'ewm-home (ewm-get-frame-home id)))
        ('ewm-strip-position
         (cons 'ewm-strip-position (seq-position (ewm-get-strip-frame-ids output) id)))
        ('ewm-position (cons 'ewm-position (ewm--frame-position (ewm--frame-by-surface-id id))))
        ('ewm-size
         (let* ((frame (ewm--frame-by-surface-id id))
                (scale (ewm--frame-scale frame)))
           (cons 'ewm-size (cons (/ (frame-pixel-width frame) scale)
                                 (/ (frame-pixel-height frame) scale)))))))))

(dolist (parameter '(ewm-surface-id ewm-after ewm-focus-on-create auto-hide-function
                     ewm-floating-origin-frame-id display))
  (add-to-list 'frameset-filter-alist (cons parameter :never)))
(dolist (parameter '(ewm-output ewm-home ewm-strip-position ewm-position ewm-size))
  (add-to-list 'frameset-filter-alist (cons parameter #'ewm-session--filter-placement)))

;;; Startup

(defun ewm-session--read-desktop (read &rest args)
  "Call READ with ARGS, the lock checked by pid, not asked about, frames afresh.
A daemon cannot ask about the lock, and its terminal frame can show nothing."
  (let ((desktop-load-locked-desktop (if (eq desktop-load-locked-desktop 'ask)
                                         'check-pid
                                       desktop-load-locked-desktop))
        (desktop-restore-reuses-frames nil))
    (apply read args)))

(defun ewm-session--hold-output (_output)
  "Hold an output empty for the desktop init reads, once a frame is up for it."
  (and desktop-save-mode (not after-init-time)
       (locate-file desktop-base-file-name desktop-path)
       (seq-some (lambda (frame) (frame-parameter frame 'ewm-output)) (frame-list))))

(defun ewm-session--select-frame ()
  "Select a compositor frame, desktop restores none under the terminal frame.
The focus of the startup frame may still be in the event queue at after-init."
  (when-let* ((frame (and desktop-save-mode
                          (seq-find (lambda (frame) (frame-parameter frame 'ewm-output))
                                    (frame-list)))))
    (select-frame frame)))

(defun ewm-session--settle ()
  "Launch the placeholders the desktop made and fill the outputs it left empty."
  (when ewm--module-mode
    (dolist (placeholder (ewm-session--placeholders))
      (unless (buffer-local-value 'ewm-session--process placeholder)
        (let ((window (get-buffer-window placeholder t)))
          (ewm-session--launch placeholder (and window (window-frame window))))))
    (ewm--populate-outputs)))

(advice-add 'desktop-read :around #'ewm-session--read-desktop)
(add-hook 'ewm-output-populate-functions #'ewm-session--hold-output)
(add-hook 'after-init-hook #'ewm-session--select-frame -90)
(add-hook 'desktop-after-read-hook #'ewm-session--settle)
(add-hook 'after-init-hook #'ewm-session--settle 90)

;;; Buffer records

(defun ewm-session--current-launch-record (&optional _dirname)
  "Launch plist of the buffer, the one of its map once the process is gone.
A stop from outside signals the clients and Emacs at once."
  (or (ewm-session--read-launch-record) ewm-session--launch-record))

(defun ewm-session--read-launch-record ()
  "Launch plist of the surface buffer from /proc, nil when its process is gone."
  (when-let* ((pid ewm-surface-pid)
              (cmdline (ignore-errors
                         (with-temp-buffer
                           (let ((coding-system-for-read 'utf-8-unix))
                             (insert-file-contents (format "/proc/%d/cmdline" pid)))
                           (buffer-string))))
              (command (split-string cmdline "\0" t)))
    ;; A client that rewrote its argv shows one argument with spaces.
    (unless (cdr command)
      (setq command (split-string-and-unquote (car command))))
    (list :app ewm-surface-app :command command
          :directory (file-name-as-directory
                      (or (file-symlink-p (format "/proc/%d/cwd" pid)) "~")))))

(defun ewm-session--bookmark-make-record ()
  "Bookmark record relaunching the surface, for `bookmark-make-record'."
  `(,(buffer-name)
    (handler . ewm-bookmark-jump)
    (ewm-launch . ,(or (ewm-session--current-launch-record)
                       (user-error "%s has no process to relaunch" (buffer-name))))))

(defun ewm-session--setup-surface ()
  "Make the surface buffer a desktop and bookmark record of its launch."
  (setq-local ewm-session--launch-record (ewm-session--read-launch-record)
              desktop-save-buffer #'ewm-session--current-launch-record
              bookmark-make-record-function #'ewm-session--bookmark-make-record))

(define-derived-mode ewm-placeholder-mode special-mode "Relaunch"
  "Stands in for an application until its relaunch maps, g runs the command again."
  (setq-local desktop-save-buffer #'ewm-session--current-launch-record
              revert-buffer-function #'ewm-session--relaunch)
  (add-hook 'window-size-change-functions #'ewm-session--draw-placeholder nil t))

(defun ewm-session--make-placeholder (name record)
  "Buffer NAME standing in for the application of launch RECORD."
  (with-current-buffer (generate-new-buffer name)
    (ewm-placeholder-mode)
    (setq ewm-session--launch-record record)
    (current-buffer)))

(defun ewm-session--draw-placeholder (window)
  "Name the application the placeholder waits for, centered in WINDOW."
  (let ((line (format "%s not ready" (plist-get ewm-session--launch-record :app)))
        (inhibit-read-only t))
    (erase-buffer)
    (insert (make-string (max 0 (1- (/ (window-body-height window) 2))) ?\n)
            (propertize " " 'display `(space :align-to (- center ,(/ (length line) 2.0))))
            line)))

(defun ewm-session--restore-desktop-buffer (_file name misc)
  "Buffer NAME, else a placeholder holding the launch MISC, a desktop handler."
  (or (get-buffer name) (and misc (ewm-session--make-placeholder name misc))))

(dolist (mode '(ewm-surface-mode ewm-placeholder-mode))
  (add-to-list 'desktop-buffer-mode-handlers (cons mode #'ewm-session--restore-desktop-buffer)))
(add-hook 'ewm-surface-mode-hook #'ewm-session--setup-surface)
(add-hook 'ewm-place-surface-functions #'ewm-session--place)

;;; Relaunch

(defun ewm-session--placeholders ()
  "Placeholder buffers whose application has not mapped yet."
  (match-buffers '(major-mode . ewm-placeholder-mode)))

(defun ewm-session--running-process (command)
  "Live process a placeholder already started for COMMAND."
  (seq-some (lambda (buffer)
              (with-current-buffer buffer
                (and (process-live-p ewm-session--process)
                     (equal (plist-get ewm-session--launch-record :command) command)
                     ewm-session--process)))
            (ewm-session--placeholders)))

(defun ewm-session--launch (placeholder &optional frame again)
  "Start the application of PLACEHOLDER to land in FRAME, AGAIN beside a live one."
  (with-current-buffer placeholder
    (when-let* ((record ewm-session--launch-record)
                (app (plist-get record :app)))
      ;; One process per command, its other windows find it by app.
      (setq ewm-session--process
            (or (and (not again) (ewm-session--running-process (plist-get record :command)))
                (with-demoted-errors "ewm-session: relaunch failed: %S"
                  (let* ((directory (plist-get record :directory))
                         (default-directory (if (file-directory-p directory) directory "~/"))
                         (ewm-launch-hint
                          (list :frame-surface-id
                                (and frame (frame-parameter frame 'ewm-surface-id))
                                :quiet t)))
                    (make-process :name app :command (plist-get record :command) :noquery t))))))))

(defun ewm-session--relaunch (&rest _)
  "Run the command of this placeholder once more, the g of the buffer."
  (ewm-session--launch (current-buffer) (selected-frame) t))

(defun ewm-session--token (placeholder)
  "Activation token the relaunch of PLACEHOLDER was given, or nil."
  (when-let* ((process (buffer-local-value 'ewm-session--process placeholder)))
    (process-get process 'ewm-launch-token)))

(defun ewm-session--placeholder-for (buffer)
  "Placeholder for the surface BUFFER by token, pid, then app, shown first."
  (when-let* ((placeholders (seq-sort-by (lambda (placeholder)
                                           (if (get-buffer-window placeholder t) 0 1))
                                         #'< (ewm-session--placeholders))))
    (let ((token (buffer-local-value 'ewm-surface-token buffer))
          (app (buffer-local-value 'ewm-surface-app buffer)))
      (or (and token (cl-find token placeholders :key #'ewm-session--token :test #'equal))
          (let ((pids (ewm--cwd-ancestors (buffer-local-value 'ewm-surface-pid buffer))))
            (seq-find (lambda (placeholder)
                        (when-let* ((process (buffer-local-value 'ewm-session--process
                                                                 placeholder)))
                          (memq (process-id process) pids)))
                      placeholders))
          (and app (cl-find app placeholders :test #'equal
                            :key (lambda (placeholder)
                                   (plist-get (buffer-local-value 'ewm-session--launch-record placeholder)
                                              :app))))))))

(defun ewm-session--place (buffer)
  "Show BUFFER in the windows of its placeholder, non-nil when one matched."
  (when-let* ((placeholder (ewm-session--placeholder-for buffer)))
    (let ((windows (get-buffer-window-list placeholder nil t)))
      (dolist (window windows)
        (set-window-dedicated-p window nil)
        (set-window-buffer window buffer))
      (kill-buffer placeholder)
      (when windows (ewm-layout--refresh))
      t)))

(defun ewm-bookmark-jump (bookmark)
  "Relaunch the application of BOOKMARK into a placeholder, its handler."
  (let ((placeholder (ewm-session--make-placeholder (bookmark-name-from-full-record bookmark)
                                               (bookmark-prop-get bookmark 'ewm-launch))))
    (ewm-session--launch placeholder (selected-frame))
    (set-buffer placeholder)))

(put 'ewm-bookmark-jump 'bookmark-handler-type "EWM")

(provide 'ewm-session)
;;; ewm-session.el ends here
