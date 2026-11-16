//! The sessions a window can show, like tmux's: projects open side by side,
//! each with its own buffers, windows, terminals, language servers and agent.
//! The window talks to the server as it would to one session (`Input` in,
//! `Frame` out) and sees the active one; a session's commands (SPC S ...,
//! opening another project) ask the server to switch, make, rename or kill
//! sessions.

use crate::editor::SessionRequest;
use crate::frame::{Frame, Input, Segment, SessionInfo, Viewport};
use crate::session::Session;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub type SessionId = u64;

/// How often config files are checked for changes, language servers are
/// synced and read, and terminals' screens are read.
pub const RELOAD_POLL: Duration = Duration::from_millis(400);
pub const LSP_POLL: Duration = Duration::from_millis(100);
pub const TERMINAL_POLL: Duration = Duration::from_millis(30);

/// What a window draws from: the sessions in this process (`Server`), or
/// in a server it's attached to (`daemon::Client`).
pub trait Host {
    fn handle(&mut self, input: Input);
    fn frame(&mut self, viewport: Viewport) -> Frame;
    /// The window should close.
    fn quit_requested(&self) -> bool;
    /// Work done on timers; each says whether the screen changed.
    fn reload_if_changed(&mut self) -> bool {
        false
    }
    fn pump_terminals(&mut self) -> bool {
        false
    }
    fn pump_servers(&mut self) -> bool {
        false
    }
    /// The server is on another machine, so the window's clipboard is
    /// relayed: what it copied comes back with `take_copied`, and the
    /// window tells it what's on this machine's clipboard with `clipboard`.
    fn wants_clipboard(&self) -> bool {
        false
    }
    fn take_copied(&mut self) -> Option<String> {
        None
    }
    fn clipboard(&mut self, _text: String) {}
    /// Sessions on the window's other machines, named as this one should
    /// list them ("devbox:notes", "local:main"), and what its sessions asked
    /// of the window (to switch to one of those, to connect).
    fn set_foreign(&mut self, _sessions: Vec<SessionInfo>) {}
    fn take_window_requests(&mut self) -> Vec<WindowRequest> {
        Vec::new()
    }
    /// This server's sessions, and makes `name` the active one (made if
    /// there's none such, or a project's when it's a folder).
    fn sessions(&self) -> Vec<SessionInfo>;
    fn switch_to(&mut self, name: &str);
    /// Kills this server's session `name` (asked from another machine's
    /// switcher); what came of it shows as a notice.
    fn kill_session(&mut self, _name: &str, _force: bool) {}
    fn take_notices(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// The machines the window knows, and how each is (SPC S m lists them).
    fn set_machines(&mut self, _machines: Vec<crate::machines::MachineInfo>) {}
}

/// What a session asks of the window it's shown in.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum WindowRequest {
    /// Show this session of another of the window's machines.
    Switch(String),
    /// Connect to another machine (`HOST [TARGET]`).
    Connect { host: String, target: String },
    /// Back to the session shown before, whichever machine it's on.
    Last,
    /// Kill another machine's session.
    Kill { name: String, force: bool },
    /// Check how the remembered machines are, or forget one.
    CheckMachines,
    ForgetMachine(String),
    /// This server is going away to restart on a newer vmac (it saved its
    /// sessions to open again): start the window again on it.
    Restart,
}

impl Host for Server {
    fn handle(&mut self, input: Input) {
        Server::handle(self, input)
    }
    fn frame(&mut self, viewport: Viewport) -> Frame {
        Server::frame(self, viewport)
    }
    fn quit_requested(&self) -> bool {
        Server::quit_requested(self)
    }
    fn reload_if_changed(&mut self) -> bool {
        Server::reload_if_changed(self) | self.tend_update()
    }
    fn pump_terminals(&mut self) -> bool {
        Server::pump_terminals(self)
    }
    fn pump_servers(&mut self) -> bool {
        Server::pump_servers(self)
    }
    fn set_foreign(&mut self, sessions: Vec<SessionInfo>) {
        Server::set_foreign(self, sessions)
    }
    fn take_window_requests(&mut self) -> Vec<WindowRequest> {
        std::mem::take(&mut self.window_requests)
    }
    fn sessions(&self) -> Vec<SessionInfo> {
        self.infos()
    }
    fn kill_session(&mut self, name: &str, force: bool) {
        let said = self.kill(name, force);
        self.notices.push(said);
        self.sync();
    }
    fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }
    fn set_machines(&mut self, machines: Vec<crate::machines::MachineInfo>) {
        Server::set_machines(self, machines)
    }
    fn switch_to(&mut self, name: &str) {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        self.attach(Some(name), &home);
    }
}

/// Makes a session: a fresh editor with the config loaded.
pub type Factory = Box<dyn FnMut() -> Session>;

struct Entry {
    id: SessionId,
    name: String,
    session: Session,
    /// A blank session: it ends when the last window detaches.
    blank: bool,
}

