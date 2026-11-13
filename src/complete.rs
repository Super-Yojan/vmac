//! Ghost-text completions from `vmac-complete`, a model running beside vmac
//! (docs/design/autocomplete/).
//!
//! The engine is a program on stdio speaking a line of JSON per message,
//! the way agents are (agent.rs): vmac keeps it in step with the buffers it
//! completes in (open, then the edits since), asks for a completion at the
//! point after a short pause in typing, and shows what comes back after the
//! cursor, dimmed, until it's accepted (TAB), typed through, or dismissed.
//! A thread reads the engine's output into a channel, drained on the
//! terminals' timer (30 ms) so a completion doesn't wait for the language
//! servers' (100 ms).
//!
//! When and how it asks is Lua's (`lua/core/65-autocomplete.lua`); this is
//! the plumbing.

use crate::editor::{BufferId, EditorState};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

/// How completions are made, from Lua's `vmac.set_autocomplete`.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub enabled: bool,
    /// The engine and its arguments.
    pub command: Vec<String>,
    /// mlx (LFM3 on Apple silicon) or fake (no model, for trying it out).
    pub backend: String,
    pub model: String,
    /// spm, psm or prefix; empty: the engine picks (spm when the model has
    /// FIM tokens, else prefix).
    pub layout: String,
    pub debounce: Duration,
    /// line or block.
    pub stop: String,
    pub max_tokens: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            command: vec!["vmac-complete".into()],
            backend: "mlx".into(),
            model: String::new(),
            layout: String::new(),
            debounce: Duration::from_millis(40),
            stop: "block".into(),
            max_tokens: 64,
        }
    }
}

/// A completion shown after the cursor: valid while the buffer is at
/// `revision` and the point at `pos`.
#[derive(Debug, Clone, PartialEq)]
pub struct Ghost {
    pub buffer: BufferId,
    pub pos: usize,
    pub revision: u64,
    pub text: String,
}

struct Engine {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Asked {
    id: u64,
    buffer: BufferId,
    revision: u64,
    pos: usize,
}

#[derive(Default)]
pub struct Completer {
    pub config: Config,
    engine: Option<Engine>,
    /// The text (and its revision) the engine has of each buffer.
    sent: HashMap<BufferId, (u64, String)>,
    next_id: u64,
    asked: Option<Asked>,
    /// Ask when this time comes (the debounce), for this buffer.
    due: Option<(Instant, BufferId)>,
    pub ghost: Option<Ghost>,
    /// What the engine last said of itself (loading, ready, an error).
    pub status: String,
    /// The last completion's numbers, as the engine sent them.
    pub stats: Option<Value>,
    failures: u32,
    retry_at: Option<Instant>,
}

impl std::fmt::Debug for Completer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completer")
            .field("config", &self.config)
            .field("running", &self.engine.is_some())
            .field("ghost", &self.ghost)
            .field("status", &self.status)
            .finish()
    }
}

