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
use std::time::{Duration, Instant};

/// What a client sends.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ToServer {
    /// Attach: to the session named `target`, or a project's session when
    /// it's a folder (relative to `cwd`), made when there's none such.
    Hello { version: String, target: Option<String>, cwd: PathBuf, viewport: Viewport },
    Input(Input),
    Resize(Viewport),
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
}

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
    dir.join("default")
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
    stream: UnixStream,
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
                }
                Some(ToServer::Hello { target, cwd, viewport, .. }) => {
                    client.viewport = viewport;
                    client.acks += 1;
                    client.hello = true;
                    client.shown = None;
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
    stream: UnixStream,
    incoming: Receiver<((), Option<ToClient>)>,
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
        let mut stream = UnixStream::connect(socket)?;
        let cwd = std::env::current_dir().unwrap_or_default();
        let viewport = Viewport { width: 1320., height: 840. };
        send_line(&mut stream, &ToServer::Hello { version: env!("CARGO_PKG_VERSION").into(), target, cwd, viewport })?;
        let (tx, incoming) = channel();
        read_lines(stream.try_clone()?, (), tx);
        let mut client = Self { stream, incoming, frame: None, viewport: Some(viewport), waiting: 0, gone: None };
        // The first frame, so the window has something to draw.
        client.waiting = 1;
        client.receive(Duration::from_secs(10));
        match &client.gone {
            Some(why) => Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, why.clone())),
            None if client.frame.is_none() => {
                Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the server didn't answer"))
            }
            None => Ok(client),
        }
    }

    fn send(&mut self, message: &ToServer) {
        if self.gone.is_none() && send_line(&mut self.stream, message).is_err() {
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
    std::process::Command::new(std::env::current_exe()?)
        .arg("--server")
        .env("VMAC_SOCKET", socket)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(match &log {
            Some(path) => std::process::Stdio::from(std::fs::OpenOptions::new().append(true).open(path)?),
            None => std::process::Stdio::null(),
        })
        // Its own process group: a Ctrl-C in the terminal it came from
        // doesn't stop it.
        .process_group(0)
        .spawn()?;
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
    use crate::lisp::{Lisp, Source};
    use crate::session::Session;

    fn fresh() -> Session {
        let editor = Editor::new(Buffer::new("*scratch*"));
        Session::new(editor.clone(), Lisp::new(editor, vec![Source { label: "core".into(), path: None }]))
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
        let hello = ToServer::Hello { version: "1".into(), target: Some("x".into()), cwd: "/tmp".into(), viewport: VIEW };
        let line = serde_json::to_string(&hello).unwrap();
        assert_eq!(serde_json::from_str::<ToServer>(&line).unwrap(), hello);
    }
}
