//! nirix: a terminal multiplexer that works like the niri window manager.
//! Each terminal is a column on a long horizontal strip. A new terminal never
//! shrinks the others; the view slides sideways to show it instead.

mod config;
mod pty;

use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::style::{
    Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::{cursor, queue, terminal};

use config::Config;
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// Widths that Alt+r cycles through, as a share of the screen.
const WIDTHS: [f32; 3] = [1.0 / 3.0, 0.5, 2.0 / 3.0];
/// Empty cells between two terminals.
const GAP: u16 = 1;
/// Smallest column: left border, one cell, right border.
const MIN_COL: u16 = 3;
/// Shortest time between two redraws (about 120 fps).
const FRAME: Duration = Duration::from_millis(8);
/// How many output chunks may wait for the main loop. When full, shells are
/// paused until we catch up, so a flood of output can't eat memory.
const QUEUE: usize = 64;
/// Stack size for the small per-pane helper threads.
const THREAD_STACK: usize = 64 * 1024;
/// Lines each pane keeps after they scroll off the top.
const SCROLLBACK: usize = 5000;
/// Lines one wheel step scrolls.
const WHEEL_LINES: usize = 3;
/// Mouse reporting we ask the user's terminal for: clicks, wheel and drags (1002), SGR form (1006).
/// Not all-motion (1003), which would flood us with events.
const MOUSE_ON: &str = "\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l";
/// Longest gap between clicks of a double or triple click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// What ends a word for double-click (same as Alacritty).
const WORD_SEP: &str = ",│`|:\"' ()[]{}<>\t";
/// How often the status bar re-reads each pane's folder and program. Programs
/// can start and stop without printing anything (`sleep 10`), so we can't wait for output.
const POLL: Duration = Duration::from_millis(500);
/// Longest folder name in the status bar; longer ones are cut with "…".
const LABEL_DIR: usize = 20;

/// What pane threads tell the main loop.
enum Ev {
    /// A shell printed something.
    Out(u64, Vec<u8>),
    /// A shell's terminal closed.
    Exit(u64),
    /// A key press or resize is waiting in the input queue.
    Wake,
}

/// One terminal: a shell running on a pty, plus the screen it has drawn.
struct Pane {
    /// Stable id; the list position changes when columns move.
    id: u64,
    /// Width as a share of the screen.
    frac: f32,
    parser: vt100::Parser<Clip>,
    master: File,
    child: Child,
    /// Key presses for this pane, written to its shell by a helper thread.
    input: Sender<Vec<u8>>,
    /// Status bar text, like "(nirix % vim)". Kept here so drawing never reads /proc.
    label: String,
}

/// Keeps what an app copied (OSC 52, already base64) so we can pass it to the real terminal.
#[derive(Default)]
struct Clip(Option<String>);

impl vt100::Callbacks for Clip {
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _ty: &[u8], data: &[u8]) {
        self.0 = Some(String::from_utf8_lossy(data).into_owned());
    }
}

/// Text picked with the mouse, in one pane. Lines count from the oldest
/// scrollback line, so the selection stays on its text while output scrolls.
/// Drifts once scrollback is full and old lines drop off.
#[derive(Clone, Copy)]
struct Sel {
    pane: u64,
    anchor: (usize, u16),
    /// None until the mouse moves: a plain click selects nothing.
    head: Option<(usize, u16)>,
}

impl Sel {
    /// Both ends in reading order, if anything is selected.
    fn range(&self) -> Option<((usize, u16), (usize, u16))> {
        let head = self.head?;
        Some((self.anchor.min(head), self.anchor.max(head)))
    }
}

/// Where the view was, and how fast it moved, when the current slide began.
struct Anim {
    pos: f32,
    vel: f32,
    start: Instant,
}

/// The most text one cell can hold (same limit vt100 uses).
const CELL_BYTES: usize = 22;

/// One screen cell. Text is stored inline so building a frame never allocates.
#[derive(Clone, Copy, PartialEq)]
struct Glyph {
    text: [u8; CELL_BYTES],
    /// Bytes used in `text`. 0 means "right half of a wide character".
    len: u8,
    fg: Color,
    bg: Color,
    attrs: u8,
}

const BOLD: u8 = 1;
const ITALIC: u8 = 2;
const UNDERLINE: u8 = 4;
const REVERSE: u8 = 8;

impl Glyph {
    const BLANK: Glyph = Glyph {
        text: [b' '; CELL_BYTES],
        len: 1,
        fg: Color::Reset,
        bg: Color::Reset,
        attrs: 0,
    };
    const WIDE_TAIL: Glyph = Glyph {
        len: 0,
        ..Glyph::BLANK
    };

    fn new(s: &str, fg: Color, bg: Color, attrs: u8) -> Self {
        let mut text = [0; CELL_BYTES];
        let len = s.len().min(CELL_BYTES);
        text[..len].copy_from_slice(&s.as_bytes()[..len]);
        Glyph {
            text,
            len: len as u8,
            fg,
            bg,
            attrs,
        }
    }

    fn plain(c: char, fg: Color) -> Self {
        Self::new(c.encode_utf8(&mut [0; 4]), fg, Color::Reset, 0)
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.text[..self.len as usize]).unwrap_or(" ")
    }
}

/// A full screen of cells. We keep the last one shown and only send cells that changed.
#[derive(Default)]
struct Frame {
    w: u16,
    h: u16,
    cells: Vec<Glyph>,
}

impl Frame {
    /// Blank the frame for the given size, reusing its memory.
    fn reset(&mut self, w: u16, h: u16) {
        (self.w, self.h) = (w, h);
        self.cells.clear();
        self.cells.resize(w as usize * h as usize, Glyph::BLANK);
    }

    /// Set one cell; anything off screen is ignored.
    fn put(&mut self, x: i32, y: i32, g: Glyph) {
        if (0..self.w as i32).contains(&x) && (0..self.h as i32).contains(&y) {
            self.cells[y as usize * self.w as usize + x as usize] = g;
        }
    }

    /// Write text, one char per cell.
    fn text(&mut self, x: i32, y: i32, s: &str, fg: Color) {
        for (i, c) in s.chars().enumerate() {
            self.put(x + i as i32, y, Glyph::plain(c, fg));
        }
    }

    /// Write a number; returns its width.
    fn number(&mut self, x: i32, y: i32, mut n: usize, fg: Color) -> i32 {
        let width = digits(n);
        for i in (0..width).rev() {
            self.put(
                x + i,
                y,
                Glyph::plain(char::from_digit((n % 10) as u32, 10).unwrap(), fg),
            );
            n /= 10;
        }
        width
    }
}

