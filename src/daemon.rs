//! vmac as a daemon: the sessions live in a server process (`vmac --server`)
//! and windows attach to it over a Unix socket (`vmac attach`), the way tmux
//! clients attach to tmux. Closing the window, or `:q`, only detaches: the
//! sessions, their terminals, language servers and agents keep running.
//!
//! The protocol is a line of JSON per message. A client says `Hello` (which
//! session it wants, and its window's size), then sends `Input`s; the server
//! answers each batch of a client's inputs with the new `Frame` (or
//! `Unchanged`), and sends frames on its own when terminals or language
//! servers change the screen.

use crate::frame::{Frame, Input, Viewport};
use crate::server::{Host, LSP_POLL, RELOAD_POLL, Server, TERMINAL_POLL};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What a client sends.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ToServer {
    /// Attach: to the session named `target`, or a project's session when
    /// it's a folder (relative to `cwd`), made when there's none such.
    /// `agents`: it can run sessions' agents (it's on another machine, where
    /// you're logged into them).
    Hello {
        version: String,
        target: Option<String>,
        cwd: PathBuf,
        viewport: Viewport,
        #[serde(default)]
        agents: bool,
    },
    Input(Input),
    Resize(Viewport),
    /// What's on the clipboard of the machine the window runs on (sent by
    /// windows far from the server, when it changes).
    Clipboard(String),
    /// A line from an agent it runs, and that the agent stopped (with what
    /// it last said on stderr).
    AgentOut { id: u64, line: String },
    AgentExit { id: u64, error: String },
    /// Ask for the sessions' names (and then the server hangs up).
    List,
    KillServer,
}

/// What the server sends.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ToClient {
    /// The screen, and how many of this client's inputs it reflects.
    Frame { frame: Box<Frame>, acks: usize },
    Unchanged { acks: usize },
    /// The active session first, then all of them in order.
    Sessions { active: String, all: Vec<String> },
    /// Detached (`:q`), or the server is going away.
    Bye(String),
    /// Text a session copied, for the clipboard of a window far from the server.
    Clipboard(String),
    /// Start an agent for a session, send it a line, stop it.
    SpawnAgent(crate::remote_agent::Spawn),
    AgentIn { id: u64, line: String },
    AgentStop { id: u64 },
}

/// Bumped when the messages change in a way older servers can't read: each
/// version has its own socket, so a newer window never talks to an older
/// server (stop that one with its own `vmac kill-server`).
const PROTOCOL: u32 = 2;

/// Where the server listens: $VMAC_SOCKET, or `vmac/default` in
/// $XDG_RUNTIME_DIR, or in a folder of your own in the temporary folder.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("VMAC_SOCKET") {
        return PathBuf::from(path);
    }
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("vmac"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("vmac-{user}"))
        }
    };
    dir.join(format!("default-{PROTOCOL}"))
}

fn send_line(stream: &mut impl Write, message: &impl Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_string(message).map_err(std::io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

/// Reads messages off `stream` into `tx` (tagged with `tag`) until it closes
/// or sends something that isn't one; then sends `None`.
fn read_lines<T: for<'de> Deserialize<'de> + Send + 'static, K: Copy + Send + 'static>(
    stream: impl std::io::Read + Send + 'static,
    tag: K,
    tx: Sender<(K, Option<T>)>,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str(&line) else { break };
            if tx.send((tag, Some(message))).is_err() {
                return;
            }
        }
        let _ = tx.send((tag, None));
    });
}

// ---- The server

struct Attached {
    /// Lines to write: a thread per client writes them, so a client that
    /// stops reading never holds up the others.
    out: Sender<String>,
    viewport: Viewport,
    /// The last frame it got, so an unchanged screen isn't sent again.
    shown: Option<Frame>,
    /// Inputs it sent that no frame has answered yet.
    acks: usize,
    hello: bool,
}

impl Attached {
    fn send(&self, message: &ToClient) {
        if let Ok(line) = serde_json::to_string(message) {
            let _ = self.out.send(line);
        }
    }
}

type ClientId = u64;

