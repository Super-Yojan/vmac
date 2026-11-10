//! One window, several machines: the sessions here (or on the server this
//! window is attached to) and those of each machine connected to from the
//! window (`:connect`, `SPC S c`). The switcher lists them all: a machine's
//! own sessions by name, the others' as `host:name`, and this machine's, from
//! another, as `local:name`. Each machine's server is told the others'
//! sessions; picking one of them asks the window, which shows that machine.
//! `:q` in another machine's session detaches from that machine only.

use crate::daemon::Client;
use crate::frame::{Echo, Frame, Input, SessionInfo, Viewport};
use crate::server::{Host, WindowRequest};
use std::sync::mpsc::{Receiver, Sender, channel};

struct Machine {
    /// "" for the window's own; else the host connected to, without user@.
    label: String,
    /// As connected to ("me@devbox"); "" for the window's own.
    address: String,
    host: Box<dyn Host>,
}

pub struct Hosts {
    machines: Vec<Machine>,
    active: usize,
    /// Connections being made (in the background: installing can take a while).
    connected: Receiver<(String, Result<Client, String>)>,
    connect: Sender<(String, Result<Client, String>)>,
    /// What the window says until the next key ("connecting to ...").
    notice: Option<String>,
    /// The session shown, and the one before (SPC S l goes back to it), as
    /// (machine's label, session).
    shown: Option<(String, String)>,
    last: Option<(String, String)>,
    /// The machines connected to before (machines.rs), how each was found,
    /// and checks of them under way in the background.
    known: Vec<crate::machines::MachineInfo>,
    checked: Receiver<Vec<(String, crate::machines::State)>>,
    check: Sender<Vec<(String, crate::machines::State)>>,
    checking: bool,
    /// Where the remembered machines come from (machines.rs; tests' own).
    remembered: fn() -> Vec<String>,
}

const LOCAL: &str = "local";

impl Hosts {
    pub fn new(home: Box<dyn Host>) -> Self {
        Self::remembering(home, crate::machines::remembered)
    }

    fn remembering(home: Box<dyn Host>, remembered: fn() -> Vec<String>) -> Self {
        let (connect, connected) = channel();
        let (check, checked) = channel();
        let mut hosts = Self {
            machines: vec![Machine { label: String::new(), address: String::new(), host: home }],
            active: 0,
            connected,
            connect,
            notice: None,
            shown: None,
            last: None,
            known: Vec::new(),
            checked,
            check,
            checking: false,
            remembered,
        };
        hosts.check_machines();
        hosts
    }

    /// Checks the machines connected to before, in the background.
    fn check_machines(&mut self) {
        use crate::machines::{MachineInfo, State};
        let remembered = (self.remembered)();
        for host in &remembered {
            if !self.known.iter().any(|m| &m.host == host) {
                self.known.push(MachineInfo { host: host.clone(), state: State::Checking, connected: false });
            }
        }
        // In the order remembered (the latest first).
        self.known.sort_by_key(|m| remembered.iter().position(|h| *h == m.host).unwrap_or(usize::MAX));
        if self.checking || remembered.is_empty() {
            return;
        }
        self.checking = true;
        let done = self.check.clone();
        std::thread::spawn(move || {
            let _ = done.send(crate::machines::check_all(&remembered));
        });
    }

    /// Takes finished checks, marks the machines this window has connected
    /// (with their sessions as they are), and tells every machine's server.
    fn tend_machines(&mut self) -> bool {
        use crate::machines::{MachineInfo, State};
        let before = self.known.clone();
        while let Ok(results) = self.checked.try_recv() {
            self.checking = false;
            for (host, state) in results {
                if let Some(m) = self.known.iter_mut().find(|m| m.host == host) {
                    m.state = state;
                }
            }
        }
        for m in &mut self.known {
            m.connected = false;
        }
        for machine in self.machines.iter().skip(1) {
            let state = State::Online(machine.host.sessions());
            match self.known.iter_mut().find(|m| m.host == machine.address) {
                Some(m) => {
                    m.state = state;
                    m.connected = true;
                }
                None => self.known.insert(0, MachineInfo { host: machine.address.clone(), state, connected: true }),
            }
        }
        for machine in &mut self.machines {
            machine.host.set_machines(self.known.clone());
        }
        self.known != before
    }