struct App {
    cfg: Config,
    panes: Vec<Pane>,
    /// Index of the focused pane in `panes`.
    focus: usize,
    /// Where the view should end up (in cells from the strip's left edge).
    view_x: u16,
    anim: Option<Anim>,
    /// What the screen shows now.
    prev: Frame,
    /// Spare frame, reused to draw the next one.
    next: Frame,
    w: u16,
    h: u16,
    next_id: u64,
    tx: SyncSender<Ev>,
    /// Error to show in the status bar until the next key press.
    notice: Option<String>,
    sel: Option<Sel>,
    /// Pane a mouse button went down in; its drags and release go there too.
    held: Option<u64>,
    /// Time, screen cell and count of the last left click, to spot double and triple clicks.
    last_click: Option<(Instant, (u16, u16), u8)>,
    /// Base64 text to put on the real terminal's clipboard with the next frame.
    clip: Option<String>,
}

/// Scroll as little as possible so the column at `col_x` (width `col_w`) fits on screen.
fn scroll_to(view_x: u16, col_x: u16, col_w: u16, screen_w: u16) -> u16 {
    if col_x < view_x {
        col_x
    } else if col_x + col_w > view_x + screen_w {
        (col_x + col_w).saturating_sub(screen_w)
    } else {
        view_x
    }
}

/// A spring that glides to `to` without bouncing (like niri).
/// Returns position and speed `t` seconds after starting at `pos` with speed `vel`.
fn spring(pos: f32, vel: f32, to: f32, omega: f32, t: f32) -> (f32, f32) {
    let c1 = pos - to;
    let c2 = vel + omega * c1;
    let e = (-omega * t).exp();
    (to + (c1 + c2 * t) * e, (c2 - omega * (c1 + c2 * t)) * e)
}

/// Start a helper thread with a small stack (the default is 2 MB each).
fn small_thread(f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    thread::Builder::new()
        .stack_size(THREAD_STACK)
        .spawn(f)
        .map(drop)
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap()
}

/// How many cells a number takes when written out.
fn digits(n: usize) -> i32 {
    n.checked_ilog10().unwrap_or(0) as i32 + 1
}

/// Which pane labels fit in `room` cells: a range around `focus`, growing
/// one step right then one step left so the focused label stays near the middle.
/// `w(i)` is how many cells label `i` takes, including the space after it.
fn label_window(len: usize, focus: usize, room: i32, w: impl Fn(usize) -> i32) -> (usize, usize) {
    let (mut lo, mut hi, mut used) = (focus, focus, w(focus));
    loop {
        let mut grew = false;
        if hi + 1 < len && used + w(hi + 1) <= room {
            hi += 1;
            used += w(hi);
            grew = true;
        }
        if lo > 0 && used + w(lo - 1) <= room {
            lo -= 1;
            used += w(lo);
            grew = true;
        }
        if !grew {
            return (lo, hi);
        }
    }
}

/// Status bar text for a pane, like "(nirix % vim)": the folder's last part
/// ("~" for home), then the program in front.
fn label(dir: Option<&Path>, home: Option<&Path>, name: &str) -> String {
    let dir = match dir {
        None => "?".into(),
        Some(d) if Some(d) == home => "~".into(),
        Some(d) => d
            .file_name()
            .map_or_else(|| d.to_string_lossy(), |n| n.to_string_lossy()),
    };
    let dir: String = if dir.chars().count() > LABEL_DIR {
        dir.chars().take(LABEL_DIR - 1).chain(['…']).collect()
    } else {
        dir.into_owned()
    };
    format!("({dir} % {name})")
}

/// Working folder of a process, if we may read it.
fn cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .filter(|d| d.is_dir())
}

/// The user's home folder, which the status bar shows as "~".
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Read a pane's label from /proc. None if its shell is already gone.
/// The folder falls back to the shell's when the program's is off limits (like sudo's).
fn describe(p: &Pane, home: Option<&Path>) -> Option<String> {
    let pid = pty::foreground(&p.master).unwrap_or(p.child.id());
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let dir = cwd(pid).or_else(|| cwd(p.child.id()));
    Some(label(dir.as_deref(), home, name.trim_end()))
}

impl App {
    /// Box width for a share of the screen. Rounds down so columns plus gaps always fit.
    fn frac_w(&self, frac: f32) -> u16 {
        (((self.w + GAP) as f32 * frac) as u16)
            .saturating_sub(GAP)
            .max(MIN_COL)
    }

    /// A single pane takes the whole screen; otherwise each uses its own width.
    fn col_w(&self, p: &Pane) -> u16 {
        if self.panes.len() == 1 {
            self.w.max(MIN_COL)
        } else {
            self.frac_w(p.frac)
        }
    }

    /// Left edge of pane `i` on the strip.
    fn col_x(&self, i: usize) -> u16 {
        self.panes[..i].iter().map(|p| self.col_w(p) + GAP).sum()
    }

    /// Rows inside a box: screen minus top border, bottom border and status bar.
    fn rows(&self) -> u16 {
        self.h.saturating_sub(3).max(1)
    }

    /// Spring speed that makes a slide finish in `animation_ms`.
    fn omega(&self) -> f32 {
        9.2 / self.cfg.animation_time.as_secs_f32()
    }

    fn animating(&self) -> bool {
        self.anim
            .as_ref()
            .is_some_and(|a| a.start.elapsed() < self.cfg.animation_time)
    }

    /// Where the view is right now, and how fast it is moving.
    fn cam(&self) -> (f32, f32) {
        match &self.anim {
            Some(a) if self.animating() => spring(
                a.pos,
                a.vel,
                self.view_x as f32,
                self.omega(),
                a.start.elapsed().as_secs_f32(),
            ),
            _ => (self.view_x as f32, 0.0),
        }
    }

    /// Start a shell in a new pane right of the focused one.
    fn spawn(&mut self) -> io::Result<()> {
        let frac = 0.5;
        let cols = if self.panes.is_empty() {
            self.w.max(MIN_COL)
        } else {
            self.frac_w(frac)
        } - 2;
        let rows = self.rows();
        // Start where the focused shell is, like a normal terminal's new window.
        let dir = self.panes.get(self.focus).and_then(|p| cwd(p.child.id()));
        let (master, mut child) = pty::spawn(&self.cfg.shell, rows, cols, dir)?;
        // Take the id even if the next step fails, so a late Exit for it can't hit another pane.
        let id = self.next_id;
        self.next_id += 1;
        let input = match self.start_io(id, &master) {
            Ok(input) => input,
            Err(e) => {
                pty::hangup(&child);
                let _ = child.wait();
                return Err(e);
            }
        };

        let parser =
            vt100::Parser::new_with_callbacks(nz(rows), nz(cols), SCROLLBACK, Clip::default());
        let mut pane = Pane {
            id,
            frac,
            parser,
            master,
            child,
            input,
            label: String::new(),
        };
        // `Command::spawn` returns after exec, so this already names the shell.
        pane.label = describe(&pane, home().as_deref()).unwrap_or_default();
        let at = if self.panes.is_empty() {
            0
        } else {
            self.focus + 1
        };
        self.panes.insert(at, pane);
        self.focus = at;
        Ok(())
    }

