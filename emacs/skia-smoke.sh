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
    (ewm-skia-smoke-fail "test abandon hook found no Skia GL state"))
  (when (pgtk-skia-gl-enabled-p frame)
    (ewm-skia-smoke-fail "Skia GL state remained active after forced abandon"))
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
