//! A Language Server Protocol client.
//!
//! One server process per (command, project). Messages are JSON-RPC framed
//! with `Content-Length` over the server's stdin/stdout; a thread per server
//! reads its output into a channel, and the editor drains it (`pump`) on a
//! timer. Documents are synced whole (open, change, save, close).
//!
//! Which server runs for which language comes from Scheme's `(lsp-servers)`.

use ropey::Rope;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};

/// A server to run for a language: `("rust" "rust-analyzer")`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub language: String,
    pub command: String,
    pub args: Vec<String>,
}

/// A diagnostic as the server sent it: positions are (line, UTF-16 column)
/// in the text the server last saw. `to_chars` places it in the text now.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub start: (usize, usize),
    pub end: (usize, usize),
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    pub message: String,
    pub source: Option<String>,
}

impl Diagnostic {
    /// Character offsets of its range in `rope`, at least one character wide.
    pub fn to_chars(&self, rope: &Rope) -> (usize, usize) {
        let a = utf16_to_char(rope, self.start);
        let b = utf16_to_char(rope, self.end).max(a);
        if b > a { (a, b) } else { (a, (a + 1).min(rope.len_chars().max(a))) }
    }

    pub fn severity_name(&self) -> &'static str {
        match self.severity {
            1 => "error",
            2 => "warning",
            3 => "info",
            _ => "hint",
        }
    }
}

/// A (line, UTF-16 column) position as a character offset, clamped to the line.
pub fn utf16_to_char(rope: &Rope, (line, col): (usize, usize)) -> usize {
    if line >= rope.len_lines() {
        return rope.len_chars();
    }
    let start = rope.line_to_char(line);
    let line_text = rope.line(line);
    let mut len = line_text.len_chars();
    while len > 0 && matches!(line_text.char(len - 1), '\n' | '\r') {
        len -= 1;
    }
    let base = rope.char_to_utf16_cu(start);
    let end = rope.char_to_utf16_cu(start + len);
    rope.utf16_cu_to_char((base + col).min(end))
}

/// A document the editor has open, as the client needs to see it.
pub struct Doc {
    pub path: PathBuf,
    pub language: &'static str,
    pub revision: u64,
    pub saves: u64,
    pub text: Rope,
}

/// The `languageId` LSP uses for a language.
fn language_id(language: &str) -> &str {
    match language {
        "tsx" => "typescriptreact",
        "javascript" => "javascript",
        other => other,
    }
}

pub fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => uri.push(b as char),
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// Appends a message to the file named by `GMACS_LSP_LOG`, when set: every
/// message to and from servers, for debugging a server that misbehaves.
fn log_traffic(direction: &str, command: &str, message: &Value) {
    if let Some(path) = std::env::var_os("GMACS_LSP_LOG")
        && let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = writeln!(file, "{direction} {command} {message}");
    }
}

/// A message framed for the wire.
pub fn encode(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

/// Reads one framed message; None at the end of the stream.
pub fn read_message(reader: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Ok(None);
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(n) = header.strip_prefix("Content-Length:") {
            length = n.trim().parse::<usize>().ok();
        }
    }
    let Some(length) = length else { return Ok(None) };
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body).ok())
}

struct Client {
    command: String,
    root: PathBuf,
    child: Child,
    stdin: ChildStdin,
    /// The server's last line on stderr: why it stopped, if it does.
    last_error: Arc<Mutex<String>>,
    incoming: Receiver<Value>,
    ready: bool,
    /// Messages written before the server answered `initialize`.
    queued: Vec<Value>,
    /// Open documents: the revision and save count the server last heard, and the version sent.
    open: HashMap<PathBuf, (u64, u64, i32)>,
}

