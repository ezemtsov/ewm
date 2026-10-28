;;; ewm-focus-tests.el --- ERT tests for EWM focus contracts -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins the focus contract between the lisp layer and the compositor:
;; whenever Emacs is reading keys for itself, every frame's
;; `:selected-entry-id' shipped to the compositor must be nil so keyboard
;; focus lands on the Emacs frame chrome rather than an embedded
;; Wayland surface.  See `docs/focus-design.md' for the full design.
;;
;; Run:
;;   emacs -Q -batch -L lisp -l tests/ewm-focus-tests.el \
;;         -f ert-run-tests-batch-and-exit

;;; Code:

(require 'ert)
(require 'cl-lib)

(load (expand-file-name "ewm-test-stubs"
                        (file-name-directory (or load-file-name buffer-file-name)))
      nil t)

(require 'ewm-layout)
(require 'ewm-focus)
(require 'ewm-surface)
(require 'ewm)

;;;; Test harness

(defconst ewm-focus-tests--output "TEST-OUT")
(defconst ewm-focus-tests--frame-surface-id 9999)
(defconst ewm-focus-tests--surface-id 5)
(defconst ewm-focus-tests--surface-entry-id
  (+ 1000 ewm-focus-tests--surface-id))

(defvar ewm-focus-tests--captured nil)

(defmacro ewm-focus-tests--with-captured-layouts (&rest body)
  "Run BODY with `ewm-layout' rebound to record into a list.
Returns the captured FRAMES vectors, oldest first."
  (declare (indent 0))
  `(let ((ewm-focus-tests--captured nil))
     (cl-letf (((symbol-function 'ewm-layout)
                (lambda (frames) (push frames ewm-focus-tests--captured))))
       ,@body)
     (nreverse ewm-focus-tests--captured)))

(defvar ewm-focus-tests--scratch-buffers nil)

(defun ewm-focus-tests--make-surface-buffer (surface-id)
  "Create a tracked buffer tagged with SURFACE-ID."
  (let ((buf (generate-new-buffer (format " *ewm-test-surface-%d*" surface-id))))
    (push buf ewm-focus-tests--scratch-buffers)
    (with-current-buffer buf (setq-local ewm-surface-id surface-id))
    buf))

(defun ewm-focus-tests--install-surface-window (window surface-id)
  "Display a fresh surface buffer in WINDOW with matching entry parameters."
  (let ((buf (ewm-focus-tests--make-surface-buffer surface-id)))
    (set-window-buffer window buf)
    (set-window-parameter window 'ewm-entry-id (+ 1000 surface-id))))

(defmacro ewm-focus-tests--with-frame (var &rest body)
  "Bind VAR to the batch frame, EWM-configured for the duration of BODY.
The batch frame is shared; we save and restore the parameters we touch."
  (declare (indent 1))
  `(let* ((,var (selected-frame))
          (saved-surface-id (frame-parameter ,var 'ewm-surface-id))
          (saved-output (frame-parameter ,var 'ewm-output))
          (saved-focus (frame-parameter ,var 'ewm-focus-on-create))
          (saved-window (frame-selected-window ,var))
          (saved-buffer (window-buffer saved-window))
          (saved-entry-id (window-parameter saved-window 'ewm-entry-id))
          (ewm-focus-tests--scratch-buffers nil))
     (set-frame-parameter ,var 'ewm-surface-id ewm-focus-tests--frame-surface-id)
     (set-frame-parameter ,var 'ewm-output ewm-focus-tests--output)
     (unwind-protect
         (cl-letf (((symbol-function 'ewm--window-body-pixel-rect)
                    (lambda (&optional _window) '(0 0 100 100))))
           ,@body)
       (set-frame-parameter ,var 'ewm-surface-id saved-surface-id)
       (set-frame-parameter ,var 'ewm-output saved-output)
       (set-frame-parameter ,var 'ewm-focus-on-create saved-focus)
       (when (buffer-live-p saved-buffer)
         (set-window-buffer saved-window saved-buffer))
       (set-window-parameter saved-window 'ewm-entry-id saved-entry-id)
       (set-window-parameter saved-window 'quit-restore nil)
       (dolist (b ewm-focus-tests--scratch-buffers)
         (when (buffer-live-p b) (kill-buffer b))))))

(defun ewm-focus-tests--capture-selected-entry-id ()
  "Return the `:selected-entry-id' the batch frame's layout carries."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (let* ((ewm--module-mode t)
           (captures (ewm-focus-tests--with-captured-layouts
                       (ewm-layout--send-layouts)))
           (frame (aref (car captures) 0)))
      (plist-get frame :selected-entry-id))))

;;;; Tests

(ert-deftest ewm-focus/a-modal-read-hides-the-selected-entry ()
  "Any read that keeps keys for Emacs ships a nil `:selected-entry-id'."
  (should (eql ewm-focus-tests--surface-entry-id
               (ewm-focus-tests--capture-selected-entry-id)))
  ;; `-batch' can't drive a real read, so stub the window and the table.
  (cl-letf (((symbol-function 'active-minibuffer-window)
             (lambda () (selected-window))))
    (let ((minibuffer-completion-table '("a" "b")))
      (should-not (ewm-focus-tests--capture-selected-entry-id)))
    (let ((minibuffer-completion-table nil))
      (should (eql ewm-focus-tests--surface-entry-id
                   (ewm-focus-tests--capture-selected-entry-id)))))
  (let ((overriding-terminal-local-map (make-sparse-keymap)))
    (should-not (ewm-focus-tests--capture-selected-entry-id)))
  ;; A popup menu suppresses only while a command shows it.
  (cl-letf (((symbol-function 'menu-or-popup-active-p) (lambda () t)))
    (let ((this-command 'menu-bar-open))
      (should-not (ewm-focus-tests--capture-selected-entry-id)))
    (let ((this-command nil))
      (should (eql ewm-focus-tests--surface-entry-id
                   (ewm-focus-tests--capture-selected-entry-id)))))
  (let ((ewm-layout-menu-predicates (list (lambda () t))))
    (should-not (ewm-focus-tests--capture-selected-entry-id))))

(ert-deftest ewm-focus/suppression-touches-only-the-selected-entry ()
  "A selected entry is one of `:entries', and a menu zeroes it without touching them."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (let* ((plist (ewm-layout--build-frame-plist f nil))
           (entries (plist-get plist :entries)))
      (should (= 1 (length entries)))
      (should (eql ewm-focus-tests--surface-id (plist-get (aref entries 0) :surface-id)))
      (should (eql (plist-get plist :selected-entry-id)
                   (plist-get (aref entries 0) :entry-id))))
    (let ((plist (ewm-layout--build-frame-plist f t)))
      (should-not (plist-get plist :selected-entry-id))
      (should (= 1 (length (plist-get plist :entries)))))))

(ert-deftest ewm-focus/menu-transitions-refresh-once-and-run-the-hook ()
  "Entering or leaving a menu refreshes once, leaving runs the deactivation hook."
  (let* ((refreshes 0)
         (deactivations 0)
         (predicates ewm-layout-menu-predicates)
         (ewm-layout--last-menu-active nil)
         (ewm-layout-menu-predicates (list #'ignore))
         (ewm-layout-menu-deactivated-hook (list (lambda () (cl-incf deactivations)))))
    (cl-letf (((symbol-function 'ewm-layout--refresh)
               (lambda () (cl-incf refreshes))))
      (ewm-layout--on-post-command)
      (should (= 0 refreshes))
      (let ((ewm-layout-menu-predicates (list (lambda () t))))
        (ewm-layout--on-post-command)
        (ewm-layout--on-post-command)
        (should (= 1 refreshes)))
      (ewm-layout--on-post-command)
      (should (= 2 refreshes))
      (should (= 1 deactivations))
      ;; A popup that opens and closes within a command still deactivates.
      (cl-letf (((symbol-function 'menu-or-popup-active-p) (lambda () t)))
        (let ((this-command 'menu-bar-open)
              (ewm-layout-menu-predicates predicates))
          (should (ewm-layout--menu-active-p))))
      (ewm-layout--on-post-command)
      (should (= 2 deactivations)))))

(ert-deftest ewm-focus/frame-plists-speak-logical-pixels ()
  "The frame scale is the monitor's over the frame's, and rectangles are divided by it."
  (cl-flet ((scale (monitor frame)
              (cl-letf (((symbol-function 'frame-monitor-attributes)
                         (lambda (&optional _frame)
                           (and monitor `((scale-factor . ,monitor)))))
                        ((symbol-function 'frame-scale-factor)
                         (lambda (&optional _frame) frame)))
                (ewm--frame-scale nil))))
    ;; Device pixels on a 1.5 monitor, logical pixels on a GDK-scaled one, no monitor.
    (should (= (scale 1.5 1.0) 1.5))
    (should (= (scale 2.0 2.0) 1.0))
    (should (= (scale nil 1.0) 1.0)))
  (ewm-focus-tests--with-frame f
    (cl-letf (((symbol-function 'ewm--frame-scale) (lambda (&rest _) 1.5))
              ((symbol-function 'ewm--window-body-pixel-rect)
               (lambda (&optional _window) '(12 41 456 815))))
      (let* ((plist (ewm-layout--build-frame-plist f nil))
             (entry (aref (plist-get plist :entries) 0)))
        (should (equal (list (plist-get entry :x) (plist-get entry :y)
                             (plist-get entry :w) (plist-get entry :h))
                       '(8 27 304 543)))
        (should (= (plist-get plist :width) (/ (frame-pixel-width f) 1.5)))
        (should (= (plist-get plist :height) (/ (frame-pixel-height f) 1.5)))))))

(ert-deftest ewm-focus/plain-emacs-windows-are-layout-entries ()
  "Plain Emacs windows publish entries without `:surface-id'."
  (ewm-focus-tests--with-frame f
    (save-window-excursion
      (delete-other-windows)
      (let ((left (frame-selected-window f))
            (left-buf (generate-new-buffer " *ewm-test-left*"))
            (right-buf (generate-new-buffer " *ewm-test-right*")))
        (push left-buf ewm-focus-tests--scratch-buffers)
        (push right-buf ewm-focus-tests--scratch-buffers)
        (set-window-buffer left left-buf)
        (let ((right (split-window-right)))
          (set-window-buffer right right-buf)
          (select-window right)
          (let* ((plist (ewm-layout--build-frame-plist f nil))
                 (sel (plist-get plist :selected-entry-id))
                 (entries (plist-get plist :entries))
                 (entry-ids (cl-loop for entry across entries
                                    collect (plist-get entry :entry-id)))
                 (entry-names (cl-loop for entry across entries
                                       collect (plist-get entry :name))))
            (should (= 2 (length entries)))
            (should sel)
            (should (member sel entry-ids))
            (should (member (buffer-name left-buf) entry-names))
            (should (member (buffer-name right-buf) entry-names))
            (cl-loop for entry across entries
                     do (should-not (plist-get entry :surface-id)))))))))

(ert-deftest ewm-focus/buffer-rotation-keeps-window-entry-id ()
  "Changing a window's surface attachment keeps the window's entry id."
  (ewm-focus-tests--with-frame f
    (let ((win (frame-selected-window f)))
      (set-window-buffer win (ewm-focus-tests--make-surface-buffer 7))
      (set-window-parameter win 'ewm-entry-id 1005)
      (let* ((plist (ewm-layout--build-frame-plist f nil))
             (sel (plist-get plist :selected-entry-id))
             (entries (plist-get plist :entries)))
        (should sel)
        (should (eql sel 1005))
        (should (= 1 (length entries)))
        (should (eql 7 (plist-get (aref entries 0) :surface-id)))
        (should (eql sel (plist-get (aref entries 0) :entry-id)))))))

(ert-deftest ewm-focus/mapped-surface-publishes-layout-immediately ()
  "A surface publishes its layout synchronously when it maps, not via deferred
window hooks -- otherwise it never renders while Emacs is blocked in a
subprocess wait (for example EPA waiting for pinentry).  `new' only creates the
buffer; `mapped' displays and publishes it."
  (ewm-focus-tests--with-frame f
    (let ((captures nil)
          (created nil)
          (ewm--surfaces (make-hash-table :test 'eql))
          (ewm--module-mode t))
      (unwind-protect
          (cl-letf (((symbol-function 'ewm-layout)
                     (lambda (frames) (push frames captures)))
                    ((symbol-function 'ewm-get-strip-frame-ids)
                     (lambda (_output) (list ewm-focus-tests--frame-surface-id))))
            (setq created (ewm--create-surface-buffer 80 "pinentry" 1234))
            (should (eq created (gethash 80 ewm--surfaces)))
            ;; New alone must not publish: placement waits for the map event.
            (should-not captures)
            (ewm--handle-mapped-surface
             `((id . 80) (open_floating . nil) (output . ,ewm-focus-tests--output)))
            (should captures)
            (let* ((frames (car captures))
                   (frame-plist (aref frames 0))
                   (entries (plist-get frame-plist :entries))
                   (entry (aref entries 0)))
              (should (= 1 (length entries)))
              (should (eql 80 (plist-get entry :surface-id)))
              (should (eql (plist-get entry :entry-id)
                           (plist-get frame-plist :selected-entry-id)))))
        (when (buffer-live-p created)
          (with-current-buffer created
            (remove-hook 'kill-buffer-query-functions
                         #'ewm--kill-buffer-query-function t))
          (kill-buffer created))))))

(ert-deftest ewm-focus/mapped-surface-waits-only-for-a-menu ()
  "A mapped surface is placed at once, or once the menu it arrived under closes."
  (let ((ewm--surfaces (make-hash-table :test 'eql))
        (ewm--pending-placements nil)
        (placed nil)
        (buf (generate-new-buffer " *ewm-test-map*")))
    (unwind-protect
        (cl-letf (((symbol-function 'ewm--place-surface-buffer)
                   (lambda (&rest args) (push args placed))))
          (puthash 42 buf ewm--surfaces)
          (cl-letf (((symbol-function 'ewm-layout--menu-active-p) #'ignore))
            (ewm--handle-mapped-surface
             '((id . 42) (open_floating . t) (output . "HEAD-A") (frame_surface_id . 7)
               (width . 480.0) (height . 160.0))))
          (should (null ewm--pending-placements))
          (should (equal (list buf t "HEAD-A" 7 nil) (car placed)))
          (should (equal '(480.0 . 160.0) (buffer-local-value 'ewm-surface-size buf)))
          (cl-letf (((symbol-function 'ewm-layout--menu-active-p) (lambda () t)))
            (ewm--handle-mapped-surface
             '((id . 42) (open_floating . nil) (output . "HEAD-A") (frame_surface_id . 7))))
          (should (= 1 (length placed)))
          (should (= 1 (length ewm--pending-placements)))
          (ewm--flush-pending-placements)
          (should (equal (list buf nil "HEAD-A" 7 nil) (car placed)))
          (should (= 2 (length placed)))
          (should (null ewm--pending-placements)))
      (kill-buffer buf))))

(ert-deftest ewm-focus/handle-focus-skips-while-modal ()
  "Incoming focus events must not change selection during modal input."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (let ((initially-selected (selected-window))
          (event `((focus_id . ,ewm-focus-tests--surface-entry-id))))
      (cl-letf (((symbol-function 'select-frame)
                 (lambda (&rest _) (error "select-frame called while modal")))
                ((symbol-function 'select-window)
                 (lambda (&rest _) (error "select-window called while modal"))))
        (let ((ewm-layout-menu-predicates (list (lambda () t))))
          (should-not (ewm--handle-focus event))
          (should (eq (selected-window) initially-selected)))))))

;;;; Focus changes are command boundaries
;;
;; telega restores point in `post-command-hook' once the buffer changes. A
;; selection made from the event filter must reach a boundary before the
;; first typed key, or that key lands behind the restored point.

(defmacro ewm-focus-tests--with-two-windows (&rest body)
  "Run BODY beside `surface-window', with `event' focusing it."
  (declare (indent 0))
  `(ewm-focus-tests--with-frame f
     (let ((surface-window (split-window (frame-selected-window f) nil t))
           (event `(focus (focus_id . ,ewm-focus-tests--surface-entry-id)))
           (unread-command-events nil)
           (this-command nil)
           (defining-kbd-macro nil)
           (post-command-hook nil))
       (ewm-focus-tests--install-surface-window
        surface-window ewm-focus-tests--surface-id)
       (unwind-protect (progn ,@body)
         (when (window-live-p surface-window) (delete-window surface-window))))))

(ert-deftest ewm-focus/selection-change-queues-focus-change ()
  "A drain that moves the selection queues one boundary behind the keys it unread."
  (ewm-focus-tests--with-two-windows
    (setq unread-command-events (list ?a))
    (ewm-test--drain event '(intercepted-command (key . "s-1")))
    (should (eq (selected-window) surface-window))
    (should (equal unread-command-events
                   (list ?a (cons t ?\s-1) (list 'ewm-focus-change (selected-frame)))))))

(ert-deftest ewm-focus/unchanged-selection-queues-nothing ()
  "A focus on the selected window is no boundary."
  (ewm-focus-tests--with-two-windows
    (select-window surface-window)
    (ewm-test--drain event)
    (should-not unread-command-events)
    (should-not post-command-hook)))

(ert-deftest ewm-focus/selection-change-during-command-waits-for-its-end ()
  "A running command never reads the boundary, it is queued once it ends."
  (ewm-focus-tests--with-two-windows
    (setq this-command 'some-command)
    (ewm-test--drain event)
    (should-not unread-command-events)
    (run-hooks 'post-command-hook)
    (should (equal unread-command-events
                   (list (list 'ewm-focus-change (selected-frame)))))
    (should-not post-command-hook)))

(ert-deftest ewm-focus/focus-change-is-held-back-like-a-frame-switch ()
  "The event waits for the key sequence in progress, as `switch-frame' does."
  (should (eq (get 'ewm-focus-change 'event-kind) 'switch-frame))
  (let* ((event (list 'ewm-focus-change (selected-frame)))
         (unread-command-events (list ?\C-x event ?b)))
    (should (equal (read-key-sequence-vector nil nil nil t) [?\C-x ?b]))
    (should (equal (read-key-sequence-vector nil nil nil t) (vector event)))))

(ert-deftest ewm-focus/selection-change-while-defining-macro-queues-nothing ()
  "Events unread from the filter are recorded, so a macro would keep the boundary."
  (ewm-focus-tests--with-two-windows
    (setq defining-kbd-macro t)
    (ewm-test--drain event)
    (should (eq (selected-window) surface-window))
    (should-not unread-command-events)
    (should-not post-command-hook)))

(ert-deftest ewm-focus/focus-change-keeps-last-command ()
  "The command after a focus change sees the one before it as `last-command'."
  (let ((last-command 'undo)
        (pre-command-hook nil))
    (ewm--focus-change)
    (setq last-command 'ewm--focus-change)
    (run-hooks 'pre-command-hook)
    (should (eq last-command 'undo))
    (should-not pre-command-hook)))

(ert-deftest ewm-focus/every-drain-moving-the-selection-is-a-boundary ()
  "Placement, activation, minimize, close and a failed handler end in the boundary a focus does."
  (ewm-focus-tests--with-two-windows
    (let ((ewm--surfaces (make-hash-table :test 'eql))
          (other (ewm-focus-tests--make-surface-buffer 42)))
      (puthash 42 other ewm--surfaces)
      (puthash ewm-focus-tests--surface-id (window-buffer surface-window) ewm--surfaces)
      (cl-letf (((symbol-function 'ewm--handle-title-update)
                 (lambda (_) (select-window surface-window) (error "boom"))))
        (pcase-dolist (`(,event . ,moves)
                       `(((mapped (id . 42) (output . ,ewm-focus-tests--output)
                                  (frame_surface_id . ,ewm-focus-tests--frame-surface-id)) . t)
                         ((minimize (id . 42)) . t)
                         ((activate_surface (id . 42)
                                            (frame_surface_id . ,ewm-focus-tests--frame-surface-id)) . t)
                         ((intercepted-command (key . "s-1")) . nil)
                         ((close (id . ,ewm-focus-tests--surface-id)) . nil)
                         ((close (id . 42)) . t)
                         ((title (id . 5) (app . "x") (title . "y")) . t)))
          (ert-info ((format "%S" event))
            (setq unread-command-events nil)
            (ewm-test--drain event)
            (should (eq (and (assq 'ewm-focus-change unread-command-events) t) moves))))))))

;;;; Handler-boilerplate contracts
;;
;; Compositor can queue events whose surface dies first; the macro must
;; absorb that race silently.

(ert-deftest ewm-focus/surface-match-is-a-display-buffer-condition ()
  "`ewm-surface-match' matches app and title regexps on surface buffers only."
  (ewm-focus-tests--with-frame f
    (let ((buf (ewm-focus-tests--make-surface-buffer 42)))
      (with-current-buffer buf
        (setq-local ewm-surface-app "firefox" ewm-surface-title "Picture-in-Picture"))
      (should (buffer-match-p (ewm-surface-match :app "^firefox$" :title "^Picture") buf))
      (should-not (buffer-match-p (ewm-surface-match :app "fire" :title "Gmail") buf))
      (should-not (buffer-match-p (ewm-surface-match :app "^chrome" :title "^Picture") buf))
      (should-not (buffer-match-p (ewm-surface-match :app "^FIREFOX$") buf))
      (should-not (buffer-match-p (ewm-surface-match) (messages-buffer)))
      (let ((display-buffer-alist `((,(ewm-surface-match :app "fire") nil (foo . t)))))
        (should (equal '(nil (foo . t))
                       (display-buffer-assq-regexp (buffer-name buf)
                                                   display-buffer-alist nil)))))))

(ert-deftest ewm-focus/output-active-frame-must-be-in-strip ()
  "`ewm-output-active-frame' returns the shown strip frame, else the first."
  (ewm-focus-tests--with-frame f
    (cl-letf (((symbol-function 'ewm-get-active-frame-id)
               (lambda (_output) ewm-focus-tests--frame-surface-id))
              ((symbol-function 'ewm-get-strip-frame-ids)
               (lambda (output)
                 (pcase output
                   ("A" (list 7 ewm-focus-tests--frame-surface-id))
                   ("B" '(7)))))
              ((symbol-function 'ewm--frame-by-surface-id)
               (lambda (id)
                 (cond ((eql id 7) 'g)
                       ((eql id ewm-focus-tests--frame-surface-id) f)))))
      (should (eq f (ewm-output-active-frame "A")))
      ;; The named active frame is not on B's strip: fall back to its first.
      (should (eq 'g (ewm-output-active-frame "B")))
      (should-not (ewm-output-active-frame "C"))
      (should-not (ewm-output-active-frame nil)))))

(ert-deftest ewm-focus/a-float-is-popped-to-only-when-activated ()
  "A compositor float is placed from its host frame with the floating action,
popped to when the compositor activates it and merely displayed otherwise."
  (ewm-focus-tests--with-frame f
    (let ((buf (ewm-focus-tests--make-surface-buffer 42))
          (action '(ewm-display-buffer-floating (ewm-output . "HEAD-A")))
          popped displayed)
      (cl-letf (((symbol-function 'pop-to-buffer)
                 (lambda (buffer action &optional _norecord)
                   (setq popped (cons buffer action))))
                ((symbol-function 'display-buffer)
                 (lambda (buffer action &optional _frame)
                   (setq displayed (cons buffer action)))))
        (ewm--place-surface-buffer buf t "HEAD-A" ewm-focus-tests--frame-surface-id t)
        (should (equal (cons buf action) popped))
        (should-not displayed)
        (setq popped nil)
        (ewm--place-surface-buffer buf t "HEAD-A" ewm-focus-tests--frame-surface-id nil)
        (should (equal (cons buf action) displayed))
        (should-not popped)))))

(defmacro ewm-focus-tests--with-stubbed-float (vars &rest body)
  "Run BODY with float creation stubbed onto the batch frame.
VARS is (MADE [FITTED]): the (OUTPUT PARAMS POS) and (W . H) the stubs saw."
  (declare (indent 1))
  (pcase-let ((`(,made ,fitted) vars))
    `(let (,made ,@(and fitted (list fitted)) (ewm--module-mode nil))
       (cl-letf (((symbol-function 'ewm--create-floating-frame-for-output)
                  (lambda (output params pos)
                    (setq ,made (list output params pos))
                    (selected-frame)))
                 ((symbol-function 'ewm--fit-floating-frame)
                  ,(if fitted
                       `(lambda (_frame w h) (setq ,fitted (cons w h)))
                     '#'ignore))
                 ((symbol-function 'make-frame-visible) #'ignore))
         ,@body))))

(ert-deftest ewm-focus/floating-action-frame-params ()
  "Output and origin come from the selected frame, looks from rules, size from the surface."
  (ewm-focus-tests--with-frame f
    (let ((buf (ewm-focus-tests--make-surface-buffer 42))
          (display-buffer-alist
           '((" \\*ewm-test-surface-42\\*" nil
              (pop-up-frame-parameters . ((minibuffer . nil)))))))
      (ewm-focus-tests--with-stubbed-float (made fitted)
        (should (display-buffer buf '(ewm-display-buffer-floating
                                      (ewm-size . (480 . 160))
                                      (ewm-position . (10 . 20)))))
        (pcase-let ((`(,output ,params ,pos) made))
          (should (equal ewm-focus-tests--output output))
          (should (equal '(10 . 20) pos))
          (should (equal '(visibility . nil) (assq 'visibility params)))
          (should (equal '(minibuffer . nil) (assq 'minibuffer params)))
          (should (eql ewm-focus-tests--frame-surface-id
                       (alist-get 'ewm-floating-origin-frame-id params))))
        (should (equal '(480 . 160) fitted)))
      (with-current-buffer buf (setq-local ewm-surface-size '(300 . 200)))
      (ewm-focus-tests--with-stubbed-float (made fitted)
        (should (display-buffer buf '(ewm-display-buffer-floating
                                      (ewm-output . "HEAD-A"))))
        (should (equal "HEAD-A" (car made)))
        (should (equal '(300 . 200) fitted))))))

(ert-deftest ewm-focus/floating-action-declines-off-output-or-inhibited ()
  "Off any output, or while attach inhibits floating, the action returns nil."
  (ewm-focus-tests--with-frame f
    (set-frame-parameter f 'ewm-output nil)
    (let ((buf (ewm-focus-tests--make-surface-buffer 42)))
      (ewm-focus-tests--with-stubbed-float (made)
        (should (display-buffer buf '(ewm-display-buffer-floating)))
        (should-not made))))
  (let ((ewm--inhibit-floating t))
    (ewm-focus-tests--with-stubbed-float (made)
      (should-not (ewm-display-buffer-floating (current-buffer) nil))
      (should-not made))))

(ert-deftest ewm-focus/detach-floats-the-tile-over-itself ()
  "The float lands where the tile was, frame origin minus output origin,
whatever same-window rule would keep the buffer in its tile."
  (ewm-focus-tests--with-frame f
    (let ((buf (ewm-focus-tests--make-surface-buffer 42))
          (display-buffer-alist
           '((".*" (display-buffer-reuse-window display-buffer-same-window)))))
      (set-window-buffer (frame-selected-window f) buf)
      (cl-letf (((symbol-function 'ewm--get-frame-origin) (lambda (_) '(200 . 300)))
                ((symbol-function 'ewm--get-output-origin) (lambda (_) '(100 . 100))))
        (ewm-focus-tests--with-stubbed-float (made fitted)
          (ewm--floating-detach-selected-buffer f)
          (should (equal '(105 . 205) (nth 2 made)))
          (should (equal '(90 . 90) fitted)))))))

(ert-deftest ewm-focus/detach-keeps-tile-when-rule-shows-elsewhere ()
  "A rule that shows the buffer in a sibling window leaves the tile alone."
  (ewm-focus-tests--with-frame f
    (save-window-excursion
      (let ((buf (ewm-focus-tests--make-surface-buffer 42))
            (source (frame-selected-window f))
            (display-buffer-alist
             '((" \\*ewm-test-surface-42\\*" display-buffer-pop-up-window)))
            (split-height-threshold 0))
        (set-window-buffer source buf)
        (ewm-focus-tests--with-stubbed-float (made)
          (ewm--floating-detach-selected-buffer f)
          (should-not made)
          (should (window-live-p source))
          (should-not (eq source (selected-window)))
          (should (eq buf (window-buffer (selected-window)))))))))

(ert-deftest ewm-focus/set-input-focus-queues-for-in-flight-frame ()
  "Focusing a frame without a surface id queues via `ewm-focus-on-create'."
  (ewm-focus-tests--with-frame f
    (let ((ewm--module-mode t) sent)
      (cl-letf (((symbol-function 'ewm-focus-target-module)
                 (lambda (id) (push id sent))))
        (set-frame-parameter f 'ewm-surface-id nil)
        (ewm--select-frame-set-input-focus f)
        (should (null sent))
        (should (frame-parameter f 'ewm-focus-on-create))
        (set-frame-parameter f 'ewm-focus-on-create nil)
        (set-frame-parameter f 'ewm-surface-id ewm-focus-tests--frame-surface-id)
        (ewm--select-frame-set-input-focus f)
        (should (equal (list (ewm-layout--ensure-frame-focus-id f)) sent))
        (should-not (frame-parameter f 'ewm-focus-on-create))))))

(ert-deftest ewm-focus/a-launch-opens-its-surface-in-the-launch-window ()
  "A launch opens its surface in the window it ran from, by token or by pid."
  (pcase-dolist (`(,by . ,activate) '((token . nil) (token . t) (pid . nil)))
    (ewm-focus-tests--with-frame f
      (let* ((ewm--surfaces (make-hash-table :test 'eql))
             (ewm--module-mode t)
             (ewm-mode t)
             (origin (frame-selected-window f))
             (target (split-window origin))
             (ewm-launch-hint (list :frame-surface-id ewm-focus-tests--frame-surface-id))
             minted env process created)
        (unwind-protect
            (cl-letf (((symbol-function 'ewm-create-launch-token)
                       (lambda (hint) (push hint minted) "tok"))
                      ((symbol-function 'ewm-layout) #'ignore)
                      ((symbol-function 'ewm-get-strip-frame-ids)
                       (lambda (_output) (list ewm-focus-tests--frame-surface-id))))
              (select-window target)
              (setq process (ewm--inject-activation-token
                             (lambda (&rest args)
                               (setq env process-environment)
                               (apply #'make-process args))
                             :name "ewm-test" :command '("sleep" "30") :noquery t))
              (select-window origin)
              (should (equal minted `((:frame-surface-id ,ewm-focus-tests--frame-surface-id))))
              (should (member "XDG_ACTIVATION_TOKEN=tok" env))
              (setq created (ewm--create-surface-buffer
                             80 "app" (if (eq by 'pid) (process-id process) 1234)))
              (ewm--handle-mapped-surface
               `((id . 80) (output . ,ewm-focus-tests--output) (activate . ,activate)
                 (frame_surface_id . ,ewm-focus-tests--frame-surface-id)
                 ,@(and (eq by 'token) '((token . "tok")))))
              (should (eq (window-buffer target) created))
              (should (eq (selected-window) (if activate target origin))))
          (when process (delete-process process))
          (select-window origin)
          (delete-window target)
          (when (buffer-live-p created) (kill-buffer created)))))))

(ert-deftest ewm-focus/an-xdg-launch-hints-the-selected-frame ()
  "A bound `ewm-launch-hint' wins, else `anchor' hints the selected frame."
  (ewm-focus-tests--with-frame _f
    (let (hints)
      (cl-letf (((symbol-function 'ewm-list-xdg-apps) (lambda () '(("App" . "app %U"))))
                ((symbol-function 'start-process-shell-command)
                 (lambda (&rest _) (push ewm-launch-hint hints) nil)))
        (ewm-launch-xdg-command "App")
        (let ((ewm-launch-hint '(:quiet t)))
          (ewm-launch-xdg-command "App"))
        (let ((ewm-launch-placement 'focus))
          (ewm-launch-xdg-command "App"))
        (should (equal hints
                       (list nil '(:quiet t)
                             (list :frame-surface-id ewm-focus-tests--frame-surface-id))))))))

(ert-deftest ewm-focus/delete-frame-advice-resolves-the-frame-and-keeps-the-last ()
  "A nil FRAME resolves once before close handling, and the only frame on an
output is deleted only with FORCE."
  (let* ((frame (selected-frame))
         (prepared nil)
         (called nil)
         (old-output (frame-parameter frame 'ewm-output))
         (old-surface-id (frame-parameter frame 'ewm-surface-id)))
    (unwind-protect
        (cl-letf (((symbol-function 'ewm-get-strip-frame-ids)
                   (lambda (_output) '(42)))
                  ((symbol-function 'ewm-prepare-frame-close-module)
                   (lambda (id) (setq prepared id))))
          (set-frame-parameter frame 'ewm-output "TEST-OUT")
          (set-frame-parameter frame 'ewm-surface-id 42)
          (ewm--prevent-frame-deletion
           (lambda (arg force) (setq called (list arg force))) nil t)
          (should (equal called (list frame t)))
          (should (eql prepared 42))
          (setq called nil prepared nil)
          (ewm--prevent-frame-deletion
           (lambda (arg force) (setq called (list arg force))) frame nil)
          (should-not called)
          (should-not prepared))
      (set-frame-parameter frame 'ewm-output old-output)
      (set-frame-parameter frame 'ewm-surface-id old-surface-id))))

(ert-deftest ewm-focus/with-surface-buffer-runs-only-in-a-live-buffer ()
  "An unknown id or a killed buffer is a no-op, a live buffer is current for the body."
  (let ((ewm--surfaces (make-hash-table :test 'eql))
        (dead (generate-new-buffer " *ewm-test-dead*"))
        (live (generate-new-buffer " *ewm-test-live*"))
        ran)
    (unwind-protect
        (progn
          (ewm--with-surface-buffer-from-event '((id . 42)) (push (current-buffer) ran))
          (puthash 42 dead ewm--surfaces)
          (kill-buffer dead)
          (ewm--with-surface-buffer-from-event '((id . 42)) (push (current-buffer) ran))
          (should-not ran)
          (puthash 42 live ewm--surfaces)
          (ewm--with-surface-buffer-from-event '((id . 42)) (push (current-buffer) ran))
          (should (equal ran (list live))))
      (kill-buffer live))))

(ert-deftest ewm-focus/mff-warps-once-per-real-selection-change ()
  "The hook warps when the selection left its anchor, never when it only re-fires.
Process filters (telega, magit) jostle `selected-window' between commands."
  (let ((warps 0)
        (ewm-mouse-follows-focus nil)
        (ewm--mff-last-window nil))
    (cl-letf (((symbol-function 'ewm-input--warp-pointer-to-window)
               (lambda (_) (cl-incf warps))))
      (ewm-input--on-window-selection-change (selected-frame))
      (should (= 0 warps))
      (let ((ewm-mouse-follows-focus t))
        (ewm-input--on-window-selection-change (selected-frame))
        (should (= 1 warps))
        (should (eq ewm--mff-last-window (selected-window)))
        (ewm-input--on-window-selection-change (selected-frame))
        (ewm-input--on-window-selection-change (selected-frame))
        (should (= 1 warps))))))

(ert-deftest ewm-focus/mff-warp-lands-in-the-window-from-the-frame-origin ()
  "The warp goes to the window body offset by the compositor frame origin,
and is skipped for the minibuffer or a pointer already inside."
  (ewm-focus-tests--with-frame f
    (let (warped)
      (cl-letf (((symbol-function 'ewm--get-frame-origin) (lambda (_) '(200 . 300)))
                ((symbol-function 'ewm--get-output-origin) (lambda (_) '(0 . 0)))
                ((symbol-function 'ewm-get-pointer-location) (lambda () '(0.0 . 0.0)))
                ((symbol-function 'window-pixel-edges) (lambda (&optional _) '(0 0 100 100)))
                ((symbol-function 'ewm--window-body-pixel-rect) (lambda (_) '(10 20 100 100)))
                ((symbol-function 'ewm-warp-pointer) (lambda (x y) (push (cons x y) warped))))
        (cl-letf (((symbol-function 'ewm-input--pointer-in-window-p) (lambda (&rest _) nil)))
          (ewm-input--warp-pointer-to-window (minibuffer-window f)))
        (cl-letf (((symbol-function 'ewm-input--pointer-in-window-p) (lambda (&rest _) t)))
          (ewm-input--warp-pointer-to-window (frame-selected-window f)))
        (should-not warped)
        (ewm-input--warp-pointer-to-window (frame-selected-window f))
        (should (equal warped '((260.0 . 370.0))))))))

(ert-deftest ewm-focus/pointer-focus-anchors-mff-and-keyboard-focus-does-not ()
  "A pointer focus on the frame's own surface anchors MFF with hover FFM on or off,
a keyboard focus leaves the anchor stale so the hook warps."
  (ewm-focus-tests--with-frame f
    (let ((warps 0)
          (ewm-focus-follows-mouse t)
          (ewm-mouse-follows-focus t)
          (ewm--mff-last-window nil)
          (pointer-focus `((focus_id . ,(ewm-layout--ensure-frame-focus-id f))
                           (x . 0) (y . 0) (pointer . t))))
      (cl-letf (((symbol-function 'ewm-input--warp-pointer-to-window)
                 (lambda (_) (cl-incf warps))))
        ;; Frame edge: no Emacs window at this point.
        (cl-letf (((symbol-function 'window-at-x-y) (lambda (&rest _) nil)))
          (ewm--handle-focus pointer-focus)
          (ewm-input--on-window-selection-change f))
        (let ((ewm-focus-follows-mouse nil)
              (ewm--mff-last-window nil))
          (cl-letf (((symbol-function 'window-at-x-y)
                     (lambda (&rest _) (error "FFM window selection should be disabled"))))
            (ewm--handle-focus pointer-focus)
            (ewm-input--on-window-selection-change f)))
        (should (= 0 warps))
        (setq ewm--mff-last-window nil)
        (ewm--handle-focus `((focus_id . ,(ewm-layout--ensure-frame-focus-id f))))
        (ewm-input--on-window-selection-change f)
        (should (= 1 warps))))))

(provide 'ewm-focus-tests)
;;; ewm-focus-tests.el ends here
