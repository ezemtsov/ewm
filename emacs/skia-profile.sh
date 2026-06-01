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
trim_module=""
stdout="$tmp_dir/stdout"
stderr="$tmp_dir/stderr"
result="$tmp_dir/result"

if [[ -n "${EWM_SKIA_PROFILE_MALLOC_TRIM:-}" ]]; then
    emacs_binary_dir="$(cd "$(dirname "$emacs_path")" && pwd -P)"
    module_header="$emacs_binary_dir/emacs-module.h"
    if [[ ! -f "$module_header" ]]; then
        echo "error: emacs-module.h not found next to $emacs_path" >&2
        exit 1
    fi

    trim_module_src="$tmp_dir/malloc-trim-module.c"
    trim_module="$tmp_dir/malloc-trim-module.so"
    cat >"$trim_module_src" <<'EOF_MODULE'
#include <emacs-module.h>
#include <malloc.h>

int plugin_is_GPL_compatible;

static emacs_value
Fmalloc_trim (emacs_env *env, ptrdiff_t nargs, emacs_value args[],
	      void *data)
{
  (void) nargs;
  (void) args;
  (void) data;
  return env->make_integer (env, malloc_trim (0));
}

int
emacs_module_init (struct emacs_runtime *runtime)
{
  emacs_env *env = runtime->get_environment (runtime);
  emacs_value symbol = env->intern (env, "ewm-skia-profile--malloc-trim");
  emacs_value function =
    env->make_function (env, 0, 0, Fmalloc_trim,
			"Return non-nil if malloc_trim released memory.",
			NULL);
  emacs_value args[] = { symbol, function };
  env->funcall (env, env->intern (env, "defalias"), 2, args);
  return 0;
}
EOF_MODULE
    "${CC:-cc}" -fPIC -shared -I"$emacs_binary_dir" \
        -o "$trim_module" "$trim_module_src"
fi

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
(defconst ewm-skia-profile-animation-iterations
  (string-to-number
   (or (getenv "EWM_SKIA_PROFILE_ANIMATION_ITERATIONS") "100")))
(defconst ewm-skia-profile-release-settle-steps
  (string-to-number
   (or (getenv "EWM_SKIA_PROFILE_RELEASE_SETTLE_STEPS") "3")))
(defconst ewm-skia-profile-release-settle-delay
  (string-to-number
   (or (getenv "EWM_SKIA_PROFILE_RELEASE_SETTLE_DELAY") "0.10")))
(defconst ewm-skia-profile-smaps-top-count
  (string-to-number (or (getenv "EWM_SKIA_PROFILE_SMAPS_TOP") "5")))
(defconst ewm-skia-profile-malloc-trim-module
  (getenv "EWM_SKIA_PROFILE_MALLOC_TRIM_MODULE"))
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
(when (and ewm-skia-profile-malloc-trim-module
           (not (string-empty-p ewm-skia-profile-malloc-trim-module)))
  (module-load ewm-skia-profile-malloc-trim-module))

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

(defun ewm-skia-profile-read-kb-fields (file keys)
  (when (file-readable-p file)
    (with-temp-buffer
      (insert-file-contents file)
      (let ((values nil))
        (while (re-search-forward
                "^\\([^:\n]+\\):[ \t]+\\([0-9]+\\)[ \t]+kB\\b"
                nil t)
          (let ((key (match-string 1))
                (value (string-to-number (match-string 2))))
            (when (member key keys)
              (push (cons key value) values))))
        (mapcar (lambda (key)
                  (cons key (cdr (assoc key values))))
                keys)))))

(defun ewm-skia-profile-report-kb-fields (source name fields)
  (when fields
    (ewm-skia-profile-report-line
     (mapconcat
      #'identity
      (cons (format "%-18s phase=%s" source name)
            (mapcar
             (lambda (field)
               (format "%s-kb=%S"
                       (replace-regexp-in-string "_" "-" (car field))
                       (cdr field)))
             fields))
      " "))))

