//! A client for coding agents that speak the Agent Client Protocol (ACP):
//! Claude Code (through claude-code-acp), Codex (codex-acp), Gemini CLI...
//!
//! The agent is a program on stdio speaking JSON-RPC, one message per line.
//! vmac starts it, opens a session in the project, and sends prompts. The
//! agent streams back what it says and does (`session/update`), and asks
//! vmac to read and write files and for permission to run tools. Reading is
//! served from open buffers, so the agent sees unsaved edits; the editor
//! answers those requests (`take_requests`, `respond`), because only it has
//! the buffers and the person.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

/// The ACP version vmac speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// How to start an agent: a program and its arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

impl AgentConfig {
    /// Parses "program arg arg" (split on whitespace).
    pub fn parse(name: &str, line: &str) -> Option<Self> {
        let mut words = line.split_whitespace().map(str::to_string);
        let command = words.next()?;
        Some(Self { name: name.to_string(), command, args: words.collect() })
    }

    /// Claude Code: the adapter when it's installed, else through npx.
    pub fn claude() -> Self {
        if on_path("claude-code-acp") {
            Self { name: "claude".into(), command: "claude-code-acp".into(), args: vec![] }
        } else {
            Self {
                name: "claude".into(),
                command: "npx".into(),
                args: vec!["-y".into(), "@zed-industries/claude-code-acp".into()],
            }
        }
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// One thing in the conversation, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// What the person asked.
    User(String),
    /// What the agent said (chunks of one reply are joined).
    Agent(String),
    /// The agent thinking out loud.
    Thought(String),
    /// A tool the agent runs, updated as it goes.
    Tool(ToolCall),
    /// The agent's plan: each step and its status.
    Plan(Vec<(String, String)>),
    /// Something from vmac: the agent started, stopped, a run ended...
    Notice(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub title: String,
    /// read, edit, delete, move, search, execute, think, fetch, other.
    pub kind: String,
    /// pending, in_progress, completed, failed.
    pub status: String,
    /// The files it touches, and the line when it says.
    pub locations: Vec<(PathBuf, Option<u64>)>,
    /// What it produced, as text (output, diffs summarized).
    pub output: String,
}

impl ToolCall {
    /// Fills in the fields an update carries.
    fn update(&mut self, v: &Value) {
        if let Some(t) = v["title"].as_str() {
            self.title = t.to_string();
        }
        if let Some(k) = v["kind"].as_str() {
            self.kind = k.to_string();
        }
        if let Some(s) = v["status"].as_str() {
            self.status = s.to_string();
        }
        if let Some(list) = v["locations"].as_array() {
            self.locations =
                list.iter().filter_map(|l| Some((PathBuf::from(l["path"].as_str()?), l["line"].as_u64()))).collect();
        }
        if let Some(content) = v["content"].as_array() {
            self.output = content.iter().map(tool_content_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
        }
    }
}

fn tool_content_text(c: &Value) -> String {
    match c["type"].as_str() {
        Some("content") => content_text(&c["content"]),
        Some("diff") => line_diff(c["oldText"].as_str().unwrap_or(""), c["newText"].as_str().unwrap_or("")),
        Some("terminal") => format!("terminal {}", c["terminalId"].as_str().unwrap_or("")),
        _ => String::new(),
    }
}

/// The text of a content block (text, or a resource's text or link).
fn content_text(block: &Value) -> String {
    match block["type"].as_str() {
        Some("text") => block["text"].as_str().unwrap_or("").to_string(),
        Some("resource_link") => block["uri"].as_str().unwrap_or("").to_string(),
        Some("resource") => block["resource"]["text"].as_str().unwrap_or("").to_string(),
        Some("image") => "[image]".into(),
        _ => String::new(),
    }
}

/// An error's message, with the detail agents put in `data` (often the
/// real reason, under a generic "Internal error").
pub fn error_text(error: &Value) -> String {
    let message = error["message"].as_str().unwrap_or("error").to_string();
    let data = &error["data"];
    let detail = data["message"].as_str().or(data["details"].as_str()).unwrap_or("");
    // The detail is sometimes a JSON error from an API: take its message.
    let inner = serde_json::from_str::<Value>(detail).ok();
    let detail = inner
        .as_ref()
        .and_then(|v| v["error"]["message"].as_str().or(v["message"].as_str()))
        .unwrap_or(detail);
    if detail.is_empty() { message } else { format!("{message}: {detail}") }
}

/// A request from the agent that the editor answers.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// The permission the agent is waiting for: what for, and the choices
/// (option id, name, kind: allow_once, allow_always, reject_once, reject_always).
#[derive(Clone, Debug, PartialEq)]
pub struct Permission {
    pub id: Value,
    pub title: String,
    pub options: Vec<(String, String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Pending {
    Initialize,
    NewSession,
    Prompt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Started, the session not open yet.
    Starting,
    /// The session is open; nothing running.
    Idle,
    /// A prompt is running.
    Running,
    /// The program stopped.
    Exited,
}

pub struct Agent {
    pub config: AgentConfig,
    pub root: PathBuf,
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    last_error: Arc<Mutex<String>>,
    next_id: i64,
    pending: HashMap<i64, Pending>,
    session: Option<String>,
    /// Prompts sent before the session opened.
    queued: Vec<String>,
    pub state: State,
    pub entries: Vec<Entry>,
    /// Requests for the editor to answer.
    requests: Vec<Request>,
    /// Permissions asked and not answered yet.
    pub permissions: Vec<Permission>,
    /// Bumped whenever the conversation changes, for redrawing.
    pub revision: u64,
    /// Tool calls shown open, by id.
    expanded: std::collections::HashSet<String>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent").field("config", &self.config).field("state", &self.state).finish()
    }
}

impl Agent {
    /// Starts the agent in `root` and opens a session.
    pub fn start(config: AgentConfig, root: &Path) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .current_dir(root)
            // Its own process group, so stopping it stops what it started
            // (npx runs the adapter as a child).
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let last_error = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let last = last_error.clone();
            std::thread::spawn(move || {
                // The first line that says "Error: why", else the last line.
                let mut named = false;
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let line = line.trim().to_string();
                    if line.is_empty() || named {
                        continue;
                    }
                    named = line.starts_with("Error:") || line.contains("error:");
                    *last.lock().unwrap_or_else(|e| e.into_inner()) = line;
                }
            });
        }
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, incoming) = channel();
        let label = config.name.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                log_traffic("<-", &label, &message);
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        let mut agent = Self {
            config,
            root: root.to_path_buf(),
            child,
            stdin,
            incoming,
            last_error,
            next_id: 0,
            pending: HashMap::new(),
            session: None,
            queued: Vec::new(),
            state: State::Starting,
            entries: Vec::new(),
            requests: Vec::new(),
            permissions: Vec::new(),
            revision: 0,
            expanded: Default::default(),
        };
        agent.call(
            Pending::Initialize,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": true, "writeTextFile": true },
                    "terminal": false,
                },
                "clientInfo": { "name": "vmac", "version": env!("CARGO_PKG_VERSION") },
            }),
        );
        let name = agent.config.name.clone();
        agent.note(format!("Starting {name} in {}", root.display()));
        Ok(agent)
    }

    fn write(&mut self, message: &Value) {
        log_traffic("->", &self.config.name, message);
        let mut line = message.to_string();
        line.push('\n');
        let _ = self.stdin.write_all(line.as_bytes()).and_then(|()| self.stdin.flush());
    }

    fn call(&mut self, kind: Pending, method: &str, params: Value) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, kind);
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Answers a request from the agent.
    pub fn respond(&mut self, id: Value, result: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn respond_error(&mut self, id: Value, message: &str) {
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": message } }));
    }

    fn note(&mut self, text: String) {
        self.entries.push(Entry::Notice(text));
        self.revision += 1;
    }

    /// Sends a prompt; held until the session opens.
    pub fn prompt(&mut self, text: &str) {
        self.prompt_as(text, text);
    }

    /// Sends `text`, showing `shown` in the conversation (a question about
    /// a selection shows the question, not the code sent with it).
    pub fn prompt_as(&mut self, shown: &str, text: &str) {
        self.entries.push(Entry::User(shown.to_string()));
        self.revision += 1;
        match self.session.clone() {
            Some(session) if self.state == State::Idle => self.send_prompt(&session, text),
            _ => self.queued.push(text.to_string()),
        }
    }

    fn send_prompt(&mut self, session: &str, text: &str) {
        self.state = State::Running;
        self.call(
            Pending::Prompt,
            "session/prompt",
            json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
        );
    }

    /// Stops the running prompt. Permissions it was waiting on are cancelled,
    /// as the protocol asks.
    pub fn cancel(&mut self) {
        let Some(session) = self.session.clone() else { return };
        if self.state != State::Running {
            self.queued.clear();
            return;
        }
        for p in std::mem::take(&mut self.permissions) {
            self.respond(p.id, json!({ "outcome": { "outcome": "cancelled" } }));
        }
        self.notify("session/cancel", json!({ "sessionId": session }));
        self.revision += 1;
    }

    /// Answers the oldest permission with one of its options (by index).
    pub fn permit(&mut self, index: usize) -> bool {
        if self.permissions.is_empty() {
            return false;
        }
        let p = self.permissions.remove(0);
        let Some((option, name, _)) = p.options.get(index).cloned() else {
            self.permissions.insert(0, p);
            return false;
        };
        self.respond(p.id, json!({ "outcome": { "outcome": "selected", "optionId": option } }));
        self.note(format!("{name}: {}", p.title));
        true
    }

    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// Reads what the agent sent. Returns whether anything changed.
    pub fn pump(&mut self) -> bool {
        let before = self.revision;
        let mut gone = false;
        loop {
            match self.incoming.try_recv() {
                Ok(message) => self.handle(message),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    gone = true;
                    break;
                }
            }
        }
        if gone && self.state != State::Exited {
            self.state = State::Exited;
            std::thread::sleep(std::time::Duration::from_millis(50));
            let why = self.last_error.lock().map(|l| l.clone()).unwrap_or_default();
            let name = self.config.name.clone();
            self.note(if why.is_empty() { format!("{name} stopped") } else { format!("{name} stopped: {why}") });
        }
        self.revision != before || !self.requests.is_empty()
    }

    fn handle(&mut self, message: Value) {
        let method = message["method"].as_str().map(str::to_string);
        match (method, message.get("id").cloned()) {
            // A reply to one of ours.
            (None, Some(id)) => {
                let Some(kind) = id.as_i64().and_then(|n| self.pending.remove(&n)) else { return };
                if let Some(error) = message.get("error") {
                    self.on_error(kind, error_text(error));
                } else {
                    self.on_result(kind, &message["result"]);
                }
            }
            // A request from the agent.
            (Some(method), Some(id)) => {
                let params = message["params"].clone();
                if method == "session/request_permission" {
                    let title = params["toolCall"]["title"].as_str().unwrap_or("a tool").to_string();
                    let options = params["options"]
                        .as_array()
                        .map(|list| {
                            list.iter()
                                .map(|o| {
                                    let s = |k: &str| o[k].as_str().unwrap_or("").to_string();
                                    (s("optionId"), s("name"), s("kind"))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    self.permissions.push(Permission { id: id.clone(), title, options });
                    self.revision += 1;
                }
                self.requests.push(Request { id, method, params });
            }
            // A notification.
            (Some(method), None) if method == "session/update" => self.on_update(&message["params"]["update"]),
            _ => {}
        }
    }

    fn on_result(&mut self, kind: Pending, result: &Value) {
        match kind {
            Pending::Initialize => {
                let cwd = self.root.display().to_string();
                self.call(Pending::NewSession, "session/new", json!({ "cwd": cwd, "mcpServers": [] }));
            }
            Pending::NewSession => {
                let Some(session) = result["sessionId"].as_str().map(str::to_string) else {
                    self.note("The agent opened no session".into());
                    return;
                };
                self.session = Some(session.clone());
                self.state = State::Idle;
                let name = self.config.name.clone();
                self.note(format!("{name} is ready"));
                if !self.queued.is_empty() {
                    let text = self.queued.remove(0);
                    self.send_prompt(&session, &text);
                }
            }
            Pending::Prompt => {
                self.state = State::Idle;
                let why = result["stopReason"].as_str().unwrap_or("end_turn");
                match why {
                    "end_turn" => self.revision += 1,
                    "cancelled" => self.note("Cancelled".into()),
                    other => self.note(format!("Stopped: {}", other.replace('_', " "))),
                }
                if let (Some(session), false) = (self.session.clone(), self.queued.is_empty()) {
                    let text = self.queued.remove(0);
                    self.send_prompt(&session, &text);
                }
            }
        }
    }

    fn on_error(&mut self, kind: Pending, text: String) {
        match kind {
            Pending::Initialize | Pending::NewSession => {
                // Nothing more will work; stop it so the next prompt starts afresh.
                self.state = State::Exited;
                self.queued.clear();
                kill_tree(&mut self.child);
                // The protocol's error is often generic; what it printed says why.
                std::thread::sleep(std::time::Duration::from_millis(50));
                let why = self.last_error.lock().map(|l| l.clone()).unwrap_or_default();
                let text = if why.is_empty() { text } else { format!("{text} ({why})") };
                self.note(format!("{} couldn't start: {text}", self.config.name));
            }
            Pending::Prompt => {
                self.state = State::Idle;
                self.note(format!("Error: {text}"));
            }
        }
    }

    fn on_update(&mut self, update: &Value) {
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        match kind {
            "agent_message_chunk" | "agent_thought_chunk" => {
                let text = content_text(&update["content"]);
                let thought = kind == "agent_thought_chunk";
                match self.entries.last_mut() {
                    Some(Entry::Agent(s)) if !thought => s.push_str(&text),
                    Some(Entry::Thought(s)) if thought => s.push_str(&text),
                    _ if thought => self.entries.push(Entry::Thought(text)),
                    _ => self.entries.push(Entry::Agent(text)),
                }
            }
            "tool_call" => {
                let mut call = ToolCall { id: update["toolCallId"].as_str().unwrap_or("").to_string(), ..Default::default() };
                call.status = "pending".into();
                call.update(update);
                self.entries.push(Entry::Tool(call));
            }
            "tool_call_update" => {
                let id = update["toolCallId"].as_str().unwrap_or("");
                let found = self.entries.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool(t) if t.id == id => Some(t),
                    _ => None,
                });
                match found {
                    Some(t) => t.update(update),
                    None => {
                        let mut call = ToolCall { id: id.to_string(), ..Default::default() };
                        call.update(update);
                        self.entries.push(Entry::Tool(call));
                    }
                }
            }
            "plan" => {
                let steps = update["entries"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|e| {
                                (e["content"].as_str().unwrap_or("").to_string(), e["status"].as_str().unwrap_or("").to_string())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // One plan, kept up to date where it first appeared.
                match self.entries.iter_mut().find(|e| matches!(e, Entry::Plan(_))) {
                    Some(Entry::Plan(p)) => *p = steps,
                    _ => self.entries.push(Entry::Plan(steps)),
                }
            }
            _ => return,
        }
        self.revision += 1;
    }

    /// Opens or closes a tool call's details.
    pub fn toggle(&mut self, id: &str) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_string());
        }
        self.revision += 1;
    }

    /// The conversation as plain text (tests).
    #[cfg(test)]
    pub fn transcript(&self) -> String {
        self.render(80).text
    }

    /// The conversation for the agent buffer: Markdown, faces to color it,
    /// and what each line points at (a tool call to open, a file to visit).
    pub fn render(&self, width: usize) -> Rendered {
        let width = width.max(20);
        let mut r = Rendered::default();
        let shorten = |s: &str| {
            let root = format!("{}/", self.root.display());
            s.replace(&root, "")
        };
        for entry in &self.entries {
            match entry {
                Entry::User(s) => {
                    if !r.text.is_empty() {
                        r.line("", Target::None);
                    }
                    let mut first = true;
                    for line in s.trim().lines() {
                        for piece in wrap(line, width - 2) {
                            let mark = if first { "› " } else { "  " };
                            first = false;
                            r.styled(&format!("{mark}{piece}"), "agent-user", Target::None);
                        }
                    }
                    r.line("", Target::None);
                }
                Entry::Agent(s) => {
                    if r.lines.last().is_some_and(|t| *t != Target::None) {
                        r.line("", Target::None);
                    }
                    // Wrapped, except code blocks and tables.
                    let mut code = false;
                    for line in s.trim().lines() {
                        let fence = line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~");
                        if code || fence || line.trim_start().starts_with('|') {
                            r.line(line, Target::None);
                        } else {
                            for piece in wrap(line, width) {
                                r.line(&piece, Target::None);
                            }
                        }
                        code ^= fence;
                    }
                    r.line("", Target::None);
                }
                Entry::Thought(s) => {
                    if r.lines.last().is_some_and(|t| *t != Target::None) {
                        r.line("", Target::None);
                    }
                    for line in s.trim().lines() {
                        for piece in wrap(line, width - 2) {
                            r.styled(&format!("  {piece}"), "agent-thought", Target::None);
                        }
                    }
                    r.line("", Target::None);
                }
                Entry::Tool(t) => {
                    let (mark, face) = match t.status.as_str() {
                        "completed" => ("✓", "green"),
                        "failed" => ("✗", "red"),
                        "in_progress" => ("…", "yellow"),
                        _ => ("·", "dim"),
                    };
                    let open = self.expanded.contains(&t.id);
                    let fold = if open { "▾" } else { "▸" };
                    let start = r.chars;
                    r.line(&format!("{fold} {mark} {}", shorten(&t.title)), Target::Tool(t.id.clone()));
                    r.faces.push((start, start + 1, "dim"));
                    r.faces.push((start + 2, start + 3, face));
                    if open {
                        for (path, line) in &t.locations {
                            let shown = shorten(&path.display().to_string());
                            let text = match line {
                                Some(n) => format!("    {shown}:{n}"),
                                None => format!("    {shown}"),
                            };
                            r.styled(&text, "cyan", Target::File(path.clone(), *line));
                        }
                        let lines: Vec<&str> = t.output.lines().collect();
                        for line in lines.iter().take(MAX_TOOL_LINES) {
                            let face = if line.starts_with("+ ") {
                                "green"
                            } else if line.starts_with("- ") {
                                "red"
                            } else {
                                "agent-output"
                            };
                            r.styled(&format!("    {line}"), face, Target::Tool(t.id.clone()));
                        }
                        if lines.len() > MAX_TOOL_LINES {
                            let more = lines.len() - MAX_TOOL_LINES;
                            r.styled(&format!("    … {more} more lines"), "dim", Target::Tool(t.id.clone()));
                        }
                        if t.locations.is_empty() && lines.is_empty() {
                            r.styled("    (no output)", "dim", Target::Tool(t.id.clone()));
                        }
                    }
                }
                Entry::Plan(steps) => {
                    if r.lines.last().is_some_and(|t| *t != Target::None) {
                        r.line("", Target::None);
                    }
                    r.styled("Plan", "agent-heading", Target::None);
                    for (step, status) in steps {
                        let (mark, face) = match status.as_str() {
                            "completed" => ("✓", "green"),
                            "in_progress" => ("▸", "yellow"),
                            _ => ("○", "dim"),
                        };
                        let start = r.chars;
                        r.line(&format!("  {mark} {step}"), Target::None);
                        r.faces.push((start + 2, start + 3, face));
                    }
                    r.line("", Target::None);
                }
                Entry::Notice(s) => {
                    r.styled(&format!("— {s}"), "dim", Target::None);
                    r.line("", Target::None);
                }
            }
        }
        for p in &self.permissions {
            r.styled(&format!("⚠ Allow {}?  SPC a p to answer", shorten(&p.title)), "yellow", Target::None);
        }
        if self.state == State::Running && self.permissions.is_empty() {
            r.styled("… working (SPC a c cancels)", "dim", Target::None);
        }
        r
    }
}

