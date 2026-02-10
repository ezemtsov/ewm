# EWM Input Methods

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

On connect, Emacs sends `configure-xkb` to initialize the keyboard.

### IPC Protocol

**Commands (Emacs → Compositor):**

| Command | Fields | Purpose |
|---------|--------|---------|
| `configure-xkb` | `layouts`, `options` | Set available layouts and XKB options |
| `switch-layout` | `layout` | Switch to layout by name |
| `get-layouts` | - | Query current layouts |

**Events (Compositor → Emacs):**

| Event | Fields | Purpose |
|-------|--------|---------|
| `layouts` | `layouts`, `current` | Report configured layouts |
| `layout-switched` | `layout`, `index` | Layout changed |

### Implementation

XKB supports multiple layouts via "groups" (indexed 0, 1, 2...). Switching
between groups is fast (no keymap recompilation). XKB options like
`grp:caps_toggle` work natively.

When redirecting intercepted keys to Emacs, the compositor switches to
the base layout (index 0) to ensure keybindings work correctly.

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
     |                              |<--commit-text {string}-------|
     |<--commit_string--------------|                              |
```

### IPC Protocol

**Events (Compositor → Emacs):**

| Event | Fields | Purpose |
|-------|--------|---------|
| `text-input-activated` | - | Text field focused in client |
| `text-input-deactivated` | - | Text field unfocused |
| `input-key` | `keysym`, `state` | Key press in text field |

**Commands (Emacs → Compositor):**

| Command | Fields | Purpose |
|---------|--------|---------|
| `commit-text` | `id`, `text` | Insert text into focused field |

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
