//! nirix: a terminal multiplexer that works like the niri window manager.
//! Each terminal is a column on a long horizontal strip. A new terminal never
//! shrinks the others; the view slides sideways to show it instead.

mod config;
mod pty;

use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::process::Child;
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::{cursor, queue, terminal};

use config::Config;

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
    parser: vt100::Parser,
    master: File,
    child: Child,
    /// Key presses for this pane, written to its shell by a helper thread.
    input: Sender<Vec<u8>>,
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
    const BLANK: Glyph = Glyph { text: [b' '; CELL_BYTES], len: 1, fg: Color::Reset, bg: Color::Reset, attrs: 0 };
    const WIDE_TAIL: Glyph = Glyph { len: 0, ..Glyph::BLANK };

    fn new(s: &str, fg: Color, bg: Color, attrs: u8) -> Self {
        let mut text = [0; CELL_BYTES];
        let len = s.len().min(CELL_BYTES);
        text[..len].copy_from_slice(&s.as_bytes()[..len]);
        Glyph { text, len: len as u8, fg, bg, attrs }
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
            self.put(x + i, y, Glyph::plain(char::from_digit((n % 10) as u32, 10).unwrap(), fg));
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
    thread::Builder::new().stack_size(THREAD_STACK).spawn(f).map(drop)
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap()
}

/// How many cells a number takes when written out.
fn digits(n: usize) -> i32 {
    n.checked_ilog10().unwrap_or(0) as i32 + 1
}

