;;; ewm-session-tests.el --- live checks of an EWM session  -*- lexical-binding: t; -*-

;; Checks against the EWM session they are loaded into, by hand or from
;; nix/vm-test.nix:
;;
;;   emacsclient -s SOCKET --eval \
;;     "(progn (load \"…/ewm-session-tests.el\") (ewm-session-tests-run))"
;;
;; Each check returns its "PASS <check> — <detail>" line or signals the
;; FAIL line, and emacsclient exits 1 on a signal.  The checks run in a
;; frame of their own: `ewm-session-tests-setup' opens it,
;; `ewm-session-tests-teardown' closes it.

(require 'cl-lib)

(declare-function ewm-frame-new "ewm-commands")
(declare-function ewm-frame-close "ewm-commands")
(declare-function ewm-frame-move-to-next-output "ewm-commands")
(declare-function ewm-toggle-fullscreen "ewm-commands")
(declare-function ewm-output-active-frame "ewm-frame" (output))
(declare-function ewm--current-frame "ewm-frame")
(declare-function ewm--frame-output "ewm-frame" (frame))
(declare-function ewm--window-body-logical-rect "ewm-layout" (window &optional scale))
(defvar ewm--output-info)

(defvar ewm-session-tests-dir nil
  "Directory for a screenshot per check, nil for none.")

(defvar ewm-session-tests--frame nil)
(defvar ewm-session-tests--foot nil)
(defvar ewm-session-tests--locker nil)
(defconst ewm-session-tests--buffer "*ewm-session-tests*")

;;;; report and waits

(defun ewm-session-tests--check (name ok detail)
  "The PASS line of check NAME with DETAIL when OK, else signal the FAIL line."
  (let ((line (format "%s %s — %s" (if ok "PASS" "FAIL") name detail)))
    (if ok line (error "%s" line))))

(defun ewm-session-tests--sit-for (seconds)
  "Dispatch events for SECONDS, `sit-for' reads nothing while input is pending."
  (let ((deadline (+ (float-time) seconds)))
    (redisplay)
    (while (< (float-time) deadline)
      (let ((unread-command-events nil))
        (read-event nil t (max 0.01 (- deadline (float-time))))))))

(defun ewm-session-tests--wait (pred &optional seconds)
  "Dispatch events until PRED returns non-nil or SECONDS pass, return its value."
  (let ((deadline (+ (float-time) (or seconds 30)))
        value)
    (while (and (not (setq value (funcall pred))) (< (float-time) deadline))
      (ewm-session-tests--sit-for 0.1))
    value))

;;;; pixels

(defun ewm-session-tests--fractions (output checks)
  "The fraction of each (RECT COLOR . _) in CHECKS on one grim capture of OUTPUT.
RECT is (X Y WIDTH HEIGHT), nil for the bottom eight rows."
  (with-temp-buffer
    (set-buffer-multibyte nil)
    (let ((coding-system-for-read 'binary))
      (unless (eq (call-process "grim" nil t nil "-t" "ppm" "-o" output "-") 0)
        (error "grim failed: %s" (string-trim (buffer-string)))))
    (goto-char (point-min))
    (unless (looking-at "P6\n\\([0-9]+\\) \\([0-9]+\\)\n255\n")
      (error "unexpected PPM header from grim"))
    (let ((width (string-to-number (match-string 1)))
          (height (string-to-number (match-string 2)))
          (start (match-end 0)))
      (mapcar (pcase-lambda (`(,rect ,color . ,_))
                (pcase-let ((`(,x0 ,y0 ,w ,h) (or rect (list 0 (- height 8) width 8)))
                            (hits 0)
                            (total 0))
                  (cl-loop for y from y0 below (+ y0 h) by 4
                           do (cl-loop for x from x0 below (+ x0 w) by 4
                                       for i = (+ start (* 3 (+ (* y width) x)))
                                       do (cl-incf total)
                                       when (funcall color (get-byte i) (get-byte (1+ i)) (get-byte (+ i 2)))
                                       do (cl-incf hits)))
                  (/ (float hits) total)))
              checks))))

(defun ewm-session-tests--wait-pixels (output checks &optional seconds)
  "Wait until each (RECT COLOR OP LIMIT) in CHECKS holds on one capture of
OUTPUT, return (HELD FRACTION...)."
  (let (fractions)
    (cons (ewm-session-tests--wait
           (lambda ()
             (setq fractions (ewm-session-tests--fractions output checks))
             (cl-every (pcase-lambda (fraction `(,_ ,_ ,op ,limit)) (funcall op fraction limit))
                       fractions checks))
           seconds)
          fractions)))

(defun ewm-session-tests--red-p (r g b)
  (and (> r 200) (< g 60) (< b 60)))

(defun ewm-session-tests--gray-p (r g b)
  (and (< (abs (- r #x33)) 8) (< (abs (- g #x33)) 8) (< (abs (- b #x33)) 8)))

(defun ewm-session-tests--shot (name output)
  (when ewm-session-tests-dir
    (make-directory ewm-session-tests-dir t)
    (call-process "grim" nil nil nil "-t" "png" "-o" output
                  (expand-file-name (concat name ".png") ewm-session-tests-dir))))

;;;; the frame and its app

(defun ewm-session-tests--focus-frame (frame)
  "Give FRAME the keyboard and wait until the compositor agrees."
  (select-frame-set-input-focus frame)
  (unless (ewm-session-tests--wait (lambda () (eq (ewm--current-frame) frame)) 10)
    (error "FAIL focus — the frame did not take the keyboard")))

(defun ewm-session-tests--focus ()
  "Focus the frame of the checks on its buffer, return its output."
  (ewm-session-tests--focus-frame ewm-session-tests--frame)
  (with-selected-frame ewm-session-tests--frame
    (switch-to-buffer ewm-session-tests--buffer))
  (ewm--frame-output ewm-session-tests--frame))

(defun ewm-session-tests--body-rect (window)
  "The text area of WINDOW in device pixels of its output, (X Y WIDTH HEIGHT)."
  (let ((scale (alist-get 'scale (alist-get (ewm--frame-output (window-frame window))
                                            ewm--output-info nil nil #'string=))))
    (mapcar (lambda (v) (round (* v scale))) (ewm--window-body-logical-rect window))))

(defun ewm-session-tests--foot-buffer ()
  (and ewm-session-tests--foot
       (let ((pid (process-id ewm-session-tests--foot)))
         (seq-find (lambda (b) (eq (buffer-local-value 'ewm-surface-pid b) pid)) (buffer-list)))))

(defun ewm-session-tests--foot-window ()
  "Select and return the window of the red foot, started on the frame if needed."
  (unless (ewm-session-tests--foot-buffer)
    (ewm-session-tests--focus)
    (setq ewm-session-tests--foot
          (make-process :name "ewm-session-tests-foot" :noquery t
                        :command '("foot" "-o" "colors-dark.background=ff0000"
                                   "-o" "colors-light.background=ff0000" "sh" "-c" "sleep infinity"))))
  (let ((window (ewm-session-tests--wait
                 (lambda ()
                   (let ((buffer (ewm-session-tests--foot-buffer)))
                     (and buffer (get-buffer-window buffer t))))
                 60)))
    (unless window
      (error "FAIL foot — no window within 60 s"))
    (ewm-session-tests--focus-frame (window-frame window))
    (select-window window)))

(defun ewm-session-tests--close-foot ()
  (when ewm-session-tests--foot
    (signal-process ewm-session-tests--foot 'TERM)
    (ewm-session-tests--wait (lambda () (not (ewm-session-tests--foot-buffer))))
    (setq ewm-session-tests--foot nil)))

;;;; checks

(defun ewm-session-tests-setup ()
  "Open the frame the checks run in and focus it."
  (unless (frame-live-p ewm-session-tests--frame)
    (setq ewm-session-tests--frame (ewm-frame-new)))
  (ewm-session-tests--check "setup" t (format "frame on %s" (ewm-session-tests--focus))))

(defun ewm-session-tests-outputs (&optional scales)
  "Every output shows a frame, at its scale in SCALES ((NAME . SCALE)...)."
  (let ((ok t) seen)
    (dolist (info ewm--output-info)
      (let ((name (car info))
            (scale (alist-get 'scale (cdr info))))
        (unless (and (ewm-session-tests--wait (lambda () (ewm-output-active-frame name)) 60)
                     (= (alist-get name scales scale nil #'equal) scale))
          (setq ok nil))
        (push (format "%s at %s" name scale) seen)))
    (ewm-session-tests--check "outputs" ok (string-join (nreverse seen) ", "))))

(defun ewm-session-tests-typed (text)
  "TEXT typed on the keyboard has reached the frame."
  (let ((ok (with-current-buffer ewm-session-tests--buffer (string-search text (buffer-string)))))
    (when ok
      (ewm-session-tests--shot "typed" (ewm--frame-output ewm-session-tests--frame)))
    (ewm-session-tests--check "typed" ok text)))

(defun ewm-session-tests-child-frame ()
  "A child frame of the frame survives."
  (let ((child (make-frame `((parent-frame . ,ewm-session-tests--frame)
                             (width . 20) (height . 4) (visibility . t)))))
    (ewm-session-tests--sit-for 1)
    (let ((live (frame-live-p child)))
      (when live (delete-frame child))
      (ewm-session-tests--check "child frame" live "lives a second later"))))

(defun ewm-session-tests-fullscreen ()
  "An app goes fullscreen over its output and back."
  (let* ((window (ewm-session-tests--foot-window))
         (output (ewm--frame-output (window-frame window)))
         (tiled '((nil ewm-session-tests--red-p < 0.5)))
         (full '((nil ewm-session-tests--red-p > 0.99)))
         (before (car (ewm-session-tests--wait-pixels output tiled)))
         covered after)
    (with-selected-window window (ewm-toggle-fullscreen))
    (setq covered (ewm-session-tests--wait-pixels output full))
    (ewm-session-tests--shot "fullscreen" output)
    (with-selected-window window (ewm-toggle-fullscreen))
    (setq after (car (ewm-session-tests--wait-pixels output tiled)))
    (ewm-session-tests--check "fullscreen" (and before (car covered) after)
                              (format "%.2f red over the output" (cadr covered)))))

(defun ewm-session-tests-views ()
  "One app surface shows in two windows at once."
  (let* ((window (ewm-session-tests--foot-window))
         (output (ewm--frame-output (window-frame window)))
         (buffer (window-buffer window))
         ;; Unequal halves, the larger lower window becomes the primary.
         (lower (with-selected-window window (split-window-below 10)))
         (upper-rect (ewm-session-tests--body-rect window))
         (lower-rect (ewm-session-tests--body-rect lower))
         (one `((,upper-rect ewm-session-tests--red-p > 0.95) (,lower-rect ewm-session-tests--red-p < 0.05)))
         (two `((,upper-rect ewm-session-tests--red-p > 0.95) (,lower-rect ewm-session-tests--red-p > 0.95)))
         split both again)
    (set-window-buffer lower ewm-session-tests--buffer)
    (setq split (car (ewm-session-tests--wait-pixels output one)))
    (set-window-buffer lower buffer)
    (setq both (ewm-session-tests--wait-pixels output two))
    (ewm-session-tests--shot "two-views" output)
    (set-window-buffer lower ewm-session-tests--buffer)
    (setq again (car (ewm-session-tests--wait-pixels output one)))
    (delete-window lower)
    (ewm-session-tests--close-foot)
    (ewm-session-tests--check "views" (and split (car both) again)
                              (format "%.2f and %.2f red" (nth 1 both) (nth 2 both)))))

(defun ewm-session-tests-move ()
  "The frame moves to the next output and back."
  (if (< (length ewm--output-info) 2)
      "SKIP move — one output"
    (let* ((frame ewm-session-tests--frame)
           (home (ewm-session-tests--focus))
           ;; pgtk calls every head "QEMU Monitor".
           (monitor (lambda ()
                      (let ((m (frame-monitor-attributes frame)))
                        (list (alist-get 'geometry m) (alist-get 'scale-factor m)))))
           (start (funcall monitor))
           there back)
      (ewm-frame-move-to-next-output)
      (setq there (ewm-session-tests--wait
                   (lambda () (and (not (equal (ewm--frame-output frame) home))
                                   (not (equal (funcall monitor) start))))))
      (ewm-session-tests--shot "moved" (ewm--frame-output frame))
      (ewm-frame-move-to-next-output)
      (setq back (ewm-session-tests--wait
                  (lambda () (and (equal (ewm--frame-output frame) home)
                                  (equal (funcall monitor) start)))))
      (ewm-session-tests--check "move" (and there back) (format "away from %s and back" home)))))

(defun ewm-session-tests-lock ()
  "swaylock covers the output."
  (let ((output (ewm-session-tests--focus)))
    (setq ewm-session-tests--locker
          (make-process :name "ewm-session-tests-swaylock" :noquery t
                        :command '("swaylock" "-c" "333333")))
    (let ((covered (ewm-session-tests--wait-pixels output '((nil ewm-session-tests--gray-p > 0.99)))))
      (ewm-session-tests--check "lock" (car covered) (format "%.2f gray over the output" (cadr covered))))))

(defun ewm-session-tests-unlocked (text)
  "The locker is gone and TEXT, the password typed, never reached the frame."
  (let ((gone (ewm-session-tests--wait (lambda () (not (process-live-p ewm-session-tests--locker))) 60))
        (leaked (with-current-buffer ewm-session-tests--buffer (string-search text (buffer-string)))))
    (ewm-session-tests--check "unlocked" (and gone (not leaked))
                              (cond ((not gone) "locker still running")
                                    (leaked "the password reached the frame")
                                    (t "locker gone, nothing reached the frame")))))

(defun ewm-session-tests-teardown ()
  "Close the app and the frame."
  (ewm-session-tests--close-foot)
  (when (frame-live-p ewm-session-tests--frame)
    (ewm-session-tests--focus)
    (ewm-frame-close))
  (when (get-buffer ewm-session-tests--buffer)
    (kill-buffer ewm-session-tests--buffer))
  "PASS teardown — frame closed")

(defun ewm-session-tests-run ()
  "Run every check that needs no keyboard, return the report."
  (mapconcat (lambda (check)
               (condition-case err
                   (funcall check)
                 (error (error-message-string err))))
             '(ewm-session-tests-outputs ewm-session-tests-setup ewm-session-tests-child-frame
               ewm-session-tests-fullscreen ewm-session-tests-views ewm-session-tests-move
               ewm-session-tests-teardown)
             "\n"))

(provide 'ewm-session-tests)
;;; ewm-session-tests.el ends here