/// Runs the server `make` makes on `socket` until it's told to stop. Errs
/// when it can't listen there, or another server already does. Listens
/// first, so that nothing the sessions start holds the socket open.
pub fn serve(make: impl FnOnce() -> Server, socket: &Path) -> std::io::Result<()> {
    let listener = listen(socket)?;
    let mut server = make();
    let (tx, rx) = channel::<(ClientId, Option<ToServer>)>();
    let (new_tx, new_rx) = channel::<(ClientId, Sender<String>)>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for (id, stream) in (1..).zip(listener.incoming()) {
                let Ok(stream) = stream else { continue };
                let Ok(reader) = stream.try_clone() else { continue };
                let (out_tx, out_rx) = channel::<String>();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    for mut line in out_rx {
                        line.push('\n');
                        if stream.write_all(line.as_bytes()).and_then(|()| stream.flush()).is_err() {
                            break;
                        }
                    }
                });
                if new_tx.send((id, out_tx)).is_err() {
                    break;
                }
                read_lines(reader, id, tx.clone());
            }
        });
    }
    let result = run(&mut server, rx, new_rx);
    let _ = std::fs::remove_file(socket);
    result
}

fn listen(socket: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = socket.parent() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir)?;
        // Yours alone: whoever can connect can type into your editor.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("a server is already running on {}", socket.display()),
            ));
        }
        // Left by a server that died.
        std::fs::remove_file(socket)?;
    }
    UnixListener::bind(socket)
}

fn run(
    server: &mut Server,
    rx: Receiver<(ClientId, Option<ToServer>)>,
    new_rx: Receiver<(ClientId, Sender<String>)>,
) -> std::io::Result<()> {
    let mut clients: std::collections::BTreeMap<ClientId, Attached> = Default::default();
    // The window that runs sessions' agents (one on another machine).
    let mut agent_window: Option<ClientId> = None;
    let mut agent_windows: Vec<ClientId> = Vec::new();
    let (mut last_lsp, mut last_reload) = (Instant::now(), Instant::now());
    loop {
        // Wait for a message, or until terminals are due to be read.
        let first = match rx.recv_timeout(TERMINAL_POLL) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        while let Ok((id, out)) = new_rx.try_recv() {
            let viewport = Viewport { width: 800., height: 600. };
            clients.insert(id, Attached { out, viewport, shown: None, acks: 0, hello: false });
        }
        let mut changed = false;
        for (id, message) in first.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            let Some(client) = clients.get_mut(&id) else { continue };
            match message {
                None => {
                    clients.remove(&id);
                    agent_windows.retain(|w| *w != id);
                    if agent_window == Some(id) {
                        agent_window = None;
                        crate::remote_agent::all_exited("the window running it closed");
                    }
                }
                Some(ToServer::Hello { target, cwd, viewport, agents, .. }) => {
                    if agents {
                        agent_windows.push(id);
                        // At once: the keys after it may start an agent.
                        agent_window.get_or_insert(id);
                        crate::remote_agent::set_available(true);
                    }
                    client.viewport = viewport;
                    client.acks += 1;
                    client.hello = true;
                    client.shown = None;
                    // No folder (a window on another machine): the home folder.
                    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
                    let cwd = if cwd.as_os_str().is_empty() { home } else { cwd };
                    server.attach(target.as_deref(), &cwd);
                    changed = true;
                }
                Some(ToServer::Input(input)) => {
                    client.acks += 1;
                    server.handle(input);
                    changed = true;
                }
                Some(ToServer::Resize(viewport)) => {
                    client.viewport = viewport;
                    client.acks += 1;
                    client.shown = None;
                    changed = true;
                }
                Some(ToServer::Clipboard(text)) => crate::clipboard::relay_from_window(text),
                Some(ToServer::AgentOut { id: agent, line }) => crate::remote_agent::deliver(agent, &line),
                Some(ToServer::AgentExit { id: agent, error }) => crate::remote_agent::exited(agent, &error),
                Some(ToServer::List) => {
                    let (active, all) = server.names();
                    client.send(&ToClient::Sessions { active, all });
                    clients.remove(&id);
                }
                Some(ToServer::KillServer) => {
                    for client in clients.values() {
                        client.send(&ToClient::Bye("The server stopped".into()));
                    }
                    // Give the writers a moment to say so.
                    std::thread::sleep(Duration::from_millis(50));
                    return Ok(());
                }
            }
        }
        // Agents start in a window that can run them, while there is one.
        if agent_window.is_none() {
            agent_window = agent_windows.last().copied();
        }
        crate::remote_agent::set_available(agent_window.is_some());
        for message in crate::remote_agent::take_outgoing() {
            use crate::remote_agent::ToWindow;
            let Some(window) = agent_window.and_then(|w| clients.get(&w)) else {
                if let ToWindow::Spawn(spawn) = message {
                    crate::remote_agent::exited(spawn.id, "no window to run it");
                }
                continue;
            };
            window.send(&match message {
                ToWindow::Spawn(spawn) => ToClient::SpawnAgent(spawn),
                ToWindow::Line(id, line) => ToClient::AgentIn { id, line },
                ToWindow::Stop(id) => ToClient::AgentStop { id },
            });
        }
        // What sessions copied, for windows on other machines.
        for text in crate::clipboard::relay_take_copied() {
            for client in clients.values() {
                client.send(&ToClient::Clipboard(text.clone()));
            }
        }
        // `:q` detaches the windows; the sessions stay.
        if server.take_quit() {
            for client in clients.values() {
                client.send(&ToClient::Bye("Detached".into()));
            }
            clients.clear();
        }
        changed |= server.pump_terminals();
        if last_lsp.elapsed() >= LSP_POLL {
            last_lsp = Instant::now();
            changed |= server.pump_servers();
        }
        if last_reload.elapsed() >= RELOAD_POLL {
            last_reload = Instant::now();
            changed |= server.reload_if_changed();
        }
        if changed {
            for client in clients.values_mut().filter(|c| c.hello) {
                let frame = server.frame(client.viewport);
                let acks = std::mem::take(&mut client.acks);
                if client.shown.as_ref() == Some(&frame) {
                    client.send(&ToClient::Unchanged { acks });
                } else {
                    client.send(&ToClient::Frame { frame: Box::new(frame.clone()), acks });
                    client.shown = Some(frame);
                }
            }
        }
    }
}

