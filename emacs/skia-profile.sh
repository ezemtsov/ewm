#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
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
page_size="$(getconf PAGESIZE 2>/dev/null || printf '4096')"
tmp_dir="$(mktemp -d -t ewm-skia-profile.XXXXXX)"
trap 'rm -rf "$tmp_dir"' EXIT

payload="$tmp_dir/profile.el"
stdout="$tmp_dir/stdout"
stderr="$tmp_dir/stderr"
result="$tmp_dir/result"

cat >"$payload" <<'EOF'
(setq debug-on-error nil
      gc-cons-threshold most-positive-fixnum
      inhibit-startup-screen t
      initial-scratch-message nil
      frame-resize-pixelwise t
      redisplay-dont-pause t)

(defconst ewm-skia-profile-iterations
  (string-to-number (getenv "EWM_SKIA_PROFILE_ITERATIONS")))
(defconst ewm-skia-profile-frame-count
  (string-to-number (or (getenv "EWM_SKIA_PROFILE_FRAMES") "4")))
(defconst ewm-skia-profile-churn-cycles
  (string-to-number (or (getenv "EWM_SKIA_PROFILE_CHURN_CYCLES") "3")))
(defconst ewm-skia-profile-output
  (getenv "EWM_SKIA_PROFILE_OUTPUT"))
(defconst ewm-skia-profile-page-size
  (string-to-number (or (getenv "EWM_SKIA_PROFILE_PAGE_SIZE") "4096")))
(defconst ewm-skia-profile-monitor-policy
  (or (getenv "EWM_SKIA_PROFILE_MONITOR") "largest"))
(defconst ewm-skia-profile-require-monitor
  (getenv "EWM_SKIA_PROFILE_REQUIRE_MONITOR"))
(defvar ewm-skia-profile-target-monitor nil)

(load-file (getenv "EWM_SKIA_HARNESS_MONITOR_EL"))

