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

(defun ewm--maybe-animate-tab-move (target)
  "Start a workspace-transition animation for a move toward TARGET.
TARGET is a 0-based tab index or nil.  No-op when TARGET is nil, it
equals the current tab, or the frame has no ewm output."
  (when-let* ((target)
              (current (tab-bar--current-tab-index))
              ((/= target current))
              (output (frame-parameter nil 'ewm-output)))
    (ewm-start-workspace-transition output (if (> target current) 1 -1))))

(defun ewm-tab-select-or-return ()
  "Select tab by number, or switch to recent if already on that tab.
Reads the digit from `last-command-event' for i3-style back-and-forth.

On an ewm-managed frame, a horizontal slide animation plays during
the switch; direction follows the sign of the target-minus-current
index delta.  `tab-bar--tab-index-recent' resolves the same-press
destination up front so the animation matches where we'll land.

Focus inside the landing tab is whatever the tab had saved -- indexed
jumps preserve per-tab window state, unlike `ewm-windmove-left' /
`ewm-windmove-right' which reposition to the outmost window."
  (interactive)
  (let* ((pressed (- (event-basic-type last-command-event) ?0))
         (same-press (= pressed (1+ (tab-bar--current-tab-index))))
         (target (if same-press
                     (tab-bar--tab-index-recent 1)
                   (and (<= pressed (length (funcall tab-bar-tabs-function)))
                        (1- pressed)))))
    (ewm--maybe-animate-tab-move target)
    (if same-press (tab-recent) (tab-bar-select-tab pressed))))

(defmacro ewm--cmd (&rest args)
  "Create an interactive command that runs ARGS as a subprocess."
  `(lambda () (interactive) (start-process ,(car args) nil ,@args)))

(defvar ewm-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "<XF86WakeUp>") #'ignore)
    ;; Media/hardware keys
    (define-key map (kbd "<AudioRaiseVolume>")  (ewm--cmd "wpctl" "set-volume" "-l" "1.0" "@DEFAULT_AUDIO_SINK@" "5%+"))
    (define-key map (kbd "<AudioLowerVolume>")  (ewm--cmd "wpctl" "set-volume" "@DEFAULT_AUDIO_SINK@" "5%-"))
    (define-key map (kbd "<AudioMute>")         (ewm--cmd "wpctl" "set-mute" "@DEFAULT_AUDIO_SINK@" "toggle"))
    (define-key map (kbd "<AudioMicMute>")      (ewm--cmd "wpctl" "set-mute" "@DEFAULT_AUDIO_SOURCE@" "toggle"))
    (define-key map (kbd "<MonBrightnessUp>")   (ewm--cmd "brightnessctl" "set" "5%+"))
    (define-key map (kbd "<MonBrightnessDown>") (ewm--cmd "brightnessctl" "set" "5%-"))
    ;; Window navigation (super + arrows). Left/right fall through to the
    ;; adjacent tab with a slide animation when there's no window in that
    ;; direction -- tab boundaries feel seamless during horizontal traversal.
    (define-key map (kbd "s-<left>") #'ewm-windmove-left)
    (define-key map (kbd "s-<right>") #'ewm-windmove-right)
    (define-key map (kbd "s-<down>") #'windmove-down)
    (define-key map (kbd "s-<up>") #'windmove-up)
    ;; Clipboard
    (define-key map (kbd "s-c") #'kill-ring-save)
    (define-key map (kbd "s-v") #'yank)
    ;; Application launcher
    (define-key map (kbd "s-d") #'ewm-launch-app)
    ;; Fullscreen toggle
    (define-key map (kbd "s-f") #'ewm-toggle-fullscreen)
    ;; Lock session
    (define-key map (kbd "s-l") #'ewm-lock-session)
    ;; Surface buffer cycling
    (define-key map (kbd "s-<tab>") #'ewm-next-surface-buffer)
    ;; Emacs sees S-Tab as iso-lefttab; compositor sees Tab+Shift.  Need both.
    (define-key map (kbd "s-<iso-lefttab>") #'ewm-prev-surface-buffer)
    (define-key map (kbd "s-S-<tab>") #'ewm-prev-surface-buffer)
    (define-key map (kbd "s-t") #'tab-new)
    (define-key map (kbd "s-w") #'tab-close)
    ;; Buffer movement
    (define-key map (kbd "C-s-<left>") #'buf-move-left)
    (define-key map (kbd "C-s-<right>") #'buf-move-right)
    (define-key map (kbd "C-s-<up>") #'buf-move-up)
    (define-key map (kbd "C-s-<down>") #'buf-move-down)
    ;; Workspace shortcuts
    (dotimes (i 9)
      (define-key map (kbd (format "s-%d" (1+ i))) #'ewm-tab-select-or-return))
    map)
  "Keymap for `ewm-mode'.
Default super-key bindings for window management.
Override individual bindings with `use-package':
  :bind (:map ewm-mode-map (\"s-d\" . my-launcher))")

(declare-function ewm-intercept-keys-module "ewm-core")
(declare-function ewm-configure-input-module "ewm-core")
(declare-function ewm-get-pointer-location "ewm-core")
(declare-function ewm-clear-prefix-sequence "ewm-core")
(declare-function ewm-in-prefix-sequence-p "ewm-core")
(declare-function ewm--sync-focus "ewm-focus")
(declare-function ewm-warp-pointer "ewm")
(declare-function ewm--get-output-origin "ewm")
;; Defined in ewm.el (loaded after ewm-input.el)
(declare-function ewm-launch-app "ewm")
(declare-function ewm-toggle-fullscreen "ewm")
(declare-function ewm-lock-session "ewm")
(declare-function ewm-windmove-left "ewm")
(declare-function ewm-windmove-right "ewm")
(declare-function ewm-start-workspace-transition "ewm")

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
  - A symbol for type defaults: `touchpad', `mouse', `trackball',
    `trackpoint', `keyboard'
  - A string for device-specific overrides (exact device name)

PROPS is a plist of settings (omitted properties use device defaults):

  Pointer devices (touchpad, mouse, trackball, trackpoint):
  :natural-scroll BOOL  - Invert scroll direction
  :tap BOOL             - Tap-to-click (touchpad only)
  :dwt BOOL             - Disable while typing (touchpad only)
  :accel-speed FLOAT    - Pointer acceleration (-1.0 to 1.0)
  :accel-profile STRING - \"flat\" or \"adaptive\"
  :click-method STRING  - \"button-areas\" or \"clickfinger\" (touchpad only)
  :scroll-method STRING - \"no-scroll\", \"two-finger\", \"edge\", \"on-button-down\"
  :left-handed BOOL     - Swap left/right buttons
  :middle-emulation BOOL - Emulate middle button
  :tap-button-map STRING - \"left-right-middle\" or
                           \"left-middle-right\" (touchpad only)

  Keyboard:
  :repeat-delay INT     - Key repeat delay in milliseconds (default 200)
  :repeat-rate INT      - Key repeat rate in Hz (default 25)
  :xkb-layouts STRING   - Comma-separated XKB layout names (e.g. \"us,ru\")
  :xkb-variants STRING  - Comma-separated XKB variants, parallel to layouts
                          (e.g. \"dvorak,\" or \",ergol\"; empty entry = default)
  :xkb-options STRING   - XKB options (e.g. \"ctrl:nocaps\")

Device-specific entries override type defaults for matching settings.

Example:
  (setq ewm-input-config
    \\='((touchpad :natural-scroll t :tap t)
      (mouse :accel-profile \"flat\")
      (keyboard :repeat-delay 200 :repeat-rate 25
                :xkb-layouts \"us,ru\" :xkb-options \"ctrl:nocaps\")
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

;;; Key interception

(defcustom ewm-intercept-prefixes
  '(?\C-x ?\C-u ?\C-h ?\M-x (?\s-f :fullscreen)
    ("<MonBrightnessUp>" :fullscreen) ("<MonBrightnessDown>" :fullscreen)
    ("<AudioRaiseVolume>" :fullscreen) ("<AudioLowerVolume>" :fullscreen)
    ("<AudioMute>" :fullscreen) ("<AudioMicMute>" :fullscreen)
    ("<Print>" :fullscreen))
  "Keys that always go to Emacs, even when a surface has focus.
Each entry is a key or (key :fullscreen).  Plain keys are intercepted
normally; :fullscreen keys are also sent to Emacs when a fullscreen
surface has focus.  Keys can be character literals or strings.

Bindings in `ewm-mode-map' are always intercepted (without :fullscreen).

Add more as needed:
  (add-to-list \\='ewm-intercept-prefixes ?\\M-\\`)  ; tmm-menubar
  (add-to-list \\='ewm-intercept-prefixes \\='(\"<print>\" :fullscreen))"
  :type '(repeat (choice character string
                          (list (choice character string) (const :fullscreen))))
  :group 'ewm-input)

(defcustom ewm-surface-emulate-keys
  '((?\s-c . "ctrl")
    (?\s-v . "ctrl"))
  "Super+KEY bindings that translate to Ctrl+KEY for non-Emacs surfaces.
Each entry is (KEY-EVENT . TARGET-MOD) where TARGET-MOD is \"ctrl\",
\"alt\", or \"shift\".

When a non-Emacs Wayland surface has focus, these keys are translated
\(e.g., s-c becomes C-c for clipboard copy in Firefox).  When Emacs
has focus, the original binding from `ewm-mode-map' is used."
  :type '(alist :key-type character :value-type string)
  :initialize 'custom-initialize-default
  :set (lambda (sym val)
         (set-default sym val)
         (when ewm--module-mode
           (ewm--send-intercept-keys)))
  :group 'ewm-input)

;;; Mouse-follows-focus

(defun ewm-input--pointer-in-window-p (window)
  "Return non-nil if pointer is inside WINDOW.
Coordinates are in compositor space."
  (let* ((frame (window-frame window))
         (output (frame-parameter frame 'ewm-output))
         (output-origin (ewm--get-output-origin output))
         (edges (window-pixel-edges window))
         (left (+ (car output-origin) (nth 0 edges)))
         (top (+ (cdr output-origin) (nth 1 edges)))
         (right (+ (car output-origin) (nth 2 edges)))
         (bottom (+ (cdr output-origin) (nth 3 edges)))
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
    (when-let* ((frame (window-frame window))
                (output (frame-parameter frame 'ewm-output))
                (output-origin (ewm--get-output-origin output)))
      (let* ((edges (window-inside-pixel-edges window))
             (x (+ (car output-origin) (/ (+ (nth 0 edges) (nth 2 edges)) 2)))
             (y (+ (cdr output-origin) (/ (+ (nth 1 edges) (nth 3 edges)) 2))))
        (ewm-warp-pointer (float x) (float y))))))

(defun ewm-input--on-window-selection-change (_frame)
  "Warp pointer when selected window changes.
Runs in `window-selection-change-functions', which fires once after
all intermediate `select-window' calls settle.  This avoids spurious
warps from packages that temporarily switch windows internally
\(e.g., Gnus `save-window-excursion')."
  (when ewm-mouse-follows-focus
    (let ((window (selected-window)))
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
Only acts when a prefix sequence was active."
  (when (ewm-in-prefix-sequence-p)
    (ewm-clear-prefix-sequence)
    (ewm--sync-focus)))

;;; Frame-switch suppression
;;
;; `handle-switch-frame' fires when PGTK's `pgtk_focus_frame' updates GDK
;; state during `select-frame'.  In multi-output setups this targets the
;; wrong frame, overriding compositor click-to-focus.  Suppress it entirely
;; EWM owns frame selection via `ewm--handle-focus'.  Same pattern as
;; EXWM suppressing `handle-focus-in'/`handle-focus-out'.

(defun ewm-input--suppress-switch-frame (orig-fn event)
  "Suppress `handle-switch-frame' when EWM is active.
EWM owns frame selection via `ewm--handle-focus'."
  (unless ewm--module-mode
    (funcall orig-fn event)))

(defun ewm-input--enable ()
  "Enable EWM input handling."
  (setq ewm--mff-last-window (selected-window))
  (add-hook 'post-command-hook #'ewm-input--clear-prefix)
  (add-hook 'window-selection-change-functions #'ewm-input--on-window-selection-change)
  ;; Suppress GDK-triggered frame switching (compositor handles this)
  (advice-add 'handle-switch-frame :around #'ewm-input--suppress-switch-frame))

(defun ewm-input--disable ()
  "Disable EWM input handling."
  (setq ewm--mff-last-window nil)
  (remove-hook 'post-command-hook #'ewm-input--clear-prefix)
  (remove-hook 'window-selection-change-functions #'ewm-input--on-window-selection-change)
  (advice-remove 'handle-switch-frame #'ewm-input--suppress-switch-frame))

;;; Key scanning and interception

(defun ewm--event-to-intercept-spec (event &optional fullscreen emulate)
  "Convert EVENT to an intercept specification for the compositor.
Returns a plist with :key, modifier flags, :fullscreen, and optionally :emulate.
EMULATE is a target modifier string (e.g. \"ctrl\") for key emulation."
  (let* ((mods (event-modifiers event))
         (base (event-basic-type event))
         ;; base is either an integer (ASCII) or a symbol (special key)
         (key-value (cond
                     ((integerp base) base)
                     ((symbolp base) (symbol-name base))
                     (t nil))))
    (when key-value
      (let ((spec `(:key ,key-value
                    :ctrl ,(if (memq 'control mods) t :false)
                    :alt ,(if (memq 'meta mods) t :false)
                    :shift ,(if (memq 'shift mods) t :false)
                    :super ,(if (memq 'super mods) t :false)
                    :fullscreen ,(if fullscreen t :false))))
        (when emulate
          (setq spec (append spec (list :emulate emulate))))
        spec))))

(defun ewm--parse-intercept-entry (entry)
  "Parse ENTRY from `ewm-intercept-prefixes'.
Returns (event . fullscreen-p) or nil."
  (cond
   ((integerp entry) (cons entry nil))
   ((stringp entry) (cons (aref (key-parse entry) 0) nil))
   ((and (listp entry) (memq :fullscreen entry))
    (let ((key (car entry)))
      (cond
       ((integerp key) (cons key t))
       ((stringp key) (cons (aref (key-parse key) 0) t))
       (t nil))))
   (t nil)))

(defun ewm--send-intercept-keys ()
  "Send intercepted keys configuration to compositor.
Scans `ewm-intercept-prefixes' first (so :fullscreen flags win),
then `ewm-mode-map' bindings.  Keys in `ewm-surface-emulate-keys'
get an :emulate flag for modifier translation on non-Emacs surfaces."
  (let ((specs '())
        (fs-events (make-hash-table :test 'eql))
        (emulate-map (make-hash-table :test 'eql)))
    ;; Build emulate lookup
    (dolist (entry ewm-surface-emulate-keys)
      (puthash (car entry) (cdr entry) emulate-map))
    ;; Intercept prefixes first; :fullscreen entries populate fs-events
    (dolist (entry ewm-intercept-prefixes)
      (when-let* ((parsed (ewm--parse-intercept-entry entry)))
        (let ((event (car parsed)))
          (when (cdr parsed)
            (puthash event t fs-events))
          (when-let* ((spec (ewm--event-to-intercept-spec event (cdr parsed))))
            (push spec specs)))))
    ;; Keymap bindings: fullscreen flag and emulate looked up
    (map-keymap
     (lambda (event binding)
       (when (and binding (not (eq binding 'undefined)) (not (eq binding 'ignore)))
         (when-let* ((spec (ewm--event-to-intercept-spec
                           event
                           (gethash event fs-events)
                           (gethash event emulate-map))))
           (push spec specs))))
     ewm-mode-map)
    (ewm-intercept-keys-module (vconcat (nreverse specs)))))

(provide 'ewm-input)
;;; ewm-input.el ends here
