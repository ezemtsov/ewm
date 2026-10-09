;;; ewm-input.el --- Input handling for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Input handling for EWM including key interception, mouse-follows-focus,
;; and keyboard layout configuration.
;;
;; The compositor intercepts keys from three sources:
;;
;; 1. `ewm-mode-map' - All bindings in this keymap are intercepted.
;;    Override defaults with use-package :bind (:map ewm-mode-map ...).
;; 2. `ewm-intercept-prefixes' - Extra keys to intercept, plus keys that
;;    should also be intercepted while a client is fullscreen.
;; 3. `ewm-surface-emulate-keys' - Key entries are intercepted as commands
;;    and `ewm-surface-emulate' sends the mapped keys to the client.
;;
;; Example:
;;   (use-package ewm
;;     :bind (:map ewm-mode-map ("s-d" . consult-buffer)))

;;; Code:

(require 'cl-lib)
(require 'windmove)

(defmacro ewm--cmd (&rest args)
  "Create an interactive command that runs ARGS as a subprocess."
  `(lambda () (interactive) (start-process ,(car args) nil ,@args)))

(defvar ewm-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "<XF86WakeUp>") #'ignore)
    (define-key map (kbd "C-M-<XF86WakeUp>") #'ignore)
    ;; Media/hardware keys
    (define-key map (kbd "<AudioRaiseVolume>")  (ewm--cmd "wpctl" "set-volume" "-l" "1.0" "@DEFAULT_AUDIO_SINK@" "5%+"))
    (define-key map (kbd "<AudioLowerVolume>")  (ewm--cmd "wpctl" "set-volume" "@DEFAULT_AUDIO_SINK@" "5%-"))
    (define-key map (kbd "<AudioMute>")         (ewm--cmd "wpctl" "set-mute" "@DEFAULT_AUDIO_SINK@" "toggle"))
    (define-key map (kbd "<AudioMicMute>")      (ewm--cmd "wpctl" "set-mute" "@DEFAULT_AUDIO_SOURCE@" "toggle"))
    (define-key map (kbd "<MonBrightnessUp>")   (ewm--cmd "brightnessctl" "set" "5%+"))
    (define-key map (kbd "<MonBrightnessDown>") (ewm--cmd "brightnessctl" "set" "5%-"))
    ;; Window navigation; crosses outputs at the frame edge.
    (define-key map (kbd "s-<left>") #'ewm-focus-left)
    (define-key map (kbd "s-<right>") #'ewm-focus-right)
    (define-key map (kbd "s-<up>") #'ewm-focus-up)
    (define-key map (kbd "s-<down>") #'ewm-focus-down)
    ;; Workspace switching on the current output's strip.
    (define-key map (kbd "s-S-<left>") #'ewm-frame-left)
    (define-key map (kbd "s-S-<right>") #'ewm-frame-right)
    ;; Overview: zoom the strip out and back in.
    (define-key map (kbd "s-S-<up>") #'ewm-overview-open)
    (define-key map (kbd "s-S-<down>") #'ewm-overview-close)
    ;; Slide every frame off screen and back.
    (define-key map (kbd "s-m") #'ewm-hide-toggle)
    ;; Clipboard
    (define-key map (kbd "s-c") #'kill-ring-save)
    (define-key map (kbd "s-v") #'yank)
    ;; Application launcher
    (define-key map (kbd "s-d") #'ewm-launch-app)
    ;; Fullscreen toggle
    (define-key map (kbd "s-f") #'ewm-toggle-fullscreen)
    ;; Floating detach/attach toggle
    (define-key map (kbd "s-S-SPC") #'ewm-floating-toggle)
    ;; Lock session
    (define-key map (kbd "s-l") #'ewm-lock-session)
    ;; Edit the focused text field
    (define-key map (kbd "s-e") #'ewm-edit)
    ;; Surface buffer cycling
    (define-key map (kbd "s-<tab>") #'ewm-next-surface-buffer)
    ;; Emacs sees S-Tab as iso-lefttab; compositor sees Tab+Shift.  Need both.
    (define-key map (kbd "s-<iso-lefttab>") #'ewm-prev-surface-buffer)
    (define-key map (kbd "s-S-<tab>") #'ewm-prev-surface-buffer)
    ;; Frame strip
    (define-key map (kbd "s-t") #'ewm-frame-new)
    (define-key map (kbd "s-w") #'ewm-frame-close)
    ;; Reorder frames horizontally; reorder windows within a frame vertically.
    (define-key map (kbd "C-s-<left>") #'ewm-frame-move-left)
    (define-key map (kbd "C-s-<right>") #'ewm-frame-move-right)
    (define-key map (kbd "C-s-<up>") #'windmove-swap-states-up)
    (define-key map (kbd "C-s-<down>") #'windmove-swap-states-down)
    ;; Select Nth frame on current output's strip.
    (dotimes (i 9)
      (define-key map (kbd (format "s-%d" (1+ i))) #'ewm-frame-select))
    map)
  "Keymap for `ewm-mode'.
Default super-key bindings for window management.
Override individual bindings with `use-package':
  :bind (:map ewm-mode-map (\"s-d\" . my-launcher))")

(declare-function ewm-intercept-keys-module "ewm-core")
(declare-function ewm-send-key-module "ewm-core" (surface-id keys))
(declare-function ewm-keyboard-capture-active-module "ewm-core")
(declare-function ewm-configure-input-module "ewm-core")
(declare-function ewm-keyboard-capture-begin-module "ewm-core" (reason))
(declare-function ewm-keyboard-capture-end-module "ewm-core" (token))
(declare-function ewm-keyboard-redirect-capture-clear-module "ewm-core")
(declare-function ewm-set-drag-source "ewm-core" (active))
(declare-function ewm-get-focused-id "ewm-core")
(declare-function ewm-get-focused-frame-id "ewm-core")
(declare-function ewm-get-pointer-location "ewm-core")
(declare-function ewm-layout--menu-active-p "ewm-layout")
(declare-function ewm--window-body-pixel-rect "ewm-layout" (window))
(declare-function ewm-warp-pointer "ewm")
(declare-function ewm--get-frame-origin "ewm")
;; Defined in ewm.el (loaded after ewm-input.el)
(declare-function ewm-launch-app "ewm")
(declare-function ewm-toggle-fullscreen "ewm")
(declare-function ewm-floating-toggle "ewm")
(declare-function ewm-lock-session "ewm")
(declare-function ewm-edit "ewm-text-input")
(declare-function ewm-frame-new "ewm")
(declare-function ewm-frame-close "ewm")
(declare-function ewm-frame-select "ewm")
(declare-function ewm-frame-left "ewm")
(declare-function ewm-frame-right "ewm")
(declare-function ewm-overview-open "ewm")
(declare-function ewm-overview-close "ewm")
(declare-function ewm-hide-toggle "ewm")
(declare-function ewm-frame-move-left "ewm")
(declare-function ewm-frame-move-right "ewm")
(declare-function ewm-focus-left "ewm")
(declare-function ewm-focus-right "ewm")
(declare-function ewm-focus-up "ewm")
(declare-function ewm-focus-down "ewm")

(defvar ewm--module-mode)
(defvar ewm-mouse-follows-focus)
(defvar ewm--mff-last-window)
(defvar ewm--surfaces)
(defvar ewm-surface-id)
(defvar ewm-surface-mode-map)
(defvar ewm-text-input--composing)

(defgroup ewm-input nil
  "EWM input handling."
  :group 'ewm)

(defun ewm-input--set-input-config (sym val)
  "Set SYM to VAL and refresh compositor input config."
  (set-default sym val)
  (when (bound-and-true-p ewm--module-mode)
    (ewm--send-input-config)))

(defun ewm-input--set-intercept-config (sym val)
  "Set SYM to VAL and refresh compositor key interception."
  (set-default sym val)
  (ewm--send-intercept-keys))

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
  :initialize 'custom-initialize-default
  :set #'ewm-input--set-input-config
  :group 'ewm-input)

(defun ewm--send-input-config ()
  "Send input device configuration to compositor."
  (when (bound-and-true-p ewm--module-mode)
    (ewm-configure-input-module
     (vconcat
      (mapcar (lambda (entry)
                (let ((key (car entry))
                      (props (cdr entry)))
                  (append (if (symbolp key)
                              (list :type (symbol-name key))
                            (list :device key))
                          props)))
              ewm-input-config)))))

;;; Key interception

(define-widget 'ewm-key 'lazy
  "A single key, as an integer event or a key description."
  :tag "Key"
  :type '(choice character integer string)
  :match (lambda (_ key) (ewm--key-event key)))

(defcustom ewm-intercept-prefixes
  '(?\C-x ?\C-u ?\C-h ?\M-x (?\s-f :fullscreen)
          ("s-S-<left>" :fullscreen) ("s-S-<right>" :fullscreen)
          ("s-S-<up>" :fullscreen) ("s-S-<down>" :fullscreen) ("s-m" :fullscreen)
          ("<MonBrightnessUp>" :fullscreen) ("<MonBrightnessDown>" :fullscreen)
          ("<AudioRaiseVolume>" :fullscreen) ("<AudioLowerVolume>" :fullscreen)
          ("<AudioMute>" :fullscreen) ("<AudioMicMute>" :fullscreen)
          ("<Print>" :fullscreen))
  "Keys that always go to Emacs, even when a surface has focus.
Each entry is a key or (key :fullscreen), where a key is an integer
event such as ?\\M-x or a key description naming one key. :fullscreen
entries are intercepted even when a fullscreen surface has focus.

Bindings in `ewm-mode-map' are always intercepted (without :fullscreen)
and use command-event delivery. Other keys enter the Emacs keyboard path.

Set with `setopt' (or `:custom' in `use-package'):

  (setopt ewm-intercept-prefixes
          \\='(\"C-x\" \"C-u\" \"C-h\" \"M-x\" \"M-`\"
            (\"<Print>\" :fullscreen)))"
  :type '(repeat (choice ewm-key (list ewm-key (const :fullscreen))))
  :initialize 'custom-initialize-default
  :set #'ewm-input--set-intercept-config
  :group 'ewm-input)

(defcustom ewm-surface-emulate-keys
  '((kill-ring-save . "C-c")
    (yank . "C-v"))
  "Keys a client receives in place of what was typed, as (FROM . KEYS) entries.
FROM is a command or a single key, KEYS a key description Emacs sends with
`ewm-send-key' instead. A surface buffer's local value adds entries for
that surface only, see `ewm-update-app-hook'."
  :type '(alist :key-type (choice function ewm-key) :value-type string)
  :initialize 'custom-initialize-default
  :set #'ewm-input--set-intercept-config
  :group 'ewm-input)

(defcustom ewm-repeatable-commands
  '(ewm-focus-left ewm-focus-right ewm-focus-up ewm-focus-down
    ewm-frame-left ewm-frame-right
    ewm-frame-move-left ewm-frame-move-right
    windmove-swap-states-up windmove-swap-states-down
    ewm-next-surface-buffer ewm-prev-surface-buffer)
  "Commands whose intercepted key bindings repeat while held."
  :type '(repeat function)
  :initialize 'custom-initialize-default
  :set #'ewm-input--set-intercept-config
  :group 'ewm-input)

(defconst ewm-input--repeatable-key-descriptions
  '("<AudioRaiseVolume>" "<AudioLowerVolume>" "<AudioMute>" "<AudioMicMute>"
    "<MonBrightnessUp>" "<MonBrightnessDown>")
  "Special key descriptions that should repeat despite anonymous bindings.")

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

(defun ewm-input--pointer-in-window-p (window)
  "Return non-nil if pointer is inside WINDOW.
Coordinates are in compositor space."
  (let* ((frame (window-frame window))
         (frame-origin (ewm--get-frame-origin frame))
         (pointer (ewm-get-pointer-location)))
    (eq window (window-at-x-y (- (car pointer) (car frame-origin))
                               (- (cdr pointer) (cdr frame-origin))
                               frame))))

(defun ewm-input--warp-pointer-to-window (window)
  "Warp pointer to center of WINDOW.
Does nothing if pointer is already inside the window or if it's a minibuffer."
  (unless (or (window-minibuffer-p window)
              (ewm-input--pointer-in-window-p window))
    (when-let* ((frame (window-frame window))
                (frame-origin (ewm--get-frame-origin frame)))
      (pcase-let ((`(,x ,y ,width ,height) (ewm--window-body-pixel-rect window)))
        (ewm-warp-pointer (float (+ (car frame-origin) x (/ width 2)))
                          (float (+ (cdr frame-origin) y (/ height 2))))))))

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
             (bound-and-true-p ewm--module-mode))
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
  (and (not (bound-and-true-p ewm-text-input--composing))
       (or (not (bound-and-true-p ewm--module-mode))
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
            (if (bound-and-true-p ewm--module-mode)
                (ewm-keyboard-capture-begin-module "command-loop")
              0)))
     ((and (not enabled) token)
      (setq ewm-input--keyboard-capture-token nil)
      (when (and (bound-and-true-p ewm--module-mode) (> token 0))
        (ewm-keyboard-capture-end-module token))))
    (when (and clear-redirect (not enabled) (bound-and-true-p ewm--module-mode))
      (ewm-keyboard-redirect-capture-clear-module)
      (dolist (send (nreverse ewm--pending-keys))
        (apply #'ewm-send-key-module send))
      (setq ewm--pending-keys nil))))

(defun ewm-input--sync-keyboard-capture ()
  "Synchronize compositor keyboard capture with current Emacs input state."
  (let ((capture-active
         (and (ewm-input--modal-input-active-p)
              (or ewm-input--keyboard-capture-token
                  (ewm-input--keyboard-capture-start-allowed-p)))))
    (ewm-input--set-keyboard-capture capture-active (not capture-active))))

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
  (ewm-input--keyboard-capture-on)
  (unwind-protect
      (apply orig-fun args)
    (ewm-input--sync-keyboard-capture)))

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

;;; Frame-switch suppression
;;
;; PGTK delivers GDK focus changes as `switch-frame' events.  In stock
;; Emacs these are bound to `handle-switch-frame' in `global-map', so they
;; fan out through the command loop -- firing `post-command-hook' between
;; the keys of an in-flight prefix sequence and clobbering EWM's keyboard
;; capture state.  Route them to `special-event-map' instead (alongside
;; `focus-in'/`focus-out'), where they're consumed without entering the
;; command loop.  EWM owns frame selection via `ewm--handle-focus'.

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
    (if (or (bound-and-true-p ewm-text-input--composing) (eq buf (current-buffer)))
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
  (define-key special-event-map [switch-frame] #'ignore)
  (setq-default input-method-function
                (apply-partially #'ewm-input--in-selected-window #'list))
  (advice-add 'quail-input-method :around #'ewm-input--in-selected-window))

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
  (setq-default input-method-function #'list)
  (advice-remove 'quail-input-method #'ewm-input--in-selected-window))

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

(defun ewm--event-key-spec (event)
  "Return a (:key VALUE modifier-flags) plist for EVENT, or nil.
VALUE is the ASCII code or special-key name; flags are t or nil."
  (let* ((mods (event-modifiers event))
         (base (event-basic-type event))
         ;; base is either an integer (ASCII) or a symbol (special key)
         (key-value (cond
                     ((integerp base) base)
                     ((symbolp base) (symbol-name base))
                     (t nil))))
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

(defun ewm--event-to-intercept-spec (event &optional fullscreen dispatch)
  "Return EVENT's compositor intercept spec plist, or nil."
  (ewm--intercept-spec event
                       :fullscreen (and fullscreen t)
                       :dispatch (or dispatch (ewm--event-dispatch event))
                       :repeat (and (ewm--event-repeat-p event) t)))

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
  (when (bound-and-true-p ewm--module-mode)
    (ewm-input--install-xf86-key-aliases)
    (let* ((prefixes (delq nil (mapcar #'ewm--parse-intercept-entry
                                      ewm-intercept-prefixes)))
           (fullscreen-events (mapcar #'car (cl-remove-if-not #'cdr prefixes)))
           specs)
      (dolist (event (delete-dups (append (mapcar #'car prefixes)
                                         (ewm--mode-map-events))))
        (when-let* ((spec (ewm--event-to-intercept-spec
                           event
                           (memq event fullscreen-events)
                           (ewm--event-dispatch event (assq event prefixes)))))
          (push spec specs)))
      (ewm-intercept-keys-module
       (vconcat (cl-loop for (surface . table) in (ewm--emulation-tables)
                         nconc (ewm--emulation-specs table surface))
                (nreverse specs))))))

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