impl Completer {
    fn spawn(&mut self, root: &str) -> bool {
        if self.engine.is_some() {
            return true;
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        let Some((program, args)) = self.config.command.split_first() else { return false };
        let log = crate::packages::data_dir()
            .map(|d| d.join("logs"))
            .and_then(|d| std::fs::create_dir_all(&d).ok().map(|_| d.join("vmac-complete.log")))
            .and_then(|p| std::fs::File::create(p).ok());
        let spawned = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log.map(Stdio::from).unwrap_or_else(Stdio::null))
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                self.failed(format!("couldn't start {program}: {e}"));
                return false;
            }
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return false;
        };
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        self.engine = Some(Engine { child, stdin, rx });
        self.sent.clear();
        self.status = "starting".into();
        let mut params = json!({ "root": root, "backend": self.config.backend });
        if !self.config.model.is_empty() {
            params["model"] = json!(self.config.model);
        }
        if !self.config.layout.is_empty() {
            params["prompt"] = json!({ "mode": self.config.layout });
        }
        self.next_id += 1;
        self.send(&json!({ "id": self.next_id, "method": "initialize", "params": params }));
        self.engine.is_some()
    }

    /// The engine couldn't start or went away: try again later, waiting
    /// longer each time (2 s, 4 s, ... a minute).
    fn failed(&mut self, why: String) {
        self.engine = None;
        self.asked = None;
        self.sent.clear();
        self.failures += 1;
        let wait = Duration::from_secs((2u64 << self.failures.min(5)).min(60));
        self.retry_at = Some(Instant::now() + wait);
        self.status = why;
    }

    fn send(&mut self, msg: &Value) {
        let Some(engine) = self.engine.as_mut() else { return };
        let ok = writeln!(engine.stdin, "{msg}").and_then(|_| engine.stdin.flush());
        if ok.is_err() {
            self.failed("the completion engine stopped".into());
        }
    }

    /// Brings the engine's copy of a buffer to `text` at `revision`: the
    /// whole text the first time, then the one range that changed.
    fn sync(&mut self, buffer: BufferId, path: &str, language: &str, revision: u64, text: String) {
        match self.sent.get(&buffer) {
            Some((rev, _)) if *rev == revision => {}
            Some((_, old)) => {
                let (start, end, new) = changed_range(old, &text);
                self.send(&json!({ "method": "edit", "params": {
                    "doc": buffer, "revision": revision,
                    "edits": [{ "start": start, "end": end, "text": new }],
                }}));
                self.sent.insert(buffer, (revision, text));
            }
            None => {
                self.send(&json!({ "method": "open", "params": {
                    "doc": buffer, "path": path, "language": language, "revision": revision, "text": text,
                }}));
                self.sent.insert(buffer, (revision, text));
            }
        }
    }

    /// The ghost text to show in `buffer` at `revision` with the point at
    /// `point`, if any.
    pub fn visible(&self, buffer: BufferId, revision: u64, point: usize) -> Option<&str> {
        self.ghost
            .as_ref()
            .filter(|g| g.buffer == buffer && g.revision == revision && g.pos == point && !g.text.is_empty())
            .map(|g| g.text.as_str())
    }
}

/// The one range that differs between `old` and `new`, in characters:
/// (start, end in `old`, its replacement).
pub fn changed_range(old: &str, new: &str) -> (usize, usize, String) {
    let prefix = old.chars().zip(new.chars()).take_while(|(a, b)| a == b).count();
    let (old_len, new_len) = (old.chars().count(), new.chars().count());
    let room = old_len.min(new_len) - prefix;
    let suffix = old.chars().rev().zip(new.chars().rev()).take(room).take_while(|(a, b)| a == b).count();
    let replacement: String = new.chars().skip(prefix).take(new_len - prefix - suffix).collect();
    (prefix, old_len - suffix, replacement)
}

/// How much of a ghost text one accept takes: all of it, its first word
/// (with the spaces before it), or its first line.
pub fn accept_part(text: &str, how: &str) -> usize {
    match how {
        "word" => {
            let lead = text.chars().take_while(|c| c.is_whitespace() && *c != '\n').count();
            let rest = text.chars().skip(lead);
            let word: usize = {
                let mut it = rest.peekable();
                match it.peek() {
                    Some(c) if c.is_alphanumeric() || *c == '_' => it.take_while(|c| c.is_alphanumeric() || *c == '_').count(),
                    Some('\n') => 1,
                    Some(_) => 1,
                    None => 0,
                }
            };
            lead + word
        }
        "line" => match text.find('\n') {
            Some(0) => text[1..].find('\n').map_or(text.chars().count(), |i| text[..i + 1].chars().count()),
            Some(i) => text[..i].chars().count(),
            None => text.chars().count(),
        },
        _ => text.chars().count(),
    }
}

impl EditorState {
    /// The buffer completions are asked for: the selected window's, when
    /// it's a file and nothing modal is open.
    fn completable(&self) -> Option<(BufferId, PathBuf)> {
        if self.minibuffer_prompt.is_some() || self.picker.is_some() {
            return None;
        }
        let buffer = self.buffer();
        if buffer.is_read_only() {
            return None;
        }
        Some((self.window().buffer, buffer.file()?.clone()))
    }

