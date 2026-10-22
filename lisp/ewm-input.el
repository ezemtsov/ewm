;;; ewm-input.el --- Input handling for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Input handling for EWM including key interception, mouse-follows-focus,
;; and keyboard layout configuration.
;;
;; The compositor intercepts keys from three sources:
;;
;; 1. `ewm-mode-map' - All bindings in this keymap are intercepted, those
;;    of commands with an `ewm-action' property run in the compositor.
;;    Override defaults with use-package :bind (:map ewm-mode-map ...).
;;    Touchpad swipes bind as `<swipe-FINGERS-DIRECTION>' keys, with the
;;    usual modifier prefixes.
;; 2. `ewm-intercept-prefixes' - Extra keys to intercept, plus keys that
;;    should also be intercepted while a client is fullscreen, as the
;;    compositor commands of `ewm-mode-map' are.
;; 3. `ewm-surface-emulate-keys' - Key entries are intercepted as commands
;;    and `ewm-surface-emulate' sends the mapped keys to the client.
;;
;; Example:
;;   (use-package ewm
;;     :bind (:map ewm-mode-map ("s-d" . consult-buffer)))

;;; Code:

(require 'cl-lib)
(require 'windmove)
(require 'ewm-module)
(ewm-module--declare)

(declare-function ewm--send-config "ewm")
(require 'ewm-surface)
(require 'ewm-layout)
(require 'ewm-frame)
(require 'ewm-focus)
(require 'ewm-text-input)
(require 'ewm-commands)

(defvar-keymap ewm-mode-map
  :doc "Keymap for `ewm-mode'.
Default super-key bindings for window management.
Override individual bindings with `use-package':
  :bind (:map ewm-mode-map (\"s-d\" . my-launcher))"
  "<XF86WakeUp>" #'ignore
  "C-M-<XF86WakeUp>" #'ignore
  ;; Media/hardware keys
  "<AudioRaiseVolume>" #'ewm-volume-raise
  "<AudioLowerVolume>" #'ewm-volume-lower
  "<AudioMute>" #'ewm-volume-mute
  "<AudioMicMute>" #'ewm-mic-mute
  "<MonBrightnessUp>" #'ewm-brightness-up
  "<MonBrightnessDown>" #'ewm-brightness-down
  ;; Window navigation; crosses outputs at the frame edge.
  "s-<left>" #'ewm-focus-left
  "s-<right>" #'ewm-focus-right
  "s-<up>" #'ewm-focus-up
  "s-<down>" #'ewm-focus-down
  ;; Workspace switching on the current output's strip.
  "S-s-<left>" #'ewm-frame-left
  "S-s-<right>" #'ewm-frame-right
  ;; Overview: zoom the strip out and back in.
  "S-s-<up>" #'ewm-overview-open
  "S-s-<down>" #'ewm-overview-close
  ;; Touchpad swipes follow the fingers: up and down drive the overview, a swipe
  ;; left brings in the frame to the right.
  "<swipe-3-up>" #'ewm-overview-open
  "<swipe-3-down>" #'ewm-overview-close
  "<swipe-3-left>" #'ewm-frame-right
  "<swipe-3-right>" #'ewm-frame-left
  ;; Slide every frame off screen and back.
  "s-m" #'ewm-hide-toggle
  ;; Clipboard
  "s-c" #'kill-ring-save
  "s-v" #'yank
  ;; Application launcher
  "s-d" #'ewm-launch-app
  ;; Fullscreen toggle
  "s-f" #'ewm-toggle-fullscreen
  ;; Floating detach/attach toggle
  "S-s-SPC" #'ewm-floating-toggle
  ;; Lock session
  "s-l" #'ewm-lock-session
  ;; Edit the focused text field
  "s-e" #'ewm-edit
  ;; Surface buffer cycling
  "s-<tab>" #'ewm-next-surface-buffer
  ;; Emacs sees S-Tab as iso-lefttab; compositor sees Tab+Shift.  Need both.
  "s-<iso-lefttab>" #'ewm-prev-surface-buffer
  "S-s-<tab>" #'ewm-prev-surface-buffer
  ;; Frame strip
  "s-t" #'ewm-frame-new
  "s-w" #'ewm-frame-close
  ;; Reorder frames horizontally; reorder windows within a frame vertically.
  "C-s-<left>" #'ewm-frame-move-left
  "C-s-<right>" #'ewm-frame-move-right
  "C-s-<up>" #'windmove-swap-states-up
  "C-s-<down>" #'windmove-swap-states-down
  ;; Select Nth frame on current output's strip.
  "s-1" #'ewm-frame-select
  "s-2" #'ewm-frame-select
  "s-3" #'ewm-frame-select
  "s-4" #'ewm-frame-select
  "s-5" #'ewm-frame-select
  "s-6" #'ewm-frame-select
  "s-7" #'ewm-frame-select
  "s-8" #'ewm-frame-select
  "s-9" #'ewm-frame-select)

;;; Libinput device configuration

(defcustom ewm-input-config nil
  "Input device configuration alist.
Each entry is (KEY . PROPS) where KEY is:
  - A symbol for type defaults: `touchpad', `mouse', `trackball',
    `trackpoint', `keyboard', `tablet', `touch'
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

  Tablet:
  :left-handed BOOL     - Rotate the tablet 180 degrees
  :calibration-matrix LIST - Six numbers, libinput calibration matrix
  :map-to-output STRING - Output name the tablet maps onto
  :map-to-focused-output BOOL - Map onto the focused output instead;
                                takes precedence over :map-to-output
  Without a mapping the tablet covers all outputs together.

  Touch:
  :calibration-matrix LIST - Six numbers, libinput calibration matrix
  :map-to-output STRING - Output name the touchscreen maps onto
  Without a mapping the touchscreen follows the first output, with
  its transform and scale.

