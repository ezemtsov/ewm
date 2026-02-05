;;; ewm.el --- Emacs Wayland Manager -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; EWM integrates Emacs with a Wayland compositor, providing an EXWM-like
;; experience without the single-threaded limitations.
;;
;; Phase 1: Prove IPC works between Rust compositor and Emacs.

;;; Code:

(defgroup ewm nil
  "Emacs Wayland Manager."
  :group 'environment)

(defvar ewm--process nil
  "Network process for compositor connection.")

(defvar ewm--surfaces (make-hash-table :test 'eql)
  "Hash table mapping surface ID to surface info.")

;;; Protocol

(defun ewm--send (cmd)
  "Send CMD as JSON to compositor."
  (when (and ewm--process (process-live-p ewm--process))
    (let ((json (json-serialize cmd)))
      (process-send-string ewm--process (concat json "\n")))))

(defun ewm--handle-event (event)
  "Handle EVENT from compositor."
  (let ((type (gethash "event" event)))
    (pcase type
      ("new" (ewm--handle-new-surface event))
      ("close" (ewm--handle-close-surface event))
      (_ (message "EWM: unknown event type: %s" type)))))

;;; Event handlers

(defun ewm--handle-new-surface (event)
  "Handle new surface EVENT."
  (let* ((id (gethash "id" event))
         (app (gethash "app" event))
         (buf (generate-new-buffer (format "*ewm:%s:%d*" app id))))
    (puthash id `(:buffer ,buf :app ,app) ewm--surfaces)
    (with-current-buffer buf
      (ewm-surface-mode)
      (setq-local ewm-surface-id id)
      (setq-local ewm-surface-app app))
    (message "EWM: new surface %d (%s)" id app)))

(defun ewm--handle-close-surface (event)
  "Handle close surface EVENT."
  (let* ((id (gethash "id" event))
         (info (gethash id ewm--surfaces)))
    (when info
      (let ((buf (plist-get info :buffer)))
        (when (buffer-live-p buf)
          (kill-buffer buf)))
      (remhash id ewm--surfaces))
    (message "EWM: closed surface %d" id)))

;;; Commands

(defun ewm-layout (id x y w h)
  "Set surface ID position to X Y and size to W H."
  (ewm--send `(:cmd "layout" :id ,id :x ,x :y ,y :w ,w :h ,h)))

(defun ewm-focus (id)
  "Focus surface ID."
  (ewm--send `(:cmd "focus" :id ,id)))

;;; Process handling

(defun ewm--filter (proc string)
  "Process filter for PROC receiving STRING."
  (let ((buf (process-buffer proc)))
    (with-current-buffer buf
      (goto-char (point-max))
      (insert string)
      ;; Process complete lines
      (goto-char (point-min))
      (while (search-forward "\n" nil t)
        (let* ((line (buffer-substring (point-min) (1- (point))))
               (event (condition-case err
                          (json-parse-string line)
                        (error
                         (message "EWM: JSON parse error: %s" err)
                         nil))))
          (delete-region (point-min) (point))
          (when event
            (ewm--handle-event event)))))))

(defun ewm--sentinel (proc event)
  "Process sentinel for PROC with EVENT."
  (message "EWM: connection %s" (string-trim event))
  (when (not (process-live-p proc))
    (setq ewm--process nil)))

;;; Public API

(defun ewm-connect (&optional socket-path)
  "Connect to compositor at SOCKET-PATH (default /tmp/ewm.sock)."
  (interactive)
  (when (and ewm--process (process-live-p ewm--process))
    (delete-process ewm--process))
  (let ((path (or socket-path "/tmp/ewm.sock")))
    (setq ewm--process
          (make-network-process
           :name "ewm"
           :buffer (generate-new-buffer " *ewm-input*")
           :family 'local
           :service path
           :filter #'ewm--filter
           :sentinel #'ewm--sentinel))
    (message "EWM: connected to %s" path)))

(defun ewm-disconnect ()
  "Disconnect from compositor."
  (interactive)
  (when ewm--process
    (delete-process ewm--process)
    (setq ewm--process nil)
    (message "EWM: disconnected")))

;;; Surface mode

(defvar-local ewm-surface-id nil
  "Surface ID for this buffer.")

(defvar-local ewm-surface-app nil
  "Application name for this buffer.")

(define-derived-mode ewm-surface-mode special-mode "EWM Surface"
  "Major mode for EWM surface buffers."
  (setq buffer-read-only t))

(provide 'ewm)
;;; ewm.el ends here
