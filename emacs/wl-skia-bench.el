;;; wl-skia-bench.el --- Wayland/Skia redisplay benchmark -*- lexical-binding: t; -*-

;; This benchmark intentionally stays inside Emacs.  It does not start a
;; subprocess, PTY, terminal emulator, or external producer.  The goal is to
;; stress Emacs redisplay and the Wayland/Skia presentation path directly.

(require 'cl-lib)
(require 'profiler)

(defgroup wl-skia-bench nil
  "Wayland/Skia redisplay benchmarks."
  :group 'frames)

(defcustom wl-skia-bench-output
  "/tmp/wl-skia-bench.txt"
  "Default output path for `wl-skia-bench-run-and-write'."
  :type 'file)

(defcustom wl-skia-bench-monitor-policy "largest"
  "Monitor policy used by batch runs.
Supported values match `ewm-skia-harness-find-monitor' when skia-monitor.el
has been loaded: largest, current, frame, or a monitor-name regexp."
  :type 'string)

(defcustom wl-skia-bench-frame-columns 180
  "Requested frame width in character columns."
  :type 'integer)

(defcustom wl-skia-bench-frame-lines 55
  "Requested frame height in character lines."
  :type 'integer)

(defcustom wl-skia-bench-sample-seconds 5.0
  "Default seconds per benchmark workload."
  :type 'number)

(defcustom wl-skia-bench-workloads
  '(dense-text face-churn cursor-dance scroll image-churn)
  "Benchmark workloads run by `wl-skia-bench-run-suite'."
  :type '(repeat symbol))

(defcustom wl-skia-bench-profile nil
  "When non-nil, collect Emacs CPU and memory profiler reports.
This is intentionally off by default because memory profiling changes the
runtime profile of mutation-heavy redisplay workloads."
  :type 'boolean)

(defcustom wl-skia-bench-frame-delay 0.001
  "Seconds to yield after each forced redisplay.
The default allows Wayland frame callbacks and presentation telemetry to
advance between mutations.  Set this to 0 to measure coalesced redisplay
throughput without pacing."
  :type 'number)

(defvar wl-skia-bench--lines nil)
(defvar wl-skia-bench--current-result nil)

(defun wl-skia-bench--number-env (name fallback)
  (let ((value (getenv name)))
    (if (and value (> (length value) 0))
        (string-to-number value)
      fallback)))

(defun wl-skia-bench--string-env (name fallback)
  (let ((value (getenv name)))
    (if (and value (> (length value) 0))
        value
      fallback)))

(defun wl-skia-bench--bool-env (name)
  (member (downcase (or (getenv name) ""))
          '("1" "t" "true" "yes" "on")))

(defun wl-skia-bench--push (fmt &rest args)
  (push (apply #'format fmt args) wl-skia-bench--lines))

(defun wl-skia-bench--format-alist (alist)
  (mapconcat
   (lambda (kv)
     (format "%s=%S" (car kv) (cdr kv)))
   alist
   " "))

(defun wl-skia-bench--presentation-stats (&optional reset)
  (cond
   ((fboundp 'pwayl-skia-test-presentation-stats)
    (pwayl-skia-test-presentation-stats nil reset))
   ((fboundp 'pgtk-skia-gl-presentation-stats)
    (pgtk-skia-gl-presentation-stats nil reset))))

(defun wl-skia-bench--framebuffer-size ()
  (cond
   ((fboundp 'pwayl-skia-test-framebuffer-size)
    (pwayl-skia-test-framebuffer-size nil))
   ((and (fboundp 'pgtk-skia-gl-texture-alpha-size)
         (eq window-system 'pgtk))
    (pgtk-skia-gl-texture-alpha-size nil))))

(defun wl-skia-bench--surface-state ()
  (and (fboundp 'pwayl-surface-test-state)
       (pwayl-surface-test-state nil)))

(defun wl-skia-bench--presentation-value (stats key)
  (or (cdr (assq key stats)) 0))

(defun wl-skia-bench--workload-buffer ()
  (let ((buf (get-buffer-create "*wl-skia-bench*")))
    (with-current-buffer buf
      (setq buffer-read-only nil)
      (setq-local bidi-display-reordering nil)
      (setq-local cursor-type 'box)
      (setq-local truncate-lines t)
      (setq-local show-trailing-whitespace nil)
      (erase-buffer))
    buf))

(defun wl-skia-bench--populate-text (rows cols)
  (let ((faces [default font-lock-keyword-face font-lock-string-face
                font-lock-comment-face font-lock-function-name-face
                font-lock-constant-face]))
    (dotimes (row rows)
      (let ((start (point)))
        (insert (format "%04d " row))
        (dotimes (col cols)
          (insert (aref "0123456789abcdefABCDEF-=+_.,;:/?[]" (% (+ row col) 32))))
        (insert "\n")
        (when (zerop (% row 3))
          (put-text-property
           start (line-end-position 0)
           'face (aref faces (% row (length faces)))))))))

(defun wl-skia-bench--ensure-text-fixture (rows cols)
  (let ((buf (wl-skia-bench--workload-buffer)))
    (switch-to-buffer buf)
    (wl-skia-bench--populate-text rows cols)
    (goto-char (point-min))
    (redisplay 'force)
    buf))

(defun wl-skia-bench--make-xpm (size a b)
  (let ((lines nil))
    (dotimes (y size)
      (let ((line (make-string size ?.)))
        (dotimes (x size)
          (aset line x
                (cond
                 ((zerop (% (+ x y) 11)) ?x)
                 ((zerop (% (+ (* x 3) y) 7)) ?o)
                 (t ?.))))
        (push line lines)))
    (create-image
     (concat
      (format "/* XPM */\nstatic char *bench[] = {\n\"%d %d 3 1\",\n"
              size size)
      "\". c #101820\",\n"
      (format "\"x c %s\",\n" a)
      (format "\"o c %s\",\n" b)
      (mapconcat (lambda (line) (format "\"%s\"" line))
                 (nreverse lines)
                 ",\n")
      "\n};\n")
     'xpm t)))

(defun wl-skia-bench--visible-window-lines ()
  (max 1 (window-body-height nil t)))

(defun wl-skia-bench--visible-window-cols ()
  (max 1 (window-body-width nil t)))

(defun wl-skia-bench--run-loop (seconds step)
  (let ((deadline (+ (float-time) seconds))
        (count 0))
    (while (< (float-time) deadline)
      (funcall step count)
      (force-window-update (selected-window))
      (redisplay 'force)
      (when (> wl-skia-bench-frame-delay 0)
        (sit-for wl-skia-bench-frame-delay))
      (setq count (1+ count)))
    count))

(defun wl-skia-bench--bench-dense-text (seconds)
  (let* ((rows (* 4 (wl-skia-bench--visible-window-lines)))
         (cols (max 120 (wl-skia-bench--visible-window-cols))))
    (wl-skia-bench--ensure-text-fixture rows cols)
    (wl-skia-bench--run-loop
     seconds
     (lambda (count)
       (goto-char (point-min))
       (forward-line (% count rows))))))

(defun wl-skia-bench--bench-face-churn (seconds)
  (let* ((rows (* 4 (wl-skia-bench--visible-window-lines)))
         (cols (max 120 (wl-skia-bench--visible-window-cols)))
         (buf (wl-skia-bench--ensure-text-fixture rows cols))
         (positions nil)
         (faces [highlight region isearch lazy-highlight mode-line]))
    (with-current-buffer buf
      (save-excursion
        (goto-char (point-min))
        (dotimes (_ 256)
          (push (point) positions)
          (forward-char 7))))
    (setq positions (vconcat (nreverse positions)))
    (wl-skia-bench--run-loop
     seconds
     (lambda (count)
       (let* ((idx (% count (length positions)))
              (pos (aref positions idx))
              (end (min (point-max) (+ pos 12))))
         (put-text-property
          pos end 'face (aref faces (% count (length faces)))))))))

(defun wl-skia-bench--bench-cursor-dance (seconds)
  (let* ((rows (* 4 (wl-skia-bench--visible-window-lines)))
         (cols (max 120 (wl-skia-bench--visible-window-cols))))
    (wl-skia-bench--ensure-text-fixture rows cols)
    (wl-skia-bench--run-loop
     seconds
     (lambda (count)
       (goto-char (point-min))
       (forward-line (% count rows))
       (forward-char (% (* count 13) (max 1 (- cols 4))))))))

(defun wl-skia-bench--bench-scroll (seconds)
  (let* ((rows (* 8 (wl-skia-bench--visible-window-lines)))
         (cols (max 120 (wl-skia-bench--visible-window-cols))))
    (wl-skia-bench--ensure-text-fixture rows cols)
    (wl-skia-bench--run-loop
     seconds
     (lambda (count)
       (goto-char (point-min))
       (forward-line (% count (max 1 (- rows 2))))
       (set-window-start (selected-window) (point) t)))))

(defun wl-skia-bench--bench-image-churn (seconds)
  (let* ((buf (wl-skia-bench--workload-buffer))
         (images (vector
                  (wl-skia-bench--make-xpm 96 "#2f80ed" "#f2c94c")
                  (wl-skia-bench--make-xpm 96 "#27ae60" "#eb5757")
                  (wl-skia-bench--make-xpm 96 "#9b51e0" "#56ccf2")))
         (rows (max 12 (/ (wl-skia-bench--visible-window-lines) 2)))
         positions)
    (switch-to-buffer buf)
    (dotimes (row rows)
      (let ((pos (point)))
        (insert-image (aref images (% row (length images)))
                      (format "img%02d" row))
        (insert "  image churn row " (number-to-string row) "\n")
        (push pos positions)))
    (setq positions (vconcat (nreverse positions)))
    (goto-char (point-min))
    (redisplay 'force)
    (wl-skia-bench--run-loop
     seconds
     (lambda (count)
       (let ((inhibit-read-only t)
             (pos (aref positions (% count (length positions)))))
         (goto-char pos)
         (delete-char 1)
         (insert-image (aref images (% count (length images)))))))))

(defun wl-skia-bench--run-workload (name seconds)
  (pcase name
    ('dense-text (wl-skia-bench--bench-dense-text seconds))
    ('face-churn (wl-skia-bench--bench-face-churn seconds))
    ('cursor-dance (wl-skia-bench--bench-cursor-dance seconds))
    ('scroll (wl-skia-bench--bench-scroll seconds))
    ('image-churn (wl-skia-bench--bench-image-churn seconds))
    (_ (error "Unknown workload: %S" name))))

(defun wl-skia-bench--workload-list-from-env ()
  (let ((value (getenv "WL_SKIA_BENCH_WORKLOADS")))
    (if (and value (> (length value) 0))
        (mapcar #'intern (split-string value "," t "[ \t\n]+"))
      wl-skia-bench-workloads)))

(defun wl-skia-bench--maybe-position-frame ()
  (when (and (fboundp 'ewm-skia-harness-find-monitor)
             (fboundp 'ewm-skia-harness-position-frame-on-monitor))
    (let ((mon (ewm-skia-harness-find-monitor wl-skia-bench-monitor-policy)))
      (when mon
        (wl-skia-bench--push
         "%s" (ewm-skia-harness-monitor-string "target-monitor" mon))
        (ewm-skia-harness-position-frame-on-monitor (selected-frame) mon)))))

(defun wl-skia-bench--prepare-frame ()
  (setq frame-resize-pixelwise t)
  (when (display-graphic-p)
    (wl-skia-bench--maybe-position-frame)
    (set-frame-size nil wl-skia-bench-frame-columns wl-skia-bench-frame-lines)
    (redisplay 'force)))

(defun wl-skia-bench--frame-report (label)
  (wl-skia-bench--push
   "%s window-system=%S graphical=%S pixel-size=%dx%d chars=%dx%d framebuffer=%S"
   label
   window-system
   (display-graphic-p)
   (frame-pixel-width)
   (frame-pixel-height)
   (frame-width)
   (frame-height)
   (wl-skia-bench--framebuffer-size))
  (let ((state (wl-skia-bench--surface-state)))
    (when state
      (wl-skia-bench--push "%s surface=%S" label state))))

(defun wl-skia-bench--write-profiler-reports (output)
  (profiler-report)
  (let ((stem (file-name-sans-extension output)))
    (dolist (buf (buffer-list))
      (let ((name (buffer-name buf)))
        (when (string-match "\\`\\*\\(CPU\\|Memory\\)-Profiler-Report" name)
          (let ((path (format "%s-%s.txt"
                              stem
                              (downcase (match-string 1 name)))))
            (with-current-buffer buf
              (let ((coding-system-for-write 'utf-8))
                (write-region (point-min) (point-max) path nil 'silent)))
            (wl-skia-bench--push "profiler kind=%s path=%s"
                                 (downcase (match-string 1 name))
                                 path)))))))

;;;###autoload
(defun wl-skia-bench-run-suite (&optional seconds workloads)
  "Run redisplay workloads and return result lines.
SECONDS defaults to `wl-skia-bench-sample-seconds'.  WORKLOADS defaults to
`wl-skia-bench-workloads'."
  (interactive)
  (let* ((seconds (or seconds wl-skia-bench-sample-seconds))
         (workloads (or workloads wl-skia-bench-workloads))
         (wl-skia-bench--lines nil)
         (gc-cons-threshold most-positive-fixnum)
         (inhibit-message t)
         (inhibit-redisplay nil)
         (debug-on-error nil))
    (unless (display-graphic-p)
      (error "wl-skia-bench requires a graphical frame"))
    (wl-skia-bench--push
     "bench name=wl-skia-bench version=3 emacs-pid=%d"
     (emacs-pid))
    (wl-skia-bench--push
     "params seconds=%.3f workloads=%S requested-chars=%dx%d monitor=%S profile=%S frame-delay=%.4f"
     seconds workloads
     wl-skia-bench-frame-columns
     wl-skia-bench-frame-lines
     wl-skia-bench-monitor-policy
     wl-skia-bench-profile
     wl-skia-bench-frame-delay)
    (wl-skia-bench--prepare-frame)
    (wl-skia-bench--frame-report "frame-before")
    (wl-skia-bench--push
     "features=%s"
     (replace-regexp-in-string
      "[ \t\n]+" "," system-configuration-features))
    (wl-skia-bench--push
     "presentation-stats=%S"
     (not (null (wl-skia-bench--presentation-stats nil))))
    (when wl-skia-bench-profile
      (profiler-start 'cpu+mem))
    (unwind-protect
        (dolist (workload workloads)
          (garbage-collect)
          (wl-skia-bench--presentation-stats t)
          (let* ((start-gcs gcs-done)
                 (start-time (float-time))
                 (iterations (wl-skia-bench--run-workload workload seconds))
                 (elapsed (- (float-time) start-time))
                 (after (or (wl-skia-bench--presentation-stats nil) nil))
                 (presents (wl-skia-bench--presentation-value
                            after 'present-count))
                 (present-pixels (wl-skia-bench--presentation-value
                                  after 'present-pixels))
                 (draws (wl-skia-bench--presentation-value
                         after 'draw-count))
                 (draw-pixels (wl-skia-bench--presentation-value
                               after 'draw-clip-pixels))
                 (area (wl-skia-bench--presentation-value
                        after 'queue-area-count))
                 (full (wl-skia-bench--presentation-value
                        after 'queue-full-count))
                 (copy-gl (wl-skia-bench--presentation-value
                           after 'copy-gl-count))
                 (copy-fallback (wl-skia-bench--presentation-value
                                 after 'copy-fallback-count))
                 (copy-failure (wl-skia-bench--presentation-value
                                after 'copy-failure-count)))
            (wl-skia-bench--push
             (concat
              "workload name=%S elapsed=%.6f iterations=%d iterations-per-sec=%.3f "
              "presents=%d presents-per-sec=%.3f present-pixels=%d present-mpix-per-sec=%.3f "
              "draws=%d draw-pixels=%d draw-mpix-per-sec=%.3f area-queues=%d full-queues=%d "
              "copy-gl=%d copy-fallback=%d copy-failure=%d gcs=%d")
             workload elapsed iterations
             (/ iterations (max elapsed 0.001))
             presents (/ presents (max elapsed 0.001))
             present-pixels (/ present-pixels 1000000.0 (max elapsed 0.001))
             draws draw-pixels (/ draw-pixels 1000000.0 (max elapsed 0.001))
             area full copy-gl copy-fallback copy-failure
             (- gcs-done start-gcs))))
      (when wl-skia-bench-profile
        (profiler-stop)))
    (wl-skia-bench--frame-report "frame-after")
    (setq wl-skia-bench--current-result (nreverse wl-skia-bench--lines))
    (when (called-interactively-p 'interactive)
      (with-current-buffer (get-buffer-create "*wl-skia-bench-results*")
        (let ((inhibit-read-only t))
          (erase-buffer)
          (insert (mapconcat #'identity wl-skia-bench--current-result "\n"))
          (insert "\n"))
        (display-buffer (current-buffer))))
    wl-skia-bench--current-result))

;;;###autoload
(defun wl-skia-bench-run-and-write (&optional output seconds workloads)
  "Run the benchmark suite and write results to OUTPUT.
When called interactively, use `wl-skia-bench-output',
`wl-skia-bench-sample-seconds', and `wl-skia-bench-workloads'."
  (interactive)
  (let* ((output (or output wl-skia-bench-output))
         (seconds (or seconds wl-skia-bench-sample-seconds))
         (workloads (or workloads wl-skia-bench-workloads))
         (lines (wl-skia-bench-run-suite seconds workloads)))
    (with-temp-buffer
      (insert (mapconcat #'identity lines "\n"))
      (insert "\n")
      (let ((coding-system-for-write 'utf-8))
        (write-region (point-min) (point-max) output nil 'silent)))
    (when wl-skia-bench-profile
      (wl-skia-bench--write-profiler-reports output)
      (with-temp-buffer
        (insert (mapconcat #'identity (nreverse wl-skia-bench--lines) "\n"))
        (insert "\n")
        (write-region (point-min) (point-max) output nil 'silent)))
    output))

(defun wl-skia-bench-batch-main ()
  (setq wl-skia-bench-output
        (wl-skia-bench--string-env "WL_SKIA_BENCH_OUTPUT"
                                   wl-skia-bench-output)
        wl-skia-bench-monitor-policy
        (wl-skia-bench--string-env "WL_SKIA_BENCH_MONITOR"
                                   wl-skia-bench-monitor-policy)
        wl-skia-bench-frame-columns
        (wl-skia-bench--number-env "WL_SKIA_BENCH_COLS"
                                   wl-skia-bench-frame-columns)
        wl-skia-bench-frame-lines
        (wl-skia-bench--number-env "WL_SKIA_BENCH_LINES"
                                   wl-skia-bench-frame-lines)
        wl-skia-bench-sample-seconds
        (wl-skia-bench--number-env "WL_SKIA_BENCH_SECONDS"
                                   wl-skia-bench-sample-seconds)
        wl-skia-bench-frame-delay
        (wl-skia-bench--number-env "WL_SKIA_BENCH_FRAME_DELAY"
                                   wl-skia-bench-frame-delay)
        wl-skia-bench-profile
        (wl-skia-bench--bool-env "WL_SKIA_BENCH_PROFILE"))
  (wl-skia-bench-run-and-write
   wl-skia-bench-output
   wl-skia-bench-sample-seconds
   (wl-skia-bench--workload-list-from-env)))

(provide 'wl-skia-bench)

;;; wl-skia-bench.el ends here