Device-specific entries override type defaults for matching settings.

Example:
  (setopt ewm-input-config
    \\='((touchpad :natural-scroll t :tap t)
      (mouse :accel-profile \"flat\")
      (keyboard :repeat-delay 200 :repeat-rate 25
                :xkb-layouts \"us,ru\" :xkb-options \"ctrl:nocaps\")
      (\"ELAN0676:00 04F3:3195 Touchpad\" :tap nil :accel-speed -0.2)))"
  :type '(alist :key-type (choice symbol string)
                :value-type plist)
  :set (ewm--send-on-set #'ewm--send-config)
  :group 'ewm-input-devices)

(defun ewm--input-config ()
  "`ewm-input-config' as the list of plists the compositor reads."
  (mapcar (lambda (entry)
            (pcase-let ((`(,key . ,props) entry))
              (append (if (symbolp key)
                          (list :type (symbol-name key))
                        (list :device key))
                      props)))
          ewm-input-config))

;;; Key interception

(defvar ewm-input--resend-timer nil
  "Idle timer resending the intercept list after an `ewm-mode-map' change.")

(defun ewm-input--cancel-resend ()
  "Cancel a pending resend of the intercept list."
  (when ewm-input--resend-timer
    (cancel-timer ewm-input--resend-timer)
    (setq ewm-input--resend-timer nil)))

(define-widget 'ewm-key 'lazy
  "A single key, as an integer event or a key description."
  :tag "Key"
  :type '(choice character integer string)
  :match (lambda (_ key) (ewm--key-event key)))

(defcustom ewm-intercept-prefixes
  '(?\C-x ?\C-u ?\C-h ?\M-x ("<Print>" :fullscreen))
  "Keys that always go to Emacs, even when a surface has focus.
Each entry is a key or (key :fullscreen), where a key is an integer
event such as ?\\M-x or a key description naming one key. :fullscreen
entries are intercepted even when a fullscreen surface has focus, as the
compositor commands of `ewm-mode-map' are.

Bindings in `ewm-mode-map' are always intercepted (without :fullscreen)
and use command-event delivery. Other keys enter the Emacs keyboard path.

Set with `setopt' (or `:custom' in `use-package'):

  (setopt ewm-intercept-prefixes
          \\='(\"C-x\" \"C-u\" \"C-h\" \"M-x\" \"M-`\"
            (\"<Print>\" :fullscreen)))"
  :type '(repeat (choice ewm-key (list ewm-key (const :fullscreen))))
  :set (ewm--send-on-set #'ewm--send-intercept-keys)
  :group 'ewm-keybindings)

(defcustom ewm-surface-emulate-keys
  '((kill-ring-save . "C-c")
    (yank . "C-v"))
  "Keys a client receives in place of what was typed, as (FROM . KEYS) entries.
FROM is a command or a single key, KEYS a key description Emacs sends with
`ewm-send-key' instead. A surface buffer's local value adds entries for
that surface only, see `ewm-update-app-hook'."
  :type '(alist :key-type (choice function ewm-key) :value-type string)
  :set (ewm--send-on-set #'ewm--send-intercept-keys)
  :group 'ewm-keybindings)

(defcustom ewm-repeatable-commands
  '(ewm-focus-left ewm-focus-right ewm-focus-up ewm-focus-down
    ewm-frame-left ewm-frame-right
    ewm-frame-move-left ewm-frame-move-right
    windmove-swap-states-up windmove-swap-states-down
    ewm-next-surface-buffer ewm-prev-surface-buffer)
  "Commands whose intercepted key bindings repeat while held."
  :type '(repeat function)
  :set (ewm--send-on-set #'ewm--send-intercept-keys)
  :group 'ewm-keybindings)

(defconst ewm-input--repeatable-key-descriptions
  '("<AudioRaiseVolume>" "<AudioLowerVolume>" "<AudioMute>" "<AudioMicMute>"
    "<MonBrightnessUp>" "<MonBrightnessDown>")
  "Hardware keys that repeat whatever they are bound to.")

(defun ewm-input--gdk-style-xf86-key-p (event)
  "Return non-nil when EVENT is a GDK-style XF86 key name."
  (let ((name (and (symbolp event) (symbol-name event))))
    (and name
         (> (length name) 0)
         (not (string-prefix-p "XF86" name))
         (<= ?A (aref name 0) ?Z))))

(defun ewm-input--install-xf86-key-aliases ()
  "Translate xkbcommon XF86 key names to GDK-compatible EWM names."
  (map-keymap
   (lambda (event _binding)
     (when (ewm-input--gdk-style-xf86-key-p event)
       (let* ((name (symbol-name event))
              (from (vector (intern (concat "XF86" name)))))
         (unless (lookup-key function-key-map from)
           (define-key function-key-map from (vector event))))))
   ewm-mode-map))

;;; Mouse-follows-focus

(defcustom ewm-mouse-follows-focus t
  "Whether the mouse pointer follows focus changes.
When non-nil, warps the pointer to the center of the focused window."
  :type 'boolean
  :group 'ewm-input-devices)

(defun ewm-warp-pointer (x y)
  "Warp pointer to absolute position X, Y."
  (ewm-warp-pointer-module x y))

(defun ewm-input--pointer-in-window-p (window origin)
  "Non-nil if the pointer is inside WINDOW, whose frame sits at ORIGIN."
  (let* ((frame (window-frame window))
         (pointer (ewm-get-pointer-location)))
    (eq window (ewm--window-at-logical frame
                                       (- (car pointer) (car origin))
                                       (- (cdr pointer) (cdr origin))))))

(defun ewm-input--warp-pointer-to-window (window)
  "Warp the pointer to WINDOW's center unless it is already there."
  (unless (window-minibuffer-p window)
    (let ((origin (ewm--get-frame-origin (window-frame window))))
      (unless (ewm-input--pointer-in-window-p window origin)
        (pcase-let ((`(,x ,y ,width ,height) (ewm--window-body-logical-rect window)))
          (ewm-warp-pointer (float (+ (car origin) x (/ width 2)))
                            (float (+ (cdr origin) y (/ height 2)))))))))

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

;;; Drag source
;;
;; mouse.el starts a cross-program drag only once the pointer leaves the
;; frame's pixel bounds, but other clients live inside frames here, so the
;; compositor reports motion over them as outside while `track-mouse' is
;; `drag-source'.  The `track-mouse' form binds and restores the variable
;; from C without notifying watchers, so the watcher only sees the `setq';
;; `post-command-hook' clears the flag once the command ends.

(defvar ewm-input--drag-source nil
  "Drag-source state last pushed to the compositor.")

(defun ewm-input--set-drag-source (active)
  "Push ACTIVE to the compositor when it differs from the last push."
  (setq active (and active t))
  ;; Cache only what a live module was told, so a change while the module
  ;; is off is re-pushed once it starts.
  (when (and (not (eq active ewm-input--drag-source))
             ewm--module-mode)
    (setq ewm-input--drag-source active)
    (ewm-set-drag-source active)))

(defun ewm-input--track-mouse-watcher (_symbol newval _operation _where)
  "Mirror `track-mouse' becoming NEWVAL into the compositor."
  (ewm-input--set-drag-source (eq newval 'drag-source)))

(defun ewm-input--sync-drag-source ()
  "Mirror the current `track-mouse' into the compositor."
  (ewm-input--set-drag-source (eq track-mouse 'drag-source)))

;;; Keyboard capture
;;
;; The compositor consumes the physical key that starts an intercepted Emacs
;; command.  Lisp then decides whether the Emacs command loop needs subsequent
;; Wayland key events: keymap prefixes, minibuffers, transient maps, universal
;; arguments, and explicit read-key/read-event calls capture keyboard focus;
;; ordinary commands do not.  Background timers may prompt, but must not start
;; capture while a client surface has focus.

(defvar ewm-input--keyboard-capture-token nil
  "Current compositor keyboard-capture lease token, or nil.")

(defun ewm-input--prefix-key-active-p ()
  "Return non-nil when the current command keys are an incomplete prefix."
  (and (null this-command)
       (let* ((keys (this-command-keys-vector))
              (binding (and (> (length keys) 0)
                            (key-binding keys t))))
         (keymapp binding))))

(defun ewm-input--modal-input-active-p ()
  "Return non-nil when Emacs is waiting for command-loop keyboard input."
  (or (ewm-layout--menu-active-p)
      prefix-arg
      (ewm-input--prefix-key-active-p)))

(defun ewm-input--emacs-frame-focused-p ()
  "Return non-nil when compositor focus is on the active Emacs frame."
  (let ((focused-id (ewm-get-focused-id))
        (frame-id (ewm-get-focused-frame-id)))
    (and (numberp focused-id)
         (numberp frame-id)
         (> frame-id 0)
         (= focused-id frame-id))))

(defun ewm-input--command-key-active-p ()
  "Return non-nil when modal input follows a real command key."
  (and this-command
       (not (memq this-command '(ignore undefined)))
       (> (length (this-command-keys-vector)) 0)))

(defun ewm-input--keyboard-capture-start-allowed-p ()
  "Return non-nil when a new compositor keyboard-capture lease may start."
  ;; A client-field input method reads keys the compositor already intercepts.
  (and (not ewm-text-input--composing)
       (or (not ewm--module-mode)
           (ewm-input--emacs-frame-focused-p)
           (ewm-input--command-key-active-p)
           (ewm-input--prefix-key-active-p))))

(defvar ewm--pending-keys nil
  "Sends waiting for Emacs to release the keyboard, newest first.")

(defun ewm-input--set-keyboard-capture (enabled &optional clear-redirect)
  "Synchronize Lisp command-loop capture.
When CLEAR-REDIRECT is non-nil, also clear compositor-owned keyboard
redirect capture."
  (let ((enabled (and enabled t))
        (token ewm-input--keyboard-capture-token))
    (cond
     ((and enabled (not token))
      (setq ewm-input--keyboard-capture-token
            (if ewm--module-mode
                (ewm-keyboard-capture-begin-module "command-loop")
              0)))
     ((and (not enabled) token)
      (setq ewm-input--keyboard-capture-token nil)
      (when (and ewm--module-mode (> token 0))
        (ewm-keyboard-capture-end-module token))))
    (when (and clear-redirect (not enabled) ewm--module-mode)
      (ewm-keyboard-redirect-capture-clear-module)
      (dolist (send (nreverse ewm--pending-keys))
        (apply #'ewm-send-key-module send))
      (setq ewm--pending-keys nil))))

(defun ewm-input--sync-keyboard-capture ()
  "Synchronize compositor keyboard capture with current Emacs input state."
  ;; A focus change ends no modal input, and may precede a redirected key.
  (unless (eq this-command 'ewm--focus-change)
    (let ((capture-active
           (and (ewm-input--modal-input-active-p)
                (or ewm-input--keyboard-capture-token
                    (ewm-input--keyboard-capture-start-allowed-p)))))
      (ewm-input--set-keyboard-capture capture-active (not capture-active)))))

(defun ewm-send-key (key &optional surface-id)
  "Send KEY, a key description, to SURFACE-ID or the current surface buffer's.
Nil for both means the focused surface. Waits while Emacs holds the keyboard."
  (let ((send (list (or surface-id ewm-surface-id)
                    (seq-keep #'ewm--event-key-spec (key-parse key)))))
    (if (ewm-keyboard-capture-active-module)
        (push send ewm--pending-keys)
      (apply #'ewm-send-key-module send))))

(defun ewm-surface-emulate ()
  "Send the keys `ewm-surface-emulate-keys' maps the typed key or command to."
  (interactive)
  (let* ((keys (this-command-keys-vector))
         (event (and (= (length keys) 1) (aref keys 0)))
         (command (unless (eq this-original-command 'ewm-surface-emulate)
                    this-original-command))
         (target (cl-loop for (from . to)
                          in (append ewm-surface-emulate-keys
                                     (default-value 'ewm-surface-emulate-keys))
                          when (if (symbolp from)
                                   (eq from command)
                                 (and event (eq (ewm--key-event from) event)))
                          return to)))
    (cond (target (ewm-send-key target))
          (command (call-interactively command))
          (t (ewm-send-key (key-description keys))))))

(defun ewm-input--keyboard-capture-on ()
  "Enable compositor keyboard capture for foreground Emacs input."
  (when (or ewm-input--keyboard-capture-token
            (ewm-input--keyboard-capture-start-allowed-p))
    (ewm-input--set-keyboard-capture t)))

(defun ewm-input--with-keyboard-capture (orig-fun &rest args)
  "Run ORIG-FUN while Emacs owns compositor keyboard focus."
  (if (ewm-layout--click-popup-p)
      ;; A click's popup is dismissed without a command to end a lease.
      (apply orig-fun args)
    (ewm-input--keyboard-capture-on)
    (unwind-protect
        (apply orig-fun args)
      (ewm-input--sync-keyboard-capture))))

(defconst ewm-input--keyboard-capture-read-functions
  '(read-key read-event read-key-sequence read-key-sequence-vector
             read-char read-char-exclusive)
  "Input-reading functions that require compositor keyboard capture.")

(defun ewm-input--set-read-function-capture (enabled)
  "Enable or disable capture advice around input-reading functions."
  (dolist (fn ewm-input--keyboard-capture-read-functions)
    (if enabled
        (advice-add fn :around #'ewm-input--with-keyboard-capture)
      (advice-remove fn #'ewm-input--with-keyboard-capture))))

;;; Input method follows the selected window
;;
;; `ewm--handle-focus' selects windows from the async event filter, where
;; `current-buffer' reverts on return though `select-window' sticks. So the
;; first key after a focus change is read against the previous buffer's input
;; method. Apply the selected window's instead, at both `read-char' entry
;; points: the global default and an :around advice on `quail-input-method'.
;; The text-input handler names its surface buffer itself and is left alone.

(defun ewm-input--in-selected-window (orig key)
  "Translate KEY through the selected window's input method, else ORIG."
  (let ((buf (window-buffer (selected-window))))
    (if (or ewm-text-input--composing (eq buf (current-buffer)))
        (funcall orig key)
      (with-current-buffer buf
        (if current-input-method (funcall input-method-function key) (list key))))))

(defun ewm-input--enable ()
  "Enable EWM input handling."
  (setq ewm--mff-last-window (selected-window))
  (add-hook 'post-command-hook #'ewm-input--sync-keyboard-capture)
  (add-hook 'post-command-hook #'ewm-input--sync-drag-source)
  (add-variable-watcher 'track-mouse #'ewm-input--track-mouse-watcher)
  (add-hook 'minibuffer-setup-hook #'ewm-input--keyboard-capture-on)
  (add-hook 'minibuffer-exit-hook #'ewm-input--sync-keyboard-capture)
  (add-hook 'window-selection-change-functions #'ewm-input--on-window-selection-change)
  (ewm-input--set-read-function-capture t)
  ;; PGTK reports GDK focus changes as `switch-frame' events. Kept out of the
  ;; command loop, they cannot fire `post-command-hook' mid-prefix.
  (define-key special-event-map [switch-frame] #'ignore)
  (define-key global-map [ewm-focus-change] #'ewm--focus-change)
  (setq-default input-method-function
                (apply-partially #'ewm-input--in-selected-window #'list))
  (advice-add 'quail-input-method :around #'ewm-input--in-selected-window)
  (advice-add 'define-key :after #'ewm-input--on-define-key))

(defun ewm-input--disable ()
  "Disable EWM input handling."
  (setq ewm--mff-last-window nil)
  (remove-hook 'post-command-hook #'ewm-input--sync-keyboard-capture)
  (remove-hook 'post-command-hook #'ewm-input--sync-drag-source)
  (remove-variable-watcher 'track-mouse #'ewm-input--track-mouse-watcher)
  (ewm-input--set-drag-source nil)
  (remove-hook 'minibuffer-setup-hook #'ewm-input--keyboard-capture-on)
  (remove-hook 'minibuffer-exit-hook #'ewm-input--sync-keyboard-capture)
  (remove-hook 'window-selection-change-functions #'ewm-input--on-window-selection-change)
  (ewm-input--set-read-function-capture nil)
  (ewm-input--set-keyboard-capture nil t)
  (define-key special-event-map [switch-frame] nil)
  (define-key global-map [ewm-focus-change] nil)
  (setq-default input-method-function #'list)
  (advice-remove 'quail-input-method #'ewm-input--in-selected-window)
  (advice-remove 'define-key #'ewm-input--on-define-key)
  (ewm-input--cancel-resend))

;;; Key scanning and interception

(defun ewm--live-binding-p (binding)
  "Return non-nil when BINDING should be intercepted."
  (not (or (null binding)
           (numberp binding)
           (memq binding '(ignore undefined)))))

(defun ewm--event-mode-binding (event)
  "Return EVENT's `ewm-mode-map' binding, or nil."
  (let ((binding (lookup-key ewm-mode-map (vector event))))
    (and (ewm--live-binding-p binding) binding)))

(defun ewm--event-dispatch (event &optional intercept-prefix-entry)
  "Return the compositor dispatch mode for EVENT.
Keymap prefixes and plain `ewm-intercept-prefixes' entries need keyboard
delivery.  `ewm-mode-map' command bindings use command-event delivery so
they can repeat without moving Wayland keyboard focus; if they enter a
minibuffer/transient/read-key state, Lisp enables keyboard capture at
runtime."
  (let ((mode-binding (ewm--event-mode-binding event)))
    (cond
     ((keymapp mode-binding) "keyboard")
     (mode-binding "command")
     ((keymapp (key-binding (vector event) t)) "keyboard")
     (intercept-prefix-entry "keyboard")
     (t "command"))))

(defun ewm--event-repeat-p (event)
  "Return non-nil when EVENT's intercepted command should repeat."
  (or (member (key-description (vector event))
              ewm-input--repeatable-key-descriptions)
      (memq (ewm--event-mode-binding event) ewm-repeatable-commands)))

(defun ewm--event-fullscreen-p (event)
  "Return non-nil when EVENT's command is intercepted under fullscreen."
  (function-get (ewm--event-mode-binding event) 'ewm-fullscreen))

(defun ewm--event-key-spec (event)
  "Return a (:key VALUE modifier-flags) plist for EVENT, or nil.
VALUE is the ASCII code or special-key name; flags are t or nil."
  (let* ((mods (event-modifiers event))
         (base (event-basic-type event))
         (key-value (cond ((integerp base) base)
                          ((symbolp base) (symbol-name base)))))
    (when key-value
      (list :key key-value
            :ctrl (and (memq 'control mods) t)
            :alt (and (memq 'meta mods) t)
            :shift (and (memq 'shift mods) t)
            :super (and (memq 'super mods) t)))))

(defun ewm--key-event (key)
  "Return the event KEY names, an integer or a one-key description, else nil."
  (cond ((integerp key) key)
        ((stringp key)
         (let ((seq (key-parse key)))
           (and (= (length seq) 1) (aref seq 0))))))

(defun ewm--intercept-spec (event &rest props)
  "Return the compositor intercept spec plist for EVENT with PROPS, or nil."
  (when-let* ((key-spec (ewm--event-key-spec event)))
    `(,@key-spec :description ,(key-description (vector event)) ,@props)))

(defun ewm--event-action (event)
  "The compositor action EVENT's `ewm-mode-map' command declares, or nil."
  (let ((binding (ewm--event-mode-binding event)))
    (when-let* (((symbolp binding))
                (action (function-get binding 'ewm-action)))
      (if (functionp action) (funcall action event) action))))

(defun ewm--event-to-intercept-spec (event &optional fullscreen dispatch)
  "Return EVENT's compositor intercept spec plist, or nil."
  (ewm--intercept-spec event
                       :fullscreen (and fullscreen t)
                       :dispatch (or dispatch (ewm--event-dispatch event))
                       :repeat (and (ewm--event-repeat-p event) t)
                       :action (ewm--event-action event)))

(defun ewm--parse-intercept-entry (entry)
  "Parse ENTRY from `ewm-intercept-prefixes' into (event . fullscreen-p), or nil."
  (let* ((fullscreen (and (listp entry) (memq :fullscreen entry)))
         (event (ewm--key-event (if fullscreen (car entry) entry))))
    (and event (cons event (and fullscreen t)))))

(defun ewm--emulation-specs (table &optional surface)
  "Return command intercepts for TABLE's key entries, scoped to SURFACE when given.
Every entry also runs as `ewm-surface-emulate' in surface buffers."
  (cl-loop for (from . _) in table
           for event = (ewm--key-event from)
           if (symbolp from)
           do (define-key ewm-surface-mode-map (vector 'remap from) #'ewm-surface-emulate)
           else if event
           do (define-key ewm-surface-mode-map (vector event) #'ewm-surface-emulate)
           and collect (ewm--intercept-spec event :dispatch "command" :repeat t
                                            :surface surface)
           else
           do (display-warning
               'ewm (format "ewm-surface-emulate-keys: %S is not a single key or command" from))))

(defun ewm--emulation-tables ()
  "Return (SURFACE . TABLE) for each surface's own table, then (nil . default)."
  (nconc (cl-loop for buf being the hash-values of ewm--surfaces using (hash-keys surface)
                  when (and (buffer-live-p buf)
                            (local-variable-p 'ewm-surface-emulate-keys buf))
                  collect (cons surface (buffer-local-value 'ewm-surface-emulate-keys buf)))
         (list (cons nil (default-value 'ewm-surface-emulate-keys)))))

(defun ewm--mode-map-events ()
  "Single-key events bound to live commands in `ewm-mode-map'."
  (let (events)
    (map-keymap
     (lambda (event binding)
       ;; Meta keys live under the ESC prefix, so descend and re-apply meta.
       (if (and (eq event meta-prefix-char) (keymapp binding))
           (map-keymap
            (lambda (ev b)
              (when (and (not (keymapp b)) (ewm--live-binding-p b))
                (push (event-apply-modifier ev 'meta 27 "M-") events)))
            binding)
         (when (ewm--live-binding-p binding)
           (push event events))))
     ewm-mode-map)
    (nreverse events)))

(defun ewm--send-intercept-keys ()
  "Send emulations, prefix and `ewm-mode-map' keys to the compositor.
Emulations go first, since the compositor takes the first match."
  (ewm-input--cancel-resend)
  (ewm-input--install-xf86-key-aliases)
  (let* ((prefixes (seq-keep #'ewm--parse-intercept-entry ewm-intercept-prefixes))
         (fullscreen-events (mapcar #'car (cl-remove-if-not #'cdr prefixes)))
         specs)
    (dolist (event (delete-dups (append (mapcar #'car prefixes)
                                       (ewm--mode-map-events))))
      (when-let* ((spec (ewm--event-to-intercept-spec
                         event
                         (or (memq event fullscreen-events) (ewm--event-fullscreen-p event))
                         (ewm--event-dispatch event (assq event prefixes)))))
        (push spec specs)))
    (ewm-intercept-keys-module
     (vconcat (cl-loop for (surface . table) in (ewm--emulation-tables)
                       nconc (ewm--emulation-specs table surface))
              (nreverse specs)))))

(defun ewm-input--on-define-key (keymap &optional _key _def _remove)
  "Resend the intercept list once idle after a change to `ewm-mode-map'."
  (when (and (eq keymap ewm-mode-map) (null ewm-input--resend-timer))
    (setq ewm-input--resend-timer
          (run-with-idle-timer 0 nil #'ewm--send-intercept-keys))))

(defun ewm--handle-intercepted-command (event)
  "Queue intercepted EVENT for normal Emacs command processing."
  (when-let* ((key (alist-get 'key event))
              ((stringp key)))
    ;; These are new keystrokes: record them for command keys and macros.
    ;; Append so multiple events received in one pipe read retain their order.
    (setq unread-command-events
          (nconc unread-command-events
                 (mapcar (lambda (event) (cons t event)) (key-parse key))))))

(provide 'ewm-input)
;;; ewm-input.el ends here