/// Tool output lines shown before "… N more lines".
const MAX_TOOL_LINES: usize = 40;

/// What a line of the agent buffer points at.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    None,
    /// A tool call: RET or TAB opens or closes it.
    Tool(String),
    /// A file (and line) a tool touched: RET visits it.
    File(PathBuf, Option<u64>),
}

/// The agent buffer's text, its faces (character ranges) and each line's target.
#[derive(Debug, Default)]
pub struct Rendered {
    pub text: String,
    pub faces: Vec<(usize, usize, &'static str)>,
    pub lines: Vec<Target>,
    chars: usize,
}

impl Rendered {
    fn line(&mut self, text: &str, target: Target) {
        self.text.push_str(text);
        self.text.push('\n');
        self.chars += text.chars().count() + 1;
        self.lines.push(target);
    }

    fn styled(&mut self, text: &str, face: &'static str, target: Target) {
        let start = self.chars;
        self.line(text, target);
        self.faces.push((start, start + text.chars().count(), face));
    }
}

/// A line broken at spaces into pieces at most `width` characters long
/// (a longer word gets a piece to itself), keeping its indent on each.
pub fn wrap(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width {
        return vec![line.to_string()];
    }
    let indent: String = line.chars().take_while(|c| *c == ' ').collect();
    // List items continue under their text, not their marker.
    let rest = &line[indent.len()..];
    let hang = match rest.split_once(' ') {
        Some((m, _)) if matches!(m, "-" | "*" | "+") || m.trim_end_matches(['.', ')']).parse::<u32>().is_ok() => {
            m.chars().count() + 1
        }
        _ => 0,
    };
    let next_indent = format!("{indent}{}", " ".repeat(hang));
    let mut out = Vec::new();
    let mut current = indent.clone();
    let mut fresh = true;
    for word in rest.split(' ').filter(|w| !w.is_empty()) {
        let len = current.chars().count();
        if !fresh && len + 1 + word.chars().count() > width {
            out.push(std::mem::replace(&mut current, next_indent.clone()));
            fresh = true;
        }
        if !fresh {
            current.push(' ');
        }
        current.push_str(word);
        fresh = false;
    }
    out.push(current);
    out
}

/// Old and new text as the lines that changed: the common start and end
/// dropped, the rest as "- old" and "+ new".
pub fn line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..].iter().rev().zip(b[head..].iter().rev()).take_while(|(x, y)| x == y).count();
    let mut out: Vec<String> = Vec::new();
    out.extend(a[head..a.len() - tail].iter().map(|l| format!("- {l}")));
    out.extend(b[head..b.len() - tail].iter().map(|l| format!("+ {l}")));
    out.join("\n")
}

