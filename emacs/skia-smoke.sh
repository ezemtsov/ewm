#!/usr/bin/env bash
set -euo pipefail

emacs_path="${1:-${EMACS:-emacs}}"

if [[ $# -gt 1 ]]; then
    echo "usage: $0 [EMACS_PATH]" >&2
    exit 2
fi

if [[ -d "$emacs_path" ]]; then
    emacs_path="$emacs_path/src/emacs"
fi

if [[ "$emacs_path" == */* && ! -x "$emacs_path" ]]; then
    echo "error: Emacs executable not found: $emacs_path" >&2
    exit 1
fi

if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
    echo "error: WAYLAND_DISPLAY is not set; run from an EWM/Wayland session" >&2
    exit 1
fi

timeout_seconds="${EWM_SKIA_SMOKE_TIMEOUT:-8}"
tmp_dir="$(mktemp -d -t ewm-skia-smoke.XXXXXX)"
if [[ -n "${EWM_SKIA_SMOKE_KEEP_TMP:-}" ]]; then
    echo "debug: keeping smoke artifacts in $tmp_dir" >&2
else
    trap 'rm -rf "$tmp_dir"' EXIT
fi

payload="$tmp_dir/smoke.el"
stdout="$tmp_dir/stdout"
stderr="$tmp_dir/stderr"

env_args=(GDK_BACKEND=wayland NO_AT_BRIDGE=1)

cat >"$payload" <<'EOF'
(setq debug-on-error t
      inhibit-startup-screen t
      initial-scratch-message nil
      frame-resize-pixelwise t)

(defun ewm-skia-smoke-fail (format-string &rest args)
  (princ (concat "EWM_SKIA_SMOKE_FAIL: "
                 (apply #'format format-string args)
                 "\n")
         #'external-debugging-output)
  (kill-emacs 2))

(defconst ewm-skia-smoke-min-red
  (string-to-number (or (getenv "EWM_SKIA_SMOKE_MIN_RED") "1000")))
(defconst ewm-skia-smoke-min-green
  (string-to-number (or (getenv "EWM_SKIA_SMOKE_MIN_GREEN") "5000")))
(defconst ewm-skia-smoke-min-translucent-green
  (string-to-number
   (or (getenv "EWM_SKIA_SMOKE_MIN_TRANSLUCENT_GREEN") "5000")))

(defun ewm-skia-smoke-framebuffer-size ()
  (unless (fboundp 'pgtk-skia-test-framebuffer-size)
    (ewm-skia-smoke-fail "missing Skia framebuffer size test hook"))
  (or (pgtk-skia-test-framebuffer-size)
      (ewm-skia-smoke-fail "framebuffer size test hook returned nil")))

(defun ewm-skia-smoke-check-framebuffer-capacity (phase)
  (let* ((size (ewm-skia-smoke-framebuffer-size))
         (render-width (nth 0 size))
         (render-height (nth 1 size))
         (texture-width (nth 2 size))
         (texture-height (nth 3 size)))
    (unless (and (<= 1 render-width texture-width)
                 (<= 1 render-height texture-height))
      (ewm-skia-smoke-fail
       "invalid framebuffer size after %s: %S" phase size))
    size))

(defun ewm-skia-smoke-pump ()
  (redisplay 'force)
  (sit-for 0.03))

(defun ewm-skia-smoke-redraw ()
  (force-window-update)
  (redraw-frame)
  (ewm-skia-smoke-pump))

(defun ewm-skia-smoke-check-framebuffer (phase &optional frame force-redraw)
  (unless (fboundp 'pgtk-skia-test-framebuffer-color-counts)
    (ewm-skia-smoke-fail "missing Skia framebuffer readback test hook"))
  (let ((stats nil)
        (ok nil))
    (catch 'done
      (dotimes (_ 10)
        (if force-redraw
            (ewm-skia-smoke-redraw)
          (ewm-skia-smoke-pump))
        (setq stats (pgtk-skia-test-framebuffer-color-counts frame))
        (when (and (consp stats)
                   (>= (nth 2 stats) ewm-skia-smoke-min-red)
                   (>= (nth 3 stats) ewm-skia-smoke-min-green))
          (setq ok t)
          (throw 'done t))
        (sit-for 0.03)))
    (unless ok
      (ewm-skia-smoke-fail
       "framebuffer missing expected red text on green background after %s: stats=%S thresholds=(red=%d green=%d)"
       phase stats ewm-skia-smoke-min-red ewm-skia-smoke-min-green))))

(defun ewm-skia-smoke-check-alpha-background (phase)
  (let ((stats (pgtk-skia-test-framebuffer-color-counts)))
    (unless (and (consp stats)
                 (>= (or (nth 4 stats) 0)
                     ewm-skia-smoke-min-translucent-green))
      (ewm-skia-smoke-fail
       "framebuffer missing translucent green background after %s: stats=%S threshold=%d"
       phase stats ewm-skia-smoke-min-translucent-green))))

(defun ewm-skia-smoke-check-gl-invariants (phase)
  (unless (fboundp 'pgtk-skia-test-gl-invariants)
    (ewm-skia-smoke-fail "missing Skia GL invariant test hook"))
  (let* ((gl-linear 9729)
         (gl-clamp-to-edge 33071)
         (state (pgtk-skia-test-gl-invariants)))
    (unless (and (consp state)
                 (= (nth 0 state) gl-linear)
                 (= (nth 1 state) gl-linear)
                 (= (nth 2 state) gl-clamp-to-edge)
                 (= (nth 3 state) gl-clamp-to-edge)
                 (= (nth 4 state) 0))
      (ewm-skia-smoke-fail
       "unexpected GL invariants after %s: state=%S expected=(%d %d %d %d 0)"
       phase state gl-linear gl-linear gl-clamp-to-edge gl-clamp-to-edge)))
  ;; The hook deliberately clears the framebuffer while testing hostile
  ;; GL state recovery, so redraw before the next visual assertion.
  (ewm-skia-smoke-redraw))

(defun ewm-skia-smoke-check-scratch-state (phase)
  (unless (fboundp 'pgtk-skia-test-scratch-state-preserved)
    (ewm-skia-smoke-fail "missing Skia scratch-state test hook"))
  (let ((state (pgtk-skia-test-scratch-state-preserved)))
    (unless (equal state '(t t t t t))
      (ewm-skia-smoke-fail
       "scratch framebuffer leaked GL state after %s: state=%S"
       phase state))))

(defun ewm-skia-smoke-check-clear-state (phase)
  (unless (fboundp 'pgtk-skia-test-clear-state-preserved)
    (ewm-skia-smoke-fail "missing Skia clear-state test hook"))
  (let ((state (pgtk-skia-test-clear-state-preserved)))
    (unless (equal state '(t t t t t))
      (ewm-skia-smoke-fail
       "framebuffer clear leaked GL state after %s: state=%S"
       phase state)))
  ;; The hook deliberately clears part of the framebuffer while testing
  ;; hostile GL state recovery, so redraw before the next visual assertion.
  (ewm-skia-smoke-redraw))

(defun ewm-skia-smoke-check-setup-state (phase)
  (unless (fboundp 'pgtk-skia-test-setup-state-preserved)
    (ewm-skia-smoke-fail "missing Skia setup-state test hook"))
  (let ((state (pgtk-skia-test-setup-state-preserved)))
    (unless (equal state '(t t t t t))
      (ewm-skia-smoke-fail
       "framebuffer setup leaked GL state after %s: state=%S"
       phase state)))
  ;; The hook runs the production setup clear path, so redraw before
  ;; checking framebuffer pixels again.
  (ewm-skia-smoke-redraw))

(defun ewm-skia-smoke-check-png-export (phase)
  (let* ((data (x-export-frames nil 'png))
         (length (if (stringp data) (length data) 0))
         (prefix (and (stringp data)
                      (substring data 0 (min 8 length)))))
    (unless (and (stringp data)
                 (> length 8)
                 (equal prefix "\211PNG\r\n\032\n"))
      (ewm-skia-smoke-fail
       "bad PNG export after %s: prefix=%S length=%d"
       phase prefix length))))

(defun ewm-skia-smoke-check-image-transform-capabilities (phase)
  (let ((capabilities (image-transforms-p)))
    (unless (and (memq 'scale capabilities)
                 (memq 'rotate90 capabilities))
      (ewm-skia-smoke-fail
       "missing image transform capability after %s: %S"
       phase capabilities))))

(defun ewm-skia-smoke-check-unavailable-export (type expected-message)
  (condition-case err
      (let ((data (x-export-frames nil type)))
        (ewm-skia-smoke-fail
         "%S export unexpectedly succeeded with %d bytes"
         type (if (stringp data) (length data) 0)))
    (error
     (unless (string-match-p expected-message (error-message-string err))
       (ewm-skia-smoke-fail
        "unexpected %S export error: %S" type err)))))

(defun ewm-skia-smoke-xpm (width height rows)
  (concat "/* XPM */
static char *image[] = {
"
          (format "\"%d %d 2 1\",\n" width height)
          "\". c None\",
\"R c #ff0000\",
"
          (mapconcat (lambda (row) (format "\"%s\"" row)) rows ",\n")
          "};"))

(defun ewm-skia-smoke-window-rect (width height)
  (let ((edges (window-inside-pixel-edges)))
    (list (nth 0 edges) (nth 1 edges) width height)))

(defun ewm-skia-smoke-check-scaled-transparent-image (phase)
  (unless (image-type-available-p 'xpm)
    (ewm-skia-smoke-fail "XPM images are not available"))
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create "*ewm-skia-image-smoke*"))
         (xpm (ewm-skia-smoke-xpm
               20 20 (make-list 20 (concat "R" (make-string 19 ?.)))))
         image-rect baseline after)
    (unwind-protect
        (progn
          (set-frame-size nil 40 8)
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert "\n"))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-redraw)
          (setq image-rect (ewm-skia-smoke-window-rect 520 220))
          (setq baseline (pgtk-skia-test-framebuffer-color-counts
                          nil image-rect))
          (with-current-buffer buffer
            (erase-buffer)
            (insert-image
             (create-image xpm 'xpm t :width 480 :height 180)
             " "))
          (ewm-skia-smoke-redraw)
          (setq after (pgtk-skia-test-framebuffer-color-counts nil image-rect))
          (unless (and (consp baseline)
                       (consp after)
                       (> (nth 2 after) 1500)
                       (>= (* (nth 3 after) 100)
                           (* (nth 3 baseline) 90)))
            (ewm-skia-smoke-fail
             "scaled transparent image damaged background after %s: baseline=%S after=%S"
             phase baseline after)))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-rotated-transparent-image (phase)
  (unless (image-type-available-p 'xpm)
    (ewm-skia-smoke-fail "XPM images are not available"))
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create "*ewm-skia-rotated-image-smoke*"))
         (transparent-row (make-string 120 ?.))
         (red-row (make-string 120 ?R))
         (xpm (ewm-skia-smoke-xpm
               120 30
               (append (make-list 20 transparent-row)
                       (make-list 10 red-row))))
         after red-bounds red-width red-height)
    (unwind-protect
        (progn
          (set-frame-size nil 40 8)
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert-image
             (create-image xpm 'xpm t
                           :rotation 90
                           :transform-smoothing nil)
             " "))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-redraw)
          (setq after (pgtk-skia-test-framebuffer-color-counts
                       nil (ewm-skia-smoke-window-rect 180 220)))
          (setq red-bounds (and (consp after) (nth 5 after)))
          (when (consp red-bounds)
            (setq red-width (1+ (- (nth 2 red-bounds) (nth 0 red-bounds)))
                  red-height (1+ (- (nth 3 red-bounds) (nth 1 red-bounds)))))
          (unless (and (consp after)
                       (> (nth 2 after) 800)
                       red-bounds
                       (> red-height (* red-width 2)))
            (ewm-skia-smoke-fail
             "rotated transparent image did not produce a vertical red band after %s: stats=%S red-bounds=%S red-size=%Sx%S"
             phase after red-bounds red-width red-height)))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-sliced-scaled-image (phase)
  (unless (image-type-available-p 'xpm)
    (ewm-skia-smoke-fail "XPM images are not available"))
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create "*ewm-skia-sliced-image-smoke*"))
         (row (concat (make-string 30 ?R) (make-string 90 ?.)))
         (xpm (ewm-skia-smoke-xpm 120 30 (make-list 30 row)))
         (image (create-image xpm 'xpm t
                              :width 480
                              :height 120
                              :transform-smoothing nil))
         red-slice transparent-slice)
    (unwind-protect
        (progn
          (set-frame-size nil 40 8)
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert-image image " " nil '(0 0 120 120)))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-redraw)
          (setq red-slice (pgtk-skia-test-framebuffer-color-counts
                           nil (ewm-skia-smoke-window-rect 160 160)))
          (with-current-buffer buffer
            (erase-buffer)
            (insert-image image " " nil '(240 0 120 120)))
          (ewm-skia-smoke-redraw)
          (setq transparent-slice (pgtk-skia-test-framebuffer-color-counts
                                   nil (ewm-skia-smoke-window-rect 160 160)))
          (unless (and (consp red-slice)
                       (consp transparent-slice)
                       (> (nth 2 red-slice) 10000)
                       (< (nth 2 transparent-slice)
                          (/ (nth 2 red-slice) 20)))
            (ewm-skia-smoke-fail
             "scaled image slice selected wrong source region after %s: red=%S transparent=%S"
             phase red-slice transparent-slice)))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-red-decoration (phase label face min-red)
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create
                  (format "*ewm-skia-%s-smoke*" label)))
         stats)
    (unwind-protect
        (progn
          (set-frame-size nil 44 6)
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert (propertize
                     "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM"
                     'face face)))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-redraw)
          (setq stats (pgtk-skia-test-framebuffer-color-counts
                       nil (ewm-skia-smoke-window-rect 700 120)))
          (unless (and (consp stats)
                       (> (nth 2 stats) min-red))
            (ewm-skia-smoke-fail
             "%s decoration missing red pixels after %s: stats=%S threshold=%d"
             label phase stats min-red)))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-text-decorations (phase)
  (dolist (case `(("underline"
                   (:foreground "#0000ff"
                    :underline (:color "#ff0000" :style line))
                   150)
                  ("overline"
                   (:foreground "#0000ff" :overline "#ff0000")
                   150)
                  ("strike"
                   (:foreground "#0000ff" :strike-through "#ff0000")
                   150)
                  ("box"
                   (:foreground "#0000ff"
                    :box (:line-width 2 :color "#ff0000"))
                   600)))
    (ewm-skia-smoke-check-red-decoration
     phase (nth 0 case) (nth 1 case) (nth 2 case))))

(defun ewm-skia-smoke-check-glyphless-text (phase)
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create "*ewm-skia-glyphless-smoke*"))
         stats)
    (unwind-protect
        (progn
          (set-frame-size nil 44 6)
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)
                          (glyphless-char (:foreground "#ff0000"
                                           :background "#00ff00")
                           glyphless-char)))
            (setq-local glyphless-char-display
                        (make-char-table 'glyphless-char-display nil))
            (set-char-table-range glyphless-char-display #x200b "ZWS")
            (erase-buffer)
            (insert (make-string 12 #x200b)))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-redraw)
          (setq stats (pgtk-skia-test-framebuffer-color-counts
                       nil (ewm-skia-smoke-window-rect 700 120)))
          (unless (and (consp stats)
                       (> (nth 2 stats) 1000))
            (ewm-skia-smoke-fail
             "glyphless text missing red pixels after %s: stats=%S"
             phase stats)))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-composite-text (phase)
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create "*ewm-skia-composite-smoke*"))
         stats)
    (unwind-protect
        (progn
          (set-frame-size nil 44 6)
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (let ((start (point)))
              (insert (propertize "  "
                                  'face '(:foreground "#ff0000"
                                          :background "#00ff00")))
              (compose-region start (point) "MMMMMMMM")))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-redraw)
          (setq stats (pgtk-skia-test-framebuffer-color-counts
                       nil (ewm-skia-smoke-window-rect 700 120)))
          (unless (and (consp stats)
                       (> (nth 2 stats) 400))
            (ewm-skia-smoke-fail
             "composite text missing red pixels after %s: stats=%S"
             phase stats)))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-minspace-font-metrics (phase)
  (let* ((family (face-attribute 'default :family nil t))
         (name (and (stringp family)
                    (format "%s:pixelsize=18:minspace=false" family)))
         (info (and (stringp family)
                    (font-info name))))
    (unless info
      (ewm-skia-smoke-fail
       "could not open :minspace nil font after %s: name=%S"
       phase name))
    (let ((pixel-size (aref info 2))
          (height (aref info 3))
          (ascent (aref info 8))
          (descent (aref info 9)))
      (unless (and (> pixel-size 0)
                   (> height 0)
                   (>= ascent 0)
                   (>= descent 0)
                   (= height (+ ascent descent))
                   (<= height (* pixel-size 4)))
        (ewm-skia-smoke-fail
         "bad :minspace nil font metrics after %s: info=%S"
         phase info)))))

(unless (and (string-match-p "\\bPGTK\\b" system-configuration-features)
             (or (string-match-p "\\bSKIA\\b" system-configuration-features)
                 (fboundp 'pgtk-skia-gl-enabled-p)))
  (princ (format "EWM_SKIA_SMOKE_SKIP: expected PGTK+SKIA, got %s\n"
                 system-configuration-features)
         #'external-debugging-output)
  (kill-emacs 77))

(unless (display-graphic-p)
  (ewm-skia-smoke-fail "display is not graphical"))

(unless (eq window-system 'pgtk)
  (ewm-skia-smoke-fail "window-system is %S, expected pgtk" window-system))

(set-face-attribute 'default nil
                    :foreground "#ff0000"
                    :background "#00ff00"
                    :height 180)
(set-frame-parameter nil 'background-color "#00ff00")
(set-frame-parameter nil 'alpha-background 50)

(let ((buffer (get-buffer-create "*ewm-skia-smoke*")))
  (with-current-buffer buffer
    (erase-buffer)
    (dotimes (line 180)
      (insert (format "skia smoke line %03d: abcdefghijklmnopqrstuvwxyz 0123456789 []{}<>/_-+=\n"
                      line)))
    (goto-char (point-min)))
  (switch-to-buffer buffer))

(set-frame-size nil 96 32)
(redisplay 'force)
(sit-for 0.02)

(dotimes (step 4)
  (set-frame-size nil (+ 80 (* step 8)) (+ 24 step))
  (goto-char (point-min))
  (forward-line (* step 20))
  (redisplay 'force)
  (sit-for 0.02))

(ewm-skia-smoke-check-framebuffer "initial draw")
(ewm-skia-smoke-check-alpha-background "initial draw")
(ewm-skia-smoke-check-framebuffer-capacity "initial draw")
(ewm-skia-smoke-check-gl-invariants "initial draw")
(ewm-skia-smoke-check-scratch-state "initial draw")
(ewm-skia-smoke-check-clear-state "initial draw")
(ewm-skia-smoke-check-setup-state "initial draw")
(ewm-skia-smoke-check-png-export "initial draw")
(ewm-skia-smoke-check-image-transform-capabilities "initial draw")
(ewm-skia-smoke-check-unavailable-export
 'pdf "Skia PDF export is not available")
(ewm-skia-smoke-check-unavailable-export
 'svg "Skia SVG export is not available")
(ewm-skia-smoke-check-scaled-transparent-image "initial draw")
(ewm-skia-smoke-check-rotated-transparent-image "initial draw")
(ewm-skia-smoke-check-sliced-scaled-image "initial draw")
(ewm-skia-smoke-check-text-decorations "initial draw")
(ewm-skia-smoke-check-glyphless-text "initial draw")
(ewm-skia-smoke-check-composite-text "initial draw")
(ewm-skia-smoke-check-minspace-font-metrics "initial draw")

(goto-char (point-min))
(ewm-skia-smoke-redraw)
(dotimes (_ 6)
  (scroll-up-command 3)
  (ewm-skia-smoke-pump))
(ewm-skia-smoke-check-framebuffer "scroll copy")

(let ((largest nil)
      (shrunk nil))
  (set-frame-size nil 124 36)
  (ewm-skia-smoke-redraw)
  (setq largest (ewm-skia-smoke-check-framebuffer-capacity "large resize"))
  (set-frame-size nil 72 20)
  (ewm-skia-smoke-redraw)
  (setq shrunk (ewm-skia-smoke-check-framebuffer-capacity "shrink resize"))
  (unless (and (<= (nth 2 largest) (nth 2 shrunk))
               (<= (nth 3 largest) (nth 3 shrunk)))
    (ewm-skia-smoke-fail
     "framebuffer capacity shrank after resize: large=%S shrunk=%S"
     largest shrunk))
  (ewm-skia-smoke-check-framebuffer "resize capacity")
  (ewm-skia-smoke-check-alpha-background "resize capacity")
  (ewm-skia-smoke-check-gl-invariants "resize capacity")
  (ewm-skia-smoke-check-scratch-state "resize capacity")
  (ewm-skia-smoke-check-clear-state "resize capacity")
  (ewm-skia-smoke-check-setup-state "resize capacity"))

(let ((main-frame (selected-frame)))
  (dotimes (step 3)
    (let ((frame nil))
      (unwind-protect
          (progn
            (setq frame
                  (make-frame `((width . ,(+ 64 (* step 8)))
                                (height . ,(+ 18 step))
                                (minibuffer . nil)
                                (visibility . t))))
            (select-frame-set-input-focus frame)
            (switch-to-buffer "*ewm-skia-smoke*")
            (goto-char (point-min))
            (forward-line (* step 30))
            (redisplay 'force)
            (sit-for 0.02)
            (unless (and (fboundp 'pgtk-skia-gl-enabled-p)
                         (pgtk-skia-gl-enabled-p frame))
              (ewm-skia-smoke-fail
               "Skia GL acceleration is not active for lifecycle frame %d"
               step)))
        (when (and frame (frame-live-p frame))
          (delete-frame frame t)
          (select-frame-set-input-focus main-frame)
          (redisplay 'force)
          (sit-for 0.01)))))
  (select-frame-set-input-focus main-frame)
  (raise-frame main-frame)
  (redisplay 'force)
  (sit-for 0.05))

(unless (and (fboundp 'pgtk-skia-gl-enabled-p)
             (pgtk-skia-gl-enabled-p))
  (ewm-skia-smoke-fail "Skia GL acceleration is not active"))

(unless (fboundp 'pgtk-skia-test-visible-bell-active-p)
  (ewm-skia-smoke-fail "missing Skia visible-bell test hook"))
(unless (fboundp 'pgtk-skia-test-recover-visible-bell)
  (ewm-skia-smoke-fail "missing Skia visible-bell recovery test hook"))
(let ((visible-bell t)
      (ring-bell-function nil))
  (ding)
  (unless (pgtk-skia-test-visible-bell-active-p)
    (ewm-skia-smoke-fail "visible bell did not create a Skia snapshot"))
  (unless (pgtk-skia-test-recover-visible-bell)
    (ewm-skia-smoke-fail "visible bell recovery hook found no snapshot"))
  (when (pgtk-skia-test-visible-bell-active-p)
    (ewm-skia-smoke-fail "visible bell Skia snapshot was not released"))
  (ewm-skia-smoke-check-framebuffer "visible bell restore"))

(let ((main-frame (selected-frame))
      (bell-frame nil)
      (visible-bell t)
      (ring-bell-function nil))
  (unwind-protect
      (progn
        (setq bell-frame
              (make-frame '((width . 64)
                            (height . 18)
                            (minibuffer . nil)
                            (visibility . t))))
        (select-frame-set-input-focus bell-frame)
        (switch-to-buffer "*ewm-skia-smoke*")
        (ewm-skia-smoke-redraw)
        (ding)
        (unless (pgtk-skia-test-visible-bell-active-p bell-frame)
          (ewm-skia-smoke-fail
           "visible bell did not create a Skia snapshot on deletion frame"))
        (delete-frame bell-frame t)
        (setq bell-frame nil)
        (select-frame-set-input-focus main-frame)
        (redisplay 'force)
        (sit-for 0.03))
    (when (and bell-frame (frame-live-p bell-frame))
      (delete-frame bell-frame t))
    (select-frame-set-input-focus main-frame)))

(unless (fboundp 'pgtk-skia-test-abandon-gl-context)
  (ewm-skia-smoke-fail "missing Skia forced-abandon test hook"))
(unless (fboundp 'pgtk-skia-test-gl-recreate-source-active-p)
  (ewm-skia-smoke-fail "missing Skia stale-source test hook"))

(let ((main-frame (selected-frame))
      (lost-frame nil))
  (unwind-protect
      (progn
        (setq lost-frame
              (make-frame '((width . 64)
                            (height . 18)
                            (minibuffer . nil)
                            (visibility . t))))
        (select-frame-set-input-focus lost-frame)
        (switch-to-buffer "*ewm-skia-smoke*")
        (ewm-skia-smoke-redraw)
        (unless (pgtk-skia-test-abandon-gl-context lost-frame)
          (ewm-skia-smoke-fail
           "test abandon hook found no Skia GL state on deletion frame"))
        (let ((deleted-frame lost-frame))
          (delete-frame lost-frame t)
          (setq lost-frame nil)
          (when (pgtk-skia-test-gl-recreate-source-active-p deleted-frame)
            (ewm-skia-smoke-fail
             "Skia GL recreate source survived deleted frame")))
        (select-frame-set-input-focus main-frame))
    (when (and lost-frame (frame-live-p lost-frame))
      (delete-frame lost-frame t))
    (select-frame-set-input-focus main-frame)))

(let ((frame (selected-frame))
      (restored nil))
  (unless (pgtk-skia-test-abandon-gl-context frame)
    (ewm-skia-smoke-fail
     "test abandon hook found no Skia GL state or failed cleanup"))
  (dotimes (_ 20)
    (select-frame-set-input-focus frame)
    (goto-char (point-min))
    (set-frame-size frame 96 32)
    (redisplay 'force)
    (sit-for 0.05)
    (when (pgtk-skia-gl-enabled-p frame)
      (setq restored t)))
  (unless restored
    (ewm-skia-smoke-fail
     "Skia GL state did not recover after forced abandon"))
  (ewm-skia-smoke-check-framebuffer "forced abandon recovery" frame))

(princ "EWM_SKIA_SMOKE_OK\n" #'external-debugging-output)
(kill-emacs 0)
EOF

set +e
timeout --kill-after=1s "$timeout_seconds" \
    env "${env_args[@]}" \
    "$emacs_path" -Q --load "$payload" >"$stdout" 2>"$stderr"
status=$?
set -e

if [[ $status -eq 124 || $status -eq 137 ]]; then
    echo "error: Skia smoke timed out after ${timeout_seconds}s" >&2
    tail -n 80 "$stderr" >&2
    exit 1
fi

if [[ $status -eq 77 ]]; then
    cat "$stderr" >&2
    exit 77
fi

if grep -E -q 'Skia: (EGL error|No surface available|GL surface creation failed|GL make-current-failed|Failed to create GL surface)|GtkGLArea make-current' "$stderr"; then
    echo "error: Skia presentation failure detected" >&2
    tail -n 120 "$stderr" >&2
    exit 1
fi

if [[ $status -ne 0 ]]; then
    echo "error: Emacs exited with status $status" >&2
    tail -n 120 "$stderr" >&2
    exit "$status"
fi

if ! grep -q 'EWM_SKIA_SMOKE_OK' "$stderr" "$stdout"; then
    echo "error: smoke test did not report success" >&2
    tail -n 120 "$stderr" >&2
    exit 1
fi

echo "ok: Skia PGTK redisplay smoke passed"
