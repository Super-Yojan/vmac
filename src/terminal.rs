//! Terminals: a program on a pseudo-terminal, shown as a read-only buffer
//! (so normal-state motions, search and yank work on its screen).
//!
//! Where the program runs is a backend. The built-in one is a local PTY
//! (`portable-pty`) whose output Rust turns into a screen (`vt100`). Plugins
//! add others in Lua (`gmacs.terminal_backend(name, {start, send, read, ...})`),
//! like herdr-core, where terminals outlive the editor; the Lisp side drives
//! those, since they run Lua.

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
fn color_face(color: vt100::Color) -> Option<String> {
    let rgb = |r: u8, g: u8, b: u8| format!("#{r:02x}{g:02x}{b:02x}");
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(match i {
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
        vt100::Color::Rgb(r, g, b) => Some(rgb(r, g, b)),
    }
}

/// A vt100 screen as gmacs shows it.
fn read_screen(parser: &vt100::Parser) -> Screen {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let mut lines: Vec<String> = screen.rows(0, cols).collect();
    let mut colors = Vec::new();
    for row in 0..rows {
        let mut run: Option<(usize, String)> = None;
        let mut col_chars = 0usize; // columns as characters in the line text
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let face = color_face(cell.fgcolor());
            match (&run, &face) {
                (Some((_, f)), Some(g)) if f == g => {}
                _ => {
                    if let Some((start, f)) = run.take() {
                        colors.push((row as usize, start, col_chars, f));
                    }
                    run = face.map(|f| (col_chars, f));
                }
            }
            col_chars += 1;
        }
        if let Some((start, f)) = run {
            colors.push((row as usize, start, col_chars, f));
        }
    }
    // Trailing blank lines carry nothing.
    while lines.len() > 1 && lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let line_count = lines.len();
    colors.retain(|c| c.0 < line_count && c.1 < lines[c.0].chars().count());
    for c in &mut colors {
        c.2 = c.2.min(lines[c.0].chars().count());
    }
    let (r, c) = screen.cursor_position();
    Screen { lines, cursor: (r as usize, c as usize), colors }
}

/// A program on a local pseudo-terminal.
pub struct LocalPty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    parser: Arc<Mutex<vt100::Parser>>,
    changed: Arc<AtomicBool>,
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
        let writer = pty.master.take_writer().map_err(|e| e.to_string())?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let changed = Arc::new(AtomicBool::new(true));
        let (p, c) = (parser.clone(), changed.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                p.lock().unwrap_or_else(|e| e.into_inner()).process(&buf[..n]);
                c.store(true, Ordering::Release);
            }
            c.store(true, Ordering::Release);
        });
        Ok(Self { master: pty.master, writer, child, parser, changed, size: (rows, cols) })
    }

    pub fn send(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes).and_then(|()| self.writer.flush());
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) == self.size || rows == 0 || cols == 0 {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        self.parser.lock().unwrap_or_else(|e| e.into_inner()).set_size(rows, cols);
        self.changed.store(true, Ordering::Release);
    }

    /// The screen, when it changed since the last call.
    pub fn screen(&mut self) -> Option<Screen> {
        if !self.changed.swap(false, Ordering::AcqRel) {
            return None;
        }
        Some(read_screen(&self.parser.lock().unwrap_or_else(|e| e.into_inner())))
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
        assert_eq!(color_face(vt100::Color::Idx(1)).as_deref(), Some("red"));
        assert_eq!(color_face(vt100::Color::Idx(196)).as_deref(), Some("#ff0000"));
        assert_eq!(color_face(vt100::Color::Rgb(1, 2, 3)).as_deref(), Some("#010203"));
        assert_eq!(color_face(vt100::Color::Default), None);
    }

    #[test]
    fn a_screen_is_read_with_its_colors() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"plain \x1b[31mred\x1b[0m\r\nsecond");
        let screen = read_screen(&parser);
        assert_eq!(screen.lines, vec!["plain red", "second"]);
        assert_eq!(screen.colors, vec![(0, 6, 9, "red".to_string())]);
        assert_eq!(screen.cursor, (1, 6));
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
