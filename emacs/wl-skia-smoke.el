;;; wl-skia-smoke.el --- fractional-scaling smoke test for the Wayland+Skia backend  -*- lexical-binding: t; -*-

;; Minimal counterpart to skia-smoke.sh for the native Wayland backend.
;; It uses the pwayl-skia-test-* framebuffer readback hooks to assert
;; rendering and fractional-scaling correctness on the live frame.
;;
;; Usage:
;;   ./wl-skia-smoke.sh ~/git/emacs
;;
;; Direct usage against a running -s wl daemon with a GUI frame:
;;   emacsclient -s wl --eval "(progn (load \"…/wl-skia-smoke.el\") (wl-skia-smoke))"
;; The call returns a multi-line report string (PASS/FAIL per check).

(require 'cl-lib)

(defvar wl-skia-smoke--log nil)

(defun wl-skia-smoke--say (fmt &rest args)
  (push (apply #'format fmt args) wl-skia-smoke--log))

(defun wl-skia-smoke--settle (&optional n)
  "Force a few redisplay cycles so the framebuffer reflects the buffer."
  (dotimes (_ (or n 3))
    (redraw-display)
    (redisplay t)
    (sit-for 0.05)))

(defun wl-skia-smoke--ratio (count w h)
  (if (and (integerp w) (integerp h) (> (* w h) 0))
      (/ (float count) (* w h))
    0.0))

(defun wl-skia-smoke--plist (plist key)
  (plist-get plist key))

(defun wl-skia-smoke--stat (stats key)
  (cdr (assq key stats)))

(defun wl-skia-smoke--wait-for-monitor (frame &optional attempts)
  "Return surface state after waiting briefly for FRAME to enter an output."
  (let ((state nil)
        (remaining (or attempts 20)))
    (while (and (> remaining 0)
                (progn
                  (setq state (pwayl-surface-test-state frame))
                  (not (wl-skia-smoke--plist state :monitor))))
      (setq remaining (1- remaining))
      (wl-skia-smoke--settle 1))
    state))

(defun wl-skia-smoke ()
  "Run the Wayland Skia fractional-scaling smoke checks on the selected frame.
Return a report string."
  (setq wl-skia-smoke--log nil)
  (let* ((f (or (cl-find-if #'display-graphic-p (frame-list))
                (error "No graphical Wayland frame found")))
         (pass 0) (fail 0))
    (cl-flet ((check (name ok detail)
                (if ok (cl-incf pass) (cl-incf fail))
                (wl-skia-smoke--say "%s %s — %s"
                                    (if ok "PASS" "FAIL") name detail)))
      ;; --- 1. framebuffer size / scale invariant -------------------------
      (wl-skia-smoke--settle)
      (let* ((s (pwayl-skia-test-framebuffer-size f))
             (rw (nth 0 s)) (rh (nth 1 s))
             (tw (nth 2 s)) (th (nth 3 s))
             (lw (nth 4 s)) (lh (nth 5 s))
             (scale (nth 6 s))
             ;; Match the backend's pwayl_physical_size rounding (half-up,
             ;; (int)(x + 0.5)), not Elisp `round' (half-to-even), so the
             ;; check is exact at .5 boundaries.
             (ew (floor (+ (* lw scale) 0.5)))
             (eh (floor (+ (* lh scale) 0.5))))
        ;; The color attachment is allocated at grow-only capacity (256px
        ;; steps, kept across shrinks), so it must contain the render
        ;; size, not equal it.
        (check "texture>=render" (and (>= tw rw) (>= th rh))
               (format "render=%dx%d texture=%dx%d" rw rh tw th))
        (check "render==round(logical*scale)" (and (= rw ew) (= rh eh))
               (format "render=%dx%d logical=%dx%d scale=%s expected=%dx%d"
                       rw rh lw lh scale ew eh)))
      ;; --- 2. red text on green background -------------------------------
      (set-face-attribute 'default f :background "#00ff00" :foreground "#ff0000")
      (let ((buf (get-buffer-create "*pwayl-smoke*")))
        (select-frame f)
        (select-window (frame-root-window f))
        (delete-other-windows (frame-root-window f))
        (set-window-buffer (frame-root-window f) buf)
        (with-current-buffer buf
          (setq mode-line-format nil)   ; keep the field uniform
          (setq header-line-format nil)
          (setq tab-line-format nil)
          (erase-buffer)
          (insert (make-string 4000 ?H))
          (goto-char (point-min))))
      (wl-skia-smoke--settle)
      (let* ((c (pwayl-skia-test-framebuffer-color-counts f))
             (rw (nth 0 c)) (rh (nth 1 c))
             (red (nth 2 c)) (green (nth 3 c)))
        (check "red text present" (> red 1000)
               (format "red=%d (frame %dx%d)" red rw rh))
        (check "green background present" (> green 5000)
               (format "green=%d" green)))
      ;; --- 3. solid-fill uniformity (artifact detector) -----------------
      ;; Empty buffer on a green background: the whole interior must be solid
      ;; green.  Any black/other pixel from a fractional-scale coverage gap
      ;; (or a failure to repaint) lowers the ratio.  fg red so stray glyphs
      ;; show up as red, not green.
      (set-face-attribute 'default f :background "#00ff00" :foreground "#ff0000")
      (with-current-buffer (get-buffer-create "*pwayl-smoke*") (erase-buffer))
      (check "solid-green buffer is empty"
             (= (buffer-size (get-buffer "*pwayl-smoke*")) 0)
             (format "size=%d" (buffer-size (get-buffer "*pwayl-smoke*"))))
      (redraw-frame f)
      (wl-skia-smoke--settle 4)
      (let* ((lw (frame-pixel-width f))
             (lh (frame-pixel-height f))
             ;; sample a generous interior rect, away from edges/cursor
             (rx (/ lw 8)) (ry (/ lh 8))
             (rwd (/ lw 2)) (rht (/ lh 2))
             (c (pwayl-skia-test-framebuffer-color-counts f (list rx ry rwd rht)))
             (red (nth 2 c)) (green (nth 3 c))
             (bounds (nth 5 c))
             ;; device-pixel area of the sampled rect, matching the
             ;; hook's per-edge round-half-up mapping exactly
             (s (pwayl-skia-test-framebuffer-size f))
             (scale (nth 6 s))
             (dw (- (floor (+ (* (+ rx rwd) scale) 0.5))
                    (floor (+ (* rx scale) 0.5))))
             (dh (- (floor (+ (* (+ ry rht) scale) 0.5))
                    (floor (+ (* ry scale) 0.5))))
             (total (* dw dh))
             (ratio (wl-skia-smoke--ratio green dw dh)))
        (when (and (< ratio 0.999)
                   (fboundp 'pwayl-skia-test-dump-png))
          (pwayl-skia-test-dump-png "/tmp/pwayl-smoke-solid-fail.png" f))
        (check "solid-green interior uniform (>=0.999)" (>= ratio 0.999)
               (format "green=%d red=%d other=%d of %d device px = %.4f (logical rect %d,%d %dx%d red-bounds=%S)"
                       green red (- total green red) total ratio rx ry rwd rht bounds)))
      ;; --- 4. alpha-background uses an ARGB render target ---------------
      (check "surface state hook present for alpha"
             (fboundp 'pwayl-surface-test-state)
             "pwayl-surface-test-state")
      (when (fboundp 'pwayl-surface-test-state)
        (unwind-protect
            (progn
              (modify-frame-parameters f '((alpha-background . 50)))
              (set-face-attribute 'default f
                                  :background "#00ff00"
                                  :foreground "#ff0000")
              (with-current-buffer (get-buffer-create "*pwayl-smoke*")
                (erase-buffer))
              (redraw-frame f)
              (wl-skia-smoke--settle 4)
              (let* ((state (pwayl-surface-test-state f))
                     (lw (frame-pixel-width f))
                     (lh (frame-pixel-height f))
                     (rx (/ lw 8)) (ry (/ lh 8))
                     (rwd (/ lw 2)) (rht (/ lh 2))
                     (c (pwayl-skia-test-framebuffer-color-counts
                         f (list rx ry rwd rht)))
                     (green (nth 3 c))
                     (translucent (nth 4 c))
                     (s (pwayl-skia-test-framebuffer-size f))
                     (scale (nth 6 s))
                     (dw (- (floor (+ (* (+ rx rwd) scale) 0.5))
                            (floor (+ (* rx scale) 0.5))))
                     (dh (- (floor (+ (* (+ ry rht) scale) 0.5))
                            (floor (+ (* ry scale) 0.5))))
                     (total (* dw dh))
                     (ratio (wl-skia-smoke--ratio translucent dw dh)))
                (check "alpha-background switches render target to RGBA"
                       (wl-skia-smoke--plist state :render-alpha)
                       (format "%S" state))
                (check "alpha-background framebuffer is translucent green"
                       (>= ratio 0.999)
                       (format "green=%d translucent=%d of %d device px = %.4f"
                               green translucent total ratio))))
          (modify-frame-parameters f '((alpha-background . nil)))
          (redraw-frame f)
          (wl-skia-smoke--settle 4))
        (let* ((lw (frame-pixel-width f))
               (lh (frame-pixel-height f))
               (rx (/ lw 8)) (ry (/ lh 8))
               (rwd (/ lw 2)) (rht (/ lh 2))
               (c (pwayl-skia-test-framebuffer-color-counts
                   f (list rx ry rwd rht)))
               (green (nth 3 c))
               (translucent (nth 4 c))
               (state (pwayl-surface-test-state f)))
          (check "alpha-background reset restores opaque pixels"
                 (and (> green 0) (= translucent 0))
                 (format "green=%d translucent=%d" green translucent))
          (check "alpha-background reset returns render target to RGB"
                 (not (wl-skia-smoke--plist state :render-alpha))
                 (format "%S" state))))
      ;; --- 6. Presentation counters -------------------------------------
      (check "presentation stats hook present"
             (fboundp 'pwayl-skia-test-presentation-stats)
             "pwayl-skia-test-presentation-stats")
      (when (fboundp 'pwayl-skia-test-presentation-stats)
        (pwayl-skia-test-presentation-stats f t)
        (wl-skia-smoke--settle 4)
        (let* ((stats (pwayl-skia-test-presentation-stats f))
               (queued (wl-skia-smoke--stat stats 'queue-area-count))
               (queued-full (wl-skia-smoke--stat stats 'queue-full-count))
               (drawn (wl-skia-smoke--stat stats 'draw-count))
               (presented (wl-skia-smoke--stat stats 'present-count))
               (pixels (wl-skia-smoke--stat stats 'present-pixels))
               (feedback-supported
                (wl-skia-smoke--stat stats 'feedback-supported))
               (feedback-clock
                (wl-skia-smoke--stat stats 'feedback-clock-id))
               (feedback-requested
                (wl-skia-smoke--stat stats 'feedback-request-count))
               (feedback-presented
                (wl-skia-smoke--stat stats 'feedback-presented-count))
               (feedback-discarded
                (wl-skia-smoke--stat stats 'feedback-discarded-count))
               (feedback-sync-output
                (wl-skia-smoke--stat stats 'feedback-sync-output-count)))
          (check "presentation queue tracked"
                 (and (integerp queued)
                      (integerp queued-full)
                      (> (+ queued queued-full) 0))
                 (format "%S" stats))
          (check "presentation draw tracked"
                 (and (integerp drawn) (> drawn 0))
                 (format "%S" stats))
          (check "presentation swap tracked"
                 (and (integerp presented) (> presented 0))
                 (format "%S" stats))
          (check "presentation pixels tracked"
                 (and (integerp pixels) (> pixels 0))
                 (format "%S" stats))
          (check "presentation feedback protocol present"
                 feedback-supported
                 (format "%S" stats))
          (check "presentation clock tracked"
                 (integerp feedback-clock)
                 (format "%S" stats))
          (check "presentation feedback requested"
                 (and (integerp feedback-requested)
                      (> feedback-requested 0))
                 (format "%S" stats))
          (check "presentation feedback delivered"
                 (and (integerp feedback-presented)
                      (integerp feedback-discarded)
                      (> (+ feedback-presented feedback-discarded) 0))
                 (format "%S" stats))
          (check "presentation feedback output tracked"
                 (and (integerp feedback-sync-output)
                      (or (= feedback-presented 0)
                          (> feedback-sync-output 0)))
                 (format "%S" stats)))
        ;; Partial present is opt-in: a Lisp variable on current builds,
        ;; the WL_SKIA_PARTIAL_PRESENT env var on older ones.
        (when (boundp 'pwayl-partial-present)
          (setq pwayl-partial-present t))
        (pwayl-skia-test-presentation-stats f t)
        (with-current-buffer (get-buffer-create "*pwayl-smoke*")
          (goto-char (point-max))
          (dotimes (i 6)
            (insert (char-to-string (+ ?a i)))
            (redisplay t)
            (sit-for 0.05)))
        (let* ((stats (pwayl-skia-test-presentation-stats f))
               (queued (wl-skia-smoke--stat stats 'queue-area-count))
               (queued-full (wl-skia-smoke--stat stats 'queue-full-count))
               (last-rect (wl-skia-smoke--stat stats 'present-last-rect))
               (s (pwayl-skia-test-framebuffer-size f))
               (rw (nth 0 s))
               (rh (nth 1 s)))
          ;; Frame-callback timeouts (compositor withholding callbacks
          ;; from an occluded window) legitimately force occasional
          ;; full presents, so only require that area damage flows.
          (check "presentation area damage tracked"
                 (and (integerp queued) (> queued 0)
                      (integerp queued-full)
                      (> queued queued-full))
                 (format "%S" stats))
          (check "presentation last damage is partial"
                 (or (not (and (wl-skia-smoke--stat
                                stats 'egl-buffer-age-supported)
                               (wl-skia-smoke--stat
                                stats 'egl-swap-damage-supported)))
                     (and (consp last-rect)
                          (< (* (nth 2 last-rect) (nth 3 last-rect))
                             (* rw rh))))
                 (format "%S" stats)))
        (when (boundp 'pwayl-partial-present)
          (setq pwayl-partial-present nil)))
      (check "flush-frame test hook present"
             (fboundp 'pwayl-skia-test-flush-frame)
             "pwayl-skia-test-flush-frame")
      (when (and (fboundp 'pwayl-skia-test-presentation-stats)
                 (fboundp 'pwayl-skia-test-flush-frame)
                 (fboundp 'pwayl-surface-test-state))
        (unwind-protect
            (progn
              (pwayl-skia-test-presentation-stats f t)
              (redraw-frame f)
              (let ((state (pwayl-surface-test-state f)))
                (check "redraw-frame marks render target dirty"
                       (wl-skia-smoke--plist state :render-dirty)
                       (format "%S" state)))
              (pwayl-skia-test-flush-frame f)
              (let* ((state (pwayl-surface-test-state f))
                     (stats (pwayl-skia-test-presentation-stats f))
                     (presented (wl-skia-smoke--stat stats 'present-count)))
                (check "flush-frame presents dirty render target"
                       (and (not (wl-skia-smoke--plist state :render-dirty))
                            (integerp presented)
                            (> presented 0))
                       (format "state=%S stats=%S" state stats))))
          (redraw-display)
          (wl-skia-smoke--settle 4)))
      ;; --- 5. M4 selection and DnD state ---------------------------------
      (check "selection state hook present" (fboundp 'pwayl-selection-test-state)
             "pwayl-selection-test-state")
      (when (fboundp 'pwayl-selection-test-state)
        (let ((state (pwayl-selection-test-state)))
          (check "clipboard data device present"
                 (wl-skia-smoke--plist state :clipboard-device)
                 (format "%S" state))
          (check "primary selection device present"
                 (wl-skia-smoke--plist state :primary-device)
                 (format "%S" state))
          (check "dnd state tracked"
                 (integerp (wl-skia-smoke--plist state :dnd-drop-count))
                 (format "%S" state))))
      (let ((value "pwayl-smoke-clipboard")
            (primary "pwayl-smoke-primary"))
        (gui-backend-set-selection 'CLIPBOARD value)
        (wl-skia-smoke--settle 1)
        (check "clipboard owner"
               (gui-backend-selection-owner-p 'CLIPBOARD)
               "self-owned CLIPBOARD")
        (check "clipboard roundtrip"
               (equal (gui-backend-get-selection 'CLIPBOARD 'UTF8_STRING)
                      value)
               value)
        (gui-backend-set-selection 'PRIMARY primary)
        (wl-skia-smoke--settle 1)
        (check "primary owner"
               (gui-backend-selection-owner-p 'PRIMARY)
               "self-owned PRIMARY")
        (check "primary roundtrip"
               (equal (gui-backend-get-selection 'PRIMARY 'UTF8_STRING)
                      primary)
               primary))
      (let ((buf (get-buffer-create "*pwayl-smoke-select-all*")))
        (with-current-buffer buf
          (erase-buffer)
          (dotimes (i 200)
            (insert (format "selection smoke line %03d\n" i)))
          (goto-char (point-min)))
        (set-window-buffer (frame-root-window f) buf)
        (select-window (frame-root-window f))
        (setq select-active-regions t)
        (execute-kbd-macro (kbd "C-x h"))
        (wl-skia-smoke--settle 2)
        (check "mark-whole-buffer activates region"
               (and (region-active-p)
                    (= (region-beginning) (point-min))
                    (= (region-end) (point-max)))
               (format "active=%S beg=%S end=%S size=%d"
                       (region-active-p)
                       (and (region-active-p) (region-beginning))
                       (and (region-active-p) (region-end))
                       (buffer-size)))
        (check "mark-whole-buffer primary owner"
               (gui-backend-selection-owner-p 'PRIMARY)
               "PRIMARY remains owned after select-all")
        (check "mark-whole-buffer primary contents"
               (equal (gui-backend-get-selection 'PRIMARY 'UTF8_STRING)
                      (buffer-string))
               (format "selection-bytes=%d buffer-bytes=%d"
                       (string-bytes (or (gui-backend-get-selection
                                          'PRIMARY 'UTF8_STRING)
                                         ""))
                       (buffer-size))))
      ;; --- 6. M4 pointer cursor shapes -----------------------------------
      (check "cursor state hook present" (fboundp 'pwayl-skia-test-cursor-state)
             "pwayl-skia-test-cursor-state")
      (check "cursor setter hook present" (fboundp 'pwayl-skia-test-set-cursor)
             "pwayl-skia-test-set-cursor")
      (when (and (fboundp 'pwayl-skia-test-cursor-state)
                 (fboundp 'pwayl-skia-test-set-cursor))
        (let ((state (pwayl-skia-test-cursor-state f)))
          (check "cursor theme loaded"
                 (wl-skia-smoke--plist state :theme)
                 (format "%S" state))
          (check "default cursor named"
                 (stringp (wl-skia-smoke--plist state :default))
                 (format "%S" state))
          (setq state (pwayl-skia-test-set-cursor f 'text))
          (check "text cursor selected"
                 (equal (wl-skia-smoke--plist state :current)
                        (wl-skia-smoke--plist state :text))
                 (format "%S" state))
          (setq state (pwayl-skia-test-set-cursor f 'default))
          (check "default cursor selected"
                 (equal (wl-skia-smoke--plist state :current)
                        (wl-skia-smoke--plist state :default))
                 (format "%S" state))
          (setq state (pwayl-skia-test-set-cursor f 'hourglass))
          (check "hourglass cursor active"
                 (wl-skia-smoke--plist state :hourglass-visible)
                 (format "%S" state))
          (setq state (pwayl-skia-test-set-cursor f 'hide-hourglass))
          (check "hourglass cursor hidden"
                 (not (wl-skia-smoke--plist state :hourglass-visible))
                 (format "%S" state))
          (setq state (pwayl-skia-test-set-cursor f 'visible))
          (check "pointer invisible hook visible baseline"
                 (not (wl-skia-smoke--plist state :pointer-invisible))
                 (format "%S" state))
          (setq state (pwayl-skia-test-set-cursor f 'invisible))
          (check "pointer invisible hook hides cursor"
                 (wl-skia-smoke--plist state :pointer-invisible)
                 (format "%S" state))
          (setq state (pwayl-skia-test-set-cursor f 'visible))
          (check "pointer invisible hook restores cursor"
                 (not (wl-skia-smoke--plist state :pointer-invisible))
                 (format "%S" state))))
      ;; --- 7. M5 text-input-v3 IME state --------------------------------
      (check "text-input state hook present" (fboundp 'pwayl-text-input-test-state)
             "pwayl-text-input-test-state")
      (check "text-input commit hook present" (fboundp 'pwayl-text-input-test-commit)
             "pwayl-text-input-test-commit")
      (when (fboundp 'pwayl-text-input-test-state)
        (let ((state (pwayl-text-input-test-state f)))
          (check "text-input manager present"
                 (wl-skia-smoke--plist state :manager)
                 (format "%S" state))
          (check "text-input object present"
                 (wl-skia-smoke--plist state :text-input)
                 (format "%S" state))
          (check "text-input counters tracked"
                 (and (integerp (wl-skia-smoke--plist state :request-serial))
                      (integerp (wl-skia-smoke--plist state :done-serial))
                      (integerp (wl-skia-smoke--plist state :enter-count))
                      (integerp (wl-skia-smoke--plist state :commit-count)))
                 (format "%S" state))))
      (when (fboundp 'pwayl-text-input-test-commit)
        (let ((before (buffer-string))
              (events (pwayl-text-input-test-commit "az" f)))
          (check "text-input commit queues character input"
                 (equal events (list ?a ?z))
                 (format "%S" events))
          (check "text-input commit does not edit buffer directly"
                 (equal (buffer-string) before)
                 (format "before %S after %S" before (buffer-string)))))
      ;; --- 8. M5 child frames use subsurfaces ---------------------------
      (check "surface role hook present" (fboundp 'pwayl-surface-test-state)
             "pwayl-surface-test-state")
      (when (fboundp 'pwayl-surface-test-state)
        (let ((state (wl-skia-smoke--wait-for-monitor f)))
          (check "top-level surface role"
                 (and (wl-skia-smoke--plist state :toplevel)
                      (not (wl-skia-smoke--plist state :subsurface))
                      (not (wl-skia-smoke--plist state :popup))
                      (not (wl-skia-smoke--plist state :tooltip)))
                 (format "%S" state))
          (check "surface entered monitor"
                 (wl-skia-smoke--plist state :monitor)
                 (format "%S" state))
          (modify-frame-parameters f '((title . "pwayl-smoke-title")))
          (wl-skia-smoke--settle 1)
          (setq state (pwayl-surface-test-state f))
          (check "toplevel title parameter reaches Wayland state"
                 (equal (wl-skia-smoke--plist state :title)
                        "pwayl-smoke-title")
                 (format "%S" state))
          (modify-frame-parameters f '((title . nil)
                                       (name . "pwayl-smoke-name")))
          (wl-skia-smoke--settle 1)
          (setq state (pwayl-surface-test-state f))
          (check "toplevel name parameter reaches Wayland title"
                 (equal (wl-skia-smoke--plist state :title)
                        "pwayl-smoke-name")
                 (format "%S" state))
          (let ((old-lines (frame-parameter f 'tab-bar-lines)))
            (unwind-protect
                (progn
                  (modify-frame-parameters f '((tab-bar-lines . 1)))
                  (wl-skia-smoke--settle 2)
                  (let* ((geometry (frame-geometry f))
                         (tab-size (cdr (assq 'tab-bar-size geometry))))
                    (check "tab bar lines parameter updates"
                           (= (or (frame-parameter f 'tab-bar-lines) 0) 1)
                           (format "%S" geometry))
                    (check "tab bar height becomes visible"
                           (and (consp tab-size) (> (cdr tab-size) 0))
                           (format "%S" geometry))))
              (modify-frame-parameters
               f `((tab-bar-lines . ,(or old-lines 0))))
              (wl-skia-smoke--settle 2)))
          (let ((old-lines (frame-parameter f 'menu-bar-lines)))
            (unwind-protect
                (progn
                  (modify-frame-parameters f '((menu-bar-lines . 1)))
                  (wl-skia-smoke--settle 2)
                  (let* ((geometry (frame-geometry f))
                         (menu-size (cdr (assq 'menu-bar-size geometry))))
                    (check "menu bar lines parameter updates"
                           (= (or (frame-parameter f 'menu-bar-lines) 0) 1)
                           (format "%S" geometry))
                    (check "menu bar height becomes visible"
                           (and (consp menu-size) (> (cdr menu-size) 0))
                           (format "%S" geometry))))
              (modify-frame-parameters
               f `((menu-bar-lines . ,(or old-lines 0))))
              (wl-skia-smoke--settle 2)))
          (setq state (wl-skia-smoke--wait-for-monitor f))
          (check "monitor dimensions tracked"
                 (and (> (or (wl-skia-smoke--plist state :monitor-width) 0) 0)
                      (> (or (wl-skia-smoke--plist state :monitor-height) 0) 0)
                      (> (or (wl-skia-smoke--plist state :monitor-scale) 0) 0))
                 (format "%S" state))
          (check "xdg-output manager present"
                 (wl-skia-smoke--plist state :xdg-output-manager)
                 (format "%S" state))
          (check "monitor xdg-output created"
                 (wl-skia-smoke--plist state :monitor-xdg-output)
                 (format "%S" state))
          (check "monitor logical geometry tracked"
                 (and (wl-skia-smoke--plist state :monitor-logical-position)
                      (wl-skia-smoke--plist state :monitor-logical-size)
                      (> (or (wl-skia-smoke--plist state :monitor-logical-width)
                             0)
                         0)
                      (> (or (wl-skia-smoke--plist state :monitor-logical-height)
                             0)
                         0))
                 (format "%S" state))
          (check "subcompositor present"
                 (wl-skia-smoke--plist state :subcompositor)
                 (format "%S" state)))
        (let* ((state (wl-skia-smoke--wait-for-monitor f))
               (monitors (display-monitor-attributes-list f)))
          (check "display monitor attributes present"
                 (and (consp monitors)
                      (assq 'geometry (car monitors))
                      (assq 'workarea (car monitors))
                      (assq 'mm-size (car monitors)))
                 (format "%S" monitors))
          (check "display monitor source is Wayland"
                 (equal (cdr (assq 'source (car monitors))) "Wayland")
                 (format "%S" monitors))
          (check "display monitor scale tracked"
                 (numberp (cdr (assq 'scale-factor (car monitors))))
                 (format "%S" monitors))
          (check "display pixel size follows monitor"
                 (let ((geometry (cdr (assq 'geometry (car monitors)))))
                   (and (>= (display-pixel-width f) (nth 2 geometry))
                        (>= (display-pixel-height f) (nth 3 geometry))))
                 (format "display=%dx%d monitors=%S"
                         (display-pixel-width f)
                         (display-pixel-height f)
                         monitors))
          (check "display monitor uses logical geometry"
                 (let ((geometry (cdr (assq 'geometry (car monitors)))))
                   (and (wl-skia-smoke--plist state :monitor-logical-size)
                        (= (nth 2 geometry)
                           (wl-skia-smoke--plist state
                                                 :monitor-logical-width))
                        (= (nth 3 geometry)
                           (wl-skia-smoke--plist state
                                                 :monitor-logical-height))))
                 (format "state=%S monitors=%S" state monitors)))
        (let ((state (pwayl-surface-test-state f)))
          (check "decoration manager present"
                 (wl-skia-smoke--plist state :decoration-manager)
                 (format "%S" state))
          (check "toplevel decoration created"
                 (wl-skia-smoke--plist state :decoration)
                 (format "%S" state))
          (check "toplevel decoration configured"
                 (> (or (wl-skia-smoke--plist state :decoration-configure-count)
                        0)
                    0)
                 (format "%S" state))
          (check "server-side decoration negotiated"
                 (wl-skia-smoke--plist state :decoration-server-side)
                 (format "%S" state)))
        (let* ((state (pwayl-surface-test-state f))
               (request-before
                (wl-skia-smoke--plist state :activation-request-count))
               (done-before
                (wl-skia-smoke--plist state :activation-done-count))
               (activate-before
                (wl-skia-smoke--plist state :activation-activate-count)))
          (check "activation manager present"
                 (wl-skia-smoke--plist state :activation-manager)
                 (format "%S" state))
          (x-focus-frame f)
          (wl-skia-smoke--settle 4)
          (setq state (pwayl-surface-test-state f))
          (check "activation request issued"
                 (> (wl-skia-smoke--plist state :activation-request-count)
                    request-before)
                 (format "%S" state))
          (check "activation token completed"
                 (> (wl-skia-smoke--plist state :activation-done-count)
                    done-before)
                 (format "%S" state))
          (check "activation applied"
                 (> (wl-skia-smoke--plist state :activation-activate-count)
                    activate-before)
                 (format "%S" state))
          (check "activation tokens drained"
                 (= (wl-skia-smoke--plist state :activation-pending-count) 0)
                 (format "%S" state)))
        (let ((parent-scale (wl-skia-smoke--plist
                             (pwayl-surface-test-state f) :scale))
              child)
          (condition-case err
              (unwind-protect
                  (progn
                    (setq child
                          (make-frame `((parent-frame . ,f)
                                        (width . 24)
                                        (height . 4)
                                        (left . 24)
                                        (top . 32)
                                        (minibuffer . nil)
                                        (undecorated . t)
                                        (visibility . t))))
                    (wl-skia-smoke--settle 2)
                    (let ((state (pwayl-surface-test-state child)))
                      (check "child frame created"
                             (frame-live-p child)
                             (format "%S" state))
                      (check "child frame is subsurface"
                             (and (wl-skia-smoke--plist state :parent)
                                  (wl-skia-smoke--plist state :subsurface)
                                  (not (wl-skia-smoke--plist state :popup))
                                  (not (wl-skia-smoke--plist state :tooltip))
                                  (not (wl-skia-smoke--plist state :toplevel)))
                             (format "%S" state))
                      (check "child frame inherits scale"
                             (< (abs (- (wl-skia-smoke--plist state :scale)
                                        parent-scale))
                                0.001)
                             (format "%S parent-scale=%S"
                                     state parent-scale))
                      (check "child frame initial position"
                             (and (= (wl-skia-smoke--plist state :x) 24)
                                  (= (wl-skia-smoke--plist state :y) 32))
                             (format "%S" state)))
                    (set-frame-position child 48 56)
                    (wl-skia-smoke--settle 2)
                    (let ((state (pwayl-surface-test-state child)))
                      (check "child frame moves subsurface"
                             (and (= (wl-skia-smoke--plist state :x) 48)
                                  (= (wl-skia-smoke--plist state :y) 56))
                             (format "%S" state))))
                (when (and child (frame-live-p child))
                  (delete-frame child t)))
            (error
             (check "child frame subsurface create" nil
                    (error-message-string err)))))
        (let ((parent-scale (wl-skia-smoke--plist
                             (pwayl-surface-test-state f) :scale))
              popup)
          (condition-case err
              (unwind-protect
                  (progn
                    (setq popup
                          (make-frame `((parent-frame . ,f)
                                        (override-redirect . t)
                                        (width . 20)
                                        (height . 3)
                                        (left . 72)
                                        (top . 80)
                                        (minibuffer . nil)
                                        (undecorated . t)
                                        (visibility . t))))
                    (wl-skia-smoke--settle 2)
                    (let ((state (pwayl-surface-test-state popup)))
                      (check "popup frame created"
                             (frame-live-p popup)
                             (format "%S" state))
                      (check "popup frame is xdg-popup"
                             (and (wl-skia-smoke--plist state :parent)
                                  (wl-skia-smoke--plist state :popup)
                                  (not (wl-skia-smoke--plist state :tooltip))
                                  (not (wl-skia-smoke--plist state :toplevel))
                                  (not (wl-skia-smoke--plist state :subsurface)))
                             (format "%S" state))
                      (check "popup frame inherits scale"
                             (< (abs (- (wl-skia-smoke--plist state :scale)
                                        parent-scale))
                                0.001)
                             (format "%S parent-scale=%S"
                                     state parent-scale))
                      (check "popup frame configured"
                             (and (wl-skia-smoke--plist state :configured)
                                  (> (wl-skia-smoke--plist state :popup-width) 0)
                                  (> (wl-skia-smoke--plist state :popup-height) 0))
                             (format "%S" state))))
                (when (and popup (frame-live-p popup))
                  (delete-frame popup t)))
            (error
             (check "popup frame xdg-popup create" nil
                    (error-message-string err))))))
      ;; --- 9. M5 tooltips use popup frames ------------------------------
      (check "tooltip show hook present" (fboundp 'x-show-tip)
             "x-show-tip")
      (check "tooltip hide hook present" (fboundp 'x-hide-tip)
             "x-hide-tip")
      (when (and (fboundp 'x-show-tip) (fboundp 'x-hide-tip)
                 (fboundp 'pwayl-surface-test-state))
        (condition-case err
            (progn
              (x-show-tip "Wayland tooltip smoke" f
                          '((left . 96)
                            (top . 104)
                            (background-color . "lightyellow")
                            (foreground-color . "black"))
                          1000 5 20)
              (wl-skia-smoke--settle 2)
              (let ((tip pwayl-tip-frame))
                (check "tooltip frame created"
                       (and (framep tip) (frame-live-p tip))
                       (format "%S" tip))
                (when (and (framep tip) (frame-live-p tip))
                  (let ((state (pwayl-surface-test-state tip)))
                    (check "tooltip frame is xdg-popup"
                           (and (wl-skia-smoke--plist state :parent)
                                (wl-skia-smoke--plist state :popup)
                                (wl-skia-smoke--plist state :tooltip)
                                (not (wl-skia-smoke--plist state :toplevel))
                                (not (wl-skia-smoke--plist state :subsurface)))
                           (format "%S" state))
                    (check "tooltip frame marked internal"
                           (not (memq tip (frame-list)))
                           (format "frame-list=%S" (frame-list))))))
              (let ((first-tip pwayl-tip-frame))
                (check "tooltip frame hidden" (x-hide-tip) "x-hide-tip")
                (check "tooltip frame cached invisible"
                       (and (framep pwayl-tip-frame)
                            (frame-live-p pwayl-tip-frame)
                            (not (frame-visible-p pwayl-tip-frame)))
                       (format "%S" pwayl-tip-frame))
                ;; Same parent and parameters: the cached frame must be
                ;; reused instead of created from scratch (P6w).
                (x-show-tip "Wayland tooltip smoke again" f
                            '((left . 96)
                              (top . 104)
                              (background-color . "lightyellow")
                              (foreground-color . "black"))
                            1000 5 20)
                (wl-skia-smoke--settle 2)
                (check "tooltip frame reused"
                       (and (framep pwayl-tip-frame)
                            (frame-live-p pwayl-tip-frame)
                            (eq pwayl-tip-frame first-tip)
                            (frame-visible-p pwayl-tip-frame))
                       (format "first=%S now=%S" first-tip pwayl-tip-frame))
                (check "tooltip frame hidden again" (x-hide-tip)
                       "x-hide-tip")))
          (error
           (ignore-errors (x-hide-tip))
           (check "tooltip frame xdg-popup create" nil
                  (error-message-string err)))))
      ;; --- 10. M5 popup menu model/render helpers ----------------------
      (check "menu renderer hook present" (fboundp 'pwayl--menu-show)
             "pwayl--menu-show")
      (check "menu row builder present" (fboundp 'pwayl--menu-build-rows)
             "pwayl--menu-build-rows")
      (check "menu renderer active predicate present"
             (fboundp 'menu-or-popup-active-p)
             "menu-or-popup-active-p")
      (when (and (fboundp 'pwayl--menu-build-rows)
                 (fboundp 'pwayl--menu-first-enabled)
                 (fboundp 'pwayl--menu-row-id)
                 (fboundp 'pwayl--menu-render))
        (let* ((menu (pwayl--menu-build-rows
                      "Smoke"
                      '((:kind pane :label "Pane")
                        (:kind item :id 7 :label "Open" :enabled t
                         :key "C-o")
                        (:kind item :id 8 :label "Disabled" :enabled nil)
                        (:kind separator)
                        (:kind item :id 9 :label "Toggle" :enabled t
                         :selected t :button toggle :indent 1))))
               (rows (car menu))
               (width (cadr menu))
               (selected (pwayl--menu-first-enabled rows))
               (buf (generate-new-buffer " *pwayl-menu-smoke*")))
          (unwind-protect
              (progn
                (check "menu rows built"
                       (and (vectorp rows) (>= (length rows) 6)
                            (integerp width) (> width 0))
                       (format "rows=%S width=%S" rows width))
                (check "menu first enabled row selected"
                       (equal (pwayl--menu-row-id rows selected) 7)
                       (format "selected=%S rows=%S" selected rows))
                (pwayl--menu-render buf rows width selected)
                (with-current-buffer buf
                  (goto-char (point-min))
                  (forward-line selected)
                  (check "menu render annotates selected row"
                         (and (equal (get-text-property (point)
                                                       'pwayl-menu-id)
                                     7)
                              (eq (get-text-property (point) 'face)
                                  'highlight))
                         (buffer-string))))
            (when (buffer-live-p buf)
              (kill-buffer buf)))))
      (when (boundp 'pwayl--menu-active)
        (let ((pwayl--menu-active t))
          (check "menu active predicate tracks active menu"
                 (and (fboundp 'menu-or-popup-active-p)
                      (menu-or-popup-active-p))
                 "pwayl--menu-active")))
      (when (fboundp 'x-popup-menu)
        (condition-case err
            (let* ((gc-cons-threshold 0)
                   (unread-command-events (list ?\r))
                   (selection
                    (x-popup-menu
                     t
                     '("Smoke popup"
                       ("Pane"
                        ("One" . one)
                        ("Two" . two))))))
              (check "x-popup-menu selects first item under GC pressure"
                     (eq selection 'one)
                     (format "%S" selection)))
          (error
           (check "x-popup-menu selects first item under GC pressure"
                  nil
                  (error-message-string err)))))
      (when (fboundp 'pwayl-skia-test-cursor-state)
        (unwind-protect
            (progn
              (modify-frame-parameters f '((cursor-color . "#ff0000")
                                           (cursor-type . (bar . 5))))
              (wl-skia-smoke--settle 2)
              (let ((state (pwayl-skia-test-cursor-state f)))
                (check "text cursor color parameter reaches backend"
                       (let ((color (wl-skia-smoke--plist
                                     state :text-cursor-color)))
                         (and (integerp color) (/= color 0)))
                       (format "%S" state))
                (check "text cursor type parameter reaches backend"
                       (and (eq (wl-skia-smoke--plist state
                                                       :text-cursor-kind)
                                'bar)
                            (= (or (wl-skia-smoke--plist
                                    state :text-cursor-width)
                                   -1)
                               5))
                       (format "%S" state))))
          (modify-frame-parameters f '((cursor-color . "black")
                                       (cursor-type . box)))
          (wl-skia-smoke--settle 2)))
      (when (fboundp 'pwayl-surface-test-state)
        (unwind-protect
            (progn
              (modify-frame-parameters f '((alpha . 50)))
              (wl-skia-smoke--settle 4)
              (let* ((state (pwayl-surface-test-state f))
                     (alpha (wl-skia-smoke--plist state :frame-alpha)))
                (check "alpha frame parameter state"
                       (and (floatp alpha) (< (abs (- alpha 0.5)) 0.01))
                       (format "%S" state))))
          (modify-frame-parameters f '((alpha . nil)))
          (wl-skia-smoke--settle 4)))
      (wl-skia-smoke--say "---- %d passed, %d failed ----" pass fail))
    (mapconcat #'identity (nreverse wl-skia-smoke--log) "\n")))

(provide 'wl-skia-smoke)
;;; wl-skia-smoke.el ends here