    /// `name` (a session of machine `of`) as machine `seen_from` lists it.
    fn name_as_seen(&self, of: usize, name: &str, seen_from: usize) -> String {
        if of == seen_from {
            name.to_string()
        } else if of == 0 {
            format!("{LOCAL}:{name}")
        } else {
            format!("{}:{name}", self.machines[of].label)
        }
    }

    /// Which machine and session `name`, listed by machine `seen_from`, is.
    fn find(&self, name: &str, seen_from: usize) -> Option<(usize, String)> {
        if let Some(rest) = name.strip_prefix(&format!("{LOCAL}:")) {
            return Some((0, rest.to_string()));
        }
        (1..self.machines.len())
            .filter(|i| *i != seen_from)
            .find_map(|i| name.strip_prefix(&format!("{}:", self.machines[i].label)).map(|rest| (i, rest.to_string())))
    }

    /// Tells each machine the others' sessions.
    fn tell_foreign(&mut self) {
        let sessions: Vec<Vec<SessionInfo>> = self.machines.iter().map(|m| m.host.sessions()).collect();
        for i in 0..self.machines.len() {
            let mut foreign = Vec::new();
            for (j, list) in sessions.iter().enumerate().filter(|(j, _)| *j != i) {
                foreign.extend(list.iter().map(|s| SessionInfo { name: self.name_as_seen(j, &s.name, i), ..s.clone() }));
            }
            self.machines[i].host.set_foreign(foreign);
        }
    }

    /// A unique label for a host: "devbox", "devbox-2"...
    fn label_for(&self, host: &str) -> String {
        let base = host.rsplit('@').next().unwrap_or(host).to_string();
        let mut label = base.clone();
        let mut n = 2;
        while label == LOCAL || self.machines.iter().any(|m| m.label == label) {
            label = format!("{base}-{n}");
            n += 1;
        }
        label
    }

    /// Does what sessions asked, takes finished connections, and lets go of
    /// machines whose server hung up. True when the screen changed.
    fn tend(&mut self) -> bool {
        let mut changed = false;
        while let Ok((host, result)) = self.connected.try_recv() {
            changed = true;
            match result {
                Ok(client) => {
                    let label = self.label_for(&host);
                    self.notice = Some(format!("Connected to {host}"));
                    self.machines.push(Machine { label, address: host.clone(), host: Box::new(client) });
                    self.active = self.machines.len() - 1;
                }
                Err(e) => self.notice = Some(format!("{host}: {e}")),
            }
        }
        let mut i = 0;
        while i < self.machines.len() {
            let mut removed = false;
            for request in self.machines[i].host.take_window_requests() {
                changed = true;
                match request {
                    WindowRequest::Switch(name) => match self.find(&name, i) {
                        Some((machine, session)) => {
                            self.active = machine;
                            self.machines[machine].host.switch_to(&session);
                        }
                        None => self.notice = Some(format!("No session {name}")),
                    },
                    WindowRequest::Connect { host, target } => self.start_connect(host, target),
                    WindowRequest::Last => self.go_back(),
                    WindowRequest::CheckMachines => self.check_machines(),
                    WindowRequest::ForgetMachine(host) => {
                        self.known.retain(|m| m.host != host);
                        self.notice = Some(if crate::machines::forget(&host) {
                            format!("Forgot {host}")
                        } else {
                            format!("{host} wasn't remembered")
                        });
                    }
                    // This window's own server: the window starts again on
                    // the new vmac. Another machine's: connected to again,
                    // its server restarted on the build this window installs.
                    WindowRequest::Restart if i == 0 => crate::update::relaunch(),
                    WindowRequest::Restart => {
                        let address = self.machines[i].address.clone();
                        self.machines.remove(i);
                        if self.active >= i {
                            self.active = self.active.saturating_sub(1).min(self.machines.len() - 1);
                        }
                        self.notice = Some(format!("Restarting vmac on {address}..."));
                        let done = self.connect.clone();
                        std::thread::spawn(move || {
                            let result = crate::remote::connect(&address, None, true).map_err(|e| e.to_string());
                            let _ = done.send((address, result));
                        });
                        // The rest of its requests went with it.
                        removed = true;
                        break;
                    }
                    WindowRequest::Kill { name, force } => match self.find(&name, i) {
                        Some((machine, session)) => self.machines[machine].host.kill_session(&session, force),
                        None => self.notice = Some(format!("No session {name}")),
                    },
                }
            }
            if !removed {
                i += 1;
            }
        }
        // What came of kills asked of other machines.
        for i in 0..self.machines.len() {
            if let Some(said) = self.machines[i].host.take_notices().pop() {
                self.notice = Some(said);
                changed = true;
            }
        }
        // Other machines that hung up (:q there, or the connection broke).
        let mut i = 1;
        while i < self.machines.len() {
            if self.machines[i].host.quit_requested() {
                let gone = self.machines.remove(i);
                self.notice = Some(format!("Detached from {}", gone.label));
                if self.active == i {
                    self.active = 0;
                } else if self.active > i {
                    self.active -= 1;
                }
                changed = true;
            } else {
                i += 1;
            }
        }
        self.tell_foreign();
        changed |= self.tend_machines();
        changed
    }

