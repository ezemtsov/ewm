;;; ewm-input.el --- Input handling for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Input handling for EWM including key interception, mouse-follows-focus,
;; and keyboard layout configuration.
;;
;; The compositor intercepts keys from two sources:
;;
;; 1. `ewm-mode-map' - All bindings in this keymap are intercepted.
;;    Override defaults with use-package :bind (:map ewm-mode-map ...).
;; 2. `ewm-intercept-prefixes' - Keys that start command sequences (C-x, M-x).
;;
;; Example:
;;   (use-package ewm
;;     :bind (:map ewm-mode-map ("s-d" . consult-buffer)))

;;; Code:

(require 'cl-lib)

(defun ewm-tab-select-or-return ()
  "Select tab by number, or switch to recent if already on that tab.
Reads the digit from `last-command-event' for i3-style back-and-forth."
  (interactive)
  (let* ((key (event-basic-type last-command-event))
         (tab (if (and (characterp key) (>= key ?1) (<= key ?9))
                  (- key ?0)
                0))
         (current (1+ (tab-bar--current-tab-index))))
    (if (eq tab current)
        (tab-recent)
      (tab-bar-select-tab tab))))

(defvar ewm-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "<XF86WakeUp>") #'ignore)
    ;; Window navigation (super + arrows)
    (define-key map (kbd "s-<left>") #'windmove-left)
    (define-key map (kbd "s-<right>") #'windmove-right)
    (define-key map (kbd "s-<down>") #'windmove-down)
    (define-key map (kbd "s-<up>") #'windmove-up)
    ;; Application launcher
    (define-key map (kbd "s-d") #'ewm-launch-app)
    ;; Tab/workspace management
    (define-key map (kbd "s-t") #'tab-new)
    (define-key map (kbd "s-w") #'tab-close)
    (dotimes (i 9)
      (define-key map (kbd (format "s-%d" (1+ i))) #'ewm-tab-select-or-return))
    map)
  "Keymap for `ewm-mode'.
Default super-key bindings for window management.
Override individual bindings with `use-package':
  :bind (:map ewm-mode-map (\"s-d\" . my-launcher))")

(declare-function ewm-intercept-keys-module "ewm-core")
(declare-function ewm-configure-xkb-module "ewm-core")
(declare-function ewm-configure-input-module "ewm-core")
(declare-function ewm-get-pointer-location "ewm-core")
(declare-function ewm-clear-prefix-sequence "ewm-core")
(declare-function ewm-in-prefix-sequence-p "ewm-core")
(declare-function ewm--sync-focus "ewm-focus")
(declare-function ewm-warp-pointer "ewm")
(declare-function ewm--get-output-offset "ewm")

(defvar ewm-surface-id)
(defvar ewm--module-mode)
(defvar ewm-mouse-follows-focus)
(defvar ewm--mff-last-window)

(defgroup ewm-input nil
  "EWM input handling."
  :group 'ewm)

;;; Libinput device configuration

(defcustom ewm-input-config nil
  "Input device configuration alist.
Each entry is (KEY . PROPS) where KEY is:
  - A symbol for type defaults: `touchpad', `mouse', `trackball', `trackpoint'
  - A string for device-specific overrides (exact device name)

PROPS is a plist of settings (omitted properties use device defaults):
  :natural-scroll BOOL  - Invert scroll direction
  :tap BOOL             - Tap-to-click (touchpad only)
  :dwt BOOL             - Disable while typing (touchpad only)
  :accel-speed FLOAT    - Pointer acceleration (-1.0 to 1.0)
  :accel-profile STRING - \"flat\" or \"adaptive\"
  :click-method STRING  - \"button-areas\" or \"clickfinger\" (touchpad only)
  :scroll-method STRING - \"no-scroll\", \"two-finger\", \"edge\", \"on-button-down\"
  :left-handed BOOL     - Swap left/right buttons
  :middle-emulation BOOL - Emulate middle button
  :tap-button-map STRING - \"left-right-middle\" or \"left-middle-right\" (touchpad only)

Device-specific entries override type defaults for matching settings.

Example:
  (setq ewm-input-config
    \\='((touchpad :natural-scroll t :tap t)
      (mouse :accel-profile \"flat\")
      (trackpoint :accel-speed 0.5)
      (\"ELAN0676:00 04F3:3195 Touchpad\" :tap nil :accel-speed -0.2)))"
  :type '(alist :key-type (choice symbol string)
                :value-type plist)
  :initialize 'custom-initialize-default
  :set (lambda (sym val)
         (set-default sym val)
         (when ewm--module-mode
           (ewm--send-input-config)))
  :group 'ewm-input)

(defun ewm--send-input-config ()
  "Send input device configuration to compositor."
  (when ewm--module-mode
    (let ((entries nil))
      (dolist (entry ewm-input-config)
        (let* ((key (car entry))
               (props (cdr entry))
               (plist (if (symbolp key)
                          (append (list :type (symbol-name key)) props)
                        (append (list :device key) props))))
          (push plist entries)))
      (ewm-configure-input-module (vconcat (nreverse entries))))))

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


;;; Input mode

(defun ewm-input--update-mode (mode)
  "Update input mode to MODE for current buffer.
MODE is either `line-mode' or `char-mode'.

In EXWM, line-mode still gives the X window keyboard focus but intercepts
keys via XGrabKey.  In EWM/Wayland, we achieve similar behavior by:
- Line-mode: surface has focus, compositor intercepts prefix keys
- Char-mode: surface has focus, no interception (same as line-mode for now)

Both modes keep focus on the surface so typing works immediately.
Focus is managed by `ewm--sync-focus' through window change hooks."
  (when ewm-surface-id
    (setq ewm-input--mode mode)
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

;;; Prefix sequence completion
;;
;; When a prefix key (C-x, s-SPC, etc.) is intercepted from an external
;; surface, the compositor redirects focus to Emacs and sets the prefix flag.
;; After the command sequence completes, we clear the flag and restore focus.
;; Window/buffer change hooks also call `ewm--sync-focus', but commands that
;; complete without changing windows (e.g. layout switch via s-SPC e) need
;; this post-command path to restore focus to the pre-intercept surface.

(defun ewm-input--clear-prefix ()
  "Complete prefix sequence: clear flag and restore compositor focus.
Only acts when a prefix sequence was active.  Skipped during SIGUSR1
handling — the signal handler runs as a command but is not a real
user action."
  (unless (eq this-command 'ewm--sigusr1-handler)
    (when (ewm-in-prefix-sequence-p)
      (ewm-clear-prefix-sequence)
      (ewm--sync-focus))))

(defun ewm-input--enable ()
  "Enable EWM input handling."
  (setq ewm--mff-last-window (selected-window))
  (add-hook 'post-command-hook #'ewm-input--clear-prefix)
  ;; Mouse-follows-focus hooks
  (advice-add 'select-window :after #'ewm-input--on-select-window)
  (advice-add 'select-frame-set-input-focus :after #'ewm-input--on-select-frame))

(defun ewm-input--disable ()
  "Disable EWM input handling."
  (setq ewm--mff-last-window nil)
  (remove-hook 'post-command-hook #'ewm-input--clear-prefix)
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

(defun ewm--scan-keymap (keymap)
  "Scan KEYMAP for all key bindings.
Returns a list of intercept specs."
  (let ((specs '()))
    (map-keymap
     (lambda (event binding)
       (when (and binding (not (eq binding 'undefined)))
         (when-let ((spec (ewm--event-to-intercept-spec event)))
           (push spec specs))))
     keymap)
    specs))

(defun ewm--send-intercept-keys ()
  "Send intercepted keys configuration to compositor.
Scans `ewm-mode-map' and adds `ewm-intercept-prefixes'."
  (let ((specs '())
        (seen (make-hash-table :test 'equal)))
    ;; Add prefix keys
    (dolist (key ewm-intercept-prefixes)
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
    ;; Scan ewm-mode-map
    (dolist (spec (ewm--scan-keymap ewm-mode-map))
      (let ((spec-key (format "%S" spec)))
        (unless (gethash spec-key seen)
          (puthash spec-key t seen)
          (push spec specs))))
    ;; Send to compositor
    (ewm-intercept-keys-module (vconcat (nreverse specs)))))

;;; XKB configuration

(defun ewm--send-xkb-config ()
  "Send XKB configuration to compositor."
  (when ewm--module-mode
    (ewm-configure-xkb-module (string-join ewm-xkb-layouts ",") ewm-xkb-options)))

(provide 'ewm-input)
;;; ewm-input.el ends here