    /// Re-read every pane's folder and program. Returns true if any label changed.
    fn refresh(&mut self) -> bool {
        let home = home();
        let mut changed = false;
        for p in &mut self.panes {
            if let Some(l) = describe(p, home.as_deref())
                && l != p.label
            {
                p.label = l;
                changed = true;
            }
        }
        changed
    }

    /// Start a pane's two helper threads: one passes the shell's output to the
    /// main loop, one writes our key presses to the shell. Writing on a separate
    /// thread means a shell that stops reading can never freeze the main loop.
    fn start_io(&self, id: u64, master: &File) -> io::Result<Sender<Vec<u8>>> {
        let mut reader = master.try_clone()?;
        let mut writer = master.try_clone()?;
        let tx = self.tx.clone();
        small_thread(move || {
            let mut buf = [0u8; 8192];
            // `send` waits while the queue is full; the shell is paused meanwhile.
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if tx.send(Ev::Out(id, buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            let _ = tx.send(Ev::Exit(id));
        })?;
        let (input, keys) = mpsc::channel::<Vec<u8>>();
        small_thread(move || {
            // Ends when the pane is dropped (its sender goes away) or the pty closes.
            for mut bytes in keys.iter() {
                // Merge whatever else is queued into one write.
                for more in keys.try_iter() {
                    bytes.extend(more);
                }
                if writer.write_all(&bytes).is_err() {
                    return;
                }
            }
        })?;
        Ok(input)
    }

    /// Drop a pane whose shell is gone; focus moves to its left neighbour.
    fn remove(&mut self, id: u64) {
        let Some(i) = self.panes.iter().position(|p| p.id == id) else {
            return;
        };
        let mut p = self.panes.remove(i);
        let _ = p.child.wait();
        if i < self.focus || self.focus >= self.panes.len() {
            self.focus = self.focus.saturating_sub(1);
        }
    }

    /// After any change: tell each shell its size, and slide the view to the focused pane.
    fn sync(&mut self) {
        let rows = self.rows();
        for i in 0..self.panes.len() {
            let cols = self.col_w(&self.panes[i]) - 2;
            let p = &mut self.panes[i];
            let (r, c) = p.parser.screen().size();
            if (r.get(), c.get()) != (rows, cols) {
                // Its lines are about to move.
                if self.sel.is_some_and(|s| s.pane == p.id) {
                    self.sel = None;
                }
                // Full-screen apps redraw themselves; shell output is re-wrapped.
                if c.get() != cols && !p.parser.screen().alternate_screen() {
                    p.parser = reflow(&mut p.parser, rows, cols);
                } else {
                    p.parser.set_size(nz(rows), nz(cols));
                }
                let _ = pty::resize(&p.master, rows, cols);
            }
        }
        if self.panes.is_empty() {
            return;
        }
        // Don't scroll past the end of the strip (the last pane has no gap after it).
        let total = self.col_x(self.panes.len()) - GAP;
        let mut view_x = self.view_x.min(total.saturating_sub(self.w));
        let (x, w) = (self.col_x(self.focus), self.col_w(&self.panes[self.focus]));
        view_x = scroll_to(view_x, x, w, self.w);
        if view_x != self.view_x {
            if self.cfg.animation && !self.cfg.animation_time.is_zero() {
                // Start from the current position and speed, so a new slide
                // during an old one continues smoothly instead of jumping.
                let (pos, vel) = self.cam();
                self.anim = Some(Anim {
                    pos,
                    vel,
                    start: Instant::now(),
                });
            }
            self.view_x = view_x;
        }
    }

    /// Handle an Alt shortcut. Returns false for other keys so they reach the shell.
    fn shortcut(&mut self, k: KeyEvent) -> bool {
        if !k.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let n = self.panes.len();
        match k.code {
            KeyCode::Enter | KeyCode::Char('n') => {
                // Too many terminals (or another system limit): say so, keep running.
                if let Err(e) = self.spawn() {
                    self.notice = Some(format!("can't open terminal: {e}"));
                }
            }
            KeyCode::Left if shift => self.swap(-1),
            KeyCode::Right if shift => self.swap(1),
            KeyCode::Char('H') => self.swap(-1),
            KeyCode::Char('L') => self.swap(1),
            KeyCode::Left | KeyCode::Char('h') => self.focus = self.focus.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => self.focus = (self.focus + 1).min(n - 1),
            KeyCode::Char(c @ '1'..='9') => self.focus = (c as usize - '1' as usize).min(n - 1),
            KeyCode::Char('r') => {
                let p = &mut self.panes[self.focus];
                p.frac = *WIDTHS
                    .iter()
                    .find(|&&f| f > p.frac + 0.01)
                    .unwrap_or(&WIDTHS[0]);
            }
            KeyCode::Char('m') => {
                let p = &mut self.panes[self.focus];
                p.frac = if p.frac >= 1.0 { 0.5 } else { 1.0 };
            }
            // The pane goes away when its terminal closes (Ev::Exit).
            KeyCode::Char('x') => pty::hangup(&self.panes[self.focus].child),
            _ => return false,
        }
        true
    }

    /// Move the focused column one place left (-1) or right (1).
    fn swap(&mut self, dir: isize) {
        let to = self.focus.wrapping_add_signed(dir);
        if to < self.panes.len() {
            self.panes.swap(self.focus, to);
            self.focus = to;
        }
    }

    fn key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        self.notice = None;
        self.sel = None;
        if self.shortcut(k) {
            return;
        }
        let p = &mut self.panes[self.focus];
        p.parser.screen_mut().set_scrollback(0);
        let _ = p
            .input
            .send(encode(k, p.parser.screen().application_cursor()));
    }

    /// Send pasted text in one go. Wrap it in paste markers if the app asked
    /// for them (editors use this to avoid auto-indenting pasted code).
    fn paste(&mut self, text: String) {
        self.notice = None;
        self.sel = None;
        let p = &mut self.panes[self.focus];
        p.parser.screen_mut().set_scrollback(0);
        let bytes = if p.parser.screen().bracketed_paste() {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        };
        let _ = p.input.send(bytes);
    }

    /// Cell (`col`, `row`) of pane `i` under screen cell (`x`, `y`). Can be outside the pane.
    fn cell_in(&self, i: usize, x: u16, y: u16) -> (i32, i32) {
        let cam = self.cam().0.round() as i32;
        (x as i32 - (self.col_x(i) as i32 - cam) - 1, y as i32 - 1)
    }

    /// The pane under screen cell (`x`, `y`), and the cell inside it.
    fn pane_at(&self, x: u16, y: u16) -> Option<(usize, u16, u16)> {
        (0..self.panes.len()).find_map(|i| {
            let (c, r) = self.cell_in(i, x, y);
            let inner = self.col_w(&self.panes[i]) as i32 - 2;
            ((0..inner).contains(&c) && (0..self.rows() as i32).contains(&r))
                .then_some((i, c as u16, r as u16))
        })
    }

    /// Mouse over a pane. A click focuses it. Apps that asked for the mouse get
    /// it. Otherwise dragging selects text (copied on release), and the wheel
    /// scrolls: arrow keys for full-screen apps (like xterm), scrollback for shells.
    fn mouse(&mut self, m: MouseEvent) {
        let (i, col, row) = match m.kind {
            // Drags and the release go to the pane the button went down in, even past its edge.
            MouseEventKind::Drag(_) | MouseEventKind::Up(_) => {
                let Some(i) = self.panes.iter().position(|p| Some(p.id) == self.held) else {
                    return;
                };
                let (c, r) = self.cell_in(i, m.column, m.row);
                (i, c, r)
            }
            _ => {
                let Some((i, c, r)) = self.pane_at(m.column, m.row) else {
                    return;
                };
                (i, c as i32, r as i32)
            }
        };
        let rows = self.rows() as i32;
        let c = col.clamp(0, self.col_w(&self.panes[i]) as i32 - 3) as u16;
        let r = row.clamp(0, rows - 1) as u16;
        match m.kind {
            MouseEventKind::Down(b) => {
                self.held = Some(self.panes[i].id);
                if b == MouseButton::Left {
                    self.focus = i;
                    self.sel = None;
                }
            }
            MouseEventKind::Up(_) => self.held = None,
            _ => {}
        }
        let p = &mut self.panes[i];
        let screen = p.parser.screen();
        // Ctrl+click opens a link, even in apps that use the mouse.
        if m.kind == MouseEventKind::Down(MouseButton::Left)
            && m.modifiers.contains(KeyModifiers::CONTROL)
            && let Some(url) = url_at(screen, r, c)
        {
            if let Err(e) = open_url(&url) {
                self.notice = Some(format!("can't open link: {e}"));
            }
            return;
        }
        let mode = screen.mouse_protocol_mode();
        if mode != MouseProtocolMode::None {
            // Mode Press wants no releases; only the motion modes want drags.
            let skip = match m.kind {
                MouseEventKind::Up(_) => mode == MouseProtocolMode::Press,
                MouseEventKind::Drag(_) => matches!(
                    mode,
                    MouseProtocolMode::Press | MouseProtocolMode::PressRelease
                ),
                _ => false,
            };
            let sgr = screen.mouse_protocol_encoding() == MouseProtocolEncoding::Sgr;
            if let Some(bytes) = encode_mouse(m, c, r, sgr).filter(|_| !skip) {
                let _ = p.input.send(bytes);
            }
            return;
        }
        let id = p.id;
        let s = p.parser.screen_mut();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let n = match self.last_click {
                    Some((t, at, n)) if at == (m.column, m.row) && t.elapsed() < DOUBLE_CLICK => {
                        n % 3 + 1
                    }
                    _ => 1,
                };
                self.last_click = Some((Instant::now(), (m.column, m.row), n));
                let top = line_of(s, 0);
                let (anchor, head) = match n {
                    2 => {
                        let (a, b) = word_at(s, r, c);
                        ((top + r as usize, a), Some((top + r as usize, b)))
                    }
                    3 => {
                        let (a, b) = line_rows(s, r);
                        (
                            (top + a as usize, 0),
                            Some((top + b as usize, s.size().1.get() - 1)),
                        )
                    }
                    _ => ((top + r as usize, c), None),
                };
                self.sel = Some(Sel {
                    pane: id,
                    anchor,
                    head,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Past the top or bottom edge: scroll the history along.
                // One line per mouse move; add a timer if that feels slow.
                if row < 0 {
                    s.set_scrollback(s.scrollback() + 1);
                } else if row >= rows {
                    s.set_scrollback(s.scrollback().saturating_sub(1));
                }
                if let Some(sel) = self.sel.as_mut().filter(|x| x.pane == id) {
                    sel.head = Some((line_of(s, r), c));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                match self.sel.filter(|x| x.pane == id).and_then(|x| x.range()) {
                    Some((a, b)) => self.clip = Some(base64(selected_text(s, a, b).as_bytes())),
                    None => self.sel = None,
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if s.alternate_screen() {
                    let key = encode(
                        KeyEvent::new(
                            if up { KeyCode::Up } else { KeyCode::Down },
                            KeyModifiers::NONE,
                        ),
                        s.application_cursor(),
                    );
                    let _ = p.input.send(key.repeat(WHEEL_LINES));
                } else {
                    let n = s.scrollback();
                    s.set_scrollback(if up {
                        n + WHEEL_LINES
                    } else {
                        n.saturating_sub(WHEEL_LINES)
                    });
                }
            }
            _ => {}
        }
    }

    fn handle(&mut self, e: Ev) {
        match e {
            Ev::Out(id, bytes) => {
                if let Some(p) = self.panes.iter_mut().find(|p| p.id == id) {
                    p.parser.process(&bytes);
                    if let Some(data) = p.parser.callbacks_mut().0.take() {
                        self.clip = Some(data);
                    }
                }
            }
            Ev::Exit(id) => self.remove(id),
            Ev::Wake => {}
        }
    }

    fn input(&mut self, e: Event) {
        match e {
            Event::Key(k) if !self.panes.is_empty() => self.key(k),
            Event::Paste(text) if !self.panes.is_empty() => self.paste(text),
            Event::Mouse(m) if !self.panes.is_empty() => self.mouse(m),
            Event::Resize(w, h) => (self.w, self.h) = (w, h),
            _ => {}
        }
    }

    /// Draw borders, terminal contents and the status bar into `f`.
    /// `cam` is the view position, so panes can be partly off screen.
    fn draw(&self, f: &mut Frame, cam: i32) {
        f.reset(self.w, self.h);
        let rows = self.rows() as i32;
        let sw = self.w as i32;
        let mut col_x = 0i32;
        for (i, p) in self.panes.iter().enumerate() {
            let w = self.col_w(p) as i32;
            let x0 = col_x - cam;
            col_x += w + GAP as i32;
            if x0 + w <= 0 || x0 >= sw {
                continue; // fully off screen
            }

            let fg = if i == self.focus {
                self.cfg.focus_color
            } else {
                self.cfg.border_color
            };
            let (lx, rx) = (x0, x0 + w - 1);
            for (y, l, r) in [(0, '┌', '┐'), (rows + 1, '└', '┘')] {
                f.put(lx, y, Glyph::plain(l, fg));
                f.put(rx, y, Glyph::plain(r, fg));
                for x in lx + 1..rx {
                    f.put(x, y, Glyph::plain('─', fg));
                }
            }
            for y in 1..=rows {
                f.put(lx, y, Glyph::plain('│', fg));
                f.put(rx, y, Glyph::plain('│', fg));
            }

            let screen = p.parser.screen();
            let top = line_of(screen, 0);
            let sel = self.sel.filter(|s| s.pane == p.id).and_then(|s| s.range());
            let inner = w - 2;
            for y in 0..rows {
                for c in 0..inner {
                    let sx = x0 + 1 + c;
                    if !(0..sw).contains(&sx) {
                        continue;
                    }
                    let Some(cell) = screen.cell(y as u16, c as u16) else {
                        break;
                    };
                    if cell.is_wide_continuation() {
                        // Its left half draws this cell. If the left half is off
                        // screen, leave a blank here instead.
                        if sx > 0 {
                            f.put(sx, y + 1, Glyph::WIDE_TAIL);
                        }
                        continue;
                    }
                    let selected =
                        sel.is_some_and(|(a, b)| (a..=b).contains(&(top + y as usize, c as u16)));
                    let attrs = (cell.bold() as u8 * BOLD)
                        | (cell.italic() as u8 * ITALIC)
                        | (cell.underline() as u8 * UNDERLINE)
                        | ((cell.inverse() != selected) as u8 * REVERSE);
                    // A wide character with no room for its right half becomes a blank.
                    let cut = cell.is_wide() && (c + 1 >= inner || sx + 1 >= sw);
                    let s = if cut || cell.contents().is_empty() {
                        " "
                    } else {
                        cell.contents()
                    };
                    f.put(
                        sx,
                        y + 1,
                        Glyph::new(s, color(cell.fgcolor()), color(cell.bgcolor()), attrs),
                    );
                }
            }
        }

        // Status bar: each pane's folder and program, focused one highlighted.
        // If they don't all fit, show the ones around the focus and "+N" for
        // how many are hidden on each side.
        // One char per cell, so wide or combining chars in a folder
        // name (CJK, Devanagari vowel signs) shift the rest of the bar. Doing
        // better needs unicode width tables, which we don't pull in.
        let y = self.h as i32 - 1;
        let n = self.panes.len();
        // Room for the "+N " counts on both sides.
        let counts = 2 * (digits(n) + 2);
        let width = |i: usize| self.panes[i].label.chars().count() as i32 + 1;
        let (lo, hi) = label_window(n, self.focus, sw - 2 - counts, width);
        let dim = self.cfg.border_color;
        let mut x = 1;
        if lo > 0 {
            f.put(x, y, Glyph::plain('+', dim));
            x += f.number(x + 1, y, lo, dim) + 2;
        }
        for i in lo..=hi {
            let fg = if i == self.focus {
                self.cfg.focus_color
            } else {
                dim
            };
            f.text(x, y, &self.panes[i].label, fg);
            x += width(i);
        }
        if hi + 1 < n {
            f.put(x, y, Glyph::plain('+', dim));
            x += f.number(x + 1, y, n - 1 - hi, dim) + 2;
        }
        if let Some(msg) = &self.notice {
            f.text(x + 1, y, msg, self.cfg.focus_color);
        }
    }

    /// Draw a new frame and send only the cells that changed since the last one.
    fn render(&mut self, out: &mut impl Write) -> io::Result<()> {
        let cam = self.cam().0.round() as i32;
        let mut f = std::mem::take(&mut self.next);
        self.draw(&mut f, cam);
        queue!(out, terminal::BeginSynchronizedUpdate, cursor::Hide)?;
        if (self.prev.w, self.prev.h) != (f.w, f.h) {
            // New size: clear everything and repaint from scratch.
            queue!(
                out,
                SetAttribute(Attribute::Reset),
                terminal::Clear(terminal::ClearType::All)
            )?;
            self.prev.reset(f.w, f.h);
        }

        let mut style = None;
        // Where the terminal's cursor is after the last print, to skip needless moves.
        let mut at = None;
        for (i, g) in f.cells.iter().enumerate() {
            if g.len == 0 || self.prev.cells[i] == *g {
                continue;
            }
            let (x, y) = ((i % f.w as usize) as u16, (i / f.w as usize) as u16);
            if at != Some((x, y)) {
                queue!(out, cursor::MoveTo(x, y))?;
            }
            let st = (g.fg, g.bg, g.attrs);
            if style != Some(st) {
                queue!(
                    out,
                    SetAttribute(Attribute::Reset),
                    SetForegroundColor(g.fg),
                    SetBackgroundColor(g.bg)
                )?;
                for (bit, a) in [
                    (BOLD, Attribute::Bold),
                    (ITALIC, Attribute::Italic),
                    (UNDERLINE, Attribute::Underlined),
                    (REVERSE, Attribute::Reverse),
                ] {
                    if g.attrs & bit != 0 {
                        queue!(out, SetAttribute(a))?;
                    }
                }
                style = Some(st);
            }
            queue!(out, Print(g.as_str()))?;
            // A wide character moves the cursor two cells.
            let wide = f.cells.get(i + 1).is_some_and(|n| n.len == 0) && x + 1 < f.w;
            at = Some((x + 1 + wide as u16, y));
        }
        queue!(out, SetAttribute(Attribute::Reset))?;

        // Put the real cursor where the focused shell's cursor is, if visible.
        if let Some(p) = self.panes.get(self.focus) {
            let s = p.parser.screen();
            let (cy, cx) = s.cursor_position();
            let sx = self.col_x(self.focus) as i32 + 1 + cx as i32 - cam;
            if !s.hide_cursor() && s.scrollback() == 0 && (0..self.w as i32).contains(&sx) {
                queue!(out, cursor::MoveTo(sx as u16, cy + 1), cursor::Show)?;
            }
        }
        // Copied text goes to the real terminal's clipboard (OSC 52).
        if let Some(data) = self.clip.take() {
            queue!(out, Print(format!("\x1b]52;c;{data}\x07")))?;
        }
        queue!(out, terminal::EndSynchronizedUpdate)?;
        // The new frame is now on screen; the old one becomes the spare.
        self.next = std::mem::replace(&mut self.prev, f);
        out.flush()
    }
}

fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::AnsiValue(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb { r, g, b },
    }
}

/// Turn a key press into the bytes a normal terminal (xterm) would send to the shell.
fn encode(k: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let m = k.modifiers;
    let mut out = Vec::new();
    if m.contains(KeyModifiers::ALT) {
        out.push(0x1b);
    }
    // xterm modifier number: 1 + shift + 2*alt + 4*ctrl
    let mods = 1
        + m.contains(KeyModifiers::SHIFT) as u8
        + 2 * m.contains(KeyModifiers::ALT) as u8
        + 4 * m.contains(KeyModifiers::CONTROL) as u8;
    // Arrows, Home and End. Apps like vim ask for the "application" form.
    let csi = |c: char| -> Vec<u8> {
        if mods > 1 {
            format!("\x1b[1;{mods}{c}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let tilde = |n: u8| format!("\x1b[{n}~").into_bytes();
    let seq: Vec<u8> = match k.code {
        KeyCode::Char(c) if m.contains(KeyModifiers::CONTROL) => match c.to_ascii_uppercase() {
            ' ' | '@' => vec![0],
            '?' => vec![0x7f],
            u @ 'A'..='_' => vec![u as u8 & 0x1f],
            _ => c.to_string().into_bytes(),
        },
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => return csi('A'),
        KeyCode::Down => return csi('B'),
        KeyCode::Right => return csi('C'),
        KeyCode::Left => return csi('D'),
        KeyCode::Home => return csi('H'),
        KeyCode::End => return csi('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => format!("\x1bO{}", (b'O' + n) as char).into_bytes(),
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        _ => vec![],
    };
    out.extend(seq);
    out
}

/// Turn a mouse event at a pane cell (`col`, `row`, from 0) into the bytes
/// xterm would send. None for events we don't pass on.
fn encode_mouse(m: MouseEvent, col: u16, row: u16, sgr: bool) -> Option<Vec<u8>> {
    let btn = |b| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (code, release) = match m.kind {
        MouseEventKind::Down(b) => (btn(b), false),
        MouseEventKind::Up(b) => (btn(b), true),
        MouseEventKind::Drag(b) => (btn(b) + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        _ => return None,
    };
    let mods = m.modifiers;
    let code = code
        + 4 * mods.contains(KeyModifiers::SHIFT) as u8
        + 8 * mods.contains(KeyModifiers::ALT) as u8
        + 16 * mods.contains(KeyModifiers::CONTROL) as u8;
    Some(if sgr {
        format!(
            "\x1b[<{code};{};{}{}",
            col + 1,
            row + 1,
            if release { 'm' } else { 'M' }
        )
        .into_bytes()
    } else {
        // The old form can't say which button was let go, and can't reach past cell 222.
        let code = if release { code | 3 } else { code };
        let pos = |n: u16| n.min(222) as u8 + 33;
        vec![0x1b, b'[', b'M', code + 32, pos(col), pos(row)]
    })
}

/// SGR code that sets exactly this cell's colors and style.
fn sgr(c: &vt100::Cell) -> String {
    let mut out = String::from("\x1b[0");
    for (on, code) in [
        (c.bold(), ";1"),
        (c.dim(), ";2"),
        (c.italic(), ";3"),
        (c.underline(), ";4"),
        (c.inverse(), ";7"),
    ] {
        if on {
            out.push_str(code);
        }
    }
    for (color, base) in [(c.fgcolor(), 38), (c.bgcolor(), 48)] {
        match color {
            vt100::Color::Default => {}
            vt100::Color::Idx(i) => out.push_str(&format!(";{base};5;{i}")),
            vt100::Color::Rgb(r, g, b) => out.push_str(&format!(";{base};2;{r};{g};{b}")),
        }
    }
    out.push('m');
    out
}

/// Rebuild a screen at a new size so long lines wrap to the new width, like a
/// real terminal. vt100 only cuts or pads rows, so we replay the old text
/// (scrollback included) into a new parser.
fn reflow<CB: vt100::Callbacks + Default>(
    old: &mut vt100::Parser<CB>,
    rows: u16,
    cols: u16,
) -> vt100::Parser<CB> {
    let s = old.screen_mut();
    let (h, w) = (s.size().0.get() as usize, s.size().1.get());
    let sb = s.scrollback_len();
    let (cy, cx) = s.cursor_position();
    let cursor = sb + cy as usize;
    let mut text = String::new();
    // Where the cursor is in `text`.
    let mut split = 0;
    // Line ends not written yet; those after the last text are dropped.
    let mut breaks = 0;
    let mut style = None;
    // Walk the old rows a screenful at a time; the last page is the live screen.
    let mut top = 0;
    while top < sb + h {
        let first = top.min(sb);
        s.set_scrollback(sb - first);
        for r in (top - first) as u16..h as u16 {
            let at_cursor = first + r as usize == cursor;
            let wrapped = s.row_wrapped(r);
            let shown = |c| {
                s.cell(r, c).is_some_and(|x| {
                    x.has_contents() || x.bgcolor() != vt100::Color::Default || x.inverse()
                })
            };
            let end = if wrapped {
                w
            } else {
                (0..w).rev().find(|&c| shown(c)).map_or(0, |c| c + 1)
            };
            for c in 0..end.max(if at_cursor { cx + 1 } else { 0 }) {
                if at_cursor && c == cx {
                    text.push_str(&"\r\n".repeat(std::mem::take(&mut breaks)));
                    split = text.len();
                    if c >= end {
                        break;
                    }
                }
                let Some(cell) = s.cell(r, c) else { break };
                if cell.is_wide_continuation() {
                    continue;
                }
                text.push_str(&"\r\n".repeat(std::mem::take(&mut breaks)));
                let key = (
                    cell.fgcolor(),
                    cell.bgcolor(),
                    cell.bold(),
                    cell.dim(),
                    cell.italic(),
                    cell.underline(),
                    cell.inverse(),
                );
                if style != Some(key) {
                    text.push_str(&sgr(cell));
                    style = Some(key);
                }
                text.push_str(if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                });
            }
            if !wrapped {
                breaks += 1;
            }
        }
        top = first + h;
    }
    s.set_scrollback(0);

    let mut new = vt100::Parser::new_with_callbacks(nz(rows), nz(cols), SCROLLBACK, CB::default());
    new.process(&text.as_bytes()[..split]);
    let (r, c) = new.screen().cursor_position();
    let before = new.screen().scrollback_len();
    new.process(&text.as_bytes()[split..]);
    // Text after the cursor may have scrolled the screen up.
    // Miscounts once scrollback is full; only matters for text below the cursor.
    let r = (r as usize).saturating_sub(new.screen().scrollback_len() - before);
    let hide = if s.hide_cursor() { "\x1b[?25l" } else { "" };
    let restore = format!(
        "\x1b[0m{}{}{hide}\x1b[{};{}H",
        s.attributes_formatted(),
        s.input_mode_formatted(),
        r + 1,
        c + 1
    );
    new.process(restore.as_bytes());
    new
}

/// Which line (counted from the oldest scrollback line) screen row `row` shows.
fn line_of(s: &vt100::Screen, row: u16) -> usize {
    s.scrollback_len() - s.scrollback() + row as usize
}

/// First and last screen row of the line `row` is part of, following wraps.
fn line_rows(s: &vt100::Screen, row: u16) -> (u16, u16) {
    let h = s.size().0.get();
    let mut top = row;
    while top > 0 && s.row_wrapped(top - 1) {
        top -= 1;
    }
    let mut bot = row;
    while bot + 1 < h && s.row_wrapped(bot) {
        bot += 1;
    }
    (top, bot)
}

/// First and last column of the word at (`row`, `col`). Off a word, just that cell.
fn word_at(s: &vt100::Screen, row: u16, col: u16) -> (u16, u16) {
    let w = s.size().1.get();
    let word = |c| {
        s.cell(row, c).is_some_and(|x| {
            x.is_wide_continuation() || (x.has_contents() && !WORD_SEP.contains(x.contents()))
        })
    };
    if !word(col) {
        return (col, col);
    }
    let start = (0..col).rev().find(|&c| !word(c)).map_or(0, |c| c + 1);
    let end = (col + 1..w).find(|&c| !word(c)).map_or(w - 1, |c| c - 1);
    (start, end)
}

/// Text from `a` to `b` (both included, `a` first), like a normal terminal
/// copies it: wrapped rows join up and spaces at line ends are dropped.
fn selected_text(s: &mut vt100::Screen, a: (usize, u16), b: (usize, u16)) -> String {
    let (sb, keep, w) = (s.scrollback_len(), s.scrollback(), s.size().1.get());
    let mut out = String::new();
    for line in a.0..=b.0 {
        // Bring the line on screen: the top row while it is in the history.
        let (off, row) = if line < sb {
            (sb - line, 0)
        } else {
            (0, line - sb)
        };
        s.set_scrollback(off);
        let c0 = if line == a.0 { a.1 } else { 0 };
        let c1 = if line == b.0 { b.1 + 1 } else { w };
        let text = s
            .rows(c0, c1.saturating_sub(c0))
            .nth(row)
            .unwrap_or_default();
        let wrapped = s.row_wrapped(row as u16);
        out.push_str(if wrapped { &text } else { text.trim_end() });
        if line != b.0 && !wrapped {
            out.push('\n');
        }
    }
    s.set_scrollback(keep);
    out
}

/// Standard base64, for OSC 52.
fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | ((b as u32) << (16 - 8 * i)));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ABC[((n >> (18 - 6 * i)) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// The link under screen cell (`row`, `col`), following lines that wrap.
fn url_at(s: &vt100::Screen, row: u16, col: u16) -> Option<String> {
    let w = s.size().1.get();
    let (top, bot) = line_rows(s, row);
    // One byte per cell; anything that can't be part of a link becomes a space.
    let line: Vec<u8> = (top..=bot)
        .flat_map(|r| (0..w).map(move |c| s.cell(r, c).map(vt100::Cell::contents)))
        .map(|t| match t.map(str::as_bytes) {
            Some(&[b]) if b.is_ascii_graphic() && !b"<>\"'`{}|\\^".contains(&b) => b,
            _ => b' ',
        })
        .collect();
    let at = (row - top) as usize * w as usize + col as usize;
    if line.get(at).is_none_or(|&b| b == b' ') {
        return None;
    }
    let start = line[..at]
        .iter()
        .rposition(|&b| b == b' ')
        .map_or(0, |i| i + 1);
    let end = line[at..]
        .iter()
        .position(|&b| b == b' ')
        .map_or(line.len(), |i| at + i);
    let word = std::str::from_utf8(&line[start..end]).ok()?;
    let i = word.find("://")?;
    let scheme = word[..i]
        .rfind(|c: char| !c.is_ascii_alphanumeric())
        .map_or(0, |j| j + 1);
    let url = word[scheme..].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']']);
    (i > scheme && url.len() > i - scheme + 3).then(|| url.to_string())
}

/// Open a link with the desktop's default app, in the background.
fn open_url(url: &str) -> io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap it so it doesn't linger as a zombie.
    small_thread(move || {
        let _ = child.wait();
    })
}

/// Give the user's terminal back in its normal state.
fn restore() {
    let _ = terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        io::stdout(),
        Print(MOUSE_OFF),
        event::DisableBracketedPaste,
        terminal::LeaveAlternateScreen,
        cursor::Show
    );
}

fn main() -> io::Result<()> {
    let cfg = config::load().unwrap_or_else(|e| {
        eprintln!("nirix: {e}");
        std::process::exit(1);
    });
    let (w, h) = terminal::size()?;
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let mut app = App {
        cfg,
        panes: vec![],
        focus: 0,
        view_x: 0,
        anim: None,
        prev: Frame::default(),
        next: Frame::default(),
        w,
        h,
        next_id: 0,
        tx: tx.clone(),
        notice: None,
        sel: None,
        held: None,
        last_click: None,
        clip: None,
    };
    app.spawn()?;

    terminal::enable_raw_mode()?;
    crossterm::execute!(
        io::stdout(),
        terminal::EnterAlternateScreen,
        event::EnableBracketedPaste,
        Print(MOUSE_ON)
    )?;
    // Fix the terminal even if we crash.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));
    // Keys get their own queue, so they never wait behind a pile of output.
    let (keys_tx, keys) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(e) = event::read() {
            if keys_tx.send(e).is_err() {
                return;
            }
            // Wake the main loop. If the queue is full it is awake anyway.
            let _ = tx.try_send(Ev::Wake);
        }
    });

    let mut out = io::BufWriter::with_capacity(1 << 16, io::stdout());
    let result = (|| -> io::Result<()> {
        // Draw at most once per FRAME. Each pass handles one key and one
        // output chunk, so neither a flood of output nor a long paste can
        // stop the screen from updating or the other from being handled.
        let mut dirty = true;
        let mut last_draw: Option<Instant> = None;
        let mut next_poll = Instant::now();
        loop {
            // Keys first, so they never wait behind output.
            let got_key = match keys.try_recv() {
                Ok(e) => {
                    app.input(e);
                    dirty = true;
                    true
                }
                Err(_) => false,
            };
            if app.panes.is_empty() {
                break; // last shell exited
            }
            let now = Instant::now();
            if now >= next_poll {
                dirty |= app.refresh();
                next_poll = now + POLL;
            }
            let due = last_draw.map_or(now, |t| t + FRAME);
            let busy = dirty || app.animating();
            if busy && now >= due {
                app.sync();
                app.render(&mut out)?;
                last_draw = Some(now);
                dirty = false;
                continue;
            }
            match rx.try_recv() {
                Ok(e) => {
                    app.handle(e);
                    dirty = true;
                    continue;
                }
                Err(TryRecvError::Empty) if got_key => continue,
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => break,
            }
            // Nothing waiting: sleep until something arrives, the next draw is
            // due, or it's time to re-read the status bar labels.
            let wake = if busy { due.min(next_poll) } else { next_poll };
            let ev = match rx.recv_timeout(wake.saturating_duration_since(now)) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            app.handle(ev);
            dirty = true;
        }
        Ok(())
    })();
    restore();
    result
}

#[cfg(test)]
mod tests {
    use super::{
        Frame, Glyph, base64, encode_mouse, label, label_window, line_of, nz, reflow, scroll_to,
        selected_text, spring, url_at, word_at,
    };
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use std::path::Path;

    fn rows(p: &vt100::Parser) -> Vec<String> {
        p.screen()
            .rows(0, 100)
            .map(|r| r.trim_end().to_string())
            .collect()
    }

    #[test]
    fn reflows() {
        let mut p = vt100::Parser::new(nz(3), nz(10), 100);
        p.process(b"1\r\n2\r\n3\r\nabcdefghijklmno\r\n$ ");
        // wider: the long line joins up, one old line goes back on screen
        let mut q = reflow(&mut p, 3, 20);
        assert_eq!(rows(&q), ["3", "abcdefghijklmno", "$"]);
        assert_eq!(q.screen().cursor_position(), (2, 2));
        assert_eq!(q.screen().scrollback_len(), 2);
        // narrower: it wraps again
        let q = reflow(&mut q, 3, 4);
        assert_eq!(rows(&q), ["ijkl", "mno", "$"]);
        assert_eq!(q.screen().cursor_position(), (2, 2));
    }

    #[test]
    fn links() {
        let mut p = vt100::Parser::new(nz(3), nz(10), 0);
        p.process(b"see https://ex.com/abc. ok");
        let s = p.screen();
        // the link wraps over three rows; any part of it works
        assert_eq!(url_at(s, 1, 3).as_deref(), Some("https://ex.com/abc"));
        assert_eq!(url_at(s, 0, 5).as_deref(), Some("https://ex.com/abc"));
        assert_eq!(url_at(s, 0, 0), None);
        assert_eq!(url_at(s, 0, 3), None);
    }

    #[test]
    fn selection() {
        let mut p = vt100::Parser::new(nz(3), nz(10), 100);
        p.process(b"one  \r\ntwo\r\nabcdefghijklmn\r\n$ ");
        let s = p.screen_mut();
        // lines: 0 "one", 1 "two" (history), 2+3 one wrapped line, 4 "$"
        assert_eq!(selected_text(s, (0, 1), (3, 1)), "ne\ntwo\nabcdefghijkl");
        assert_eq!(s.scrollback(), 0);
        s.set_scrollback(1);
        assert_eq!(line_of(s, 0), 1);

        let mut p = vt100::Parser::new(nz(1), nz(20), 0);
        p.process(b"foo bar:baz");
        assert_eq!(word_at(p.screen(), 0, 5), (4, 6));
        assert_eq!(word_at(p.screen(), 0, 9), (8, 10));
        assert_eq!(word_at(p.screen(), 0, 3), (3, 3));

        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"Man"), "TWFu");
    }

