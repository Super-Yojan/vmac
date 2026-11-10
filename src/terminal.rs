//! Terminals: a program on a pseudo-terminal, shown as a read-only buffer
//! (so normal-state motions, search and yank work on its screen).
//!
//! Where the program runs is a backend. The built-in one is a local PTY
//! (`portable-pty`) whose output Ghostty's terminal engine (`qwertty-term-vt`)
//! turns into a screen with scrollback, answering the program's queries. Plugins
//! add others in Lua (`vmac.terminal_backend(name, {start, send, read, ...})`),
//! like herdr-core, where terminals outlive the editor; the Lisp side drives
//! those, since they run Lua.

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use qwertty_term_vt::snapshot::{Snapshot, SnapshotColor};
use qwertty_term_vt::stream::{Stream, TerminalHandler};
pub use qwertty_term_vt::terminal::{MouseEvent, MouseFormat};
use qwertty_term_vt::terminal::{Options, ScreenKey, Terminal};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

/// Lines of history a local terminal keeps above its screen.
const SCROLLBACK: usize = 2_000;

/// What a terminal shows: its lines, where its cursor is, and colored runs
/// (line, start column, end column, face).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Screen {
    pub lines: Vec<String>,
    pub cursor: (usize, usize),
    pub colors: Vec<(usize, usize, usize, String)>,
}

impl Screen {
    /// A screen from plain text (a plugin backend's `read`), its cursor at
    /// the end. Lines lose trailing whitespace, padding included (screens
    /// pad lines to their width, sometimes with non-breaking spaces), so the
    /// cursor lands after the last thing on the screen.
    pub fn from_text(text: &str) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(|l| l.trim_end().to_string()).collect();
        while lines.len() > 1 && lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        let last = lines.len().saturating_sub(1);
        let cursor = (last, lines.last().map_or(0, |l| l.chars().count()));
        Self { lines, cursor, colors: Vec::new() }
    }
}

/// The bytes a key sends to a program, in xterm's encoding. `key` is
/// vmac's notation (`RET`, `C-c`, `<up>`); `text` is what it types.
pub fn key_bytes(key: &str, text: &str) -> Vec<u8> {
    let named: Option<&[u8]> = match key {
        "RET" => Some(b"\r"),
        "TAB" => Some(b"\t"),
        "S-TAB" => Some(b"\x1b[Z"),
        "DEL" => Some(b"\x7f"),
        "ESC" => Some(b"\x1b"),
        "SPC" => Some(b" "),
        "<up>" => Some(b"\x1b[A"),
        "<down>" => Some(b"\x1b[B"),
        "<right>" => Some(b"\x1b[C"),
        "<left>" => Some(b"\x1b[D"),
        "<home>" => Some(b"\x1b[H"),
        "<end>" => Some(b"\x1b[F"),
        "<delete>" => Some(b"\x1b[3~"),
        "<prior>" => Some(b"\x1b[5~"),
        "<next>" => Some(b"\x1b[6~"),
        _ => None,
    };
    if let Some(bytes) = named {
        return bytes.to_vec();
    }
    // C-a .. C-z, C-[ C-\ C-] : control codes.
    if let Some(rest) = key.strip_prefix("C-")
        && let [c] = rest.as_bytes()
    {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            return vec![c - b'a' + 1];
        }
        match c {
            b'[' => return vec![0x1b],
            b'\\' => return vec![0x1c],
            b']' => return vec![0x1d],
            b' ' | b'@' => return vec![0],
            _ => {}
        }
    }
    // M-x: ESC then the key.
    if let Some(rest) = key.strip_prefix("M-")
        && rest.chars().count() == 1
    {
        let mut out = vec![0x1b];
        out.extend_from_slice(rest.as_bytes());
        return out;
    }
    text.as_bytes().to_vec()
}

