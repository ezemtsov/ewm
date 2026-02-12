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

On startup, Emacs configures XKB via the dynamic module.

### Module Interface

**Emacs → Compositor (module functions):**

```elisp
(ewm-configure-xkb-module layouts options)  ; Set layouts and XKB options
(ewm-switch-layout-module layout)           ; Switch to layout by name
(ewm-get-layouts-module)                    ; Query current layouts
```

**Compositor → Emacs (events via SIGUSR1):**

| Event | Fields | Purpose |
|-------|--------|---------|
| `layouts` | `layouts`, `current` | Report configured layouts |
| `layout_switched` | `layout`, `index` | Layout changed |

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

### Module Interface

**Compositor → Emacs (events via SIGUSR1):**

| Event | Fields | Purpose |
|-------|--------|---------|
| `text_input_activated` | - | Text field focused in client |
| `text_input_deactivated` | - | Text field unfocused |
| `key` | `keysym`, `utf8` | Key press in text field |

**Emacs → Compositor (module functions):**

```elisp
(ewm-im-commit-module text)              ; Insert text into focused field
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
