//! Packages: language servers and tree-sitter grammars that vmac installs
//! for itself, the way Neovim's Mason and nvim-treesitter do.
//!
//! Opening a file whose server or grammar is missing installs it in the
//! background (unless `auto-install` is off), into vmac's data folder
//! (`$XDG_DATA_HOME/vmac`, else `~/.local/share/vmac`):
//!
//! - `packages/NAME/`: a server, from npm, `go install`, pip, or a GitHub
//!   release; `bin/COMMAND` is a small script running it. Servers are found
//!   there first, then on PATH.
//! - `grammars/LANGUAGE/`: a grammar cloned with git and compiled with the
//!   C compiler (`parser.so`), and its `highlights.scm`.
//!
//! `:packages` lists them all (RET installs or removes one); `:install NAME`
//! and `:uninstall NAME` do it by name. Each install's output is kept in
//! `logs/NAME.log`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Where a server comes from.
#[derive(Debug, Clone, Copy)]
pub enum Source {
    /// npm packages (the first is the server); commands in node_modules/.bin.
    Npm(&'static [&'static str]),
    /// `go install MODULE@latest`.
    Go(&'static str),
    /// A pip package, in its own virtual environment.
    Pip(&'static str),
    /// The latest GitHub release of a repository: the asset `asset` picks
    /// for this OS and architecture (substrings its name must contain), and
    /// where the command is inside it (a .gz holds just the command).
    Github { repo: &'static str, asset: fn(&str, &str) -> Option<&'static [&'static str]>, bin: &'static str },
}

/// A language server vmac can install.
#[derive(Debug, Clone, Copy)]
pub struct Server {
    pub name: &'static str,
    /// The commands it provides, as `(lsp-servers)` names them.
    pub commands: &'static [&'static str],
    pub languages: &'static [&'static str],
    pub source: Source,
}

/// A tree-sitter grammar vmac can install.
#[derive(Debug, Clone, Copy)]
pub struct Grammar {
    pub language: &'static str,
    /// GitHub `owner/repo`.
    pub repo: &'static str,
    /// The folder holding `src/` and `queries/`, when it isn't the top.
    pub subdir: &'static str,
    /// Its C symbol: `tree_sitter_<symbol>`.
    pub symbol: &'static str,
    /// A language whose highlights come first (C++ adds to C's).
    pub inherits: Option<&'static str>,
    /// The branch to clone, when the default one lacks a generated
    /// src/parser.c.
    pub branch: &'static str,
}

const fn grammar(language: &'static str, repo: &'static str) -> Grammar {
    Grammar { language, repo, subdir: "", symbol: language, inherits: None, branch: "" }
}

pub const GRAMMARS: &[Grammar] = &[
    grammar("lua", "tree-sitter-grammars/tree-sitter-lua"),
    grammar("yaml", "tree-sitter-grammars/tree-sitter-yaml"),
    grammar("html", "tree-sitter/tree-sitter-html"),
    grammar("css", "tree-sitter/tree-sitter-css"),
    grammar("java", "tree-sitter/tree-sitter-java"),
    grammar("ruby", "tree-sitter/tree-sitter-ruby"),
    Grammar { inherits: Some("c"), ..grammar("cpp", "tree-sitter/tree-sitter-cpp") },
    grammar("zig", "tree-sitter-grammars/tree-sitter-zig"),
    grammar("nix", "nix-community/tree-sitter-nix"),
    Grammar { subdir: "php", ..grammar("php", "tree-sitter/tree-sitter-php") },
    grammar("haskell", "tree-sitter/tree-sitter-haskell"),
    grammar("elixir", "elixir-lang/tree-sitter-elixir"),
    grammar("scala", "tree-sitter/tree-sitter-scala"),
    Grammar { symbol: "c_sharp", ..grammar("c_sharp", "tree-sitter/tree-sitter-c-sharp") },
    Grammar { branch: "with-generated-files", ..grammar("swift", "alex-pinkus/tree-sitter-swift") },
    grammar("kotlin", "fwcd/tree-sitter-kotlin"),
];

