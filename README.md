# nirix

A scrolling terminal multiplexer, inspired by [niri](https://github.com/niri-wm/niri).

Each terminal is a column. New terminals open to the right, and the view scrolls to them.

## Install

Linux only. Run the same command again to update.

```sh
curl -fsSL https://raw.githubusercontent.com/AkshayDhola/nirix/main/install.sh | bash
```

## Keys

| Key | Action |
|---|---|
| `Alt+Enter`, `Alt+n` | New terminal |
| `Alt+Left`, `Alt+Right` | Focus left or right |
| `Alt+Shift+Left`, `Alt+Shift+Right` | Move terminal left or right |
| `Alt+1` to `Alt+9` | Jump to terminal |
| `Alt+r` | Change width |
| `Alt+m` | Full width |
| `Alt+x` | Close terminal |

## Config

Optional file at `~/.config/nirix/config.toml`:

```toml
shell = "/usr/bin/zsh"
focus_color = "blue"          # name, bright-<name>, 0-255, or "#rrggbb"
border_color = "bright-black"
animation = true
animation_ms = 250
```

## License

MIT