(defun ewm-skia-profile-report-proc-memory (name)
  (ewm-skia-profile-report-line
   (format "%-18s phase=%s live-frames=%d"
           "live-frames" name (length (frame-list))))
  (ewm-skia-profile-report-kb-fields
   "status" name
   (ewm-skia-profile-read-kb-fields
    "/proc/self/status"
    '("VmRSS" "RssAnon" "RssFile" "RssShmem" "VmData" "VmLib")))
  (ewm-skia-profile-report-kb-fields
   "smaps-rollup" name
   (ewm-skia-profile-read-kb-fields
    "/proc/self/smaps_rollup"
    '("Rss" "Pss" "Shared_Clean" "Shared_Dirty"
      "Private_Clean" "Private_Dirty" "Anonymous" "LazyFree"
      "AnonHugePages" "FilePmdMapped" "ShmemPmdMapped"))))

(defun ewm-skia-profile-entry-get (entry key)
  (cdr (assoc key entry)))

(defun ewm-skia-profile-smaps-top-private-dirty (limit)
  (when (file-readable-p "/proc/self/smaps")
    (with-temp-buffer
      (insert-file-contents "/proc/self/smaps")
      (let ((entries nil)
            (entry nil))
        (goto-char (point-min))
        (while (not (eobp))
          (cond
           ((looking-at
             "^\\([0-9a-f]+-[0-9a-f]+\\) \\([-rwxps]+\\) [0-9a-f]+ [0-9a-f:]+ [0-9]+[ \t]*\\(.*\\)$")
            (when entry
              (push entry entries))
            (setq entry
                  (list
                   (cons 'map (match-string 1))
                   (cons 'perms (match-string 2))
                   (cons 'path (let ((path (match-string 3)))
                                 (if (string-empty-p path)
                                     "(anonymous)"
                                   path)))
                   (cons 'Size 0)
                   (cons 'Rss 0)
                   (cons 'Private_Dirty 0)
                   (cons 'Anonymous 0))))
           ((and entry
                 (looking-at
                  "^\\(Size\\|Rss\\|Private_Dirty\\|Anonymous\\):[ \t]+\\([0-9]+\\)[ \t]+kB\\b"))
            (setcdr (assoc (intern (match-string 1)) entry)
                    (string-to-number (match-string 2)))))
          (forward-line 1))
        (when entry
          (push entry entries))
        (let ((sorted (sort entries
                            (lambda (left right)
                              (> (or (ewm-skia-profile-entry-get
                                      left 'Private_Dirty)
                                     0)
                                 (or (ewm-skia-profile-entry-get
                                      right 'Private_Dirty)
                                     0)))))
              (result nil)
              (count 0))
          (while (and sorted (< count limit))
            (let ((entry (pop sorted)))
              (when (> (or (ewm-skia-profile-entry-get
                            entry 'Private_Dirty)
                           0)
                       0)
                (push entry result)
                (setq count (1+ count)))))
          (nreverse result))))))

(defun ewm-skia-profile-report-smaps-top (name)
  (let ((rank 0))
    (dolist (entry (ewm-skia-profile-smaps-top-private-dirty
                    ewm-skia-profile-smaps-top-count))
      (setq rank (1+ rank))
      (ewm-skia-profile-report-line
       (format "%-18s phase=%s rank=%d map=%s perms=%s size-kb=%S rss-kb=%S private-dirty-kb=%S anonymous-kb=%S path=%S"
               "smaps-top" name rank
               (ewm-skia-profile-entry-get entry 'map)
               (ewm-skia-profile-entry-get entry 'perms)
               (ewm-skia-profile-entry-get entry 'Size)
               (ewm-skia-profile-entry-get entry 'Rss)
               (ewm-skia-profile-entry-get entry 'Private_Dirty)
               (ewm-skia-profile-entry-get entry 'Anonymous)
               (ewm-skia-profile-entry-get entry 'path))))))

(defun ewm-skia-profile-report-malloc-trim (name)
  (when (fboundp 'ewm-skia-profile--malloc-trim)
    (let ((before-kb (ewm-skia-profile-rss-kb))
          (result (ewm-skia-profile--malloc-trim)))
      (ewm-skia-profile-report-rss
       (format "malloc-trim-%s" name) 0 before-kb
       (ewm-skia-profile-rss-kb))
      (ewm-skia-profile-report-line
       (format "%-18s phase=%s result=%S"
               "malloc-trim" name result))
      t)))

(defun ewm-skia-profile-report-context-state (name frame index)
  (when (fboundp 'pgtk-skia-test-context-state)
    (let ((state (pgtk-skia-test-context-state frame t))
          (framebuffer-size (and (fboundp 'pgtk-skia-test-framebuffer-size)
                                 (pgtk-skia-test-framebuffer-size frame))))
      (unless (and (consp state)
                   (nth 0 state)
                   (nth 1 state)
                   (eq (nth 2 state) t)
                   (nth 3 state)
                   (integerp (nth 4 state))
                   (integerp (nth 5 state))
                   (nth 6 state)
                   (nth 7 state)
                   (integerp (nth 9 state))
                   (integerp (nth 10 state))
                   (integerp (nth 11 state)))
        (ewm-skia-profile-fail
         "invalid Skia context state for %s frame %d: %S"
         name index state))
      (ewm-skia-profile-report-line
       (format "%-18s frame=%d gdk-context=%S current-gdk-context=%S gdk-current=%S skia-context=%S cache-resources=%S cache-bytes=%S framebuffer=%S texture=%S framebuffer-size=%S gdk-shared-context=%S cache-max-resources=%S cache-max-bytes=%S cache-purgeable-bytes=%S"
               name index
               (nth 0 state) (nth 1 state) (nth 2 state) (nth 3 state)
               (nth 4 state) (nth 5 state) (nth 6 state) (nth 7 state)
               framebuffer-size
               (nth 8 state) (nth 9 state) (nth 10 state)
               (nth 11 state))))))

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

(when (and (> ewm-skia-profile-animation-iterations 0)
           (image-type-available-p 'webp))
  (let* ((source-buffer (current-buffer))
         (soak-buffer (get-buffer-create "*ewm-skia-profile-animation*"))
         (data (base64-decode-string
                "UklGRoQAAABXRUJQVlA4WAoAAAACAAAAAQAAAQAAQU5JTQYAAAD/////AABBTk1GKAAAAAAAAAAAAAEAAAEAAGQAAAJWUDhMDwAAAC8BQAAABxD9j/4HIqL/AQBBTk1GKAAAAAAAAAAAAAEAAAEAAGQAAABWUDhMDwAAAC8BQAAAB9D/iP4HIqL/AQA="))
         (image (create-image data 'webp t
                              :width 400
                              :height 160
                              :transform-smoothing nil))
         (before-kb (ewm-skia-profile-rss-kb))
         (midpoint-target (/ ewm-skia-profile-animation-iterations 2))
         midpoint-kb after-kb
         (frame-index 0)
         (call-count 0))
    (unwind-protect
        (progn
          (with-current-buffer soak-buffer
            (setq-local cursor-type nil)
            (setq-local mode-line-format nil)
            (erase-buffer)
            (insert-image image " "))
          (switch-to-buffer soak-buffer)
          (plist-put (cdr image) :animate-buffer soak-buffer)
          (ewm-skia-profile-redraw)
          (ewm-skia-profile-report-context-state
           "animation-soak-start" (selected-frame) 0)
          (ewm-skia-profile-time
           "animation-soak" ewm-skia-profile-animation-iterations
           (lambda ()
             (setq call-count (1+ call-count))
             (setq frame-index (mod (1+ frame-index) 2))
             (image-show-frame image frame-index t)
             (when (and (not midpoint-kb)
                        (>= call-count midpoint-target))
               (setq midpoint-kb (ewm-skia-profile-rss-kb)))
             (ewm-skia-profile-redraw)))
          (setq after-kb (ewm-skia-profile-rss-kb))
          (when midpoint-kb
            (ewm-skia-profile-report-rss
             "animation-soak-rss-mid" 1 before-kb midpoint-kb))
          (ewm-skia-profile-report-rss
           "animation-soak-rss" 1 before-kb after-kb)
          (ewm-skia-profile-report-context-state
           "animation-soak-end" (selected-frame) 0))
      (clear-image-cache nil image)
      (when (buffer-live-p source-buffer)
        (switch-to-buffer source-buffer))
      (when (buffer-live-p soak-buffer)
        (kill-buffer soak-buffer))
      (ewm-skia-profile-redraw))))

(let ((main-frame (selected-frame))
      (frames nil)
      (before-kb (ewm-skia-profile-rss-kb))
      (multi-iterations (max 1 (/ ewm-skia-profile-iterations 4))))
  (unwind-protect
      (progn
        (ewm-skia-profile-report-context-state
         "main-frame-cache" main-frame 0)
        (ewm-skia-profile-report-proc-memory "before-multi")
        (ewm-skia-profile-report-smaps-top "before-multi")
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
             "multi-frame-cache" frame (1+ index))
            (ewm-skia-profile-report-proc-memory
             (format "multi-frame-%d" (1+ index)))
            (when (= (1+ index) ewm-skia-profile-frame-count)
              (ewm-skia-profile-report-smaps-top
               (format "multi-frame-%d" (1+ index))))))
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
     "main-after-release" main-frame 0)
    (ewm-skia-profile-report-proc-memory "after-release")
    (ewm-skia-profile-report-smaps-top "after-release")
    (when (ewm-skia-profile-report-malloc-trim "after-release")
      (ewm-skia-profile-report-proc-memory "after-release-trim")
      (ewm-skia-profile-report-smaps-top "after-release-trim"))
    (dotimes (step ewm-skia-profile-release-settle-steps)
      (sit-for ewm-skia-profile-release-settle-delay)
      (redisplay 'force)
      (garbage-collect)
      (ewm-skia-profile-report-rss
       (format "release-settle-%d" (1+ step))
       ewm-skia-profile-frame-count before-kb
       (ewm-skia-profile-rss-kb))
      (ewm-skia-profile-report-proc-memory
       (format "release-settle-%d" (1+ step)))
      (when (= (1+ step) ewm-skia-profile-release-settle-steps)
        (ewm-skia-profile-report-smaps-top
         (format "release-settle-%d" (1+ step)))))))

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
       "main-after-churn" main-frame (1+ cycle))
      (ewm-skia-profile-report-proc-memory
       (format "after-churn-%d" (1+ cycle)))))
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
        EWM_SKIA_PROFILE_MALLOC_TRIM_MODULE="$trim_module" \
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
