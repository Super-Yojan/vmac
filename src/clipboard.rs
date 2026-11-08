//! The system clipboard: pbcopy/pbpaste on macOS, wl-copy/wl-paste or
//! xclip elsewhere. Behind a trait so tests use one in memory.

use std::io::Write;
use std::process::{Command, Stdio};

pub trait Clipboard: Send {
    /// What's on the clipboard (None when it can't be read).
    fn read(&mut self) -> Option<String>;
    fn write(&mut self, text: &str);
}

/// The real one.
pub struct System;

impl System {
    fn paste_command() -> Option<(&'static str, &'static [&'static str])> {
        if cfg!(target_os = "macos") {
            Some(("pbpaste", &[]))
        } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            Some(("wl-paste", &["--no-newline"]))
        } else if std::env::var_os("DISPLAY").is_some() {
            Some(("xclip", &["-selection", "clipboard", "-o"]))
        } else {
            None
        }
    }

    fn copy_command() -> Option<(&'static str, &'static [&'static str])> {
        if cfg!(target_os = "macos") {
            Some(("pbcopy", &[]))
        } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            Some(("wl-copy", &[]))
        } else if std::env::var_os("DISPLAY").is_some() {
            Some(("xclip", &["-selection", "clipboard"]))
        } else {
            None
        }
    }
}

impl Clipboard for System {
    fn read(&mut self) -> Option<String> {
        let (program, args) = Self::paste_command()?;
        let out = Command::new(program).args(args).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Copies without waiting (a yank shouldn't wait on a process).
    fn write(&mut self, text: &str) {
        let Some((program, args)) = Self::copy_command() else { return };
        let text = text.to_string();
        std::thread::spawn(move || {
            let child = Command::new(program).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
            if let Ok(mut child) = child {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                let _ = child.wait();
            }
        });
    }
}

/// A server's clipboard: the system's, and the clipboard of a window on
/// another machine (`vmac connect`). What it copies goes to both (the daemon
/// sends it to such windows); pasting takes the far window's clipboard when
/// it has said what's on it, else the system's.
pub struct Relay;

#[derive(Default)]
struct RelayState {
    from_window: Option<String>,
    copied: Vec<String>,
}

static RELAY: std::sync::Mutex<RelayState> = std::sync::Mutex::new(RelayState { from_window: None, copied: Vec::new() });

fn relay() -> std::sync::MutexGuard<'static, RelayState> {
    RELAY.lock().unwrap_or_else(|e| e.into_inner())
}

/// What's on a far window's clipboard.
pub fn relay_from_window(text: String) {
    relay().from_window = Some(text);
}

/// What sessions copied since the last call, for far windows.
pub fn relay_take_copied() -> Vec<String> {
    std::mem::take(&mut relay().copied)
}

impl Clipboard for Relay {
    fn read(&mut self) -> Option<String> {
        let far = relay().from_window.clone();
        far.or_else(|| System.read())
    }

    fn write(&mut self, text: &str) {
        System.write(text);
        relay().copied.push(text.to_string());
    }
}

/// A clipboard in memory, shared with the test that made it.
#[cfg(test)]
pub struct Memory(pub std::sync::Arc<std::sync::Mutex<String>>);

#[cfg(test)]
impl Clipboard for Memory {
    fn read(&mut self) -> Option<String> {
        Some(self.0.lock().unwrap().clone())
    }

    fn write(&mut self, text: &str) {
        *self.0.lock().unwrap() = text.to_string();
    }
}