impl Client {
    fn start(config: &ServerConfig, root: &Path) -> std::io::Result<Self> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let last_error = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let last = last_error.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        *last.lock().unwrap_or_else(|e| e.into_inner()) = line;
                    }
                }
            });
        }
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, incoming) = channel();
        let command = config.command.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(message)) = read_message(&mut reader) {
                log_traffic("<-", &command, &message);
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        let mut client = Self {
            command: config.command.clone(),
            root: root.to_path_buf(),
            child,
            stdin,
            last_error,
            incoming,
            ready: false,
            queued: Vec::new(),
            open: HashMap::new(),
        };
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let initialize = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": path_to_uri(root),
                "workspaceFolders": [{ "uri": path_to_uri(root), "name": name }],
                "capabilities": {
                    "textDocument": {
                        "synchronization": { "didSave": true, "dynamicRegistration": false },
                        "publishDiagnostics": { "relatedInformation": false },
                    },
                    "workspace": { "configuration": true, "workspaceFolders": true },
                    "window": { "workDoneProgress": true },
                },
            },
        });
        client.write(&initialize);
        Ok(client)
    }

    fn write(&mut self, message: &Value) {
        log_traffic("->", &self.command, message);
        let _ = self.stdin.write_all(&encode(message)).and_then(|()| self.stdin.flush());
    }

    /// Sends a notification, holding it until the server is initialized.
    fn notify(&mut self, method: &str, params: Value) {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        if self.ready {
            self.write(&message);
        } else {
            self.queued.push(message);
        }
    }

    fn sync(&mut self, doc: &Doc) {
        let uri = path_to_uri(&doc.path);
        match self.open.get(&doc.path).copied() {
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": {
                        "uri": uri, "languageId": language_id(doc.language),
                        "version": 1, "text": doc.text.to_string() } }),
                );
                self.open.insert(doc.path.clone(), (doc.revision, doc.saves, 1));
            }
            Some((revision, saves, version)) => {
                let mut version = version;
                if revision != doc.revision {
                    version += 1;
                    self.notify(
                        "textDocument/didChange",
                        json!({ "textDocument": { "uri": uri, "version": version },
                                "contentChanges": [{ "text": doc.text.to_string() }] }),
                    );
                }
                if saves != doc.saves {
                    self.notify("textDocument/didSave", json!({ "textDocument": { "uri": uri } }));
                }
                self.open.insert(doc.path.clone(), (doc.revision, doc.saves, version));
            }
        }
    }

    fn close(&mut self, path: &Path) {
        self.notify("textDocument/didClose", json!({ "textDocument": { "uri": path_to_uri(path) } }));
        self.open.remove(path);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[derive(Default)]
pub struct Lsp {
    configs: Vec<ServerConfig>,
    clients: Vec<Client>,
    diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    /// (command, root) pairs that failed to start or exited, not retried.
    failed: HashSet<(String, PathBuf)>,
    /// What the servers are doing (progress), for the modeline.
    status: String,
    /// Something worth telling the person (a server failed), taken once.
    notice: Option<String>,
}

impl std::fmt::Debug for Lsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let servers: Vec<_> = self.clients.iter().map(|c| (&c.command, &c.root)).collect();
        f.debug_struct("Lsp").field("servers", &servers).field("status", &self.status).finish()
    }
}

impl Lsp {
    pub fn set_configs(&mut self, configs: Vec<ServerConfig>) {
        self.configs = configs;
    }