// ---- The client

/// The connection a window has to a server: sends its inputs, keeps the
/// last frame the server sent.
pub struct Client {
    /// Shared with the threads reading agents' output.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Agents it runs for the server's sessions: their input, by id.
    agents: std::collections::HashMap<u64, LocalAgent>,
    incoming: Receiver<((), Option<ToClient>)>,
    /// What carries it (ssh), stopped when the window closes.
    carrier: Option<std::process::Child>,
    /// The server is on another machine: the clipboard goes back and forth.
    remote: bool,
    /// Text the server copied, for this machine's clipboard.
    copied: Option<String>,
    frame: Option<Frame>,
    viewport: Option<Viewport>,
    /// Messages sent (Hello, inputs, sizes) and not answered yet: drawing
    /// waits (briefly) for them, so typing shows at once.
    waiting: usize,
    /// Why the server hung up, once it has.
    gone: Option<String>,
}

/// How long drawing waits for the server to answer what was typed.
const ANSWER_WAIT: Duration = Duration::from_millis(100);

impl Client {
    /// Connects to the server on `socket` and attaches to `target` (a
    /// session's name or a project's folder; None: whichever is active).
    pub fn attach(socket: &Path, target: Option<String>) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket)?;
        let cwd = std::env::current_dir().unwrap_or_default();
        Self::over(Box::new(stream.try_clone()?), Box::new(stream), target, cwd, None)
    }

    /// Attaches over a connection to a server: `reader` and `writer`, which
    /// `carrier` (ssh) may provide; folders in `target` are relative to
    /// `cwd` (empty: the server's home folder). A carrier means the server
    /// is on another machine.
    pub fn over(
        reader: Box<dyn std::io::Read + Send>,
        mut writer: Box<dyn Write + Send>,
        target: Option<String>,
        cwd: PathBuf,
        carrier: Option<std::process::Child>,
    ) -> std::io::Result<Self> {
        let viewport = Viewport { width: 1320., height: 840. };
        let remote = carrier.is_some();
        let hello = ToServer::Hello { version: env!("CARGO_PKG_VERSION").into(), target, cwd, viewport, agents: remote };
        send_line(&mut writer, &hello)?;
        let (tx, incoming) = channel();
        read_lines(reader, (), tx);
        let mut client = Self {
            writer: Arc::new(Mutex::new(writer)),
            agents: Default::default(),
            incoming,
            carrier,
            remote,
            copied: None,
            frame: None,
            viewport: Some(viewport),
            waiting: 0,
            gone: None,
        };
        // The first frame, so the window has something to draw (over ssh,
        // the server may have to start first).
        client.waiting = 1;
        client.receive(Duration::from_secs(30));
        match &client.gone {
            Some(why) => Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, why.clone())),
            None if client.frame.is_none() => {
                Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the server didn't answer"))
            }
            None => Ok(client),
        }
    }

    fn send(&mut self, message: &ToServer) {
        if self.gone.is_none() && send_to(&self.writer, message).is_err() {
            self.gone = Some("Lost the server".into());
        }
    }

    /// Takes what the server sent; waits up to `wait` while inputs are
    /// unanswered. True when there's a new frame (or the server went away).
    fn receive(&mut self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let mut news = false;
        loop {
            let message = if self.waiting > 0 {
                match self.incoming.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(m) => Some(m),
                    Err(_) => None,
                }
            } else {
                self.incoming.try_recv().ok()
            };
            let Some(((), message)) = message else { break };
            match message {
                Some(ToClient::Frame { frame, acks }) => {
                    self.frame = Some(*frame);
                    self.waiting = self.waiting.saturating_sub(acks);
                    news = true;
                }
                Some(ToClient::Unchanged { acks }) => self.waiting = self.waiting.saturating_sub(acks),
                Some(ToClient::Bye(why)) => {
                    self.gone = Some(why);
                    self.waiting = 0;
                    return true;
                }
                Some(ToClient::Sessions { .. }) => {}
                Some(ToClient::SpawnAgent(spawn)) => {
                    let id = spawn.id;
                    match LocalAgent::start(spawn, self.writer.clone()) {
                        Ok(agent) => {
                            self.agents.insert(id, agent);
                        }
                        Err(e) => self.send(&ToServer::AgentExit { id, error: e.to_string() }),
                    }
                }
                Some(ToClient::AgentIn { id, line }) => {
                    if let Some(agent) = self.agents.get_mut(&id) {
                        agent.send(&line);
                    }
                }
                Some(ToClient::AgentStop { id }) => drop(self.agents.remove(&id)),
                // A server on this machine has the clipboard already.
                Some(ToClient::Clipboard(text)) => {
                    if self.remote {
                        self.copied = Some(text);
                    }
                }
                None => {
                    self.gone.get_or_insert_with(|| "Lost the server".into());
                    self.waiting = 0;
                    return true;
                }
            }
        }
        news
    }

    /// Why the server hung up, once it has.
    pub fn gone(&self) -> Option<&str> {
        self.gone.as_deref()
    }
}

