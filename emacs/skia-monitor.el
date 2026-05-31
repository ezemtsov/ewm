;;; skia-monitor.el --- shared monitor helpers for Skia harnesses

(defun ewm-skia-harness-monitor-rect (monitor key)
  (let ((rect (cdr (assq key monitor))))
    (and (consp rect) rect)))

(defun ewm-skia-harness-rect-area (rect)
  (if (and (consp rect)
           (numberp (nth 2 rect))
           (numberp (nth 3 rect)))
      (* (nth 2 rect) (nth 3 rect))
    0))

(defun ewm-skia-harness-find-monitor (policy)
  (cond
   ((or (string= policy "current")
        (string= policy "frame"))
    (frame-monitor-attributes))
   ((string= policy "largest")
    (let ((best nil)
          (best-area -1))
      (dolist (monitor (display-monitor-attributes-list))
        (let* ((workarea (ewm-skia-harness-monitor-rect monitor 'workarea))
               (geometry (ewm-skia-harness-monitor-rect monitor 'geometry))
               (area (max (ewm-skia-harness-rect-area workarea)
                          (ewm-skia-harness-rect-area geometry))))
          (when (> area best-area)
            (setq best monitor
                  best-area area))))
      best))
   (t
    (catch 'match
      (dolist (monitor (display-monitor-attributes-list))
        (let ((name (format "%s" (cdr (assq 'name monitor)))))
          (when (string-match-p (regexp-quote policy) name)
            (throw 'match monitor))))))))

(defun ewm-skia-harness-monitor-string (label monitor)
  (format
   "%s name=%S geometry=%S workarea=%S scale=%S source=%S"
   label
   (cdr (assq 'name monitor))
   (ewm-skia-harness-monitor-rect monitor 'geometry)
   (ewm-skia-harness-monitor-rect monitor 'workarea)
   (cdr (assq 'scale-factor monitor))
   (cdr (assq 'source monitor))))

(defun ewm-skia-harness-same-monitor-p (left right)
  (and (equal (cdr (assq 'name left))
              (cdr (assq 'name right)))
       (equal (ewm-skia-harness-monitor-rect left 'geometry)
              (ewm-skia-harness-monitor-rect right 'geometry))))

(defun ewm-skia-harness-position-frame-on-monitor (frame monitor)
  (let ((workarea (ewm-skia-harness-monitor-rect monitor 'workarea)))
    (when workarea
      (set-frame-position frame
                          (+ (nth 0 workarea) 24)
                          (+ (nth 1 workarea) 24)))))

;;; skia-monitor.el ends here
