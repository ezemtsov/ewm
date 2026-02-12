;;; ewm-input.el --- Input handling for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Input handling for EWM including key interception, mouse-follows-focus,
;; and keyboard layout configuration.
;;
;; EWM automatically intercepts keys based on two settings:
;;
;; 1. `ewm-intercept-prefixes' - Keys that start command sequences (C-x, M-x)
;; 2. `ewm-intercept-modifiers' - Modifiers whose bindings are scanned from
;;    Emacs keymaps (default: super)
;;
;; Unlike EXWM, you don't need a separate `exwm-input-global-keys'.
;; Just use normal `global-set-key' or use-package :bind, and EWM
;; will automatically intercept keys with the configured modifiers.
;;
;; Example:
;;   (global-set-key (kbd "s-d") 'consult-buffer)
;;   (global-set-key (kbd "s-<left>") 'windmove-left)

;;; Code:

(require 'cl-lib)

(declare-function ewm-intercept-keys-module "ewm-core")
(declare-function ewm-configure-xkb-module "ewm-core")
(declare-function ewm-get-pointer-location "ewm-core")
(declare-function ewm-in-prefix-sequence-p "ewm-core")
(declare-function ewm-clear-prefix-sequence "ewm-core")
(declare-function ewm-warp-pointer "ewm")
(declare-function ewm-focus "ewm")
(declare-function ewm--get-output-offset "ewm")
(declare-function ewm-layout--refresh "ewm-layout")

(defvar ewm-surface-id)
(defvar ewm--module-mode)
(defvar ewm-mouse-follows-focus)
(defvar ewm--mff-last-window)

(defgroup ewm-input nil
  "EWM input handling."
  :group 'ewm)

;;; Keyboard layout configuration

(defcustom ewm-xkb-layouts '("us")
  "List of XKB layout names to configure in the compositor.
These layouts will be available for switching via `set-input-method'.
Example: \\='(\"us\" \"ru\" \"no\")"
  :type '(repeat string)
  :group 'ewm-input)

