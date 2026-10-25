# nirix

A terminal multiplexer that scrolls, like the [niri](https://github.com/niri-wm/niri) window manager.

Each terminal is a column. A new one opens to the right and the view slides over to it, so the others never shrink.

```
  ┌───────┐ ┌───────────────────────┐ ┌───────┐
  │ term 1│ │ term 2    │ term 3    │ │ term 4│
  └───────┘ └───────────────────────┘ └───────┘
   off screen        on screen         off screen
```

## Install

You need [Rust](https://rustup.rs). Works on Linux and macOS.

```sh
git clone https://github.com/AkshayDhola/nirix
cd nirix
./install.sh
```

This puts `nirix` in `~/.local/bin` and creates a config file at `~/.config/nirix/config.toml`.

## Keys

| Key | Action |
|---|---|
| `Alt+Enter` | New terminal |
| `Alt+Left` / `Alt+Right` | Focus left / right |
| `Alt+Shift+Left` / `Alt+Shift+Right` | Move terminal left / right |
| `Alt+1` … `Alt+9` | Jump to terminal |
| `Alt+r` | Change width |
| `Alt+m` | Full width |
| `Alt+x` | Close terminal |

`Alt+n` also opens a terminal, and `Alt+h/l` (`Alt+H/L` to move) work like the arrow keys.

## Config

`~/.config/nirix/config.toml`. Every setting is optional.

```toml
shell = "/usr/bin/zsh"
focus_color = "blue"          # color name, bright-<name>, 0-255, or "#rrggbb"
border_color = "bright-black"
animation = true
animation_ms = 250
```

## License

MIT
