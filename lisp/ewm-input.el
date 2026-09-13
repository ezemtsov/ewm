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
;; 2. `ewm-intercept-prefixes' - Extra keys to intercept, plus keys that
;;    should also be intercepted while a client is fullscreen.
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
(declare-function ewm-configure-input-module "ewm-core")
(declare-function ewm-keyboard-capture-begin-module "ewm-core" (reason))
(declare-function ewm-keyboard-capture-end-module "ewm-core" (token))
(declare-function ewm-keyboard-redirect-capture-clear-module "ewm-core")
(declare-function ewm-set-drag-source "ewm-core" (active))
(declare-function ewm-get-focused-id "ewm-core")
(declare-function ewm-get-focused-frame-id "ewm-core")
(declare-function ewm-get-pointer-location "ewm-core")
(declare-function ewm-layout--menu-active-p "ewm-layout")
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
(declare-function ewm-frame-move-left "ewm")
(declare-function ewm-frame-move-right "ewm")
(declare-function ewm-focus-left "ewm")
(declare-function ewm-focus-right "ewm")
(declare-function ewm-focus-up "ewm")
(declare-function ewm-focus-down "ewm")

(defvar ewm--module-mode)
(defvar ewm-mouse-follows-focus)
(defvar ewm--mff-last-window)

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
  (when (bound-and-true-p ewm--module-mode)
    (ewm--send-intercept-keys)))

;;; Libinput device configuration

(defcustom ewm-input-config nil
  "Input device configuration alist.
Each entry is (KEY . PROPS) where KEY is:
  - A symbol for type defaults: `touchpad', `mouse', `trackball',
    `trackpoint', `keyboard', `tablet'
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

(defcustom ewm-intercept-prefixes
  '(?\C-x ?\C-u ?\C-h ?\M-x (?\s-f :fullscreen)
          ("s-S-<left>" :fullscreen) ("s-S-<right>" :fullscreen)
          ("<MonBrightnessUp>" :fullscreen) ("<MonBrightnessDown>" :fullscreen)
          ("<AudioRaiseVolume>" :fullscreen) ("<AudioLowerVolume>" :fullscreen)
          ("<AudioMute>" :fullscreen) ("<AudioMicMute>" :fullscreen)
          ("<Print>" :fullscreen))
  "Keys that always go to Emacs, even when a surface has focus.
Each entry is a key or (key :fullscreen).  Plain keys are intercepted
normally; :fullscreen keys are also sent to Emacs when a fullscreen
surface has focus.  Keys can be character literals or strings.  Plain
entries enter the Emacs keyboard path; entries that are also bound in
`ewm-mode-map' use command-event delivery.

Bindings in `ewm-mode-map' are always intercepted (without :fullscreen).

Set with `setopt' (or `:custom' in `use-package'):

  (setopt ewm-intercept-prefixes
          \\='(\"C-x\" \"C-u\" \"C-h\" \"M-x\" \"M-`\"
            (\"<Print>\" :fullscreen)))"
  :type '(repeat (choice character string
                         (list (choice character string) (const :fullscreen))))
  :initialize 'custom-initialize-default
  :set #'ewm-input--set-intercept-config
  :group 'ewm-input)

(defcustom ewm-surface-emulate-keys
  '((?\s-c . "C-c")
    (?\s-v . "C-v"))
  "Key bindings translated to a different key for non-Emacs surfaces.