    pub fn diagnostics(&self, path: &Path) -> &[Diagnostic] {
        self.diagnostics.get(path).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    /// Sets a file's diagnostics directly (tests).
    #[cfg(test)]
    pub fn set_diagnostics(&mut self, path: PathBuf, list: Vec<Diagnostic>) {
        self.diagnostics.insert(path, list);
    }

    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    /// Names of the servers running for a file's project.
    pub fn servers_for(&self, path: &Path) -> Vec<String> {
        self.clients.iter().filter(|c| c.open.contains_key(path)).map(|c| c.command.clone()).collect()
    }

    /// Reads what the servers sent and syncs the open documents. Returns
    /// whether anything on screen should change.
    pub fn pump(&mut self, docs: &[Doc], project_root: impl Fn(&Path) -> PathBuf) -> bool {
        let mut changed = self.drain();

        // Start servers and sync documents.
        for doc in docs {
            let Some(config) = self.configs.iter().find(|c| c.language == doc.language).cloned() else { continue };
            let root = project_root(&doc.path);
            let key = (config.command.clone(), root.clone());
            if self.failed.contains(&key) {
                continue;
            }
            let index = match self.clients.iter().position(|c| c.command == config.command && c.root == root) {
                Some(i) => i,
                None => match Client::start(&config, &root) {
                    Ok(client) => {
                        self.clients.push(client);
                        self.clients.len() - 1
                    }
                    Err(e) => {
                        self.notice = Some(format!("{}: {e}", config.command));
                        self.failed.insert(key);
                        changed = true;
                        continue;
                    }
                },
            };
            self.clients[index].sync(doc);
        }

        // Close documents that are gone.
        let live: HashSet<&Path> = docs.iter().map(|d| d.path.as_path()).collect();
        for client in &mut self.clients {
            let gone: Vec<PathBuf> = client.open.keys().filter(|p| !live.contains(p.as_path())).cloned().collect();
            for path in gone {
                client.close(&path);
                changed |= self.diagnostics.remove(&path).is_some();
            }
        }
        changed
    }

    fn drain(&mut self) -> bool {
        let mut changed = false;
        let mut exited = Vec::new();
        for (i, client) in self.clients.iter_mut().enumerate() {
            loop {
                match client.incoming.try_recv() {
                    Ok(message) => changed |= handle(client, message, &mut self.diagnostics, &mut self.status),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        exited.push(i);
                        break;
                    }
                }
            }
        }
        for i in exited.into_iter().rev() {
            let client = self.clients.remove(i);
            // Give the stderr reader a moment to catch the server's last words.
            std::thread::sleep(std::time::Duration::from_millis(50));
            let why = client.last_error.lock().map(|l| l.clone()).unwrap_or_default();
            self.notice = Some(if why.is_empty() {
                format!("{} stopped", client.command)
            } else {
                format!("{} stopped: {why}", client.command)
            });
            self.failed.insert((client.command.clone(), client.root.clone()));
            changed = true;
        }
        changed
    }
}

fn position(v: &Value) -> (usize, usize) {
    (v["line"].as_u64().unwrap_or(0) as usize, v["character"].as_u64().unwrap_or(0) as usize)
}