fn send_to(writer: &Mutex<Box<dyn Write + Send>>, message: &ToServer) -> std::io::Result<()> {
    send_line(&mut *writer.lock().unwrap_or_else(|e| e.into_inner()), message)
}

/// An agent a window runs for a session on another machine (see
/// remote_agent.rs): its output goes to the server line by line.
struct LocalAgent {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
}

impl LocalAgent {
    fn start(spawn: crate::remote_agent::Spawn, writer: Arc<Mutex<Box<dyn Write + Send>>>) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        // The stand-in folder, with the project's rules.
        std::fs::create_dir_all(&spawn.cwd)?;
        for (rel, text) in &spawn.files {
            let path = spawn.cwd.join(rel);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, text)?;
        }
        let mut child = std::process::Command::new(&spawn.command)
            .args(&spawn.args)
            .current_dir(&spawn.cwd)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped");
        let (stdout, stderr) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
        let id = spawn.id;
        let last_error = Arc::new(Mutex::new(String::new()));
        {
            let last = last_error.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        *last.lock().unwrap_or_else(|e| e.into_inner()) = line.trim().to_string();
                    }
                }
            });
        }
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if send_to(&writer, &ToServer::AgentOut { id, line }).is_err() {
                    return;
                }
            }
            // Let stderr's last words arrive.
            std::thread::sleep(Duration::from_millis(50));
            let error = last_error.lock().map(|e| e.clone()).unwrap_or_default();
            let _ = send_to(&writer, &ToServer::AgentExit { id, error });
        });
        Ok(Self { child, stdin })
    }

    fn send(&mut self, line: &str) {
        let _ = self.stdin.write_all(line.as_bytes()).and_then(|()| self.stdin.write_all(b"\n")).and_then(|()| self.stdin.flush());
    }
}

impl Drop for LocalAgent {
    fn drop(&mut self) {
        // Its whole process group: npx runs the adapter as a child.
        // SAFETY: kill with a negative pid signals that process group.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGTERM);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(mut carrier) = self.carrier.take() {
            let _ = carrier.kill();
            let _ = carrier.wait();
        }
    }
}

impl Host for Client {
    fn handle(&mut self, input: Input) {
        self.waiting += 1;
        self.send(&ToServer::Input(input));
    }