Each entry is (KEY-EVENT . TARGET) where TARGET is a key description
\(e.g. \"C-c\", \"<down>\", \"M-S-c\").

When a non-Emacs Wayland surface has focus, the matched key is delivered
to it as TARGET instead of going to Emacs (e.g. s-c becomes C-c for
clipboard copy in Firefox, or C-j becomes <down>).  When Emacs has focus,
the original binding from `ewm-mode-map' is used."
  :type '(alist :key-type character :value-type string)
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
         (edges (window-pixel-edges window))
         (left (+ (car frame-origin) (nth 0 edges)))
         (top (+ (cdr frame-origin) (nth 1 edges)))
         (right (+ (car frame-origin) (nth 2 edges)))
         (bottom (+ (cdr frame-origin) (nth 3 edges)))
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
                (frame-origin (ewm--get-frame-origin frame)))
      (let* ((edges (window-inside-pixel-edges window))
             (x (+ (car frame-origin) (/ (+ (nth 0 edges) (nth 2 edges)) 2)))
             (y (+ (cdr frame-origin) (/ (+ (nth 1 edges) (nth 3 edges)) 2))))
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
  (or (not (bound-and-true-p ewm--module-mode))
      (ewm-input--emacs-frame-focused-p)
      (ewm-input--command-key-active-p)
      (ewm-input--prefix-key-active-p)))

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
      (ewm-keyboard-redirect-capture-clear-module))))

(defun ewm-input--sync-keyboard-capture ()
  "Synchronize compositor keyboard capture with current Emacs input state."
  (let ((capture-active
         (and (ewm-input--modal-input-active-p)
              (or ewm-input--keyboard-capture-token
                  (ewm-input--keyboard-capture-start-allowed-p)))))
    (ewm-input--set-keyboard-capture capture-active (not capture-active))))

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

(defun ewm-input--in-selected-window (orig key)
  "Translate KEY through the selected window's input method, else ORIG."
  (let ((buf (window-buffer (selected-window))))
    (if (eq buf (current-buffer))
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
VALUE is the ASCII code or special-key name; flags are t or :false."
  (let* ((mods (event-modifiers event))
         (base (event-basic-type event))
         ;; base is either an integer (ASCII) or a symbol (special key)
         (key-value (cond
                     ((integerp base) base)
                     ((symbolp base) (symbol-name base))
                     (t nil))))
    (when key-value
      (list :key key-value
            :ctrl (if (memq 'control mods) t :false)
            :alt (if (memq 'meta mods) t :false)
            :shift (if (memq 'shift mods) t :false)
            :super (if (memq 'super mods) t :false)))))

(defun ewm--event-to-intercept-spec (event &optional fullscreen translate dispatch)
  "Convert EVENT to an intercept specification for the compositor.
Returns a plist with :key, modifier flags, :fullscreen, :dispatch, and
optionally :translate.  TRANSLATE is a target key description (e.g. \"C-c\"
or \"<down>\"); when set, the matched key is delivered to non-Emacs surfaces
as that key instead of going to Emacs."
  (let ((key-spec (ewm--event-key-spec event)))
    (when key-spec
      `(,@key-spec
        :description ,(key-description (vector event))
        :fullscreen ,(if fullscreen t :false)
        :dispatch ,(or dispatch (ewm--event-dispatch event))
        :repeat ,(if (ewm--event-repeat-p event) t :false)
        ,@(when translate
            (list :translate (ewm--event-key-spec
                              (aref (key-parse translate) 0))))))))

(defun ewm--parse-intercept-entry (entry)
  "Parse ENTRY from `ewm-intercept-prefixes'.
Returns (event . fullscreen-p) or nil."
  (let* ((fullscreen (and (listp entry) (memq :fullscreen entry)))
         (key (if fullscreen (car entry) entry)))
    (cond
     ((integerp key) (cons key (and fullscreen t)))
     ((stringp key) (cons (aref (key-parse key) 0) (and fullscreen t))))))

(defun ewm--send-intercept-keys ()
  "Send intercepted keys configuration to compositor.
Scans `ewm-intercept-prefixes' first (so :fullscreen flags win),
then `ewm-mode-map' bindings.  Keys in `ewm-surface-emulate-keys'
get a :translate target delivered to non-Emacs surfaces."
  (ewm-input--install-xf86-key-aliases)
  (let ((specs nil)
        (fullscreen-events nil))
    (cl-flet ((put-spec (event spec)
                (setf (alist-get event specs nil nil #'eql) spec)))
      ;; Intercept prefixes first; :fullscreen entries populate fullscreen-events.
      (dolist (entry ewm-intercept-prefixes)
        (when-let* ((parsed (ewm--parse-intercept-entry entry)))
          (let ((event (car parsed)))
            (when (cdr parsed)
              (push event fullscreen-events))
            (when-let* ((spec (ewm--event-to-intercept-spec
                               event
                               (or (cdr parsed) (memq event fullscreen-events))
                               nil
                               (ewm--event-dispatch event t))))
              (put-spec event spec)))))
      ;; Keymap bindings: fullscreen flag and translate target looked up.
      (map-keymap
       (lambda (event binding)
         (when (ewm--live-binding-p binding)
           (when-let* ((spec (ewm--event-to-intercept-spec
                              event
                              (memq event fullscreen-events)
                              (alist-get event ewm-surface-emulate-keys nil nil #'eql)
                              (ewm--event-dispatch event))))
             (put-spec event spec))))
       ewm-mode-map)
      ;; Translate keys not bound in `ewm-mode-map' still need a spec so the
      ;; surface receives the target; on Emacs focus the key passes through.
      (dolist (entry ewm-surface-emulate-keys)
        (let ((event (car entry)))
          (unless (alist-get event specs nil nil #'eql)
            (when-let* ((spec (ewm--event-to-intercept-spec
                               event (memq event fullscreen-events) (cdr entry))))
              (put-spec event spec)))))
      (ewm-intercept-keys-module
       (vconcat (mapcar #'cdr (nreverse specs)))))))

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
