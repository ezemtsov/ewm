;;; ewm-input.el --- Input handling for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Input handling for EWM including key interception, mouse-follows-focus,
;; and keyboard layout configuration.
;;
;; The compositor intercepts keys from two sources:
;;
;; 1. `ewm-mode-map' - Every binding is intercepted, those of commands with
;;    an `ewm-action' property run in the compositor. `ewm-key' flags a
;;    binding: :repeat while held, :fullscreen over a fullscreen window, and
;;    :intercept for an Emacs key such as C-x that keeps its own meaning but
;;    reaches Emacs while an application has focus. Override defaults with
;;    use-package :bind (:map ewm-mode-map ...). Touchpad swipes bind as
;;    `<swipe-FINGERS-DIRECTION>' keys and scrolls as `<wheel-DIRECTION>'
;;    keys, with the usual modifier prefixes.
;; 2. `ewm-surface-emulate-keys' - Key entries are intercepted as commands
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

(defconst ewm-input--flags '(:repeat :fullscreen :intercept)
  "The flags `ewm-key' takes.")

(defun ewm-key (&rest args)
  "A binding for `ewm-mode-map': a command followed by flags, or flags alone.
Flags are present or absent. :repeat repeats the key while it is held,
:fullscreen keeps it intercepted over a fullscreen window, and :intercept,
with no command, lets an Emacs key such as C-x or M-x keep its own meaning
while reaching Emacs even when an application has focus.

  (keymap-set ewm-mode-map \"s-=\" (ewm-key #\='ewm-volume-raise :repeat))
  (keymap-set ewm-mode-map \"C-x\" (ewm-key :intercept))"
  (let ((command (and (not (keywordp (car args))) (pop args))))
    (dolist (flag args)
      (unless (memq flag ewm-input--flags)
        (error "ewm-key: %S is not one of %S" flag ewm-input--flags)))
    (cond ((and command (memq :intercept args))
           (error "ewm-key: %S and :intercept cannot both apply" command))
          ((not (or command (memq :intercept args)))
           (error "ewm-key: a command or :intercept is needed")))
    (if (or args (null command)) `(menu-item "" ,command :ewm ,args) command)))

;; Commands that repeat on any key they are bound to.
(defvar-keymap ewm-mode-map
  :doc "Keymap for `ewm-mode'.
Default super-key bindings for window management, see `ewm-key' for the
flags a binding takes. Add or override bindings in the same shape:
  (define-keymap :keymap ewm-mode-map \"s-d\" #\\='my-launcher)"
  "<XF86WakeUp>" #'ignore
  "C-M-<XF86WakeUp>" #'ignore
  ;; Media/hardware keys
  "<AudioRaiseVolume>" (ewm-key #'ewm-volume-raise :fullscreen :repeat)
  "<AudioLowerVolume>" (ewm-key #'ewm-volume-lower :fullscreen :repeat)
  "<AudioMute>" (ewm-key #'ewm-volume-mute :fullscreen :repeat)
  "<AudioMicMute>" (ewm-key #'ewm-mic-mute :fullscreen :repeat)
  "<MonBrightnessUp>" (ewm-key #'ewm-brightness-up :fullscreen :repeat)
  "<MonBrightnessDown>" (ewm-key #'ewm-brightness-down :fullscreen :repeat)
  ;; Window navigation; crosses outputs at the frame edge.
  "s-<left>" (ewm-key #'ewm-focus-left :repeat)
  "s-<right>" (ewm-key #'ewm-focus-right :repeat)
  "s-<up>" (ewm-key #'ewm-focus-up :repeat)
  "s-<down>" (ewm-key #'ewm-focus-down :repeat)
  ;; Workspace switching on the current output's strip.
  "S-s-<left>" (ewm-key #'ewm-frame-left :fullscreen :repeat)
  "S-s-<right>" (ewm-key #'ewm-frame-right :fullscreen :repeat)
  ;; Overview: zoom the strip out and back in.
  "S-s-<up>" (ewm-key #'ewm-overview-open :fullscreen)
  "S-s-<down>" (ewm-key #'ewm-overview-close :fullscreen)
  ;; Touchpad swipes follow the fingers: up and down drive the overview, a swipe
  ;; left brings in the frame to the right.
  "<swipe-3-up>" (ewm-key #'ewm-overview-open :fullscreen)
  "<swipe-3-down>" (ewm-key #'ewm-overview-close :fullscreen)
  "<swipe-3-left>" (ewm-key #'ewm-frame-right :fullscreen)
  "<swipe-3-right>" (ewm-key #'ewm-frame-left :fullscreen)
  ;; Slide every frame off screen and back.
  "s-m" (ewm-key #'ewm-hide-toggle :fullscreen)
  ;; Clipboard
  "s-c" #'kill-ring-save
  "s-v" #'yank
  ;; Application launcher
  "s-d" #'ewm-launch-app
  ;; Fullscreen toggle
  "s-f" (ewm-key #'ewm-toggle-fullscreen :fullscreen)
  ;; Floating detach/attach toggle
  "S-s-SPC" #'ewm-floating-toggle
  ;; Lock session
  "s-l" (ewm-key #'ewm-lock-session :fullscreen)
  ;; Edit the focused text field
  "s-e" #'ewm-edit
  ;; Surface buffer cycling
  "s-<tab>" (ewm-key #'ewm-next-surface-buffer :repeat)
  ;; Emacs sees S-Tab as iso-lefttab; compositor sees Tab+Shift.  Need both.
  "s-<iso-lefttab>" (ewm-key #'ewm-prev-surface-buffer :repeat)
  "S-s-<tab>" (ewm-key #'ewm-prev-surface-buffer :repeat)
  ;; Frame strip
  "s-t" #'ewm-frame-new
  "s-w" #'ewm-frame-close
  ;; Reorder frames horizontally; reorder windows within a frame vertically.
  "C-s-<left>" (ewm-key #'ewm-frame-move-left :fullscreen :repeat)
  "C-s-<right>" (ewm-key #'ewm-frame-move-right :fullscreen :repeat)
  "C-s-<up>" (ewm-key #'windmove-swap-states-up :repeat)
  "C-s-<down>" (ewm-key #'windmove-swap-states-down :repeat)
  ;; Select Nth frame on current output's strip.
  "s-1" (ewm-key #'ewm-frame-select :fullscreen)
  "s-2" (ewm-key #'ewm-frame-select :fullscreen)
  "s-3" (ewm-key #'ewm-frame-select :fullscreen)
  "s-4" (ewm-key #'ewm-frame-select :fullscreen)
  "s-5" (ewm-key #'ewm-frame-select :fullscreen)
  "s-6" (ewm-key #'ewm-frame-select :fullscreen)
  "s-7" (ewm-key #'ewm-frame-select :fullscreen)
  "s-8" (ewm-key #'ewm-frame-select :fullscreen)
  "s-9" (ewm-key #'ewm-frame-select :fullscreen)
  ;; Emacs keys that win over an application, keeping their own meaning.
  "C-x" (ewm-key :intercept)
  "C-u" (ewm-key :intercept)
  "C-h" (ewm-key :intercept)
  "M-x" (ewm-key :intercept)
  "<Print>" (ewm-key :intercept :fullscreen))

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

(defun ewm--binding-entry (binding)
  "BINDING as (COMMAND . FLAGS), the flags from `ewm-key', nil for a dead binding."
  (pcase binding
    (`(menu-item ,_ ,command . ,props)
     (let ((flags (plist-get props :ewm)))
       (and (or (memq :intercept flags) (ewm--live-binding-p command))
            (cons command flags))))
    ((pred ewm--live-binding-p) (cons binding nil))))

(defun ewm--mode-map-entries ()
  "Every intercepted single key of `ewm-mode-map' as (EVENT COMMAND . FLAGS)."
  (let (entries)
    (map-keymap
     (lambda (event binding)
       ;; Meta keys live under the ESC prefix, so descend and re-apply meta.
       (if (and (eq event meta-prefix-char) (keymapp binding))
           (map-keymap
            (lambda (ev b)
              (when-let* (((not (keymapp b))) (entry (ewm--binding-entry b)))
                (push (cons (event-apply-modifier ev 'meta 27 "M-") entry) entries)))
            binding)
         (when-let* ((entry (ewm--binding-entry binding)))
           (push (cons event entry) entries))))
     ewm-mode-map)
    (nreverse entries)))

(defun ewm--event-entry (event)
  "EVENT's (COMMAND . FLAGS) in `ewm-mode-map', or nil."
  (cdr (assq event (ewm--mode-map-entries))))

(defun ewm--event-dispatch (event command &optional intercept)
  "Return the compositor dispatch mode for EVENT, bound to COMMAND.
Keymap prefixes and intercepted Emacs keys need keyboard delivery.
`ewm-mode-map' command bindings use command-event delivery so they can
repeat without moving Wayland keyboard focus; if they enter a
minibuffer/transient/read-key state, Lisp enables keyboard capture at
runtime."
  (cond
   ((keymapp command) "keyboard")
   (command "command")
   ((keymapp (key-binding (vector event) t)) "keyboard")
   (intercept "keyboard")
   (t "command")))

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

(defun ewm--event-action (event command)
  "The compositor action COMMAND declares for EVENT, or nil."
  (when-let* (((symbolp command))
              (action (function-get command 'ewm-action)))
    (if (functionp action) (funcall action event) action)))

(defun ewm--event-to-intercept-spec (event &optional entry)
  "Return EVENT's compositor intercept spec plist, or nil.
ENTRY is its (COMMAND . FLAGS) in `ewm-mode-map', looked up when omitted."
  (pcase-let ((`(,command . ,flags) (or entry (ewm--event-entry event))))
    (ewm--intercept-spec event
                         :fullscreen (and (or (memq :fullscreen flags)
                                              (function-get command 'ewm-fullscreen))
                                          t)
                         :dispatch (ewm--event-dispatch event command (memq :intercept flags))
                         :repeat (and (memq :repeat flags) t)
                         :action (ewm--event-action event command))))

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

(defun ewm--send-intercept-keys ()
  "Send emulations and `ewm-mode-map' keys to the compositor.
Emulations go first, since the compositor takes the first match."
  (ewm-input--cancel-resend)
  (ewm-input--install-xf86-key-aliases)
  (ewm-intercept-keys-module
   (vconcat (cl-loop for (surface . table) in (ewm--emulation-tables)
                     nconc (ewm--emulation-specs table surface))
            (seq-keep (lambda (item) (ewm--event-to-intercept-spec (car item) (cdr item)))
                      (ewm--mode-map-entries)))))

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