    fn frame(&mut self, viewport: Viewport) -> Frame {
        if self.viewport != Some(viewport) {
            self.viewport = Some(viewport);
            self.waiting += 1;
            self.send(&ToServer::Resize(viewport));
        }
        if self.waiting > 0 {
            self.receive(ANSWER_WAIT);
        }
        self.frame.clone().expect("attach waits for the first frame")
    }

    fn quit_requested(&self) -> bool {
        self.gone.is_some()
    }

    fn pump_terminals(&mut self) -> bool {
        self.receive(Duration::ZERO)
    }

    fn wants_clipboard(&self) -> bool {
        self.remote
    }

    fn take_copied(&mut self) -> Option<String> {
        self.copied.take()
    }

    fn clipboard(&mut self, text: String) {
        self.send(&ToServer::Clipboard(text));
    }
}

/// Carries a connection to the server on `socket` over stdin and stdout
/// (`vmac proxy`, which `vmac connect` runs over ssh), starting the server
/// when none is running.
pub fn proxy(socket: &Path) -> std::io::Result<()> {
    ensure_server(socket)?;
    let stream = UnixStream::connect(socket)?;
    let mut to_server = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = pass(&mut std::io::stdin().lock(), &mut to_server);
        let _ = to_server.shutdown(std::net::Shutdown::Write);
    });
    let mut from_server = stream;
    pass(&mut from_server, &mut std::io::stdout().lock())
}

/// Copies `from` to `to` as it comes, flushing each piece. Not
/// std::io::copy: on Linux it hands sockets to splice/sendfile, which
/// carried nothing over sshd's connection.
fn pass(from: &mut impl std::io::Read, to: &mut impl Write) -> std::io::Result<()> {
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
}

/// Asks the server on `socket` for its sessions.
pub fn list(socket: &Path) -> std::io::Result<(String, Vec<String>)> {
    let mut stream = UnixStream::connect(socket)?;
    send_line(&mut stream, &ToServer::List)?;
    for line in BufReader::new(stream).lines() {
        if let Ok(ToClient::Sessions { active, all }) = serde_json::from_str(&line?) {
            return Ok((active, all));
        }
    }
    Err(std::io::Error::other("the server didn't say"))
}

/// Tells the server on `socket` to stop.
pub fn kill(socket: &Path) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    send_line(&mut stream, &ToServer::KillServer)
}