/// A key as a name, for backends that take named keys (herdr's `hc keys`):
/// `enter`, `esc`, `ctrl+c`, `up`; None for a key that types text.
pub fn key_name(key: &str) -> Option<String> {
    let name = match key {
        "RET" => "enter",
        "TAB" => "tab",
        "S-TAB" => "shift+tab",
        "DEL" => "backspace",
        "ESC" => "esc",
        "SPC" => "space",
        "<up>" => "up",
        "<down>" => "down",
        "<left>" => "left",
        "<right>" => "right",
        "<home>" => "home",
        "<end>" => "end",
        "<delete>" => "delete",
        "<prior>" => "pageup",
        "<next>" => "pagedown",
        _ => {
            if let Some(rest) = key.strip_prefix("C-") {
                return Some(format!("ctrl+{}", rest.to_lowercase()));
            }
            if let Some(rest) = key.strip_prefix("M-") {
                return Some(format!("alt+{rest}"));
            }
            return None;
        }
    };
    Some(name.to_string())
}

/// The face for a terminal color: the theme's names for the 16 ANSI colors,
/// `#rrggbb` for the rest.
fn color_face(color: SnapshotColor) -> Option<String> {
    let rgb = |r: u8, g: u8, b: u8| format!("#{r:02x}{g:02x}{b:02x}");
    match color {
        SnapshotColor::Default => None,
        SnapshotColor::Palette(i) => Some(match i {
            0 | 8 => "dim".into(),
            1 | 9 => "red".into(),
            2 | 10 => "green".into(),
            3 | 11 => "yellow".into(),
            4 | 12 => "blue".into(),
            5 | 13 => "magenta".into(),
            6 | 14 => "cyan".into(),
            7 | 15 => "default".into(),
            16..=231 => {
                // The 6x6x6 cube.
                let i = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            _ => {
                let gray = 8 + (i - 232) * 10;
                rgb(gray, gray, gray)
            }
        }),
        SnapshotColor::Rgb { r, g, b } => Some(rgb(r, g, b)),
    }
}

/// A snapshot (history and screen) as vmac shows it: one line per row,
/// colored runs, and the cursor counted from the top of the history.
fn read_screen(snap: &Snapshot) -> Screen {
    let mut lines = Vec::with_capacity(snap.all_rows.len());
    let mut colors = Vec::new();
    for (row, cells) in snap.all_rows.iter().enumerate() {
        let mut text = String::new();
        let mut run: Option<(usize, String)> = None;
        let mut col = 0usize; // in characters of `text`
        for cell in &cells.cells {
            // The second half of a wide character, and its line-end placeholder.
            if cell.is_spacer() {
                continue;
            }
            text.push(if cell.ch == '\0' { ' ' } else { cell.ch });
            text.extend(&cell.combining);
            let face = color_face(cell.style.fg);
            match (&run, &face) {
                (Some((_, f)), Some(g)) if f == g => {}
                _ => {
                    if let Some((start, f)) = run.take() {
                        colors.push((row, start, col, f));
                    }
                    run = face.map(|f| (col, f));
                }
            }
            col += 1;
        }
        if let Some((start, f)) = run {
            colors.push((row, start, col, f));
        }
        lines.push(text.trim_end().to_string());
    }
    let cursor_row = snap.active_start + snap.cursor.row;
    // Blank rows after both the text and the cursor carry nothing.
    while lines.len() > cursor_row + 1 && lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let count = lines.len();
    colors.retain(|c| c.0 < count && c.1 < lines[c.0].chars().count());
    for c in &mut colors {
        c.2 = c.2.min(lines[c.0].chars().count());
    }
    Screen { lines, cursor: (cursor_row.min(count.saturating_sub(1)), snap.cursor.col), colors }
}

/// Ghostty's terminal engine over a stream of a program's output.
fn engine(rows: u16, cols: u16) -> Stream<TerminalHandler> {
    let options = Options { cols, rows, max_scrollback: SCROLLBACK, ..Default::default() };
    Stream::new(TerminalHandler::new(Terminal::new(options)))
}

/// What a terminal's engine thread hears: the program's output, or a new size.
enum Msg {
    Output(Vec<u8>),
    Resize(u16, u16),
}

/// What a program asked of the mouse, and where its screen starts in the
/// buffer (the line of screen row 0, below the scrollback).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MouseMode {
    pub event: MouseEvent,
    pub format: MouseFormat,
    pub screen_top: usize,
    /// On the alternate screen (a full-screen program: less, vim, htop).
    pub alternate: bool,
}