fn rust_analyzer_asset(os: &str, arch: &str) -> Option<&'static [&'static str]> {
    Some(match (os, arch) {
        ("macos", "aarch64") => &["aarch64-apple-darwin.gz"],
        ("macos", "x86_64") => &["x86_64-apple-darwin.gz"],
        ("linux", "aarch64") => &["aarch64-unknown-linux-gnu.gz"],
        ("linux", "x86_64") => &["x86_64-unknown-linux-gnu.gz"],
        _ => return None,
    })
}

fn kotlin_ls_asset(_os: &str, _arch: &str) -> Option<&'static [&'static str]> {
    // One zip for every platform: it runs on the JVM.
    Some(&["server.zip"])
}

fn clangd_asset(os: &str, _arch: &str) -> Option<&'static [&'static str]> {
    Some(match os {
        "macos" => &["clangd-mac-", ".zip"],
        "linux" => &["clangd-linux-", ".zip"],
        _ => return None,
    })
}

fn lua_ls_asset(os: &str, arch: &str) -> Option<&'static [&'static str]> {
    Some(match (os, arch) {
        ("macos", "aarch64") => &["darwin-arm64.tar.gz"],
        ("macos", "x86_64") => &["darwin-x64.tar.gz"],
        ("linux", "aarch64") => &["linux-arm64.tar.gz"],
        ("linux", "x86_64") => &["linux-x64.tar.gz"],
        _ => return None,
    })
}

fn marksman_asset(os: &str, arch: &str) -> Option<&'static [&'static str]> {
    Some(match (os, arch) {
        ("macos", _) => &["marksman-macos"],
        ("linux", "aarch64") => &["marksman-linux-arm64"],
        ("linux", "x86_64") => &["marksman-linux-x64"],
        _ => return None,
    })
}

fn taplo_asset(os: &str, arch: &str) -> Option<&'static [&'static str]> {
    Some(match (os, arch) {
        ("macos", "aarch64") => &["taplo-darwin-aarch64.gz"],
        ("macos", "x86_64") => &["taplo-darwin-x86_64.gz"],
        ("linux", "aarch64") => &["taplo-linux-aarch64.gz"],
        ("linux", "x86_64") => &["taplo-linux-x86_64.gz"],
        _ => return None,
    })
}

pub const SERVERS: &[Server] = &[
    Server {
        name: "rust-analyzer",
        commands: &["rust-analyzer"],
        languages: &["rust"],
        source: Source::Github { repo: "rust-lang/rust-analyzer", asset: rust_analyzer_asset, bin: "rust-analyzer" },
    },
    Server { name: "pyright", commands: &["pyright-langserver"], languages: &["python"], source: Source::Npm(&["pyright"]) },
    Server {
        name: "typescript-language-server",
        commands: &["typescript-language-server"],
        languages: &["typescript", "tsx", "javascript"],
        source: Source::Npm(&["typescript-language-server", "typescript"]),
    },
    Server {
        name: "clangd",
        commands: &["clangd"],
        languages: &["c", "cpp"],
        source: Source::Github { repo: "clangd/clangd", asset: clangd_asset, bin: "bin/clangd" },
    },
    Server { name: "gopls", commands: &["gopls"], languages: &["go"], source: Source::Go("golang.org/x/tools/gopls") },
    Server {
        name: "lua-language-server",
        commands: &["lua-language-server"],
        languages: &["lua"],
        source: Source::Github { repo: "LuaLS/lua-language-server", asset: lua_ls_asset, bin: "bin/lua-language-server" },
    },
    Server {
        name: "bash-language-server",
        commands: &["bash-language-server"],
        languages: &["bash"],
        source: Source::Npm(&["bash-language-server"]),
    },
    Server {
        name: "yaml-language-server",
        commands: &["yaml-language-server"],
        languages: &["yaml"],
        source: Source::Npm(&["yaml-language-server"]),
    },
    Server {
        name: "vscode-langservers-extracted",
        commands: &["vscode-html-language-server", "vscode-css-language-server", "vscode-json-language-server"],
        languages: &["html", "css", "json"],
        source: Source::Npm(&["vscode-langservers-extracted"]),
    },
    Server {
        name: "marksman",
        commands: &["marksman"],
        languages: &["markdown"],
        source: Source::Github { repo: "artempyanykh/marksman", asset: marksman_asset, bin: "marksman" },
    },
    Server {
        name: "taplo",
        commands: &["taplo"],
        languages: &["toml"],
        source: Source::Github { repo: "tamasfe/taplo", asset: taplo_asset, bin: "taplo" },
    },
    Server {
        name: "kotlin-language-server",
        commands: &["kotlin-language-server"],
        languages: &["kotlin"],
        source: Source::Github {
            repo: "fwcd/kotlin-language-server",
            asset: kotlin_ls_asset,
            bin: "bin/kotlin-language-server",
        },
    },
    Server { name: "python-lsp-server", commands: &["pylsp"], languages: &["python"], source: Source::Pip("python-lsp-server") },
];

