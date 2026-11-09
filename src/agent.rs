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
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

/// The ACP version vmac speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// How to start an agent: a program and its arguments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// A CLI agent with its own interface (claude, codex, pi...), run in a
    /// terminal beside your code, rather than an ACP agent in *agent*.
    pub terminal: bool,
    /// For a CLI agent: the command that reopens a past session, `{id}`
    /// standing for its id ("claude --resume {id}"). Empty: it can't.
    pub resume: String,
}

impl AgentConfig {
    /// Parses "program arg arg" (split on whitespace).
    pub fn parse(name: &str, line: &str) -> Option<Self> {
        let mut words = line.split_whitespace().map(str::to_string);
        let command = words.next()?;
        Some(Self { name: name.to_string(), command, args: words.collect(), ..Default::default() })
    }

    /// A CLI agent: `line` runs it in a terminal; `resume` reopens a session.
    pub fn cli(name: &str, line: &str, resume: &str) -> Option<Self> {
        let mut config = Self::parse(name, line)?;
        config.terminal = true;
        config.resume = resume.trim().to_string();
        Some(config)
    }

    /// The agents vmac knows: Claude Code, Codex and Gemini CLI.
    pub fn built_in() -> Vec<Self> {
        let codex = if on_path("codex-acp") { "codex-acp" } else { "npx -y @zed-industries/codex-acp" };
        vec![
            Self::claude(),
            Self::parse("codex", codex).expect("a command"),
            Self::parse("gemini", "gemini --experimental-acp").expect("a command"),
        ]
    }

