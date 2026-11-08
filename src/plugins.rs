//! Plugins: Scheme and Lua that load after the core and before the user's
//! init files, from `~/.config/vmac/plugins/`.
//!
//! A plugin is a folder with an `init.scm` and/or `init.lua` (a Lua plugin
//! can `require` modules next to it or in its `lua/`), or a single
//! `name.scm` / `name.lua` file. A folder holding a file named `disabled` is
//! skipped. Plugins installed with git (`:plugin-install URL`) are folders
//! that `:plugin-update` pulls.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    pub name: String,
    /// The plugin's folder, or its single file.
    pub path: PathBuf,
    pub scheme: Option<PathBuf>,
    pub lua: Option<PathBuf>,
    pub disabled: bool,
}

impl Plugin {
    /// The folder its Lua `require`s from: its own folder (or the file's).
    pub fn lua_root(&self) -> PathBuf {
        if self.path.is_dir() {
            self.path.clone()
        } else {
            self.path.parent().map(Path::to_path_buf).unwrap_or_default()
        }
    }

    pub fn languages(&self) -> String {
        match (&self.scheme, &self.lua) {
            (Some(_), Some(_)) => "scheme + lua".into(),
            (Some(_), None) => "scheme".into(),
            (None, Some(_)) => "lua".into(),
            (None, None) => "empty".into(),
        }
    }
}

/// How a plugin's last load went.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Loaded,
    Disabled,
    Failed(String),
}

/// The plugins in `dir`, by name. Folders without an init file are left out.
pub fn discover(dir: &Path) -> Vec<Plugin> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut plugins: Vec<Plugin> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let file_name = path.file_name()?.to_string_lossy().into_owned();
            if file_name.starts_with('.') {
                return None;
            }
            if path.is_dir() {
                let scheme = Some(path.join("init.scm")).filter(|p| p.is_file());
                let lua = Some(path.join("init.lua")).filter(|p| p.is_file());
                if scheme.is_none() && lua.is_none() {
                    return None;
                }
                let disabled = path.join("disabled").exists();
                return Some(Plugin { name: file_name, path, scheme, lua, disabled });
            }
            let ext = path.extension()?.to_str()?;
            let name = path.file_stem()?.to_string_lossy().into_owned();
            let (scheme, lua) = match ext {
                "scm" => (Some(path.clone()), None),
                "lua" => (None, Some(path.clone())),
                _ => return None,
            };
            Some(Plugin { name, path, scheme, lua, disabled: false })
        })
        .collect();
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    plugins
}

/// Every plugin file and when it changed: a reload follows any difference.
pub fn signature(dir: &Path) -> Vec<(PathBuf, Option<SystemTime>)> {
    let mut out = Vec::new();
    let modified = |p: &Path| p.metadata().ok().and_then(|m| m.modified().ok());
    for plugin in discover(dir) {
        out.push((plugin.path.join("disabled"), modified(&plugin.path.join("disabled"))));
        let root = plugin.lua_root();
        let files: Vec<PathBuf> = if plugin.path.is_dir() {
            ignore::WalkBuilder::new(&root)
                .max_depth(Some(3))
                .build()
                .filter_map(Result::ok)
                .map(|e| e.into_path())
                .filter(|p| p.extension().is_some_and(|e| e == "scm" || e == "lua"))
                .collect()
        } else {
            vec![plugin.path.clone()]
        };
        out.extend(files.into_iter().map(|f| {
            let m = modified(&f);
            (f, m)
        }));
    }
    // A plugin added or removed changes the folder itself.
    out.push((dir.to_path_buf(), modified(dir)));
    out.sort();
    out
}

/// A plugin's name from its git URL: `https://x/y/vmac-zen.git` -> `vmac-zen`.
pub fn name_from_url(url: &str) -> Option<String> {
    let last = url.trim_end_matches('/').rsplit(['/', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    (!name.is_empty()).then(|| name.to_string())
}

/// Turns a plugin folder on or off (the `disabled` file). Single-file
/// plugins can't be disabled this way.
pub fn set_disabled(plugin: &Plugin, disabled: bool) -> std::io::Result<()> {
    if !plugin.path.is_dir() {
        return Err(std::io::Error::other("a single-file plugin; move it into a folder to disable it"));
    }
    let marker = plugin.path.join("disabled");
    if disabled { std::fs::write(marker, "") } else { std::fs::remove_file(marker).or(Ok(())) }
}

/// Runs git, returning its last line of output on failure.
fn git(args: &[&str], cwd: &Path) -> Result<(), String> {
    let out = Command::new("git").args(args).current_dir(cwd).output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("git failed").to_string())
    }
}