// ---- Where things go

/// vmac's data folder: VMAC_DATA_DIR, else $XDG_DATA_HOME/vmac, else
/// ~/.local/share/vmac.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("VMAC_DATA_DIR") {
        return Some(PathBuf::from(d));
    }
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    Some(base.join("vmac"))
}

fn bin_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("bin"))
}

fn grammar_dir(language: &str) -> Option<PathBuf> {
    data_dir().map(|d| d.join("grammars").join(language))
}

pub fn on_path(command: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(command).is_file()))
}

// ---- State shared by the editor, the highlighter and the install threads

static AUTO_INSTALL: AtomicBool = AtomicBool::new(false);
/// Bumped when an install finishes, so grammars that weren't there are
/// looked for again.
static GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Jobs {
    /// Installs and removals running, by package name, with what each is doing.
    running: HashMap<String, Arc<Mutex<String>>>,
    /// Packages auto-install already tried this session (once each).
    tried: HashSet<String>,
    /// What to tell the person, oldest first.
    notices: Vec<String>,
}

static JOBS: Mutex<Option<Jobs>> = Mutex::new(None);

fn jobs<T>(f: impl FnOnce(&mut Jobs) -> T) -> T {
    let mut guard = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Jobs::default))
}

/// Whether opening a file installs its missing server and grammar.
pub fn set_auto_install(on: bool) {
    AUTO_INSTALL.store(on, Ordering::Relaxed);
}

pub fn auto_install() -> bool {
    AUTO_INSTALL.load(Ordering::Relaxed)
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// The next thing to tell the person (an install started, finished, failed).
pub fn take_notice() -> Option<String> {
    jobs(|j| (!j.notices.is_empty()).then(|| j.notices.remove(0)))
}

/// What installs are doing, for the modeline: "installing gopls".
pub fn status() -> String {
    jobs(|j| {
        let mut names: Vec<&String> = j.running.keys().collect();
        names.sort();
        match names.as_slice() {
            [] => String::new(),
            [one] => format!("installing {one}"),
            many => format!("installing {} packages", many.len()),
        }
    })
}

pub fn notify(text: String) {
    jobs(|j| j.notices.push(text));
}

// ---- Looking things up

pub fn server_named(name: &str) -> Option<&'static Server> {
    SERVERS.iter().find(|s| s.name == name)
}

fn server_for_command(command: &str) -> Option<&'static Server> {
    SERVERS.iter().find(|s| s.commands.contains(&command))
}

pub fn grammar_for(language: &str) -> Option<&'static Grammar> {
    GRAMMARS.iter().find(|g| g.language == language)
}

/// A grammar's package name: `tree-sitter-lua`.
fn grammar_package(language: &str) -> String {
    format!("tree-sitter-{language}")
}

pub fn server_installed(server: &Server) -> bool {
    bin_dir().is_some_and(|bin| server.commands.iter().all(|c| bin.join(c).is_file()))
}

pub fn grammar_installed(language: &str) -> bool {
    grammar_dir(language).is_some_and(|d| d.join("parser.so").is_file())
}

/// The program to run for a server's command: vmac's own install, else the
/// command as is (found on PATH). When it is nowhere and vmac can install
/// it, the install starts (with auto-install on) and None says to wait.
pub fn resolve_command(command: &str) -> Option<String> {
    if let Some(path) = bin_dir().map(|b| b.join(command)).filter(|p| p.is_file()) {
        return Some(path.display().to_string());
    }
    if command.contains('/') || on_path(command) {
        return Some(command.to_string());
    }
    let Some(server) = server_for_command(command) else { return Some(command.to_string()) };
    if jobs(|j| j.running.contains_key(server.name)) {
        return None;
    }
    if auto_install() && jobs(|j| j.tried.insert(server.name.to_string())) {
        let _ = install(server.name);
        return None;
    }
    Some(command.to_string())
}