impl Drop for Agent {
    fn drop(&mut self) {
        kill_tree(&mut self.child);
        let _ = self.child.wait();
    }
}

/// Stops a program and everything in its process group.
fn kill_tree(child: &mut Child) {
    let group = format!("-{}", child.id());
    let _ = Command::new("kill").args(["-TERM", "--", &group]).stderr(Stdio::null()).status();
    let _ = child.kill();
}

fn log_traffic(direction: &str, agent: &str, message: &Value) {
    let Some(path) = std::env::var_os("VMAC_AGENT_LOG") else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{direction} {agent} {message}");
    }
}

/// Lines of `text` for fs/read_text_file: from `line` (1-based), at most `limit`.
pub fn slice_lines(text: &str, line: Option<u64>, limit: Option<u64>) -> String {
    if line.is_none() && limit.is_none() {
        return text.to_string();
    }
    let skip = line.unwrap_or(1).saturating_sub(1) as usize;
    let take = limit.map(|n| n as usize).unwrap_or(usize::MAX);
    text.split_inclusive('\n').skip(skip).take(take).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tiny ACP agent in Python. "edit PATH" reads PATH through the
    /// editor, asks permission, and writes it back upper-cased; "wait" runs
    /// until cancelled; anything else is echoed.
    pub(crate) const FAKE_AGENT: &str = r#"
import sys, json
def send(m):
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
def read():
    line = sys.stdin.readline()
    if not line: sys.exit(0)
    return json.loads(line)
def reply_to(id):
    while True:
        m = read()
        if m.get("id") == id and "method" not in m: return m
def update(session, u):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": u}})
def say(session, text):
    update(session, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})