/// A mouse event to report to a program.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseAction {
    /// Button 0 (left), 1 (middle) or 2 (right) went down.
    Press(u8),
    Release(u8),
    /// The pointer moved with a button down.
    Drag(u8),
    WheelUp,
    WheelDown,
}

/// Modifier keys held during a mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MouseMods {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

/// The bytes reporting a mouse event at a cell (0-based row and column) in
/// the encoding the program asked for; None when its mode doesn't report it.
pub fn mouse_report(mode: &MouseMode, action: MouseAction, row: usize, col: usize, mods: MouseMods) -> Option<Vec<u8>> {
    let reported = match mode.event {
        MouseEvent::None => false,
        MouseEvent::X10 => matches!(action, MouseAction::Press(_) | MouseAction::WheelUp | MouseAction::WheelDown),
        MouseEvent::Normal => !matches!(action, MouseAction::Drag(_)),
        MouseEvent::Button | MouseEvent::Any => true,
    };
    if !reported {
        return None;
    }
    let sgr = matches!(mode.format, MouseFormat::Sgr | MouseFormat::SgrPixels);
    let mut code: u32 = match action {
        MouseAction::Press(b) => b.min(2) as u32,
        // Only SGR says which button let go.
        MouseAction::Release(b) => if sgr { b.min(2) as u32 } else { 3 },
        MouseAction::Drag(b) => 32 + b.min(2) as u32,
        MouseAction::WheelUp => 64,
        MouseAction::WheelDown => 65,
    };
    // X10 compatibility mode reports no modifiers.
    if mode.event != MouseEvent::X10 {
        code += 4 * mods.shift as u32 + 8 * mods.alt as u32 + 16 * mods.control as u32;
    }
    let (x, y) = match mode.format {
        // A cell of the editor's font is about 8 by 16 pixels (as `answer_queries` says).
        MouseFormat::SgrPixels => (col as u32 * 8 + 1, row as u32 * 16 + 1),
        _ => (col as u32 + 1, row as u32 + 1),
    };
    let release = matches!(action, MouseAction::Release(_));
    Some(match mode.format {
        MouseFormat::Sgr | MouseFormat::SgrPixels => {
            format!("\x1b[<{code};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes()
        }
        MouseFormat::Urxvt => format!("\x1b[{};{x};{y}M", code + 32).into_bytes(),
        MouseFormat::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            for v in [code + 32, x + 32, y + 32] {
                let c = char::from_u32(v.min(2047))?;
                out.extend(c.to_string().into_bytes());
            }
            out
        }
        // Plain X10 bytes reach column and row 223 at most.
        MouseFormat::X10 => vec![0x1b, b'[', b'M', (code + 32) as u8, (x.min(223) + 32) as u8, (y.min(223) + 32) as u8],
    })
}

/// The colors terminals report as theirs when a program asks (OSC 10 / 11 /
/// 12): the editor's text and background, set as its theme changes.
static COLORS: Mutex<(u32, u32)> = Mutex::new((0xdddddd, 0x1c1c1c));

/// The theme's text and background colors (0xRRGGBB), for programs that ask.
pub fn set_colors(foreground: u32, background: u32) {
    *COLORS.lock().unwrap_or_else(|e| e.into_inner()) = (foreground, background);
}