pub struct Server {
    /// In switcher order. Names are unique: checked when a session is made
    /// or renamed. Clients will attach by id, so a rename breaks nothing.
    list: Vec<Entry>,
    active: SessionId,
    /// The session switched from last (SPC S l goes back to it).
    last: Option<SessionId>,
    next_id: SessionId,
    make: Factory,
    /// What the window said about its machine (fonts, themes, appearance,
    /// terminal colors), told again to each new session.
    client: Vec<Input>,
    /// Keep the process's working folder at the active session's, for
    /// plugins that run programs without saying where (the app; not tests).
    follow_cwd: bool,
    /// Sessions on the window's other machines (listed after these), and
    /// what sessions asked of the window.
    foreign: Vec<SessionInfo>,
    window_requests: Vec<WindowRequest>,
    /// What came of what another machine asked (a kill).
    notices: Vec<String>,
    /// Releases checked for and installed, and the restart onto a newer
    /// vmac: what's holding it back (said when it changes), and the version
    /// offered (once).
    update: crate::update::Tracker,
    update_waiting: Vec<String>,
    update_offered: Option<String>,
    /// The session the server started with, while no window has used it:
    /// the first window starting fresh takes it rather than making another.
    unused_first: Option<SessionId>,
}

impl Server {
    pub fn new(first: Session, make: Factory) -> Self {
        let mut server = Self {
            list: Vec::new(),
            active: 0,
            last: None,
            next_id: 0,
            make,
            client: Vec::new(),
            follow_cwd: false,
            foreign: Vec::new(),
            window_requests: Vec::new(),
            notices: Vec::new(),
            update: crate::update::Tracker::new(),
            update_waiting: Vec::new(),
            update_offered: None,
            unused_first: None,
        };
        let name = {
            let st = first.editor().lock();
            st.project.as_ref().and_then(|p| p.file_name()).map_or("main".into(), |n| n.to_string_lossy().into_owned())
        };
        server.active = server.add(name, first);
        server.unused_first = Some(server.active);
        server.sync();
        server
    }

    /// Keeps the process's working folder at the active session's.
    pub fn follow_cwd(mut self) -> Self {
        self.follow_cwd = true;
        self.sync();
        self
    }

    fn add(&mut self, name: String, session: Session) -> SessionId {
        self.next_id += 1;
        let id = self.next_id;
        let name = self.unique(&name, None);
        {
            let mut st = session.editor().lock();
            st.in_server = true;
            st.session_name = name.clone();
            // `vmac` run in its terminals opens here (daemon.rs's Open).
            st.terminal_env = vec![("VMAC_SESSION".into(), id.to_string())];
        }
        self.list.push(Entry { id, name, session, blank: false });
        id
    }

    /// `name`, or `name-2`, `name-3`... when another session has it.
    fn unique(&self, name: &str, except: Option<SessionId>) -> String {
        let name = if name.trim().is_empty() { "main" } else { name.trim() };
        let taken = |n: &str| self.list.iter().any(|e| e.name == n && Some(e.id) != except);
        let mut candidate = name.to_string();
        let mut n = 2;
        while taken(&candidate) {
            candidate = format!("{name}-{n}");
            n += 1;
        }
        candidate
    }

    fn entry(&mut self, id: SessionId) -> &mut Entry {
        self.list.iter_mut().find(|e| e.id == id).expect("the session exists")
    }

    fn by_name(&self, name: &str) -> Option<SessionId> {
        self.list.iter().find(|e| e.name == name).map(|e| e.id)
    }

    fn active_session(&mut self) -> &mut Session {
        let id = self.active;
        &mut self.entry(id).session
    }

    /// The active session's name, and all of them in order.
    pub fn names(&self) -> (String, Vec<String>) {
        let active = self.list.iter().find(|e| e.id == self.active).map(|e| e.name.clone()).unwrap_or_default();
        (active, self.list.iter().map(|e| e.name.clone()).collect())
    }

    /// Tells every session the sessions' names, and moves the process's
    /// working folder to the active one's.
    fn sync(&mut self) {
        let details: Vec<SessionInfo> = self.infos().into_iter().chain(self.foreign.iter().cloned()).collect();
        let names: Vec<String> = details.iter().map(|d| d.name.clone()).collect();
        for e in &self.list {
            let mut st = e.session.editor().lock();
            st.session_names = names.clone();
            st.session_details = details.clone();
            st.session_name = e.name.clone();
        }
        if self.follow_cwd {
            let cwd = self.active_session().editor().lock().cwd.clone();
            if std::env::current_dir().ok().as_ref() != Some(&cwd) {
                let _ = std::env::set_current_dir(&cwd);
            }
        }
    }

    /// Sessions on the window's other machines.
    pub fn set_foreign(&mut self, sessions: Vec<SessionInfo>) {
        if sessions != self.foreign {
            self.foreign = sessions;
            self.sync();
        }
    }

    /// The machines the window knows: each session gets them, and hears
    /// that they changed (an open machines picker shows them anew).
    pub fn set_machines(&mut self, machines: Vec<crate::machines::MachineInfo>) {
        for e in &self.list {
            let mut st = e.session.editor().lock();
            if st.machines != machines {
                st.machines = machines.clone();
                st.push_event("machines-changed", Vec::new());
            }
        }
    }

