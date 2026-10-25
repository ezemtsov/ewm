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
(declare-function ewm--frame-scale "ewm-layout" (frame &optional monitors))
(declare-function ewm--float-buffer "ewm-frame" (buffer size pos))
(declare-function ewm--get-frame-origin "ewm-frame" (frame))
(declare-function ewm--get-output-origin "ewm-frame" (output-name))
(declare-function ewm-configure-output "ewm-frame" (name &rest args))
(declare-function ewm-get-active-outputs "ewm-core")
(defvar ewm--output-info)

(defvar ewm-session-tests-dir nil
  "Directory for a screenshot per check, nil for none.")

(defvar ewm-session-tests--frame nil)
(defvar ewm-session-tests--frames nil)
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
RECT is (X Y WIDTH HEIGHT), nil for the whole output, bottom for its last eight rows."
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
                (pcase-let ((`(,x0 ,y0 ,w ,h) (pcase rect
                                                ('nil (list 0 0 width height))
                                                ('bottom (list 0 (- height 8) width 8))
                                                (_ rect)))
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

(defun ewm-session-tests--scale (output)
  (alist-get 'scale (alist-get output ewm--output-info nil nil #'string=)))

(defun ewm-session-tests--device-rect (output rect)
  "RECT, logical pixels of OUTPUT, in its device pixels, two in from each edge."
  (pcase-let ((`(,x ,y ,w ,h) (mapcar (lambda (v) (round (* v (ewm-session-tests--scale output)))) rect)))
    (list (+ x 2) (+ y 2) (- w 4) (- h 4))))