/// Asks for a language's grammar: installs it when it's missing and
/// auto-install is on. Returns whether one is coming.
pub fn want_grammar(language: &str) -> bool {
    if grammar_for(language).is_none() || grammar_installed(language) {
        return false;
    }
    let name = grammar_package(language);
    if jobs(|j| j.running.contains_key(&name)) {
        return true;
    }
    if auto_install() && jobs(|j| j.tried.insert(name.clone())) {
        let _ = install(&name);
        return true;
    }
    false
}

/// Every package, one line each for a picker: "✓ name  (server: rust)".
pub fn list() -> Vec<String> {
    let running: HashSet<String> = jobs(|j| j.running.keys().cloned().collect());
    let mark = |name: &str, installed: bool| {
        if running.contains(name) {
            "…"
        } else if installed {
            "✓"
        } else {
            " "
        }
    };
    let mut out: Vec<String> = SERVERS
        .iter()
        .map(|s| format!("{} {}  (server: {})", mark(s.name, server_installed(s)), s.name, s.languages.join(", ")))
        .collect();
    out.extend(GRAMMARS.iter().map(|g| {
        let name = grammar_package(g.language);
        format!("{} {name}  (grammar)", mark(&name, grammar_installed(g.language)))
    }));
    out
}

/// The package named in a `list` line.
pub fn name_in_line(line: &str) -> String {
    line.trim_start_matches(['✓', '…', ' ']).split_whitespace().next().unwrap_or("").to_string()
}

pub fn is_installed(name: &str) -> bool {
    match (server_named(name), name.strip_prefix("tree-sitter-")) {
        (Some(s), _) => server_installed(s),
        (None, Some(lang)) => grammar_installed(lang),
        _ => false,
    }
}

// ---- Installing

/// Starts installing a package in the background. Err when there's no
/// such package or it's already being installed.
pub fn install(name: &str) -> Result<(), String> {
    let job: Box<dyn FnOnce(&Step) -> Result<(), String> + Send> = if let Some(server) = server_named(name) {
        let server = *server;
        Box::new(move |step| install_server(&server, step))
    } else if let Some(g) = name.strip_prefix("tree-sitter-").and_then(grammar_for) {
        let g = *g;
        Box::new(move |step| install_grammar(&g, step))
    } else {
        return Err(format!("No package named {name}; :packages lists them"));
    };
    run_job(name, "Installing", "Installed", job)
}

