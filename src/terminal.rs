//! Terminals: a program on a pseudo-terminal, shown as a read-only buffer
//! (so normal-state motions, search and yank work on its screen).
//!
//! Where the program runs is a backend. The built-in one is a local PTY
//! (`portable-pty`) whose output Ghostty's terminal engine (`qwertty-term-vt`)
//! turns into a screen with scrollback, answering the program's queries. Plugins
//! add others in Lua (`gmacs.terminal_backend(name, {start, send, read, ...})`),
//! like herdr-core, where terminals outlive the editor; the Lisp side drives
//! those, since they run Lua.

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use qwertty_term_vt::snapshot::{Snapshot, SnapshotColor};
use qwertty_term_vt::stream::{Stream, TerminalHandler};
use qwertty_term_vt::terminal::{Options, Terminal};
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
/// gmacs's notation (`RET`, `C-c`, `<up>`); `text` is what it types.
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

/// A snapshot (history and screen) as gmacs shows it: one line per row,
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
    size: (u16, u16),
}

impl LocalPty {
    /// Runs `command` (the user's shell when empty) in `cwd`.
    pub fn start(command: &str, cwd: &Path, rows: u16, cols: u16) -> Result<Self, String> {
        let pty = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
        let mut cmd = if command.trim().is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            let mut words = command.split_whitespace();
            let mut cmd = CommandBuilder::new(words.next().unwrap_or("sh"));
            cmd.args(words);
            cmd
        };
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
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(Msg::Output(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        });

        // The engine: made here and never leaves this thread.
        let latest = Arc::new(Mutex::new(None));
        let (screen_out, replies) = (latest.clone(), writer.clone());
        std::thread::spawn(move || {
            let mut stream = engine(rows, cols);
            let apply = |stream: &mut Stream<TerminalHandler>, msg: Msg| match msg {
                Msg::Output(bytes) => stream.feed(&bytes),
                Msg::Resize(rows, cols) => stream.terminal_mut().resize(cols, rows),
            };
            while let Ok(msg) = from_pty.recv() {
                apply(&mut stream, msg);
                // Take everything already waiting before building a screen.
                while let Ok(msg) = from_pty.try_recv() {
                    apply(&mut stream, msg);
                }
                let answers = stream.handler.take_output();
                if !answers.is_empty() {
                    let mut w = replies.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = w.write_all(&answers).and_then(|()| w.flush());
                }
                let screen = read_screen(&stream.terminal().snapshot());
                *screen_out.lock().unwrap_or_else(|e| e.into_inner()) = Some(screen);
            }
        });

        Ok(Self { master: pty.master, writer, child, to_engine, latest, size: (rows, cols) })
    }

    pub fn send(&mut self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = w.write_all(bytes).and_then(|()| w.flush());
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

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
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
    use super::*;

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
        pty.send(b"echo gmacs-$((40 + 2))\r");
        let mut seen = String::new();
        for _ in 0..100 {
            if let Some(screen) = pty.screen() {
                seen = screen.lines.join("\n");
                if seen.contains("gmacs-42") {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(seen.contains("gmacs-42"), "{seen}");
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
}