/// Handles one message from a server. Returns whether something visible changed.
fn handle(
    client: &mut Client,
    message: Value,
    diagnostics: &mut HashMap<PathBuf, Vec<Diagnostic>>,
    status: &mut String,
) -> bool {
    let method = message["method"].as_str().unwrap_or_default().to_string();
    let id = message.get("id").cloned();

    // The answer to `initialize`: say we're ready and send what waited.
    if method.is_empty() && id == Some(json!(0)) {
        client.ready = true;
        client.write(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
        // Some servers (pyright) don't start checking until they hear settings.
        client.write(&json!({ "jsonrpc": "2.0", "method": "workspace/didChangeConfiguration",
                              "params": { "settings": {} } }));
        for queued in std::mem::take(&mut client.queued) {
            client.write(&queued);
        }
        return false;
    }

    // Requests from the server need an answer, or some servers wait forever.
    if let (Some(id), false) = (id, method.is_empty()) {
        let result = match method.as_str() {
            "workspace/configuration" => {
                let n = message["params"]["items"].as_array().map_or(0, Vec::len);
                Value::Array(vec![Value::Null; n])
            }
            _ => Value::Null,
        };
        client.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        return false;
    }

    match method.as_str() {
        "textDocument/publishDiagnostics" => {
            let params = &message["params"];
            let Some(path) = params["uri"].as_str().and_then(uri_to_path) else { return false };
            let list = params["diagnostics"]
                .as_array()
                .map(|ds| {
                    ds.iter()
                        .map(|d| Diagnostic {
                            start: position(&d["range"]["start"]),
                            end: position(&d["range"]["end"]),
                            severity: d["severity"].as_u64().unwrap_or(1) as u8,
                            message: d["message"].as_str().unwrap_or_default().to_string(),
                            source: d["source"].as_str().map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default();
            diagnostics.insert(path, list);
            true
        }
        "$/progress" => {
            let value = &message["params"]["value"];
            *status = match value["kind"].as_str() {
                Some("begin") | Some("report") => {
                    let title = value["title"].as_str().or(value["message"].as_str()).unwrap_or_default();
                    format!("{}: {title}", client.command)
                }
                _ => String::new(),
            };
            true
        }
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let message = json!({ "jsonrpc": "2.0", "id": 3, "result": "é" });
        let bytes = encode(&message);
        assert!(bytes.starts_with(b"Content-Length: "));
        let mut reader = BufReader::new(&bytes[..]);
        assert_eq!(read_message(&mut reader).unwrap(), Some(message));
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn uris_round_trip() {
        let path = Path::new("/tmp/a dir/é.rs");
        let uri = path_to_uri(path);
        assert_eq!(uri, "file:///tmp/a%20dir/%C3%A9.rs");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn utf16_columns_become_character_offsets() {
        // "😀" is two UTF-16 units but one character.
        let rope = Rope::from_str("a😀b\nxyz");
        assert_eq!(utf16_to_char(&rope, (0, 3)), 2, "after the emoji: on b");
        assert_eq!(utf16_to_char(&rope, (0, 99)), 3, "clamped to the line's end");
        assert_eq!(utf16_to_char(&rope, (1, 1)), 5);
        assert_eq!(utf16_to_char(&rope, (9, 0)), rope.len_chars());
        let d = Diagnostic { start: (1, 1), end: (1, 1), severity: 1, message: "m".into(), source: None };
        assert_eq!(d.to_chars(&rope), (5, 6), "an empty range covers one character");
    }

    /// A tiny language server in Python: answers initialize, and publishes
    /// one diagnostic per document on the first line, naming the text's
    /// length so changes are visible.
    pub(crate) const FAKE_SERVER: &str = r#"
import sys, json
def read():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        line = line.decode().strip()
        if not line: break
        if line.lower().startswith("content-length:"): length = int(line.split(":")[1])
    return json.loads(sys.stdin.buffer.read(length))
def send(m):
    body = json.dumps(m).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()
while True:
    m = read()
    method = m.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"capabilities": {"textDocumentSync": 1}}})
        send({"jsonrpc": "2.0", "id": 99, "method": "workspace/configuration", "params": {"items": [{}]}})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        doc = m["params"]["textDocument"]
        text = m["params"]["textDocument"].get("text") or m["params"]["contentChanges"][0]["text"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": doc["uri"],
            "diagnostics": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}},
                             "severity": 1, "message": "length %d" % len(text)}]}})
