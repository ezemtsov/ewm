# ewm - Emacs Wayland Manager

<video src="https://codeberg.org/ezemtsov/ewm/raw/branch/master/docs/showcase/ewm-showcase.mp4" autoplay loop muted playsinline controls></video>

ewm is a modern, fast Linux desktop environment written in Rust, based on [Smithay](https://github.com/Smithay/smithay), and running as a dynamic module inside of your emacs session. Graphical applications appear as normal emacs buffers, and are managed with the same commands, keybindings, and workflows as the rest of your configuration.

The main goal of ewm is to offer a distraction-free working environment, optimized for keyboard input. Most emacs actions do not require visual feedback: once a command sequence is learned, it can be executed blindly through deterministic key presses. Over time this creates a huge productivity boost, with virtually no skill cap. The only missing piece with emacs was graphical windows, and ewm solves that by making Wayland windows part of the Emacs environment itself.

## quick start

Launch apps with `s-d`. See the [wiki](https://codeberg.org/ezemtsov/ewm/wiki) for full setup, configuration, and NixOS instructions.

For local development and testing, I use the helper script, started from inside `nix-shell`:

```bash
cd compositor
./test.sh
```

The script builds ewm with screencast support, sets `EWM_MODULE_PATH`, and starts Emacs with `--fg-daemon=vt2`. Pass an init directory as the only argument when you need one, for example `./test.sh /etc/nixos/emacs`. The named daemon is intentional: plain `--fg-daemon` uses the default Emacs server socket and can conflict with an existing Emacs daemon. Connect to this session with `emacsclient --socket-name=vt2`.

## current features

- Wayland applications as regular Emacs buffers
- Emacs-native window management, focus, keybindings, and fullscreen
- Multi-monitor DRM backend with hotplug, output configuration, and fractional scaling
- Layer-shell, workspace, screen lock, idle, activation, system bell, clipboard, and drag-and-drop protocol support
- Screen sharing through xdg-desktop-portal and PipeWire
- Input methods and keyboard configuration integrated with Emacs
- External app launching through XDG desktop entries

## known limitations

- Must run from TTY (no nested mode)

## inspirations

- **[EXWM](https://github.com/ch11ng/exwm)** (Emacs side): Buffer-per-window model, prefix key interception, automatic focus management
- **[niri](https://github.com/YaLTeR/niri)** (Compositor side): Backend architecture, Smithay patterns, DRM abstraction

## approach

Writing a Wayland compositor from scratch is a staggering amount of work. I wanted to switch from EXWM as soon as possible, so the initial bootstrap was done with a bunch of agentic tools, which helped a lot in reverse-engineering the brilliant niri codebase and surgically extracting the pieces relevant to ewm. Today, coding agents are used mainly to compare ewm against [niri](https://github.com/YaLTeR/niri) and other relevant references to catch divergences from proven Smithay compositor patterns before they become correctness, performance, or reliability issues. This feedback loop is a major reason ewm could be built quickly while still becoming fast and reliable enough for daily use.

For people who are strictly against LLMs and prefer to keep their hands completely clean, I highly recommend checking out another take on a Wayland-based emacs compositor from my great friend tazjin: [reka](https://code.tvl.fyi/about/tools/emacs-pkgs/reka). It's based on River, so at a small cost in features it helped to contain the complexity enough to write everything by hand in a relatively short time.

## community

Join the Matrix room: [#ewm:matrix.org](https://matrix.to/#/#ewm:matrix.org)

## license

GPL-3.0-or-later

Portions copyright Ivan Molodetskikh and niri contributors.
