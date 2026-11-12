//! Updating vmac to the latest release.
//!
//! - Which release is newest: the tag `releases/latest` on GitHub redirects
//!   to (no API, so no rate limit).
//! - How this vmac was installed, from where its program is: the Mac app,
//!   a Linux release unpacked into a prefix, a Windows release, `cargo
//!   install`, a checkout, or the server `vmac connect` installed.
//! - Installing: the release's file for this system, checked against the
//!   release's SHA256SUMS, unpacked beside the old one and moved over it (a
//!   running vmac keeps running the old program). `cargo install` builds are
//!   rebuilt by cargo, in a terminal, instead.
//!
//! vmac checks once a day and says when a release is out (vmac never
//! downloads one unasked). Once a newer program is in place, the server
//! running the old one offers to restart onto it when nothing would be lost
//! (`Tracker`, server.rs); the window then starts again (`relaunch`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant, SystemTime};

/// Where releases come from: VMAC_UPDATE_URL (tests, forks), else vmac's.
fn repo() -> String {
    std::env::var("VMAC_UPDATE_URL").unwrap_or_else(|_| "https://github.com/Super-Yojan/vmac".into())
}

pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// "v0.1.2" or "0.1.2" (a "-beta" after it is ignored) as numbers.
pub fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let v = version.trim().trim_start_matches('v');
    let v = v.split(['-', '+']).next()?;
    let mut parts = v.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

/// Whether `version` is newer than `than`.
pub fn newer(version: &str, than: &str) -> bool {
    match (parse(version), parse(than)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

// ---- This program

static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
static ARGS: OnceLock<Vec<String>> = OnceLock::new();

/// Notes where this program is and how it was started, before anything can
/// move it (on Windows an update renames the running program).
pub fn remember_start(args: &[String]) {
    EXE.get_or_init(|| std::env::current_exe().ok().map(|p| p.canonicalize().unwrap_or(p)));
    ARGS.get_or_init(|| args.to_vec());
    #[cfg(windows)]
    if let Some(exe) = exe() {
        // Left by the last update.
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }
}

/// This program's path as it was started.
pub fn exe() -> Option<PathBuf> {
    EXE.get_or_init(|| std::env::current_exe().ok()).clone()
}

/// How this vmac was installed.
#[derive(Debug, Clone, PartialEq)]
pub enum Install {
    /// vmac.app (its bundle).
    MacApp(PathBuf),
    /// A Linux release unpacked into this prefix (bin/vmac, share/...).
    Linux(PathBuf),
    /// A Windows release: vmac.exe in this folder.
    Windows(PathBuf),
    /// `cargo install`, into $CARGO_HOME/bin.
    Cargo,
    /// Built in a checkout (cargo run): updated with git.
    Checkout,
    /// The server `vmac connect` installed: it follows the vmac connecting.
    Server,
    /// Somewhere else (a package manager's, a copy): not vmac's to replace.
    Other(PathBuf),
}

impl Install {
    /// This program's.
    pub fn here() -> Install {
        let Some(exe) = exe() else { return Install::Other(PathBuf::new()) };
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".cargo")));
        detect(&exe, cargo_home.as_deref(), Path::new(env!("CARGO_MANIFEST_DIR")), cfg!(feature = "gui"))
    }

    /// Whether vmac checks for releases on its own (once a day).
    pub fn checks(&self) -> bool {
        matches!(self, Install::MacApp(_) | Install::Linux(_) | Install::Windows(_) | Install::Cargo)
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// How a vmac at `exe` was installed (`manifest`: the checkout it was built
/// from; `gui`: built with the window).
pub fn detect(exe: &Path, cargo_home: Option<&Path>, manifest: &Path, gui: bool) -> Install {
    let under = |dir: &Path| exe.starts_with(dir);
    if !gui || exe.components().any(|c| c.as_os_str() == ".vmac") {
        return Install::Server;
    }
    if manifest.join(".git").exists() && under(&manifest.join("target")) {
        return Install::Checkout;
    }
    if cargo_home.is_some_and(|c| under(&c.join("bin"))) {
        return Install::Cargo;
    }
    // .../vmac.app/Contents/MacOS/vmac-bin
    let macos = exe.parent();
    let contents = macos.and_then(Path::parent);
    let app = contents.and_then(Path::parent);
    if let (Some(macos), Some(contents), Some(app)) = (macos, contents, app)
        && macos.file_name().is_some_and(|n| n == "MacOS")
        && contents.file_name().is_some_and(|n| n == "Contents")
        && app.extension().is_some_and(|e| e == "app")
    {
        return Install::MacApp(app.to_path_buf());
    }
    if exe.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        return match exe.parent() {
            Some(dir) => Install::Windows(dir.to_path_buf()),
            None => Install::Other(exe.to_path_buf()),
        };
    }
    // <prefix>/bin/vmac beside <prefix>/share/applications/vmac.desktop.
    if let Some(bin) = exe.parent().filter(|b| b.file_name().is_some_and(|n| n == "bin"))
        && let Some(prefix) = bin.parent()
        && prefix.join("share/applications/vmac.desktop").is_file()
    {
        return Install::Linux(prefix.to_path_buf());
    }
    Install::Other(exe.to_path_buf())
}