    /// Notes the session shown: when it changed, the one before is last.
    fn note_shown(&mut self, session: &str) {
        let now = (self.machines[self.active].label.clone(), session.to_string());
        if session.is_empty() || self.shown.as_ref() == Some(&now) {
            return;
        }
        if let Some(before) = self.shown.replace(now) {
            self.last = Some(before);
        }
    }

    /// SPC S l: the session shown before, on whichever machine (while it
    /// and its machine are still there).
    fn go_back(&mut self) {
        let target = self.last.clone().and_then(|(label, session)| {
            let machine = self.machines.iter().position(|m| m.label == label)?;
            self.machines[machine].host.sessions().iter().any(|s| s.name == session).then_some((machine, session))
        });
        match target {
            Some((machine, session)) => {
                self.active = machine;
                self.machines[machine].host.switch_to(&session);
            }
            None => self.notice = Some("No other session".into()),
        }
    }

    fn start_connect(&mut self, host: String, target: String) {
        if let Some(i) = self.machines.iter().position(|m| m.label == host.rsplit('@').next().unwrap_or(&host)) {
            // Already connected: show it (on that session, if one is named).
            self.active = i;
            if !target.is_empty() {
                self.machines[i].host.switch_to(&target);
            }
            return;
        }
        self.notice = Some(format!("Connecting to {host}..."));
        let done = self.connect.clone();
        std::thread::spawn(move || {
            let target = Some(target).filter(|t| !t.is_empty());
            let result = crate::remote::connect(&host, target, false).map_err(|e| e.to_string());
            let _ = done.send((host, result));
        });
    }

    fn each(&mut self, work: impl Fn(&mut dyn Host) -> bool) -> bool {
        let active = self.active;
        let mut changed = false;
        for (i, m) in self.machines.iter_mut().enumerate() {
            changed |= work(m.host.as_mut()) && i == active;
        }
        changed | self.tend()
    }
}

impl Host for Hosts {
    fn handle(&mut self, input: Input) {
        if matches!(input, Input::Key { .. }) {
            self.notice = None;
        }
        // What the window says about its machine, every machine needs.
        if matches!(input, Input::Client { .. } | Input::Appearance { .. } | Input::TerminalColors { .. }) {
            for m in &mut self.machines {
                m.host.handle(input.clone());
            }
        } else {
            self.machines[self.active].host.handle(input);
        }
        self.tend();
    }

    fn frame(&mut self, viewport: Viewport) -> Frame {
        let active = self.active;
        let mut frame = self.machines[active].host.frame(viewport);
        self.note_shown(&frame.session);
        // Another machine's session: its name says which machine.
        if active > 0 {
            let label = &self.machines[active].label;
            let name = format!("[{}]", frame.session);
            if let Some(segment) = frame.windows.iter_mut().find(|w| w.selected).and_then(|w| w.modeline.iter_mut().find(|s| s.text == name)) {
                segment.text = format!("[{label}:{}]", frame.session);
            }
        }
        if let Some(notice) = &self.notice
            && matches!(frame.echo, Echo::Empty | Echo::Message(_))
        {
            frame.echo = Echo::Message(notice.clone());
        }
        frame
    }

    /// The window closes when its own machine's session quits.
    fn quit_requested(&self) -> bool {
        self.active == 0 && self.machines[0].host.quit_requested()
    }