while True:
    m = read()
    method = m.get("method")
    if method == "initialize":
        assert m["params"]["clientCapabilities"]["fs"]["readTextFile"]
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessionId": "s1"}})
    elif method == "session/prompt":
        s = m["params"]["sessionId"]
        text = m["params"]["prompt"][0]["text"]
        if text.startswith("edit "):
            path = text[5:]
            update(s, {"sessionUpdate": "plan", "entries": [
                {"content": "Read it", "priority": "high", "status": "completed"},
                {"content": "Shout it", "priority": "high", "status": "pending"}]})
            say(s, "Reading ")
            say(s, "it.")
            send({"jsonrpc": "2.0", "id": 100, "method": "fs/read_text_file", "params": {"sessionId": s, "path": path}})
            content = reply_to(100)["result"]["content"]
            update(s, {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit " + path, "kind": "edit", "status": "pending"})
            send({"jsonrpc": "2.0", "id": 101, "method": "session/request_permission", "params": {"sessionId": s,
                  "toolCall": {"toolCallId": "t1", "title": "Edit " + path},
                  "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                              {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}})
            outcome = reply_to(101)["result"]["outcome"]
            if outcome.get("optionId") == "yes":
                send({"jsonrpc": "2.0", "id": 102, "method": "fs/write_text_file", "params": {"sessionId": s, "path": path, "content": content.upper()}})
                reply_to(102)
                update(s, {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"})
            else:
                update(s, {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed"})
            send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
        elif text == "wait":
            say(s, "Thinking for a long time")
            while True:
                c = read()
                if c.get("method") == "session/cancel": break
            send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "cancelled"}})
        else:
            say(s, "You said: " + text)
            send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#;

    pub(crate) fn fake_config(dir: &Path) -> AgentConfig {
        std::fs::create_dir_all(dir).unwrap();
        let script = dir.join("agent.py");
        std::fs::write(&script, FAKE_AGENT).unwrap();
        AgentConfig { name: "fake".into(), command: "python3".into(), args: vec![script.display().to_string()] }
    }

    fn wait_until(agent: &mut Agent, done: impl Fn(&Agent) -> bool) -> bool {
        for _ in 0..250 {
            agent.pump();
            if done(agent) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn prompts_stream_back_and_runs_cancel() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-{}", std::process::id()));
        let mut agent = Agent::start(fake_config(&dir), &dir).unwrap();
        agent.prompt("hello");
        assert!(wait_until(&mut agent, |a| a.state == State::Idle && a.entries.iter().any(|e| matches!(e, Entry::Agent(_)))));
        assert!(agent.entries.contains(&Entry::User("hello".into())));
        assert!(agent.entries.contains(&Entry::Agent("You said: hello".into())), "{:?}", agent.entries);

        agent.prompt("wait");
        assert!(wait_until(&mut agent, |a| a.transcript().contains("long time")));
        assert_eq!(agent.state, State::Running);
        agent.cancel();
        assert!(wait_until(&mut agent, |a| a.state == State::Idle), "{:?}", agent.entries);
        assert!(agent.transcript().contains("— Cancelled"), "{}", agent.transcript());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_or_crashing_agent_says_so() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-gone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = AgentConfig { name: "x".into(), command: "vmac-no-such-agent".into(), args: vec![] };
        assert!(Agent::start(config, &dir).is_err());
        let config = AgentConfig {
            name: "crash".into(),
            command: "sh".into(),
            args: vec!["-c".into(), "echo broken >&2; exit 1".into()],
        };
        let mut agent = Agent::start(config, &dir).unwrap();
        assert!(wait_until(&mut agent, |a| a.state == State::Exited));
        assert!(agent.transcript().contains("crash stopped: broken"), "{}", agent.transcript());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude Code through claude-code-acp, if it can run (npx, a login).
    #[test]
    #[ignore]
    fn a_real_agent_answers_a_prompt() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("answer.txt"), "The answer is 42.\n").unwrap();
        // VMAC_TEST_AGENT="npx -y @zed-industries/codex-acp" tries another.
        let config = std::env::var("VMAC_TEST_AGENT")
            .ok()
            .and_then(|line| AgentConfig::parse("test", &line))
            .unwrap_or_else(AgentConfig::claude);
        let mut agent = Agent::start(config, &dir).unwrap();
        agent.prompt("Read answer.txt and reply with only the number in it.");
        for _ in 0..3000 {
            agent.pump();
            // Allow whatever it asks for (reading the file).
            while !agent.permissions.is_empty() {
                agent.permit(0);
            }
            for r in agent.take_requests() {
                if r.method == "fs/read_text_file" {
                    let path = r.params["path"].as_str().unwrap_or("").to_string();
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    agent.respond(r.id, json!({ "content": text }));
                }
            }
            if agent.state == State::Idle && agent.entries.iter().any(|e| matches!(e, Entry::Agent(_))) {
                break;
            }
            assert_ne!(agent.state, State::Exited, "{}", agent.transcript());
            assert!(!agent.transcript().contains("couldn't start"), "{}", agent.transcript());
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let t = agent.transcript();
        println!("{t}");
        assert!(t.contains("42"), "{t}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lines_are_sliced_one_based() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(slice_lines(text, None, None), text);
        assert_eq!(slice_lines(text, Some(2), Some(2)), "b\nc\n");
        assert_eq!(slice_lines(text, Some(3), None), "c\nd\n");
        assert_eq!(slice_lines(text, None, Some(1)), "a\n");
    }

    #[test]
    fn diffs_show_the_lines_that_changed() {
        assert_eq!(line_diff("a\nb\nc\n", "a\nB\nc\n"), "- b\n+ B");
        assert_eq!(line_diff("", "new\n"), "+ new");
        assert_eq!(line_diff("same", "same"), "");
    }

    #[test]
    fn tool_calls_fold_and_point_at_their_files() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-render-{}", std::process::id()));
        let mut agent = Agent::start(fake_config(&dir), &dir).unwrap();
        agent.on_update(&json!({ "sessionUpdate": "tool_call", "toolCallId": "t1",
            "title": format!("Edit {}/src/a.rs", dir.display()), "status": "completed",
            "locations": [{ "path": format!("{}/src/a.rs", dir.display()), "line": 3 }],
            "content": [{ "type": "diff", "path": "a.rs", "oldText": "x\n", "newText": "y\n" }] }));
        let r = agent.render(80);
        let line = r.text.lines().position(|l| l.contains("Edit src/a.rs")).unwrap();
        assert!(r.text.contains("▸ ✓ Edit src/a.rs"), "paths are shown from the project: {}", r.text);
        assert_eq!(r.lines[line], Target::Tool("t1".into()));
        assert!(!r.text.contains("+ y"));

        agent.toggle("t1");
        let r = agent.render(80);
        assert!(r.text.contains("▾ ✓ Edit src/a.rs\n    src/a.rs:3\n    - x\n    + y\n"), "{}", r.text);
        assert_eq!(r.lines[line + 1], Target::File(dir.join("src/a.rs"), Some(3)));
        assert_eq!(r.text.lines().count(), r.lines.len());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn long_lines_wrap_at_spaces_keeping_indents() {
        assert_eq!(wrap("short", 10), vec!["short"]);
        assert_eq!(wrap("one two three four", 9), vec!["one two", "three", "four"]);
        assert_eq!(wrap("- item with words", 10), vec!["- item", "  with", "  words"]);
        assert_eq!(wrap("  1. aa bb cc", 8), vec!["  1. aa", "     bb", "     cc"]);
        assert_eq!(wrap("averyverylongword x", 5), vec!["averyverylongword", "x"]);
    }

    #[test]
    fn errors_say_why() {
        assert_eq!(error_text(&json!({ "code": 1, "message": "Nope" })), "Nope");
        assert_eq!(
            error_text(&json!({ "message": "Internal error", "data": { "details": "Query closed" } })),
            "Internal error: Query closed"
        );
        let api = r#"{"type":"error","error":{"message":"The model is not supported"}}"#;
        assert_eq!(
            error_text(&json!({ "message": "Internal error", "data": { "message": api } })),
            "Internal error: The model is not supported"
        );
    }

    #[test]
    fn configs_parse_from_a_command_line() {
        let c = AgentConfig::parse("codex", "npx -y codex-acp").unwrap();
        assert_eq!(c.command, "npx");
        assert_eq!(c.args, vec!["-y", "codex-acp"]);
        assert!(AgentConfig::parse("x", "  ").is_none());
    }
}