    /// Asks for a completion at the point once typing pauses (the debounce).
    pub fn autocomplete_request(&mut self) {
        if !self.completer.config.enabled {
            return;
        }
        if let Some((id, _)) = self.completable() {
            self.completer.due = Some((Instant::now() + self.completer.config.debounce, id));
        }
    }

    /// Reads what the engine sent, and asks when a request is due; true
    /// when the screen changes.
    pub fn autocomplete_pump(&mut self) -> bool {
        let mut changed = false;
        loop {
            let msg = match self.completer.engine.as_ref().map(|e| e.rx.try_recv()) {
                Some(Ok(m)) => m,
                Some(Err(TryRecvError::Disconnected)) => {
                    self.completer.failed("the completion engine stopped".into());
                    changed = true;
                    break;
                }
                _ => break,
            };
            changed |= self.autocomplete_message(msg);
        }
        if let Some((when, buffer)) = self.completer.due
            && Instant::now() >= when
        {
            self.completer.due = None;
            if self.completable().is_some_and(|(id, _)| id == buffer) {
                changed |= self.autocomplete_ask();
            }
        }
        changed
    }

    fn autocomplete_ask(&mut self) -> bool {
        let Some((id, path)) = self.completable() else { return false };
        let root = crate::picker::project_root(&path).display().to_string();
        if !self.completer.spawn(&root) {
            return false;
        }
        let buffer = self.buffer();
        let (revision, point) = (buffer.revision(), buffer.point());
        let language = buffer
            .language()
            .or_else(|| crate::syntax::language_for_path(&path))
            .unwrap_or("")
            .to_string();
        let text = buffer.text().to_string();
        self.completer.sync(id, &path.display().to_string(), &language, revision, text);
        if let Some(old) = self.completer.asked.take() {
            self.completer.send(&json!({ "method": "cancel", "params": { "id": old.id } }));
        }
        self.completer.next_id += 1;
        let ask = Asked { id: self.completer.next_id, buffer: id, revision, pos: point };
        let (stop, max_tokens) = (self.completer.config.stop.clone(), self.completer.config.max_tokens);
        self.completer.send(&json!({ "id": ask.id, "method": "complete", "params": {
            "doc": id, "revision": revision, "pos": point, "stop": stop, "max_tokens": max_tokens,
        }}));
        if self.completer.engine.is_some() {
            self.completer.asked = Some(ask);
        }
        false
    }

    fn autocomplete_message(&mut self, msg: Value) -> bool {
        if msg.get("method").and_then(Value::as_str) == Some("status") {
            let p = &msg["params"];
            let state = p["state"].as_str().unwrap_or("");
            let detail = p["detail"].as_str().unwrap_or("");
            self.completer.status = if detail.is_empty() { state.to_string() } else { format!("{state}: {detail}") };
            if state == "ready" {
                self.completer.failures = 0;
            }
            return false;
        }
        if msg.get("method").and_then(Value::as_str) == Some("resync") {
            if let Some(doc) = msg["params"]["doc"].as_u64() {
                self.completer.sent.remove(&doc);
            }
            return false;
        }
        let (id, text, partial) = if msg.get("method").and_then(Value::as_str) == Some("partial") {
            (msg["params"]["id"].as_u64(), msg["params"]["text"].as_str().map(str::to_string), true)
        } else if let Some(result) = msg.get("result") {
            if result.get("stats").is_some() {
                self.completer.stats = Some(result["stats"].clone());
            }
            (msg["id"].as_u64(), result.get("text").and_then(Value::as_str).map(str::to_string), false)
        } else {
            if let Some(err) = msg.get("error") {
                self.completer.status = err["message"].as_str().unwrap_or("error").to_string();
            }
            return false;
        };
        let Some(ask) = self.completer.asked.filter(|a| Some(a.id) == id) else { return false };
        if !partial {
            self.completer.asked = None;
        }
        let Some(text) = text.filter(|t| !t.is_empty()) else { return false };
        // Only if nothing moved since it was asked.
        let still = self.window().buffer == ask.buffer
            && self.buffer().revision() == ask.revision
            && self.buffer().point() == ask.pos;
        if !still {
            return false;
        }
        let ghost = Ghost { buffer: ask.buffer, pos: ask.pos, revision: ask.revision, text };
        let changed = self.completer.ghost.as_ref() != Some(&ghost);
        self.completer.ghost = Some(ghost);
        changed
    }