    #[test]
    fn labels_window() {
        // labels of mixed widths, 8 to 17 cells
        let w = |i: usize| 8 + (i % 10) as i32;
        // everything fits
        assert_eq!(label_window(3, 1, 80, w), (0, 2));
        // 40 panes, room for about 3: focus 20 stays inside
        let (lo, hi) = label_window(40, 20, 40, w);
        assert!(lo <= 20 && 20 <= hi && lo > 0 && hi < 39);
        let used: i32 = (lo..=hi).map(w).sum();
        assert!(used <= 40);
        // focus at the ends
        assert_eq!(label_window(40, 0, 30, w).0, 0);
        assert_eq!(label_window(40, 39, 30, w).1, 39);
        // a focused label wider than the room is still shown, alone
        assert_eq!(label_window(5, 2, 4, w), (2, 2));
    }

    #[test]
    fn labels() {
        let home = Some(Path::new("/home/u"));
        assert_eq!(label(Some(Path::new("/home/u")), home, "zsh"), "(~ % zsh)");
        assert_eq!(label(Some(Path::new("/")), home, "zsh"), "(/ % zsh)");
        assert_eq!(
            label(Some(Path::new("/home/u/nirix")), home, "vim"),
            "(nirix % vim)"
        );
        assert_eq!(label(None, home, "zsh"), "(? % zsh)");
        let long = label(
            Some(Path::new("/srv/a-very-long-folder-name-here")),
            home,
            "zsh",
        );
        assert_eq!(long, "(a-very-long-folder-… % zsh)");
    }
    use crossterm::style::Color;

