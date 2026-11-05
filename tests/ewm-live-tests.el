;;; ewm-live-tests.el --- live checks of an EWM session  -*- lexical-binding: t; -*-

;; Checks against the EWM session they are loaded into, by hand or from
;; nix/vm-test.nix:
;;
;;   emacsclient -s SOCKET --eval \
;;     "(progn (load \"…/ewm-live-tests.el\") (ewm-live-tests-run))"
;;
;; Each check returns its "PASS <check> — <detail>" line or signals the
;; FAIL line, and emacsclient exits 1 on a signal.  The checks run in a
;; frame of their own: `ewm-live-tests-setup' opens it,
;; `ewm-live-tests-teardown' closes it.

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
(declare-function ewm--frame-position "ewm-frame" (frame))
(declare-function ewm-configure-output "ewm-frame" (name &rest args))
(declare-function ewm-get-active-outputs "ewm-core")
(declare-function ewm-get-strip-frame-ids "ewm-core" (name))
(declare-function ewm-launch-xdg-command "ewm-commands" (name))
(declare-function ewm-list-xdg-apps "ewm-core")
(defvar ewm--output-info)
(defvar ewm-launch-hint)

(defvar ewm-live-tests-dir nil
  "Directory for a screenshot per check, nil for none.")

(defvar ewm-live-tests--frame nil)
(defvar ewm-live-tests--frames nil)
(defvar ewm-live-tests--foot nil)
(defvar ewm-live-tests--locker nil)
(defvar ewm-live-tests--launched nil)
(defconst ewm-live-tests--buffer "*ewm-live-tests*")

;;;; report and waits

(defun ewm-live-tests--check (name ok detail)
  "The PASS line of check NAME with DETAIL when OK, else signal the FAIL line."
  (let ((line (format "%s %s — %s" (if ok "PASS" "FAIL") name detail)))
    (if ok line (error "%s" line))))

(defun ewm-live-tests--sit-for (seconds)
  "Dispatch events for SECONDS, `sit-for' reads nothing while input is pending."
  (let ((deadline (+ (float-time) seconds)))
    (redisplay)
    (while (< (float-time) deadline)
      (let ((unread-command-events nil))
        (read-event nil t (max 0.01 (- deadline (float-time))))))))

(defun ewm-live-tests--wait (pred &optional seconds)
  "Dispatch events until PRED returns non-nil or SECONDS pass, return its value."
  (let ((deadline (+ (float-time) (or seconds 30)))
        value)
    (while (and (not (setq value (funcall pred))) (< (float-time) deadline))
      (ewm-live-tests--sit-for 0.1))
    value))

;;;; pixels

(defun ewm-live-tests--grid (from count)
  "Every fourth coordinate from FROM over COUNT, and the last one."
  (let ((last (+ from count -1)))
    (delete-dups (append (number-sequence from last 4) (list last)))))

(defun ewm-live-tests--fractions (output checks)
  "The fraction of each (RECT COLOR . _) in CHECKS on one grim capture of OUTPUT.
RECT is (X Y WIDTH HEIGHT), nil for the whole output, bottom for its last eight
rows, edges for its outermost two rows and columns."
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
                (let ((hits 0)
                      (total 0))
                  (pcase-dolist (`(,x0 ,y0 ,w ,h)
                                 (pcase rect
                                   ('nil `((0 0 ,width ,height)))
                                   ('bottom `((0 ,(- height 8) ,width 8)))
                                   ('edges `((0 0 ,width 2) (0 ,(- height 2) ,width 2)
                                             (0 0 2 ,height) (,(- width 2) 0 2 ,height)))
                                   (_ (list rect))))
                    (dolist (y (ewm-live-tests--grid y0 h))
                      (dolist (x (ewm-live-tests--grid x0 w))
                        (let ((i (+ start (* 3 (+ (* y width) x)))))
                          (cl-incf total)
                          (when (funcall color (get-byte i) (get-byte (1+ i)) (get-byte (+ i 2)))
                            (cl-incf hits))))))
                  (/ (float hits) total)))
              checks))))

