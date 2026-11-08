//! The sessions a window can show, like tmux's: projects open side by side,
//! each with its own buffers, windows, terminals, language servers and agent.
//! The window talks to the server as it would to one session (`Input` in,
//! `Frame` out) and sees the active one; a session's commands (SPC S ...,
//! opening another project) ask the server to switch, make, rename or kill
//! sessions.

use crate::editor::SessionRequest;
use crate::frame::{Frame, Input, Segment, Viewport};
use crate::session::Session;
use std::path::PathBuf;

pub type SessionId = u64;

/// Makes a session: a fresh editor with the config loaded.
pub type Factory = Box<dyn FnMut() -> Session>;

struct Entry {
    id: SessionId,
    name: String,
    session: Session,
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
        };
        let name = {
            let st = first.editor().lock();
            st.project.as_ref().and_then(|p| p.file_name()).map_or("main".into(), |n| n.to_string_lossy().into_owned())
        };
        server.active = server.add(name, first);
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
        }
        self.list.push(Entry { id, name, session });
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
        let names: Vec<String> = self.list.iter().map(|e| e.name.clone()).collect();
        for e in &self.list {
            let mut st = e.session.editor().lock();
            st.session_names = names.clone();
            st.session_name = e.name.clone();
        }
        if self.follow_cwd {
            let cwd = self.active_session().editor().lock().cwd.clone();
            if std::env::current_dir().ok().as_ref() != Some(&cwd) {
                let _ = std::env::set_current_dir(&cwd);
            }
        }
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
                None => self.message(format!("No session {name}")),
            },
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
            SessionRequest::Kill { name, force } => self.kill(&name, force),
        }
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

    fn kill(&mut self, name: &str, force: bool) {
        let Some(id) = self.by_name(name) else { return self.message(format!("No session {name}")) };
        if self.list.len() == 1 {
            return self.message("That's the only session".into());
        }
        let unsaved = {
            let st = self.entry(id).session.editor().lock();
            st.buffer_ids().into_iter().filter(|b| st.buffer_by_id(*b).is_some_and(|b| b.is_modified() && b.file().is_some())).count()
        };
        if unsaved > 0 && !force {
            let s = if unsaved == 1 { "" } else { "s" };
            return self.message(format!("{name} has {unsaved} unsaved file{s} (kill it anyway with SPC S K)"));
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
        self.message(format!("Killed session {name}"));
    }

    /// The active session's frame; with several sessions, its name leads
    /// the selected window's modeline.
    pub fn frame(&mut self, viewport: Viewport) -> Frame {
        let several = self.list.len() > 1;
        let (name, _) = self.names();
        let mut frame = self.active_session().frame(viewport);
        if several && let Some(w) = frame.windows.iter_mut().find(|w| w.selected) {
            w.modeline.insert(0, Segment { text: format!("[{name}]"), face: "accent".into() });
        }
        frame
    }

    pub fn quit_requested(&self) -> bool {
        self.list.iter().find(|e| e.id == self.active).is_some_and(|e| e.session.quit_requested())
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
    use crate::lisp::{Lisp, Source};

    fn fresh() -> Session {
        let editor = Editor::new(Buffer::new("*scratch*"));
        Session::new(editor.clone(), Lisp::new(editor, vec![Source { label: "core".into(), path: None }]))
    }

    fn server() -> Server {
        Server::new(fresh(), Box::new(fresh))
    }

    fn eval_lua(server: &mut Server, code: &str) {
        let editor = server.active_session().editor().clone();
        editor.lock().pending_scheme.push(format!("(lua-eval {code:?})"));
        // Queued Scheme runs after the next command; a harmless key runs it.
        server.handle(Input::Key { key: "ESC".into(), text: String::new() });
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
}