    #[test]
    fn mouse_bytes() {
        let ev = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            encode_mouse(ev(MouseEventKind::ScrollUp), 4, 2, true).unwrap(),
            b"\x1b[<64;5;3M"
        );
        assert_eq!(
            encode_mouse(ev(MouseEventKind::Up(MouseButton::Right)), 0, 0, true).unwrap(),
            b"\x1b[<2;1;1m"
        );
        assert_eq!(
            encode_mouse(ev(MouseEventKind::Down(MouseButton::Left)), 0, 0, false).unwrap(),
            b"\x1b[M !!"
        );
        assert_eq!(
            encode_mouse(ev(MouseEventKind::Up(MouseButton::Left)), 0, 0, false).unwrap(),
            b"\x1b[M#!!"
        );
        assert_eq!(
            encode_mouse(ev(MouseEventKind::Drag(MouseButton::Left)), 0, 0, true).unwrap(),
            b"\x1b[<32;1;1M"
        );
        assert!(encode_mouse(ev(MouseEventKind::Moved), 0, 0, true).is_none());
    }

    #[test]
    fn scroll() {
        // three 39-wide columns with 1-cell gaps on an 80-wide screen
        assert_eq!(scroll_to(0, 0, 39, 80), 0);
        assert_eq!(scroll_to(0, 40, 39, 80), 0);
        assert_eq!(scroll_to(0, 80, 39, 80), 39); // 3rd opens: 1st slides off left
        assert_eq!(scroll_to(39, 0, 39, 80), 0); // focus back to 1st
        assert_eq!(scroll_to(39, 40, 39, 80), 39); // already visible: no scroll
    }

    #[test]
    fn springs() {
        let w = 9.2 / 0.25;
        assert_eq!(spring(0.0, 0.0, 40.0, w, 0.0), (0.0, 0.0));
        let (end, _) = spring(0.0, 0.0, 40.0, w, 0.25);
        assert!((end - 40.0).abs() < 0.1, "settles in time: {end}");
        // never overshoots when starting still
        let mut last = 0.0;
        for i in 1..=25 {
            let (x, _) = spring(0.0, 0.0, 40.0, w, i as f32 / 100.0);
            assert!(x >= last && x <= 40.0);
            last = x;
        }
        // a new target mid-slide keeps position and speed: no jump
        let (p, v) = spring(0.0, 0.0, 40.0, w, 0.05);
        let (p2, v2) = spring(p, v, 80.0, w, 0.0);
        assert!((p2 - p).abs() < 1e-3 && (v2 - v).abs() < 1e-2);
    }

    #[test]
    fn frame_text() {
        let mut f = Frame::default();
        f.reset(10, 1);
        assert_eq!(f.number(0, 0, 42, Color::Reset), 2);
        f.put(2, 0, Glyph::plain('é', Color::Reset));
        let s: String = f.cells.iter().map(Glyph::as_str).collect();
        assert_eq!(s, "42é       ");
        assert_eq!(
            Glyph::new("e\u{301}", Color::Reset, Color::Reset, 0).as_str(),
            "e\u{301}"
        );
    }
}
