#!/usr/bin/env bash
set -euo pipefail

emacs_path="${1:-${EMACS:-emacs}}"
iterations="${2:-${EWM_SKIA_PROFILE_ITERATIONS:-80}}"

if [[ $# -gt 2 ]]; then
    echo "usage: $0 [EMACS_PATH] [ITERATIONS]" >&2
    exit 2
fi

if ! [[ "$iterations" =~ ^[1-9][0-9]*$ ]]; then
    echo "error: ITERATIONS must be a positive integer" >&2
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

timeout_seconds="${EWM_SKIA_PROFILE_TIMEOUT:-10}"
tmp_dir="$(mktemp -d -t ewm-skia-profile.XXXXXX)"
trap 'rm -rf "$tmp_dir"' EXIT

payload="$tmp_dir/profile.el"
stdout="$tmp_dir/stdout"
stderr="$tmp_dir/stderr"
result="$tmp_dir/result"

cat >"$payload" <<'EOF'
(setq debug-on-error t
      gc-cons-threshold most-positive-fixnum
      inhibit-startup-screen t
      initial-scratch-message nil
      frame-resize-pixelwise t
      redisplay-dont-pause t)

(defconst ewm-skia-profile-iterations
  (string-to-number (getenv "EWM_SKIA_PROFILE_ITERATIONS")))
(defconst ewm-skia-profile-output
  (getenv "EWM_SKIA_PROFILE_OUTPUT"))

(defun ewm-skia-profile-fail (format-string &rest args)
  (princ (concat "EWM_SKIA_PROFILE_FAIL: "
                 (apply #'format format-string args)
                 "\n")
         #'external-debugging-output)
  (kill-emacs 2))

(defun ewm-skia-profile-seconds (time)
  (float-time (time-subtract (current-time) time)))

(defun ewm-skia-profile-report (name iterations seconds)
  (let ((line (format "%-18s iterations=%d seconds=%.6f per-op-us=%.1f\n"
                      name iterations seconds
                      (/ (* seconds 1000000.0) iterations))))
    (with-temp-buffer
      (insert line)
      (append-to-file (point-min) (point-max)
                      ewm-skia-profile-output))))

(defun ewm-skia-profile-time (name iterations function)
  (garbage-collect)
  (let ((start (current-time)))
    (dotimes (_ iterations)
      (funcall function))
    (ewm-skia-profile-report
     name iterations (ewm-skia-profile-seconds start))))

(defun ewm-skia-profile-redraw ()
  (redisplay 'force))

(unless (and (string-match-p "\\bPGTK\\b" system-configuration-features)
             (or (string-match-p "\\bSKIA\\b" system-configuration-features)
                 (fboundp 'pgtk-skia-gl-enabled-p)))
  (princ (format "EWM_SKIA_PROFILE_SKIP: expected PGTK+SKIA, got %s\n"
                 system-configuration-features)
         #'external-debugging-output)
  (kill-emacs 77))

(unless (display-graphic-p)
  (ewm-skia-profile-fail "display is not graphical"))

(unless (eq window-system 'pgtk)
  (ewm-skia-profile-fail "window-system is %S, expected pgtk" window-system))

(set-face-attribute 'default nil
                    :foreground "#ff0000"
                    :background "#00ff00"
                    :height 180)
(set-frame-parameter nil 'background-color "#00ff00")
(set-frame-size nil 96 32)

(let ((buffer (get-buffer-create "*ewm-skia-profile*")))
  (with-current-buffer buffer
    (setq-local cursor-type nil)
    (setq-local mode-line-format nil)
    (setq-local truncate-lines t)
    (buffer-disable-undo)
    (erase-buffer)
    (dotimes (line 1200)
      (insert (format "skia profile line %04d: abcdefghijklmnopqrstuvwxyz 0123456789 []{}<>/_-+= %s\n"
                      line
                      (make-string 180
                                   (aref "abcdefghijklmnopqrstuvwxyz"
                                         (% line 26))))))
    (goto-char (point-min)))
  (switch-to-buffer buffer))

(redisplay 'force)
(sit-for 0.02)

(unless (and (fboundp 'pgtk-skia-gl-enabled-p)
             (pgtk-skia-gl-enabled-p))
  (ewm-skia-profile-fail "Skia GL acceleration is not active"))

(ewm-skia-profile-time
 "redisplay-force" ewm-skia-profile-iterations
 (lambda ()
   (force-window-update)
   (ewm-skia-profile-redraw)))

(let ((line-state nil))
  (ewm-skia-profile-time
   "line-update" ewm-skia-profile-iterations
   (lambda ()
     (save-excursion
       (goto-char (point-min))
       (delete-char 1)
       (insert (if line-state "s" "S"))
       (setq line-state (not line-state)))
     (ewm-skia-profile-redraw))))

(let ((hscroll 0))
  (ewm-skia-profile-time
   "hscroll-redraw" ewm-skia-profile-iterations
   (lambda ()
     (setq hscroll (if (zerop hscroll) 96 0))
     (set-window-hscroll nil hscroll)
     (ewm-skia-profile-redraw))))

(goto-char (point-min))
(set-window-hscroll nil 0)
(ewm-skia-profile-redraw)

(let ((scroll-direction 1))
  (ewm-skia-profile-time
   "scroll-copy" ewm-skia-profile-iterations
   (lambda ()
     (if (> scroll-direction 0)
         (scroll-up-command 1)
       (scroll-down-command 1))
     (setq scroll-direction (- scroll-direction))
     (ewm-skia-profile-redraw))))

(kill-emacs 0)
EOF

set +e
timeout --kill-after=1s "$timeout_seconds" \
    env GDK_BACKEND=wayland NO_AT_BRIDGE=1 \
        EWM_SKIA_PROFILE_ITERATIONS="$iterations" \
        EWM_SKIA_PROFILE_OUTPUT="$result" \
    "$emacs_path" -Q --load "$payload" >"$stdout" 2>"$stderr"
status=$?
set -e

if [[ $status -eq 124 || $status -eq 137 ]]; then
    echo "error: Skia profile timed out after ${timeout_seconds}s" >&2
    if [[ -f "$result" ]]; then
        cat "$result" >&2
    fi
    tail -n 80 "$stdout" >&2
    tail -n 80 "$stderr" >&2
    exit 1
fi

if [[ $status -eq 77 ]]; then
    cat "$stderr" >&2
    exit 77
fi

if [[ $status -ne 0 ]]; then
    echo "error: Emacs exited with status $status" >&2
    tail -n 120 "$stderr" >&2
    exit "$status"
fi

cat "$result"
