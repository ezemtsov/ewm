# EWM Input Methods

## Feature Status

| Feature | Protocol | Status | Notes |
|---------|----------|--------|-------|
| Commit string | `text_input_v3` / `input_method_v2` | Done | Direct on compositor thread, queued during gaps |
| Key interception | `text_input_v3` | Done | Printable keys routed to Emacs for IM translation |
| Activate/deactivate | `input_method_v2` | Done | Relay thread forwards events to Emacs |
| XKB layout switching | core Wayland | Done | Multi-layout with temporary reset for intercepts |
| Preedit (composing text) | `text_input_v3` | TODO | Inline composition preview in client (CJK essential) |
| Content type hints | `text_input_v3` | TODO | Auto-disable intercept for password fields |
| Surrounding text | `text_input_v3` | TODO | Selection access for cut/copy in surface buffers |
| Delete surrounding text | `input_method_v2` | TODO | Cut in surface buffers, backspace during composition |
| Input popup surface | `input_method_v2` | TODO | Candidate window positioned at client cursor |

### Priority

**Preedit** is the most impactful missing feature. It's required for proper
CJK input (Japanese, Chinese, Korean) in external apps. Without it, composing
characters only appear in Emacs's echo area, not inline in the client text field.

**Surrounding text + delete** together enable clipboard operations (cut/copy)
in surface buffers via the protocol rather than synthetic keypresses:

1. Client sends `set_surrounding_text(text, cursor, anchor)`. When
   `cursor != anchor`, the text between them is the selection.
2. Emacs extracts the selected portion and puts it on the clipboard.
3. For cut, compositor sends `delete_surrounding_text` via `input_method_v2`
   to remove the selection in the client.

This would let `s-x` (cut) and `s-c` (copy) work in surface buffers the
same way they work in Emacs buffers, using the Wayland protocol instead of
injecting synthetic Ctrl+X/C keypresses.

## Overview

EWM provides full input method support:
- **XKB Layout Switching**: Multiple keyboard layouts with hardware/software switching
- **Text Input Protocol**: Emacs input methods work in external Wayland apps (Firefox, terminals)

## XKB Keyboard Layouts

### Configuration

```elisp
(setopt ewm-input-config
        '((keyboard :xkb-layouts "us,ru,no"
                    :xkb-options "grp:caps_toggle")))
```

On startup, Emacs configures XKB through the input-device configuration sent
to the dynamic module.

### Module Interface

**Emacs -> Compositor (module functions):**

```elisp
(ewm-configure-input-module configs) ; Set keyboard layouts/options
(ewm-switch-layout-module layout)    ; Switch to layout by name
(ewm-get-layouts-module)             ; Query current layouts
```

**Compositor -> Emacs (events via pipe fd):**

| Event | Fields | Purpose |
|-------|--------|---------|
| `layouts` | `layouts`, `current` | Report configured layouts |
| `layout-switched` | `layout`, `index` | Layout changed |

### Implementation

XKB supports multiple layouts via "groups" (indexed 0, 1, 2...). Switching
between groups is fast (no keymap recompilation). XKB options like
`grp:caps_toggle` work natively.

#### Layout preservation during key interception

When keyboard capture routes a key sequence to Emacs (for example `C-x ...`),
the compositor temporarily switches to the base layout (index 0) so Emacs
receives Latin keysyms for keybinding dispatch. The layout is restored
immediately after each forwarded key; no persistent change occurs.

Emacs requests keyboard capture through tokened leases. Beginning capture
returns a token, and ending capture must present that same token. Capture stays
active while any lease remains, so stale cleanup from one
minibuffer/transient/read-key unwind cannot clear another live capture.
Keyboard-dispatch intercepts also open synchronous redirect capture in
the compositor so a prefix like `C-x` keeps the next key routed to Emacs even
if the physical prefix key is released before Lisp's post-command hook runs.
The next inactive command-loop sync clears that redirect capture.

`ewm-mode-map` command bindings do not move Wayland keyboard focus. They are
delivered to Lisp over the event pipe. Only commands listed in
`ewm-repeatable-commands` and the built-in media/brightness keys repeat from
the compositor timer. Plain `ewm-intercept-prefixes` entries that are not also
bound in `ewm-mode-map` use keyboard delivery, so keys such as `M-x` and `C-u`
enter Emacs's normal command loop.

The repeat configuration has one effective source: the `(keyboard
:repeat-rate ... :repeat-delay ...)` entry in `ewm-input-config`, falling back
to `25Hz` and `200ms`. The compositor applies that same `(rate, delay)` to
Wayland keyboard repeat and to intercepted command/text-input repeat.
Repeatable command-dispatch bindings use the compositor timer. Text-input
repeat still follows XKB repeatability so client text input behaves like
ordinary Wayland keyboard input.

Held-key ownership and timers live in `im::repeat`.

#### Intercepted key invariants

The intercept/capture hot path keeps these invariants:

1. Keyboard capture is a Wayland keyboard-focus overlay; it does not change
   EWM's stored focused frame/view pair.
2. Command-dispatch intercepted keys are `ewm-mode-map` command bindings; they
   go through the event pipe and do not move keyboard focus to Emacs by
   themselves.
3. Once EWM consumes an intercepted physical press, backend repeat presses and
   the matching release are consumed by EWM too, never forwarded to the client
   surface.
4. Keyboard capture and command-repeat ownership are mutually exclusive, so a
   held command/media key cannot keep repeating behind a minibuffer, transient,
   or `read-key` sequence.
5. Stale keyboard-capture token ends are ignored; capture remains active while
   any other lease is live.
6. Repeat timers are valid only for the currently held intercepted key.

## Text Input (Emacs IM in External Apps)

Allows Emacs input methods to work in external Wayland surfaces, similar
to exwm-xim on X11.

### How It Works

```
Application                    Compositor                      Emacs
     |                              |                              |
     |--enable text_input---------->|                              |
     |                              |--text-input-activated------->|
     |                              |                              |
     |  [user types]                |                              |
     |                              |--key {keysym, utf8}--------->|
     |                              |                              |
     |                              |<--im-commit {text, id}-------|
     |<--commit_string (direct)-----|                              |
```

Commits are applied directly on the compositor thread via `TextInputHandle`,
avoiding cross-thread serial races. If the client is in a disable->enable gap
(e.g., after focus returned from the minibuffer), commits are queued and
drained on the next Activated event for the same surface. Lisp translates
intercepted keys inside the focused surface buffer, so buffer-local input
methods cannot leak between surfaces.

### Module Interface

**Compositor -> Emacs (events via pipe fd):**

| Event | Fields | Purpose |
|-------|--------|---------|
| `text-input-activated` | - | Text field focused in client |
| `text-input-deactivated` | - | Text field unfocused |
| `key` | `keysym`, `utf8` | Key press in text field |

**Emacs -> Compositor (module functions):**

```elisp
(ewm-im-commit-module text surface-id)   ; Insert text into focused field
(ewm-text-input-intercept-module enable) ; Enable/disable key interception
```

### Usage

1. Focus a text field in a Wayland app (Firefox, foot, etc.)
2. Activate an Emacs input method: `M-x set-input-method RET russian-computer`
3. Type - keys are intercepted, processed by Emacs, and committed to the app

No environment variables needed (unlike X11's `XMODIFIERS`).

### Implementation Notes

The compositor implements both protocols:
- `zwp_text_input_v3` (client-side): Apps request text input
- `zwp_input_method_v2` (compositor-side): Manages input method state

Key interception only occurs when:
1. Text input is active (app requested it)
2. An Emacs input method is enabled
3. The focused surface is not Emacs (Emacs handles its own input)