(defun ewm-session-tests--body-at (output window pos)
  "The text area of WINDOW at POS, logical (X . Y) on OUTPUT, in its device pixels."
  (pcase-let ((`(,x ,y ,w ,h) (ewm--window-body-logical-rect window)))
    (ewm-session-tests--device-rect output (list (+ (car pos) x) (+ (cdr pos) y) w h))))

(defun ewm-session-tests--body-rect (window)
  "The text area of WINDOW in device pixels of its output, (X Y WIDTH HEIGHT)."
  (ewm-session-tests--body-at (ewm--frame-output (window-frame window)) window '(0 . 0)))

(defun ewm-session-tests--monitor (frame)
  "Geometry and scale of the monitor under FRAME, pgtk names every head alike."
  (let ((m (frame-monitor-attributes frame)))
    (list (alist-get 'geometry m) (alist-get 'scale-factor m))))

(defun ewm-session-tests--red-buffer (name)
  "The buffer NAME, drawing nothing but red."
  (with-current-buffer (get-buffer-create name)
    (setq-local cursor-type nil)
    (face-remap-add-relative 'default :background "red")
    (current-buffer)))

(defun ewm-session-tests--marker (text)
  (expand-file-name text temporary-file-directory))

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
                        :command (list "foot" "-o" "colors-dark.background=ff0000"
                                       "-o" "colors-light.background=ff0000"
                                       "sh" "-c" "while read line; do touch \"$0/$line\"; done"
                                       temporary-file-directory))))
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

(defun ewm-session-tests-foot ()
  "Focus the app, the keyboard goes to it from here."
  (let ((window (ewm-session-tests--foot-window)))
    (setq ewm-session-tests--frames (frame-list))
    (ewm-session-tests--check "foot" t (format "focused on %s" (ewm--frame-output (window-frame window))))))

(defun ewm-session-tests-foot-typed (text)
  "TEXT typed on the keyboard reached the app."
  (let ((ok (ewm-session-tests--wait (lambda () (file-exists-p (ewm-session-tests--marker text))))))
    (ewm-session-tests--check "foot typed" ok text)))

(defun ewm-session-tests--foot-covered (on)
  "Wait for the app to cover its output when ON, else to sit tiled."
  (let ((output (ewm--frame-output (window-frame (get-buffer-window (ewm-session-tests--foot-buffer) t)))))
    (ewm-session-tests--wait-pixels output (if on
                                               '((nil ewm-session-tests--red-p > 0.99))
                                             '((bottom ewm-session-tests--red-p < 0.5))))))

(defun ewm-session-tests-foot-fullscreen (on)
  "The app covers its output when ON, else its frame shows below it."
  (let ((held (ewm-session-tests--foot-covered on)))
    (ewm-session-tests--check (if on "key fullscreen" "key tiled") (car held)
                              (format "%.2f red over the output" (cadr held)))))

(defun ewm-session-tests-new-frame ()
  "A frame opened since `ewm-session-tests-foot', closed again here."
  (let* ((known ewm-session-tests--frames)
         (new (ewm-session-tests--wait (lambda () (seq-difference (frame-list) known)))))
    (mapc #'delete-frame new)
    (ewm-session-tests--check "bound key" new (format "%d new frame" (length new)))))

(defun ewm-session-tests-child-frame ()
  "A child frame shows where its parent put it."
  (let* ((output (ewm-session-tests--focus))
         (child (make-frame `((parent-frame . ,ewm-session-tests--frame) (left . 60) (top . 40)
                              (width . 20) (height . 4) (minibuffer . nil) (visibility . t))))
         ;; Where the parent was told to put it, its pixels to logical ones.
         (scale (ewm--frame-scale ewm-session-tests--frame))
         rect red)
    (set-window-buffer (frame-root-window child) (ewm-session-tests--red-buffer "*ewm-session-tests-child*"))
    (setq rect (ewm-session-tests--body-at output (frame-root-window child) (cons (/ 60 scale) (/ 40 scale))))
    (setq red (ewm-session-tests--wait-pixels output `((,rect ewm-session-tests--red-p > 0.95))))
    (ewm-session-tests--shot "child-frame" output)
    (delete-frame child)
    (ewm-session-tests--check "child frame" (car red) (format "%.2f red at %s" (cadr red) rect))))

(defun ewm-session-tests-float ()
  "A float opens with its body at the place and size asked for."
  (let* ((output (ewm-session-tests--focus))
         (origin (ewm--get-output-origin output))
         (buffer (ewm-session-tests--red-buffer "*ewm-session-tests-float*"))
         (window (progn (ewm--float-buffer buffer '(400 . 300) '(100 . 80))
                        (get-buffer-window buffer t)))
         (frame (window-frame window))
         (at (ewm-session-tests--wait
              (lambda () (and (frame-parameter frame 'ewm-surface-id) (ewm--get-frame-origin frame)))))
         (pos (and at (cons (- (car at) (car origin)) (- (cdr at) (cdr origin)))))
         (body (ewm--window-body-logical-rect window))
         (rect (ewm-session-tests--body-at output window pos))
         (red (ewm-session-tests--wait-pixels output `((,rect ewm-session-tests--red-p > 0.95)))))
    (ewm-session-tests--shot "float" output)
    (delete-frame frame)
    ;; Emacs rounds a frame to whole lines unless `frame-resize-pixelwise' says otherwise.
    (ewm-session-tests--check "float" (and (equal pos '(100 . 80))
                                           (< (abs (- (nth 2 body) 400)) (frame-char-width frame))
                                           (< (abs (- (nth 3 body) 300)) (frame-char-height frame))
                                           (car red))
                              (format "at %s, body %sx%s, %.2f red" pos (nth 2 body) (nth 3 body) (cadr red)))))

(defun ewm-session-tests-fullscreen ()
  "An app goes fullscreen over its output and back."
  (let* ((window (ewm-session-tests--foot-window))
         (output (ewm--frame-output (window-frame window)))
         (before (car (ewm-session-tests--foot-covered nil)))
         covered after)
    (with-selected-window window (ewm-toggle-fullscreen))
    (setq covered (ewm-session-tests--foot-covered t))
    (ewm-session-tests--shot "fullscreen" output)
    (with-selected-window window (ewm-toggle-fullscreen))
    (setq after (car (ewm-session-tests--foot-covered nil)))
    (ewm-session-tests--check "fullscreen" (and before (car covered) after)
                              (format "%.2f red over the output" (cadr covered)))))

(defun ewm-session-tests-views ()
  "One app surface shows in two windows at once, one of them scaled."
  (let* ((window (ewm-session-tests--foot-window))
         (output (ewm--frame-output (window-frame window)))
         (buffer (window-buffer window))
         ;; Unequal halves, the larger lower window becomes the primary.
         (lower (with-selected-window window (split-window-below 10)))
         (upper-rect (ewm-session-tests--body-rect window))
         (lower-rect (ewm-session-tests--body-rect lower))
         (one `((,upper-rect ewm-session-tests--red-p > 0.95) (,lower-rect ewm-session-tests--red-p < 0.05)))
         (two `((,upper-rect ewm-session-tests--red-p > 0.95) (,lower-rect ewm-session-tests--red-p > 0.95)))
         split both scaled again)
    (set-window-buffer lower ewm-session-tests--buffer)
    (setq split (car (ewm-session-tests--wait-pixels output one)))
    (set-window-buffer lower buffer)
    (setq both (ewm-session-tests--wait-pixels output two))
    (ewm-session-tests--shot "two-views" output)
    ;; A window narrower than the primary shows the view at a smaller scale.
    (let* ((right (with-selected-window window (split-window-right)))
           (left-rect (ewm-session-tests--body-rect window))
           (right-rect (ewm-session-tests--body-rect right)))
      (set-window-buffer right ewm-session-tests--buffer)
      (setq scaled (ewm-session-tests--wait-pixels
                    output `((,left-rect ewm-session-tests--red-p > 0.95)
                             (,right-rect ewm-session-tests--red-p < 0.05)
                             (,lower-rect ewm-session-tests--red-p > 0.95))))
      (ewm-session-tests--shot "scaled-view" output)
      (delete-window right))
    (set-window-buffer lower ewm-session-tests--buffer)
    (setq again (car (ewm-session-tests--wait-pixels output one)))
    (delete-window lower)
    (ewm-session-tests--close-foot)
    (ewm-session-tests--check "views" (and split (car both) (car scaled) again)
                              (format "%.2f and %.2f red, %.2f scaled"
                                      (nth 1 both) (nth 2 both) (nth 1 scaled)))))

(defun ewm-session-tests-move ()
  "The frame moves to the next output and back."
  (if (< (length ewm--output-info) 2)
      "SKIP move — one output"
    (let* ((frame ewm-session-tests--frame)
           (home (ewm-session-tests--focus))
           (start (ewm-session-tests--monitor frame))
           there back)
      (ewm-frame-move-to-next-output)
      (setq there (ewm-session-tests--wait
                   (lambda () (and (not (equal (ewm--frame-output frame) home))
                                   (not (equal (ewm-session-tests--monitor frame) start))))))
      (ewm-session-tests--shot "moved" (ewm--frame-output frame))
      (ewm-frame-move-to-next-output)
      (setq back (ewm-session-tests--wait
                  (lambda () (and (equal (ewm--frame-output frame) home)
                                  (equal (ewm-session-tests--monitor frame) start)))))
      (ewm-session-tests--check "move" (and there back) (format "away from %s and back" home)))))

(defun ewm-session-tests-reconfigure ()
  "A head switched off hands its frame over and gets one back when on."
  (if (< (length ewm--output-info) 2)
      "SKIP reconfigure — one output"
    (let* ((home (ewm-session-tests--focus))
           (other (car (remove home (mapcar #'car ewm--output-info))))
           (orphan (ewm-output-active-frame other))
           off on)
      (ewm-configure-output other :enabled nil)
      (setq off (ewm-session-tests--wait
                 (lambda () (and (not (member other (ewm-get-active-outputs)))
                                 (or (not (frame-live-p orphan)) (equal (ewm--frame-output orphan) home))))))
      (ewm-configure-output other :enabled t)
      (setq on (ewm-session-tests--wait
                (lambda () (and (member other (ewm-get-active-outputs))
                                (frame-live-p (ewm-output-active-frame other))))
                60))
      (ewm-session-tests--check "reconfigure" (and off on)
                                (format "%s off, its frame %s, and on with a frame" other
                                        (if (frame-live-p orphan) "moved" "closed"))))))

(defun ewm-session-tests-rescale ()
  "The frame follows its output to another scale and back."
  (let* ((frame ewm-session-tests--frame)
         (output (ewm-session-tests--focus))
         (scale (ewm-session-tests--scale output))
         (other (if (= scale 1.0) 2.0 1.0))
         (start (ewm-session-tests--monitor frame))
         changed restored)
    (ewm-configure-output output :scale other)
    (setq changed (ewm-session-tests--wait
                   (lambda () (and (= (ewm-session-tests--scale output) other)
                                   (not (equal (ewm-session-tests--monitor frame) start))))))
    (ewm-session-tests--shot "rescaled" output)
    (ewm-configure-output output :scale scale)
    (setq restored (ewm-session-tests--wait
                    (lambda () (and (= (ewm-session-tests--scale output) scale)
                                    (equal (ewm-session-tests--monitor frame) start)))))
    (ewm-session-tests--check "rescale" (and changed restored)
                              (format "%s at %s and back at %s" output other scale))))

(defun ewm-session-tests-lock ()
  "swaylock covers every output."
  (ewm-session-tests--focus)
  (setq ewm-session-tests--locker
        (make-process :name "ewm-session-tests-swaylock" :noquery t
                      :command '("swaylock" "-c" "333333")))
  (let ((covered (mapcar (lambda (info)
                           (cons (car info)
                                 (ewm-session-tests--wait-pixels (car info) '((nil ewm-session-tests--gray-p > 0.99)))))
                         ewm--output-info)))
    (ewm-session-tests--check "lock" (cl-every #'cadr covered)
                              (mapconcat (pcase-lambda (`(,output ,_ ,fraction))
                                           (format "%.2f gray on %s" fraction output))
                                         covered ", "))))

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
             '(ewm-session-tests-outputs ewm-session-tests-setup ewm-session-tests-fullscreen
               ewm-session-tests-views ewm-session-tests-child-frame ewm-session-tests-float
               ewm-session-tests-move ewm-session-tests-reconfigure ewm-session-tests-rescale
               ewm-session-tests-teardown)
             "\n"))

(provide 'ewm-session-tests)
;;; ewm-session-tests.el ends here