/// Removes an installed package.
pub fn uninstall(name: &str) -> Result<(), String> {
    let data = data_dir().ok_or("no home folder")?;
    if let Some(server) = server_named(name) {
        let _ = std::fs::remove_dir_all(data.join("packages").join(server.name));
        for c in server.commands {
            let _ = std::fs::remove_file(data.join("bin").join(c));
        }
    } else if let Some(lang) = name.strip_prefix("tree-sitter-").filter(|l| grammar_for(l).is_some()) {
        let _ = std::fs::remove_dir_all(data.join("grammars").join(lang));
    } else {
        return Err(format!("No package named {name}"));
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn run_job(
    name: &str,
    doing: &str,
    done: &str,
    job: Box<dyn FnOnce(&Step) -> Result<(), String> + Send>,
) -> Result<(), String> {
    let state = Arc::new(Mutex::new(String::new()));
    let fresh = jobs(|j| {
        if j.running.contains_key(name) {
            return false;
        }
        j.tried.insert(name.to_string());
        j.running.insert(name.to_string(), state.clone());
        true
    });
    if !fresh {
        return Err(format!("{name} is already being installed"));
    }
    notify(format!("{doing} {name}…"));
    let (name, done) = (name.to_string(), done.to_string());
    std::thread::spawn(move || {
        let step = Step::new(&name, state);
        let result = job(&step);
        jobs(|j| j.running.remove(&name));
        GENERATION.fetch_add(1, Ordering::Relaxed);
        notify(match result {
            Ok(()) => format!("{done} {name}"),
            Err(e) => format!("{name} didn't install: {e} (see {})", step.log.display()),
        });
    });
    Ok(())
}

/// One install's steps: runs commands, logging what they print.
struct Step {
    log: PathBuf,
    state: Arc<Mutex<String>>,
}

impl Step {
    fn new(name: &str, state: Arc<Mutex<String>>) -> Self {
        let logs = data_dir().unwrap_or_else(std::env::temp_dir).join("logs");
        let _ = std::fs::create_dir_all(&logs);
        let log = logs.join(format!("{name}.log"));
        let _ = std::fs::write(&log, "");
        Self { log, state }
    }

    fn note(&self, text: &str) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = text.to_string();
        self.append(&format!("# {text}\n"));
    }

    fn append(&self, text: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(&self.log) {
            let _ = f.write_all(text.as_bytes());
        }
    }

    /// Runs a program; Err with its last line of output when it fails.
    fn run(&self, program: &str, args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> Result<String, String> {
        self.append(&format!("$ {program} {}\n", args.join(" ")));
        let out = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("{program}: {e}"))?;
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        self.append(&stdout);
        self.append(&stderr);
        if out.status.success() {
            Ok(stdout.into_owned())
        } else {
            let last = stderr.lines().chain(stdout.lines()).filter(|l| !l.trim().is_empty()).last().unwrap_or("failed");
            Err(format!("{program}: {}", last.trim()))
        }
    }
}

fn fresh_dir(dir: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// `bin/COMMAND`: a script that runs the installed program.
fn link_command(command: &str, target: &Path) -> Result<(), String> {
    let bin = bin_dir().ok_or("no home folder")?;
    std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    if !target.is_file() {
        return Err(format!("{} isn't there after installing", target.display()));
    }
    let script = format!("#!/bin/sh\nexec '{}' \"$@\"\n", target.display().to_string().replace('\'', r"'\''"));
    let path = bin.join(command);
    std::fs::write(&path, script).map_err(|e| e.to_string())?;
    make_executable(&path)
}

fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

fn install_server(server: &Server, step: &Step) -> Result<(), String> {
    let dir = data_dir().ok_or("no home folder")?.join("packages").join(server.name);
    fresh_dir(&dir)?;
    let dir_s = dir.display().to_string();
    let targets: Vec<PathBuf> = match server.source {
        Source::Npm(packages) => {
            if !on_path("npm") {
                return Err("it needs npm (Node.js)".into());
            }
            step.note(&format!("npm install {}", packages.join(" ")));
            let mut args = vec!["install", "--prefix", &dir_s, "--no-save", "--no-fund", "--no-audit"];
            args.extend(packages);
            step.run("npm", &args, &dir, &[])?;
            server.commands.iter().map(|c| dir.join("node_modules/.bin").join(c)).collect()
        }
        Source::Go(module) => {
            if !on_path("go") {
                return Err("it needs Go".into());
            }
            step.note(&format!("go install {module}"));
            let gobin = dir.join("bin").display().to_string();
            step.run("go", &["install", &format!("{module}@latest")], &dir, &[("GOBIN", &gobin)])?;
            server.commands.iter().map(|c| dir.join("bin").join(c)).collect()
        }
        Source::Pip(package) => {
            step.note(&format!("pip install {package}"));
            step.run("python3", &["-m", "venv", "venv"], &dir, &[])?;
            step.run(&dir.join("venv/bin/pip").display().to_string(), &["install", "--quiet", package], &dir, &[])?;
            server.commands.iter().map(|c| dir.join("venv/bin").join(c)).collect()
        }
        Source::Github { repo, asset, bin } => {
            let wanted = asset(std::env::consts::OS, std::env::consts::ARCH)
                .ok_or_else(|| format!("no release for {} {}", std::env::consts::OS, std::env::consts::ARCH))?;
            step.note(&format!("finding the latest release of {repo}"));
            let api = format!("https://api.github.com/repos/{repo}/releases/latest");
            let json = step.run("curl", &["-fsSL", "-H", "Accept: application/vnd.github+json", &api], &dir, &[])?;
            let release: serde_json::Value = serde_json::from_str(&json).map_err(|e| format!("GitHub's answer: {e}"))?;
            let (file, url) = release["assets"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|a| {
                    let name = a["name"].as_str()?;
                    let url = a["browser_download_url"].as_str()?;
                    wanted.iter().all(|w| name.contains(w)).then(|| (name.to_string(), url.to_string()))
                })
                .ok_or_else(|| format!("no asset matching {wanted:?} in {repo}'s latest release"))?;
            step.note(&format!("downloading {file}"));
            step.run("curl", &["-fsSL", "-o", &file, &url], &dir, &[])?;
            let target = unpack(step, &dir, &file, bin)?;
            make_executable(&target)?;
            vec![target]
        }
    };
    // One program can provide several commands (vscode-langservers-extracted).
    for (command, target) in server.commands.iter().zip(targets.iter().cycle()) {
        link_command(command, target)?;
    }
    Ok(())
}

/// Unpacks a downloaded release in `dir`, returning where `bin` is in it.
fn unpack(step: &Step, dir: &Path, file: &str, bin: &str) -> Result<PathBuf, String> {
    let name = Path::new(bin).file_name().and_then(|n| n.to_str()).unwrap_or(bin);
    if file.ends_with(".tar.gz") || file.ends_with(".tgz") {
        step.run("tar", &["xzf", file], dir, &[])?;
    } else if file.ends_with(".zip") {
        step.run("unzip", &["-q", "-o", file], dir, &[])?;
    } else if file.ends_with(".gz") {
        step.run("sh", &["-c", &format!("gunzip -c '{file}' > '{name}'")], dir, &[])?;
        return Ok(dir.join(name));
    } else {
        // The asset is the program itself.
        if file != name {
            std::fs::rename(dir.join(file), dir.join(name)).map_err(|e| e.to_string())?;
        }
        return Ok(dir.join(name));
    }
    let _ = std::fs::remove_file(dir.join(file));
    // Archives often hold a versioned folder (clangd_19.1.2/bin/clangd).
    find_file(dir, bin).ok_or_else(|| format!("{bin} isn't in {file}"))
}

/// A file whose path ends with `tail` (`bin/clangd`), anywhere under `dir`.
fn find_file(dir: &Path, tail: &str) -> Option<PathBuf> {
    let direct = dir.join(tail);
    if direct.is_file() {
        return Some(direct);
    }
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir()
            && !path.is_symlink()
            && let Some(found) = find_file(&path, tail)
        {
            return Some(found);
        }
    }
    None
}

fn install_grammar(g: &Grammar, step: &Step) -> Result<(), String> {
    let data = data_dir().ok_or("no home folder")?;
    let build = data.join("build").join(g.language);
    let out = data.join("grammars").join(g.language);
    let _ = std::fs::remove_dir_all(&build);
    std::fs::create_dir_all(build.parent().unwrap_or(&data)).map_err(|e| e.to_string())?;
    step.note(&format!("cloning {}", g.repo));
    let url = format!("https://github.com/{}", g.repo);
    let build_arg = build.display().to_string();
    let mut clone = vec!["clone", "--depth", "1"];
    if !g.branch.is_empty() {
        clone.extend(["--branch", g.branch]);
    }
    clone.extend([url.as_str(), build_arg.as_str()]);
    step.run("git", &clone, &data, &[("GIT_TERMINAL_PROMPT", "0")])?;
    let top = if g.subdir.is_empty() { build.clone() } else { build.join(g.subdir) };
    let src = top.join("src");
    step.note(&format!("compiling the {} grammar", g.language));
    let mut objects = Vec::new();
    for (file, compiler, std) in
        [("parser.c", "cc", "-std=c11"), ("scanner.c", "cc", "-std=c11"), ("scanner.cc", "c++", "-std=c++14")]
    {
        if !src.join(file).is_file() {
            continue;
        }
        let object = format!("{file}.o");
        step.run(compiler, &["-c", "-fPIC", "-O2", std, "-I", "src", &format!("src/{file}"), "-o", &object], &top, &[])?;
        objects.push(object);
    }
    if objects.is_empty() {
        return Err(format!("{} has no src/parser.c", g.repo));
    }
    let linker = if objects.iter().any(|o| o.ends_with(".cc.o")) { "c++" } else { "cc" };
    let mut args = vec!["-shared", "-o", "parser.so"];
    args.extend(objects.iter().map(String::as_str));
    step.run(linker, &args, &top, &[])?;
    fresh_dir(&out)?;
    std::fs::copy(top.join("parser.so"), out.join("parser.so")).map_err(|e| e.to_string())?;
    // Highlights: the grammar's own, or the repository's top-level ones.
    let query = [top.join("queries/highlights.scm"), build.join("queries/highlights.scm")]
        .into_iter()
        .find(|p| p.is_file())
        .ok_or("it has no queries/highlights.scm")?;
    std::fs::copy(query, out.join("highlights.scm")).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&build);
    // Loading it once now catches a grammar this tree-sitter can't use.
    load_grammar(g.language).map(|_| ())
}