// ---- Releases

/// The newest release's version ("0.0.9"), once it's ready to install: a
/// release appears before its build finishes, and its SHA256SUMS comes
/// last, so until that's up it isn't out yet.
pub fn latest() -> Result<String, String> {
    let version = latest_tag()?;
    if !ready(&version) {
        return Err(format!("vmac {version} is still being built; try again in a few minutes"));
    }
    Ok(version)
}

/// Whether release `version` has its SHA256SUMS (its build is done).
fn ready(version: &str) -> bool {
    let url = format!("{}/releases/download/v{version}/SHA256SUMS", repo());
    if let Some(path) = url.strip_prefix("file://") {
        return Path::new(path).is_file();
    }
    Command::new("curl")
        .args(["-fsSLI", "-o", if cfg!(windows) { "NUL" } else { "/dev/null" }, "--max-time", "20"])
        .arg(&url)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The newest release's tag, as a version, whether or not it's built.
fn latest_tag() -> Result<String, String> {
    let url = format!("{}/releases/latest", repo());
    // file:// (tests): a file holding the version.
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read_to_string(path).map(|v| v.trim().trim_start_matches('v').to_string()).map_err(|e| e.to_string());
    }
    let out = Command::new("curl")
        .args(["-fsSLI", "-o", if cfg!(windows) { "NUL" } else { "/dev/null" }, "-w", "%{url_effective}", "--max-time", "20"])
        .arg(&url)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err("couldn't reach GitHub".into());
    }
    let effective = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let tag = effective.rsplit_once("/tag/").map(|(_, t)| t.to_string()).ok_or("no release found")?;
    parse(&tag).ok_or_else(|| format!("odd release tag {tag}"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// The release file for this system, for an install of this kind.
pub fn asset(install: &Install) -> Option<String> {
    match install {
        Install::MacApp(_) => Some("vmac-macos-universal.zip".into()),
        Install::Linux(_) => match std::env::consts::ARCH {
            arch @ ("x86_64" | "aarch64") => Some(format!("vmac-{arch}-unknown-linux-gnu.tar.gz")),
            _ => None,
        },
        Install::Windows(_) => (std::env::consts::ARCH == "x86_64").then(|| "vmac-x86_64-pc-windows-msvc.zip".into()),
        _ => None,
    }
}

/// What installing did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The release is in place (restart vmac to use it).
    Installed(String),
    /// Run this (in a terminal) to install it: cargo rebuilds it.
    Command(String),
}

/// Installs release `version` over this vmac.
pub fn install(version: &str) -> Result<Outcome, String> {
    let how = Install::here();
    match &how {
        Install::Cargo => Ok(Outcome::Command(format!(
            "cargo install --git {} --tag v{version} --locked --force vmac",
            repo()
        ))),
        Install::Checkout => Err("this vmac was built in a checkout: git pull and build it again".into()),
        Install::Server => Err("this is vmac's server: it's updated by the vmac that connects to it".into()),
        Install::Other(path) => Err(format!("vmac doesn't know how {} was installed; update it the way you installed it", path.display())),
        Install::MacApp(_) | Install::Linux(_) | Install::Windows(_) => {
            let name = asset(&how).ok_or("there's no release built for this system")?;
            install_release(&how, version, &name)?;
            Ok(Outcome::Installed(version.to_string()))
        }
    }
}

/// The folder an install's files are swapped in from: beside them, so the
/// move is a rename on the same disk.
fn parent_of(how: &Install) -> Option<PathBuf> {
    match how {
        Install::MacApp(app) => app.parent().map(Path::to_path_buf),
        Install::Linux(prefix) => Some(prefix.clone()),
        Install::Windows(dir) => Some(dir.clone()),
        _ => None,
    }
}

fn install_release(how: &Install, version: &str, name: &str) -> Result<(), String> {
    let parent = parent_of(how).ok_or("no folder to install into")?;
    let work = parent.join(format!(".vmac-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => format!(
            "vmac can't write to {}; download {name} from {}/releases/tag/v{version} and install it as you did before",
            parent.display(),
            repo()
        ),
        _ => format!("{}: {e}", work.display()),
    })?;
    let result = (|| {
        let base = format!("{}/releases/download/v{version}", repo());
        let file = work.join(name);
        download(&format!("{base}/{name}"), &file)?;
        let sums = work.join("SHA256SUMS");
        download(&format!("{base}/SHA256SUMS"), &sums).map_err(|_| format!("release v{version} has no SHA256SUMS yet (it's still being built?); try again in a few minutes"))?;
        verify(&file, name, &std::fs::read_to_string(&sums).map_err(|e| e.to_string())?)?;
        let unpacked = work.join("new");
        std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
        unpack(&file, &unpacked)?;
        swap_in(how, &unpacked, &work)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::copy(path, to).map(|_| ()).map_err(|e| format!("{path}: {e}"));
    }
    let ok = Command::new("curl")
        .args(["-fsSL", "--max-time", "600", "-o"])
        .arg(to)
        .arg(url)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if ok { Ok(()) } else { Err(format!("couldn't download {url}")) }
}

/// Checks `file` against its line in a SHA256SUMS ("<hex>  <name>").
pub fn verify(file: &Path, name: &str, sums: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let expected = sums
        .lines()
        .find_map(|line| {
            let (hash, file) = line.split_once(char::is_whitespace)?;
            (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| format!("{name} isn't in the release's SHA256SUMS"))?;
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(file).map_err(|e| e.to_string())?;
    std::io::copy(&mut f, &mut hasher).map_err(|e| e.to_string())?;
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got == expected { Ok(()) } else { Err(format!("{name} doesn't match its SHA256SUMS (a broken download?); nothing was changed")) }
}

fn unpack(file: &Path, into: &Path) -> Result<(), String> {
    let name = file.to_string_lossy();
    let mut cmd = if cfg!(target_os = "macos") && name.ends_with(".zip") {
        // ditto keeps the app's signature and attributes.
        let mut c = Command::new("ditto");
        c.args(["-x", "-k"]).arg(file).arg(into);
        c
    } else {
        // Windows 10's tar reads zips too.
        let mut c = Command::new("tar");
        c.arg(if name.ends_with(".zip") { "-xf" } else { "-xzf" }).arg(file).arg("-C").arg(into);
        c
    };
    let ok = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if ok { Ok(()) } else { Err(format!("couldn't unpack {}", file.display())) }
}

/// The one folder an archive unpacked into (vmac-<target>/), else itself.
fn single_folder(dir: &Path) -> PathBuf {
    let entries: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().collect();
    match entries.as_slice() {
        [only] if only.path().is_dir() && only.path().extension().is_none_or(|e| e != "app") => only.path(),
        _ => dir.to_path_buf(),
    }
}

/// Moves the unpacked release over the install.
fn swap_in(how: &Install, unpacked: &Path, work: &Path) -> Result<(), String> {
    let err = |what: &str, e: std::io::Error| format!("{what}: {e}");
    match how {
        Install::MacApp(app) => {
            let new = unpacked.join("vmac.app");
            if !new.is_dir() {
                return Err("the release has no vmac.app".into());
            }
            let old = work.join("old.app");
            std::fs::rename(app, &old).map_err(|e| err("moving the old app aside", e))?;
            if let Err(e) = std::fs::rename(&new, app) {
                let _ = std::fs::rename(&old, app);
                return Err(err("moving the new app in", e));
            }
            Ok(())
        }
        Install::Linux(prefix) => {
            let root = single_folder(unpacked);
            if !root.join("bin/vmac").is_file() {
                return Err("the release has no bin/vmac".into());
            }
            move_tree(&root, prefix)
        }
        Install::Windows(dir) => {
            let root = single_folder(unpacked);
            let new = root.join("vmac.exe");
            if !new.is_file() {
                return Err("the release has no vmac.exe".into());
            }
            let exe = dir.join("vmac.exe");
            let old = dir.join("vmac.exe.old");
            let _ = std::fs::remove_file(&old);
            // A running program can be renamed, not replaced.
            if exe.exists() {
                std::fs::rename(&exe, &old).map_err(|e| err("moving the old vmac.exe aside", e))?;
            }
            if let Err(e) = std::fs::rename(&new, &exe) {
                let _ = std::fs::rename(&old, &exe);
                return Err(err("moving the new vmac.exe in", e));
            }
            Ok(())
        }
        _ => Err("not a release install".into()),
    }
}

/// Moves every file under `from` to the same place under `to`, each by a
/// rename (a running program keeps the file it had open).
fn move_tree(from: &Path, to: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(from).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            std::fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
            move_tree(&path, &dest)?;
        } else {
            std::fs::rename(&path, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        }
    }
    Ok(())
}

/// The version of the vmac program at `path` (`vmac --version`).
pub fn version_of(path: &Path) -> Option<String> {
    let out = Command::new(path).arg("--version").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.split_whitespace().nth(1)?.to_string();
    parse(&version).map(|_| version)
}

// ---- Checking once a day

static CHECK_ON: AtomicBool = AtomicBool::new(true);

/// vmac.set_update_check(false) turns the daily check off.
pub fn set_check(on: bool) {
    CHECK_ON.store(on, Ordering::Relaxed);
}

pub fn check_on() -> bool {
    CHECK_ON.load(Ordering::Relaxed) && std::env::var_os("VMAC_NO_UPDATE").is_none()
}

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn last_check_file() -> Option<PathBuf> {
    crate::packages::data_dir().map(|d| d.join("update-checked"))
}

/// Whether a day has passed since the last check (any vmac's on this machine).
fn check_due() -> bool {
    let Some(file) = last_check_file() else { return false };
    std::fs::metadata(&file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_none_or(|age| age >= DAY)
}

fn note_checked() {
    if let Some(file) = last_check_file() {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, current());
    }
}

/// What the server hears from the tracker.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A newer release is out (not installed).
    Out(String),
    /// A check asked for found nothing newer.
    UpToDate,
    /// Installed (the program on disk is now this version).
    Installed(String),
    /// Install it by running this in a terminal (cargo).
    Run(String),
    Failed(String),
}

/// The server's view of updates: checks (daily, or asked), installs asked
/// for, and whether the program on disk became newer than this one.
pub struct Tracker {
    checking: Option<(Receiver<Result<String, String>>, bool)>,
    installing: Option<Receiver<Result<Outcome, String>>>,
    /// The latest release known to be out.
    pub out: Option<String>,
    /// A newer program to restart onto: on disk here, or the version of a
    /// newer window that attached (its server is installed).
    pub installed: Option<String>,
    exe_mtime: Option<SystemTime>,
    last_look: Instant,
    started: Instant,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

const LOOK_EVERY: Duration = Duration::from_secs(3);
/// The first daily check waits until vmac has started up.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(10);

impl Tracker {
    pub fn new() -> Self {
        let exe_mtime = exe().and_then(|e| std::fs::metadata(e).and_then(|m| m.modified()).ok());
        Tracker {
            checking: None,
            installing: None,
            out: None,
            installed: None,
            exe_mtime,
            last_look: Instant::now(),
            started: Instant::now(),
        }
    }

    /// Checks for a release now, in the background; `asked`: say when
    /// there's none newer.
    pub fn check(&mut self, asked: bool) {
        if self.checking.is_some() {
            return;
        }
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(latest());
        });
        self.checking = Some((rx, asked));
    }

    /// Installs the latest release, in the background (checking first).
    pub fn install(&mut self) -> bool {
        if self.installing.is_some() {
            return false;
        }
        let (tx, rx) = channel();
        let known = self.out.clone();
        std::thread::spawn(move || {
            let result = match known.map(Ok).unwrap_or_else(latest) {
                Ok(v) if newer(&v, current()) => install(&v),
                Ok(_) => Err(format!("vmac {} is the latest", current())),
                Err(e) => Err(e),
            };
            let _ = tx.send(result);
        });
        self.installing = Some(rx);
        true
    }

    /// A window of a newer vmac attached (its version): this server should
    /// restart onto it.
    pub fn newer_window(&mut self, version: &str) -> Option<Event> {
        let known = self.installed.as_deref().unwrap_or(current());
        if newer(version, known) {
            self.installed = Some(version.to_string());
            return Some(Event::Installed(version.to_string()));
        }
        None
    }

    /// What happened since last asked.
    pub fn tend(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some((rx, asked)) = &self.checking
            && let Ok(result) = rx.try_recv()
        {
            let asked = *asked;
            self.checking = None;
            match result {
                Ok(v) => {
                    note_checked();
                    if newer(&v, current()) {
                        let news = self.out.as_deref() != Some(v.as_str()) || asked;
                        self.out = Some(v.clone());
                        if news && !self.installed.as_deref().is_some_and(|i| !newer(&v, i)) {
                            events.push(Event::Out(v));
                        }
                    } else if asked {
                        events.push(Event::UpToDate);
                    }
                }
                Err(e) if asked => events.push(Event::Failed(e)),
                Err(_) => {}
            }
        }
        if let Some(rx) = &self.installing
            && let Ok(result) = rx.try_recv()
        {
            self.installing = None;
            match result {
                // The look at the program on disk says so too, soon after.
                Ok(Outcome::Installed(v)) => {
                    if self.installed.as_deref().is_none_or(|i| newer(&v, i)) {
                        self.installed = Some(v.clone());
                        events.push(Event::Installed(v));
                    }
                }
                Ok(Outcome::Command(c)) => events.push(Event::Run(c)),
                Err(e) => events.push(Event::Failed(e)),
            }
        }
        if self.last_look.elapsed() >= LOOK_EVERY {
            self.last_look = Instant::now();
            if let Some(v) = self.look_at_program() {
                events.push(Event::Installed(v));
            }
            if !cfg!(test)
                && self.checking.is_none()
                && self.started.elapsed() >= FIRST_CHECK_AFTER
                && check_on()
                && Install::here().checks()
                && check_due()
            {
                // Noted now, so other vmacs here don't check too.
                note_checked();
                self.check(false);
            }
        }
        events
    }

    /// When the program this was started from changed on disk (an update,
    /// by vmac or otherwise), its version, if newer.
    fn look_at_program(&mut self) -> Option<String> {
        let exe = exe()?;
        let mtime = std::fs::metadata(&exe).and_then(|m| m.modified()).ok()?;
        if Some(mtime) == self.exe_mtime {
            return None;
        }
        let version = version_of(&exe)?;
        // Read once it runs (a file half written doesn't).
        self.exe_mtime = Some(mtime);
        let known = self.installed.as_deref().unwrap_or(current());
        if newer(&version, known) {
            self.installed = Some(version.clone());
            return Some(version);
        }
        None
    }
}

