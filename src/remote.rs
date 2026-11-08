//! vmac on another machine: `vmac connect HOST [TARGET]` installs the server
//! there over ssh (once per build), then opens a window on it, the
//! connection carried by `ssh HOST .vmac/server/<build>/vmac proxy`. The
//! sessions run there, near the files; closing the window detaches, as
//! `vmac attach` does here.
//!
//! The server for the remote's system: this program when it's the same
//! system; else one built for it: from the checkout this program came from
//! (with cargo-zigbuild), else ~/.local/share/vmac/servers/ (put one there
//! yourself), else a release's download.

use crate::daemon::Client;
use std::io::{Error, ErrorKind, Read, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The Rust target to build a server for, from the remote's `uname -sm`.
pub fn target_for(uname: &str) -> Result<&'static str> {
    let mut words = uname.split_whitespace();
    let (os, arch) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    Ok(match (os, arch) {
        ("Linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("Linux", "aarch64" | "arm64") => "aarch64-unknown-linux-musl",
        ("Darwin", "arm64" | "aarch64") => "aarch64-apple-darwin",
        ("Darwin", "x86_64") => "x86_64-apple-darwin",
        _ => return Err(Error::new(ErrorKind::Unsupported, format!("vmac has no server for {os} {arch}"))),
    })
}

/// This program's target.
fn own_target() -> Option<&'static str> {
    target_for(&format!(
        "{} {}",
        match std::env::consts::OS {
            "macos" => "Darwin",
            "linux" => "Linux",
            other => other,
        },
        std::env::consts::ARCH
    ))
    .ok()
}

/// Runs `ssh host command`, returning what it printed; errs with what ssh
/// said when it fails.
fn ssh(host: &str, command: &str) -> Result<String> {
    let out = Command::new("ssh").args(["-o", "BatchMode=yes", host, command]).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(Error::other(format!("ssh {host}: {why}")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn say(text: &str) {
    eprintln!("vmac: {text}");
}

/// Where servers for other systems are kept: ~/.local/share/vmac/servers/<version>/<target>/vmac.
fn cached(target: &str) -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    Some(base.join("vmac/servers").join(env!("CARGO_PKG_VERSION")).join(target).join("vmac"))
}

/// A server binary for `target`, found, downloaded or built.
fn server_binary(target: &str) -> Result<PathBuf> {
    if Some(target) == own_target() {
        return std::env::current_exe();
    }
    let cache = cached(target).ok_or_else(|| Error::other("no home folder"))?;
    std::fs::create_dir_all(cache.parent().expect("a folder"))?;
    // Run from a checkout (with cargo-zigbuild): built from it, so the
    // server matches the code (cargo only rebuilds what changed).
    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let is_checkout = checkout.join(".git").exists() && !checkout.components().any(|c| c.as_os_str() == ".cargo");
    let zigbuild = || Command::new("cargo").args(["zigbuild", "--help"]).output().is_ok_and(|o| o.status.success());
    if is_checkout && zigbuild() {
        say(&format!("building the server for {target} (a few minutes the first time)"));
        let _ = Command::new("rustup").args(["target", "add", target]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let built = Command::new("cargo")
            .args(["zigbuild", "--release", "--quiet", "--no-default-features", "--target", target, "--manifest-path"])
            .arg(checkout.join("Cargo.toml"))
            .status()?;
        let out = checkout.join("target").join(target).join("release/vmac");
        if built.success() && out.is_file() {
            std::fs::copy(&out, &cache)?;
            return Ok(cache);
        }
    }
    if cache.is_file() {
        return Ok(cache);
    }
    // A release's server.
    let url = format!(
        "https://github.com/Super-Yojan/vmac/releases/download/v{}/vmac-server-{target}",
        env!("CARGO_PKG_VERSION")
    );
    say(&format!("fetching the server for {target}"));
    let fetched = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&cache)
        .arg(&url)
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if fetched {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755))?;
        return Ok(cache);
    }
    let _ = std::fs::remove_file(&cache);
    Err(Error::new(
        ErrorKind::NotFound,
        format!(
            "no server for {target}: build one (cargo zigbuild --release --no-default-features --target {target}) and put it at {}",
            cache.display()
        ),
    ))
}

/// The build's name: its version and a hash of the binary, so a changed
/// build is installed afresh beside the old.
fn build_id(binary: &Path) -> Result<String> {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut file = std::fs::File::open(binary)?;
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.write(&buf[..n]);
    }
    Ok(format!("{}-{:08x}", env!("CARGO_PKG_VERSION"), hasher.finish() as u32))
}

/// Installs the server on `host` (unless this build is there already) and
/// returns its path there, relative to the home folder, and whether it
/// was just installed.
pub fn install(host: &str) -> Result<(String, bool)> {
    let uname = ssh(host, "uname -sm")?;
    let target = target_for(&uname)?;
    let binary = server_binary(target)?;
    let path = format!(".vmac/server/{}/vmac", build_id(&binary)?);
    if ssh(host, &format!("test -x {path} && echo yes || echo no"))?.trim() == "yes" {
        return Ok((path, false));
    }
    let size = binary.metadata()?.len();
    say(&format!("installing the server on {host} ({} MB)", size / 1_000_000));
    let dir = path.trim_end_matches("/vmac");
    let status = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-C", host])
        // Then builds no server runs from go.
        .arg(format!(
            "mkdir -p {dir} && cat > {path}.part && chmod +x {path}.part && mv {path}.part {path} && \
             for old in .vmac/server/*; do [ \"$old\" = {dir} ] || pgrep -f \"$old/vmac\" >/dev/null || rm -rf \"$old\"; done"
        ))
        .stdin(std::fs::File::open(&binary)?)
        .status()?;
    if !status.success() {
        return Err(Error::other(format!("couldn't copy the server to {host}")));
    }
    Ok((path, true))
}

