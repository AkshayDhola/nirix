//! `~/.config/nirix/config.toml`: a flat `key = value` subset of TOML.

use std::path::PathBuf;
use std::time::Duration;

use crossterm::style::Color;

pub struct Config {
    pub shell: String,
    pub focus_color: Color,
    pub border_color: Color,
    pub animation: bool,
    pub animation_time: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            shell: std::env::var("SHELL").unwrap_or("/bin/sh".into()),
            focus_color: Color::AnsiValue(4),
            border_color: Color::AnsiValue(8),
            animation: true,
            animation_time: Duration::from_millis(250),
        }
    }
}

const NAMES: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];

/// Palette name (follows terminal theme), `bright-<name>`, 0-255, `#rrggbb` or `default`.
fn color(v: &str) -> Option<Color> {
    let (bright, name) = match v.strip_prefix("bright-") {
        Some(n) => (8, n),
        None => (0, v),
    };
    if let Some(i) = NAMES.iter().position(|&n| n == name) {
        return Some(Color::AnsiValue(i as u8 + bright));
    }
    if v == "default" {
        return Some(Color::Reset);
    }
    if let Some(hex) = v.strip_prefix('#').filter(|h| h.len() == 6) {
        let n = u32::from_str_radix(hex, 16).ok()?;
        return Some(Color::Rgb { r: (n >> 16) as u8, g: (n >> 8) as u8, b: n as u8 });
    }
    v.parse().ok().map(Color::AnsiValue)
}

/// Drop a trailing `# comment` that isn't inside quotes.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

pub fn parse(text: &str) -> Result<Config, String> {
    let mut cfg = Config::default();
    for (n, line) in text.lines().enumerate() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let err = |msg: String| format!("line {}: {msg}", n + 1);
        let (key, raw) = line.split_once('=').ok_or_else(|| err("expected `key = value`".into()))?;
        let (key, raw) = (key.trim(), raw.trim());
        let val = raw.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(raw);
        let bad = || err(format!("bad value for `{key}`: {raw}"));
        match key {
            "shell" => cfg.shell = val.into(),
            "focus_color" => cfg.focus_color = color(val).ok_or_else(bad)?,
            "border_color" => cfg.border_color = color(val).ok_or_else(bad)?,
            "animation" => cfg.animation = val.parse().map_err(|_| bad())?,
            "animation_ms" => cfg.animation_time = Duration::from_millis(val.parse().map_err(|_| bad())?),
            _ => return Err(err(format!("unknown key `{key}`"))),
        }
    }
    Ok(cfg)
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("nirix/config.toml"))
}

/// Missing file means defaults; a broken one is an error naming the file.
pub fn load() -> Result<Config, String> {
    let Some(path) = path() else { return Ok(Config::default()) };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        let c = parse("# hi\nshell = \"/bin/zsh\" # mine\nfocus_color = \"#ff0080\"\nborder_color = bright-black\nanimation = false\nanimation_ms = 100\n").unwrap();
        assert_eq!(c.shell, "/bin/zsh");
        assert_eq!(c.focus_color, Color::Rgb { r: 255, g: 0, b: 128 });
        assert_eq!(c.border_color, Color::AnsiValue(8));
        assert!(!c.animation);
        assert_eq!(c.animation_time, Duration::from_millis(100));
        assert_eq!(color("blue"), Some(Color::AnsiValue(4)));
        assert_eq!(color("200"), Some(Color::AnsiValue(200)));
    }

    #[test]
    fn example_file_parses() {
        parse(include_str!("../config.example.toml")).unwrap();
    }

    #[test]
    fn errors() {
        assert_eq!(parse("\nfoo = 1").err().unwrap(), "line 2: unknown key `foo`");
        assert!(parse("focus_color = pink").is_err());
        assert!(parse("animation = maybe").is_err());
        assert!(parse("shell").is_err());
    }
}