(defcustom ewm-xkb-options nil
  "XKB options string for the compositor.
Example: \"ctrl:nocaps,grp:alt_shift_toggle\""
  :type '(choice (const nil) string)
  :group 'ewm-input)

(defvar-local ewm-input--mode 'line-mode
  "Current input mode: `line-mode' or `char-mode'.")

;;; Key interception

(defcustom ewm-intercept-prefixes
  '(?\C-x ?\C-u ?\C-h ?\M-x)
  "Prefix keys that always go to Emacs.
These are keys that start command sequences.
Can be character literals (e.g., ?\\C-x) or strings (e.g., \"C-x\").

Default includes only essential prefixes. Add more as needed:
  (add-to-list \\='ewm-intercept-prefixes ?\\M-`)   ; tmm-menubar
  (add-to-list \\='ewm-intercept-prefixes ?\\M-&)  ; async-shell-command
  (add-to-list \\='ewm-intercept-prefixes ?\\M-:)  ; eval-expression"
  :type '(repeat (choice character string))
  :group 'ewm-input)

(defcustom ewm-intercept-modifiers
  '(super)
  "Modifiers whose key bindings are auto-detected from Emacs keymaps.
EWM scans `global-map' for keys with these modifiers and intercepts them.
This means you can use normal `global-set-key' for bindings like s-d, s-left.

Valid values: control, meta, super, hyper, shift, alt.
Default is (super) to intercept all Super-key bindings."
  :type '(repeat symbol)
  :group 'ewm-input)

;;; Input mode

(defun ewm-input--update-mode (mode)
  "Update input mode to MODE for current buffer.
MODE is either `line-mode' or `char-mode'.

In EXWM, line-mode still gives the X window keyboard focus but intercepts
keys via XGrabKey.  In EWM/Wayland, we achieve similar behavior by:
- Line-mode: surface has focus, compositor intercepts prefix keys
- Char-mode: surface has focus, no interception (same as line-mode for now)

Both modes keep focus on the surface so typing works immediately."
  (when ewm-surface-id
    (setq ewm-input--mode mode)
    (ewm-focus ewm-surface-id)
    (force-mode-line-update)))

(defun ewm-input-char-mode ()
  "Switch to char-mode: keys go directly to surface.
Press a prefix key to return to line-mode."
  (interactive)
  (ewm-input--update-mode 'char-mode))

(defun ewm-input-line-mode ()
  "Switch to line-mode: keys go to Emacs."
  (interactive)
  (ewm-input--update-mode 'line-mode))

(defun ewm-input-toggle-mode ()
  "Toggle between line-mode and char-mode."
  (interactive)
  (ewm-input--update-mode
   (if (eq ewm-input--mode 'char-mode) 'line-mode 'char-mode)))

;;; Mouse-follows-focus

(defun ewm-input--pointer-in-window-p (window)
  "Return non-nil if pointer is inside WINDOW.
Coordinates are in compositor space."
  (let* ((frame (window-frame window))
         (output (frame-parameter frame 'ewm-output))
         (output-offset (ewm--get-output-offset output))
         (edges (window-inside-pixel-edges window))
         (left (+ (car output-offset) (nth 0 edges)))
         (top (+ (cdr output-offset) (nth 1 edges)))
         (right (+ (car output-offset) (nth 2 edges)))
         (bottom (+ (cdr output-offset) (nth 3 edges)))
         (pointer (ewm-get-pointer-location))
         (px (car pointer))
         (py (cdr pointer)))
    (and (<= left px right)
         (<= top py bottom))))

(defun ewm-input--warp-pointer-to-window (window)
  "Warp pointer to center of WINDOW.
Does nothing if pointer is already inside the window or if it's a minibuffer."
  (unless (or (minibufferp (window-buffer window))
              (ewm-input--pointer-in-window-p window))
    (let* ((frame (window-frame window))
           (output (frame-parameter frame 'ewm-output))
           (output-offset (ewm--get-output-offset output))
           (edges (window-inside-pixel-edges window))
           (x (+ (car output-offset) (/ (+ (nth 0 edges) (nth 2 edges)) 2)))
           (y (+ (cdr output-offset) (/ (+ (nth 1 edges) (nth 3 edges)) 2))))
      (ewm-warp-pointer (float x) (float y)))))

(defun ewm-input--mouse-triggered-p ()
  "Return non-nil if current focus change was triggered by mouse."
  (or (mouse-event-p last-input-event)
      (eq this-command 'handle-select-window)))

(defun ewm-input--on-select-window (window &optional norecord)
  "Advice for `select-window' to implement mouse-follows-focus."
  (when (and ewm-mouse-follows-focus
             (not norecord)
             (not (eq window ewm--mff-last-window))
             (not (ewm-input--mouse-triggered-p)))
    (setq ewm--mff-last-window window)
    (ewm-input--warp-pointer-to-window window)))

(defun ewm-input--on-select-frame (frame &optional _norecord)
  "Advice for `select-frame-set-input-focus' to implement mouse-follows-focus."
  (when (and ewm-mouse-follows-focus
             (not (ewm-input--mouse-triggered-p)))
    (let ((window (frame-selected-window frame)))
      (unless (eq window ewm--mff-last-window)
        (setq ewm--mff-last-window window)
        (ewm-input--warp-pointer-to-window window)))))

;;; Focus sync (debounced)
;;
;; Focus is synced after commands complete, with a short debounce delay.
;; This lets Emacs "settle" before syncing, naturally handling:
;; - Popup windows (transient, which-key, etc.)
;; - Prefix key sequences
;; - Rapid command sequences
;; This is the same approach EXWM uses.

(defconst ewm-input--focus-delay 0.01
  "Delay in seconds before syncing focus.
Short enough to be imperceptible, long enough for Emacs to settle.")

(defvar ewm-input--focus-timer nil
  "Timer for debounced focus sync.")

(defun ewm-input--sync-focus ()
  "Actually sync focus after debounce delay."
  (setq ewm-input--focus-timer nil)
  ;; Always clear the prefix sequence flag - the debounced timer means
  ;; the user's command completed (even if we're now in minibuffer etc.)
  (ewm-clear-prefix-sequence)
  ;; Check other conditions (NOT prefix sequence - we just cleared it)
  (unless (or (active-minibuffer-window)
              (> (minibuffer-depth) 0)
              prefix-arg
              (and overriding-terminal-local-map
                   (keymapp overriding-terminal-local-map)))
    (ewm-layout--refresh)))

(defun ewm-input--on-post-command ()
  "Schedule debounced focus sync after command completes."
  (when ewm-input--focus-timer
    (cancel-timer ewm-input--focus-timer))
  ;; Schedule debounced sync - let sync-focus decide when to clear the flag
  (setq ewm-input--focus-timer
        (run-with-timer ewm-input--focus-delay nil
                        #'ewm-input--sync-focus)))

(defun ewm-input--enable ()
  "Enable EWM input handling."
  (setq ewm--mff-last-window (selected-window))
  (add-hook 'post-command-hook #'ewm-input--on-post-command)
  ;; Mouse-follows-focus hooks
  (advice-add 'select-window :after #'ewm-input--on-select-window)
  (advice-add 'select-frame-set-input-focus :after #'ewm-input--on-select-frame))

(defun ewm-input--disable ()
  "Disable EWM input handling."
  (setq ewm--mff-last-window nil)
  (when ewm-input--focus-timer
    (cancel-timer ewm-input--focus-timer)
    (setq ewm-input--focus-timer nil))
  (remove-hook 'post-command-hook #'ewm-input--on-post-command)
  (advice-remove 'select-window #'ewm-input--on-select-window)
  (advice-remove 'select-frame-set-input-focus #'ewm-input--on-select-frame))

;;; Key scanning and interception

(defun ewm--event-to-intercept-spec (event)
  "Convert EVENT to an intercept specification for the compositor.
Returns a plist with :key, modifier flags, and :is-prefix."
  (let* ((mods (event-modifiers event))
         (base (event-basic-type event))
         ;; base is either an integer (ASCII) or a symbol (special key)
         (key-value (cond
                     ((integerp base) base)
                     ((symbolp base) (symbol-name base))
                     (t nil)))
         ;; Check if this key is bound to a keymap (prefix)
         (binding (key-binding (vector event)))
         (is-prefix (keymapp binding)))
    (when key-value
      `(:key ,key-value
        :ctrl ,(if (memq 'control mods) t :false)
        :alt ,(if (memq 'meta mods) t :false)
        :shift ,(if (memq 'shift mods) t :false)
        :super ,(if (memq 'super mods) t :false)
        :is-prefix ,(if is-prefix t :false)))))

(defun ewm--scan-keymap-for-modifiers (keymap modifiers)
  "Scan KEYMAP for keys that have any of MODIFIERS.
Returns a list of intercept specs."
  (let ((specs '()))
    (map-keymap
     (lambda (event binding)
       (when (and binding
                  (not (eq binding 'undefined))
                  ;; Check if event has any of the target modifiers
                  (let ((event-mods (event-modifiers event)))
                    (cl-intersection event-mods modifiers)))
         (when-let ((spec (ewm--event-to-intercept-spec event)))
           (push spec specs))))
     keymap)
    specs))

(defun ewm--send-intercept-keys ()
  "Send intercepted keys configuration to compositor.
Scans Emacs keymaps for keys matching `ewm-intercept-modifiers',
and adds `ewm-intercept-prefixes'.
This allows normal `global-set-key' bindings to work with EWM."
  (let ((specs '())
        (seen (make-hash-table :test 'equal)))
    ;; Add prefix keys first
    (dolist (key ewm-intercept-prefixes)
      ;; Handle both character literals (integers) and strings
      (let ((event (cond
                    ((integerp key) key)
                    ((stringp key) (aref (key-parse key) 0))
                    (t nil))))
        (when event
          (when-let ((spec (ewm--event-to-intercept-spec event)))
            (let ((spec-key (format "%S" spec)))
              (unless (gethash spec-key seen)
                (puthash spec-key t seen)
                (push spec specs)))))))
    ;; Scan global-map for keys with configured modifiers
    (when ewm-intercept-modifiers
      (dolist (spec (ewm--scan-keymap-for-modifiers
                     (current-global-map) ewm-intercept-modifiers))
        (let ((spec-key (format "%S" spec)))
          (unless (gethash spec-key seen)
            (puthash spec-key t seen)
            (push spec specs)))))
    ;; Send to compositor
    (ewm-intercept-keys-module (vconcat (nreverse specs)))))

;;; XKB configuration

(defun ewm--send-xkb-config ()
  "Send XKB configuration to compositor."
  (when ewm--module-mode
    (ewm-configure-xkb-module (string-join ewm-xkb-layouts ",") ewm-xkb-options)))

(provide 'ewm-input)
;;; ewm-input.el ends here