/// Which pane numbers fit in `room` cells: a range around `focus`, growing
/// one step right then one step left so the focused number stays near the middle.
/// Each number takes its digits plus one space.
fn number_window(len: usize, focus: usize, room: i32) -> (usize, usize) {
    let w = |i: usize| digits(i + 1) + 1;
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

impl App {
    /// Box width for a share of the screen. Rounds down so columns plus gaps always fit.
    fn frac_w(&self, frac: f32) -> u16 {
        (((self.w + GAP) as f32 * frac) as u16).saturating_sub(GAP).max(MIN_COL)
    }

    /// A single pane takes the whole screen; otherwise each uses its own width.
    fn col_w(&self, p: &Pane) -> u16 {
        if self.panes.len() == 1 { self.w.max(MIN_COL) } else { self.frac_w(p.frac) }
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
        self.anim.as_ref().is_some_and(|a| a.start.elapsed() < self.cfg.animation_time)
    }

    /// Where the view is right now, and how fast it is moving.
    fn cam(&self) -> (f32, f32) {
        match &self.anim {
            Some(a) if self.animating() => spring(a.pos, a.vel, self.view_x as f32, self.omega(), a.start.elapsed().as_secs_f32()),
            _ => (self.view_x as f32, 0.0),
        }
    }

    /// Start a shell in a new pane right of the focused one.
    fn spawn(&mut self) -> io::Result<()> {
        let frac = 0.5;
        let cols = if self.panes.is_empty() { self.w.max(MIN_COL) } else { self.frac_w(frac) } - 2;
        let rows = self.rows();
        let (master, mut child) = pty::spawn(&self.cfg.shell, rows, cols)?;
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

        let pane = Pane { id, frac, parser: vt100::Parser::new(nz(rows), nz(cols), 0), master, child, input };
        let at = if self.panes.is_empty() { 0 } else { self.focus + 1 };
        self.panes.insert(at, pane);
        self.focus = at;
        Ok(())
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
        let Some(i) = self.panes.iter().position(|p| p.id == id) else { return };
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
                p.parser.set_size(nz(rows), nz(cols));
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
                self.anim = Some(Anim { pos, vel, start: Instant::now() });
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
                p.frac = *WIDTHS.iter().find(|&&f| f > p.frac + 0.01).unwrap_or(&WIDTHS[0]);
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
        if self.shortcut(k) {
            return;
        }
        let p = &self.panes[self.focus];
        let _ = p.input.send(encode(k, p.parser.screen().application_cursor()));
    }

    /// Send pasted text in one go. Wrap it in paste markers if the app asked
    /// for them (editors use this to avoid auto-indenting pasted code).
    fn paste(&mut self, text: String) {
        self.notice = None;
        let p = &self.panes[self.focus];
        let bytes = if p.parser.screen().bracketed_paste() {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        };
        let _ = p.input.send(bytes);
    }

    fn handle(&mut self, e: Ev) {
        match e {
            Ev::Out(id, bytes) => {
                if let Some(p) = self.panes.iter_mut().find(|p| p.id == id) {
                    p.parser.process(&bytes);
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

            let fg = if i == self.focus { self.cfg.focus_color } else { self.cfg.border_color };
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
            let inner = w - 2;
            for y in 0..rows {
                for c in 0..inner {
                    let sx = x0 + 1 + c;
                    if !(0..sw).contains(&sx) {
                        continue;
                    }
                    let Some(cell) = screen.cell(y as u16, c as u16) else { break };
                    if cell.is_wide_continuation() {
                        // Its left half draws this cell. If the left half is off
                        // screen, leave a blank here instead.
                        if sx > 0 {
                            f.put(sx, y + 1, Glyph::WIDE_TAIL);
                        }
                        continue;
                    }
                    let attrs = (cell.bold() as u8 * BOLD)
                        | (cell.italic() as u8 * ITALIC)
                        | (cell.underline() as u8 * UNDERLINE)
                        | (cell.inverse() as u8 * REVERSE);
                    // A wide character with no room for its right half becomes a blank.
                    let cut = cell.is_wide() && (c + 1 >= inner || sx + 1 >= sw);
                    let s = if cut || cell.contents().is_empty() { " " } else { cell.contents() };
                    f.put(sx, y + 1, Glyph::new(s, color(cell.fgcolor()), color(cell.bgcolor()), attrs));
                }
            }
        }

        // Status bar: pane numbers, focused one highlighted. If they don't all
        // fit, show the ones around the focus and "+N" for how many are hidden
        // on each side.
        let y = self.h as i32 - 1;
        let n = self.panes.len();
        // Room for the "+N " counts on both sides.
        let counts = 2 * (digits(n) + 2);
        let (lo, hi) = number_window(n, self.focus, sw - 2 - counts);
        let dim = self.cfg.border_color;
        let mut x = 1;
        if lo > 0 {
            f.put(x, y, Glyph::plain('+', dim));
            x += f.number(x + 1, y, lo, dim) + 2;
        }
        for i in lo..=hi {
            let fg = if i == self.focus { self.cfg.focus_color } else { dim };
            x += f.number(x, y, i + 1, fg) + 1;
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
            queue!(out, SetAttribute(Attribute::Reset), terminal::Clear(terminal::ClearType::All))?;
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
                queue!(out, SetAttribute(Attribute::Reset), SetForegroundColor(g.fg), SetBackgroundColor(g.bg))?;
                for (bit, a) in [(BOLD, Attribute::Bold), (ITALIC, Attribute::Italic), (UNDERLINE, Attribute::Underlined), (REVERSE, Attribute::Reverse)] {
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
            if !s.hide_cursor() && (0..self.w as i32).contains(&sx) {
                queue!(out, cursor::MoveTo(sx as u16, cy + 1), cursor::Show)?;
            }
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
    let mods = 1 + m.contains(KeyModifiers::SHIFT) as u8 + 2 * m.contains(KeyModifiers::ALT) as u8 + 4 * m.contains(KeyModifiers::CONTROL) as u8;
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

/// Give the user's terminal back in its normal state.
fn restore() {
    let _ = terminal::disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), event::DisableBracketedPaste, terminal::LeaveAlternateScreen, cursor::Show);
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
    };
    app.spawn()?;

    terminal::enable_raw_mode()?;
    crossterm::execute!(io::stdout(), terminal::EnterAlternateScreen, event::EnableBracketedPaste)?;
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
            // Nothing waiting: sleep until something arrives or the next draw is due.
            let ev = if busy {
                match rx.recv_timeout(due - now) {
                    Ok(e) => e,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(e) => e,
                    Err(_) => break,
                }
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
    use super::{Frame, Glyph, number_window, scroll_to, spring};

    #[test]
    fn numbers_window() {
        // everything fits
        assert_eq!(number_window(3, 1, 80), (0, 2));
        // 40 panes, room for 10 two-digit numbers: focus 20 (shown as 21) stays inside
        let (lo, hi) = number_window(40, 20, 30);
        assert!(lo <= 20 && 20 <= hi && lo > 0 && hi < 39);
        let used: i32 = (lo..=hi).map(|i| super::digits(i + 1) + 1).sum();
        assert!(used <= 30);
        // focus at the ends
        assert_eq!(number_window(40, 0, 10).0, 0);
        assert_eq!(number_window(40, 39, 10).1, 39);
    }
    use crossterm::style::Color;

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
        assert_eq!(Glyph::new("e\u{301}", Color::Reset, Color::Reset, 0).as_str(), "e\u{301}");
    }
}