/// Answers to queries in a program's output that the engine doesn't answer:
/// its colors (OSC 10 text, 11 background, 12 cursor; programs like Claude
/// Code wait for these to pick a light or dark look) and its size (CSI 14 /
/// 16 / 18 t). `size` is (rows, cols).
pub fn answer_queries(bytes: &[u8], size: (u16, u16)) -> Vec<u8> {
    let (fg, bg) = *COLORS.lock().unwrap_or_else(|e| e.into_inner());
    let rgb = |c: u32| {
        let (r, g, b) = ((c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff);
        format!("rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")
    };
    let mut out = Vec::new();
    let text = String::from_utf8_lossy(bytes);
    for (code, color) in [("10", fg), ("11", bg), ("12", fg)] {
        for (query, end) in [(format!("\x1b]{code};?\x07"), "\x07"), (format!("\x1b]{code};?\x1b\\"), "\x1b\\")] {
            for _ in text.matches(query.as_str()) {
                out.extend(format!("\x1b]{code};{}{end}", rgb(color)).into_bytes());
            }
        }
    }
    let (rows, cols) = size;
    // A cell of the editor's font, roughly: 8 by 16 pixels.
    for (query, reply) in [
        ("\x1b[18t", format!("\x1b[8;{rows};{cols}t")),
        ("\x1b[16t", "\x1b[6;16;8t".to_string()),
        ("\x1b[14t", format!("\x1b[4;{};{}t", rows as u32 * 16, cols as u32 * 8)),
    ] {
        for _ in text.matches(query) {
            out.extend(reply.clone().into_bytes());
        }
    }
    out
}

/// With VMAC_PTY_LOG=FILE, what terminal programs print, what vmac answers
/// their queries with, and the keys sent, escaped, one line each: for a
/// program that hangs or draws wrong.
fn pty_log(direction: &str, bytes: &[u8]) {
    let Some(path) = std::env::var_os("VMAC_PTY_LOG") else { return };
    let text: String = String::from_utf8_lossy(bytes).escape_debug().collect();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{direction} {text}");
    }
}

/// A program on a local pseudo-terminal. Ghostty's engine isn't `Send`, so
/// each terminal's engine lives on its own thread: it is fed the program's
/// output and new sizes, answers the program's queries, and hands back
/// finished screens.
pub struct LocalPty {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    to_engine: Sender<Msg>,
    /// The latest screen the engine built, until it is shown.
    latest: Arc<Mutex<Option<Screen>>>,
    /// The screen and scrollback as text, always the latest (what an agent
    /// reads of a command it ran).
    text: Arc<Mutex<String>>,
    /// The program's mouse mode, as of the latest screen.
    mouse: Arc<Mutex<MouseMode>>,
    size: (u16, u16),
}