/// Installs the server on `host` if needed, and attaches to `target` there
/// (a session's name or a folder, from the remote home folder). `restart`
/// stops the server running there first (it may be an older build), and
/// everything in it.
pub fn connect(host: &str, target: Option<String>, restart: bool) -> Result<Client> {
    let (path, fresh) = install(host)?;
    if restart {
        let _ = ssh(host, &format!("{path} kill-server; sleep 0.3"));
    } else if fresh && ssh(host, &format!("{path} ls >/dev/null 2>&1 && echo running || true"))?.trim() == "running" {
        say(&format!(
            "the server running on {host} is an older build (it keeps its sessions); \
             `vmac connect --restart {host}` replaces it, closing them"
        ));
    }
    let (reader, writer, ssh) = dial(host, &path)?;
    let mut client = Client::over(reader, writer, target, PathBuf::new(), ssh)?;
    // A dropped connection (sleep, a network change) is made again.
    let (host, path) = (host.to_string(), path.clone());
    client.set_redial(std::sync::Arc::new(move || dial(&host, &path)));
    Ok(client)
}

/// A connection to the server on `host`: `ssh HOST PATH proxy`. ssh gives
/// up on a connection that stops answering for 45 seconds.
fn dial(host: &str, path: &str) -> Result<crate::daemon::Transport> {
    let mut ssh = Command::new("ssh")
        .args(["-C", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3", host, path, "proxy"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let (reader, writer) = (ssh.stdout.take().expect("piped"), ssh.stdin.take().expect("piped"));
    Ok((Box::new(reader), Box::new(writer), Some(ssh)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code (claude-code-acp, logged in here) on a project on
    /// another machine: VMAC_TEST_HOST=user@host, the project
    /// /tmp/vmac-agent-test there. Its command must run there.
    #[test]
    #[ignore]
    fn a_local_agent_works_on_a_remote_project() {
        use crate::frame::{Content, Input, Viewport};
        use crate::server::Host;
        let host = std::env::var("VMAC_TEST_HOST").expect("VMAC_TEST_HOST=user@host");
        let remote_name = ssh(&host, "hostname").unwrap().trim().to_string();
        let mut client = connect(&host, Some("/tmp/vmac-agent-test".into()), true).unwrap();
        let view = Viewport { width: 1400., height: 900. };
        let ex = |client: &mut Client, text: &str| {
            for k in ["ESC", "ESC", ":"] {
                client.handle(Input::Key { key: k.into(), text: if k == ":" { ":".into() } else { String::new() } });
            }
            for c in text.chars() {
                let k = if c == ' ' { "SPC".to_string() } else { c.to_string() };
                client.handle(Input::Key { key: k, text: c.to_string() });
            }
            client.handle(Input::Key { key: "RET".into(), text: String::new() });
        };
        ex(&mut client, "lua vmac.hooks.agent_permission = function() return 'allow' end");
        ex(&mut client, "agent-use claude");
        ex(&mut client, "agent Run the shell command hostname, then use a command to find which files here contain the word println. Reply with one line: HOST=<hostname> FILES=<files>");
        let mut shown = String::new();
        for _ in 0..900 {
            client.pump_terminals();
            let frame = client.frame(view);
            shown = frame
                .windows
                .iter()
                .filter_map(|w| match &w.content {
                    Content::Text(lines) => Some(lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>().join("\n")),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n---\n");
            if shown.contains(&format!("HOST={remote_name}")) && shown.contains("main.rs") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        eprintln!("{shown}");
        assert!(shown.contains(&format!("HOST={remote_name}")), "the command ran on {remote_name}");
        assert!(shown.contains("main.rs"));
    }

    /// The ssh connection to VMAC_TEST_HOST dies (its proxy killed there):
    /// the window reconnects, on the same session.
    #[test]
    #[ignore]
    fn a_dropped_connection_comes_back() {
        use crate::frame::{Echo, Viewport};
        use crate::server::Host;
        let host = std::env::var("VMAC_TEST_HOST").expect("VMAC_TEST_HOST=user@host");
        let mut client = connect(&host, Some("drop-test".into()), true).unwrap();
        let view = Viewport { width: 1000., height: 700. };
        assert_eq!(client.frame(view).session, "drop-test");
        ssh(&host, "pkill -f '[v]mac proxy'; true").unwrap();
        let (mut lost, mut back) = (false, false);
        for _ in 0..300 {
            client.pump_terminals();
            let frame = client.frame(view);
            match &frame.echo {
                Echo::Message(m) if m.contains("reconnecting") => lost = true,
                _ if lost && frame.session == "drop-test" && client.gone().is_none() => {
                    back = true;
                }
                _ => {}
            }
            if back {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(lost, "it noticed");
        assert!(back, "and came back on drop-test");
    }

    #[test]
    fn targets_from_uname() {
        assert_eq!(target_for("Linux x86_64\n").unwrap(), "x86_64-unknown-linux-musl");
        assert_eq!(target_for("Linux aarch64").unwrap(), "aarch64-unknown-linux-musl");
        assert_eq!(target_for("Darwin arm64").unwrap(), "aarch64-apple-darwin");
        assert!(target_for("FreeBSD amd64").is_err());
        assert!(own_target().is_some());
    }
}