(defun ewm-skia-profile-fail (format-string &rest args)
  (princ (concat "EWM_SKIA_PROFILE_FAIL: "
                 (apply #'format format-string args)
                 "\n")
         #'external-debugging-output)
  (kill-emacs 2))

(defun ewm-skia-profile-dump-messages ()
  (let ((buffer (get-buffer "*Messages*")))
    (when buffer
      (princ "EWM_SKIA_PROFILE_MESSAGES_BEGIN\n"
             #'external-debugging-output)
      (with-current-buffer buffer
        (princ
         (buffer-substring-no-properties
          (max (point-min) (- (point-max) 6000))
          (point-max))
         #'external-debugging-output))
      (princ "\nEWM_SKIA_PROFILE_MESSAGES_END\n"
             #'external-debugging-output))))

(defun ewm-skia-profile-report-error (err)
  (princ (format "EWM_SKIA_PROFILE_ERROR: %S: %s\n"
                 err (error-message-string err))
         #'external-debugging-output)
  (ewm-skia-profile-dump-messages)
  (kill-emacs 2))

(defun ewm-skia-profile-timeout-seconds ()
  (max 1
       (string-to-number
        (or (getenv "EWM_SKIA_PROFILE_TIMEOUT_SECONDS") "10"))))

(defun ewm-skia-profile-watchdog-timeout ()
  (princ (format "EWM_SKIA_PROFILE_TIMEOUT: Emacs watchdog fired after %ds\n"
                 (ewm-skia-profile-timeout-seconds))
         #'external-debugging-output)
  (ewm-skia-profile-dump-messages)
  (kill-emacs 2))

(defvar ewm-skia-profile-watchdog
  (run-at-time (max 1 (- (ewm-skia-profile-timeout-seconds) 1))
               nil #'ewm-skia-profile-watchdog-timeout))

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

(defun ewm-skia-profile-report-rss (name frames before-kb after-kb)
  (when (and before-kb after-kb)
    (let ((line (format "%-18s frames=%d rss-kb-before=%d rss-kb-after=%d rss-kb-delta=%d\n"
                        name frames before-kb after-kb
                        (- after-kb before-kb))))
      (with-temp-buffer
        (insert line)
        (append-to-file (point-min) (point-max)
                        ewm-skia-profile-output)))))

(defun ewm-skia-profile-report-line (line)
  (with-temp-buffer
    (insert line "\n")
    (append-to-file (point-min) (point-max)
                    ewm-skia-profile-output)))

(defun ewm-skia-profile-report-context-state (name frame index)
  (when (fboundp 'pgtk-skia-test-context-state)
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
        (ewm-skia-profile-fail
         "invalid Skia context state for %s frame %d: %S"
         name index state))
      (ewm-skia-profile-report-line
       (format "%-18s frame=%d gdk-context=%S current-gdk-context=%S gdk-current=%S skia-context=%S cache-resources=%S cache-bytes=%S framebuffer=%S texture=%S gdk-shared-context=%S"
               name index
               (nth 0 state) (nth 1 state) (nth 2 state) (nth 3 state)
               (nth 4 state) (nth 5 state) (nth 6 state) (nth 7 state)
               (nth 8 state))))))

(defun ewm-skia-profile-time (name iterations function)
  (garbage-collect)
  (let ((start (current-time)))
    (dotimes (_ iterations)
      (funcall function))
    (ewm-skia-profile-report
     name iterations (ewm-skia-profile-seconds start))))

(defun ewm-skia-profile-redraw ()
  (redisplay 'force))

(defun ewm-skia-profile-rss-kb ()
  (when (file-readable-p "/proc/self/statm")
    (with-temp-buffer
      (insert-file-contents "/proc/self/statm")
      (let* ((fields (split-string (buffer-string)))
             (rss-pages (and (cadr fields)
                             (string-to-number (cadr fields)))))
        (when (and rss-pages (> rss-pages 0))
          (/ (* rss-pages ewm-skia-profile-page-size) 1024))))))

(defun ewm-skia-profile-redraw-frames (frames)
  (dolist (frame frames)
    (when (frame-live-p frame)
      (with-selected-frame frame
        (force-window-update)
        (redraw-frame frame))))
  (redisplay 'force))

(condition-case err
    (progn

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
(setq ewm-skia-profile-target-monitor
      (ewm-skia-harness-find-monitor ewm-skia-profile-monitor-policy))
(unless ewm-skia-profile-target-monitor
  (ewm-skia-profile-fail
   "monitor %S not found" ewm-skia-profile-monitor-policy))
(when ewm-skia-profile-target-monitor
  (ewm-skia-profile-report-line
   (ewm-skia-harness-monitor-string "target-monitor"
                                    ewm-skia-profile-target-monitor))
  (ewm-skia-harness-position-frame-on-monitor
   (selected-frame) ewm-skia-profile-target-monitor))
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
(let ((actual-monitor (frame-monitor-attributes)))
  (ewm-skia-profile-report-line
   (ewm-skia-harness-monitor-string "actual-monitor" actual-monitor))
  (when (and ewm-skia-profile-require-monitor
             ewm-skia-profile-target-monitor
             (not (ewm-skia-harness-same-monitor-p
                   ewm-skia-profile-target-monitor actual-monitor)))
    (ewm-skia-profile-fail
     "profile frame landed on %s, target was %s"
     (ewm-skia-harness-monitor-string "actual-monitor" actual-monitor)
     (ewm-skia-harness-monitor-string
      "target-monitor" ewm-skia-profile-target-monitor))))

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

(let ((main-frame (selected-frame))
      (frames nil)
      (before-kb (ewm-skia-profile-rss-kb))
      (multi-iterations (max 1 (/ ewm-skia-profile-iterations 4))))
  (unwind-protect
      (progn
        (ewm-skia-profile-report-context-state
         "main-frame-cache" main-frame 0)
        (dotimes (index ewm-skia-profile-frame-count)
          (let ((frame (make-frame `((width . 72)
                                     (height . 18)
                                     (minibuffer . nil)
                                     (visibility . t)))))
            (push frame frames)
            (select-frame-set-input-focus frame)
            (switch-to-buffer "*ewm-skia-profile*")
            (goto-char (point-min))
            (forward-line (* index 40))
            (ewm-skia-profile-redraw)
            (ewm-skia-profile-report-rss
             "multi-frame-rss" (1+ index) before-kb
             (ewm-skia-profile-rss-kb))
            (ewm-skia-profile-report-context-state
             "multi-frame-cache" frame (1+ index))))
        (setq frames (nreverse frames))
        (dolist (frame frames)
          (unless (and (pgtk-skia-gl-enabled-p frame)
                       (frame-live-p frame))
            (ewm-skia-profile-fail
             "Skia GL acceleration is not active for profile frame")))
        (let ((index 0))
          (dolist (frame frames)
            (setq index (1+ index))
            (ewm-skia-profile-report-context-state
             "multi-frame-ready" frame index)))
        (ewm-skia-profile-time
         "multi-frame-redraw" multi-iterations
         (lambda ()
           (ewm-skia-profile-redraw-frames frames))))
    (dolist (frame frames)
      (when (frame-live-p frame)
        (delete-frame frame t)))
    (when (frame-live-p main-frame)
      (select-frame-set-input-focus main-frame))
    (redisplay 'force)
    (sit-for 0.05)
    (garbage-collect)
    (ewm-skia-profile-report-rss
     "multi-frame-release" ewm-skia-profile-frame-count before-kb
     (ewm-skia-profile-rss-kb))
    (ewm-skia-profile-report-context-state
     "main-after-release" main-frame 0)))

(let ((main-frame (selected-frame))
      (before-kb (ewm-skia-profile-rss-kb))
      (start (current-time)))
  (dotimes (cycle (max 1 ewm-skia-profile-churn-cycles))
    (let ((frames nil))
      (unwind-protect
          (progn
            (dotimes (index ewm-skia-profile-frame-count)
              (let ((frame (make-frame `((width . 64)
                                         (height . 16)
                                         (minibuffer . nil)
                                         (visibility . t)))))
                (push frame frames)
                (select-frame-set-input-focus frame)
                (switch-to-buffer "*ewm-skia-profile*")
                (goto-char (point-min))
                (forward-line (+ (* cycle 20) (* index 10)))
                (ewm-skia-profile-redraw)
                (unless (pgtk-skia-gl-enabled-p frame)
                  (ewm-skia-profile-fail
                   "Skia GL acceleration is not active for churn frame"))
                (ewm-skia-profile-report-context-state
                 "frame-churn-cache"
                 frame (+ (* cycle ewm-skia-profile-frame-count) index 1))))
            (ewm-skia-profile-redraw-frames frames))
        (dolist (frame frames)
          (when (frame-live-p frame)
            (delete-frame frame t)))
        (when (frame-live-p main-frame)
          (select-frame-set-input-focus main-frame))
        (redisplay 'force)
        (sit-for 0.03)
        (garbage-collect))
      (ewm-skia-profile-report-rss
       "frame-churn-rss" (* (1+ cycle) ewm-skia-profile-frame-count)
       before-kb (ewm-skia-profile-rss-kb))
      (ewm-skia-profile-report-context-state
       "main-after-churn" main-frame (1+ cycle))))
  (ewm-skia-profile-report
   "frame-churn" (* (max 1 ewm-skia-profile-churn-cycles)
                    ewm-skia-profile-frame-count)
   (ewm-skia-profile-seconds start)))

(when (timerp ewm-skia-profile-watchdog)
  (cancel-timer ewm-skia-profile-watchdog))
(kill-emacs 0))
  (error
   (ewm-skia-profile-report-error err)))
EOF

set +e
timeout --kill-after=1s "$timeout_seconds" \
    env GDK_BACKEND=wayland NO_AT_BRIDGE=1 \
        EWM_SKIA_PROFILE_TIMEOUT_SECONDS="$timeout_seconds" \
        EWM_SKIA_PROFILE_ITERATIONS="$iterations" \
        EWM_SKIA_PROFILE_OUTPUT="$result" \
        EWM_SKIA_PROFILE_PAGE_SIZE="$page_size" \
        EWM_SKIA_HARNESS_MONITOR_EL="$script_dir/skia-monitor.el" \
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