    /// Each session: its project, buffers and unsaved files.
    pub fn infos(&self) -> Vec<SessionInfo> {
        self.list
            .iter()
            .map(|e| {
                let st = e.session.editor().lock();
                let folder = st.project.clone().unwrap_or_else(|| st.cwd.clone());
                let files: Vec<_> = st.buffer_ids().into_iter().filter_map(|b| st.buffer_by_id(b)).filter(|b| b.file().is_some()).collect();
                SessionInfo {
                    name: e.name.clone(),
                    project: crate::projects::abbreviate(&folder),
                    buffers: files.len(),
                    unsaved: files.iter().filter(|b| b.is_modified()).count(),
                }
            })
            .collect()
    }

    /// What sessions asked of the window.
    pub fn take_window_requests(&mut self) -> Vec<WindowRequest> {
        std::mem::take(&mut self.window_requests)
    }

    fn switch(&mut self, id: SessionId) {
        if id != self.active {
            self.last = Some(self.active);
            self.active = id;
        }
    }

    fn message(&mut self, text: String) {
        self.active_session().editor().lock().message = text;
    }

    /// A new session, told about the window, named and switched to.
    fn create(&mut self, name: &str) -> SessionId {
        let mut session = (self.make)();
        for input in self.client.clone() {
            session.handle(input);
        }
        let id = self.add(name.to_string(), session);
        self.switch(id);
        id
    }

    pub fn handle(&mut self, input: Input) {
        if matches!(input, Input::Client { .. } | Input::Appearance { .. } | Input::TerminalColors { .. }) {
            let same = std::mem::discriminant(&input);
            self.client.retain(|i| std::mem::discriminant(i) != same);
            self.client.push(input.clone());
        }
        self.active_session().handle(input);
        self.serve_requests();
    }

    /// Does what sessions asked (each session's requests, in order).
    fn serve_requests(&mut self) {
        loop {
            let pending: Vec<(SessionId, SessionRequest)> = self
                .list
                .iter()
                .flat_map(|e| {
                    let id = e.id;
                    std::mem::take(&mut e.session.editor().lock().session_requests).into_iter().map(move |r| (id, r))
                })
                .collect();
            if pending.is_empty() {
                break;
            }
            for (from, request) in pending {
                self.serve(from, request);
            }
        }
        self.sync();
    }

    fn serve(&mut self, from: SessionId, request: SessionRequest) {
        match request {
            SessionRequest::Switch(name) => match self.by_name(&name) {
                Some(id) => self.switch(id),
                None if self.foreign.iter().any(|f| f.name == name) => self.window_requests.push(WindowRequest::Switch(name)),
                None => self.message(format!("No session {name}")),
            },
            SessionRequest::Connect { host, target } => self.window_requests.push(WindowRequest::Connect { host, target }),
            SessionRequest::CheckMachines => self.window_requests.push(WindowRequest::CheckMachines),
            SessionRequest::ForgetMachine(host) => self.window_requests.push(WindowRequest::ForgetMachine(host)),
            // In a window with other machines' sessions, it knows which was last.
            SessionRequest::Last if !self.foreign.is_empty() => self.window_requests.push(WindowRequest::Last),
            SessionRequest::Last => match self.last.filter(|id| self.list.iter().any(|e| e.id == *id)) {
                Some(id) => self.switch(id),
                None => self.message("No other session".into()),
            },
            SessionRequest::Rename(name) => {
                let name = self.unique(&name, Some(from));
                self.entry(from).name = name;
            }
            SessionRequest::New { name, root } => {
                self.create(&name);
                self.open_in_new(root);
            }
            SessionRequest::Project(root) => {
                let existing = self
                    .list
                    .iter()
                    .find(|e| e.session.editor().lock().project.as_ref() == Some(&root))
                    .map(|e| e.id);
                match existing {
                    Some(id) => self.switch(id),
                    None => {
                        let name = root.file_name().map_or("main".into(), |n| n.to_string_lossy().into_owned());
                        self.create(&name);
                        self.open_in_new(Some(root));
                    }
                }
            }
            // Another machine's: the window asks that machine.
            SessionRequest::Kill { name, force } if self.foreign.iter().any(|f| f.name == name) => {
                self.window_requests.push(WindowRequest::Kill { name, force })
            }
            SessionRequest::Kill { name, force } => {
                let said = self.kill(&name, force);
                self.message(said);
            }
            SessionRequest::UpdateCheck => {
                self.update.check(true);
                self.message("Checking for a new vmac...".into());
            }
            SessionRequest::Update => {
                let msg = if self.update.install() { "Installing the latest vmac..." } else { "Already installing vmac" };
                self.message(msg.into());
            }
            SessionRequest::Persist(name) => {
                let name = self.unique(&name, Some(from));
                let entry = self.entry(from);
                entry.name = name.clone();
                entry.blank = false;
                let mut st = entry.session.editor().lock();
                st.session_blank = false;
                st.message = format!("Session {name}: it keeps running when the window closes");
            }
            SessionRequest::Adopt(name) => match self.by_name(&name) {
                Some(id) if id != from => {
                    self.switch(id);
                    if self.list.iter().any(|e| e.id == from && e.blank) {
                        self.list.retain(|e| e.id != from);
                        if self.last == Some(from) {
                            self.last = None;
                        }
                    }
                }
                Some(_) => {}
                None => self.message(format!("No session {name}")),
            },
            SessionRequest::RestartServer { force } => {
                let busy = self.busy();
                if !force && !busy.is_empty() {
                    self.message(format!("Not restarting: {} (:restart-server! restarts anyway)", busy.join(", ")));
                } else {
                    self.restart();
                }
            }
        }
    }

