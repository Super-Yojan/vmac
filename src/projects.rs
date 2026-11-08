//! Projects and recent files: what the dashboard, SPC p p and SPC f r offer.
//!
//! A project is a folder with a `.git` in it. vmac remembers the projects and
//! files you open, most recent first, in its state folder
//! (`~/.local/state/vmac`: `projects`, `recent-files`, one path a line), and
//! finds projects you haven't opened yet in the usual places (`~/git`,
//! `~/code`, `~/projects`, `~/src`, `~/dev`, `~/work`).

use std::path::{Path, PathBuf};

const MAX_FILES: usize = 100;
const MAX_PROJECTS: usize = 50;

/// The remembered lists, kept in `dir` (None: in memory only, for tests).
#[derive(Debug, Default)]
pub struct History {
    dir: Option<PathBuf>,
    files: Vec<PathBuf>,
    projects: Vec<PathBuf>,
}

impl History {
    /// Reads the lists from `dir`.
    pub fn load(dir: Option<PathBuf>) -> Self {
        let read = |name: &str| -> Vec<PathBuf> {
            dir.as_ref()
                .and_then(|d| std::fs::read_to_string(d.join(name)).ok())
                .map(|s| s.lines().filter(|l| !l.trim().is_empty()).map(PathBuf::from).collect())
                .unwrap_or_default()
        };
        Self { files: read("recent-files"), projects: read("projects"), dir }
    }

    /// The default place: $XDG_STATE_HOME/vmac, else ~/.local/state/vmac.
    pub fn default_dir() -> Option<PathBuf> {
        let base = match std::env::var_os("XDG_STATE_HOME") {
            Some(d) => PathBuf::from(d),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
        };
        Some(base.join("vmac"))
    }

    /// Whether the lists are kept on disk (the app; not tests).
    pub fn is_saved(&self) -> bool {
        self.dir.is_some()
    }

    /// Recent files that still exist.
    pub fn files(&self) -> Vec<PathBuf> {
        self.files.iter().filter(|p| p.is_file()).cloned().collect()
    }

    /// Projects opened, most recent first, that still exist.
    pub fn projects(&self) -> Vec<PathBuf> {
        self.projects.iter().filter(|p| p.is_dir()).cloned().collect()
    }

    /// Remembers a file opened, and its project if it's in one.
    pub fn opened_file(&mut self, path: &Path) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        push_front(&mut self.files, path.clone(), MAX_FILES);
        let root = crate::picker::project_root(&path);
        if root.join(".git").exists() {
            push_front(&mut self.projects, root, MAX_PROJECTS);
        }
        self.save();
    }

    pub fn opened_project(&mut self, root: &Path) {
        push_front(&mut self.projects, root.to_path_buf(), MAX_PROJECTS);
        self.save();
    }

    pub fn forget_project(&mut self, root: &Path) {
        self.projects.retain(|p| p != root);
        self.save();
    }

    fn save(&self) {
        let Some(dir) = &self.dir else { return };
        let _ = std::fs::create_dir_all(dir);
        let write = |name: &str, list: &[PathBuf]| {
            let text: String = list.iter().map(|p| format!("{}\n", p.display())).collect();
            let _ = std::fs::write(dir.join(name), text);
        };
        write("recent-files", &self.files);
        write("projects", &self.projects);
    }
}

fn push_front(list: &mut Vec<PathBuf>, item: PathBuf, max: usize) {
    list.retain(|p| *p != item);
    list.insert(0, item);
    list.truncate(max);
}

/// Where projects usually live, those that exist.
pub fn search_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
    ["git", "code", "projects", "src", "dev", "work", "Developer"]
        .iter()
        .map(|d| home.join(d))
        .filter(|d| d.is_dir())
        .collect()
}

/// Git repositories in `dirs`, and one level further down in folders that
/// aren't repositories themselves (~/git/org/repo).
pub fn discover(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let children = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_dir() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    };
    let mut found = Vec::new();
    for dir in dirs {
        for child in children(dir) {
            if child.join(".git").exists() {
                found.push(child);
            } else {
                found.extend(children(&child).into_iter().filter(|g| g.join(".git").exists()));
            }
        }
    }
    found
}

/// "~/git/vmac" for a path in the home folder.
pub fn abbreviate(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_ref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// "~/git/vmac" back to a full path.
pub fn expand(path: &str) -> PathBuf {
    let path = path.trim();
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest.trim_start_matches('/')),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_recent_first_and_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("vmac-history-{}", std::process::id()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let (a, b) = (repo.join("a.rs"), repo.join("src/b.rs"));
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        let loose = dir.join("loose.txt");
        std::fs::write(&loose, "").unwrap();

        let state = dir.join("state");
        let mut h = History::load(Some(state.clone()));
        h.opened_file(&a);
        h.opened_file(&b);
        h.opened_file(&loose);
        h.opened_file(&a);
        assert_eq!(h.files(), vec![a.clone(), loose.clone(), b.clone()]);
        assert_eq!(h.projects(), vec![repo.clone()], "loose.txt is in no project");

        let again = History::load(Some(state));
        assert_eq!(again.files(), vec![a, loose.clone(), b]);
        assert_eq!(again.projects(), vec![repo.clone()]);
        std::fs::remove_file(&loose).unwrap();
        assert!(!again.files().contains(&loose), "gone files aren't offered");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn projects_are_found_one_and_two_levels_down() {
        let dir = std::env::temp_dir().join(format!("vmac-discover-{}", std::process::id()));
        for p in ["top/.git", "org/inner/.git", "org/plain", ".hidden/.git"] {
            std::fs::create_dir_all(dir.join(p)).unwrap();
        }
        assert_eq!(discover(std::slice::from_ref(&dir)), vec![dir.join("org/inner"), dir.join("top")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn home_paths_abbreviate_and_expand() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(abbreviate(&home.join("git/vmac")), "~/git/vmac");
        assert_eq!(expand("~/git/vmac"), home.join("git/vmac"));
        assert_eq!(expand("/tmp/x"), PathBuf::from("/tmp/x"));
    }
}