(defun ewm-live-tests--wait-pixels (output checks &optional seconds)
  "Wait until each (RECT COLOR OP LIMIT) in CHECKS holds on one capture of
OUTPUT, return (HELD FRACTION...)."
  (let (fractions)
    (cons (ewm-live-tests--wait
           (lambda ()
             (setq fractions (ewm-live-tests--fractions output checks))
             (cl-every (pcase-lambda (fraction `(,_ ,_ ,op ,limit)) (funcall op fraction limit))
                       fractions checks))
           seconds)
          fractions)))

(defun ewm-live-tests--red-p (r g b)
  (and (> r 200) (< g 60) (< b 60)))

(defun ewm-live-tests--gray-p (r g b)
  (and (< (abs (- r #x33)) 8) (< (abs (- g #x33)) 8) (< (abs (- b #x33)) 8)))

(defun ewm-live-tests--covered (color)
  "Checks for COLOR filling an output to its edges."
  `((nil ,color > 0.99) (edges ,color = 1.0)))

(defun ewm-live-tests--shot (name output)
  (when ewm-live-tests-dir
    (make-directory ewm-live-tests-dir t)
    (call-process "grim" nil nil nil "-t" "png" "-o" output
                  (expand-file-name (concat name ".png") ewm-live-tests-dir))))

;;;; the frame and its app

(defun ewm-live-tests--focus-frame (frame)
  "Give FRAME the keyboard and wait until the compositor agrees."
  (select-frame-set-input-focus frame)
  (unless (ewm-live-tests--wait (lambda () (eq (ewm--current-frame) frame)) 10)
    (error "FAIL focus — the frame did not take the keyboard")))

(defun ewm-live-tests--focus ()
  "Focus the frame of the checks on its buffer, return its output."
  (ewm-live-tests--focus-frame ewm-live-tests--frame)
  (with-selected-frame ewm-live-tests--frame
    (switch-to-buffer ewm-live-tests--buffer))
  (ewm--frame-output ewm-live-tests--frame))

(defun ewm-live-tests--scale (output)
  (alist-get 'scale (alist-get output ewm--output-info nil nil #'string=)))

(defun ewm-live-tests--device-rect (output rect)
  "RECT, logical pixels of OUTPUT, in its device pixels, two in from each edge."
  (pcase-let ((`(,x ,y ,w ,h) (mapcar (lambda (v) (round (* v (ewm-live-tests--scale output)))) rect)))
    (list (+ x 2) (+ y 2) (- w 4) (- h 4))))

(defun ewm-live-tests--body-at (output window pos)
  "The text area of WINDOW at POS, logical (X . Y) on OUTPUT, in its device pixels."
  (pcase-let ((`(,x ,y ,w ,h) (ewm--window-body-logical-rect window)))
    (ewm-live-tests--device-rect output (list (+ (car pos) x) (+ (cdr pos) y) w h))))

(defun ewm-live-tests--body-rect (window)
  "The text area of WINDOW in device pixels of its output, (X Y WIDTH HEIGHT)."
  (ewm-live-tests--body-at (ewm--frame-output (window-frame window)) window '(0 . 0)))

(defun ewm-live-tests--monitor (frame)
  "Geometry and scale of the monitor under FRAME, pgtk names every head alike."
  (let ((m (frame-monitor-attributes frame)))
    (list (alist-get 'geometry m) (alist-get 'scale-factor m))))

(defun ewm-live-tests--red-buffer (name)
  "The buffer NAME, drawing nothing but red."
  (with-current-buffer (get-buffer-create name)
    (setq-local cursor-type nil)
    (face-remap-add-relative 'default :background "red")
    (current-buffer)))

(defun ewm-live-tests--marker (text)
  (expand-file-name text temporary-file-directory))

(defun ewm-live-tests--foot-buffer ()
  (and ewm-live-tests--foot
       (let ((pid (process-id ewm-live-tests--foot)))
         (seq-find (lambda (b) (eq (buffer-local-value 'ewm-surface-pid b) pid)) (buffer-list)))))

(defun ewm-live-tests--foot-window ()
  "Select and return the window of the red foot, started on the frame if needed.
It echoes nothing, hides its cursor and paints four cells white near its top right."
  (unless (ewm-live-tests--foot-buffer)
    (ewm-live-tests--focus)
    (setq ewm-live-tests--foot
          (make-process :name "ewm-live-tests-foot" :noquery t
                        :command (list "foot" "-o" "colors-dark.background=ff0000"
                                       "-o" "colors-light.background=ff0000"
                                       "sh" "-c" "stty -echo; printf '\\033[?25l\\033[2;9999H\\033[7D\\033[48;2;255;255;255m    \\033[0m'; while read line; do touch \"$0/$line\"; done"
                                       temporary-file-directory))))
  (let ((window (ewm-live-tests--wait
                 (lambda ()
                   (let ((buffer (ewm-live-tests--foot-buffer)))
                     (and buffer (get-buffer-window buffer t))))
                 60)))
    (unless window
      (error "FAIL foot — no window within 60 s"))
    (ewm-live-tests--focus-frame (window-frame window))
    (select-window window)))

(defun ewm-live-tests--close-foot ()
  (when ewm-live-tests--foot
    (signal-process ewm-live-tests--foot 'TERM)
    (ewm-live-tests--wait (lambda () (not (ewm-live-tests--foot-buffer))))
    (setq ewm-live-tests--foot nil)))

;;;; checks

(defun ewm-live-tests-setup ()
  "Open the frame the checks run in and focus it."
  (unless (frame-live-p ewm-live-tests--frame)
    (setq ewm-live-tests--frame (ewm-frame-new)))
  (ewm-live-tests--check "setup" t (format "frame on %s" (ewm-live-tests--focus))))

(defun ewm-live-tests-outputs (&optional scales)
  "Every output shows a frame, at its scale in SCALES ((NAME . SCALE)...)."
  (let ((ok t) seen)
    (dolist (info ewm--output-info)
      (let ((name (car info))
            (scale (alist-get 'scale (cdr info))))
        (unless (and (ewm-live-tests--wait (lambda () (ewm-output-active-frame name)) 60)
                     (= (alist-get name scales scale nil #'equal) scale))
          (setq ok nil))
        (push (format "%s at %s" name scale) seen)))
    (ewm-live-tests--check "outputs" ok (string-join (nreverse seen) ", "))))

(defun ewm-live-tests-typed (text)
  "TEXT typed on the keyboard has reached the frame."
  (let ((ok (with-current-buffer ewm-live-tests--buffer (string-search text (buffer-string)))))
    (when ok
      (ewm-live-tests--shot "typed" (ewm--frame-output ewm-live-tests--frame)))
    (ewm-live-tests--check "typed" ok text)))

(defun ewm-live-tests-foot ()
  "Focus the app, the keyboard goes to it from here."
  (let ((window (ewm-live-tests--foot-window)))
    (setq ewm-live-tests--frames (frame-list))
    (ewm-live-tests--check "foot" t (format "focused on %s" (ewm--frame-output (window-frame window))))))

(defun ewm-live-tests-foot-typed (text)
  "TEXT typed on the keyboard reached the app."
  (let ((ok (ewm-live-tests--wait (lambda () (file-exists-p (ewm-live-tests--marker text))))))
    (ewm-live-tests--check "foot typed" ok text)))

(defun ewm-live-tests--foot-covered (on)
  "Wait for the app to cover its output when ON, else to sit tiled."
  (let ((output (ewm--frame-output (window-frame (get-buffer-window (ewm-live-tests--foot-buffer) t)))))
    (ewm-live-tests--wait-pixels output (if on
                                               (ewm-live-tests--covered 'ewm-live-tests--red-p)
                                             '((bottom ewm-live-tests--red-p < 0.01))))))

(defun ewm-live-tests-foot-fullscreen (on)
  "The app covers its output when ON, else its frame shows below it."
  (let ((held (ewm-live-tests--foot-covered on)))
    (ewm-live-tests--check (if on "key fullscreen" "key tiled") (car held)
                              (format "%.2f red over the output" (cadr held)))))

(defun ewm-live-tests--launch (hint &optional then)
  "Launch foot under HINT, run THEN, return its surface buffer once placed."
  (let ((known (buffer-list))
        (name (car (rassoc "foot" (ewm-list-xdg-apps))))
        (ewm-launch-hint hint))
    (unless name (error "FAIL launch — foot is not an XDG app here"))
    (push (ewm-launch-xdg-command name) ewm-live-tests--launched)
    (when then (funcall then))
    (let ((buffer (or (ewm-live-tests--wait
                       (lambda ()
                         (seq-find (lambda (buffer)
                                     (and (not (memq buffer known))
                                          (equal (buffer-local-value 'ewm-surface-app buffer) "foot")
                                          (get-buffer-window buffer t)))
                                   (buffer-list))))
                      (error "FAIL launch — no foot surface was placed"))))
      ;; A wrong selection or grant would arrive with the next layout, give it a moment.
      (ewm-live-tests--sit-for 1)
      buffer)))

(defun ewm-live-tests--close-launched ()
  (dolist (process ewm-live-tests--launched)
    (when (process-live-p process) (delete-process process)))
  (setq ewm-live-tests--launched nil))

(defun ewm-live-tests-launch-anchor ()
  "An app launched from a window opens there, the keyboard stays put."
  (ewm-live-tests--focus)
  (let* ((window (frame-selected-window ewm-live-tests--frame))
         (other (split-window window))
         (buffer (ewm-live-tests--launch nil (lambda () (select-window other))))
         (there (eq (window-buffer window) buffer))
         (stayed (eq (selected-window) other)))
    (ewm-live-tests--shot "launch-anchor" (ewm--frame-output ewm-live-tests--frame))
    (ewm-live-tests--close-launched)
    (delete-window other)
    (ewm-live-tests--check "launch anchor" (and there stayed)
                              (format "%s in the launch window %s, keyboard stayed in the other window %s"
                                      (buffer-name buffer) there stayed))))

(defun ewm-live-tests-launch-away ()
  "An app launched for an unfocused frame opens there, the keyboard stays put."
  (if (< (length ewm--output-info) 2)
      "SKIP launch away — one output"
    (let* ((home (ewm-live-tests--focus))
           (other (car (seq-find (lambda (info) (not (equal (car info) home))) ewm--output-info)))
           (away (ewm-output-active-frame other))
           (window (frame-selected-window ewm-live-tests--frame)))
      (ewm-live-tests--focus-frame away)
      (let* ((before (list (selected-frame) (selected-window)))
             (buffer (ewm-live-tests--launch
                      (list :frame-surface-id (frame-parameter ewm-live-tests--frame 'ewm-surface-id)))))
        (let ((there (eq (window-buffer window) buffer))
              (stayed (and (equal (list (selected-frame) (selected-window)) before)
                           (eq (ewm--current-frame) away))))
          (ewm-live-tests--shot "launch-away" home)
          (ewm-live-tests--close-launched)
          (ewm-live-tests--check "launch away" (and there stayed)
                                    (format "%s in the hinted window %s, keyboard stayed %s on %s"
                                            (buffer-name buffer) there stayed
                                            (ewm--frame-output (ewm--current-frame)))))))))

(defun ewm-live-tests-new-frame ()
  "A frame opened since `ewm-live-tests-foot', closed again here."
  (let* ((known ewm-live-tests--frames)
         (new (ewm-live-tests--wait (lambda () (seq-difference (frame-list) known)))))
    (mapc #'delete-frame new)
    (ewm-live-tests--check "bound key" new (format "%d new frame" (length new)))))

(defun ewm-live-tests-child-frame ()
  "A child frame shows where its parent put it."
  (let* ((output (ewm-live-tests--focus))
         (child (make-frame `((parent-frame . ,ewm-live-tests--frame) (left . 60) (top . 40)
                              (width . 20) (height . 4) (minibuffer . nil) (visibility . t))))
         ;; Where the parent was told to put it, its pixels to logical ones.
         (scale (ewm--frame-scale ewm-live-tests--frame))
         rect red)
    (set-window-buffer (frame-root-window child) (ewm-live-tests--red-buffer "*ewm-live-tests-child*"))
    (setq rect (ewm-live-tests--body-at output (frame-root-window child) (cons (/ 60 scale) (/ 40 scale))))
    (setq red (ewm-live-tests--wait-pixels output `((,rect ewm-live-tests--red-p > 0.99))))
    (ewm-live-tests--shot "child-frame" output)
    (delete-frame child)
    (ewm-live-tests--check "child frame" (car red) (format "%.2f red at %s" (cadr red) rect))))

(defun ewm-live-tests-float ()
  "A float opens with its body at the place and size asked for."
  (let* ((output (ewm-live-tests--focus))
         (buffer (ewm-live-tests--red-buffer "*ewm-live-tests-float*"))
         (window (progn (ewm--float-buffer buffer '(400 . 300) '(100 . 80))
                        (get-buffer-window buffer t)))
         (frame (window-frame window))
         (pos (progn
                ;; The frame reports its place and size before the placement lands.
                (ewm-live-tests--wait
                 (lambda () (and (equal (ewm--frame-position frame) '(100 . 80))
                                 (equal (cddr (ewm--window-body-logical-rect window)) '(400 300)))))
                (ewm--frame-position frame)))
         (body (ewm--window-body-logical-rect window))
         (rect (ewm-live-tests--body-at output window pos))
         (red (ewm-live-tests--wait-pixels output `((,rect ewm-live-tests--red-p > 0.99)))))
    (ewm-live-tests--shot "float" output)
    (delete-frame frame)
    (ewm-live-tests--check "float" (and (equal pos '(100 . 80)) (equal (cddr body) '(400 300)) (car red))
                              (format "at %s, body %sx%s, %.2f red" pos (nth 2 body) (nth 3 body) (cadr red)))))

(defun ewm-live-tests-frame-pairing ()
  "Frames pair with their surfaces in order, deleted, hidden and reshown ones too."
  (let* ((output (ewm-live-tests--focus))
         (gone (make-frame))
         (hidden (progn (delete-frame gone t) (make-frame '((visibility . nil)))))
         (a (make-frame))
         (b (make-frame))
         (id (lambda (frame) (frame-parameter frame 'ewm-surface-id)))
         (paired (lambda ()
                   (and (seq-every-p id (list hidden a b))
                        (member (funcall id b) (member (funcall id a) (ewm-get-strip-frame-ids output))))))
         (first (progn (make-frame-visible hidden) (ewm-live-tests--wait paired) (funcall id a)))
         ;; A hidden frame loses its toplevel and pairs again once shown.
         (reshown (progn (make-frame-invisible a)
                         (ewm-live-tests--wait (lambda () (null (funcall id a))))
                         (make-frame-visible a)
                         (ewm-live-tests--wait (lambda () (funcall id a)))))
         (kept (and (frame-live-p hidden) (frame-live-p b) (funcall id hidden) (funcall id b)))
         (detail (format "a %s then %s, hidden %s, b %s" first reshown
                         (and (frame-live-p hidden) (funcall id hidden))
                         (and (frame-live-p b) (funcall id b)))))
    (dolist (frame (list hidden a b)) (when (frame-live-p frame) (delete-frame frame t)))
    (ewm-live-tests--check "frame pairing" (and first reshown kept (not (eql first reshown))) detail)))

(defun ewm-live-tests-fullscreen ()
  "An app goes fullscreen over its output and back."
  (let* ((window (ewm-live-tests--foot-window))
         (output (ewm--frame-output (window-frame window)))
         (before (car (ewm-live-tests--foot-covered nil)))
         covered after)
    (with-selected-window window (ewm-toggle-fullscreen))
    (setq covered (ewm-live-tests--foot-covered t))
    (ewm-live-tests--shot "fullscreen" output)
    (with-selected-window window (ewm-toggle-fullscreen))
    (setq after (car (ewm-live-tests--foot-covered nil)))
    (ewm-live-tests--check "fullscreen" (and before (car covered) after)
                              (format "%.2f red over the output" (cadr covered)))))

(defun ewm-live-tests-views ()
  "One app surface shows in two windows at once, one of them scaled."
  (let* ((window (ewm-live-tests--foot-window))
         (output (ewm--frame-output (window-frame window)))
         (buffer (window-buffer window))
         ;; Unequal halves, the larger lower window becomes the primary.
         (lower (with-selected-window window (split-window-below 10)))
         (upper-rect (ewm-live-tests--body-rect window))
         (lower-rect (ewm-live-tests--body-rect lower))
         (one `((,upper-rect ewm-live-tests--red-p > 0.99) (,lower-rect ewm-live-tests--red-p < 0.01)))
         (two `((,upper-rect ewm-live-tests--red-p > 0.99) (,lower-rect ewm-live-tests--red-p > 0.99)))
         split both scaled again)
    (set-window-buffer lower ewm-live-tests--buffer)
    (setq split (car (ewm-live-tests--wait-pixels output one)))
    (set-window-buffer lower buffer)
    (setq both (ewm-live-tests--wait-pixels output two))
    (ewm-live-tests--shot "two-views" output)
    ;; A window narrower than the primary shows the view at a smaller scale, the white cells
    ;; near the right of the app land in its top right corner, a crop would cut them off.
    (let* ((right (with-selected-window window (split-window-right)))
           (left-rect (ewm-live-tests--body-rect window))
           (right-rect (ewm-live-tests--body-rect right))
           (corner (pcase-let ((`(,x ,y ,w ,_) left-rect)) (list (+ x w -64) y 64 64))))
      (set-window-buffer right ewm-live-tests--buffer)
      (setq scaled (ewm-live-tests--wait-pixels
                    output `((,left-rect ewm-live-tests--red-p > 0.99)
                             (,corner ewm-live-tests--red-p < 1.0)
                             (,right-rect ewm-live-tests--red-p < 0.01)
                             (,lower-rect ewm-live-tests--red-p > 0.99))))
      (ewm-live-tests--shot "scaled-view" output)
      (delete-window right))
    (set-window-buffer lower ewm-live-tests--buffer)
    (setq again (car (ewm-live-tests--wait-pixels output one)))
    (delete-window lower)
    (ewm-live-tests--close-foot)
    (ewm-live-tests--check "views" (and split (car both) (car scaled) again)
                              (format "%.2f and %.2f red, %.2f scaled"
                                      (nth 1 both) (nth 2 both) (nth 1 scaled)))))

(defun ewm-live-tests-move ()
  "The frame moves to the next output and back."
  (if (< (length ewm--output-info) 2)
      "SKIP move — one output"
    (let* ((frame ewm-live-tests--frame)
           (home (ewm-live-tests--focus))
           (start (ewm-live-tests--monitor frame))
           there back)
      (ewm-frame-move-to-next-output)
      (setq there (ewm-live-tests--wait
                   (lambda () (and (not (equal (ewm--frame-output frame) home))
                                   (not (equal (ewm-live-tests--monitor frame) start))))))
      (ewm-live-tests--shot "moved" (ewm--frame-output frame))
      (ewm-frame-move-to-next-output)
      (setq back (ewm-live-tests--wait
                  (lambda () (and (equal (ewm--frame-output frame) home)
                                  (equal (ewm-live-tests--monitor frame) start)))))
      (ewm-live-tests--check "move" (and there back) (format "away from %s and back" home)))))

(defun ewm-live-tests-reconfigure ()
  "A head switched off closes its empty frame, gets one back when on and takes no other."
  (if (< (length ewm--output-info) 2)
      "SKIP reconfigure — one output"
    (let* ((home (ewm-live-tests--focus))
           (other (car (remove home (mapcar #'car ewm--output-info))))
           (orphan (ewm-output-active-frame other))
           off on)
      (ewm-configure-output other :enabled nil)
      (setq off (ewm-live-tests--wait
                 (lambda () (and (not (member other (ewm-get-active-outputs)))
                                 (not (frame-live-p orphan))))))
      (ewm-configure-output other :enabled t)
      (setq on (ewm-live-tests--wait
                (lambda () (and (member other (ewm-get-active-outputs))
                                (frame-live-p (ewm-output-active-frame other))
                                (equal (ewm--frame-output ewm-live-tests--frame) home)))
                60))
      (ewm-live-tests--check "reconfigure" (and off on)
                                (format "%s off, its frame closed, on with a frame, %s kept its own"
                                        other home)))))

(defun ewm-live-tests-rescale ()
  "The frame follows its output to another scale and back."
  (let* ((frame ewm-live-tests--frame)
         (output (ewm-live-tests--focus))
         (scale (ewm-live-tests--scale output))
         (other (if (= scale 1.0) 2.0 1.0))
         (start (ewm-live-tests--monitor frame))
         changed restored)
    (ewm-configure-output output :scale other)
    (setq changed (ewm-live-tests--wait
                   (lambda () (and (= (ewm-live-tests--scale output) other)
                                   (not (equal (ewm-live-tests--monitor frame) start))))))
    (ewm-live-tests--shot "rescaled" output)
    (ewm-configure-output output :scale scale)
    (setq restored (ewm-live-tests--wait
                    (lambda () (and (= (ewm-live-tests--scale output) scale)
                                    (equal (ewm-live-tests--monitor frame) start)))))
    (ewm-live-tests--check "rescale" (and changed restored)
                              (format "%s at %s and back at %s" output other scale))))

(defun ewm-live-tests-lock ()
  "swaylock covers every output."
  (ewm-live-tests--focus)
  (setq ewm-live-tests--locker
        (make-process :name "ewm-live-tests-swaylock" :noquery t
                      :command '("swaylock" "-c" "333333")))
  (let ((covered (mapcar (lambda (info)
                           (cons (car info)
                                 (ewm-live-tests--wait-pixels
                                  (car info) (ewm-live-tests--covered 'ewm-live-tests--gray-p))))
                         ewm--output-info)))
    (ewm-live-tests--check "lock" (cl-every #'cadr covered)
                              (mapconcat (pcase-lambda (`(,output ,_ ,fraction))
                                           (format "%.2f gray on %s" fraction output))
                                         covered ", "))))

(defun ewm-live-tests-unlocked (text)
  "The locker is gone and TEXT, the password typed, never reached the frame."
  (let ((gone (ewm-live-tests--wait (lambda () (not (process-live-p ewm-live-tests--locker))) 60))
        (leaked (with-current-buffer ewm-live-tests--buffer (string-search text (buffer-string)))))
    (ewm-live-tests--check "unlocked" (and gone (not leaked))
                              (cond ((not gone) "locker still running")
                                    (leaked "the password reached the frame")
                                    (t "locker gone, nothing reached the frame")))))

(defun ewm-live-tests-teardown ()
  "Close the apps and the frame."
  (ewm-live-tests--close-foot)
  (ewm-live-tests--close-launched)
  (when (frame-live-p ewm-live-tests--frame)
    (ewm-live-tests--focus)
    (ewm-frame-close))
  (when (get-buffer ewm-live-tests--buffer)
    (kill-buffer ewm-live-tests--buffer))
  "PASS teardown — frame closed")

(defun ewm-live-tests-run ()
  "Run every check that needs no keyboard, return the report."
  (mapconcat (lambda (check)
               (condition-case err
                   (funcall check)
                 (error (error-message-string err))))
             '(ewm-live-tests-outputs ewm-live-tests-setup ewm-live-tests-fullscreen
               ewm-live-tests-views ewm-live-tests-child-frame ewm-live-tests-float
               ewm-live-tests-move ewm-live-tests-reconfigure ewm-live-tests-rescale
               ewm-live-tests-teardown)
             "\n"))

(provide 'ewm-live-tests)
;;; ewm-live-tests.el ends here