    fn reload_if_changed(&mut self) -> bool {
        self.each(|h| h.reload_if_changed())
    }

    fn pump_terminals(&mut self) -> bool {
        self.each(|h| h.pump_terminals())
    }

    fn pump_servers(&mut self) -> bool {
        self.each(|h| h.pump_servers())
    }

    fn wants_clipboard(&self) -> bool {
        self.machines.iter().any(|m| m.host.wants_clipboard())
    }

    fn take_copied(&mut self) -> Option<String> {
        self.machines.iter_mut().find_map(|m| m.host.take_copied())
    }

    fn clipboard(&mut self, text: String) {
        for m in self.machines.iter_mut().filter(|m| m.host.wants_clipboard()) {
            m.host.clipboard(text.clone());
        }
    }

    fn sessions(&self) -> Vec<SessionInfo> {
        self.machines[0].host.sessions()
    }

    fn switch_to(&mut self, name: &str) {
        self.active = 0;
        self.machines[0].host.switch_to(name);
    }
}

#[cfg(test)]
impl Hosts {
    /// Another machine, as :connect adds one.
    fn add(&mut self, label: &str, host: Box<dyn Host>) {
        self.machines.push(Machine { label: label.into(), address: label.into(), host });
        self.tend();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use crate::editor::Editor;
    use crate::lisp::Lisp;
    use crate::server::Server;
    use crate::session::Session;
    use crate::sys::UnixStream;
    use std::time::Duration;

    fn fresh() -> Session {
        let editor = Editor::new(Buffer::new("*scratch*"));
        Session::new(editor.clone(), Lisp::new(editor))
    }

    fn ex(hosts: &mut Hosts, line: &str) {
        for _ in 0..2 {
            hosts.handle(Input::Key { key: "ESC".into(), text: String::new() });
        }
        hosts.handle(Input::Key { key: ":".into(), text: ":".into() });
        for c in line.chars() {
            let key = if c == ' ' { "SPC".to_string() } else { c.to_string() };
            hosts.handle(Input::Key { key, text: c.to_string() });
        }
        hosts.handle(Input::Key { key: "RET".into(), text: String::new() });
        // Let the far machine answer, and the window do what was asked.
        for _ in 0..20 {
            hosts.pump_terminals();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    const VIEW: Viewport = Viewport { width: 800., height: 600. };

    fn said(hosts: &mut Hosts) -> String {
        match hosts.frame(VIEW).echo {
            Echo::Message(m) => m,
            other => format!("{other:?}"),
        }
    }

    /// :connect from a window, to VMAC_TEST_HOST=user@host (its project
    /// /tmp/vmac-agent-test): its sessions join this window's.
    #[test]
    #[ignore]
    fn connect_adds_a_real_machines_sessions() {
        let host = std::env::var("VMAC_TEST_HOST").expect("VMAC_TEST_HOST=user@host");
        let label = host.rsplit('@').next().unwrap().to_string();
        let mut hosts = Hosts::new(Box::new(Server::new(fresh(), Box::new(fresh))));
        ex(&mut hosts, &format!("connect {host} /tmp/vmac-agent-test"));
        for _ in 0..3000 {
            if hosts.machines.len() > 1 {
                break;
            }
            hosts.pump_terminals();
            std::thread::sleep(Duration::from_millis(100));
        }
        let notice = said(&mut hosts);
        assert_eq!(hosts.active, 1, "{notice}");
        let modeline: String = hosts.frame(VIEW).windows.iter().flat_map(|w| w.modeline.iter().map(|s| s.text.clone())).collect();
        assert!(modeline.contains(&format!("[{label}:vmac-agent-test]")), "{modeline}");
        ex(&mut hosts, "lua vmac.session_switch('local:main')");
        assert_eq!(hosts.active, 0);
        ex(&mut hosts, "lua print(table.concat(vmac.session_list(), ','))");
        let list = said(&mut hosts);
        assert!(list.contains(&format!("{label}:vmac-agent-test")), "{list}");
        ex(&mut hosts, &format!("lua vmac.session_switch('{label}:vmac-agent-test')"));
        assert_eq!(hosts.active, 1);
        // SPC S l, from the far machine back to this one's session.
        let _ = hosts.frame(VIEW);
        ex(&mut hosts, "lua vmac.session_last()");
        let notice = said(&mut hosts);
        assert_eq!(hosts.active, 0, "{notice}");
    }

    #[test]
    fn one_switcher_for_every_machines_sessions() {
        let dir = std::env::temp_dir().join(format!("vmac-hosts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("socket");
        let window = {
            let socket = socket.clone();
            std::thread::spawn(move || {
                while UnixStream::connect(&socket).is_err() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let mut hosts =
                    Hosts::remembering(Box::new(Server::new(fresh(), Box::new(fresh))), || vec!["vmac-no-such-host.invalid".into()]);
                let far = Client::attach(&socket, Some("notes".into())).unwrap();
                hosts.add("box", Box::new(far));
                let mut seen = Vec::new();
                ex(&mut hosts, "lua print(table.concat(vmac.session_list(), ','))");
                seen.push(said(&mut hosts));
                // Picking the far machine's session shows it.
                ex(&mut hosts, "lua vmac.session_switch('box:notes')");
                seen.push(hosts.active.to_string());
                ex(&mut hosts, "lua print(vmac.session_name() .. ' / ' .. table.concat(vmac.session_list(), ','))");
                seen.push(said(&mut hosts));
                let frame = hosts.frame(VIEW);
                seen.push(frame.windows.iter().flat_map(|w| w.modeline.iter().map(|s| s.text.clone())).collect::<Vec<_>>().join(" "));
                // And back.
                ex(&mut hosts, "lua vmac.session_switch('local:main')");
                seen.push(hosts.active.to_string());
                // SPC S l: back to the other machine's session, and again.
                let _ = hosts.frame(VIEW);
                ex(&mut hosts, "lua vmac.session_last()");
                let _ = hosts.frame(VIEW);
                let session = hosts.frame(VIEW).session;
                seen.push(format!("{} {session}", hosts.active));
                ex(&mut hosts, "lua vmac.session_last()");
                let _ = hosts.frame(VIEW);
                let session = hosts.frame(VIEW).session;
                seen.push(format!("{} {session}", hosts.active));
                // Every machine's sessions, described.
                ex(&mut hosts, "lua local t = {} for _, d in ipairs(vmac.session_details()) do t[#t+1] = d[1] .. ':' .. d[3] end print(table.concat(t, ' '))");
                seen.push(said(&mut hosts));
                // The machines: the connected one with its sessions, the
                // unreachable one offline (once its check is back).
                for _ in 0..100 {
                    hosts.pump_terminals();
                    if !hosts.checking {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                ex(&mut hosts, "lua local t = {} for _, m in ipairs(vmac.machines()) do t[#t+1] = m[1] .. '=' .. m[2] .. '=' .. m[3] end print(table.concat(t, ' / '))");
                seen.push(said(&mut hosts));
                // Killing the other machine's session from this one's switcher.
                ex(&mut hosts, "lua vmac.session_kill('box:notes', false)");
                seen.push(said(&mut hosts));
                let names: Vec<String> = hosts.machines[1].host.sessions().into_iter().map(|s| s.name).collect();
                seen.push(names.join(","));
                drop(hosts);
                crate::daemon::kill(&socket).unwrap();
                seen
            })
        };
        crate::daemon::serve(|| Server::new(fresh(), Box::new(fresh)), &socket).unwrap();
        let seen = window.join().unwrap();
        assert_eq!(seen[0], "main,box:main,box:notes", "this machine's, then the other's");
        assert_eq!(seen[1], "1", "the far machine shows");
        assert_eq!(seen[2], "notes / main,notes,local:main", "named as that machine sees them");
        assert!(seen[3].contains("[box:notes]"), "{}", seen[3]);
        assert_eq!(seen[4], "0", "back on this machine");
        assert_eq!(seen[5], "1 notes", "SPC S l goes back to the other machine's session");
        assert_eq!(seen[6], "0 main", "and back again");
        assert_eq!(seen[7], "main:0 box:main:0 box:notes:0", "each with its files");
        assert_eq!(seen[8], "box=2 sessions: main, notes=connected / vmac-no-such-host.invalid=offline=", "{}", seen[8]);
        assert_eq!(seen[9], "Killed session notes", "the other machine says what came of it");
        assert_eq!(seen[10], "main", "gone from there");
        std::fs::remove_dir_all(&dir).ok();
    }
}