"#;

    fn wait_for(lsp: &mut Lsp, docs: &[Doc], root: &Path, done: impl Fn(&Lsp) -> bool) -> bool {
        for _ in 0..200 {
            lsp.pump(docs, |_| root.to_path_buf());
            if done(lsp) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn talks_to_a_server_and_keeps_its_diagnostics() {
        let dir = std::env::temp_dir().join(format!("gmacs-lsp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let server = dir.join("server.py");
        std::fs::write(&server, FAKE_SERVER).unwrap();
        let file = dir.join("main.fake");

        let mut lsp = Lsp::default();
        lsp.set_configs(vec![ServerConfig {
            language: "rust".into(),
            command: "python3".into(),
            args: vec![server.display().to_string()],
        }]);
        let doc = |text: &str, revision| Doc {
            path: file.clone(),
            language: "rust",
            revision,
            saves: 0,
            text: Rope::from_str(text),
        };
        let docs = vec![doc("hello", 1)];
        assert!(wait_for(&mut lsp, &docs, &dir, |l| !l.diagnostics(&file).is_empty()), "opened and diagnosed");
        assert_eq!(lsp.diagnostics(&file)[0].message, "length 5");
        assert_eq!(lsp.servers_for(&file), vec!["python3"]);

        let docs = vec![doc("hello world", 2)];
        assert!(
            wait_for(&mut lsp, &docs, &dir, |l| l.diagnostics(&file).first().is_some_and(|d| d.message == "length 11")),
            "the change reached the server"
        );

        assert!(wait_for(&mut lsp, &[], &dir, |l| l.diagnostics(&file).is_empty()), "closing drops its diagnostics");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Against the real pyright, when it is installed: `cargo test -- --ignored pyright`.
    #[test]
    #[ignore]
    fn pyright_reports_an_undefined_name() {
        let dir = std::env::temp_dir().join(format!("gmacs-pyright-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.py");
        let text = "x = undefined_name\n";
        std::fs::write(&file, text).unwrap();
        let mut lsp = Lsp::default();
        lsp.set_configs(vec![ServerConfig {
            language: "python".into(),
            command: "pyright-langserver".into(),
            args: vec!["--stdio".into()],
        }]);
        let docs = vec![Doc { path: file.clone(), language: "python", revision: 0, saves: 0, text: Rope::from_str(text) }];
        let mut found = false;
        for _ in 0..300 {
            lsp.pump(&docs, |_| dir.clone());
            if lsp.diagnostics(&file).iter().any(|d| d.message.contains("undefined_name")) {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::fs::remove_dir_all(&dir).ok();
        assert!(found, "pyright's diagnostics: {:?}", lsp.diagnostics(&file));
    }

    /// Against the real rust-analyzer: `cargo test -- --ignored rust_analyzer`.
    #[test]
    #[ignore]
    fn rust_analyzer_reports_a_syntax_error() {
        let dir = std::env::temp_dir().join(format!("gmacs-ra-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        let file = dir.join("src/main.rs");
        let text = "fn main() { let x = ; }\n";
        std::fs::write(&file, text).unwrap();
        let mut lsp = Lsp::default();
        lsp.set_configs(vec![ServerConfig { language: "rust".into(), command: "rust-analyzer".into(), args: vec![] }]);
        let docs = vec![Doc { path: file.clone(), language: "rust", revision: 0, saves: 0, text: Rope::from_str(text) }];
        let mut found = false;
        for _ in 0..600 {
            lsp.pump(&docs, |_| dir.clone());
            if !lsp.diagnostics(&file).is_empty() {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::fs::remove_dir_all(&dir).ok();
        assert!(found, "no diagnostics from rust-analyzer");
    }

    #[test]
    fn a_server_that_exits_says_why() {
        let mut lsp = Lsp::default();
        lsp.set_configs(vec![ServerConfig {
            language: "rust".into(),
            command: "sh".into(),
            args: vec!["-c".into(), "echo 'error: no such toolchain' >&2; exit 1".into()],
        }]);
        let docs = vec![Doc { path: "/tmp/x.rs".into(), language: "rust", revision: 0, saves: 0, text: Rope::new() }];
        let mut notice = None;
        for _ in 0..100 {
            lsp.pump(&docs, |_| PathBuf::from("/tmp"));
            if let Some(n) = lsp.take_notice() {
                notice = Some(n);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(notice.as_deref(), Some("sh stopped: error: no such toolchain"));
    }

    #[test]
    fn a_missing_server_is_reported_once() {
        let mut lsp = Lsp::default();
        lsp.set_configs(vec![ServerConfig { language: "rust".into(), command: "no-such-server-xyz".into(), args: vec![] }]);
        let docs = vec![Doc { path: "/tmp/x.rs".into(), language: "rust", revision: 0, saves: 0, text: Rope::new() }];
        lsp.pump(&docs, |_| PathBuf::from("/tmp"));
        assert!(lsp.take_notice().unwrap().contains("no-such-server-xyz"));
        lsp.pump(&docs, |_| PathBuf::from("/tmp"));
        assert!(lsp.take_notice().is_none(), "not retried every tick");
    }
}