impl LocalPty {
    /// Runs `command` (the user's shell when empty) in `cwd`.
    pub fn start(command: &str, cwd: &Path, rows: u16, cols: u16) -> Result<Self, String> {
        let cmd = if command.trim().is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            let mut words = command.split_whitespace();
            let mut cmd = CommandBuilder::new(words.next().unwrap_or("sh"));
            cmd.args(words);
            cmd
        };
        Self::spawn(cmd, cwd, rows, cols)
    }

    /// Runs `program` with exactly these arguments and extra environment.
    pub fn run(program: &str, args: &[String], env: &[(String, String)], cwd: &Path, rows: u16, cols: u16) -> Result<Self, String> {
        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        Self::spawn(cmd, cwd, rows, cols)
    }

    fn spawn(mut cmd: CommandBuilder, cwd: &Path, rows: u16, cols: u16) -> Result<Self, String> {
        let pty = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pty.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pty.master.take_writer().map_err(|e| e.to_string())?));
        let (to_engine, from_pty) = channel::<Msg>();

        // The program's output, to the engine.
        let tx = to_engine.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                let r = reader.read(&mut buf);
                if let Ok(n) = &r {
                    pty_log("<-", &buf[..*n]);
                }
                let Ok(n) = r else { break };
                if n == 0 || tx.send(Msg::Output(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        });

        // The engine: made here and never leaves this thread.
        let latest = Arc::new(Mutex::new(None));
        let text = Arc::new(Mutex::new(String::new()));
        let mouse = Arc::new(Mutex::new(MouseMode::default()));
        let (screen_out, replies, text_out, mouse_out) = (latest.clone(), writer.clone(), text.clone(), mouse.clone());
        std::thread::spawn(move || {
            let mut stream = engine(rows, cols);
            let mut size = (rows, cols);
            // Queries the engine leaves unanswered, answered here.
            let mut extra: Vec<u8> = Vec::new();
            let mut apply = |stream: &mut Stream<TerminalHandler>, msg: Msg, extra: &mut Vec<u8>| match msg {
                Msg::Output(bytes) => {
                    extra.extend(answer_queries(&bytes, size));
                    stream.feed(&bytes)
                }
                Msg::Resize(rows, cols) => {
                    size = (rows, cols);
                    stream.terminal_mut().resize(cols, rows)
                }
            };
            while let Ok(msg) = from_pty.recv() {
                apply(&mut stream, msg, &mut extra);
                // Take everything already waiting before building a screen.
                while let Ok(msg) = from_pty.try_recv() {
                    apply(&mut stream, msg, &mut extra);
                }
                let mut answers = stream.handler.take_output();
                answers.append(&mut extra);
                if !answers.is_empty() {
                    pty_log("answer ->", &answers);
                    let mut w = replies.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = w.write_all(&answers).and_then(|()| w.flush());
                }
                let snapshot = stream.terminal().snapshot();
                let screen = read_screen(&snapshot);
                let flags = &stream.terminal().flags;
                *mouse_out.lock().unwrap_or_else(|e| e.into_inner()) = MouseMode {
                    event: flags.mouse_event,
                    format: flags.mouse_format,
                    screen_top: snapshot.active_start,
                    alternate: stream.terminal().screens.active_key() == ScreenKey::Alternate,
                };
                let mut lines: Vec<&str> = screen.lines.iter().map(|l| l.trim_end()).collect();
                while lines.last().is_some_and(|l| l.is_empty()) {
                    lines.pop();
                }
                *text_out.lock().unwrap_or_else(|e| e.into_inner()) = lines.join("\n");
                *screen_out.lock().unwrap_or_else(|e| e.into_inner()) = Some(screen);
            }
        });

        Ok(Self { master: pty.master, writer, child, to_engine, latest, text, mouse, size: (rows, cols) })
    }

    pub fn send(&mut self, bytes: &[u8]) {
        pty_log("keys ->", bytes);
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = w.write_all(bytes).and_then(|()| w.flush());
    }

    /// What the program asked of the mouse, as of its latest screen.
    pub fn mouse_mode(&self) -> MouseMode {
        *self.mouse.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) == self.size || rows == 0 || cols == 0 {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        let _ = self.to_engine.send(Msg::Resize(rows, cols));
    }

    /// The screen, when the engine built a new one since the last call.
    pub fn screen(&mut self) -> Option<Screen> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Everything on the screen and in its scrollback, as text.
    pub fn text(&self) -> String {
        self.text.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How it ended: (exit code, signal), or None while it runs.
    pub fn exit_status(&mut self) -> Option<(u32, Option<String>)> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some((status.exit_code(), status.signal().map(str::to_string))),
            _ => None,
        }
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Whether something is running in it: a program it was started with,
    /// or a command its shell runs (the terminal's foreground process isn't
    /// the shell; on Windows, which can't tell, a shell is taken as idle).
    pub fn running_command(&mut self, shell: bool) -> bool {
        if !self.alive() {
            return false;
        }
        if !shell {
            return true;
        }
        #[cfg(unix)]
        {
            match (self.master.process_group_leader(), self.child.process_id()) {
                (Some(leader), Some(pid)) => leader as u32 != pid,
                _ => false,
            }
        }
        #[cfg(not(unix))]
        false
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    /// A program on a PTY, its screen after a few seconds: VMAC_PTY_CMD.
    #[test]
    #[ignore]
    fn shows_a_programs_screen() {
        let Ok(cmd) = std::env::var("VMAC_PTY_CMD") else { return };
        let dir = std::env::current_dir().unwrap();
        let mut pty = LocalPty::start(&cmd, &dir, 30, 100).unwrap();
        for _ in 0..8 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let _ = pty.screen();
        }
        println!("alive={} exit={:?}\n---\n{}\n---", pty.alive(), pty.exit_status(), pty.text());
    }

    use super::*;

    #[test]
    fn color_and_size_queries_are_answered() {
        set_colors(0x112233, 0xffeedd);
        let a = String::from_utf8(answer_queries(b"\x1b]11;?\x1b\\x\x1b]10;?\x07", (24, 80))).unwrap();
        assert!(a.contains("\x1b]11;rgb:ffff/eeee/dddd\x1b\\"), "{a:?}");
        assert!(a.contains("\x1b]10;rgb:1111/2222/3333\x07"), "the query's terminator: {a:?}");
        let s = String::from_utf8(answer_queries(b"\x1b[18t\x1b[14t", (24, 80))).unwrap();
        assert_eq!(s, "\x1b[8;24;80t\x1b[4;384;640t");
        assert!(answer_queries(b"plain text", (24, 80)).is_empty());
        set_colors(0xdddddd, 0x1c1c1c);
    }

    #[test]
    fn mouse_events_are_reported_as_the_program_asked() {
        let mut mode = MouseMode::default();
        let none = MouseMods::default();
        assert_eq!(mouse_report(&mode, MouseAction::Press(0), 0, 0, none), None, "no tracking, no reports");
        mode.event = MouseEvent::Normal;
        assert_eq!(mouse_report(&mode, MouseAction::Press(0), 2, 4, none).unwrap(), b"\x1b[M\x20\x25\x23");
        assert_eq!(mouse_report(&mode, MouseAction::Release(0), 2, 4, none).unwrap(), b"\x1b[M\x23\x25\x23");
        assert_eq!(mouse_report(&mode, MouseAction::Drag(0), 2, 4, none), None, "normal tracking skips motion");
        mode.format = MouseFormat::Sgr;
        assert_eq!(mouse_report(&mode, MouseAction::Release(2), 9, 0, none).unwrap(), b"\x1b[<2;1;10m");
        assert_eq!(mouse_report(&mode, MouseAction::WheelDown, 0, 0, MouseMods { control: true, ..none }).unwrap(), b"\x1b[<81;1;1M");
        mode.event = MouseEvent::Button;
        assert_eq!(mouse_report(&mode, MouseAction::Drag(0), 1, 1, none).unwrap(), b"\x1b[<32;2;2M");
    }

    #[test]
    fn programs_mouse_modes_reach_the_pty() {
        let script = r"printf '\033[?1002h\033[?1006h'; sleep 2".to_string();
        let mut pty = LocalPty::run("sh", &["-c".into(), script], &[], &std::env::temp_dir(), 10, 40).unwrap();
        let start = std::time::Instant::now();
        while pty.mouse_mode().event != MouseEvent::Button && start.elapsed().as_secs() < 5 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = pty.screen();
        }
        let mode = pty.mouse_mode();
        assert_eq!((mode.event, mode.format), (MouseEvent::Button, MouseFormat::Sgr));
    }

    #[test]
    fn keys_become_terminal_bytes() {
        assert_eq!(key_bytes("RET", ""), b"\r");
        assert_eq!(key_bytes("C-c", ""), [3]);
        assert_eq!(key_bytes("C-\\", ""), [0x1c]);
        assert_eq!(key_bytes("M-x", ""), b"\x1bx");
        assert_eq!(key_bytes("<up>", ""), b"\x1b[A");
        assert_eq!(key_bytes("a", "a"), b"a");
        assert_eq!(key_bytes("é", "é"), "é".as_bytes());
    }

    #[test]
    fn keys_become_names() {
        assert_eq!(key_name("RET").as_deref(), Some("enter"));
        assert_eq!(key_name("C-c").as_deref(), Some("ctrl+c"));
        assert_eq!(key_name("<up>").as_deref(), Some("up"));
        assert_eq!(key_name("a"), None);
    }

    #[test]
    fn colors_become_faces() {
        assert_eq!(color_face(SnapshotColor::Palette(1)).as_deref(), Some("red"));
        assert_eq!(color_face(SnapshotColor::Palette(196)).as_deref(), Some("#ff0000"));
        assert_eq!(color_face(SnapshotColor::Rgb { r: 1, g: 2, b: 3 }).as_deref(), Some("#010203"));
        assert_eq!(color_face(SnapshotColor::Default), None);
    }

    fn screen_of(bytes: &[u8], rows: u16, cols: u16) -> Screen {
        let mut stream = engine(rows, cols);
        stream.feed(bytes);
        read_screen(&stream.terminal().snapshot())
    }

    #[test]
    fn a_screen_is_read_with_its_colors() {
        let screen = screen_of(b"plain \x1b[31mred\x1b[0m \x1b[38;2;1;2;3mrgb\x1b[0m\r\nsecond", 5, 20);
        assert_eq!(screen.lines, vec!["plain red rgb", "second"]);
        assert_eq!(screen.colors, vec![(0, 6, 9, "red".to_string()), (0, 10, 13, "#010203".to_string())]);
        assert_eq!(screen.cursor, (1, 6));
    }

    #[test]
    fn history_scrolls_above_the_screen() {
        let lines: String = (0..10).map(|i| format!("line {i}\r\n")).collect();
        let screen = screen_of(lines.as_bytes(), 3, 20);
        assert_eq!(screen.lines.first().map(String::as_str), Some("line 0"), "the scrollback is kept");
        assert_eq!(screen.lines.last().map(String::as_str), Some(""), "the cursor's empty line at the bottom");
        assert_eq!(screen.cursor.0, screen.lines.len() - 1);
    }

    #[test]
    fn wide_characters_take_one_place_in_the_text() {
        let screen = screen_of("界x".as_bytes(), 2, 10);
        assert_eq!(screen.lines, vec!["界x"]);
    }

    #[test]
    fn the_program_gets_answers_to_its_queries() {
        // A program asking where the cursor is (CSI 6n) needs an answer.
        let mut stream = engine(5, 20);
        stream.feed(b"ab\x1b[6n");
        assert_eq!(stream.handler.take_output(), b"\x1b[1;3R");
    }

    #[test]
    fn a_shell_runs_on_a_pty() {
        let dir = std::env::temp_dir();
        let mut pty = LocalPty::start("sh", &dir, 10, 40).unwrap();
        pty.send(b"echo vmac-$((40 + 2))\r");
        let mut seen = String::new();
        for _ in 0..100 {
            if let Some(screen) = pty.screen() {
                seen = screen.lines.join("\n");
                if seen.contains("vmac-42") {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(seen.contains("vmac-42"), "{seen}");
        assert!(pty.alive());
        pty.send(b"exit\r");
        for _ in 0..100 {
            if !pty.alive() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!pty.alive(), "exit ends it");
    }

    #[test]
    fn plain_text_screens() {
        let s = Screen::from_text("$ ls\nfile\n$ \n");
        assert_eq!(s.lines, vec!["$ ls", "file", "$"]);
        assert_eq!(s.cursor, (2, 1));
        // A prompt padded to the screen's width with non-breaking spaces.
        let padded = format!("~\n→{}\n\n", "\u{a0}".repeat(198));
        let s = Screen::from_text(&padded);
        assert_eq!(s.lines, vec!["~", "→"]);
        assert_eq!(s.cursor, (1, 1), "after the arrow, not at column 199");
    }

    /// A shell waiting isn't busy; a command it runs is (what a restart
    /// waits for).
    #[cfg(unix)]
    #[test]
    fn a_command_running_in_a_shell_is_seen() {
        let mut pty = LocalPty::start("sh", &std::env::temp_dir(), 10, 40).unwrap();
        let wait = |pty: &mut LocalPty, want: bool| {
            for _ in 0..100 {
                if pty.running_command(true) == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            false
        };
        assert!(wait(&mut pty, false), "a shell waiting");
        pty.send(b"sleep 30\n");
        assert!(wait(&mut pty, true), "sleep running");
        pty.send(&[3]);
        assert!(wait(&mut pty, false), "interrupted");
        assert!(pty.running_command(false), "a program it was started with counts while it runs");
        pty.kill();
    }
}