/// An installed grammar: its language and highlight query source.
pub fn load_grammar(language: &str) -> Result<(tree_sitter::Language, String), String> {
    let g = grammar_for(language).ok_or_else(|| format!("no grammar for {language}"))?;
    let dir = grammar_dir(language).ok_or("no home folder")?;
    // SAFETY: the library is a tree-sitter grammar vmac compiled; it stays
    // loaded for good (its language is used for as long as vmac runs).
    let language_obj = unsafe {
        let lib = libloading::Library::new(dir.join("parser.so")).map_err(|e| e.to_string())?;
        let symbol = format!("tree_sitter_{}", g.symbol);
        let f: libloading::Symbol<unsafe extern "C" fn() -> *const ()> =
            lib.get(symbol.as_bytes()).map_err(|e| format!("{symbol}: {e}"))?;
        let language = tree_sitter::Language::new(tree_sitter_language::LanguageFn::from_raw(*f));
        std::mem::forget(lib);
        language
    };
    let mut probe = tree_sitter::Parser::new();
    probe.set_language(&language_obj).map_err(|e| format!("the {language} grammar: {e}"))?;
    let query = std::fs::read_to_string(dir.join("highlights.scm")).map_err(|e| e.to_string())?;
    Ok((language_obj, query))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_server_and_grammar_has_a_unique_name() {
        let mut names: Vec<String> = SERVERS.iter().map(|s| s.name.to_string()).collect();
        names.extend(GRAMMARS.iter().map(|g| grammar_package(g.language)));
        let unique: HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        assert!(list().iter().any(|l| name_in_line(l) == "rust-analyzer"));
        assert_eq!(name_in_line("✓ tree-sitter-lua  (grammar)"), "tree-sitter-lua");
    }

    #[test]
    fn release_assets_exist_for_this_machine() {
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        for s in SERVERS {
            if let Source::Github { asset, .. } = s.source {
                assert!(asset(os, arch).is_some(), "{} on {os} {arch}", s.name);
            }
        }
    }

    #[test]
    fn found_on_path_commands_run_as_they_are() {
        assert_eq!(resolve_command("sh").as_deref(), Some("sh"));
        assert_eq!(resolve_command("no-such-server-xyz").as_deref(), Some("no-such-server-xyz"), "unknown: left to fail");
    }

    /// Installs a grammar and a server for real (network, compilers):
    /// `cargo test packages -- --ignored`.
    #[test]
    #[ignore]
    fn installs_a_grammar_and_a_server() {
        let dir = std::env::temp_dir().join(format!("vmac-packages-{}", std::process::id()));
        // SAFETY: only this ignored test reads VMAC_DATA_DIR while it runs.
        unsafe { std::env::set_var("VMAC_DATA_DIR", &dir) };
        let names: Vec<String> = match std::env::var("VMAC_PACKAGES") {
            Ok(list) => list.split(',').map(str::to_string).collect(),
            Err(_) => vec!["tree-sitter-lua".into(), "marksman".into()],
        };
        for name in &names {
            install(name).unwrap();
        }
        let start = std::time::Instant::now();
        while !status().is_empty() && start.elapsed().as_secs() < 600 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let mut notices = Vec::new();
        while let Some(n) = take_notice() {
            notices.push(n);
        }
        for name in &names {
            assert!(is_installed(name), "{name}: {notices:?}");
            // Grammars: their highlights compile against them.
            if let Some(lang) = name.strip_prefix("tree-sitter-") {
                let (language, query) = load_grammar(lang).unwrap();
                tree_sitter::Query::new(&language, &query).unwrap_or_else(|e| panic!("{lang} highlights: {e:?}"));
            }
            // Servers: each command's script runs its program (126/127: it couldn't).
            if let Some(server) = server_named(name) {
                for c in server.commands {
                    let status = Command::new(dir.join("bin").join(c))
                        .arg("--version")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .unwrap_or_else(|e| panic!("{c}: {e}"));
                    assert!(!matches!(status.code(), Some(126 | 127)), "{c} didn't run: {status}");
                }
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
