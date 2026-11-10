//! A fuzzy picker (telescope-style): a list of items filtered by a query.
//!
//! Lua decides what to pick from and what picking does (`vmac.picker_open`,
//! then its own callback). Rust does the hot path: walking the project,
//! scoring thousands of items on every keystroke, and the file preview.

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::path::{Path, PathBuf};

/// Most items a project listing returns, so a huge tree can't stall the editor.
const MAX_FILES: usize = 50_000;
/// Lines the preview shows.
pub const PREVIEW_LINES: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub item: String,
    /// Character indices of the query's letters in `item`, for highlighting.
    pub positions: Vec<usize>,
}

#[derive(Debug)]
pub struct Picker {
    title: String,
    items: Vec<String>,
    matches: Vec<Match>,
    selected: usize,
    query: Option<String>,
    /// When set, items are paths relative to it and the selected one is previewed.
    preview_root: Option<PathBuf>,
}

impl Picker {
    /// Everything it lists, before filtering.
    #[cfg(test)]
    pub fn items(&self) -> &[String] {
        &self.items
    }

    pub fn new(title: String, items: Vec<String>, preview_root: Option<PathBuf>) -> Self {
        let mut picker =
            Self { title, items, matches: Vec::new(), selected: 0, query: None, preview_root };
        picker.filter("");
        picker
    }

    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn matches(&self) -> &[Match] {
        &self.matches
    }
    pub fn selected(&self) -> usize {
        self.selected
    }
    pub fn total(&self) -> usize {
        self.items.len()
    }

    /// Re-scores every item against the query, best first. An empty query
    /// keeps the original order. The same query again changes nothing, so the
    /// selection survives keys that don't edit the query.
    pub fn filter(&mut self, query: &str) {
        if self.query.as_deref() == Some(query) {
            return;
        }
        self.query = Some(query.to_string());
        self.selected = 0;
        if query.is_empty() {
            self.matches =
                self.items.iter().map(|i| Match { item: i.clone(), positions: vec![] }).collect();
            return;
        }
        let matcher = SkimMatcherV2::default().smart_case();
        let mut scored: Vec<(i64, Match)> = self
            .items
            .iter()
            .filter_map(|item| {
                let (score, positions) = matcher.fuzzy_indices(item, query)?;
                Some((score, Match { item: item.clone(), positions }))
            })
            .collect();
        // Best score first; among equals, shorter paths first.
        scored.sort_by(|(a, ma), (b, mb)| b.cmp(a).then(ma.item.len().cmp(&mb.item.len())));
        self.matches = scored.into_iter().map(|(_, m)| m).collect();
    }

    /// Moves the selection by `n`, wrapping around.
    pub fn move_selection(&mut self, n: isize) {
        let len = self.matches.len() as isize;
        if len > 0 {
            self.selected = (self.selected as isize + n).rem_euclid(len) as usize;
        }
    }

    /// Selects a result by index, clamped; negative counts from the end.
    pub fn select(&mut self, index: isize) {
        let len = self.matches.len() as isize;
        if len > 0 {
            let i = if index < 0 { len + index } else { index };
            self.selected = i.clamp(0, len - 1) as usize;
        }
    }

    pub fn selection(&self) -> Option<&str> {
        self.matches.get(self.selected).map(|m| m.item.as_str())
    }

    /// The selected file's lines, when this picker previews files, and which
    /// of them to mark. An item like `path:12:5  code` previews around line
    /// 12 and marks it.
    pub fn preview(&self) -> Option<(Vec<String>, Option<usize>)> {
        let item = self.selection()?;
        let (file, line) = match parse_location(item) {
            Some((path, line, _)) => (path, Some(line)),
            None => (item.to_string(), None),
        };
        let path = self.preview_root.as_ref()?.join(file);
        let bytes = std::fs::read(path).ok()?;
        if bytes.iter().take(8000).any(|&b| b == 0) {
            return Some((vec!["(binary file)".into()], None));
        }
        let text = String::from_utf8_lossy(&bytes);
        let first = line.map_or(0, |l| l.saturating_sub(6));
        let lines = text.lines().skip(first).take(PREVIEW_LINES).map(|l| l.replace('\t', "    ")).collect();
        Some((lines, line.map(|l| l - 1 - first)))
    }
}

