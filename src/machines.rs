//! The machines you connect to, remembered (~/.local/state/vmac/machines,
//! the latest first), and how each is: offline, online without a vmac
//! server, or online with its sessions. `vmac ls --all` and the window's
//! machines picker (SPC S m) show them; a window checks them in the
//! background, so the editor never waits on ssh.

use crate::frame::SessionInfo;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A machine and how it was found.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MachineInfo {
    pub host: String,
    pub state: State,
    /// The window it's shown in has it connected.
    #[serde(default)]
    pub connected: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum State {
    Checking,
    /// ssh couldn't reach it (or log in without asking).
    Offline,
    /// Reachable, but no vmac server runs there.
    NoServer,
    Online(Vec<SessionInfo>),
}

impl State {
    /// "offline", "no server", "2 sessions"...
    pub fn describe(&self) -> String {
        match self {
            State::Checking => "checking...".into(),
            State::Offline => "offline".into(),
            State::NoServer => "online, no vmac server".into(),
            State::Online(sessions) => {
                let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
                let s = if sessions.len() == 1 { "" } else { "s" };
                format!("{} session{s}: {}", sessions.len(), names.join(", "))
            }
        }
    }
}

fn file() -> Option<PathBuf> {
    crate::projects::History::default_dir().map(|d| d.join("machines"))
}

/// The machines remembered, the latest first.
pub fn remembered() -> Vec<String> {
    file().map(|f| remembered_in(&f)).unwrap_or_default()
}

fn remembered_in(file: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(file)
        .map(|text| text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

fn save(file: &std::path::Path, hosts: &[String]) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, hosts.iter().map(|h| format!("{h}\n")).collect::<String>());
}

/// Remembers `host` (connected to just now): first in the list.
pub fn remember(host: &str) {
    if let Some(f) = file() {
        remember_in(&f, host);
    }
}

fn remember_in(file: &std::path::Path, host: &str) {
    let mut hosts = remembered_in(file);
    hosts.retain(|h| h != host);
    hosts.insert(0, host.to_string());
    save(file, &hosts);
}

/// Forgets `host`; false when it wasn't remembered.
pub fn forget(host: &str) -> bool {
    file().is_some_and(|f| forget_in(&f, host))
}

fn forget_in(file: &std::path::Path, host: &str) -> bool {
    let mut hosts = remembered_in(file);
    let before = hosts.len();
    hosts.retain(|h| h != host);
    save(file, &hosts);
    hosts.len() != before
}

/// How `host` is, asked over ssh (giving up after a few seconds): the
/// server there (whichever installed build it runs) lists its sessions.
pub fn check(host: &str) -> State {
    let out = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=4", host])
        .arg("for vmac in .vmac/server/*/vmac; do \"$vmac\" ls --json 2>/dev/null && exit 0; done; exit 3")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => match serde_json::from_slice::<Vec<SessionInfo>>(&o.stdout) {
            Ok(sessions) => State::Online(sessions),
            Err(_) => State::NoServer,
        },
        Ok(o) if o.status.code() == Some(3) => State::NoServer,
        _ => State::Offline,
    }
}

/// Checks each host at once (in threads), in the order given.
pub fn check_all(hosts: &[String]) -> Vec<(String, State)> {
    let handles: Vec<_> = hosts
        .iter()
        .cloned()
        .map(|host| std::thread::spawn(move || {
            let state = check(&host);
            (host, state)
        }))
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machines_are_remembered_latest_first_and_forgotten() {
        let dir = std::env::temp_dir().join(format!("vmac-machines-{}", std::process::id()));
        let file = dir.join("machines");
        remember_in(&file, "a@one");
        remember_in(&file, "two");
        remember_in(&file, "a@one");
        assert_eq!(remembered_in(&file), ["a@one", "two"]);
        assert!(forget_in(&file, "two"));
        assert!(!forget_in(&file, "two"));
        assert_eq!(remembered_in(&file), ["a@one"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreachable_machine_is_offline() {
        assert_eq!(check("vmac-no-such-host.invalid"), State::Offline);
        assert_eq!(State::Online(vec![SessionInfo { name: "main".into(), ..Default::default() }]).describe(), "1 session: main");
    }
}