    /// Takes all, a word or a line of the ghost text into the buffer; the
    /// rest stays shown. False when there's none to take.
    pub fn ghost_accept(&mut self, how: &str) -> bool {
        let (buffer, revision, point) = (self.window().buffer, self.buffer().revision(), self.buffer().point());
        let Some(text) = self.completer.visible(buffer, revision, point).map(str::to_string) else { return false };
        let n = accept_part(&text, how);
        let taken: String = text.chars().take(n).collect();
        let rest: String = text.chars().skip(n).collect();
        self.current().insert(&taken);
        let (revision, point) = (self.buffer().revision(), self.buffer().point());
        self.completer.ghost = (!rest.is_empty()).then_some(Ghost { buffer, pos: point, revision, text: rest });
        if let Some(asked) = self.completer.asked.take() {
            self.completer.send(&json!({ "method": "cancel", "params": { "id": asked.id } }));
        }
        true
    }

    /// After `text` was typed: if it's how the ghost text goes on, the ghost
    /// text just gets shorter (and true: no need to ask again).
    pub fn ghost_type_through(&mut self, typed: &str) -> bool {
        let (buffer, revision, point) = (self.window().buffer, self.buffer().revision(), self.buffer().point());
        let Some(g) = self.completer.ghost.as_ref() else { return false };
        let n = typed.chars().count();
        let fits = g.buffer == buffer
            && g.pos + n == point
            && g.text.starts_with(typed)
            && self.buffer().substring(g.pos, point) == typed;
        if !fits {
            self.completer.ghost = None;
            return false;
        }
        let rest: String = g.text.chars().skip(n).collect();
        if rest.is_empty() {
            self.completer.ghost = None;
            return false;
        }
        self.completer.ghost = Some(Ghost { buffer, pos: point, revision, text: rest });
        true
    }

    pub fn ghost_dismiss(&mut self) {
        self.completer.ghost = None;
        self.completer.due = None;
        if let Some(asked) = self.completer.asked.take() {
            self.completer.send(&json!({ "method": "cancel", "params": { "id": asked.id } }));
        }
    }

    /// Turns completions on or off, or changes how they're made. A new
    /// engine command or model restarts the engine.
    pub fn autocomplete_configure(&mut self, config: Config) {
        let restart = config.command != self.completer.config.command
            || config.model != self.completer.config.model
            || config.backend != self.completer.config.backend
            || config.layout != self.completer.config.layout
            || !config.enabled;
        if restart {
            self.completer.engine = None;
            self.completer.sent.clear();
            self.completer.asked = None;
            self.completer.retry_at = None;
            self.completer.failures = 0;
        }
        if !config.enabled {
            self.completer.ghost = None;
            self.completer.due = None;
            self.completer.status = "off".into();
        }
        self.completer.config = config;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_range_finds_one_range() {
        assert_eq!(changed_range("hello world", "hello brave world"), (6, 6, "brave ".into()));
        assert_eq!(changed_range("abc", "abc"), (3, 3, String::new()));
        assert_eq!(changed_range("aaa", "aa"), (2, 3, String::new()));
        assert_eq!(changed_range("héllo", "hallo"), (1, 2, "a".into()));
    }

    #[test]
    fn accept_parts() {
        assert_eq!(accept_part("  foo_bar(x)", "word"), 9);
        assert_eq!(accept_part("(x)", "word"), 1);
        assert_eq!(accept_part("abc\ndef", "line"), 3);
        assert_eq!(accept_part("\n    def\nghi", "line"), 8);
        assert_eq!(accept_part("abc\ndef", "all"), 7);
    }
}