/// Connects to the server on `socket`, starting one (this program, with
/// `--server`) when none is running.
pub fn ensure_server(socket: &Path) -> std::io::Result<()> {
    if UnixStream::connect(socket).is_ok() {
        return Ok(());
    }
    use std::os::unix::process::CommandExt;
    let log = crate::projects::History::default_dir().map(|d| {
        let _ = std::fs::create_dir_all(&d);
        d.join("server.log")
    });
    let out = match &log {
        Some(path) => std::process::Stdio::from(std::fs::OpenOptions::new().create(true).append(true).open(path)?),
        None => std::process::Stdio::null(),
    };
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("--server")
        .env("VMAC_SOCKET", socket)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(match &log {
            Some(path) => std::process::Stdio::from(std::fs::OpenOptions::new().append(true).open(path)?),
            None => std::process::Stdio::null(),
        });
    // Its own session: neither a Ctrl-C in the terminal it came from nor
    // the end of the ssh connection that started it stops it.
    // SAFETY: setsid is async-signal-safe, and nothing else runs in the
    // child between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn()?;
    for _ in 0..200 {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let hint = log.map(|l| format!(" (see {})", l.display())).unwrap_or_default();
    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("the server didn't start{hint}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use crate::editor::Editor;
    use crate::frame::Content;
    use crate::lisp::Lisp;
    use crate::session::Session;

    fn fresh() -> Session {
        let editor = Editor::new(Buffer::new("*scratch*"));
        Session::new(editor.clone(), Lisp::new(editor))
    }

    fn text(frame: &Frame) -> String {
        match &frame.windows[0].content {
            Content::Text(lines) => lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        }
    }

    fn keys(client: &mut Client, keys: &[&str]) {
        for key in keys {
            let text = if key.chars().count() == 1 { key.to_string() } else { String::new() };
            client.handle(Input::Key { key: key.to_string(), text });
        }
    }

    const VIEW: Viewport = Viewport { width: 800., height: 600. };

    /// A window attaches, types, detaches with :q; the session (and what
    /// was typed) is still there for the next window.
    #[test]
    fn windows_attach_and_detach_and_the_sessions_stay() {
        let dir = std::env::temp_dir().join(format!("vmac-daemon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("socket");
        let windows = {
            let socket = socket.clone();
            std::thread::spawn(move || {
                while UnixStream::connect(&socket).is_err() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let mut client = Client::attach(&socket, None).unwrap();
                keys(&mut client, &["i", "h", "i", "ESC"]);
                let typed = text(&client.frame(VIEW));
                keys(&mut client, &[":", "q", "RET"]);
                let _ = client.frame(VIEW);
                let detached = client.gone().map(String::from);

                let mut again = Client::attach(&socket, None).unwrap();
                let kept = text(&again.frame(VIEW));
                drop(again);
                let beta = Client::attach(&socket, Some("beta".into())).unwrap();
                drop(beta);
                let sessions = list(&socket).unwrap();
                kill(&socket).unwrap();
                (typed, detached, kept, sessions)
            })
        };
        serve(|| Server::new(fresh(), Box::new(fresh)), &socket).unwrap();
        let (typed, detached, kept, sessions) = windows.join().unwrap();
        assert!(typed.starts_with("hi"), "{typed:?}");
        assert_eq!(detached.as_deref(), Some("Detached"));
        assert!(kept.starts_with("hi"), "{kept:?}");
        assert_eq!(sessions, ("beta".into(), vec!["main".into(), "beta".into()]));
        assert!(!socket.exists(), "the socket goes with the server");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A window on "another machine" (a carrier: here a stand-in process)
    /// runs the session's agent; its lines go through the server's relay.
    #[test]
    fn a_far_window_runs_the_sessions_agent() {
        let dir = std::env::temp_dir().join(format!("vmac-daemon-agent-{}", std::process::id()));
        let config = crate::agent::tests::fake_config(&dir);
        let socket = dir.join("socket");
        let windows = {
            let socket = socket.clone();
            let command = config.line();
            std::thread::spawn(move || {
                while UnixStream::connect(&socket).is_err() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let stream = UnixStream::connect(&socket).unwrap();
                let carrier = std::process::Command::new("sleep").arg("60").spawn().unwrap();
                let mut client =
                    Client::over(Box::new(stream.try_clone().unwrap()), Box::new(stream), None, PathBuf::new(), Some(carrier))
                        .unwrap();
                let line = |client: &mut Client, text: &str| {
                    keys(client, &[":"]);
                    for c in text.chars() {
                        let k = if c == ' ' { "SPC".to_string() } else { c.to_string() };
                        client.handle(Input::Key { key: k, text: c.to_string() });
                    }
                    keys(client, &["RET"]);
                };
                line(&mut client, &format!("lua vmac.agent_define('fake', {command:?})"));
                line(&mut client, "agent-use fake");
                line(&mut client, "agent hello there");
                let mut shown = String::new();
                for _ in 0..300 {
                    client.pump_terminals();
                    let frame = client.frame(VIEW);
                    shown = frame
                        .windows
                        .iter()
                        .filter_map(|w| match &w.content {
                            Content::Text(lines) => Some(lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>().join("\n")),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n---\n");
                    if shown.contains("You said: hello there") {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let ran_here = !client.agents.is_empty();
                drop(client);
                kill(&socket).unwrap();
                (shown, ran_here)
            })
        };
        serve(|| Server::new(fresh(), Box::new(fresh)), &socket).unwrap();
        let (shown, ran_here) = windows.join().unwrap();
        assert!(ran_here, "the agent ran in the window's process");
        assert!(shown.contains("You said: hello there"), "{shown}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_server_on_the_same_socket_is_refused() {
        let dir = std::env::temp_dir().join(format!("vmac-daemon-twice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("socket");
        let first = listen(&socket).unwrap();
        assert_eq!(listen(&socket).unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
        drop(first);
        std::fs::remove_file(&socket).unwrap();
        // A socket left by a server that died (one that exited) is cleared away.
        let bind = format!("import socket; socket.socket(socket.AF_UNIX).bind({:?})", socket.display().to_string());
        assert!(std::process::Command::new("python3").args(["-c", &bind]).status().unwrap().success());
        assert!(socket.exists());
        if let Err(e) = listen(&socket) {
            panic!("{e}")
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn messages_survive_the_wire() {
        let hello = ToServer::Hello { version: "1".into(), target: Some("x".into()), cwd: "/tmp".into(), viewport: VIEW, agents: false };
        let line = serde_json::to_string(&hello).unwrap();
        assert_eq!(serde_json::from_str::<ToServer>(&line).unwrap(), hello);
    }
}