    // ---- Updates (update.rs)

    /// A window of a newer vmac attached: offer to restart onto it.
    pub fn newer_window(&mut self, version: &str) {
        if let Some(event) = self.update.newer_window(version) {
            self.update_event(event);
        }
    }

    /// Checks and installs under way, and the restart once a newer vmac is
    /// installed: when nothing would be lost, the active session asks
    /// whether to restart. True when the screen changed.
    pub fn tend_update(&mut self) -> bool {
        let events = self.update.tend();
        let mut changed = !events.is_empty();
        for event in events {
            self.update_event(event);
        }
        let Some(version) = self.update.installed.clone() else { return changed };
        if self.update_offered.as_ref() == Some(&version) {
            return changed;
        }
        let busy = self.busy();
        if busy != self.update_waiting {
            if !busy.is_empty() {
                self.event("update-waiting", vec![version.clone(), busy.join(", ")]);
                changed = true;
            }
            self.update_waiting = busy.clone();
        }
        if busy.is_empty() {
            self.update_offered = Some(version.clone());
            self.event("update-ready", vec![version]);
            changed = true;
        }
        changed
    }

    fn update_event(&mut self, event: crate::update::Event) {
        use crate::update::Event;
        match event {
            Event::Out(v) => self.event("update-out", vec![v]),
            Event::UpToDate => self.event("update-latest", vec![crate::update::current().into()]),
            Event::Installed(v) => {
                // Asked again (a newer one still): offered afresh.
                self.update_waiting.clear();
                self.event("update-installed", vec![v]);
            }
            Event::Run(command) => self.event("update-run", vec![command]),
            Event::Failed(why) => self.event("update-failed", vec![why]),
        }
    }

    /// Raises a Lua event (core.events[name]) in the active session.
    fn event(&mut self, name: &str, args: Vec<String>) {
        self.active_session().editor().lock().push_event(name, args);
    }

    /// What a restart would lose: unsaved files, an agent at work, commands
    /// running in terminals (each said with its session's name).
    pub fn busy(&self) -> Vec<String> {
        let several = self.list.len() > 1;
        let mut reasons = Vec::new();
        for e in &self.list {
            let mut st = e.session.editor().lock();
            let within = if several { format!(" in {}", e.name) } else { String::new() };
            let unsaved = st
                .buffer_ids()
                .into_iter()
                .filter(|b| st.buffer_by_id(*b).is_some_and(|b| b.is_modified() && b.file().is_some()))
                .count();
            if unsaved > 0 {
                let s = if unsaved == 1 { "" } else { "s" };
                reasons.push(format!("{unsaved} unsaved file{s}{within}"));
            }
            if st.agent.as_ref().is_some_and(|a| a.state == crate::agent::State::Running) {
                reasons.push(format!("the agent working{within}"));
            }
            let running = st.running_terminals();
            if !running.is_empty() {
                reasons.push(format!("{} running{within}", running.join(", ")));
            }
        }
        reasons
    }

    /// Saves the sessions to open again, and asks the window to start
    /// again on the newer vmac (which stops this server).
    fn restart(&mut self) {
        let restore = self.restore_state();
        if let Err(e) = crate::update::save_restore(restore) {
            self.message(format!("Couldn't save the sessions to reopen: {e}"));
            return;
        }
        self.window_requests.push(WindowRequest::Restart);
    }

    /// The sessions as they are: names, projects, files open.
    pub fn restore_state(&self) -> crate::update::Restore {
        let sessions = self
            .list
            .iter()
            .map(|e| {
                let st = e.session.editor().lock();
                let files = st.buffer_ids().into_iter().filter_map(|b| st.buffer_by_id(b)?.file().cloned()).collect();
                crate::update::RestoreSession { name: e.name.clone(), project: st.project.clone(), files }
            })
            .collect();
        let active = self.list.iter().find(|e| e.id == self.active).map(|e| e.name.clone()).unwrap_or_default();
        crate::update::Restore { active, sessions, written: 0 }
    }

    /// Opens the sessions a restart saved: each with its project and files.
    pub fn restore(&mut self, restore: crate::update::Restore) {
        self.unused_first = None;
        for (i, saved) in restore.sessions.iter().enumerate() {
            // The first goes in the session there is.
            if i > 0 {
                self.create(&saved.name);
            } else {
                let id = self.active;
                let name = self.unique(&saved.name, Some(id));
                self.entry(id).name = name;
            }
            let session = self.active_session();
            let mut st = session.editor().lock();
            if let Some(root) = &saved.project {
                let _ = st.project_open(root);
                st.session_requests.retain(|r| !matches!(r, SessionRequest::Rename(_)));
            }
            for file in &saved.files {
                if file.is_file() {
                    let _ = st.open_file(file);
                }
            }
        }
        if let Some(id) = self.by_name(&restore.active) {
            self.switch(id);
        }
        self.message(format!("Restarted on vmac {}", crate::update::current()));
        self.serve_requests();
    }