    /// The command line, as typed.
    pub fn line(&self) -> String {
        std::iter::once(self.command.as_str()).chain(self.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ")
    }

    /// Claude Code: the adapter when it's installed, else through npx.
    pub fn claude() -> Self {
        if on_path("claude-code-acp") {
            Self { name: "claude".into(), command: "claude-code-acp".into(), ..Default::default() }
        } else {
            Self {
                name: "claude".into(),
                command: "npx".into(),
                args: vec!["-y".into(), "@zed-industries/claude-code-acp".into()],
                ..Default::default()
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
    /// Terminals it runs commands in (their ids).
    pub terminals: Vec<String>,
}

impl ToolCall {
    /// Whether it hasn't finished.
    pub fn running(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "in_progress")
    }

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
            self.output = content
                .iter()
                .filter(|c| c["type"] != "terminal")
                .map(tool_content_text)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            for id in content.iter().filter_map(|c| c["terminalId"].as_str()) {
                if !self.terminals.iter().any(|t| t == id) {
                    self.terminals.push(id.to_string());
                }
            }
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

/// What a permission request is about, from its tool call: its content
/// (a plan, a diff), else the plan, command or path it was given.
fn permission_details(call: &Value) -> String {
    let content: Vec<String> = call["content"]
        .as_array()
        .map(|c| c.iter().map(tool_content_text).filter(|t| !t.trim().is_empty()).collect())
        .unwrap_or_default();
    if !content.is_empty() {
        return content.join("\n\n");
    }
    let input = &call["rawInput"];
    for key in ["plan", "command", "description", "file_path", "path", "url", "query"] {
        if let Some(v) = input[key].as_str().filter(|v| !v.trim().is_empty()) {
            return v.to_string();
        }
    }
    String::new()
}

/// A permission's title as a question: "Allow Edit a.rs?", or the title
/// itself when it's one already ("Ready to code?").
pub fn permission_question(title: &str) -> String {
    let title = title.trim();
    if title.ends_with('?') { title.to_string() } else { format!("Allow {title}?") }
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

/// Something hooks hear about.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A tool call appeared or changed status: id, title, kind, status.
    Tool(String, String, String, String),
    /// A prompt finished: why (end_turn, cancelled, max_tokens, refusal, error).
    Done(String),
    /// The sessions it listed (session/list), newest first.
    Sessions(Vec<crate::sessions::Session>),
}

/// A prompt's text and the files attached to it (path, text).
type Prompt = (String, Vec<(PathBuf, String)>);

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
    /// The tool's kind: read, edit, delete, move, search, execute, fetch...
    pub kind: String,
    /// What it's about, to read before answering: a plan, a command, a diff.
    pub details: String,
    pub options: Vec<(String, String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Pending {
    Initialize,
    NewSession,
    /// session/load or session/resume: reopening a past session.
    LoadSession,
    /// session/list.
    List,
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
    pipe: Pipe,
    next_id: i64,
    pending: HashMap<i64, Pending>,
    session: Option<String>,
    /// Prompts sent before the session opened.
    queued: Vec<Prompt>,
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
    /// Whether it takes files' text in prompts (else links to them).
    pub embedded_context: bool,
    /// What happened, for hooks: taken by the editor.
    events: Vec<Event>,
    /// What it can do with past sessions: reopen (load, replaying them, or
    /// resume) and list them.
    pub can_load: bool,
    pub can_resume: bool,
    pub can_list: bool,
    /// Answered initialize (so lists can be asked).
    initialized: bool,
    /// The past session to reopen instead of a new one.
    pub load: Option<String>,
    /// Sent along when a session opens (`_meta`): what claude-code-acp
    /// takes beyond ACP (tools to leave out, words for its system prompt).
    pub meta: Option<Value>,
    /// Replaying a reopened session: its tool calls aren't news for hooks.
    replaying: bool,
    /// A list of sessions asked before initialize was answered (Some(None):
    /// all of them; Some(Some(dir)): those in a folder).
    list_wanted: Option<Option<PathBuf>>,
    /// Terminals the editor runs for it: id -> (buffer name, last lines).
    pub terminals: HashMap<String, (String, String)>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent").field("config", &self.config).field("state", &self.state).finish()
    }
}

impl Agent {
    /// Starts the agent in `root` and opens a session.
    pub fn start(config: AgentConfig, root: &Path) -> std::io::Result<Self> {
        let pipe = Pipe::spawn(&config, root)?;
        Ok(Self::over(config, root, pipe))
    }

    /// Talks to an agent over `pipe` (a program started here, or one on
    /// another machine) as if it ran in `root`, and opens a session.
    pub fn over(config: AgentConfig, root: &Path, pipe: Pipe) -> Self {
        let mut agent = Self {
            config,
            root: root.to_path_buf(),
            pipe,
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
            embedded_context: false,
            events: Vec::new(),
            can_load: false,
            can_resume: false,
            can_list: false,
            initialized: false,
            load: None,
            meta: None,
            replaying: false,
            list_wanted: None,
            terminals: HashMap::new(),
        };
        agent.call(
            Pending::Initialize,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": true, "writeTextFile": true },
                    "terminal": true,
                },
                "clientInfo": { "name": "vmac", "version": env!("CARGO_PKG_VERSION") },
            }),
        );
        let name = agent.config.name.clone();
        agent.note(format!("Starting {name} in {}", root.display()));
        agent
    }

    fn write(&mut self, message: &Value) {
        log_traffic("->", &self.config.name, message);
        let mut line = message.to_string();
        line.push('\n');
        let _ = self.pipe.send.write_all(line.as_bytes()).and_then(|()| self.pipe.send.flush());
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

    /// Adds a line from vmac to the conversation.
    pub fn prompt_notice(&mut self, text: String) {
        self.note(text);
    }

    fn note(&mut self, text: String) {
        self.entries.push(Entry::Notice(text));
        self.revision += 1;
    }

    /// Sends a prompt; held until the session opens.
    #[cfg(test)]
    pub fn prompt(&mut self, text: &str) {
        self.prompt_as(text, text);
    }

    /// Sends `text`, showing `shown` in the conversation (a question about
    /// a selection shows the question, not the code sent with it).
    pub fn prompt_as(&mut self, shown: &str, text: &str) {
        self.prompt_with(shown, text, &[]);
    }

    /// Sends `text` with files attached: their text when the agent takes
    /// it, else links. `files` are (path, text).
    pub fn prompt_with(&mut self, shown: &str, text: &str, files: &[(PathBuf, String)]) {
        self.entries.push(Entry::User(shown.to_string()));
        self.revision += 1;
        let prompt = (text.to_string(), files.to_vec());
        match self.session.clone() {
            Some(session) if self.state == State::Idle => self.send_prompt(&session, prompt),
            _ => self.queued.push(prompt),
        }
    }

    /// Sends a prompt, built now that the agent said what it takes.
    fn send_prompt(&mut self, session: &str, (text, files): Prompt) {
        let mut blocks = vec![json!({ "type": "text", "text": text })];
        for (path, content) in &files {
            let uri = crate::lsp::path_to_uri(path);
            let name = path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
            blocks.push(if self.embedded_context {
                json!({ "type": "resource", "resource": { "uri": uri, "text": content, "mimeType": "text/plain" } })
            } else {
                json!({ "type": "resource_link", "uri": uri, "name": name })
            });
        }
        self.state = State::Running;
        self.call(Pending::Prompt, "session/prompt", json!({ "sessionId": session, "prompt": blocks }));
    }

    /// Shows a terminal's latest lines under the tool calls running in it.
    pub fn set_terminal(&mut self, id: &str, buffer: &str, tail: String) {
        let entry = (buffer.to_string(), tail);
        if self.terminals.get(id) != Some(&entry) {
            self.terminals.insert(id.to_string(), entry);
            self.revision += 1;
        }
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

    /// Asks for its past sessions (all, or those in `dir`): Event::Sessions
    /// brings them. Asked before it's ready, they're asked once it is.
    pub fn list_sessions(&mut self, dir: Option<&Path>) -> bool {
        if !self.initialized {
            self.list_wanted = Some(dir.map(Path::to_path_buf));
            return true;
        }
        if !self.can_list {
            return false;
        }
        let cwd = dir.map(|d| d.display().to_string());
        self.call(Pending::List, "session/list", json!({ "cwd": cwd }));
        true
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// Reads what the agent sent. Returns whether anything changed.
    pub fn pump(&mut self) -> bool {
        let before = self.revision;
        let mut gone = false;
        loop {
            match self.pipe.incoming.try_recv() {
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
            let why = self.pipe.last_error.lock().map(|l| l.clone()).unwrap_or_default();
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
                    let mut kind = params["toolCall"]["kind"].as_str().unwrap_or("").to_string();
                    // The tool call it's about, when the request doesn't say.
                    if kind.is_empty() {
                        let tool = params["toolCall"]["toolCallId"].as_str().unwrap_or("");
                        kind = self
                            .entries
                            .iter()
                            .find_map(|e| match e {
                                Entry::Tool(t) if t.id == tool => Some(t.kind.clone()),
                                _ => None,
                            })
                            .unwrap_or_default();
                    }
                    let details = permission_details(&params["toolCall"]);
                    self.permissions.push(Permission { id: id.clone(), title, kind, details, options });
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
                let caps = &result["agentCapabilities"];
                self.embedded_context = caps["promptCapabilities"]["embeddedContext"] == true;
                self.can_load = caps["loadSession"] == true;
                self.can_resume = caps["sessionCapabilities"]["resume"].is_object();
                self.can_list = caps["sessionCapabilities"]["list"].is_object();
                self.initialized = true;
                let cwd = self.root.display().to_string();
                // `_meta` only when there is some (strict agents refuse null).
                let meta = self.meta.clone();
                let params = |mut v: Value| {
                    if let Some(meta) = &meta {
                        v["_meta"] = meta.clone();
                    }
                    v
                };
                match self.load.take() {
                    // Reopen a past session: load replays it; resume just continues.
                    Some(id) if self.can_load => {
                        self.replaying = true;
                        self.session = Some(id.clone());
                        self.call(Pending::LoadSession, "session/load", params(json!({ "sessionId": id, "cwd": cwd, "mcpServers": [] })));
                    }
                    Some(id) if self.can_resume => {
                        self.session = Some(id.clone());
                        self.call(Pending::LoadSession, "session/resume", params(json!({ "sessionId": id, "cwd": cwd, "mcpServers": [] })));
                        self.note("Resumed: its earlier messages aren't shown (this agent doesn't replay them)".into());
                    }
                    other => {
                        if other.is_some() {
                            self.note(format!("{} can't reopen sessions; starting a new one", self.config.name));
                        }
                        self.call(Pending::NewSession, "session/new", params(json!({ "cwd": cwd, "mcpServers": [] })));
                    }
                }
                if let Some(dir) = self.list_wanted.take() {
                    self.list_sessions(dir.as_deref());
                }
            }
            Pending::LoadSession => {
                self.replaying = false;
                self.state = State::Idle;
                let name = self.config.name.clone();
                self.note(format!("{name} reopened the session; carry on below"));
                if let (Some(session), false) = (self.session.clone(), self.queued.is_empty()) {
                    let prompt = self.queued.remove(0);
                    self.send_prompt(&session, prompt);
                }
            }
            Pending::List => {
                let sessions = result["sessions"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .filter_map(|s| {
                                Some(crate::sessions::Session {
                                    id: s["sessionId"].as_str()?.to_string(),
                                    cwd: PathBuf::from(s["cwd"].as_str().unwrap_or("")),
                                    title: s["title"].as_str().filter(|t| !t.trim().is_empty()).unwrap_or("(untitled)").to_string(),
                                    updated: s["updatedAt"].as_str().map_or(0, crate::sessions::parse_time),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.events.push(Event::Sessions(sessions));
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
                    self.send_prompt(&session, text);
                }
            }
            Pending::Prompt => {
                self.state = State::Idle;
                let why = result["stopReason"].as_str().unwrap_or("end_turn");
                self.events.push(Event::Done(why.to_string()));
                match why {
                    "end_turn" => self.revision += 1,
                    "cancelled" => self.note("Cancelled".into()),
                    other => self.note(format!("Stopped: {}", other.replace('_', " "))),
                }
                if let (Some(session), false) = (self.session.clone(), self.queued.is_empty()) {
                    let text = self.queued.remove(0);
                    self.send_prompt(&session, text);
                }
            }
        }
    }

    fn on_error(&mut self, kind: Pending, text: String) {
        match kind {
            Pending::List => self.note(format!("Couldn't list sessions: {text}")),
            Pending::Initialize | Pending::NewSession | Pending::LoadSession => {
                // Nothing more will work; stop it so the next prompt starts afresh.
                self.state = State::Exited;
                self.queued.clear();
                (self.pipe.stop)();
                // The protocol's error is often generic; what it printed says why.
                std::thread::sleep(std::time::Duration::from_millis(50));
                let why = self.pipe.last_error.lock().map(|l| l.clone()).unwrap_or_default();
                let text = if why.is_empty() { text } else { format!("{text} ({why})") };
                self.note(format!("{} couldn't start: {text}", self.config.name));
            }
            Pending::Prompt => {
                self.state = State::Idle;
                self.events.push(Event::Done("error".into()));
                self.note(format!("Error: {text}"));
            }
        }
    }

    fn on_update(&mut self, update: &Value) {
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        match kind {
            // A reopened session's own prompts, replayed.
            "user_message_chunk" => {
                let text = content_text(&update["content"]);
                match self.entries.last_mut() {
                    Some(Entry::User(s)) => s.push_str(&text),
                    _ => self.entries.push(Entry::User(text)),
                }
            }
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
                if !self.replaying {
                    self.events.push(Event::Tool(call.id.clone(), call.title.clone(), call.kind.clone(), call.status.clone()));
                }
                self.entries.push(Entry::Tool(call));
            }
            "tool_call_update" => {
                let id = update["toolCallId"].as_str().unwrap_or("");
                let found = self.entries.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool(t) if t.id == id => Some(t),
                    _ => None,
                });
                let event = |t: &ToolCall| Event::Tool(t.id.clone(), t.title.clone(), t.kind.clone(), t.status.clone());
                match found {
                    Some(t) => {
                        let before = t.status.clone();
                        t.update(update);
                        if t.status != before && !self.replaying {
                            let e = event(t);
                            self.events.push(e);
                        }
                    }
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

    /// A path as shown: from the project when it's in it.
    fn shorten(&self, s: &str) -> String {
        s.replace(&format!("{}/", self.root.display()), "")
    }

    /// The conversation for the agent buffer: Markdown, faces to color it,
    /// boxes behind some lines (your messages, the permission asked), and
    /// what each line points at (a tool call to open, a file to visit).
    pub fn render(&self, width: usize) -> Rendered {
        let width = width.max(20);
        let mut r = Rendered::default();
        let running = self.state == State::Running;
        let last = self.entries.len().saturating_sub(1);
        // Quiet lines (thinking, tool calls) stack without gaps between them.
        let mut quiet = false;
        let mut i = 0;
        while i < self.entries.len() {
            let entry = &self.entries[i];
            let was_quiet = std::mem::replace(&mut quiet, matches!(entry, Entry::Thought(_) | Entry::Tool(_)));
            if !(quiet && was_quiet) {
                r.gap();
            }
            match entry {
                // Yours: a bubble on the right.
                Entry::User(s) => {
                    let inner = (width * 3 / 4).clamp(16, width - 2);
                    let pieces: Vec<String> = s.trim().lines().flat_map(|l| wrap(l, inner)).collect();
                    let block = pieces.iter().map(|p| p.chars().count()).max().unwrap_or(0);
                    let from = width.saturating_sub(block + 1);
                    for piece in &pieces {
                        r.styled(&format!("{}{piece}", " ".repeat(from)), "agent-user", Target::None);
                        r.band(from, from + block, "bubble");
                    }
                }
                Entry::Agent(s) => {
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
                }
                // Thinking: one line, opened with TAB.
                Entry::Thought(s) => {
                    let key = format!("thought:{i}");
                    let open = self.expanded.contains(&key);
                    let label = if running && i == last { "Thinking…" } else { "Thought" };
                    let fold = if open { "▾" } else { "▸" };
                    r.styled(&format!("{fold} {label}"), "agent-thought", Target::Tool(key.clone()));
                    if open {
                        for line in s.trim().lines() {
                            for piece in wrap(line, width - 4) {
                                r.styled(&format!("    {piece}"), "agent-thought", Target::Tool(key.clone()));
                            }
                        }
                    }
                }
                // Tool calls in a row: one line saying what they did, opened
                // with TAB; the one running shows under it.
                Entry::Tool(_) => {
                    let mut tools = Vec::new();
                    while let Some(Entry::Tool(t)) = self.entries.get(i) {
                        tools.push(t);
                        i += 1;
                    }
                    i -= 1;
                    if let [t] = tools[..] {
                        self.render_tool(&mut r, t, "", width);
                    } else {
                        let key = format!("group:{}", tools[0].id);
                        let open = self.expanded.contains(&key);
                        let busy = tools.iter().any(|t| t.running());
                        let failed = tools.iter().filter(|t| t.status == "failed").count();
                        let (mark, face) = match (busy, failed) {
                            (true, _) => ("…", "yellow"),
                            (false, 0) => ("✓", "green"),
                            _ => ("✗", "red"),
                        };
                        let fold = if open { "▾" } else { "▸" };
                        let failures = if failed > 0 { format!(" · {failed} failed") } else { String::new() };
                        let start = r.chars;
                        r.line(&format!("{fold} {mark} {}{failures}", tools_summary(&tools)), Target::Tool(key));
                        r.faces.push((start, start + 1, "dim"));
                        r.faces.push((start + 2, start + 3, face));
                        for t in tools.iter().filter(|t| open || t.running()) {
                            self.render_tool(&mut r, t, "  ", width);
                        }
                    }
                }
                Entry::Plan(steps) => {
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
                }
                Entry::Notice(s) => {
                    for (i, piece) in wrap(&format!("— {s}"), width).into_iter().enumerate() {
                        let piece = if i == 0 { piece } else { format!("  {piece}") };
                        r.styled(&piece, "dim", Target::None);
                    }
                }
            }
            i += 1;
        }
        // Permissions: the first can be answered here, in a box: its details
        // to read first, then its choices (RET on one, or its number).
        for (n, p) in self.permissions.iter().enumerate() {
            let question = permission_question(&self.shorten(&p.title));
            r.gap();
            if n > 0 {
                r.styled(&format!("⚠ {question}  (after the one above)"), "yellow", Target::None);
                continue;
            }
            let boxed = |r: &mut Rendered| r.band(0, width, "ask");
            r.styled(&format!("⚠ {question}"), "agent-ask", Target::Ask);
            boxed(&mut r);
            if !p.details.trim().is_empty() {
                r.line("", Target::Ask);
                boxed(&mut r);
                let mut code = false;
                for line in p.details.trim().lines() {
                    let fence = line.trim_start().starts_with("```");
                    if line.trim().is_empty() {
                        r.line("", Target::Ask);
                        boxed(&mut r);
                    } else if code || fence {
                        r.line(&format!("  {line}"), Target::Ask);
                        boxed(&mut r);
                    } else {
                        for piece in wrap(line, width.saturating_sub(2)) {
                            r.line(&format!("  {piece}"), Target::Ask);
                            boxed(&mut r);
                        }
                    }
                    code ^= fence;
                }
                r.line("", Target::Ask);
                boxed(&mut r);
            }
            for (i, (_, name, kind)) in p.options.iter().enumerate() {
                let face = if kind.starts_with("allow") { "green" } else { "red" };
                let start = r.chars;
                r.line(&format!("  {}  {name}", i + 1), Target::Permission(i));
                r.faces.push((start + 2, start + 3, face));
                boxed(&mut r);
            }
            r.styled("  RET or its number answers · SPC a p picks from a list", "dim", Target::None);
            boxed(&mut r);
        }
        if running && self.permissions.is_empty() {
            r.gap();
            r.styled("… working (SPC a c cancels)", "dim", Target::None);
        }
        if !r.text.is_empty() {
            r.gap();
        }
        r
    }

    /// One tool call: its line, and opened, the files it touched and what
    /// it produced (or the terminal it runs in).
    fn render_tool(&self, r: &mut Rendered, t: &ToolCall, indent: &str, width: usize) {
        let (mark, face) = match t.status.as_str() {
            "completed" => ("✓", "green"),
            "failed" => ("✗", "red"),
            "in_progress" => ("…", "yellow"),
            _ => ("·", "dim"),
        };
        let open = self.expanded.contains(&t.id);
        let fold = if open { "▾" } else { "▸" };
        let at = r.chars + indent.chars().count();
        r.line(&format!("{indent}{fold} {mark} {}", self.shorten(&t.title)), Target::Tool(t.id.clone()));
        r.faces.push((at, at + 1, "dim"));
        r.faces.push((at + 2, at + 3, face));
        let pad = format!("{indent}    ");
        for id in &t.terminals {
            let Some((buffer, tail)) = self.terminals.get(id) else { continue };
            if open {
                r.styled(&format!("{pad}⌁ {buffer}  (RET opens it)"), "cyan", Target::Buffer(buffer.clone()));
                let lines: Vec<&str> = tail.lines().collect();
                for line in &lines[lines.len().saturating_sub(MAX_TERMINAL_LINES)..] {
                    r.styled(&format!("{pad}{line}"), "agent-output", Target::Buffer(buffer.clone()));
                }
            } else if t.running() && let Some(last) = tail.lines().rev().find(|l| !l.trim().is_empty()) {
                // Closed but running: its latest line.
                let last: String = last.trim().chars().take(width.saturating_sub(pad.len() + 2)).collect();
                r.styled(&format!("{pad}{last}"), "dim", Target::Buffer(buffer.clone()));
            }
        }
        if !open {
            return;
        }
        for (path, line) in &t.locations {
            let shown = self.shorten(&path.display().to_string());
            let text = match line {
                Some(n) => format!("{pad}{shown}:{n}"),
                None => format!("{pad}{shown}"),
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
            r.styled(&format!("{pad}{line}"), face, Target::Tool(t.id.clone()));
        }
        if lines.len() > MAX_TOOL_LINES {
            let more = lines.len() - MAX_TOOL_LINES;
            r.styled(&format!("{pad}… {more} more lines"), "dim", Target::Tool(t.id.clone()));
        }
        if t.locations.is_empty() && lines.is_empty() && t.terminals.is_empty() {
            r.styled(&format!("{pad}(no output)"), "dim", Target::Tool(t.id.clone()));
        }
    }
}

/// What tool calls did, in a line: "Read 2 files, edited 1 file and ran a command".
pub fn tools_summary(tools: &[&ToolCall]) -> String {
    const KINDS: [(&str, &str, &str, &str); 7] = [
        ("read", "read", "file", "files"),
        ("edit", "edited", "file", "files"),
        ("delete", "deleted", "file", "files"),
        ("move", "moved", "file", "files"),
        ("search", "searched", "once", "times"),
        ("execute", "ran", "command", "commands"),
        ("fetch", "fetched", "page", "pages"),
    ];
    let count = |kind: &str| tools.iter().filter(|t| t.kind == kind).count();
    let mut parts: Vec<String> = KINDS
        .iter()
        .filter_map(|&(kind, verb, one, many)| match count(kind) {
            0 => None,
            1 if kind == "search" => Some(format!("{verb} {one}")),
            1 if kind == "execute" => Some(format!("{verb} a {one}")),
            1 => Some(format!("{verb} 1 {one}")),
            n => Some(format!("{verb} {n} {many}")),
        })
        .collect();
    let others = tools.iter().filter(|t| !KINDS.iter().any(|k| k.0 == t.kind)).count();
    match others {
        0 => {}
        1 => parts.push("took 1 other step".into()),
        n => parts.push(format!("took {n} other steps")),
    }
    let mut text = match parts.split_last() {
        Some((tail, [])) => tail.clone(),
        Some((tail, head)) => format!("{} and {tail}", head.join(", ")),
        None => String::new(),
    };
    if let Some(c) = text.get(..1) {
        text = c.to_uppercase() + &text[1..];
    }
    text
}

/// Tool output lines shown before "… N more lines".
const MAX_TOOL_LINES: usize = 40;

/// A terminal's last lines shown under its tool call.
const MAX_TERMINAL_LINES: usize = 12;

/// What a line of the agent buffer points at.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    None,
    /// A tool call: RET or TAB opens or closes it.
    Tool(String),
    /// A file (and line) a tool touched: RET visits it.
    File(PathBuf, Option<u64>),
    /// A buffer (the terminal a command runs in): RET shows it.
    Buffer(String),
    /// The permission waiting: its question and details.
    Ask,
    /// One of its choices: RET picks it.
    Permission(usize),
    /// The changes waiting for review: RET lists them.
    Review,
}

/// The agent buffer's text, its faces (character ranges) and each line's target.
#[derive(Debug, Default)]
pub struct Rendered {
    pub text: String,
    pub faces: Vec<(usize, usize, &'static str)>,
    pub lines: Vec<Target>,
    /// Boxes behind lines: (line, from column, to column, kind).
    pub bands: Vec<(usize, usize, usize, &'static str)>,
    chars: usize,
}

impl Rendered {
    pub fn line(&mut self, text: &str, target: Target) {
        self.text.push_str(text);
        self.text.push('\n');
        self.chars += text.chars().count() + 1;
        self.lines.push(target);
    }

    pub fn styled(&mut self, text: &str, face: &'static str, target: Target) {
        let start = self.chars;
        self.line(text, target);
        self.faces.push((start, start + text.chars().count(), face));
    }

    /// Puts the line just added in a box from column `from` to `to`: a
    /// "bubble" (your message), "ask" (a permission), "card" or "input".
    pub fn band(&mut self, from: usize, to: usize, kind: &'static str) {
        self.bands.push((self.lines.len() - 1, from, to, kind));
    }

    /// A blank line, unless there is one (or nothing yet).
    fn gap(&mut self) {
        if !self.text.is_empty() && !self.text.ends_with("\n\n") {
            self.line("", Target::None);
        }
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
        (self.pipe.stop)();
    }
}

/// How the agent's messages travel: lines of JSON written to `send`, and
/// the ones it sends back on `incoming`. Usually a program's stdin and
/// stdout; it can as well be lines carried from the machine a client runs
/// on, so an agent you're logged into there edits files here.
pub struct Pipe {
    send: Box<dyn Write + Send>,
    incoming: Receiver<Value>,
    /// What it printed on stderr that says why it failed.
    last_error: Arc<Mutex<String>>,
    /// Stops it (called again on drop: must not mind).
    stop: Box<dyn FnMut() + Send>,
}

#[cfg(test)]
impl Pipe {
    pub fn write_line(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.send.write_all(line.as_bytes()).unwrap();
    }

    pub fn try_recv(&self) -> Option<Value> {
        self.incoming.try_recv().ok()
    }
}

impl Pipe {
    /// Messages written to `send`, read from `incoming`; `last_error` says
    /// why it failed, once it has.
    pub fn new(
        send: Box<dyn Write + Send>,
        incoming: Receiver<Value>,
        last_error: Arc<Mutex<String>>,
        stop: Box<dyn FnMut() + Send>,
    ) -> Self {
        Self { send, incoming, last_error, stop }
    }

    /// Runs the agent's program in `root`.
    fn spawn(config: &AgentConfig, root: &Path) -> std::io::Result<Self> {
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
        let mut child = Some(child);
        let stop = Box::new(move || {
            if let Some(mut child) = child.take() {
                kill_tree(&mut child);
                let _ = child.wait();
            }
        });
        Ok(Self { send: Box::new(stdin), incoming, last_error, stop })
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
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": 1,
              "agentCapabilities": {"promptCapabilities": {"embeddedContext": True},
                                    "loadSession": True, "sessionCapabilities": {"list": {}}}}})
    elif method == "session/list":
        cwd = m["params"].get("cwd") or "/"
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"sessions": [
            {"sessionId": "s-new", "cwd": cwd, "title": "Second session", "updatedAt": "2026-11-07T22:00:00Z"},
            {"sessionId": "s-old", "cwd": cwd, "title": "First session"}]}})
    elif method == "session/load":
        sid = m["params"]["sessionId"]
        update(sid, {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "earlier question " + sid}})
        update(sid, {"sessionUpdate": "tool_call", "toolCallId": "old1", "title": "Read old.txt", "kind": "read", "status": "completed"})
        say(sid, "earlier answer")
        send({"jsonrpc": "2.0", "id": m["id"], "result": {}})
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
                  "toolCall": {"toolCallId": "t1", "title": "Edit " + path,
                               "content": [{"type": "content", "content": {"type": "text", "text": "Shout the file.\n\n- read it\n- upper-case it"}}]},
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
        elif text.startswith("patch "):
            # Line 2 upper-cased, line 4 deleted, a line added at the end.
            for path in text[6:].split():
                send({"jsonrpc": "2.0", "id": 110, "method": "fs/read_text_file", "params": {"sessionId": s, "path": path}})
                lines = reply_to(110)["result"]["content"].split("\n")
                lines[1] = lines[1].upper()
                del lines[3]
                lines.insert(len(lines) - 1, "added")
                send({"jsonrpc": "2.0", "id": 111, "method": "fs/write_text_file", "params": {"sessionId": s, "path": path, "content": "\n".join(lines)}})
                reply_to(111)
            say(s, "Patched.")
            send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
        elif text.startswith("run "):
            # Runs a shell line in the editor's terminal (the whole line as
            # the command, as claude-code-acp sends it), waits, reads it.
            update(s, {"sessionUpdate": "tool_call", "toolCallId": "r1", "title": "Run it", "kind": "execute", "status": "in_progress"})
            send({"jsonrpc": "2.0", "id": 120, "method": "terminal/create", "params": {"sessionId": s,
                  "command": text[4:], "env": [{"name": "GREETING", "value": "hi there"}], "outputByteLimit": 1000}})
            tid = reply_to(120)["result"]["terminalId"]
            update(s, {"sessionUpdate": "tool_call_update", "toolCallId": "r1", "content": [{"type": "terminal", "terminalId": tid}]})
            send({"jsonrpc": "2.0", "id": 121, "method": "terminal/wait_for_exit", "params": {"sessionId": s, "terminalId": tid}})
            code = reply_to(121)["result"]["exitCode"]
            send({"jsonrpc": "2.0", "id": 122, "method": "terminal/output", "params": {"sessionId": s, "terminalId": tid}})
            out = reply_to(122)["result"]["output"]
            send({"jsonrpc": "2.0", "id": 123, "method": "terminal/release", "params": {"sessionId": s, "terminalId": tid}})
            reply_to(123)
            update(s, {"sessionUpdate": "tool_call_update", "toolCallId": "r1", "status": "completed"})
            say(s, "exit %d: %s" % (code, out.replace("\n", " / ")))
            send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
        elif text == "wait":
            say(s, "Thinking for a long time")
            while True:
                c = read()
                if c.get("method") == "session/cancel": break
            send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "cancelled"}})
        else:
            files = [b for b in m["params"]["prompt"][1:]]
            if files:
                names = ", ".join(b["resource"]["uri"].rsplit("/", 1)[-1] + "=" + b["resource"]["text"].strip() for b in files)
                say(s, "Attached: " + names + ". ")
            say(s, "You said: " + text)
            send({"jsonrpc": "2.0", "id": m["id"], "result": {"stopReason": "end_turn"}})
"#;

    pub(crate) fn fake_config(dir: &Path) -> AgentConfig {
        std::fs::create_dir_all(dir).unwrap();
        let script = dir.join("agent.py");
        std::fs::write(&script, FAKE_AGENT).unwrap();
        AgentConfig { name: "fake".into(), command: "python3".into(), args: vec![script.display().to_string()], ..Default::default() }
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

    /// An agent whose lines come from elsewhere (as they will from a
    /// client's machine): a channel in, a writer out.
    #[test]
    fn an_agent_over_a_carried_pipe() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-pipe-{}", std::process::id()));
        let config = fake_config(&dir);
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(serde_json::from_str(&line).unwrap());
            }
        });
        let pipe = Pipe::new(Box::new(stdin), rx, Default::default(), Box::new(move || drop(child.kill())));
        let mut agent = Agent::over(config, &dir, pipe);
        agent.prompt("hello");
        assert!(wait_until(&mut agent, |a| a.entries.contains(&Entry::Agent("You said: hello".into()))), "{:?}", agent.entries);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_or_crashing_agent_says_so() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-gone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = AgentConfig { name: "x".into(), command: "vmac-no-such-agent".into(), ..Default::default() };
        assert!(Agent::start(config, &dir).is_err());
        let config = AgentConfig {
            name: "crash".into(),
            command: "sh".into(),
            args: vec!["-c".into(), "echo broken >&2; exit 1".into()],
            ..Default::default()
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
    fn thinking_and_tool_calls_in_a_row_fold_into_a_line() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-groups-{}", std::process::id()));
        let mut agent = Agent::start(fake_config(&dir), &dir).unwrap();
        agent.entries.clear();
        agent.entries.push(Entry::User("fix it".into()));
        agent.entries.push(Entry::Thought("Let me look first.".into()));
        for (id, kind, status) in [("t1", "read", "completed"), ("t2", "read", "completed"), ("t3", "edit", "failed")] {
            agent.entries.push(Entry::Tool(ToolCall {
                id: id.into(),
                title: format!("{kind} {id}"),
                kind: kind.into(),
                status: status.into(),
                ..Default::default()
            }));
        }
        let r = agent.render(40);
        assert!(r.text.contains("▸ Thought\n▸ ✗ Read 2 files and edited 1 file · 1 failed\n"), "{}", r.text);
        assert!(!r.text.contains("Let me look") && !r.text.contains("read t1"), "{}", r.text);
        // Your message: on the right, in a bubble.
        let bubble = r.text.lines().position(|l| l.trim() == "fix it").unwrap();
        assert_eq!(r.text.lines().nth(bubble).unwrap(), format!("{}fix it", " ".repeat(33)));
        assert_eq!(r.bands, vec![(bubble, 33, 39, "bubble")]);

        let group = r.text.lines().position(|l| l.contains("Read 2 files")).unwrap();
        assert_eq!(r.lines[group], Target::Tool("group:t1".into()));
        agent.toggle("group:t1");
        agent.toggle(&format!("thought:{}", 1));
        let r = agent.render(40);
        assert!(r.text.contains("▾ Thought\n    Let me look first.\n▾ ✗ Read"), "{}", r.text);
        assert!(r.text.contains("  ▸ ✓ read t1\n  ▸ ✓ read t2\n  ▸ ✗ edit t3\n"), "{}", r.text);

        // Running, the step under way shows while the rest stay folded.
        agent.toggle("group:t1");
        if let Some(Entry::Tool(t)) = agent.entries.last_mut() {
            t.status = "in_progress".into();
        }
        let r = agent.render(40);
        assert!(r.text.contains("▸ … Read 2 files and edited 1 file\n  ▸ … edit t3\n"), "{}", r.text);
        assert_eq!(tools_summary(&[&ToolCall { kind: "execute".into(), ..Default::default() }]), "Ran a command");
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
