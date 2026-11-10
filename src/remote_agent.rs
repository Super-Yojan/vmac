//! Agents for sessions on another machine run on the window's machine, where
//! you're logged into them: `vmac connect` asks the server for this, and
//! the session's agent then talks to a program the window starts, its lines
//! carried over the connection (`SpawnAgent`, `AgentIn`, `AgentOut`).
//!
//! The agent works in a stand-in folder there, /tmp/vmac/<host>-<project>,
//! holding the project's CLAUDE.md and .claude/ (so it knows the project's
//! rules); every path in its messages is turned from the stand-in to the
//! project's real folder on the way in, and back on the way out, so the
//! sessions only ever see real paths. Its Read, Write, Edit and commands
//! reach the files through vmac; tools that would run in its own process
//! are left out: Grep and Glob, and subagents (Task), whose tools run there
//! too. It's told to search with commands.

use crate::agent::Pipe;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

/// What the window is asked to start.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Spawn {
    pub id: u64,
    pub command: String,
    pub args: Vec<String>,
    /// The stand-in folder it runs in, and files to put there first
    /// (relative path, text).
    pub cwd: PathBuf,
    pub files: Vec<(String, String)>,
}

/// Messages for the window, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum ToWindow {
    Spawn(Spawn),
    Line(u64, String),
    Stop(u64),
}

#[derive(Default)]
struct Relay {
    /// A window that can run agents is attached.
    available: bool,
    next_id: u64,
    outgoing: Vec<ToWindow>,
    /// Each agent's messages, and what it said when it failed.
    inboxes: HashMap<u64, (Sender<Value>, Arc<Mutex<String>>, Translate)>,
}

thread_local! {
    /// The server's: sessions and the daemon's loop share one thread (the
    /// scripting engine can't move between threads), so each thread has
    /// its own relay (and tests running side by side don't mix theirs).
    static RELAY: std::cell::RefCell<Relay> = Default::default();
}

fn with<R>(f: impl FnOnce(&mut Relay) -> R) -> R {
    RELAY.with(|r| f(&mut r.borrow_mut()))
}

/// Whether a window that runs agents is attached (sessions then start
/// their agents there).
pub fn available() -> bool {
    with(|r| r.available)
}

pub fn set_available(yes: bool) {
    with(|r| r.available = yes);
}

/// What to send the window.
pub fn take_outgoing() -> Vec<ToWindow> {
    with(|r| std::mem::take(&mut r.outgoing))
}

/// A line the agent sent.
pub fn deliver(id: u64, line: &str) {
    with(|r| {
        let Some((tx, _, translate)) = r.inboxes.get(&id) else { return };
        if let Ok(message) = serde_json::from_str::<Value>(&translate.inward(line))
            && tx.send(message).is_err()
        {
            r.inboxes.remove(&id);
        }
    });
}

/// The agent stopped: `error` is what it last said on stderr.
pub fn exited(id: u64, error: &str) {
    if let Some((_, last, _)) = with(|r| r.inboxes.remove(&id)) {
        *last.lock().unwrap_or_else(|e| e.into_inner()) = error.to_string();
    }
}

/// Every agent stopped (the window that ran them went away).
pub fn all_exited(why: &str) {
    let ids: Vec<u64> = with(|r| r.inboxes.keys().copied().collect());
    for id in ids {
        exited(id, why);
    }
}

/// Paths between the project's real folder and the stand-in.
#[derive(Clone, Debug)]
struct Translate {
    real: String,
    stand_in: String,
}

impl Translate {
    fn inward(&self, line: &str) -> String {
        // macOS's /tmp is /private/tmp: the agent may say either.
        line.replace(&format!("/private{}", self.stand_in), &self.real).replace(&self.stand_in, &self.real)
    }
    fn outward(&self, line: &str) -> String {
        line.replace(&self.real, &self.stand_in)
    }
}

/// Writes whole lines to the window, paths made the stand-in's.
struct Out {
    id: u64,
    pending: Vec<u8>,
    translate: Translate,
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            let message = ToWindow::Line(self.id, self.translate.outward(&line));
            with(|r| r.outgoing.push(message));
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The stand-in folder for a project on this machine: /tmp/vmac/<host>-<name>.
pub fn stand_in(root: &Path) -> PathBuf {
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "home".into());
    PathBuf::from("/tmp/vmac").join(format!("{}-{name}", hostname()))
}

fn hostname() -> String {
    let name = crate::sys::hostname();
    let name = name.split('.').next().unwrap_or("").to_string();
    if name.is_empty() { "remote".into() } else { name }
}

