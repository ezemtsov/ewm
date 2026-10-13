;;; ewm-frame.el --- Output and frame management for EWM -*- lexical-binding: t -*-

;; Copyright (C) 2025
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Outputs and the Emacs frames EWM places on them. Each output shows a
;; strip of tiled frames, a floating frame holds one buffer above the
;; strip, and the compositor decides where every frame goes.

;;; Code:

(require 'cl-lib)
(require 'ewm-module)
(ewm-module--declare)
(require 'ewm-surface)
(require 'ewm-layout)

;;; Outputs

(defvar ewm--output-info nil
  "Alist of (CONNECTOR-NAME . EVENT-ALIST) for currently connected outputs.
Each value is the full output_detected event alist, including make, model,
serial, modes, scale, position and physical size.
Updated on output_detected and output_disconnected events.")

(defun ewm--send-on-set (send)
  "Return a `defcustom' setter that stores the value, then calls SEND if defined."
  (lambda (sym val)
    (set-default sym val)
    (when (fboundp send)
      (funcall send))))

(defcustom ewm-output-config nil
  "Output configuration alist.
Each entry is (OUTPUT-NAME . PLIST) where PLIST can contain:
  :width     - desired width in pixels
  :height    - desired height in pixels
  :refresh   - desired refresh rate in Hz (optional)
  :custom    - when non-nil, generate a CVT mode for :width/:height/:refresh
               instead of matching an advertised mode.  Use this for
               resolutions the monitor does not advertise (e.g. a lower
               resolution on a HiDPI panel for games).  :refresh is required.
  :modeline  - explicit X11 modeline string, takes precedence over the mode
               fields when set:
               \"CLOCK HDISP HSS HSE HTOTAL VDISP VSS VSE VTOTAL +hsync +vsync\"
  :x         - horizontal position (optional)
  :y         - vertical position (optional)
  :scale     - fractional scale, e.g. 1.5 (optional)
  :transform - transform as integer (optional):
               0=Normal 1=90 2=180 3=270
               4=Flipped 5=Flipped90 6=Flipped180 7=Flipped270
  :enabled   - whether output is enabled (default t)

Custom modes are not guaranteed to work: the compositor asks the display to
run a mode its manufacturer did not advertise, and the kernel may reject it.

OUTPUT-NAME can be either a connector name (e.g. \"DP-1\") or a
Make-Model-Serial string (e.g. \"Dell U2722D ABC123\").  The MMS form is
preferred because it survives connector re-numbering when cables are
replugged.  Use `ewm-list-outputs' to see both forms for each connected
output and copy a ready-to-use config entry.

Configuration is stored in the compositor and re-applied when outputs
reconnect (hot-plug), so values persist across connect/disconnect cycles.

Example:
  \\='((\"Dell U2722D ABC123\" :width 2560 :height 1440 :scale 1.5)
    (\"eDP-1\" :width 1920 :height 1200 :x 0 :y 0 :transform 0)
    (\"eDP-1\" :width 1024 :height 640 :refresh 60 :custom t))"
  :type '(alist :key-type string :value-type plist)
  :set (ewm--send-on-set 'ewm--apply-output-config)
  :group 'ewm-output)

(defun ewm--output-mms (info)
  "Return \"Make Model Serial\" string for output INFO event alist.
Uses \"Unknown\" for any component absent from EDID so config keys
written from `ewm-list-outputs' round-trip correctly."
  (let* ((make   (or (alist-get 'make   info) "Unknown"))
         (model  (or (alist-get 'model  info) "Unknown"))
         (serial (or (alist-get 'serial info) ""))
         (serial-str (if (string-empty-p serial) "Unknown" serial)))
    (format "%s %s %s" make model serial-str)))

(defun ewm--output-config-key (info)
  "Return the recommended ewm-output-config key for output INFO.
Uses the Make-Model-Serial string when make/model are known (survives
connector re-numbering after cable changes), otherwise the connector name."
  (let ((make  (or (alist-get 'make  info) "Unknown"))
        (model (or (alist-get 'model info) "Unknown")))
    (if (and (string= make "Unknown") (string= model "Unknown"))
        (alist-get 'name info)
      (ewm--output-mms info))))

(defun ewm--find-connector-for-config (config-name)
  "Resolve CONFIG-NAME to a live connector name.
CONFIG-NAME may be a connector name (e.g. \"DP-1\") or a Make-Model-Serial
string (e.g. \"Dell U2722D ABC123\").  Tries exact connector match first,
then case-insensitive MMS match against `ewm--output-info'.
Returns CONFIG-NAME itself when no live output matches, so the compositor
still stores the config for future hot-plug."
  (or (car (assoc config-name ewm--output-info))
      (car (cl-find config-name ewm--output-info
                    :key (lambda (entry) (ewm--output-mms (cdr entry)))
                    :test #'string-equal-ignore-case))
      config-name))

(defun ewm-configure-output (name &rest args)
  "Configure output NAME with ARGS.
ARGS is a plist with optional keys:
  :x :y       - position in global coordinate space
  :width :height :refresh - video mode
  :custom     - when non-nil, generate a CVT mode for :width/:height/:refresh
                instead of matching an advertised mode (:refresh required)
  :modeline   - X11 modeline string, takes precedence over the mode fields:
                \"CLOCK HDISP HSS HSE HTOTAL VDISP VSS VSE VTOTAL +hsync +vsync\"
  :scale      - fractional scale (e.g. 1.5)
  :transform  - transform integer (0=Normal, 1=90, 2=180, 3=270, ...)
  :enabled    - t or nil to enable/disable the output"
  (ewm-configure-output-module name args))

(defun ewm--apply-output-config ()
  "Apply user output configuration from `ewm-output-config'.
Config keys may be connector names (\"DP-1\") or Make-Model-Serial strings
(\"Dell U2722D ABC123\"); the latter are resolved to the live connector name
via `ewm--find-connector-for-config' before sending to the compositor."
  (dolist (config ewm-output-config)
    (let ((props (cdr config)))
      (when (or (plist-member props :enabled)
                (cl-some (lambda (key) (plist-get props key))
                         '(:width :height :refresh :modeline :scale :transform :x :y)))
        (apply #'ewm-configure-output
               (ewm--find-connector-for-config (car config)) props)))))

(defun ewm--get-output-origin (output-name)
  "Return (x . y) origin for OUTPUT-NAME, or (0 . 0) if not found."
  (or (and output-name (ewm-get-output-origin output-name)) '(0 . 0)))

;;; Frames

(defconst ewm--managed-window-systems '(pgtk pwayl)
  "GUI window systems whose frames EWM binds to outputs.
EWM can host either a pgtk or a native-Wayland (+ Skia) Emacs build.")

(defun ewm--gtk-backed-p ()
  "Non-nil when EWM's host Emacs frames are backed by GTK.
The native Wayland backend has no GTK widgets or style context to mutate."
  (eq (ignore-errors (ewm--window-system)) 'pgtk))

(defun ewm--window-system ()
  "Return the window system to create EWM output frames with.
Picks whichever GUI backend this Emacs was built with (pgtk or the
native Wayland backend)."
  (cond ((featurep 'pgtk) 'pgtk)
        ((featurep 'pwayl) 'pwayl)
        (t (error "EWM requires a pgtk or native-Wayland Emacs build"))))

(defun ewm--output-managed-p (frame)
  "Return non-nil if FRAME is bound to a compositor output."
  (memq (framep-on-display frame) ewm--managed-window-systems))

(defun ewm--frame-by-surface-id (surface-id)
  "Return the Emacs frame whose `ewm-surface-id' parameter is SURFACE-ID."
  (and surface-id
       (cl-find surface-id (frame-list)
                :key (lambda (f) (frame-parameter f 'ewm-surface-id)))))

(defun ewm--frame-output (frame)
  "FRAME's output as the compositor places it, or its intended one before it maps."
  (if-let* ((id (frame-parameter frame 'ewm-surface-id)))
      (ewm-get-frame-output id)
    (frame-parameter frame 'ewm-output)))

(defun ewm--frame-in-flight-p (frame)
  "Return non-nil when FRAME has an output assignment but no surface id yet."
  (and (frame-parameter frame 'ewm-output)
       (null (frame-parameter frame 'ewm-surface-id))))

(defun ewm--strip-frames (output)
  "Return OUTPUT's strip frames left to right, as the compositor places them."
  (delq nil (mapcar #'ewm--frame-by-surface-id (ewm-get-strip-frame-ids output))))

(defun ewm--frame-in-flight-on-p (output)
  "Non-nil when a tiled frame bound for OUTPUT has not mapped yet."
  (seq-some (lambda (frame)
              (and (ewm--frame-in-flight-p frame)
                   (not (frame-parameter frame 'ewm-floating))
                   (equal (frame-parameter frame 'ewm-output) output)))
            (frame-list)))

(defun ewm-output-active-frame (output)
  "Return the strip frame shown on OUTPUT, or its first frame."
  (let ((frames (ewm--strip-frames output))
        (active (and output
                     (ewm--frame-by-surface-id (ewm-get-active-frame-id output)))))
    (if (memq active frames) active (car frames))))

(defun ewm--current-frame ()
  "Frame the user is visually on -- compositor-authoritative.
Emacs's `selected-frame' can drift via `display-buffer'/`select-window'."
  (when-let* ((id (ewm-get-focused-frame-id))
              ((> id 0)))
    (ewm--frame-by-surface-id id)))

(defun ewm--focused-frame ()
  "Frame the user is on per the compositor, falling back to `selected-frame'."
  (or (ewm--current-frame) (selected-frame)))

(defun ewm--surface-target-frame-for-output (output-name)
  "Return the best tiled fallback frame for a new surface on OUTPUT-NAME."
  (or (let ((frame (ignore-errors (ewm--focused-frame))))
        (and frame
             (not (frame-parameter frame 'ewm-floating))
             (equal (ewm--frame-output frame) output-name)
             frame))
      (ewm-output-active-frame output-name)))

(defun ewm--get-frame-origin (frame)
  "Return global origin for FRAME, falling back to its output origin."
  (or (and (frame-parameter frame 'ewm-floating)
           (fboundp 'ewm-get-frame-origin)
           (when-let* ((id (frame-parameter frame 'ewm-surface-id)))
             (ewm-get-frame-origin id)))
      (ewm--get-output-origin (ewm--frame-output frame))))

(defvar ewm--frame-seq 0
  "Monotonic counter advanced for each EWM-managed frame creation.
The compositor echoes it in the `new' event of the frame's surface.")

(defun ewm-prepare-frame (output seq &optional after)
  "Tell the compositor frame SEQ goes on OUTPUT, right of frame id AFTER."
  (ewm-prepare-frame-module output seq after))

(defun ewm-prepare-floating-frame (output seq &optional x y)
  "Tell the compositor frame SEQ floats on OUTPUT at top-left X/Y."
  (ewm-prepare-floating-frame-module output seq x y))

(defun ewm--make-output-frame (output-name seq extra-params)
  "Make managed frame SEQ on OUTPUT-NAME carrying EXTRA-PARAMS.
EXTRA-PARAMS win over the defaults, so a caller can override `visibility'."
  (make-frame `(,@extra-params
                (visibility . t)
                (window-system . ,(ewm--window-system))
                (ewm-output . ,output-name)
                (ewm-frame-seq . ,seq))))

(defun ewm--create-frame-for-output (output-name &optional after-frame extra-params)
  "Create a frame on OUTPUT-NAME inserted after AFTER-FRAME (or appended)."
  (let ((seq (cl-incf ewm--frame-seq)))
    (ewm-prepare-frame output-name seq
                       (and after-frame (frame-parameter after-frame 'ewm-surface-id)))
    (ewm--make-output-frame output-name seq extra-params)))

(defun ewm--create-floating-frame-for-output (output-name &optional extra-params pos)
  "Create a floating frame on OUTPUT-NAME, optionally at POS (X . Y) pixels."
  (let* ((seq (cl-incf ewm--frame-seq))
         (frame (progn
                  (ewm-prepare-floating-frame output-name seq (car pos) (cdr pos))
                  (ewm--make-output-frame
                   output-name seq
                   `((ewm-floating . t) ,@extra-params (width . 80) (height . 24))))))
    ;; `quit-window' would iconify otherwise, which EWM frames cannot do.
    (set-frame-parameter frame 'auto-hide-function (lambda () (delete-frame frame)))
    frame))

(defun ewm--stray-frame-p (frame)
  "Non-nil when FRAME is a managed frame EWM did not make."
  (and (ewm--output-managed-p frame)
       (null (frame-parameter frame 'ewm-output))))

(defun ewm--on-make-frame (frame)
  "Hook for frame creation. Drop frames EWM did not make."
  (when (ewm--stray-frame-p frame)
    (ewm--log "on-make-frame: frame=%s action=schedule-delete" frame)
    (run-at-time 0 nil (lambda () (ignore-errors (delete-frame frame))))))

(defun ewm--delete-stray-frames ()
  "Delete managed frames made before `ewm--on-make-frame' was installed."
  (dolist (frame (frame-list))
    (when (ewm--stray-frame-p frame)
      (ignore-errors (delete-frame frame)))))

(defun ewm--prevent-frame-deletion (orig-fun &optional frame force)
  "Around advice for `delete-frame': refuse to delete the last frame of
an output unless FORCE.

When deletion proceeds, pass the captured frame to ORIG-FUN explicitly:
interactive `delete-frame' passes nil, and nil means \"delete the
selected frame\", which can change during pgtk's internal close handling."
  (let* ((f (or frame (selected-frame)))
         (output (ewm--frame-output f))
         (alone (and output
                     (not (frame-parameter f 'ewm-floating))
                     (<= (length (ewm--strip-frames output)) 1))))
    (cond
     ((and alone (not force))
      (message "Cannot delete the only frame on output %s" output))
     (t
      (when (eq f (selected-frame))
        (when-let* ((id (frame-parameter f 'ewm-surface-id)))
          (ewm-prepare-frame-close-module id)))
      (funcall orig-fun f force)))))

;;; Floating frames

(defun ewm--floating-return-frame (floating-frame)
  "Return the tiled frame that should receive FLOATING-FRAME's buffer."
  (or (when-let* ((origin-id (frame-parameter floating-frame
                                              'ewm-floating-origin-frame-id))
                  (origin (ewm--frame-by-surface-id origin-id))
                  ((not (frame-parameter origin 'ewm-floating))))
        origin)
      (ewm-output-active-frame (ewm--frame-output floating-frame))))

(defun ewm--fit-floating-frame (frame width height)
  "Size FRAME so its client window body is WIDTH×HEIGHT logical pixels."
  (let ((rect (ewm--window-body-pixel-rect (frame-selected-window frame)))
        (area (frame-monitor-workarea frame)))
    (cl-flet ((axis (want limit text body)
                (round (+ (min want limit) (- text body)))))
      (set-frame-size frame
                      (axis width (nth 2 area) (frame-text-width frame) (nth 2 rect))
                      (axis height (nth 3 area) (frame-text-height frame) (nth 3 rect))
                      t))))

(defun ewm--make-floating-frame (alist)
  "Make a floating frame from ALIST and `pop-up-frame-alist', or nil."
  (when-let* ((output (or (alist-get 'ewm-output alist)
                          (ewm--frame-output (selected-frame)))))
    (ewm--create-floating-frame-for-output
     output
     ;; Hidden until fitted.
     `((visibility . nil)
       ,@pop-up-frame-alist
       (ewm-floating-origin-frame-id . ,(frame-parameter nil 'ewm-surface-id)))
     (alist-get 'ewm-position alist))))

(defvar ewm--inhibit-floating nil
  "Non-nil makes `ewm-display-buffer-floating' decline, so attach cannot re-float.")

(defun ewm-display-buffer-floating (buffer alist)
  "Display BUFFER in a new floating frame; a `display-buffer' action function.
Looks come from `pop-up-frame-parameters'.  The frame is fitted to `ewm-size'
(client body W . H) in ALIST, else to the buffer's `ewm-surface-size'.  EWM's
own placements add `ewm-output' and `ewm-position' (top-left X . Y)."
  (unless ewm--inhibit-floating
    (let ((pop-up-frame-function (lambda () (ewm--make-floating-frame alist))))
      ;; Fit after the user's window parameters land, then show.
      (when-let* ((window (display-buffer-pop-up-frame
                           buffer (cons '(inhibit-switch-frame . t) alist)))
                  (frame (window-frame window)))
        (let ((size (or (alist-get 'ewm-size alist)
                        (buffer-local-value 'ewm-surface-size buffer))))
          (when size (ewm--fit-floating-frame frame (car size) (cdr size)))
          (make-frame-visible frame)
          (when size
            ;; Hidden, the window only estimated its mode-line height.
            (redisplay t)
            (ewm--fit-floating-frame frame (car size) (cdr size))))
        window))))

(defvar ewm-floating-popout-scale 0.9
  "Fraction of the source window a popped-out floating frame occupies.")

(defun ewm--float-buffer (buffer frame size pos)
  "Pop BUFFER off FRAME into a float of client body SIZE at output-relative POS."
  (pop-to-buffer buffer `(ewm-display-buffer-floating
                          (inhibit-same-window . t)
                          (ewm-output . ,(ewm--frame-output frame))
                          (pop-up-frame-parameters
                           (ewm-floating-origin-frame-id
                            . ,(frame-parameter frame 'ewm-surface-id)))
                          (ewm-size . ,size) (ewm-position . ,pos))))

(defun ewm--floating-detach-selected-buffer (frame)
  "Detach FRAME's selected buffer into a floating frame centered over its tile."
  (let* ((source-window (frame-selected-window frame))
         (rect (ewm--window-body-pixel-rect source-window))
         (w (nth 2 rect))
         (h (nth 3 rect))
         (fw (round (* w ewm-floating-popout-scale)))
         (fh (round (* h ewm-floating-popout-scale))))
    (unless (ewm--frame-output frame)
      (user-error "Current frame is on no output"))
    (when (window-minibuffer-p source-window)
      (user-error "Cannot detach minibuffer window"))
    (select-frame frame)
    ;; Float positions are output-relative.
    (pcase-let ((`(,fx . ,fy) (ewm--get-frame-origin frame))
                (`(,ox . ,oy) (ewm--get-output-origin (ewm--frame-output frame))))
      (ewm--float-buffer (window-buffer source-window) frame (cons fw fh)
                         (cons (+ (nth 0 rect) (- fx ox) (/ (- w fw) 2))
                               (+ (nth 1 rect) (- fy oy) (/ (- h fh) 2)))))
    ;; Reclaim the tile once the buffer has left for a float.
    (when (and (window-live-p source-window)
               (frame-parameter (window-frame (selected-window)) 'ewm-floating))
      (save-selected-window
        (if (eq t (window-deletable-p source-window))
            (delete-window source-window)
          (quit-window nil source-window))))))

(defun ewm--absorb-floating (floating-frame target action)
  "Move FLOATING-FRAME's buffer into tiled TARGET, a window or frame, via ACTION."
  (let ((buffer (window-buffer (frame-selected-window floating-frame))))
    (delete-frame floating-frame t)
    (if (windowp target)
        (select-window target)
      (select-frame-set-input-focus target))
    ;; pop-to-buffer lets Emacs reuse or split the target window on its own.
    (let ((ewm--inhibit-floating t))
      (pop-to-buffer buffer action))
    (ewm-layout--refresh)))

(defun ewm--floating-attach-selected-buffer (floating-frame)
  "Absorb FLOATING-FRAME's buffer into the tiled window under its center."
  (let* ((sid (frame-parameter floating-frame 'ewm-surface-id))
         (entry-id (and sid (ewm-entry-under-floating-center-module sid)))
         (target (or (and entry-id (ewm--find-window-by-entry-id entry-id))
                     (ewm--floating-return-frame floating-frame))))
    (unless target
      (user-error "No tiled frame on output %s" (ewm--frame-output floating-frame)))
    (ewm--absorb-floating floating-frame target nil)))

(provide 'ewm-frame)
;;; ewm-frame.el ends here