/// Clones a plugin into `dir` (blocking; call it off the UI thread).
pub fn install(dir: &Path, url: &str) -> Result<String, String> {
    let name = name_from_url(url).ok_or_else(|| format!("no plugin name in {url}"))?;
    if dir.join(&name).exists() {
        return Err(format!("{name} is already installed"));
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    git(&["clone", "--depth", "1", url, &name], dir)?;
    Ok(name)
}

/// Pulls every plugin installed with git. Returns what happened to each.
pub fn update_all(dir: &Path) -> Vec<(String, Result<(), String>)> {
    discover(dir)
        .into_iter()
        .filter(|p| p.path.join(".git").exists())
        .map(|p| {
            let result = git(&["pull", "--ff-only", "--quiet"], &p.path);
            (p.name, result)
        })
        .collect()
}

pub fn remove(dir: &Path, name: &str) -> Result<(), String> {
    let plugin = discover(dir).into_iter().find(|p| p.name == name).ok_or_else(|| format!("no plugin named {name}"))?;
    let gone = if plugin.path.is_dir() { std::fs::remove_dir_all(&plugin.path) } else { std::fs::remove_file(&plugin.path) };
    gone.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugins_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vmac-plugins-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_folders_and_single_files() {
        let dir = plugins_dir("discover");
        std::fs::create_dir_all(dir.join("alpha")).unwrap();
        std::fs::write(dir.join("alpha/init.scm"), "").unwrap();
        std::fs::create_dir_all(dir.join("beta")).unwrap();
        std::fs::write(dir.join("beta/init.lua"), "").unwrap();
        std::fs::write(dir.join("beta/init.scm"), "").unwrap();
        std::fs::write(dir.join("gamma.lua"), "").unwrap();
        std::fs::create_dir_all(dir.join("off")).unwrap();
        std::fs::write(dir.join("off/init.lua"), "").unwrap();
        std::fs::write(dir.join("off/disabled"), "").unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        let found = discover(&dir);
        let names: Vec<_> = found.iter().map(|p| (p.name.as_str(), p.languages(), p.disabled)).collect();
        assert_eq!(
            names,
            vec![
                ("alpha", "scheme".into(), false),
                ("beta", "scheme + lua".into(), false),
                ("gamma", "lua".into(), false),
                ("off", "lua".into(), true),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn names_from_urls() {
        assert_eq!(name_from_url("https://github.com/someone/vmac-zen.git").as_deref(), Some("vmac-zen"));
        assert_eq!(name_from_url("git@github.com:someone/tool").as_deref(), Some("tool"));
        assert_eq!(name_from_url("file:///tmp/x/plug/").as_deref(), Some("plug"));
    }

    #[test]
    fn installs_updates_and_removes_with_git() {
        let dir = plugins_dir("git");
        // A plugin repository to install from.
        let repo = dir.with_extension("repo").join("hello");
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("init.lua"), "-- v1").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec!["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "v1"],
        ] {
            git(&args, &repo).unwrap();
        }
        let url = format!("file://{}", repo.display());
        assert_eq!(install(&dir, &url), Ok("hello".to_string()));
        assert!(dir.join("hello/init.lua").exists());
        assert!(install(&dir, &url).unwrap_err().contains("already installed"));

        std::fs::write(repo.join("init.lua"), "-- v2").unwrap();
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qam", "v2"], &repo).unwrap();
        let updates = update_all(&dir);
        assert_eq!(updates, vec![("hello".to_string(), Ok(()))]);
        assert_eq!(std::fs::read_to_string(dir.join("hello/init.lua")).unwrap(), "-- v2");

        remove(&dir, "hello").unwrap();
        assert!(!dir.join("hello").exists());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }
}