    /// In a session just made (the active one): opens the project and the
    /// file picker in it, or the dashboard without one.
    fn open_in_new(&mut self, root: Option<PathBuf>) {
        let session = self.active_session();
        match root {
            Some(root) => {
                let opened = {
                    let mut st = session.editor().lock();
                    let opened = st.project_open(&root);
                    // Its name is already chosen: not renamed after the project.
                    st.session_requests.retain(|r| !matches!(r, SessionRequest::Rename(_)));
                    opened
                };
                match opened {
                    Ok(_) => session.run_command("find-file-command"),
                    Err(e) => session.editor().lock().message = e,
                }
            }
            None => session.run_command("dashboard"),
        }
    }

    /// Kills a session (refused with unsaved files unless `force`); says
    /// what came of it.
    fn kill(&mut self, name: &str, force: bool) -> String {
        let Some(id) = self.by_name(name) else { return format!("No session {name}") };
        if self.list.len() == 1 {
            return format!("{name} is the only session there is");
        }
        let unsaved = {
            let st = self.entry(id).session.editor().lock();
            st.buffer_ids().into_iter().filter(|b| st.buffer_by_id(*b).is_some_and(|b| b.is_modified() && b.file().is_some())).count()
        };
        if unsaved > 0 && !force {
            let s = if unsaved == 1 { "" } else { "s" };
            return format!("{name} has {unsaved} unsaved file{s} (kill it anyway with SPC S K)");
        }
        if id == self.active {
            let next = self.last.filter(|l| *l != id).unwrap_or_else(|| self.list.iter().find(|e| e.id != id).expect("another").id);
            self.active = next;
        }
        if self.last == Some(id) {
            self.last = None;
        }
        // Dropping the session stops its terminals, language servers and agent.
        self.list.retain(|e| e.id != id);
        format!("Killed session {name}")
    }

    /// The active session's frame; with several sessions, its name leads
    /// the selected window's modeline.
    pub fn frame(&mut self, viewport: Viewport) -> Frame {
        let several = self.list.len() + self.foreign.len() > 1;
        let (name, _) = self.names();
        let infos = self.infos();
        let mut frame = self.active_session().frame(viewport);
        frame.sessions = infos;
        frame.session = name.clone();
        if several && let Some(w) = frame.windows.iter_mut().find(|w| w.selected) {
            w.modeline.insert(0, Segment { text: format!("[{name}]"), face: "accent".into() });
        }
        frame
    }

    pub fn quit_requested(&self) -> bool {
        self.list.iter().find(|e| e.id == self.active).is_some_and(|e| e.session.quit_requested())
    }

    /// Whether the active session asked to quit, forgetting that it did (a
    /// daemon detaches its windows instead).
    pub fn take_quit(&mut self) -> bool {
        std::mem::take(&mut self.active_session().editor().lock().quit_requested)
    }

    /// A window attaching: to the session named `target`, or the session
    /// of the project in that folder (relative to `cwd`), made when there's
    /// none such; None stays on the active one.
    /// A window starting fresh (`vmac`, `vmac PATH`): a blank session, in
    /// `cwd`, that starts as a terminal filling the window. With no `path`
    /// it offers the start picker (a new persistent session, an existing
    /// one, or staying blank); with one, it opens it as `vmac PATH` run in
    /// that terminal does. A blank session ends when the window detaches.
    pub fn start(&mut self, cwd: &Path, path: Option<&Path>) {
        let id = match self.unused_first.take().filter(|id| self.list.iter().any(|e| e.id == *id)) {
            Some(id) => {
                let name = self.unique("blank", Some(id));
                self.entry(id).name = name;
                self.switch(id);
                id
            }
            None => self.create("blank"),
        };
        let entry = self.entry(id);
        entry.blank = true;
        {
            let mut st = entry.session.editor().lock();
            st.session_blank = true;
            st.terminal_dir = Some(cwd.to_path_buf());
            st.cwd = cwd.to_path_buf();
            // Not renamed after a project it opens: Persist names it.
            st.session_requests.retain(|r| !matches!(r, SessionRequest::Rename(_)));
        }
        let call = match path {
            Some(p) => format!("core.start_in_terminal({}, {})", lua_string(&p.display().to_string()), p.is_dir()),
            None => "core.start_in_terminal()".to_string(),
        };
        let _ = entry.session.run_lua(&call);
        self.serve_requests();
    }

    /// `vmac PATH` run in a terminal of session `id`: the session is shown,
    /// the terminal docks below and PATH opens above it (a folder: its
    /// files in the fuzzy finder). False when there's no such session.
    pub fn open_from_terminal(&mut self, id: SessionId, path: &Path) -> bool {
        if !self.list.iter().any(|e| e.id == id) {
            return false;
        }
        self.unused_first = None;
        self.switch(id);
        let call = format!("core.open_from_terminal({}, {})", lua_string(&path.display().to_string()), path.is_dir());
        let _ = self.entry(id).session.run_lua(&call);
        self.serve_requests();
        true
    }