// ---- Restarting

/// Sessions to open again after a restart, written by the server going
/// away and read by the one starting.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Restore {
    pub active: String,
    pub sessions: Vec<RestoreSession>,
    /// When it was written (seconds since 1970): an old one is ignored.
    pub written: u64,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Default)]
pub struct RestoreSession {
    pub name: String,
    pub project: Option<PathBuf>,
    pub files: Vec<PathBuf>,
}

fn restore_file() -> Option<PathBuf> {
    crate::packages::data_dir().map(|d| d.join("restart.json"))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn save_restore(mut restore: Restore) -> std::io::Result<()> {
    let file = restore_file().ok_or_else(|| std::io::Error::other("no data folder"))?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    restore.written = now_secs();
    crate::sys::write_private(&file, serde_json::to_string(&restore).unwrap_or_default().as_bytes())
}

/// The sessions a restart left to open (taken: only once), if it was just now.
pub fn take_restore() -> Option<Restore> {
    let file = restore_file()?;
    let text = std::fs::read_to_string(&file).ok()?;
    let _ = std::fs::remove_file(&file);
    let restore: Restore = serde_json::from_str(&text).ok()?;
    (now_secs().saturating_sub(restore.written) < 120).then_some(restore)
}

/// How the window was started, for starting it again on the new program.
#[derive(Debug, Clone, PartialEq)]
pub enum Relaunch {
    /// Run these arguments again (after stopping the local server first, when
    /// `kill_local` is set).
    Args { args: Vec<String>, kill_local: bool },
}

/// The arguments to start this window again with: a `vmac connect` gets
/// `--restart` (its server is the one restarting), a `vmac attach` stops the
/// local server first.
pub fn relaunch_for(args: &[String]) -> Relaunch {
    match args.first().map(String::as_str) {
        Some("connect" | "c") => {
            let mut args = args.to_vec();
            if !args.iter().any(|a| a == "--restart") {
                args.insert(1, "--restart".into());
            }
            Relaunch::Args { args, kill_local: false }
        }
        Some("attach" | "a") => Relaunch::Args { args: args.to_vec(), kill_local: true },
        // A file given is open again with the rest.
        _ => Relaunch::Args { args: args.to_vec(), kill_local: false },
    }
}

/// Starts this window again on the program now at its path, and leaves.
pub fn relaunch() -> ! {
    let args = ARGS.get().cloned().unwrap_or_default();
    let Relaunch::Args { args, kill_local } = relaunch_for(&args);
    if kill_local {
        let _ = crate::daemon::kill(&crate::daemon::socket_path());
        // Until it's gone, so the new window starts a new one.
        let socket = crate::daemon::socket_path();
        for _ in 0..50 {
            if crate::sys::UnixStream::connect(&socket).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let how = Install::here();
    // The app through its launcher (it sets PATH), else the program itself.
    let program = match &how {
        Install::MacApp(app) => app.join("Contents/MacOS/vmac"),
        _ => exe().unwrap_or_else(|| PathBuf::from("vmac")),
    };
    let mut cmd = Command::new(&program);
    cmd.args(&args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec();
        eprintln!("vmac: couldn't start {}: {e}", program.display());
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        match cmd.spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => {
                eprintln!("vmac: couldn't start {}: {e}", program.display());
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert_eq!(parse("v0.0.9"), Some((0, 0, 9)));
        assert_eq!(parse("1.2"), Some((1, 2, 0)));
        assert_eq!(parse("0.1.0-beta"), Some((0, 1, 0)));
        assert_eq!(parse("nope"), None);
        assert!(newer("0.0.10", "0.0.9"));
        assert!(newer("v0.1.0", "0.0.99"));
        assert!(!newer("0.0.8", "0.0.8"));
        assert!(!newer("0.0.7", "0.0.8"));
        assert!(!newer("junk", "0.0.8"));
    }

    #[test]
    fn installs_are_told_apart() {
        let tmp = std::env::temp_dir().join(format!("vmac-detect-{}", std::process::id()));
        let manifest = tmp.join("checkout");
        std::fs::create_dir_all(manifest.join(".git")).unwrap();
        let prefix = tmp.join("local");
        std::fs::create_dir_all(prefix.join("share/applications")).unwrap();
        std::fs::write(prefix.join("share/applications/vmac.desktop"), "").unwrap();
        let cargo = tmp.join(".cargo");
        let d = |exe: PathBuf| detect(&exe, Some(&cargo), &manifest, true);

        assert_eq!(d(manifest.join("target/debug/vmac")), Install::Checkout);
        assert_eq!(d(cargo.join("bin/vmac")), Install::Cargo);
        assert_eq!(d(PathBuf::from("/Applications/vmac.app/Contents/MacOS/vmac-bin")), Install::MacApp("/Applications/vmac.app".into()));
        assert_eq!(d(prefix.join("bin/vmac")), Install::Linux(prefix.clone()));
        assert_eq!(d(PathBuf::from("/usr/bin/vmac")), Install::Other("/usr/bin/vmac".into()));
        assert_eq!(d(tmp.join("tools/vmac.exe")), Install::Windows(tmp.join("tools")));
        assert_eq!(d(tmp.join(".vmac/server/0.0.8-abc/vmac")), Install::Server);
        assert_eq!(detect(&prefix.join("bin/vmac"), Some(&cargo), &manifest, false), Install::Server);
        assert!(Install::Cargo.checks() && !Install::Checkout.checks() && !Install::Server.checks());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_release_isnt_out_until_its_sums_are() {
        let repo = std::env::temp_dir().join(format!("vmac-ready-{}", std::process::id()));
        let download = repo.join("releases/download/v9.9.9");
        std::fs::create_dir_all(&download).unwrap();
        // SAFETY: the only test reading VMAC_UPDATE_URL.
        unsafe { std::env::set_var("VMAC_UPDATE_URL", format!("file://{}", repo.display())) };
        std::fs::write(repo.join("releases/latest"), "v9.9.9\n").unwrap();
        assert!(latest().unwrap_err().contains("still being built"));
        std::fs::write(download.join("SHA256SUMS"), "").unwrap();
        assert_eq!(latest().unwrap(), "9.9.9");
        unsafe { std::env::remove_var("VMAC_UPDATE_URL") };
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_download_is_checked_against_the_sums() {
        let tmp = std::env::temp_dir().join(format!("vmac-sums-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("vmac.tar.gz");
        std::fs::write(&file, b"hello").unwrap();
        let hello = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify(&file, "vmac.tar.gz", &format!("{hello}  vmac.tar.gz\n")).is_ok());
        assert!(verify(&file, "vmac.tar.gz", &format!("{hello} *vmac.tar.gz\n")).is_ok());
        assert!(verify(&file, "vmac.tar.gz", &format!("{}  vmac.tar.gz\n", "0".repeat(64))).is_err());
        assert!(verify(&file, "vmac.tar.gz", &format!("{hello}  other.zip\n")).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_linux_release_is_moved_over_the_old() {
        let tmp = std::env::temp_dir().join(format!("vmac-swap-{}", std::process::id()));
        let prefix = tmp.join("prefix");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::write(prefix.join("bin/vmac"), "old").unwrap();
        std::fs::write(prefix.join("bin/other"), "kept").unwrap();
        let unpacked = tmp.join("work/new/vmac-x86_64-unknown-linux-gnu");
        std::fs::create_dir_all(unpacked.join("bin")).unwrap();
        std::fs::create_dir_all(unpacked.join("share/applications")).unwrap();
        std::fs::write(unpacked.join("bin/vmac"), "new").unwrap();
        std::fs::write(unpacked.join("share/applications/vmac.desktop"), "desktop").unwrap();
        swap_in(&Install::Linux(prefix.clone()), &tmp.join("work/new"), &tmp.join("work")).unwrap();
        assert_eq!(std::fs::read_to_string(prefix.join("bin/vmac")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(prefix.join("bin/other")).unwrap(), "kept");
        assert_eq!(std::fs::read_to_string(prefix.join("share/applications/vmac.desktop")).unwrap(), "desktop");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn relaunching_restarts_what_the_window_was_on() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            relaunch_for(&args(&["connect", "devbox", "notes"])),
            Relaunch::Args { args: args(&["connect", "--restart", "devbox", "notes"]), kill_local: false }
        );
        assert_eq!(relaunch_for(&args(&["attach", "api"])), Relaunch::Args { args: args(&["attach", "api"]), kill_local: true });
        assert_eq!(relaunch_for(&args(&["notes.md"])), Relaunch::Args { args: args(&["notes.md"]), kill_local: false });
    }
}
