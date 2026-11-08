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
/// returns its path there, relative to the home folder.
pub fn install(host: &str) -> Result<String> {
    let uname = ssh(host, "uname -sm")?;
    let target = target_for(&uname)?;
    let binary = server_binary(target)?;
    let path = format!(".vmac/server/{}/vmac", build_id(&binary)?);
    if ssh(host, &format!("test -x {path} && echo yes || echo no"))?.trim() == "yes" {
        return Ok(path);
    }
    let size = binary.metadata()?.len();
    say(&format!("installing the server on {host} ({} MB)", size / 1_000_000));
    let dir = path.trim_end_matches("/vmac");
    let status = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-C", host])
        .arg(format!("mkdir -p {dir} && cat > {path}.part && chmod +x {path}.part && mv {path}.part {path}"))
        .stdin(std::fs::File::open(&binary)?)
        .status()?;
    if !status.success() {
        return Err(Error::other(format!("couldn't copy the server to {host}")));
    }
    Ok(path)
}

/// Installs the server on `host` if needed, and attaches to `target` there
/// (a session's name or a folder, from the remote home folder).
pub fn connect(host: &str, target: Option<String>) -> Result<Client> {
    let path = install(host)?;
    let mut ssh = Command::new("ssh")
        .args(["-C", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=15", host, &path, "proxy"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let (reader, writer) = (ssh.stdout.take().expect("piped"), ssh.stdin.take().expect("piped"));
    Client::over(Box::new(reader), Box::new(writer), target, PathBuf::new(), Some(ssh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_from_uname() {
        assert_eq!(target_for("Linux x86_64\n").unwrap(), "x86_64-unknown-linux-musl");
        assert_eq!(target_for("Linux aarch64").unwrap(), "aarch64-unknown-linux-musl");
        assert_eq!(target_for("Darwin arm64").unwrap(), "aarch64-apple-darwin");
        assert!(target_for("FreeBSD amd64").is_err());
        assert!(own_target().is_some());
    }
}