    /// The last window detached: blank sessions end. True when that leaves
    /// none (the server has nothing left to keep).
    pub fn detached(&mut self) -> bool {
        if !self.list.iter().any(|e| e.blank) {
            return false;
        }
        self.list.retain(|e| !e.blank);
        let Some(first) = self.list.first().map(|e| e.id) else { return true };
        if !self.list.iter().any(|e| e.id == self.active) {
            self.active = self.last.filter(|l| self.list.iter().any(|e| e.id == *l)).unwrap_or(first);
        }
        if self.last.is_some_and(|l| !self.list.iter().any(|e| e.id == l)) || self.last == Some(self.active) {
            self.last = None;
        }
        self.sync();
        false
    }

    pub fn attach(&mut self, target: Option<&str>, cwd: &Path) {
        self.unused_first = None;
        let Some(target) = target.filter(|t| !t.is_empty()) else { return };
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let path = match target.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if target == "~" => home,
            None => cwd.join(target),
        };
        if path.is_dir() {
            let root = std::path::absolute(&path).unwrap_or(path);
            let fresh = self.active_session().editor().lock().project.is_none()
                && !self.list.iter().any(|e| e.session.editor().lock().project.as_ref() == Some(&root));
            if fresh {
                // The session it's on has no project yet: this one is its.
                let session = self.active_session();
                let opened = session.editor().lock().project_open(&root);
                match opened {
                    Ok(_) => session.run_command("find-file-command"),
                    Err(e) => session.editor().lock().message = e,
                }
            } else {
                let from = self.active;
                self.serve(from, SessionRequest::Project(root));
            }
        } else {
            let request = match self.by_name(target) {
                Some(_) => SessionRequest::Switch(target.to_string()),
                None => SessionRequest::New { name: target.to_string(), root: None },
            };
            let from = self.active;
            self.serve(from, request);
        }
        self.serve_requests();
    }

    // ---- Work on timers: every session's, so background ones keep up
    // (their terminals and language servers); only the active one's
    // changes need drawing.

    fn each(&mut self, work: fn(&mut Session) -> bool) -> bool {
        let active = self.active;
        let mut changed = false;
        for e in &mut self.list {
            changed |= work(&mut e.session) && e.id == active;
        }
        self.serve_requests();
        changed
    }

    pub fn reload_if_changed(&mut self) -> bool {
        self.each(Session::reload_if_changed)
    }

    pub fn pump_terminals(&mut self) -> bool {
        self.each(Session::pump_terminals)
    }

    pub fn pump_servers(&mut self) -> bool {
        self.each(Session::pump_servers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use crate::editor::Editor;
    use crate::frame::Content;
    use crate::lisp::Lisp;

    fn fresh() -> Session {
        let editor = Editor::new(Buffer::new("*scratch*"));
        Session::new(editor.clone(), Lisp::new(editor))
    }

    fn server() -> Server {
        Server::new(fresh(), Box::new(fresh))
    }

    /// Runs Lua in the active session, after a harmless key (as a command
    /// would run it), then what it asked of the server.
    fn eval_lua(server: &mut Server, code: &str) {
        server.handle(Input::Key { key: "ESC".into(), text: String::new() });
        server.active_session().eval(code).unwrap();
        server.serve_requests();
    }

    fn project(dir: &std::path::Path, name: &str) -> PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("main.rs"), format!("// {name}\n")).unwrap();
        root
    }

    const VIEW: Viewport = Viewport { width: 800., height: 600. };

    fn shown(server: &mut Server) -> String {
        match &server.frame(VIEW).windows[0].content {
            Content::Text(lines) => lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        }
    }

    #[test]
    fn projects_open_in_their_own_sessions_with_their_own_buffers() {
        let dir = std::env::temp_dir().join(format!("vmac-server-{}", std::process::id()));
        let (alpha, beta) = (project(&dir, "alpha"), project(&dir, "beta"));
        let mut s = server();
        assert_eq!(s.names(), ("main".into(), vec!["main".into()]));

        // The first project names the session it opens in.
        eval_lua(&mut s, &format!("vmac.project_open({:?})", alpha.display().to_string()));
        assert_eq!(s.names(), ("alpha".into(), vec!["alpha".into()]));
        eval_lua(&mut s, &format!("vmac.find_file({:?})", alpha.join("main.rs").display().to_string()));
        assert!(shown(&mut s).contains("// alpha"));

        // Another project: a new session, its own buffers, alpha's untouched.
        eval_lua(&mut s, &format!("vmac.project_open({:?})", beta.display().to_string()));
        assert_eq!(s.names(), ("beta".into(), vec!["alpha".into(), "beta".into()]));
        assert!(s.frame(VIEW).picker.is_some(), "the file picker opens in the new project");
        s.handle(Input::Key { key: "ESC".into(), text: String::new() });
        s.handle(Input::Key { key: "ESC".into(), text: String::new() });
        let beta_buffers = s.active_session().editor().lock().buffer_names();
        assert!(!beta_buffers.iter().any(|b| b == "main.rs"), "alpha's file isn't in beta: {beta_buffers:?}");
        assert_eq!(s.active_session().editor().lock().cwd, beta);

        // Back to alpha, by name and then by "last".
        eval_lua(&mut s, "vmac.session_switch('alpha')");
        assert_eq!(s.names().0, "alpha");
        assert!(shown(&mut s).contains("// alpha"));
        eval_lua(&mut s, "vmac.session_last()");
        assert_eq!(s.names().0, "beta");
        // Opening alpha again returns to its session rather than making one.
        eval_lua(&mut s, &format!("vmac.project_open({:?})", alpha.display().to_string()));
        assert_eq!(s.names(), ("alpha".into(), vec!["alpha".into(), "beta".into()]));

        // The selected window's modeline names the session.
        let frame = s.frame(VIEW);
        assert_eq!(frame.windows[0].modeline[0].text, "[alpha]");

        // Renaming keeps names unique; killing goes back to the other one.
        eval_lua(&mut s, "vmac.session_rename('beta')");
        assert_eq!(s.names().0, "beta-2");
        eval_lua(&mut s, "vmac.session_kill('beta-2', false)");
        assert_eq!(s.names(), ("beta".into(), vec!["beta".into()]));
        eval_lua(&mut s, "vmac.session_kill('beta', false)");
        assert_eq!(s.names().1.len(), 1, "the last session stays");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn with_sh() -> Session {
        let mut session = fresh();
        session.eval("vmac.set_terminal_shell('/bin/sh')").unwrap();
        session
    }

    fn key(server: &mut Server, key: &str, text: &str) {
        server.handle(Input::Key { key: key.into(), text: text.into() });
    }

    fn typed(server: &mut Server, text: &str) {
        for c in text.chars() {
            match c {
                '\n' => key(server, "RET", ""),
                ' ' => key(server, "SPC", " "),
                c => key(server, &c.to_string(), &c.to_string()),
            }
        }
    }

    fn picker_title(server: &mut Server) -> Option<String> {
        server.frame(VIEW).picker.map(|p| p.title)
    }

    #[test]
    fn windows_start_as_a_terminal_in_a_blank_session() {
        let dir = std::env::temp_dir().join(format!("vmac-start-{}", std::process::id()));
        let alpha = project(&dir, "alpha");
        let mut s = Server::new(with_sh(), Box::new(with_sh));
        // The first window takes the session the server started with.
        s.start(&alpha, None);
        assert_eq!(s.names(), ("blank".into(), vec!["blank".into()]));
        assert!(s.active_session().editor().lock().terminal_here().is_some(), "a terminal fills the window");
        assert_eq!(picker_title(&mut s).as_deref(), Some("Start"));
        // New persistent session: named (after the folder unless typed).
        typed(&mut s, "New\n");
        let echo = format!("{:?}", s.frame(VIEW).echo);
        assert!(echo.contains("Session name"), "{echo}");
        for _ in 0..10 {
            key(&mut s, "DEL", "");
        }
        typed(&mut s, "work\n");
        assert_eq!(s.names(), ("work".into(), vec!["work".into()]));
        assert!(!s.detached(), "a persistent session stays when the window goes");
        assert_eq!(s.names().1, vec!["work".to_string()]);

        // Another window: its own blank session; detaching ends it.
        s.start(&alpha, None);
        assert_eq!(s.names(), ("blank".into(), vec!["work".into(), "blank".into()]));
        key(&mut s, "ESC", "");
        assert!(!s.detached());
        assert_eq!(s.names(), ("work".into(), vec!["work".into()]));

        // Find a session: go there, and the blank one ends at once.
        s.start(&alpha, None);
        typed(&mut s, "Find\n");
        assert_eq!(picker_title(&mut s).as_deref(), Some("Find a session"));
        typed(&mut s, "work\n");
        assert_eq!(s.names(), ("work".into(), vec!["work".into()]));

        // Only blank sessions left: the server has nothing to keep.
        let mut lone = Server::new(with_sh(), Box::new(with_sh));
        lone.start(&alpha, None);
        key(&mut lone, "ESC", "");
        assert!(lone.detached());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn vmac_in_a_terminal_opens_the_editor_with_the_terminal_below() {
        let dir = std::env::temp_dir().join(format!("vmac-open-{}", std::process::id()));
        let alpha = project(&dir, "alpha");
        let mut s = Server::new(with_sh(), Box::new(with_sh));
        s.start(&alpha, None);
        // ESC twice closes the start picker (the first is its normal state), typing still.
        key(&mut s, "ESC", "");
        key(&mut s, "ESC", "");
        let id = s.active;
        // Its terminals know which session they're in, and start in the folder.
        typed(&mut s, "echo in-$VMAC_SESSION-$(basename $PWD)\n");
        let want = format!("in-{id}-alpha");
        let mut seen = false;
        for _ in 0..200 {
            s.pump_terminals();
            if s.active_session().editor().lock().buffer().text().to_string().lines().any(|l| l.trim().ends_with(&want) && !l.contains("echo")) {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(seen, "{}", s.active_session().editor().lock().buffer().text());
        key(&mut s, "ESC", "");
        // `vmac .` in it: the file finder over the folder, the terminal below.
        assert!(s.open_from_terminal(id, &alpha));
        assert_eq!(picker_title(&mut s).as_deref(), Some("Find file"));
        typed(&mut s, "main\n");
        {
            let session = s.active_session();
            let mut st = session.editor().lock();
            assert_eq!(st.window_count(), 2);
            assert_eq!(st.buffer().name(), "main.rs", "the file opens above");
            assert!(st.terminal_here().is_none());
            assert_eq!(st.project.as_deref(), Some(alpha.as_path()));
            let rects = st.window_rects();
            let (top, bottom) = (rects[0].1, rects[1].1);
            assert!(bottom.y > top.y && bottom.h < top.h, "the terminal is docked below, smaller: {top:?} {bottom:?}");
        }
        assert_eq!(s.names().0, "alpha", "the blank session is named after its project");
        // An unknown session says so.
        assert!(!s.open_from_terminal(999, &alpha));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsaved_files_keep_a_session_alive_unless_forced() {
        let dir = std::env::temp_dir().join(format!("vmac-server-kill-{}", std::process::id()));
        let alpha = project(&dir, "alpha");
        let mut s = server();
        eval_lua(&mut s, "vmac.session_new('scratch', '')");
        assert_eq!(s.names().1, ["main", "scratch"]);
        eval_lua(&mut s, &format!("vmac.find_file({:?})", alpha.join("main.rs").display().to_string()));
        s.handle(Input::Key { key: "x".into(), text: "x".into() });
        eval_lua(&mut s, "vmac.session_kill('scratch', false)");
        assert_eq!(s.names().1, ["main", "scratch"], "refused: an unsaved file");
        eval_lua(&mut s, "vmac.session_kill('scratch', true)");
        assert_eq!(s.names(), ("main".into(), vec!["main".into()]));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn update_events(server: &mut Server) -> Vec<(String, Vec<String>)> {
        server.active_session().editor().lock().take_events().into_iter().filter(|(n, _)| n.starts_with("update-")).collect()
    }

    #[test]
    fn a_newer_vmac_is_offered_once_nothing_would_be_lost() {
        let dir = std::env::temp_dir().join(format!("vmac-server-update-{}", std::process::id()));
        let alpha = project(&dir, "alpha");
        let mut s = server();
        eval_lua(&mut s, &format!("vmac.find_file({:?})", alpha.join("main.rs").display().to_string()));
        s.handle(Input::Key { key: "x".into(), text: "x".into() });
        assert_eq!(s.busy(), ["1 unsaved file"]);

        // A newer window attached: waiting on the unsaved file, said once.
        s.newer_window("99.0.0");
        s.tend_update();
        let events = update_events(&mut s);
        assert!(events.contains(&("update-installed".into(), vec!["99.0.0".into()])), "{events:?}");
        assert!(events.contains(&("update-waiting".into(), vec!["99.0.0".into(), "1 unsaved file".into()])), "{events:?}");
        s.tend_update();
        assert!(update_events(&mut s).is_empty());

        // Restarting by hand is refused while something would be lost.
        eval_lua(&mut s, "vmac.restart_server(false)");
        assert!(s.active_session().editor().lock().message.starts_with("Not restarting: 1 unsaved file"));
        assert!(s.take_window_requests().is_empty());

        // Saved: asked, once.
        eval_lua(&mut s, "vmac.save_buffer()");
        assert!(s.busy().is_empty());
        s.tend_update();
        assert_eq!(update_events(&mut s), [("update-ready".to_string(), vec!["99.0.0".to_string()])]);
        s.tend_update();
        assert!(update_events(&mut s).is_empty());
        // An older or the same window changes nothing.
        s.newer_window("0.0.1");
        s.newer_window("99.0.0");
        s.tend_update();
        assert!(update_events(&mut s).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_restart_reopens_the_sessions_projects_and_files() {
        let dir = std::env::temp_dir().join(format!("vmac-server-restore-{}", std::process::id()));
        let (alpha, beta) = (project(&dir, "alpha"), project(&dir, "beta"));
        let mut s = server();
        eval_lua(&mut s, &format!("vmac.project_open({:?})", alpha.display().to_string()));
        eval_lua(&mut s, &format!("vmac.find_file({:?})", alpha.join("main.rs").display().to_string()));
        eval_lua(&mut s, &format!("vmac.project_open({:?})", beta.display().to_string()));
        s.handle(Input::Key { key: "ESC".into(), text: String::new() });
        eval_lua(&mut s, "vmac.session_switch('alpha')");
        let saved = s.restore_state();
        assert_eq!(saved.active, "alpha");
        assert_eq!(saved.sessions.len(), 2);

        let mut fresh = server();
        fresh.restore(saved);
        assert_eq!(fresh.names(), ("alpha".into(), vec!["alpha".into(), "beta".into()]));
        assert!(shown(&mut fresh).contains("// alpha"));
        assert_eq!(fresh.active_session().editor().lock().project.as_ref(), Some(&alpha));
        eval_lua(&mut fresh, "vmac.session_switch('beta')");
        assert_eq!(fresh.active_session().editor().lock().project.as_ref(), Some(&beta));
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// `text` as a Lua string literal.
fn lua_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
