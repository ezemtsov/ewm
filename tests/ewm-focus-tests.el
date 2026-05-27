;;; ewm-focus-tests.el --- ERT tests for EWM focus contracts -*- lexical-binding: t -*-

;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:

;; Pins the focus contract between the lisp layer and the compositor:
;; whenever Emacs is reading keys for itself, every frame's
;; `:selected-view-id' shipped to the compositor must be nil so keyboard
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
(unless (fboundp 'ewm-focus-view-module)
  (defun ewm-focus-view-module (_view-id) nil))
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
(defconst ewm-focus-tests--surface-view-id
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
  "Display a fresh surface buffer in WINDOW with matching view parameters."
  (let ((buf (ewm-focus-tests--make-surface-buffer surface-id)))
    (set-window-buffer window buf)
    (set-window-parameter window 'ewm-view-id (+ 1000 surface-id))
    (set-window-parameter window 'ewm-view-surface surface-id)))

(defmacro ewm-focus-tests--with-frame (var &rest body)
  "Bind VAR to the batch frame, EWM-configured for the duration of BODY.
The batch frame is shared; we save and restore the parameters we touch."
  (declare (indent 1))
  `(let* ((,var (selected-frame))
          (saved-surface-id (frame-parameter ,var 'ewm-surface-id))
          (saved-output (frame-parameter ,var 'ewm-output))
          (saved-window (frame-selected-window ,var))
          (saved-buffer (window-buffer saved-window))
          (saved-view-id (window-parameter saved-window 'ewm-view-id))
          (saved-view-surface (window-parameter saved-window 'ewm-view-surface))
          (ewm-focus-tests--scratch-buffers nil))
     (set-frame-parameter ,var 'ewm-surface-id ewm-focus-tests--frame-surface-id)
     (set-frame-parameter ,var 'ewm-output ewm-focus-tests--output)
     (unwind-protect
         (progn ,@body)
       (set-frame-parameter ,var 'ewm-surface-id saved-surface-id)
       (set-frame-parameter ,var 'ewm-output saved-output)
       (when (buffer-live-p saved-buffer)
         (set-window-buffer saved-window saved-buffer))
       (set-window-parameter saved-window 'ewm-view-id saved-view-id)
       (set-window-parameter saved-window 'ewm-view-surface saved-view-surface)
       (dolist (b ewm-focus-tests--scratch-buffers)
         (when (buffer-live-p b) (kill-buffer b))))))

(defun ewm-focus-tests--single-frame-strip (frame)
  "Set `ewm--strips' to a single-output strip containing FRAME."
  (setq ewm--strips `((,ewm-focus-tests--output . (,frame))))
  (setq ewm--module-mode t))

(defun ewm-focus-tests--capture-selected-view-id ()
  "Set up a single-frame strip and return the layout's `:selected-view-id'."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (ewm-focus-tests--single-frame-strip f)
    (let* ((captures (ewm-focus-tests--with-captured-layouts
                       (ewm-layout--send-layouts)))
           (frame (aref (cdr (assoc ewm-focus-tests--output captures)) 0)))
      (plist-get frame :selected-view-id))))

;;;; Tests

(ert-deftest ewm-focus/baseline-no-menu ()
  "No menu active: `:selected-view-id' names the embedded surface."
  (should (eql ewm-focus-tests--surface-view-id
               (ewm-focus-tests--capture-selected-view-id))))

(ert-deftest ewm-focus/minibuffer-suppresses ()
  "Active minibuffer suppresses embedded-view focus."
  ;; `-batch' can't drive a real minibuffer read, so stub the predicate.
  (cl-letf (((symbol-function 'active-minibuffer-window)
             (lambda () (selected-window))))
    (should (null (ewm-focus-tests--capture-selected-view-id)))))

(ert-deftest ewm-focus/overriding-keymap-suppresses ()
  "Any `overriding-terminal-local-map' suppresses embedded-view focus."
  (let ((overriding-terminal-local-map (make-sparse-keymap)))
    (should (null (ewm-focus-tests--capture-selected-view-id)))))

(ert-deftest ewm-focus/custom-predicate-suppresses ()
  "A user predicate added to `ewm-layout-menu-predicates' suppresses."
  (let ((ewm-layout-menu-predicates (list (lambda () t))))
    (should (null (ewm-focus-tests--capture-selected-view-id)))))

(ert-deftest ewm-focus/menu-active-preserves-entries ()
  "Suppression touches `:selected-view-id' only; `:surfaces' is unchanged."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (ewm-focus-tests--single-frame-strip f)
    (let* ((ewm-layout-menu-predicates (list (lambda () t)))
           (captures (ewm-focus-tests--with-captured-layouts
                       (ewm-layout--send-layouts)))
           (surfaces (plist-get (aref (cdr (assoc ewm-focus-tests--output captures)) 0)
                                :surfaces)))
      (should (= 1 (length surfaces)))
      (should (eql ewm-focus-tests--surface-id
                   (plist-get (aref surfaces 0) :id))))))

(ert-deftest ewm-focus/build-frame-plist-honors-menu-flag ()
  "`build-frame-plist' zeros `:selected-view-id' when MENU-ACTIVE is non-nil."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (should (eql ewm-focus-tests--surface-view-id
                 (plist-get (ewm-layout--build-frame-plist f nil)
                            :selected-view-id)))
    (should (null (plist-get (ewm-layout--build-frame-plist f t)
                             :selected-view-id)))))

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

(ert-deftest ewm-focus/selected-view-id-always-in-surfaces ()
  "Lisp-side invariant 6: a non-nil `:selected-view-id' is in `:surfaces'."
  (ewm-focus-tests--with-frame f
    (ewm-focus-tests--install-surface-window
     (frame-selected-window f) ewm-focus-tests--surface-id)
    (let* ((plist (ewm-layout--build-frame-plist f nil))
           (sel (plist-get plist :selected-view-id))
           (view-ids (cl-loop for s across (plist-get plist :surfaces)
                              collect (plist-get s :view-id))))
      (should sel)
      (should (member sel view-ids)))))

(ert-deftest ewm-focus/buffer-rotation-keeps-selection-consistent ()
  "Stale window params from a prior surface must be reconciled within one
call to `build-frame-plist'; reorder its two passes and this fails."
  (ewm-focus-tests--with-frame f
    (let ((win (frame-selected-window f)))
      (set-window-buffer win (ewm-focus-tests--make-surface-buffer 7))
      ;; Seed parameters as if the window had previously held surface 5.
      (set-window-parameter win 'ewm-view-id 1005)
      (set-window-parameter win 'ewm-view-surface 5)
      (let* ((plist (ewm-layout--build-frame-plist f nil))
             (sel (plist-get plist :selected-view-id))
             (surfaces (plist-get plist :surfaces)))
        (should sel)
        (should-not (eql sel 1005))
        (should (= 1 (length surfaces)))
        (should (eql 7 (plist-get (aref surfaces 0) :id)))
        (should (eql sel (plist-get (aref surfaces 0) :view-id)))))))

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
                   (surfaces (plist-get frame-plist :surfaces))
                   (entry (aref surfaces 0)))
              (should (equal (car (car captures)) ewm-focus-tests--output))
              (should (= 1 (length surfaces)))
              (should (eql 80 (plist-get entry :id)))
              (should (eql (plist-get entry :view-id)
                           (plist-get frame-plist :selected-view-id)))))
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
          (event `((view_id . ,ewm-focus-tests--surface-view-id))))
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
          (frame-view-id (ewm-layout--ensure-frame-view-id f)))
      (cl-letf (((symbol-function 'ewm-input--warp-pointer-to-window)
                 (lambda (_) (cl-incf warps)))
                ;; Frame-edge: no Emacs window at this point.
                ((symbol-function 'window-at-x-y) (lambda (&rest _) nil)))
        (ewm--handle-focus `((view_id . ,frame-view-id) (x . 0) (y . 0)))
        (ewm-input--on-window-selection-change f)
        (should (= 0 warps))))))

(ert-deftest ewm-focus/keyboard-cross-output-warps ()
  "Keyboard focus event (no x/y) leaves MFF anchor stale so the hook warps."
  (ewm-focus-tests--with-frame f
    (let ((warps 0)
          (ewm-focus-follows-mouse t)
          (ewm-mouse-follows-focus t)
          (ewm--mff-last-window nil)
          (frame-view-id (ewm-layout--ensure-frame-view-id f)))
      (cl-letf (((symbol-function 'ewm-input--warp-pointer-to-window)
                 (lambda (_) (cl-incf warps))))
        (ewm--handle-focus `((view_id . ,frame-view-id)))
        (ewm-input--on-window-selection-change f)
        (should (= 1 warps))))))

(provide 'ewm-focus-tests)
;;; ewm-focus-tests.el ends here
