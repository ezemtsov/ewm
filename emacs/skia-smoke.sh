#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
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

timeout_seconds="${EWM_SKIA_SMOKE_TIMEOUT:-30}"
tmp_dir="$(mktemp -d -t ewm-skia-smoke.XXXXXX)"
if [[ -n "${EWM_SKIA_SMOKE_KEEP_TMP:-}" ]]; then
    echo "debug: keeping smoke artifacts in $tmp_dir" >&2
else
    trap 'rm -rf "$tmp_dir"' EXIT
fi

payload="$tmp_dir/smoke.el"
stdout="$tmp_dir/stdout"
stderr="$tmp_dir/stderr"

env_args=(GDK_BACKEND=wayland NO_AT_BRIDGE=1
          EWM_SKIA_SMOKE_TIMEOUT_SECONDS="$timeout_seconds"
          EWM_SKIA_HARNESS_MONITOR_EL="$script_dir/skia-monitor.el")

cat >"$payload" <<'EOF'
(setq debug-on-error nil
      inhibit-startup-screen t
      initial-scratch-message nil
      frame-resize-pixelwise t)

(defun ewm-skia-smoke-fail (format-string &rest args)
  (princ (concat "EWM_SKIA_SMOKE_FAIL: "
                 (apply #'format format-string args)
                 "\n")
         #'external-debugging-output)
  (kill-emacs 2))

(defun ewm-skia-smoke-dump-messages ()
  (let ((buffer (get-buffer "*Messages*")))
    (when buffer
      (princ "EWM_SKIA_SMOKE_MESSAGES_BEGIN\n"
             #'external-debugging-output)
      (with-current-buffer buffer
        (princ
         (buffer-substring-no-properties
          (max (point-min) (- (point-max) 6000))
          (point-max))
         #'external-debugging-output))
      (princ "\nEWM_SKIA_SMOKE_MESSAGES_END\n"
             #'external-debugging-output))))

(defun ewm-skia-smoke-report-error (err)
  (princ (format "EWM_SKIA_SMOKE_ERROR: %S: %s\n"
                 err (error-message-string err))
         #'external-debugging-output)
  (ewm-skia-smoke-dump-messages)
  (kill-emacs 2))

(defun ewm-skia-smoke-timeout-seconds ()
  (max 1
       (string-to-number
        (or (getenv "EWM_SKIA_SMOKE_TIMEOUT_SECONDS") "30"))))

(defun ewm-skia-smoke-watchdog-timeout ()
  (princ (format "EWM_SKIA_SMOKE_TIMEOUT: Emacs watchdog fired after %ds\n"
                 (ewm-skia-smoke-timeout-seconds))
         #'external-debugging-output)
  (ewm-skia-smoke-dump-messages)
  (kill-emacs 2))

(defvar ewm-skia-smoke-watchdog
  (run-at-time (max 1 (- (ewm-skia-smoke-timeout-seconds) 1))
               nil #'ewm-skia-smoke-watchdog-timeout))

(defconst ewm-skia-smoke-min-red
  (string-to-number (or (getenv "EWM_SKIA_SMOKE_MIN_RED") "1000")))
(defconst ewm-skia-smoke-min-green
  (string-to-number (or (getenv "EWM_SKIA_SMOKE_MIN_GREEN") "5000")))
(defconst ewm-skia-smoke-min-translucent-green
  (string-to-number
   (or (getenv "EWM_SKIA_SMOKE_MIN_TRANSLUCENT_GREEN") "5000")))
(defconst ewm-skia-smoke-required-scale
  (let ((scale (getenv "EWM_SKIA_SMOKE_REQUIRE_SCALE")))
    (and scale (> (length scale) 0) (string-to-number scale))))
(defconst ewm-skia-smoke-monitor-policy
  (let ((monitor (getenv "EWM_SKIA_SMOKE_MONITOR")))
    (and monitor (> (length monitor) 0) monitor)))
(defconst ewm-skia-smoke-require-monitor
  (getenv "EWM_SKIA_SMOKE_REQUIRE_MONITOR"))
(defconst ewm-skia-smoke-selected-groups
  (let ((groups (getenv "EWM_SKIA_SMOKE_GROUPS")))
    (if (and groups (> (length groups) 0))
        (split-string groups "[,[:space:]]+" t)
      '("all"))))
(defconst ewm-skia-smoke-lifecycle-groups
  '("tooltip" "frames" "bell" "recovery"))

(defun ewm-skia-smoke-group-selected-p (group)
  (or (member group ewm-skia-smoke-selected-groups)
      (and (member "lifecycle" ewm-skia-smoke-selected-groups)
           (member group ewm-skia-smoke-lifecycle-groups))))

(defun ewm-skia-smoke-group-enabled-p (&rest groups)
  (or (member "all" ewm-skia-smoke-selected-groups)
      (catch 'found
        (dolist (group groups nil)
          (when (ewm-skia-smoke-group-selected-p group)
            (throw 'found t))))))

(defmacro ewm-skia-smoke-run-group (groups &rest body)
  (declare (indent 1))
  `(when (ewm-skia-smoke-group-enabled-p
          ,@(if (listp groups) groups (list groups)))
     ,@body))

(load-file (getenv "EWM_SKIA_HARNESS_MONITOR_EL"))

(defun ewm-skia-smoke-select-monitor ()
  (when ewm-skia-smoke-monitor-policy
    (let ((monitor
           (ewm-skia-harness-find-monitor ewm-skia-smoke-monitor-policy)))
      (unless monitor
        (ewm-skia-smoke-fail
         "monitor %S not found" ewm-skia-smoke-monitor-policy))
      (princ (concat (ewm-skia-harness-monitor-string
                      "EWM_SKIA_SMOKE_TARGET_MONITOR" monitor)
                     "\n")
             #'external-debugging-output)
      (ewm-skia-harness-position-frame-on-monitor (selected-frame) monitor)
      monitor)))

(defun ewm-skia-smoke-check-actual-monitor (target)
  (when target
    (let ((actual (frame-monitor-attributes)))
      (princ (concat (ewm-skia-harness-monitor-string
                      "EWM_SKIA_SMOKE_ACTUAL_MONITOR" actual)
                     "\n")
             #'external-debugging-output)
      (when (and ewm-skia-smoke-require-monitor
                 (not (ewm-skia-harness-same-monitor-p target actual)))
        (ewm-skia-smoke-fail
         "frame landed on wrong monitor: target=%S actual=%S"
         target actual)))))

(defun ewm-skia-smoke-framebuffer-size ()
  (unless (fboundp 'pgtk-skia-test-framebuffer-size)
    (ewm-skia-smoke-fail "missing Skia framebuffer size test hook"))
  (or (pgtk-skia-test-framebuffer-size)
      (ewm-skia-smoke-fail "framebuffer size test hook returned nil")))

(defun ewm-skia-smoke-check-framebuffer-size (phase)
  (let* ((size (ewm-skia-smoke-framebuffer-size))
         (render-width (nth 0 size))
         (render-height (nth 1 size))
         (texture-width (nth 2 size))
         (texture-height (nth 3 size))
         (logical-width (nth 4 size))
         (logical-height (nth 5 size))
         (scale (nth 6 size)))
    (unless (and (= render-width texture-width)
                 (= render-height texture-height)
                 (<= 1 render-width)
                 (<= 1 render-height))
      (ewm-skia-smoke-fail
       "framebuffer storage is not exact after %s: %S" phase size))
    (unless (and (integerp logical-width)
                 (integerp logical-height)
                 (integerp scale)
                 (<= 1 logical-width)
                 (<= 1 logical-height)
                 (<= 1 scale)
                 (= render-width (* logical-width scale))
                 (= render-height (* logical-height scale)))
      (ewm-skia-smoke-fail
       "invalid framebuffer scale after %s: %S" phase size))
    (when (and ewm-skia-smoke-required-scale
               (/= scale ewm-skia-smoke-required-scale))
      (ewm-skia-smoke-fail
       "framebuffer scale after %s is %d, expected %d: %S"
       phase scale ewm-skia-smoke-required-scale size))
    size))

(defun ewm-skia-smoke-check-force-surface-update (phase)
  (unless (fboundp 'pgtk-skia-test-force-surface-update)
    (ewm-skia-smoke-fail "missing Skia force surface update test hook"))
  (unless (pgtk-skia-test-force-surface-update)
    (ewm-skia-smoke-fail
     "forced Skia surface update kept the old surface after %s" phase))
  (ewm-skia-smoke-redraw)
  (ewm-skia-smoke-check-framebuffer "forced surface update")
  (ewm-skia-smoke-check-framebuffer-size "forced surface update"))

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

(defun ewm-skia-smoke-check-gl-enabled (frame phase)
  (unless (fboundp 'pgtk-skia-gl-enabled-p)
    (ewm-skia-smoke-fail "missing Skia GL enabled test hook"))
  (let ((enabled nil))
    (catch 'done
      (dotimes (_ 10)
        (redisplay 'force)
        (sit-for 0.03)
        (when (pgtk-skia-gl-enabled-p frame)
          (setq enabled t)
          (throw 'done t))))
    (unless enabled
      (ewm-skia-smoke-fail
       "Skia GL acceleration is not active after %s" phase))))

(defun ewm-skia-smoke-context-state (frame phase)
  (unless (fboundp 'pgtk-skia-test-context-state)
    (ewm-skia-smoke-fail "missing Skia context-state test hook"))
  (let ((state (pgtk-skia-test-context-state frame t)))
    (unless (and (consp state)
                 (nth 0 state)
                 (nth 1 state)
                 (eq (nth 2 state) t)
                 (nth 3 state)
                 (integerp (nth 4 state))
                 (integerp (nth 5 state))
                 (nth 6 state)
                 (nth 7 state))
      (ewm-skia-smoke-fail
       "invalid Skia context state after %s: state=%S" phase state))
    state))

(defun ewm-skia-smoke-context-share-id (state)
  (or (nth 8 state) (nth 0 state)))

(defun ewm-skia-smoke-check-context-ownership (phase)
  (let ((main-frame (selected-frame))
        (frames nil)
        (states nil)
        (policy (getenv "EWM_SKIA_SMOKE_GR_CONTEXT_POLICY")))
    (unwind-protect
        (progn
          (dotimes (index 2)
            (let ((frame (make-frame `((width . 64)
                                       (height . 16)
                                       (minibuffer . nil)
                                       (visibility . t)))))
              (push frame frames)
              (select-frame-set-input-focus frame)
              (switch-to-buffer "*ewm-skia-smoke*")
              (goto-char (point-min))
              (forward-line (* index 20))
              (ewm-skia-smoke-settle-framebuffer
               frame (format "%s frame %d" phase index) 64 16 200 100)
              (ewm-skia-smoke-check-framebuffer
               (format "%s frame %d" phase index) frame t)
              (push (ewm-skia-smoke-context-state
                     frame (format "%s frame %d" phase index))
                    states)))
          (setq frames (nreverse frames)
                states (nreverse states))
          (ewm-skia-smoke-context-state
           (cadr frames) (format "%s force second frame current" phase))
          (let ((wrong-current
                 (pgtk-skia-test-context-state (car frames) nil)))
            (when (eq (nth 2 wrong-current) t)
              (ewm-skia-smoke-fail
               "context state did not expose wrong current frame after %s: state=%S"
               phase wrong-current)))
          (select-frame-set-input-focus (car frames))
          (ewm-skia-smoke-check-framebuffer
           (format "%s return to first frame" phase) (car frames) t)
          (setq states
                (list (ewm-skia-smoke-context-state
                       (car frames) (format "%s return first frame" phase))
                      (ewm-skia-smoke-context-state
                       (cadr frames) (format "%s return second frame" phase))))
          (when (and (equal (nth 3 (car states)) (nth 3 (cadr states)))
                     (not (equal (ewm-skia-smoke-context-share-id (car states))
                                 (ewm-skia-smoke-context-share-id (cadr states)))))
            (ewm-skia-smoke-fail
             "Skia direct context is shared across unrelated GDK GL contexts after %s: states=%S"
             phase states))
          (unless (fboundp 'pgtk-skia-test-texture-shared-p)
            (ewm-skia-smoke-fail
             "missing Skia texture-sharing test hook"))
          (let ((texture-shared
                 (pgtk-skia-test-texture-shared-p
                  (car frames) (cadr frames))))
            (princ (format "EWM_SKIA_SMOKE_GL_TEXTURE_SHARED=%S\n"
                           texture-shared)
                   #'external-debugging-output)
            (when (and (equal (nth 3 (car states)) (nth 3 (cadr states)))
                       (not texture-shared))
              (ewm-skia-smoke-fail
               "shared Skia direct context requires GL object sharing after %s: states=%S"
               phase states))
            (when (and (equal (ewm-skia-smoke-context-share-id (car states))
                              (ewm-skia-smoke-context-share-id (cadr states)))
                       (not texture-shared))
              (ewm-skia-smoke-fail
               "equal GDK GL share identity without GL object sharing after %s: states=%S"
               phase states)))
          (pcase policy
            ("shared"
             (unless (equal (nth 3 (car states)) (nth 3 (cadr states)))
               (ewm-skia-smoke-fail
                "expected shared Skia direct context after %s: states=%S"
                phase states))
             (unless (equal (ewm-skia-smoke-context-share-id (car states))
                            (ewm-skia-smoke-context-share-id (cadr states)))
               (ewm-skia-smoke-fail
                "shared Skia direct context requires a shared GDK GL context after %s: states=%S"
                phase states)))
            ("per-frame"
             (when (equal (nth 3 (car states)) (nth 3 (cadr states)))
               (ewm-skia-smoke-fail
                "expected per-frame Skia direct contexts after %s: states=%S"
                phase states)))
            ((and (pred identity)
                  (pred (lambda (value)
                          (not (member value '("shared" "per-frame"))))))
             (ewm-skia-smoke-fail
              "unknown EWM_SKIA_SMOKE_GR_CONTEXT_POLICY=%S" policy))))
      (dolist (frame frames)
        (when (frame-live-p frame)
          (delete-frame frame t)))
      (select-frame-set-input-focus main-frame)
      (redisplay 'force)
      (sit-for 0.03))
    (ewm-skia-smoke-check-main-frame-rendering
     main-frame (format "after %s deletion" phase))))

(defun ewm-skia-smoke-check-main-frame-rendering (frame phase)
  (select-frame-set-input-focus frame)
  (switch-to-buffer "*ewm-skia-smoke*")
  (goto-char (point-min))
  (ewm-skia-smoke-redraw)
  (ewm-skia-smoke-check-gl-enabled frame phase)
  (ewm-skia-smoke-check-framebuffer phase frame t))

(defun ewm-skia-smoke-settle-framebuffer
    (frame phase columns rows min-logical-width min-logical-height)
  (unless (fboundp 'pgtk-skia-test-framebuffer-size)
    (ewm-skia-smoke-fail "missing Skia framebuffer size test hook"))
  (let ((size nil)
        (settled nil))
    (catch 'done
      (dotimes (_ 30)
        (when (and columns rows)
          (set-frame-size frame columns rows))
        (with-selected-frame frame
          (force-window-update)
          (redraw-frame frame)
          (redisplay 'force))
        (sit-for 0.05)
        (setq size (pgtk-skia-test-framebuffer-size frame))
        (when (and (consp size)
                   (>= (nth 4 size) min-logical-width)
                   (>= (nth 5 size) min-logical-height))
          (setq settled t)
          (throw 'done t))))
    (unless settled
      (ewm-skia-smoke-fail
       "framebuffer did not settle after %s: size=%S expected-logical>=(%d %d)"
       phase size min-logical-width min-logical-height))))

(defun ewm-skia-smoke-resize-selected-frame (columns rows phase)
  (ewm-skia-smoke-settle-framebuffer
   (selected-frame) phase columns rows 1 1))

(defun ewm-skia-smoke-check-alpha-background (phase)
  (let ((stats (pgtk-skia-test-framebuffer-color-counts)))
    (unless (and (consp stats)
                 (>= (or (nth 4 stats) 0)
                     ewm-skia-smoke-min-translucent-green))
      (ewm-skia-smoke-fail
       "framebuffer missing translucent green background after %s: stats=%S threshold=%d"
       phase stats ewm-skia-smoke-min-translucent-green))))

(defun ewm-skia-smoke-check-partial-clear (phase)
  (unless (fboundp 'pgtk-skia-test-clear-area)
    (ewm-skia-smoke-fail "missing Skia clear-area test hook"))
  (let* ((rects (pgtk-skia-test-clear-area))
         (left (and (consp rects)
                    (pgtk-skia-test-framebuffer-color-counts
                     nil (nth 0 rects))))
         (cleared (and (consp rects)
                       (pgtk-skia-test-framebuffer-color-counts
                        nil (nth 1 rects))))
         (right (and (consp rects)
                     (pgtk-skia-test-framebuffer-color-counts
                      nil (nth 2 rects)))))
    (unless (and (consp left)
                 (> (nth 2 left) 1000)
                 (consp cleared)
                 (< (nth 2 cleared) 50)
                 (> (nth 3 cleared) 1000)
                 (> (nth 4 cleared) 1000)
                 (consp right)
                 (> (nth 2 right) 1000))
      (ewm-skia-smoke-fail
       "partial clear-area lost alpha/background isolation after %s: rects=%S left=%S cleared=%S right=%S"
       phase rects left cleared right)))
  (ewm-skia-smoke-redraw))

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

(defun ewm-skia-smoke-check-copy-bits-gl-error (phase)
  (unless (fboundp 'pgtk-skia-test-copy-bits-gl-error-return)
    (ewm-skia-smoke-fail "missing Skia copy-bits error test hook"))
  (unless (pgtk-skia-test-copy-bits-gl-error-return)
    (ewm-skia-smoke-fail
     "copy_bits GL blit failure did not report fallback after %s"
     phase))
  (ewm-skia-smoke-redraw))

(defun ewm-skia-smoke-check-copy-bits-fallback (phase)
  (unless (fboundp 'pgtk-skia-test-copy-bits-snapshot-fallback)
    (ewm-skia-smoke-fail "missing Skia copy-bits fallback test hook"))
  (let* ((rect (pgtk-skia-test-copy-bits-snapshot-fallback))
         (stats (and (consp rect)
                     (pgtk-skia-test-framebuffer-color-counts nil rect))))
    (unless (and (consp stats) (> (nth 2 stats) 1000))
      (ewm-skia-smoke-fail
       "copy_bits snapshot fallback did not render copied red pixels after %s: rect=%S stats=%S"
       phase rect stats)))
  (ewm-skia-smoke-redraw))

(defun ewm-skia-smoke-check-horizontal-copy-bits (phase)
  (unless (fboundp 'pgtk-skia-test-copy-bits-horizontal-overlap)
    (ewm-skia-smoke-fail "missing Skia horizontal copy-bits test hook"))
  (let* ((rects (pgtk-skia-test-copy-bits-horizontal-overlap))
         (dst (and (consp rects) (nth 0 rects)))
         (guard (and (consp rects) (nth 1 rects)))
         (dst-stats (and (consp dst)
                         (pgtk-skia-test-framebuffer-color-counts nil dst)))
         (guard-stats (and (consp guard)
                           (pgtk-skia-test-framebuffer-color-counts
                            nil guard))))
    (unless (and (consp dst-stats) (> (nth 2 dst-stats) 1000))
      (ewm-skia-smoke-fail
       "horizontal copy_bits did not copy red pixels after %s: rects=%S stats=%S"
       phase rects dst-stats))
    (unless (and (consp guard-stats)
                 (< (nth 2 guard-stats) 50)
                 (> (nth 3 guard-stats) 1000))
      (ewm-skia-smoke-fail
       "horizontal copy_bits copied past destination after %s: rects=%S stats=%S"
       phase rects guard-stats)))
  (ewm-skia-smoke-redraw))

(defun ewm-skia-smoke-check-copy-bits-scissor-isolation (phase)
  (unless (fboundp 'pgtk-skia-test-copy-bits-scissor-isolation)
    (ewm-skia-smoke-fail
     "missing Skia copy-bits scissor-isolation test hook"))
  (let* ((rect (pgtk-skia-test-copy-bits-scissor-isolation))
         (stats (and (consp rect)
                     (pgtk-skia-test-framebuffer-color-counts nil rect))))
    (unless (and (consp stats) (> (nth 2 stats) 1000))
      (ewm-skia-smoke-fail
       "copy_bits inherited stale GL scissor after %s: rect=%S stats=%S"
       phase rect stats)))
  (ewm-skia-smoke-redraw))

(defun ewm-skia-smoke-rect-contains-p (outer inner &optional slack)
  (let ((slack (or slack 0)))
    (and (consp outer)
         (consp inner)
         (<= (nth 0 outer) (+ (nth 0 inner) slack))
         (<= (nth 1 outer) (+ (nth 1 inner) slack))
         (>= (+ (nth 0 outer) (nth 2 outer))
             (- (+ (nth 0 inner) (nth 2 inner)) slack))
         (>= (+ (nth 1 outer) (nth 3 outer))
             (- (+ (nth 1 inner) (nth 3 inner)) slack)))))

(defun ewm-skia-smoke-check-copy-bits-damage (phase)
  (unless (fboundp 'pgtk-skia-test-copy-bits-damage)
    (ewm-skia-smoke-fail "missing Skia copy-bits damage test hook"))
  (let* ((result (pgtk-skia-test-copy-bits-damage))
         (dst (and (consp result) (nth 0 result)))
         (damage (and (consp result) (nth 1 result)))
         (valid (and (consp result) (nth 2 result)))
         (full (and (consp result) (nth 3 result)))
         (dst-stats (and (consp dst)
                         (pgtk-skia-test-framebuffer-color-counts nil dst))))
    (unless (and (consp dst-stats) (> (nth 2 dst-stats) 1000))
      (ewm-skia-smoke-fail
       "copy_bits damage setup did not copy red pixels after %s: result=%S stats=%S"
       phase result dst-stats))
    (unless (and valid
                 (or full
                     (ewm-skia-smoke-rect-contains-p damage dst 1)))
      (ewm-skia-smoke-fail
       "copy_bits did not mark destination damage after %s: result=%S stats=%S"
       phase result dst-stats)))
  (ewm-skia-smoke-redraw))

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

(defun ewm-skia-smoke-u32be (data offset)
  (+ (ash (aref data offset) 24)
     (ash (aref data (1+ offset)) 16)
     (ash (aref data (+ offset 2)) 8)
     (aref data (+ offset 3))))

(defun ewm-skia-smoke-check-png-export (phase)
  (let* ((data (x-export-frames nil 'png))
         (length (if (stringp data) (length data) 0))
         (prefix (and (stringp data)
                      (substring data 0 (min 8 length))))
         (width (and (stringp data) (>= length 24)
                     (ewm-skia-smoke-u32be data 16)))
         (height (and (stringp data) (>= length 24)
                      (ewm-skia-smoke-u32be data 20))))
    (unless (and (stringp data)
                 (> length 24)
                 (equal prefix "\211PNG\r\n\032\n")
                 (equal (substring data 12 16) "IHDR")
                 (= width (frame-pixel-width))
                 (= height (frame-pixel-height)))
      (ewm-skia-smoke-fail
       "bad PNG export after %s: prefix=%S length=%d size=%S expected=%S"
       phase prefix length (list width height)
       (list (frame-pixel-width) (frame-pixel-height))))))

(defun ewm-skia-smoke-check-image-transform-capabilities (phase)
  (let ((capabilities (image-transforms-p)))
    (unless (and (memq 'scale capabilities)
                 (memq 'rotate90 capabilities))
      (ewm-skia-smoke-fail
       "missing image transform capability after %s: %S"
       phase capabilities))))

(defun ewm-skia-smoke-export-supported-p (type data)
  (pcase type
    ('pdf
     (and (stringp data)
          (> (length data) 5)
          (equal (substring data 0 5) "%PDF-")
          (string-match-p "%%EOF[ \t\r\n]*\\'" data)
          (string-match-p
           (format "/MediaBox[ \t\r\n]*\\[[ \t\r\n]*0[ \t\r\n]+0[ \t\r\n]+%d\\(?:\\.0+\\)?[ \t\r\n]+%d\\(?:\\.0+\\)?[ \t\r\n]*\\]"
                   (frame-pixel-width) (frame-pixel-height))
           data)))
    ('svg
     (and (stringp data)
          (string-match-p "<svg\\b" data)
          (string-match-p "</svg>[ \t\r\n]*\\'" data)
          (string-match-p
           (format "\\bwidth=\"%d\\(?:\\.0+\\)?\\(?:px\\)?\""
                   (frame-pixel-width))
           data)
          (string-match-p
           (format "\\bheight=\"%d\\(?:\\.0+\\)?\\(?:px\\)?\""
                   (frame-pixel-height))
           data)))
    (_ nil)))

(defun ewm-skia-smoke-check-optional-export
    (phase type expected-unavailable-message)
  (condition-case err
      (let ((data (x-export-frames nil type)))
        (unless (ewm-skia-smoke-export-supported-p type data)
          (ewm-skia-smoke-fail
           "bad %S export after %s: length=%d prefix=%S"
           type phase (if (stringp data) (length data) 0)
           (and (stringp data)
                (substring data 0 (min 32 (length data)))))))
    (error
     (unless (string-match-p expected-unavailable-message
                             (error-message-string err))
       (ewm-skia-smoke-fail
        "unexpected %S export error after %s: %S" type phase err)))))

(defun ewm-skia-smoke-check-export-input-unwind (phase)
  (unless (fboundp 'pgtk-skia-test-input-unwind-preserved-p)
    (ewm-skia-smoke-fail "missing Skia input-block unwind test hook"))
  (unless (pgtk-skia-test-input-unwind-preserved-p)
    (ewm-skia-smoke-fail
     "Skia input block leaked after %s" phase)))

(defun ewm-skia-smoke-check-recovered-surface-operations (phase)
  (ewm-skia-smoke-check-framebuffer phase nil t)
  (ewm-skia-smoke-check-copy-bits-fallback phase)
  (ewm-skia-smoke-check-horizontal-copy-bits phase)
  (ewm-skia-smoke-check-png-export phase))

(defun ewm-skia-smoke-wait-visible-bell-release (&optional frame)
  (let ((released nil))
    (catch 'done
      (dotimes (_ 150)
        (unless (pgtk-skia-test-visible-bell-active-p frame)
          (setq released t)
          (throw 'done t))
        (sit-for 0.02)))
    released))

(defun ewm-skia-smoke-check-tooltip-frame (phase)
  (let ((main-frame (selected-frame))
        (old-use-system-tooltips use-system-tooltips)
        (old-x-gtk-use-system-tooltips x-gtk-use-system-tooltips)
        (old-tooltip-reuse-hidden-frame tooltip-reuse-hidden-frame)
        tooltip-frame stats ok)
    (unwind-protect
        (progn
          (unless (fboundp 'pgtk-skia-test-current-tooltip-frame)
            (ewm-skia-smoke-fail
             "missing Skia tooltip frame test hook"))
          (setq use-system-tooltips nil
                x-gtk-use-system-tooltips nil
                tooltip-reuse-hidden-frame nil)
          (x-show-tip
           (propertize "Skia tooltip smoke"
                       'face '(:foreground "#ff0000"
                               :background "#00ff00"))
           nil
           '((name . "skia-tooltip-smoke")
             (left . 40)
             (top . 40)
             (foreground-color . "#ff0000")
             (background-color . "#00ff00")
             (border-color . "#ff0000"))
           1 0 0)
          (catch 'done
            (dotimes (_ 10)
              (redisplay 'force)
              (sit-for 0.03)
              (setq tooltip-frame
                    (pgtk-skia-test-current-tooltip-frame))
              (when tooltip-frame
                (throw 'done t))))
          (unless tooltip-frame
            (ewm-skia-smoke-fail
             "tooltip frame was not visible after %s" phase))
          (ewm-skia-smoke-check-gl-enabled tooltip-frame
                                           (format "tooltip %s" phase))
          (catch 'done
            (dotimes (_ 10)
              (redisplay 'force)
              (sit-for 0.03)
              (setq stats
                    (pgtk-skia-test-framebuffer-color-counts tooltip-frame))
              (when (and (consp stats)
                         (> (nth 2 stats) 50)
                         (> (nth 3 stats) 200))
                (setq ok t)
                (throw 'done t))))
          (unless ok
            (ewm-skia-smoke-fail
             "tooltip frame missing rendered red/green pixels after %s: stats=%S"
             phase stats)))
      (x-hide-tip)
      (setq use-system-tooltips old-use-system-tooltips
            x-gtk-use-system-tooltips old-x-gtk-use-system-tooltips
            tooltip-reuse-hidden-frame old-tooltip-reuse-hidden-frame))
    (ewm-skia-smoke-check-main-frame-rendering
     main-frame (format "tooltip cleanup %s" phase))))

(defun ewm-skia-smoke-check-temporary-frame
    (phase parameters columns rows &optional line check-inherited-buffer)
  (let ((main-frame (selected-frame))
        (frame nil))
    (unwind-protect
        (progn
          (setq frame (make-frame parameters))
          (select-frame-set-input-focus frame)
          (when check-inherited-buffer
            (unless (eq (window-buffer (frame-selected-window frame))
                        (get-buffer "*ewm-skia-smoke*"))
              (ewm-skia-smoke-fail
               "%s did not inherit smoke buffer" phase)))
          (switch-to-buffer "*ewm-skia-smoke*")
          (goto-char (point-min))
          (forward-line (or line 0))
          (ewm-skia-smoke-settle-framebuffer
           frame phase columns rows 200 100)
          (ewm-skia-smoke-check-gl-enabled frame phase)
          (ewm-skia-smoke-check-framebuffer phase frame t))
      (when (and frame (frame-live-p frame))
        (delete-frame frame t))
      (select-frame-set-input-focus main-frame)
      (redisplay 'force)
      (sit-for 0.01))
    (ewm-skia-smoke-check-main-frame-rendering
     main-frame (format "after %s deletion" phase))))

(defun ewm-skia-smoke-check-popup-child-frame (phase)
  (let ((main-frame (selected-frame))
        (source-buffer (current-buffer))
        (popup-buffer (get-buffer-create "*ewm-skia-popup-smoke*"))
        popup-window popup-frame)
    (unwind-protect
        (progn
          (with-current-buffer popup-buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#ff0000"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (dotimes (line 24)
              (insert (format "popup frame render line %02d\n" line)))
            (goto-char (point-min)))
          (setq popup-window
                (display-buffer-in-child-frame
                 popup-buffer
                 `((child-frame-parameters
                    . ((parent-frame . ,main-frame)
                       (left . 48)
                       (top . 48)
                       (width . 48)
                       (height . 8)
                       (minibuffer . nil)
                       (undecorated . t)
                       (visibility . t))))))
          (unless (window-live-p popup-window)
            (ewm-skia-smoke-fail
             "display-buffer popup did not return a live window after %s"
             phase))
          (setq popup-frame (window-frame popup-window))
          (unless (and (frame-live-p popup-frame)
                       (not (eq popup-frame main-frame)))
            (ewm-skia-smoke-fail
             "display-buffer popup did not create a child frame after %s"
             phase))
          (ewm-skia-smoke-settle-framebuffer
           popup-frame "display-buffer popup frame" 48 8 160 80)
          (ewm-skia-smoke-check-gl-enabled
           popup-frame "display-buffer popup frame")
          (ewm-skia-smoke-check-framebuffer
           "display-buffer popup frame" popup-frame t))
      (when (and popup-frame (frame-live-p popup-frame))
        (delete-frame popup-frame t))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (select-frame-set-input-focus main-frame)
      (redisplay 'force)
      (sit-for 0.01))
    (ewm-skia-smoke-check-main-frame-rendering
     main-frame (format "popup cleanup %s" phase))))

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

(defun ewm-skia-smoke-window-rect (width height &optional window)
  (let ((edges (window-inside-pixel-edges window)))
    (list (nth 0 edges) (nth 1 edges) width height)))

(defun ewm-skia-smoke-window-offset-rect (x y width height &optional window)
  (let ((edges (window-inside-pixel-edges window)))
    (list (+ (nth 0 edges) x) (+ (nth 1 edges) y) width height)))

(defun ewm-skia-smoke-window-line-rect (line width &optional window)
  (let* ((edges (window-inside-pixel-edges window))
         (height (frame-char-height)))
    (list (nth 0 edges)
          (+ (nth 1 edges) (* line height) 2)
          width
          (max 1 (- height 4)))))

(defun ewm-skia-smoke-window-visible-lines (&optional window)
  (let* ((edges (window-inside-pixel-edges window))
         (height (frame-char-height)))
    (max 1 (/ (- (nth 3 edges) (nth 1 edges)) height))))

(defun ewm-skia-smoke-fill-scroll-buffer (&optional red-line)
  (let ((red-face '(:foreground "#ff0000" :background "#00ff00"))
        (green-face '(:foreground "#00ff00" :background "#00ff00")))
    (setq-local cursor-type nil)
    (setq-local mode-line-format nil)
    (setq-local truncate-lines t)
    (setq-local bidi-display-reordering nil)
    (erase-buffer)
    (dotimes (line 40)
      (insert (propertize (make-string 72 ?M)
                          'face (if (equal line red-line)
                                    red-face
                                  green-face))
              "\n"))))

(defun ewm-skia-smoke-fill-underwave-scroll-buffer ()
  (let ((red-underwave-face '(:foreground "#ff0000"
                              :background "#00ff00"
                              :underline (:color "#ff0000" :style wave)))
        (green-face '(:foreground "#00ff00" :background "#00ff00")))
    (setq-local cursor-type nil)
    (setq-local mode-line-format nil)
    (setq-local truncate-lines t)
    (setq-local bidi-display-reordering nil)
    (erase-buffer)
    (dotimes (line 40)
      (insert (propertize (make-string 72 ?M)
                          'face (if (= line 1)
                                    red-underwave-face
                                  green-face))
              "\n"))))

(defun ewm-skia-smoke-left-fringe-rect ()
  (let ((outer (window-pixel-edges))
        (inside (window-inside-pixel-edges)))
    (list (nth 0 outer)
          (nth 1 inside)
          (max 1 (- (nth 0 inside) (nth 0 outer)))
          (max 1 (- (nth 3 inside) (nth 1 inside))))))

(defun ewm-skia-smoke-right-fringe-rect ()
  (let* ((inside (window-inside-pixel-edges))
         (fringes (window-fringes))
         (right-width (or (nth 1 fringes) 0)))
    (list (nth 2 inside)
          (nth 1 inside)
          (max 1 right-width)
          (max 1 (- (nth 3 inside) (nth 1 inside))))))

(defun ewm-skia-smoke-bounds-width (bounds)
  (and (consp bounds)
       (1+ (- (nth 2 bounds) (nth 0 bounds)))))

(defun ewm-skia-smoke-bounds-height (bounds)
  (and (consp bounds)
       (1+ (- (nth 3 bounds) (nth 1 bounds)))))

(defun ewm-skia-smoke-cursor-cell-rect (&optional column)
  (let ((edges (window-inside-pixel-edges))
        (char-width (frame-char-width))
        (char-height (frame-char-height)))
    (list (+ (nth 0 edges) (* (or column 0) char-width))
          (nth 1 edges)
          (max 1 char-width)
          (max 1 char-height))))

(defun ewm-skia-smoke-check-region-colors
    (phase label rect min-red min-green)
  (let ((stats nil)
        (ok nil))
    (catch 'done
      (dotimes (_ 10)
        (ewm-skia-smoke-pump)
        (setq stats (pgtk-skia-test-framebuffer-color-counts nil rect))
        (when (and (consp stats)
                   (> (nth 2 stats) min-red)
                   (> (nth 3 stats) min-green))
          (setq ok t)
          (throw 'done t))))
    (unless ok
      (ewm-skia-smoke-fail
       "%s missing expected colors after %s: stats=%S rect=%S thresholds=(red=%d green=%d)"
       label phase stats rect min-red min-green))
    stats))

(defun ewm-skia-smoke-check-region-no-red
    (phase label rect max-red min-green)
  (let ((stats nil)
        (ok nil))
    (catch 'done
      (dotimes (_ 10)
        (ewm-skia-smoke-pump)
        (setq stats (pgtk-skia-test-framebuffer-color-counts nil rect))
        (when (and (consp stats)
                   (< (nth 2 stats) max-red)
                   (> (nth 3 stats) min-green))
          (setq ok t)
          (throw 'done t))))
    (unless ok
      (ewm-skia-smoke-fail
       "%s leaked red pixels after %s: stats=%S rect=%S thresholds=(red<%d green>%d)"
       label phase stats rect max-red min-green))
    stats))

(defun ewm-skia-smoke-check-fringe-geometry
    (phase label rect stats scale)
  (let* ((bounds (nth 5 stats))
         (expected-width (* 8 scale))
         (expected-height (* 16 scale))
         (expected-red (* expected-width expected-height))
         (expected-left
          (+ (* (nth 0 rect) scale)
             (/ (- (* (nth 2 rect) scale) expected-width) 2)))
         (rect-raw-min
          (- (nth 1 stats) (* (+ (nth 1 rect) (nth 3 rect)) scale)))
         (rect-raw-max
          (1- (- (nth 1 stats) (* (nth 1 rect) scale))))
         (actual-width (ewm-skia-smoke-bounds-width bounds))
         (actual-height (ewm-skia-smoke-bounds-height bounds)))
    (unless (and (consp bounds)
                 (= (nth 2 stats) expected-red)
                 (= actual-width expected-width)
                 (= actual-height expected-height)
                 (= (nth 0 bounds) expected-left)
                 (<= rect-raw-min (nth 1 bounds))
                 (<= (nth 3 bounds) rect-raw-max))
      (ewm-skia-smoke-fail
       "%s fringe geometry mismatch after %s: rect=%S scale=%d stats=%S expected=(red=%d width=%d height=%d left=%d y-range=%d..%d)"
       label phase rect scale stats expected-red expected-width
       expected-height expected-left rect-raw-min rect-raw-max))))

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
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert "\n"))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-resize-selected-frame
           40 8 "scaled transparent image baseline")
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
          (ewm-skia-smoke-resize-selected-frame
           40 8 "rotated transparent image")
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
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert-image image " " nil '(0 0 120 120)))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-resize-selected-frame
           40 8 "sliced scaled image")
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

(defun ewm-skia-smoke-check-transparent-image-loader
    (phase type data label)
  (unless (image-type-available-p type)
    (ewm-skia-smoke-fail "%s images are not available" label))
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create
                  (format "*ewm-skia-%s-image-smoke*" type)))
         (image (create-image data type t
                              :width 400
                              :height 160
                              :transform-smoothing nil))
         red-stats transparent-stats)
    (unwind-protect
        (progn
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert-image image " "))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-resize-selected-frame
           44 8 (format "%s transparent image loader" label))
          (setq red-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-offset-rect 0 0 180 180)))
          (setq transparent-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-offset-rect 320 0 80 180)))
          (unless (and (consp red-stats)
                       (> (nth 2 red-stats) 10000))
            (ewm-skia-smoke-fail
             "%s image loader missing opaque red pixels after %s: stats=%S"
             label phase red-stats))
          (unless (and (consp transparent-stats)
                       (< (nth 2 transparent-stats) 50)
                       (> (nth 3 transparent-stats) 10000))
            (ewm-skia-smoke-fail
             "%s image loader did not preserve transparent green pixels after %s: stats=%S"
             label phase transparent-stats)))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-opaque-image-loader (phase type data label)
  (unless (image-type-available-p type)
    (ewm-skia-smoke-fail "%s images are not available" label))
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create
                  (format "*ewm-skia-%s-image-smoke*" type)))
         (image (create-image data type t
                              :width 400
                              :height 160
                              :transform-smoothing nil))
         stats)
    (unwind-protect
        (progn
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert-image image " "))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-resize-selected-frame
           44 8 (format "%s opaque image loader" label))
          (setq stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-offset-rect 0 0 420 180)))
          (unless (and (consp stats)
                       (> (nth 2 stats) 10000))
            (ewm-skia-smoke-fail
             "%s image loader missing opaque red pixels after %s: stats=%S"
             label phase stats)))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-png-image-loader (phase)
  (ewm-skia-smoke-check-transparent-image-loader
   phase 'png
   (base64-decode-string
    "iVBORw0KGgoAAAANSUhEUgAAABQAAAAUCAYAAACNiR0NAAAAIElEQVR42mP4z8DwHx0zUAJGDRw1cNTAUQNHDRypBgIA3FfHOfmPdmEAAAAASUVORK5CYII=")
   "PNG"))

(defun ewm-skia-smoke-check-jpeg-image-loader (phase)
  (ewm-skia-smoke-check-opaque-image-loader
   phase 'jpeg
   (base64-decode-string
    "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAIBAQEBAQIBAQECAgICAgQDAgICAgUEBAMEBgUGBgYFBgYGBwkIBgcJBwYGCAsICQoKCgoKBggLDAsKDAkKCgr/2wBDAQICAgICAgUDAwUKBwYHCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgr/wAARCAAUABQDAREAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwD4vr+Uz/fwKACgAoAKACgAoAKACgD/2Q==")
   "JPEG"))

(defun ewm-skia-smoke-check-gif-image-loader (phase)
  (ewm-skia-smoke-check-opaque-image-loader
   phase 'gif
   (base64-decode-string
    "R0lGODlhAQABAIAAAP8AAAAAACwAAAAAAQABAAACAkQBADs=")
   "GIF"))

(defun ewm-skia-smoke-check-webp-image-loader (phase)
  (ewm-skia-smoke-check-transparent-image-loader
   phase 'webp
   (base64-decode-string
    "UklGRiYAAABXRUJQVlA4TBoAAAAvE8AEEA8Q8x/zHwwCAYri/2YDHIjof6z8AA==")
   "WebP"))

(defun ewm-skia-smoke-check-animated-webp-image-loader (phase)
  (unless (image-type-available-p 'webp)
    (ewm-skia-smoke-fail "WebP images are not available"))
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create "*ewm-skia-animated-webp-smoke*"))
         (data (base64-decode-string
                "UklGRoQAAABXRUJQVlA4WAoAAAACAAAAAQAAAQAAQU5JTQYAAAD/////AABBTk1GKAAAAAAAAAAAAAEAAAEAAGQAAAJWUDhMDwAAAC8BQAAABxD9j/4HIqL/AQBBTk1GKAAAAAAAAAAAAAEAAAEAAGQAAABWUDhMDwAAAC8BQAAAB9D/iP4HIqL/AQA="))
         (image (create-image data 'webp t
                              :width 400
                              :height 160
                              :transform-smoothing nil))
         animation red-stats green-stats)
    (unwind-protect
        (progn
          (setq animation (image-multi-frame-p image))
          (unless (and (consp animation) (= (car animation) 2))
            (ewm-skia-smoke-fail
             "animated WebP metadata missing after %s: %S"
             phase animation))
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#0000ff") default)))
            (erase-buffer)
            (insert-image image " "))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-resize-selected-frame
           44 8 "animated WebP image loader")
          (setq red-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-offset-rect 0 0 420 180)))
          (unless (and (consp red-stats)
                       (> (nth 2 red-stats) 10000)
                       (< (nth 3 red-stats) 100))
            (ewm-skia-smoke-fail
             "animated WebP first frame has wrong colors after %s: stats=%S"
             phase red-stats))
          (plist-put (cdr image) :animate-buffer buffer)
          (image-show-frame image 1 t)
          (ewm-skia-smoke-redraw)
          (setq green-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-offset-rect 0 0 420 180)))
          (unless (and (consp green-stats)
                       (> (nth 3 green-stats) 10000)
                       (< (nth 2 green-stats) 100))
            (ewm-skia-smoke-fail
             "animated WebP second frame has wrong colors after %s: first=%S second=%S"
             phase red-stats green-stats)))
      (clear-image-cache nil image)
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-masked-image-cursor (phase)
  (unless (image-type-available-p 'xpm)
    (ewm-skia-smoke-fail "XPM images are not available"))
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create "*ewm-skia-masked-image-smoke*"))
         (old-cursor-color (frame-parameter nil 'cursor-color))
         (row (concat (make-string 30 ?R) (make-string 90 ?.)))
         (xpm (ewm-skia-smoke-xpm 120 40 (make-list 40 row)))
         (image (create-image xpm 'xpm t
                              :width 480
                              :height 160
                              :transform-smoothing nil))
         red-stats transparent-stats cursor-stats)
    (unwind-protect
        (progn
          (blink-cursor-mode -1)
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert-image image " "))
          (switch-to-buffer buffer)
          (goto-char (point-min))
          (ewm-skia-smoke-resize-selected-frame
           44 8 "masked image")
          (setq red-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-offset-rect 0 0 180 180)))
          (setq transparent-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-offset-rect 260 0 200 180)))
          (unless (and (consp red-stats)
                       (> (nth 2 red-stats) 10000))
            (ewm-skia-smoke-fail
             "masked image missing opaque red pixels after %s: stats=%S"
             phase red-stats))
          (unless (and (consp transparent-stats)
                       (< (nth 2 transparent-stats) 50)
                       (> (nth 3 transparent-stats) 10000))
            (ewm-skia-smoke-fail
             "masked image did not preserve transparent green pixels after %s: stats=%S"
             phase transparent-stats))
          (modify-frame-parameters nil '((cursor-color . "#ff0000")))
          (with-current-buffer buffer
            (setq-local cursor-type 'box))
          (goto-char (point-min))
          (ewm-skia-smoke-redraw)
          (setq cursor-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-offset-rect 260 0 200 180)))
          (unless (and (consp cursor-stats)
                       (> (- (nth 2 cursor-stats)
                             (nth 2 transparent-stats))
                          100))
            (ewm-skia-smoke-fail
             "cursor over masked image did not expose red cursor pixels after %s: baseline=%S cursor=%S"
             phase transparent-stats cursor-stats)))
      (modify-frame-parameters nil `((cursor-color . ,old-cursor-color)))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-tool-bar-image (phase)
  (require 'tool-bar)
  (unless (image-type-available-p 'xpm)
    (ewm-skia-smoke-fail "XPM images are not available"))
  (let* ((xpm (ewm-skia-smoke-xpm
               24 24 (make-list 24 "rrrrrrrrrrrrrrrrrrrrrrrr")))
         (image (create-image xpm 'xpm t :ascent 'center))
         (map (make-sparse-keymap))
         (old-lines (frame-parameter nil 'tool-bar-lines))
         (old-mode tool-bar-mode))
    (define-key map [ewm-skia-smoke]
                (list 'menu-item "Skia" #'ignore :image image))
    (unwind-protect
        (let ((tool-bar-map map)
              (tool-bar-mode t))
          (tool-bar--flush-cache)
          (set-frame-parameter nil 'tool-bar-lines 1)
          (redisplay 'force)
          (sit-for 0.02)
          (unless (> (or (frame-parameter nil 'tool-bar-lines) 0) 0)
            (ewm-skia-smoke-fail
             "tool bar image path did not enable after %s" phase)))
      (tool-bar--flush-cache)
      (set-frame-parameter nil 'tool-bar-lines old-lines)
      (setq tool-bar-mode old-mode)
      (redisplay 'force)
      (sit-for 0.02))))

(defun ewm-skia-smoke-check-window-clipping-case
    (phase label left-buffer-name setup-left min-left-red)
  (let* ((source-buffer (current-buffer))
         (source-window (selected-window))
         (left-buffer (get-buffer-create left-buffer-name))
         (right-buffer (get-buffer-create
                        (format "*ewm-skia-%s-clip-right-smoke*" label)))
         (left-window nil)
         (right-window nil)
         left-stats right-stats)
    (unwind-protect
        (progn
          (set-frame-size nil 80 10)
          (delete-other-windows)
          (setq left-window (selected-window)
                right-window (split-window-right))
          (with-current-buffer left-buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (funcall setup-left))
          (with-current-buffer right-buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert "right window clipping guard\n"))
          (set-window-buffer left-window left-buffer)
          (set-window-buffer right-window right-buffer)
          (select-window left-window)
          (ewm-skia-smoke-redraw)
          (setq left-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-rect 260 160 left-window)))
          (setq right-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-rect 260 160 right-window)))
          (unless (and (consp left-stats)
                       (> (nth 2 left-stats) min-left-red))
            (ewm-skia-smoke-fail
             "%s clipping source missing red pixels after %s: stats=%S threshold=%d"
             label phase left-stats min-left-red))
          (unless (and (consp right-stats)
                       (< (nth 2 right-stats) 50)
                       (> (nth 3 right-stats) 1000))
            (ewm-skia-smoke-fail
             "%s clipping leaked red pixels after %s: stats=%S"
             label phase right-stats)))
      (delete-other-windows)
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (when (window-live-p source-window)
        (select-window source-window))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-window-clipping (phase)
  (unless (image-type-available-p 'xpm)
    (ewm-skia-smoke-fail "XPM images are not available"))
  (let ((xpm (ewm-skia-smoke-xpm
              120 40 (make-list 40 (make-string 120 ?R)))))
    (ewm-skia-smoke-check-window-clipping-case
     phase "image" "*ewm-skia-image-clip-left-smoke*"
     (lambda ()
       (insert-image
        (create-image xpm 'xpm t
                      :width 900
                      :height 140
                      :transform-smoothing nil)
        " "))
     10000)))

(defun ewm-skia-smoke-check-glyph-window-clipping (phase)
  (ewm-skia-smoke-check-window-clipping-case
   phase "glyph" "*ewm-skia-glyph-clip-left-smoke*"
   (lambda ()
     (setq-local bidi-display-reordering nil)
     (setq-local truncate-lines t)
     (setq-local face-remapping-alist
                 '((default (:foreground "#ff0000"
                              :background "#00ff00") default)))
     (insert (make-string 240 ?M) "\n"))
   1000))

(defun ewm-skia-smoke-check-decorated-composite-window-clipping (phase)
  (ewm-skia-smoke-check-window-clipping-case
   phase "decorated-glyph" "*ewm-skia-decorated-clip-left-smoke*"
   (lambda ()
     (setq-local bidi-display-reordering nil)
     (setq-local truncate-lines t)
     (insert
      (propertize
       (make-string 240 ?M)
       'face '(:foreground "#0000ff"
               :background "#00ff00"
               :underline (:color "#ff0000" :style line)))
      "\n"))
   100)
  (ewm-skia-smoke-check-window-clipping-case
   phase "composite-glyph" "*ewm-skia-composite-clip-left-smoke*"
   (lambda ()
     (setq-local bidi-display-reordering nil)
     (setq-local truncate-lines t)
     (dotimes (_ 48)
       (let ((start (point)))
         (insert (propertize "  "
                             'face '(:foreground "#ff0000"
                                     :background "#00ff00")))
         (compose-region start (point) "MMMMMMMM")))
     (insert "\n"))
   400))

(defun ewm-skia-smoke-check-multirect-clipping (phase)
  (unless (fboundp 'pgtk-skia-test-draw-multirect-clip)
    (ewm-skia-smoke-fail "missing Skia multi-rect clip test hook"))
  (let* ((source-window (selected-window))
         rects left-rect gap-rect right-rect
         left-stats gap-stats right-stats)
    (unwind-protect
        (progn
          (set-frame-size nil 44 8)
          (delete-other-windows)
          (ewm-skia-smoke-redraw)
          (setq rects (pgtk-skia-test-draw-multirect-clip))
          (unless (and (consp rects)
                       (= (length rects) 3))
            (ewm-skia-smoke-fail
             "multi-rect clip hook could not draw after %s: rects=%S"
             phase rects))
          (setq left-rect (nth 0 rects)
                gap-rect (nth 1 rects)
                right-rect (nth 2 rects))
          (setq left-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil left-rect))
          (setq gap-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil gap-rect))
          (setq right-stats
                (pgtk-skia-test-framebuffer-color-counts
                 nil right-rect))
          (unless (and (consp left-stats)
                       (> (nth 2 left-stats) 1000))
            (ewm-skia-smoke-fail
             "multi-rect clip left rect missing red pixels after %s: rect=%S stats=%S"
             phase left-rect left-stats))
          (unless (and (consp right-stats)
                       (> (nth 2 right-stats) 1000))
            (ewm-skia-smoke-fail
             "multi-rect clip right rect missing red pixels after %s: rect=%S stats=%S"
             phase right-rect right-stats))
          (unless (and (consp gap-stats)
                       (< (nth 2 gap-stats) 50)
                       (> (nth 3 gap-stats) 1000))
            (ewm-skia-smoke-fail
             "multi-rect clip filled excluded gap after %s: rect=%S stats=%S"
             phase gap-rect gap-stats)))
      (when (window-live-p source-window)
        (select-window source-window))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-glyph-overlap-clipping (phase)
  (unless (fboundp 'pgtk-skia-test-draw-glyph-overlap-clip)
    (ewm-skia-smoke-fail "missing Skia glyph overlap clip test hook"))
  (let* ((source-window (selected-window))
         rects top-rect gap-rect bottom-rect
         top-stats gap-stats bottom-stats)
    (unwind-protect
        (progn
          (set-frame-size nil 44 10)
          (delete-other-windows)
          (ewm-skia-smoke-redraw)
          (setq rects (pgtk-skia-test-draw-glyph-overlap-clip))
          (unless (and (consp rects)
                       (= (length rects) 3))
            (ewm-skia-smoke-fail
             "glyph overlap clip hook could not draw after %s: rects=%S"
             phase rects))
          (setq top-rect (nth 0 rects)
                gap-rect (nth 1 rects)
                bottom-rect (nth 2 rects))
          (setq top-stats
                (pgtk-skia-test-framebuffer-color-counts nil top-rect))
          (setq gap-stats
                (pgtk-skia-test-framebuffer-color-counts nil gap-rect))
          (setq bottom-stats
                (pgtk-skia-test-framebuffer-color-counts nil bottom-rect))
          (unless (and (consp top-stats)
                       (> (nth 2 top-stats) 1000))
            (ewm-skia-smoke-fail
             "glyph overlap clip top rect missing red pixels after %s: rect=%S stats=%S"
             phase top-rect top-stats))
          (unless (and (consp bottom-stats)
                       (> (nth 2 bottom-stats) 1000))
            (ewm-skia-smoke-fail
             "glyph overlap clip bottom rect missing red pixels after %s: rect=%S stats=%S"
             phase bottom-rect bottom-stats))
          (unless (and (consp gap-stats)
                       (< (nth 2 gap-stats) 50)
                       (> (nth 3 gap-stats) 1000))
            (ewm-skia-smoke-fail
             "glyph overlap clip filled excluded row after %s: rect=%S stats=%S"
             phase gap-rect gap-stats)))
      (when (window-live-p source-window)
        (select-window source-window))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-overlap-scroll-copy (phase)
  (let* ((source-buffer (current-buffer))
         (source-window (selected-window))
         (buffer (get-buffer-create "*ewm-skia-overlap-scroll-smoke*"))
         top-before second-before top-after second-after)
    (unwind-protect
        (progn
          (set-frame-size nil 44 8)
          (delete-other-windows)
          (with-current-buffer buffer
            (ewm-skia-smoke-fill-scroll-buffer 1))
          (switch-to-buffer buffer)
          (goto-char (point-min))
          (ewm-skia-smoke-redraw)
          (setq top-before
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 520)))
          (setq second-before
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 1 520)))
          (unless (and (consp top-before)
                       (< (nth 2 top-before) 50)
                       (> (nth 3 top-before) 1000))
            (ewm-skia-smoke-fail
             "scroll-copy setup top line is not green after %s: stats=%S"
             phase top-before))
          (unless (and (consp second-before)
                       (> (nth 2 second-before) 1000))
            (ewm-skia-smoke-fail
             "scroll-copy setup second line is not red after %s: stats=%S"
             phase second-before))
          (scroll-up-command 1)
          (ewm-skia-smoke-pump)
          (setq top-after
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 520)))
          (unless (and (consp top-after)
                       (> (nth 2 top-after) 1000))
            (ewm-skia-smoke-fail
             "overlapping upward scroll copy lost red line after %s: before=%S after=%S"
             phase second-before top-after))
          (scroll-down-command 1)
          (ewm-skia-smoke-pump)
          (setq top-after
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 520)))
          (setq second-after
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 1 520)))
          (unless (and (consp top-after)
                       (< (nth 2 top-after) 50)
                       (> (nth 3 top-after) 1000)
                       (consp second-after)
                       (> (nth 2 second-after) 1000))
            (ewm-skia-smoke-fail
             "overlapping downward scroll copy restored wrong colors after %s: top=%S second=%S"
             phase top-after second-after)))
      (delete-other-windows)
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (when (window-live-p source-window)
        (select-window source-window))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-underwave-scroll-copy (phase)
  (let* ((source-buffer (current-buffer))
         (source-window (selected-window))
         (buffer (get-buffer-create "*ewm-skia-underwave-scroll-smoke*"))
         top-before second-before top-after)
    (unwind-protect
        (progn
          (set-frame-size nil 44 8)
          (delete-other-windows)
          (with-current-buffer buffer
            (ewm-skia-smoke-fill-underwave-scroll-buffer))
          (switch-to-buffer buffer)
          (goto-char (point-min))
          (ewm-skia-smoke-redraw)
          (setq top-before
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 520)))
          (setq second-before
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 1 520)))
          (unless (and (consp top-before)
                       (< (nth 2 top-before) 50)
                       (> (nth 3 top-before) 1000)
                       (consp second-before)
                       (> (nth 2 second-before) 1000))
            (ewm-skia-smoke-fail
             "underwave scroll setup wrong colors after %s: top=%S second=%S"
             phase top-before second-before))
          (scroll-up-command 1)
          (ewm-skia-smoke-pump)
          (setq top-after
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 520)))
          (unless (and (consp top-after)
                       (>= (* (nth 2 top-after) 100)
                           (* (nth 2 second-before) 80)))
            (ewm-skia-smoke-fail
             "underwave scroll copy lost red source pixels after %s: before=%S after=%S"
             phase second-before top-after)))
      (delete-other-windows)
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (when (window-live-p source-window)
        (select-window source-window))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-partial-window-scroll-copy (phase)
  (let* ((source-buffer (current-buffer))
         (source-window (selected-window))
         (left-buffer (get-buffer-create "*ewm-skia-partial-scroll-left*"))
         (right-buffer (get-buffer-create "*ewm-skia-partial-scroll-right*"))
         left-window right-window bottom-line
         left-top-before left-second-before right-before
         left-top-after left-second-after left-bottom-after right-after
         settled)
    (unwind-protect
        (progn
          (set-frame-size nil 80 10)
          (delete-other-windows)
          (setq left-window (selected-window)
                right-window (split-window-right))
          (with-current-buffer left-buffer
            (ewm-skia-smoke-fill-scroll-buffer 1))
          (with-current-buffer right-buffer
            (ewm-skia-smoke-fill-scroll-buffer))
          (set-window-buffer left-window left-buffer)
          (set-window-buffer right-window right-buffer)
          (select-window left-window)
          (goto-char (point-min))
          (set-window-point right-window
                            (with-current-buffer right-buffer (point-min)))
          (ewm-skia-smoke-redraw)
          (setq left-top-before
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 220 left-window)))
          (setq left-second-before
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 1 220 left-window)))
          (setq right-before
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 220 right-window)))
          (unless (and (consp left-top-before)
                       (< (nth 2 left-top-before) 50)
                       (> (nth 3 left-top-before) 1000)
                       (consp left-second-before)
                       (> (nth 2 left-second-before) 1000)
                       (consp right-before)
                       (< (nth 2 right-before) 50)
                       (> (nth 3 right-before) 1000))
            (ewm-skia-smoke-fail
             "partial-window scroll setup wrong colors after %s: left-top=%S left-second=%S right=%S"
             phase left-top-before left-second-before right-before))
          (scroll-up-command 1)
          (catch 'settled
            (dotimes (_ 20)
              (ewm-skia-smoke-pump)
              (setq bottom-line
                    (1- (ewm-skia-smoke-window-visible-lines left-window)))
              (setq left-top-after
                    (pgtk-skia-test-framebuffer-color-counts
                     nil (ewm-skia-smoke-window-line-rect 0 220 left-window)))
              (setq left-second-after
                    (pgtk-skia-test-framebuffer-color-counts
                     nil (ewm-skia-smoke-window-line-rect 1 220 left-window)))
              (setq left-bottom-after
                    (pgtk-skia-test-framebuffer-color-counts
                     nil (ewm-skia-smoke-window-line-rect
                          bottom-line 220 left-window)))
              (setq right-after
                    (pgtk-skia-test-framebuffer-color-counts
                     nil (ewm-skia-smoke-window-line-rect 0 220 right-window)))
              (when (and (consp left-top-after)
                         (> (nth 2 left-top-after) 1000)
                         (consp left-second-after)
                         (< (nth 2 left-second-after) 50)
                         (> (nth 3 left-second-after) 1000)
                         (consp left-bottom-after)
                         (< (nth 2 left-bottom-after) 50)
                         (> (nth 3 left-bottom-after) 1000)
                         (consp right-after)
                         (< (nth 2 right-after) 50)
                         (> (nth 3 right-after) 1000))
                (setq settled t)
                (throw 'settled t))))
          (unless settled
            (ewm-skia-smoke-fail
             "partial-window scroll copied outside expected geometry after %s: left-top=%S left-second=%S left-bottom=%S right=%S"
             phase left-top-after left-second-after
             left-bottom-after right-after)))
      (delete-other-windows)
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (when (window-live-p source-window)
        (select-window source-window))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-large-scroll-copy (phase)
  (let* ((source-buffer (current-buffer))
         (source-window (selected-window))
         (buffer (get-buffer-create "*ewm-skia-large-scroll-smoke*"))
         bottom-line top-before target-before top-after second-after
         bottom-after)
    (unwind-protect
        (progn
          (set-frame-size nil 44 10)
          (delete-other-windows)
          (with-current-buffer buffer
            (ewm-skia-smoke-fill-scroll-buffer 5))
          (switch-to-buffer buffer)
          (goto-char (point-min))
          (ewm-skia-smoke-redraw)
          (setq top-before
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 520)))
          (setq target-before
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 5 520)))
          (unless (and (consp top-before)
                       (< (nth 2 top-before) 50)
                       (> (nth 3 top-before) 1000)
                       (consp target-before)
                       (> (nth 2 target-before) 1000))
            (ewm-skia-smoke-fail
             "large scroll setup wrong colors after %s: top=%S target=%S"
             phase top-before target-before))
          (scroll-up-command 5)
          (ewm-skia-smoke-pump)
          (setq bottom-line
                (1- (ewm-skia-smoke-window-visible-lines)))
          (setq top-after
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 520)))
          (setq second-after
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 1 520)))
          (setq bottom-after
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect bottom-line 520)))
          (unless (and (consp top-after)
                       (> (nth 2 top-after) 1000)
                       (consp second-after)
                       (< (nth 2 second-after) 50)
                       (> (nth 3 second-after) 1000)
                       (consp bottom-after)
                       (< (nth 2 bottom-after) 50)
                       (> (nth 3 bottom-after) 1000))
            (ewm-skia-smoke-fail
             "large scroll copy wrong colors after %s: top=%S second=%S bottom=%S"
             phase top-after second-after bottom-after)))
      (delete-other-windows)
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (when (window-live-p source-window)
        (select-window source-window))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-horizontal-scroll-text (phase)
  (let* ((source-buffer (current-buffer))
         (source-window (selected-window))
         (buffer (get-buffer-create "*ewm-skia-hscroll-smoke*"))
         before after)
    (unwind-protect
        (progn
          (set-frame-size nil 44 6)
          (delete-other-windows)
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local truncate-lines t)
            (setq-local bidi-display-reordering nil)
            (erase-buffer)
            (insert (propertize (make-string 90 ?M)
                                'face '(:foreground "#00ff00"
                                        :background "#00ff00"))
                    (propertize (make-string 80 ?M)
                                'face '(:foreground "#ff0000"
                                        :background "#00ff00"))
                    "\n"))
          (switch-to-buffer buffer)
          (goto-char (point-min))
          (set-window-hscroll nil 0)
          (ewm-skia-smoke-redraw)
          (setq before
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 520)))
          (unless (and (consp before)
                       (< (nth 2 before) 50)
                       (> (nth 3 before) 1000))
            (ewm-skia-smoke-fail
             "horizontal scroll setup wrong colors after %s: stats=%S"
             phase before))
          (set-window-hscroll nil 90)
          (ewm-skia-smoke-redraw)
          (unless (> (window-hscroll) 0)
            (ewm-skia-smoke-fail
             "horizontal scroll did not set hscroll after %s" phase))
          (setq after
                (pgtk-skia-test-framebuffer-color-counts
                 nil (ewm-skia-smoke-window-line-rect 0 520)))
          (unless (and (consp after)
                       (> (nth 2 after) 1000))
            (ewm-skia-smoke-fail
             "horizontal scroll did not expose red text after %s: before=%S after=%S"
             phase before after)))
      (set-window-hscroll nil 0)
      (delete-other-windows)
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (when (window-live-p source-window)
        (select-window source-window))
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
                  ("dash-underline"
                   (:foreground "#0000ff"
                    :underline (:color "#ff0000" :style dashes))
                   150)
                  ("underwave"
                   (:foreground "#0000ff"
                    :underline (:color "#ff0000" :style wave))
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
                   600)
                  ("raised-box"
                   (:foreground "#0000ff"
                    :box (:line-width 4 :color "#ff0000"
                          :style released-button))
                   600)
                  ("sunken-box"
                   (:foreground "#0000ff"
                    :box (:line-width 4 :color "#ff0000"
                          :style pressed-button))
                   600)))
    (ewm-skia-smoke-check-red-decoration
     phase (nth 0 case) (nth 1 case) (nth 2 case))))

(defun ewm-skia-smoke-check-relief-polarity (phase)
  (unless (fboundp 'pgtk-skia-test-draw-relief-polarity)
    (ewm-skia-smoke-fail "missing Skia relief polarity test hook"))
  (let* ((source-window (selected-window))
         rects raised-top raised-bottom sunken-top sunken-bottom
         raised-top-stats raised-bottom-stats
         sunken-top-stats sunken-bottom-stats)
    (unwind-protect
        (progn
          (set-frame-size nil 44 12)
          (delete-other-windows)
          (ewm-skia-smoke-redraw)
          (setq rects (pgtk-skia-test-draw-relief-polarity))
          (unless (and (consp rects) (= (length rects) 4))
            (ewm-skia-smoke-fail
             "relief polarity hook could not draw after %s: rects=%S"
             phase rects))
          (setq raised-top (nth 0 rects)
                raised-bottom (nth 1 rects)
                sunken-top (nth 2 rects)
                sunken-bottom (nth 3 rects)
                raised-top-stats
                (pgtk-skia-test-framebuffer-color-counts nil raised-top)
                raised-bottom-stats
                (pgtk-skia-test-framebuffer-color-counts nil raised-bottom)
                sunken-top-stats
                (pgtk-skia-test-framebuffer-color-counts nil sunken-top)
                sunken-bottom-stats
                (pgtk-skia-test-framebuffer-color-counts nil sunken-bottom))
          (unless (and (consp raised-top-stats)
                       (> (nth 2 raised-top-stats) 100))
            (ewm-skia-smoke-fail
             "raised relief top-left edge is not red after %s: rect=%S stats=%S"
             phase raised-top raised-top-stats))
          (unless (and (consp raised-bottom-stats)
                       (> (nth 3 raised-bottom-stats) 100))
            (ewm-skia-smoke-fail
             "raised relief bottom-right edge is not green after %s: rect=%S stats=%S"
             phase raised-bottom raised-bottom-stats))
          (unless (and (consp sunken-top-stats)
                       (> (nth 3 sunken-top-stats) 100))
            (ewm-skia-smoke-fail
             "sunken relief top-left edge is not green after %s: rect=%S stats=%S"
             phase sunken-top sunken-top-stats))
          (unless (and (consp sunken-bottom-stats)
                       (> (nth 2 sunken-bottom-stats) 100))
            (ewm-skia-smoke-fail
             "sunken relief bottom-right edge is not red after %s: rect=%S stats=%S"
             phase sunken-bottom sunken-bottom-stats)))
      (when (window-live-p source-window)
        (select-window source-window))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

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
            (dotimes (_ 12)
              (let ((start (point)))
                (insert (propertize "  "
                                    'face '(:foreground "#ff0000"
                                            :background "#00ff00")))
                (compose-region start (point) "MMMMMMMM")))
            (insert "\n"))
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

(defun ewm-skia-smoke-font-name (font)
  (and font
       (or (ignore-errors (font-xlfd-name font))
           (format "%S" font))))

(defun ewm-skia-smoke-find-fallback-char (phase)
  (let* ((base-font (car (internal-char-font nil ?M)))
         (base-name (ewm-skia-smoke-font-name base-font))
         (candidates '(#x4e2d #x05d0 #x0915 #x0e01
                       #x3042 #x03a9 #x0416 #x1f642)))
    (or
     (catch 'found
       (dolist (char candidates)
         (let* ((entry (ignore-errors (internal-char-font nil char)))
                (font (and (consp entry) (car entry)))
                (font-name (ewm-skia-smoke-font-name font)))
           (when (and font-name
                      (not (equal font-name base-name))
                      (or (not (fboundp 'pgtk-skia-test-font-ft-size-p))
                          (pgtk-skia-test-font-ft-size-p font)))
             (throw 'found (list char font-name))))))
     (ewm-skia-smoke-fail
      "could not find a live fallback font after %s: base=%S"
      phase base-name))))

(defun ewm-skia-smoke-check-mixed-font-text (phase)
  (let* ((source-buffer (current-buffer))
         (buffer (get-buffer-create "*ewm-skia-mixed-font-smoke*"))
         (fallback (ewm-skia-smoke-find-fallback-char phase))
         (fallback-char (car fallback))
         (fallback-font (cadr fallback))
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
            (insert "MMMM ")
            (dotimes (_ 18)
              (insert (propertize (char-to-string fallback-char)
                                  'face '(:foreground "#ff0000"
                                          :background "#00ff00"))
                      " "))
            (insert " MMMM"))
          (switch-to-buffer buffer)
          (ewm-skia-smoke-redraw)
          (setq stats (ewm-skia-smoke-check-region-colors
                       phase
                       (format "mixed fallback font U+%04X %s"
                               fallback-char fallback-font)
                       (ewm-skia-smoke-window-rect 700 120)
                       80 1000))
          (ewm-skia-smoke-check-region-no-red
           phase "mixed-font default text"
           (ewm-skia-smoke-window-offset-rect
            0 0 (* 4 (frame-char-width)) (frame-char-height))
           5 20))
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
         phase info)))
    (unless (fboundp 'pgtk-skia-test-font-ft-size-p)
      (ewm-skia-smoke-fail "missing Skia font ft_size test hook"))
    (let ((char-font (internal-char-font nil ?M)))
      (unless (consp char-font)
        (ewm-skia-smoke-fail
         "could not get default font object after %s" phase))
      (unless (pgtk-skia-test-font-ft-size-p (car char-font))
        (ewm-skia-smoke-fail
         "default font object missing live FreeType size after %s"
         phase)))
    (clear-font-cache)))

(defun ewm-skia-smoke-check-cursor (phase)
  (let ((source-buffer (current-buffer))
        (buffer (get-buffer-create "*ewm-skia-cursor-smoke*"))
        (old-cursor-color (frame-parameter nil 'cursor-color)))
    (unwind-protect
        (progn
          (set-frame-size nil 44 6)
          (blink-cursor-mode -1)
          (modify-frame-parameters nil '((cursor-color . "#ff0000")))
          (with-current-buffer buffer
            (setq-local cursor-type 'box)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)))
            (erase-buffer)
            (insert "    MMMMMMMMMMMMMMMMMMMM\n")
            (goto-char (point-min)))
          (switch-to-buffer buffer)
          (goto-char (point-min))
          (ewm-skia-smoke-redraw)
          (ewm-skia-smoke-check-region-colors
           phase "block cursor" (ewm-skia-smoke-window-rect 260 120)
           50 1000)
          (ewm-skia-smoke-check-region-no-red
           phase "block cursor adjacent cell"
           (ewm-skia-smoke-cursor-cell-rect 2)
           5 20)
          (dolist (case '((bar . "bar cursor")
                          ((hbar . 6) . "hbar cursor")
                          (hollow . "hollow cursor")))
            (with-current-buffer buffer
              (setq-local cursor-type (car case))
              (goto-char (point-min)))
            (goto-char (point-min))
            (ewm-skia-smoke-redraw)
            (ewm-skia-smoke-check-region-colors
             phase (cdr case) (ewm-skia-smoke-cursor-cell-rect 0)
             10 20)
            (ewm-skia-smoke-check-region-no-red
             phase (format "%s adjacent cell" (cdr case))
             (ewm-skia-smoke-cursor-cell-rect 2)
             5 20)))
      (modify-frame-parameters nil `((cursor-color . ,old-cursor-color)))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(defun ewm-skia-smoke-check-fringe (phase)
  (define-fringe-bitmap
    'ewm-skia-smoke-fringe
    [#b11111111 #b11111111 #b11111111 #b11111111
     #b11111111 #b11111111 #b11111111 #b11111111
     #b11111111 #b11111111 #b11111111 #b11111111
     #b11111111 #b11111111 #b11111111 #b11111111]
    16 8 'center)
  (unless (facep 'ewm-skia-smoke-fringe-face)
    (make-face 'ewm-skia-smoke-fringe-face))
  (set-face-attribute 'ewm-skia-smoke-fringe-face nil
                      :foreground "#ff0000"
                      :background "#00ff00")
  (let ((source-buffer (current-buffer))
        (buffer (get-buffer-create "*ewm-skia-fringe-smoke*"))
        (old-fringes (window-fringes))
        scale left-rect right-rect left-stats right-stats)
    (unwind-protect
        (progn
          (set-frame-size nil 44 6)
          (with-current-buffer buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (setq-local face-remapping-alist
                        '((default (:foreground "#0000ff"
                                     :background "#00ff00") default)
                          (fringe (:foreground "#ff0000"
                                   :background "#00ff00") fringe)))
            (erase-buffer)
            (insert
             (propertize
              " "
              'display
              '(left-fringe ewm-skia-smoke-fringe
                            ewm-skia-smoke-fringe-face)))
            (insert "left fringe marker\n")
            (insert
             (propertize
              " "
              'display
              '(right-fringe ewm-skia-smoke-fringe
                             ewm-skia-smoke-fringe-face)))
            (insert "right fringe marker\n"))
          (switch-to-buffer buffer)
          (set-window-fringes nil 24 24)
          (ewm-skia-smoke-redraw)
          (setq scale (nth 6 (ewm-skia-smoke-framebuffer-size))
                left-rect (ewm-skia-smoke-left-fringe-rect)
                right-rect (ewm-skia-smoke-right-fringe-rect)
                left-stats
                (ewm-skia-smoke-check-region-colors
                 phase "left fringe" left-rect 50 100)
                right-stats
                (ewm-skia-smoke-check-region-colors
                 phase "right fringe" right-rect 50 100))
          (ewm-skia-smoke-check-fringe-geometry
           phase "left" left-rect left-stats scale)
          (ewm-skia-smoke-check-fringe-geometry
           phase "right" right-rect right-stats scale))
      (set-window-fringes nil (nth 0 old-fringes)
                          (nth 1 old-fringes)
                          (nth 2 old-fringes)
                          (nth 3 old-fringes))
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (set-frame-size nil 96 32)
      (ewm-skia-smoke-redraw))))

(condition-case err
    (progn

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

(defvar ewm-skia-smoke-target-monitor
  (ewm-skia-smoke-select-monitor))

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
(ewm-skia-smoke-check-actual-monitor ewm-skia-smoke-target-monitor)

(dotimes (step 4)
  (set-frame-size nil (+ 80 (* step 8)) (+ 24 step))
  (goto-char (point-min))
  (forward-line (* step 20))
  (redisplay 'force)
  (sit-for 0.02))

(ewm-skia-smoke-check-framebuffer "initial draw")
(ewm-skia-smoke-check-gl-enabled nil "initial draw")
(when ewm-skia-smoke-required-scale
  (ewm-skia-smoke-check-framebuffer-size "required scale"))

(ewm-skia-smoke-run-group "core"
  (ewm-skia-smoke-check-alpha-background "initial draw")
  (ewm-skia-smoke-check-partial-clear "initial draw")
  (ewm-skia-smoke-check-framebuffer-size "initial draw")
  (ewm-skia-smoke-check-gl-invariants "initial draw")
  (ewm-skia-smoke-check-scratch-state "initial draw")
  (ewm-skia-smoke-check-clear-state "initial draw")
  (ewm-skia-smoke-check-setup-state "initial draw")
  (ewm-skia-smoke-check-force-surface-update "initial draw"))

(ewm-skia-smoke-run-group "copy"
  (ewm-skia-smoke-check-copy-bits-gl-error "initial draw")
  (ewm-skia-smoke-check-copy-bits-fallback "initial draw")
  (ewm-skia-smoke-check-horizontal-copy-bits "initial draw")
  (ewm-skia-smoke-check-copy-bits-scissor-isolation "initial draw")
  (ewm-skia-smoke-check-copy-bits-damage "initial draw"))

(ewm-skia-smoke-run-group "export"
  (ewm-skia-smoke-check-png-export "initial draw")
  (ewm-skia-smoke-check-image-transform-capabilities "initial draw")
  (ewm-skia-smoke-check-optional-export
   "initial draw" 'pdf "Skia PDF export is not available")
  (ewm-skia-smoke-check-optional-export
   "initial draw" 'svg "Skia SVG export is not available")
  (ewm-skia-smoke-check-export-input-unwind "export error path"))

(ewm-skia-smoke-run-group "tooltip"
  (ewm-skia-smoke-check-tooltip-frame "initial draw"))

(ewm-skia-smoke-run-group "images"
  (ewm-skia-smoke-check-scaled-transparent-image "initial draw")
  (ewm-skia-smoke-check-rotated-transparent-image "initial draw")
  (ewm-skia-smoke-check-sliced-scaled-image "initial draw")
  (ewm-skia-smoke-check-png-image-loader "initial draw")
  (ewm-skia-smoke-check-jpeg-image-loader "initial draw")
  (ewm-skia-smoke-check-gif-image-loader "initial draw")
  (ewm-skia-smoke-check-webp-image-loader "initial draw")
  (ewm-skia-smoke-check-animated-webp-image-loader "initial draw")
  (ewm-skia-smoke-check-masked-image-cursor "initial draw")
  (ewm-skia-smoke-check-tool-bar-image "initial draw"))

(ewm-skia-smoke-run-group "clip"
  (ewm-skia-smoke-check-window-clipping "initial draw")
  (ewm-skia-smoke-check-glyph-window-clipping "initial draw")
  (ewm-skia-smoke-check-decorated-composite-window-clipping "initial draw")
  (ewm-skia-smoke-check-multirect-clipping "initial draw")
  (ewm-skia-smoke-check-glyph-overlap-clipping "initial draw"))

(ewm-skia-smoke-run-group "copy"
  (ewm-skia-smoke-check-overlap-scroll-copy "initial draw")
  (ewm-skia-smoke-check-underwave-scroll-copy "initial draw")
  (ewm-skia-smoke-check-partial-window-scroll-copy "initial draw")
  (ewm-skia-smoke-check-large-scroll-copy "initial draw"))

(ewm-skia-smoke-run-group "text"
  (ewm-skia-smoke-check-horizontal-scroll-text "initial draw")
  (ewm-skia-smoke-check-text-decorations "initial draw")
  (ewm-skia-smoke-check-relief-polarity "initial draw")
  (ewm-skia-smoke-check-glyphless-text "initial draw")
  (ewm-skia-smoke-check-composite-text "initial draw")
  (ewm-skia-smoke-check-mixed-font-text "initial draw")
  (ewm-skia-smoke-check-minspace-font-metrics "initial draw")
  (ewm-skia-smoke-check-cursor "initial draw")
  (ewm-skia-smoke-check-fringe "initial draw"))

(ewm-skia-smoke-run-group "copy"
  (goto-char (point-min))
  (ewm-skia-smoke-redraw)
  (dotimes (_ 6)
    (scroll-up-command 3)
    (ewm-skia-smoke-pump))
  (ewm-skia-smoke-check-framebuffer "scroll copy"))

(ewm-skia-smoke-run-group ("core" "resize")
  (set-frame-size nil 124 36)
  (ewm-skia-smoke-redraw)
  (ewm-skia-smoke-check-framebuffer-size "large resize")
  (set-frame-size nil 72 20)
  (ewm-skia-smoke-redraw)
  (ewm-skia-smoke-check-framebuffer-size "shrink resize")
  (ewm-skia-smoke-check-framebuffer "resize")
  (ewm-skia-smoke-check-alpha-background "resize")
  (ewm-skia-smoke-check-gl-invariants "resize")
  (ewm-skia-smoke-check-scratch-state "resize")
  (ewm-skia-smoke-check-clear-state "resize")
  (ewm-skia-smoke-check-setup-state "resize"))

(ewm-skia-smoke-run-group "frames"
  (let ((main-frame (selected-frame)))
    (ewm-skia-smoke-check-temporary-frame
     "child lifecycle frame"
     `((parent-frame . ,main-frame)
       (left . 24)
       (top . 24)
       (width . 56)
       (height . 12)
       (minibuffer . nil)
       (visibility . t))
     56 12 nil t))
  (ewm-skia-smoke-check-popup-child-frame "initial draw"))

(ewm-skia-smoke-run-group "frames"
  (dotimes (step 3)
    (ewm-skia-smoke-check-temporary-frame
     (format "lifecycle frame %d" step)
     `((width . ,(+ 64 (* step 8)))
       (height . ,(+ 18 step))
       (minibuffer . nil)
       (visibility . t))
     (+ 64 (* step 8)) (+ 18 step) (* step 30)))
  (ewm-skia-smoke-check-context-ownership "multi-frame context ownership")
  (raise-frame (selected-frame))
  (redisplay 'force)
  (sit-for 0.05))

(ewm-skia-smoke-run-group "frames"
  (ewm-skia-smoke-check-export-input-unwind "frame lifecycle"))

(unless (fboundp 'pgtk-skia-test-visible-bell-active-p)
  (ewm-skia-smoke-fail "missing Skia visible-bell test hook"))
(unless (fboundp 'pgtk-skia-test-recover-visible-bell)
  (ewm-skia-smoke-fail "missing Skia visible-bell recovery test hook"))
(ewm-skia-smoke-run-group "bell"
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

  (let ((visible-bell t)
        (ring-bell-function nil))
    (ding)
    (unless (pgtk-skia-test-visible-bell-active-p)
      (ewm-skia-smoke-fail "visible bell timer test created no snapshot"))
    (unless (ewm-skia-smoke-wait-visible-bell-release)
      (ewm-skia-smoke-fail "visible bell timer did not release snapshot"))
    (ewm-skia-smoke-check-framebuffer "visible bell timer restore"))

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
      (select-frame-set-input-focus main-frame))
    (ewm-skia-smoke-check-main-frame-rendering
     main-frame "after visible-bell frame deletion")))

(unless (fboundp 'pgtk-skia-test-abandon-gl-context)
  (ewm-skia-smoke-fail "missing Skia forced-abandon test hook"))
(unless (fboundp 'pgtk-skia-test-gl-recreate-source-active-p)
  (ewm-skia-smoke-fail "missing Skia stale-source test hook"))

(ewm-skia-smoke-run-group "recovery"
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
      (select-frame-set-input-focus main-frame))
    (ewm-skia-smoke-check-main-frame-rendering
     main-frame "after abandoned frame deletion")))

(ewm-skia-smoke-run-group "recovery"
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
    (ewm-skia-smoke-check-recovered-surface-operations
     "forced abandon recovery")))

(ewm-skia-smoke-run-group "lifecycle"
  (let ((frame (selected-frame)))
    (select-frame-set-input-focus frame)
    (switch-to-buffer "*ewm-skia-smoke*")
    (goto-char (point-min))
    (set-frame-size frame 96 32)
    (ewm-skia-smoke-redraw)
    (ewm-skia-smoke-check-gl-enabled frame "combined lifecycle")
    (ewm-skia-smoke-check-framebuffer "combined lifecycle" frame t))
  (ewm-skia-smoke-check-temporary-frame
   "post-recovery lifecycle frame"
   '((width . 72)
     (height . 20)
     (minibuffer . nil)
     (visibility . t))
   72 20 40)
  (ewm-skia-smoke-check-tooltip-frame "post-recovery lifecycle"))

(when (timerp ewm-skia-smoke-watchdog)
  (cancel-timer ewm-skia-smoke-watchdog))
(clear-image-cache t)
(garbage-collect)
(princ "EWM_SKIA_SMOKE_OK\n" #'external-debugging-output)
(kill-emacs 0))
  (error
   (ewm-skia-smoke-report-error err)))
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