/// The project's rules for the agent, copied to the stand-in: CLAUDE.md,
/// AGENTS.md and .claude/ (settings, commands, agents), small text files.
fn rules(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut add = |path: &Path| {
        let small = path.metadata().is_ok_and(|m| m.is_file() && m.len() < 256 * 1024);
        if let (true, Ok(rel), Ok(text)) = (small, path.strip_prefix(root), std::fs::read_to_string(path)) {
            out.push((rel.display().to_string(), text));
        }
    };
    for name in ["CLAUDE.md", "CLAUDE.local.md", "AGENTS.md"] {
        add(&root.join(name));
    }
    let dot = root.join(".claude");
    if dot.is_dir() {
        for entry in ignore::WalkBuilder::new(&dot).max_depth(Some(3)).hidden(false).build().filter_map(Result::ok) {
            add(entry.path());
        }
    }
    out
}

/// What claude-code-acp is told for a session on another machine.
pub fn session_meta(root: &Path) -> Value {
    let host = hostname();
    json!({
        "claudeCode": { "options": { "disallowedTools": ["Grep", "Glob", "NotebookEdit", "Task", "Agent"] } },
        "systemPrompt": { "append": format!(
            "This project lives on another machine ({host}); you reach it only through your tools. \
             Read, Write and Edit work on its files, and commands run there. Grep, Glob and subagents \
             (Task) aren't available: search with commands instead (rg, grep -rn, find, ls), run in the \
             project folder. \
             The project folder is {}.", stand_in(root).display()) }
    })
}

/// A pipe to an agent the window starts: `command args`, in the stand-in
/// for `root`.
pub fn pipe(command: &str, args: &[String], root: &Path) -> Pipe {
    let cwd = stand_in(root);
    let translate = Translate { real: root.display().to_string(), stand_in: cwd.display().to_string() };
    let (tx, incoming) = channel();
    let last_error = Arc::new(Mutex::new(String::new()));
    let files = rules(root);
    let id = with(|r| {
        r.next_id += 1;
        let id = r.next_id;
        r.inboxes.insert(id, (tx, last_error.clone(), translate.clone()));
        r.outgoing.push(ToWindow::Spawn(Spawn {
            id,
            command: command.to_string(),
            args: args.to_vec(),
            cwd,
            files,
        }));
        id
    });
    let out = Out { id, pending: Vec::new(), translate };
    let mut stopped = false;
    let stop = Box::new(move || {
        if !stopped {
            stopped = true;
            // Dropped on another thread (or at exit): nothing to tell.
            let _ = RELAY.try_with(|r| {
                let mut r = r.borrow_mut();
                r.inboxes.remove(&id);
                r.outgoing.push(ToWindow::Stop(id));
            });
        }
    });
    Pipe::new(Box::new(out), incoming, last_error, stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_the_stand_ins_there_and_real_here() {
        let root = std::env::temp_dir().join(format!("vmac-remote-agent-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".claude/commands")).unwrap();
        std::fs::write(root.join("CLAUDE.md"), "be nice").unwrap();
        std::fs::write(root.join(".claude/commands/fix.md"), "fix it").unwrap();
        let mut pipe = pipe("claude-code-acp", &[], &root);
        let spawn = take_outgoing()
            .into_iter()
            .find_map(|m| match m {
                ToWindow::Spawn(s) => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(spawn.cwd.starts_with("/tmp/vmac"));
        assert!(spawn.files.contains(&("CLAUDE.md".into(), "be nice".into())), "{:?}", spawn.files);
        assert!(spawn.files.iter().any(|(rel, _)| rel == ".claude/commands/fix.md"));

        // Out: the real folder becomes the stand-in.
        let real = root.join("src/main.rs").display().to_string();
        pipe.write_line(&json!({ "path": real }));
        let sent = take_outgoing();
        let ToWindow::Line(id, line) = &sent[0] else { panic!("{sent:?}") };
        assert_eq!(*id, spawn.id);
        assert!(line.contains(&spawn.cwd.join("src/main.rs").display().to_string()), "{line}");

        // In: the stand-in becomes the real folder.
        deliver(spawn.id, &format!(r#"{{"path":"{}/src/lib.rs"}}"#, spawn.cwd.display()));
        let got = pipe.try_recv().unwrap();
        assert_eq!(got["path"], root.join("src/lib.rs").display().to_string());

        exited(spawn.id, "gone");
        assert!(pipe.try_recv().is_none(), "nothing more after it exits");
        std::fs::remove_dir_all(&root).ok();
    }
}
