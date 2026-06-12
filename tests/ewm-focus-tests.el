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

;; A stale .elc next to a newer .el would otherwise mask the source we're
;; meant to be testing.
(setq load-prefer-newer t)

;; The compositor dynamic module isn't loaded in the test harness; stub
;; the module entry points the libs would otherwise pull in.
(unless (fboundp 'ewm-output-layout)
  (defun ewm-output-layout (_output _frames) nil))
(unless (fboundp 'ewm-focus-target-module)
  (defun ewm-focus-target-module (_focus-id) nil))
(unless (fboundp 'ewm-prepare-frame-close-module)
  (defun ewm-prepare-frame-close-module (_surface-id) nil))
;; The dynamic module provides the `ewm-core' feature in production;
;; stub it here so `(require 'ewm)' resolves under batch.
(provide 'ewm-core)

(add-to-list 'load-path
             (expand-file-name "../lisp"
                               (file-name-directory (or load-file-name buffer-file-name))))
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
  "Run BODY with `ewm-output-layout' rebound to record into a list.
Returns the captures as an alist of (OUTPUT . FRAMES-VECTOR)."
  (declare (indent 0))
  `(let ((ewm-focus-tests--captured nil))
     (cl-letf (((symbol-function 'ewm-output-layout)
                (lambda (output frames)
                  (push (cons output frames) ewm-focus-tests--captured))))
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
          (saved-window (frame-selected-window ,var))
          (saved-buffer (window-buffer saved-window))
          (saved-entry-id (window-parameter saved-window 'ewm-entry-id))
          (ewm-focus-tests--scratch-buffers nil))
     (set-frame-parameter ,var 'ewm-surface-id ewm-focus-tests--frame-surface-id)
     (set-frame-parameter ,var 'ewm-output ewm-focus-tests--output)
     (unwind-protect
         (cl-letf (((symbol-function 'ewm--window-inside-absolute-pixel-edges)
                    (lambda (&optional _window) '(0 0 100 100))))
           ,@body)
       (set-frame-parameter ,var 'ewm-surface-id saved-surface-id)
       (set-frame-parameter ,var 'ewm-output saved-output)
       (when (buffer-live-p saved-buffer)
         (set-window-buffer saved-window saved-buffer))
       (set-window-parameter saved-window 'ewm-entry-id saved-entry-id)
       (dolist (b ewm-focus-tests--scratch-buffers)
         (when (buffer-live-p b) (kill-buffer b))))))

(defun ewm-focus-tests--single-frame-strip (frame)
  "Set `ewm--strips' to a single-output strip containing FRAME."
  (setq ewm--strips `((,ewm-focus-tests--output . (,frame))))
  (setq ewm--module-mode t))

(defun ewm-focus-tests--capture-selected-entry-id ()
  "Set up a single-frame strip and return the layout's `:selected-entry-id'."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (ewm-focus-tests--single-frame-strip f)
    (let* ((captures (ewm-focus-tests--with-captured-layouts
                       (ewm-layout--send-layouts)))
           (frame (aref (cdr (assoc ewm-focus-tests--output captures)) 0)))
      (plist-get frame :selected-entry-id))))

;;;; Tests

(ert-deftest ewm-focus/baseline-no-menu ()
  "No menu active: `:selected-entry-id' names the embedded surface."
  (should (eql ewm-focus-tests--surface-entry-id
               (ewm-focus-tests--capture-selected-entry-id))))

(ert-deftest ewm-focus/minibuffer-suppresses ()
  "A completion minibuffer suppresses embedded entry focus."
  ;; `-batch' can't drive a real read, so stub the window and the table.
  (cl-letf (((symbol-function 'active-minibuffer-window)
             (lambda () (selected-window))))
    (let ((minibuffer-completion-table '("a" "b")))
      (should (null (ewm-focus-tests--capture-selected-entry-id))))))

(ert-deftest ewm-focus/minibuffer-plain-prompt-does-not-suppress ()
  "A plain prompt keeps embedded entry focus."
  (cl-letf (((symbol-function 'active-minibuffer-window)
             (lambda () (selected-window))))
    (let ((minibuffer-completion-table nil))
      (should (eql ewm-focus-tests--surface-entry-id
                   (ewm-focus-tests--capture-selected-entry-id))))))

(ert-deftest ewm-focus/overriding-keymap-suppresses ()
  "Any `overriding-terminal-local-map' suppresses embedded entry focus."
  (let ((overriding-terminal-local-map (make-sparse-keymap)))
    (should (null (ewm-focus-tests--capture-selected-entry-id)))))

(ert-deftest ewm-focus/custom-predicate-suppresses ()
  "A user predicate added to `ewm-layout-menu-predicates' suppresses."
  (let ((ewm-layout-menu-predicates (list (lambda () t))))
    (should (null (ewm-focus-tests--capture-selected-entry-id)))))

(ert-deftest ewm-focus/menu-active-preserves-entries ()
  "Suppression touches `:selected-entry-id' only; `:entries' is unchanged."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (ewm-focus-tests--single-frame-strip f)
    (let* ((ewm-layout-menu-predicates (list (lambda () t)))
           (captures (ewm-focus-tests--with-captured-layouts
                       (ewm-layout--send-layouts)))
           (entries (plist-get (aref (cdr (assoc ewm-focus-tests--output captures)) 0)
                               :entries)))
      (should (= 1 (length entries)))
      (should (eql ewm-focus-tests--surface-id
                   (plist-get (aref entries 0) :surface-id))))))

(ert-deftest ewm-focus/build-frame-plist-honors-menu-flag ()
  "`build-frame-plist' zeros `:selected-entry-id' when MENU-ACTIVE is non-nil."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (should (eql ewm-focus-tests--surface-entry-id
                 (plist-get (ewm-layout--build-frame-plist f nil)
                            :selected-entry-id)))
    (should (null (plist-get (ewm-layout--build-frame-plist f t)
                             :selected-entry-id)))))

(ert-deftest ewm-focus/post-command-refreshes-on-transition ()
  "Modal-state transitions trigger exactly one refresh; staying does not."
  (let ((refresh-count 0)
        (ewm-layout--last-menu-active nil)
        (ewm-layout-menu-predicates (list #'ignore)))
    (cl-letf (((symbol-function 'ewm-layout--refresh)
               (lambda () (cl-incf refresh-count))))
      (ewm-layout--on-post-command)
      (should (= 0 refresh-count))
      (let ((ewm-layout-menu-predicates (list (lambda () t))))
        (ewm-layout--on-post-command)
        (should (= 1 refresh-count))
        (ewm-layout--on-post-command)
        (should (= 1 refresh-count)))
      (ewm-layout--on-post-command)
      (should (= 2 refresh-count)))))

(ert-deftest ewm-focus/selected-entry-id-always-in-entries ()
  "Lisp-side invariant 6: a non-nil `:selected-entry-id' is in `:entries'."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (let* ((plist (ewm-layout--build-frame-plist f nil))
           (sel (plist-get plist :selected-entry-id))
           (entry-ids (cl-loop for entry across (plist-get plist :entries)
                              collect (plist-get entry :entry-id))))
      (should sel)
      (should (member sel entry-ids)))))

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

(ert-deftest ewm-focus/new-surface-publishes-layout-immediately ()
  "New external surfaces must not wait for deferred window hooks to render."
  (ewm-focus-tests--with-frame f
    (let ((captures nil)
          (created nil)
          (ewm--surfaces (make-hash-table :test 'eql))
          (ewm--module-mode t)
          (ewm--strips `((,ewm-focus-tests--output . (,f)))))
      (unwind-protect
          (cl-letf (((symbol-function 'ewm-output-layout)
                     (lambda (output frames)
                       (push (cons output frames) captures))))
            (ewm--create-surface-buffer 80 "pinentry" ewm-focus-tests--output nil 1234)
            (setq created (gethash 80 ewm--surfaces))
            (should created)
            (should captures)
            (let* ((frames (cdr (car captures)))
                   (frame-plist (aref frames 0))
                   (entries (plist-get frame-plist :entries))
                   (entry (aref entries 0)))
              (should (equal (car (car captures)) ewm-focus-tests--output))
              (should (= 1 (length entries)))
              (should (eql 80 (plist-get entry :surface-id)))
              (should (eql (plist-get entry :entry-id)
                           (plist-get frame-plist :selected-entry-id)))))
        (when (buffer-live-p created)
          (with-current-buffer created
            (remove-hook 'kill-buffer-query-functions
                         #'ewm--kill-buffer-query-function t))
          (kill-buffer created))))))

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

;;;; Handler-boilerplate contracts
;;
;; Compositor can queue events whose surface dies first; the macro must
;; absorb that race silently.

(ert-deftest ewm-focus/focused-frame-prefers-compositor ()
  "`ewm--focused-frame' returns the compositor's frame when present."
  (let ((sentinel (cons 'fake 'frame)))
    (cl-letf (((symbol-function 'ewm--current-frame) (lambda () sentinel)))
      (should (eq sentinel (ewm--focused-frame))))))

(ert-deftest ewm-focus/focused-frame-falls-back-to-selected ()
  "When the compositor has no answer, fall back to `selected-frame'."
  (cl-letf (((symbol-function 'ewm--current-frame) #'ignore))
    (should (eq (selected-frame) (ewm--focused-frame)))))

(ert-deftest ewm-focus/delete-frame-advice-passes-captured-frame ()
  "Interactive nil FRAME must resolve once, before close handling can alter selection."
  (let* ((frame (selected-frame))
         (output "TEST-OUT")
         (prepared nil)
         (called-frame nil)
         (called-force nil)
         (old-output (frame-parameter frame 'ewm-output))
         (old-surface-id (frame-parameter frame 'ewm-surface-id))
         (ewm--strips `((,output . (,frame)))))
    (unwind-protect
        (progn
          (set-frame-parameter frame 'ewm-output output)
          (set-frame-parameter frame 'ewm-surface-id 42)
          (cl-letf (((symbol-function 'ewm-prepare-frame-close-module)
                     (lambda (id) (setq prepared id))))
            (ewm--prevent-frame-deletion
             (lambda (arg force)
               (setq called-frame arg)
               (setq called-force force))
             nil t))
          (should (eq called-frame frame))
          (should (eq called-force t))
          (should (eql prepared 42)))
      (set-frame-parameter frame 'ewm-output old-output)
      (set-frame-parameter frame 'ewm-surface-id old-surface-id))))

(ert-deftest ewm-focus/delete-frame-advice-rejects-last-frame-on-output ()
  "The only frame on an output is not deleted unless FORCE is non-nil."
  (let* ((frame (selected-frame))
         (output "TEST-OUT")
         (prepared nil)
         (called nil)
         (old-output (frame-parameter frame 'ewm-output))
         (old-surface-id (frame-parameter frame 'ewm-surface-id))
         (ewm--strips `((,output . (,frame)))))
    (unwind-protect
        (progn
          (set-frame-parameter frame 'ewm-output output)
          (set-frame-parameter frame 'ewm-surface-id 42)
          (cl-letf (((symbol-function 'ewm-prepare-frame-close-module)
                     (lambda (id) (setq prepared id))))
            (ewm--prevent-frame-deletion
             (lambda (&rest _) (setq called t))
             frame nil))
          (should-not called)
          (should-not prepared))
      (set-frame-parameter frame 'ewm-output old-output)
      (set-frame-parameter frame 'ewm-surface-id old-surface-id))))

(ert-deftest ewm-focus/frame-moved-event-updates-local-strip-and-home ()
  "Elisp mirrors compositor-owned frame topology."
  (let* ((frame (selected-frame))
         (old-output (frame-parameter frame 'ewm-output))
         (old-home-output (frame-parameter frame 'ewm-home-output))
         (old-surface-id (frame-parameter frame 'ewm-surface-id))
         (refresh-count 0)
         (ewm--strips `(("eDP-1" . (,frame)))))
    (unwind-protect
        (progn
          (set-frame-parameter frame 'ewm-output "eDP-1")
          (set-frame-parameter frame 'ewm-home-output nil)
          (set-frame-parameter frame 'ewm-surface-id 42)
          (cl-letf (((symbol-function 'ewm-layout--refresh)
                     (lambda () (cl-incf refresh-count))))
            (ewm--handle-frame-moved
             '((event . "frame_moved")
               (id . 42)
               (output . "DP-2")
               (home_output . "eDP-1"))))
          (should (= refresh-count 1))
          (should (equal (frame-parameter frame 'ewm-output) "DP-2"))
          (should (equal (frame-parameter frame 'ewm-home-output) "eDP-1"))
          (should (equal (ewm--strip-frames "DP-2") (list frame)))
          (should-not (ewm--strip-frames "eDP-1")))
      (set-frame-parameter frame 'ewm-output old-output)
      (set-frame-parameter frame 'ewm-home-output old-home-output)
      (set-frame-parameter frame 'ewm-surface-id old-surface-id))))

(ert-deftest ewm-focus/with-surface-buffer-skips-unknown-id ()
  "Event for an unregistered surface id is a no-op."
  (let ((ewm--surfaces (make-hash-table :test 'eql))
        (ran nil))
    (ewm--with-surface-buffer-from-event '((id . 42)) (setq ran t))
    (should-not ran)))

(ert-deftest ewm-focus/with-surface-buffer-skips-dead-buffer ()
  "Event whose surface buffer has been killed is a no-op."
  (let ((ewm--surfaces (make-hash-table :test 'eql))
        (buf (generate-new-buffer " *ewm-test-dead*"))
        (ran nil))
    (puthash 42 buf ewm--surfaces)
    (kill-buffer buf)
    (ewm--with-surface-buffer-from-event '((id . 42)) (setq ran t))
    (should-not ran)))

(ert-deftest ewm-focus/with-surface-buffer-runs-in-live-buffer ()
  "Body runs with the live surface buffer as `current-buffer'."
  (let ((ewm--surfaces (make-hash-table :test 'eql))
        (buf (generate-new-buffer " *ewm-test-live*")))
    (unwind-protect
        (progn
          (puthash 42 buf ewm--surfaces)
          (let (captured)
            (ewm--with-surface-buffer-from-event '((id . 42))
              (setq captured (current-buffer)))
            (should (eq captured buf))))
      (kill-buffer buf))))

(ert-deftest ewm-focus/mff-off-skips-warp ()
  "With MFF off, the selection-change hook is a no-op."
  (let ((warps 0)
        (ewm-mouse-follows-focus nil))
    (cl-letf (((symbol-function 'ewm-input--warp-pointer-to-window)
               (lambda (_) (cl-incf warps))))
      (ewm-input--on-window-selection-change (selected-frame))
      (should (= 0 warps)))))

(ert-deftest ewm-focus/mff-warps-on-selection-change ()
  "MFF on + selection differs from anchor: warp fires and anchor updates."
  (let ((warps 0)
        (ewm-mouse-follows-focus t)
        (ewm--mff-last-window nil))
    (cl-letf (((symbol-function 'ewm-input--warp-pointer-to-window)
               (lambda (_) (cl-incf warps))))
      (ewm-input--on-window-selection-change (selected-frame))
      (should (= 1 warps))
      (should (eq ewm--mff-last-window (selected-window))))))

(ert-deftest ewm-focus/mff-idempotent-on-unchanged-selection ()
  "Repeated hook fires without a real selection change must not warp.
Process filters (telega, magit) jostle `selected-window' between
commands; if this guard breaks, every background event warps the
pointer."
  (let ((warps 0)
        (ewm-mouse-follows-focus t)
        (ewm--mff-last-window (selected-window)))
    (cl-letf (((symbol-function 'ewm-input--warp-pointer-to-window)
               (lambda (_) (cl-incf warps))))
      (ewm-input--on-window-selection-change (selected-frame))
      (ewm-input--on-window-selection-change (selected-frame))
      (should (= 0 warps)))))

(ert-deftest ewm-focus/mff-skips-minibuffer ()
  "Selecting the minibuffer never warps the pointer."
  (ewm-focus-tests--with-frame f
    (let ((warps 0))
      (cl-letf (((symbol-function 'ewm-warp-pointer)
                 (lambda (_ _) (cl-incf warps)))
                ((symbol-function 'ewm--get-output-origin)
                 (lambda (_) (cons 0 0)))
                ;; Force the pointer-inside guard to fall through.
                ((symbol-function 'ewm-input--pointer-in-window-p)
                 (lambda (_) nil)))
        (ewm-input--warp-pointer-to-window (minibuffer-window f))
        (should (= 0 warps))))))

(ert-deftest ewm-focus/mff-skips-when-pointer-already-inside ()
  "If the pointer is already in the target window, no warp."
  (ewm-focus-tests--with-frame f
    (let ((warps 0))
      (cl-letf (((symbol-function 'ewm-warp-pointer)
                 (lambda (_ _) (cl-incf warps)))
                ((symbol-function 'ewm--get-output-origin)
                 (lambda (_) (cons 0 0)))
                ((symbol-function 'ewm-input--pointer-in-window-p)
                 (lambda (_) t)))
        (ewm-input--warp-pointer-to-window (frame-selected-window f))
        (should (= 0 warps))))))

(ert-deftest ewm-focus/ffm-on-frame-edge-does-not-warp ()
  "FFM hover on a frame edge keeps `ewm--mff-last-window' in sync, suppressing MFF warp."
  (ewm-focus-tests--with-frame f
    (let ((warps 0)
          (ewm-focus-follows-mouse t)
          (ewm-mouse-follows-focus t)
          (ewm--mff-last-window nil)
          (frame-focus-id (ewm-layout--ensure-frame-focus-id f)))
      (cl-letf (((symbol-function 'ewm-input--warp-pointer-to-window)
                 (lambda (_) (cl-incf warps)))
                ;; Frame-edge: no Emacs window at this point.
                ((symbol-function 'window-at-x-y) (lambda (&rest _) nil)))
        (ewm--handle-focus `((focus_id . ,frame-focus-id) (x . 0) (y . 0)))
        (ewm-input--on-window-selection-change f)
        (should (= 0 warps))))))

(ert-deftest ewm-focus/keyboard-cross-output-warps ()
  "Keyboard focus event (no x/y) leaves MFF anchor stale so the hook warps."
  (ewm-focus-tests--with-frame f
    (let ((warps 0)
          (ewm-focus-follows-mouse t)
          (ewm-mouse-follows-focus t)
          (ewm--mff-last-window nil)
          (frame-focus-id (ewm-layout--ensure-frame-focus-id f)))
      (cl-letf (((symbol-function 'ewm-input--warp-pointer-to-window)
                 (lambda (_) (cl-incf warps))))
        (ewm--handle-focus `((focus_id . ,frame-focus-id)))
        (ewm-input--on-window-selection-change f)
        (should (= 1 warps))))))

(provide 'ewm-focus-tests)
;;; ewm-focus-tests.el ends here