/// `path:line:col` at the start of an item (`src/a.rs:12:5  code`).
pub fn parse_location(item: &str) -> Option<(String, usize, usize)> {
    let head = item.split("  ").next()?;
    let mut parts = head.rsplitn(3, ':');
    let col = parts.next()?.parse().ok()?;
    let line = parts.next()?.parse().ok()?;
    Some((parts.next()?.to_string(), line, col))
}

/// The project a file belongs to: the nearest folder above it with `.git`,
/// else its own folder.
pub fn project_root(start: &Path) -> PathBuf {
    let start = std::path::absolute(start).unwrap_or_else(|_| start.to_path_buf());
    let start = start.as_path();
    let dir = if start.is_dir() { start } else { start.parent().unwrap_or(start) };
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(dir)
        .to_path_buf()
}

/// Files under `root`, relative to it, skipping what .gitignore ignores and
/// hidden folders, sorted.
pub fn project_files(root: &Path) -> Vec<String> {
    let mut files: Vec<String> = ignore::WalkBuilder::new(root)
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| e.path().strip_prefix(root).ok().map(|p| p.to_string_lossy().into_owned()))
        .take(MAX_FILES)
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fuzzy_filter_ranks_and_reports_positions() {
        let mut p = Picker::new(
            "t".into(),
            items(&["src/shell.rs", "src/lisp.rs", "lisp/core.scm", "README.md"]),
            None,
        );
        assert_eq!(p.matches().len(), 4, "empty query keeps everything");
        p.filter("lsp");
        let names: Vec<_> = p.matches().iter().map(|m| m.item.as_str()).collect();
        assert!(names.contains(&"src/lisp.rs") && names.contains(&"lisp/core.scm"), "{names:?}");
        assert!(!names.contains(&"README.md"));
        let m = &p.matches()[0];
        assert_eq!(m.positions.len(), 3);
    }

    #[test]
    fn select_clamps_and_counts_from_the_end() {
        let mut p = Picker::new("t".into(), items(&["a", "b", "c"]), None);
        p.select(-1);
        assert_eq!(p.selection(), Some("c"));
        p.select(10);
        assert_eq!(p.selection(), Some("c"));
        p.select(0);
        assert_eq!(p.selection(), Some("a"));
    }

    #[test]
    fn locations_parse_from_items() {
        assert_eq!(parse_location("src/a.rs:12:5  let x"), Some(("src/a.rs".into(), 12, 5)));
        assert_eq!(parse_location("/abs/b.rs:1:1"), Some(("/abs/b.rs".into(), 1, 1)));
        assert_eq!(parse_location("README.md"), None);
    }

    #[test]
    fn selection_wraps() {
        let mut p = Picker::new("t".into(), items(&["a", "b", "c"]), None);
        p.move_selection(-1);
        assert_eq!(p.selection(), Some("c"));
        p.move_selection(2);
        assert_eq!(p.selection(), Some("b"));
        p.filter("zzz");
        assert_eq!(p.selection(), None);
        p.move_selection(1);
    }

    #[test]
    fn lists_this_project_without_ignored_files() {
        let root = project_root(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs").as_path());
        assert_eq!(root, PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let files = project_files(&root);
        assert!(files.contains(&"src/main.rs".to_string()), "{files:?}");
        assert!(files.contains(&"lua/core/80-looks.lua".to_string()));
        assert!(!files.iter().any(|f| f.starts_with("target/")), "target is gitignored");
    }

    #[test]
    fn a_relative_start_finds_the_project() {
        // Tests run in the crate's folder, like launching `vmac src/main.rs` there.
        let root = project_root(Path::new("src/main.rs"));
        assert_eq!(root, PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        assert!(!project_files(&root).is_empty());
    }

    #[test]
    fn previews_the_selected_file() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let p = Picker::new("t".into(), items(&["Cargo.toml"]), Some(root));
        let (lines, mark) = p.preview().unwrap();
        assert_eq!(lines[0], "[package]");
        assert_eq!(mark, None);
        let p = Picker::new("t".into(), items(&["Cargo.toml:8:1  [dependencies]"]), Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))));
        let (lines, mark) = p.preview().unwrap();
        assert_eq!(mark, Some(5), "line 8, shown from line 3");
        assert_eq!(lines.len().min(1), 1);
    }
}
