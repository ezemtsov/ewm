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

**Preedit** is the most impactful missing feature — it's required for proper
CJK input (Japanese, Chinese, Korean) in external apps. Without it, composing
characters only appear in Emacs's echo area, not inline in the client text field.

**Surrounding text + delete** together enable clipboard operations (cut/copy)
in surface buffers via the protocol rather than synthetic keypresses:

1. Client sends `set_surrounding_text(text, cursor, anchor)` — when
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
(setq ewm-xkb-layouts '("us" "ru" "no"))     ; Available layouts
(setq ewm-xkb-options "grp:caps_toggle")     ; Caps Lock toggles layout
```

On startup, Emacs configures XKB via the dynamic module.

### Module Interface

**Emacs → Compositor (module functions):**

```elisp
(ewm-configure-xkb-module layouts options)  ; Set layouts and XKB options
(ewm-switch-layout-module layout)           ; Switch to layout by name
(ewm-get-layouts-module)                    ; Query current layouts
```

**Compositor → Emacs (events via pipe fd):**

| Event | Fields | Purpose |
|-------|--------|---------|
| `layouts` | `layouts`, `current` | Report configured layouts |
| `layout_switched` | `layout`, `index` | Layout changed |

### Implementation

XKB supports multiple layouts via "groups" (indexed 0, 1, 2...). Switching
between groups is fast (no keymap recompilation). XKB options like
`grp:caps_toggle` work natively.

#### Layout preservation during key interception

When an intercepted key (e.g., `s-d`, `C-x`) redirects focus to Emacs, the
compositor temporarily switches to the base layout (index 0) so Emacs
receives Latin keysyms for keybinding dispatch. The layout is restored
immediately after the key is forwarded — no persistent change occurs.

For prefix sequences (`C-x ...`), the same temporary reset is applied to
each subsequent key while the sequence is active, using the existing
`in_prefix_sequence` flag. Once the sequence completes and focus returns
to the external surface, the user's chosen layout is intact.

This is implemented entirely with local variables in `handle_keyboard_event`
— no cross-function state needed.

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
     |                              |--input-key {keysym}--------->|
     |                              |                              |
     |                              |<--im-commit {text, id}-------|
     |<--commit_string (direct)-----|                              |
```

Commits are applied directly on the compositor thread via `TextInputHandle`,
avoiding cross-thread serial races. If the client is in a disable→enable gap
(e.g., after focus returned from the minibuffer), commits are queued and
drained on the next Activated event.

### Module Interface

**Compositor → Emacs (events via pipe fd):**

| Event | Fields | Purpose |
|-------|--------|---------|
| `text_input_activated` | - | Text field focused in client |
| `text_input_deactivated` | - | Text field unfocused |
| `key` | `keysym`, `utf8` | Key press in text field |

**Emacs → Compositor (module functions):**

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
