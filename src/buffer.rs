//! A text buffer: a rope plus a point (the cursor), in characters.
//!
//! This is the Rust core's job: fast text storage and movement. What a key
//! *does* is decided in Scheme; these are the primitives it calls.

use ropey::Rope;
use std::path::PathBuf;

/// A saved state for undo: the text (ropes share structure, so this is cheap),
/// the point, and the edit count it had.
#[derive(Debug, Clone)]
struct Snapshot {
    text: Rope,
    point: usize,
    edit: u64,
}

#[derive(Debug, Clone)]
pub struct Buffer {
    name: String,
    text: Rope,
    point: usize,
    /// Column the cursor wants when moving up/down through shorter lines.
    goal_column: Option<usize>,
    file: Option<PathBuf>,
    modified: bool,
    /// Edits are refused (the file tree, help buffers). `set_text` still works.
    read_only: bool,
    /// Faces on ranges of the text, set from Scheme (like Emacs text
    /// properties): `(start, end, face)` in characters.
    faces: Vec<(usize, usize, String)>,
    /// Bumped on every change; tells undo snapshots apart.
    edit: u64,
    /// Every edit count ever used, so a new edit after an undo never reuses
    /// a snapshot's number.
    edits_made: u64,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Changes whenever the text does (edits, undo, redo): what syntax
    /// highlighting checks before parsing again.
    revision: u64,
    /// How many times it was saved (language servers hear about each save).
    saves: u64,
}

impl Buffer {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: Rope::new(),
            point: 0,
            goal_column: None,
            file: None,
            modified: false,
            read_only: false,
            faces: Vec::new(),
            edit: 0,
            edits_made: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            revision: 0,
            saves: 0,
        }
    }

    /// Opens a file. The path is stored absolute, so the buffer still knows
    /// its file (and project) whatever the working directory.
    pub fn from_file(path: PathBuf) -> std::io::Result<Self> {
        let path = std::path::absolute(&path).unwrap_or(path);
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => Rope::from_str(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Rope::new(),
            Err(e) => return Err(e),
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Ok(Self {
            name,
            text,
            point: 0,
            goal_column: None,
            file: Some(path),
            modified: false,
            read_only: false,
            faces: Vec::new(),
            edit: 0,
            edits_made: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            revision: 0,
            saves: 0,
        })
    }

    pub fn save(&mut self) -> std::io::Result<PathBuf> {
        let path = self
            .file
            .clone()
            .ok_or_else(|| std::io::Error::other("buffer has no file"))?;
        std::fs::write(&path, self.text.to_string())?;
        self.modified = false;
        self.saves += 1;
        Ok(path)
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn file(&self) -> Option<&PathBuf> {
        self.file.as_ref()
    }
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }
    pub fn add_face(&mut self, start: usize, end: usize, face: String) {
        self.faces.push((start, end, face));
    }
    pub fn faces(&self) -> &[(usize, usize, String)] {
        &self.faces
    }

    pub fn saves(&self) -> u64 {
        self.saves
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn is_modified(&self) -> bool {
        self.modified
    }
    pub fn text(&self) -> &Rope {
        &self.text
    }
    pub fn point(&self) -> usize {
        self.point
    }
    pub fn len(&self) -> usize {
        self.text.len_chars()
    }

    /// Zero-based line of the point.
    pub fn line(&self) -> usize {
        self.text.char_to_line(self.point)
    }

    /// Zero-based column of the point, in characters.
    pub fn column(&self) -> usize {
        self.point - self.text.line_to_char(self.line())
    }

    /// The character at `pos` as a string, or "" past the end.
    pub fn char_at(&self, pos: usize) -> String {
        if pos < self.len() { self.text.char(pos).to_string() } else { String::new() }
    }

    /// What kind of character is at `pos`: "word" (letters, digits, _),
    /// "space", "newline", "punct", or "eof" past the end.
    pub fn char_class(&self, pos: usize) -> &'static str {
        if pos >= self.len() {
            return "eof";
        }
        match self.text.char(pos) {
            '\n' => "newline",
            c if c.is_alphanumeric() || c == '_' => "word",
            c if c.is_whitespace() => "space",
            _ => "punct",
        }
    }

    /// Start of the first match at or after `from`, in characters.
    pub fn search_forward(&self, re: &regex::Regex, from: usize) -> Option<usize> {
        let text = self.text.to_string();
        let byte = self.text.char_to_byte(from.min(self.len()));
        re.find_at(&text, byte).map(|m| self.text.byte_to_char(m.start()))
    }

    /// Start of the last match that starts before `before`, in characters.
    pub fn search_backward(&self, re: &regex::Regex, before: usize) -> Option<usize> {
        let text = self.text.to_string();
        let limit = self.text.char_to_byte(before.min(self.len()));
        let mut last = None;
        let mut at = 0;
        while let Some(m) = re.find_at(&text, at) {
            if m.start() >= limit {
                break;
            }
            last = Some(m.start());
            // Step past this match start (by a whole character) to find later ones.
            at = m.start() + text[m.start()..].chars().next().map_or(1, char::len_utf8);
        }
        last.map(|b| self.text.byte_to_char(b))
    }

    pub fn line_of(&self, pos: usize) -> usize {
        self.text.char_to_line(pos.min(self.len()))
    }

    /// Start of a line, clamped to the last one.
    pub fn line_to_pos(&self, line: usize) -> usize {
        self.text.line_to_char(line.min(self.line_count() - 1))
    }

    pub fn line_count(&self) -> usize {
        self.text.len_lines()
    }

    pub fn substring(&self, start: usize, end: usize) -> String {
        let (a, b) = (start.min(self.len()), end.min(self.len()));
        self.text.slice(a.min(b)..b.max(a)).to_string()
    }

    /// Start of the line holding `pos`.
    pub fn line_start(&self, pos: usize) -> usize {
        self.text.line_to_char(self.text.char_to_line(pos.min(self.len())))
    }

    /// End of the line holding `pos`, before its line break.
    pub fn line_end(&self, pos: usize) -> usize {
        let line = self.text.char_to_line(pos.min(self.len()));
        self.text.line_to_char(line) + self.line_len(line)
    }

    /// Removes `start..end` and returns it; the point moves to `start` when it
    /// was inside or after the range.
    pub fn delete_range(&mut self, start: usize, end: usize) -> String {
        if self.read_only {
            return String::new();
        }
        let (a, b) = (start.min(self.len()), end.min(self.len()));
        let (a, b) = (a.min(b), a.max(b));
        let removed = self.text.slice(a..b).to_string();
        if a < b {
            self.text.remove(a..b);
            self.point = if self.point >= b { self.point - (b - a) } else { self.point.min(a) };
            self.touch();
        }
        self.goal_column = None;
        removed
    }

    fn touch(&mut self) {
        self.revision += 1;
        self.modified = true;
        self.edits_made += 1;
        self.edit = self.edits_made;
        self.redo.clear();
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { text: self.text.clone(), point: self.point, edit: self.edit }
    }

    fn restore(&mut self, snap: Snapshot) {
        self.revision += 1;
        self.text = snap.text;
        self.point = snap.point.min(self.text.len_chars());
        self.edit = snap.edit;
        self.goal_column = None;
        self.modified = true;
    }

    /// Marks an undo step: the state now is what `undo` goes back to.
    /// Calling it again without edits in between does nothing.
    pub fn undo_boundary(&mut self) {
        if self.undo.last().map(|s| s.edit) != Some(self.edit) {
            self.undo.push(self.snapshot());
        }
    }

    /// Goes back to the previous boundary. Returns false with nothing to undo.
    pub fn undo(&mut self) -> bool {
        while self.undo.last().is_some_and(|s| s.edit == self.edit) {
            self.undo.pop();
        }
        let Some(prev) = self.undo.pop() else { return false };
        self.redo.push(self.snapshot());
        self.restore(prev);
        self.undo_boundary();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        self.undo_boundary();
        let redo = std::mem::take(&mut self.redo);
        self.restore(next);
        self.redo = redo;
        true
    }

    /// Replaces the whole text, dropping its faces.
    pub fn set_text(&mut self, s: &str) {
        self.faces.clear();
        self.revision += 1;
        self.text = Rope::from_str(s);
        self.point = self.point.min(self.len());
        self.goal_column = None;
    }

    pub fn goto_char(&mut self, pos: usize) {
        self.point = pos.min(self.len());
        self.goal_column = None;
    }

    pub fn insert(&mut self, s: &str) {
        if self.read_only {
            return;
        }
        self.text.insert(self.point, s);
        self.point += s.chars().count();
        self.goal_column = None;
        self.touch();
    }

    pub fn delete_backward(&mut self, n: usize) {
        if self.read_only {
            return;
        }
        let start = self.point.saturating_sub(n);
        if start < self.point {
            self.text.remove(start..self.point);
            self.point = start;
            self.touch();
        }
        self.goal_column = None;
    }

    pub fn delete_forward(&mut self, n: usize) {
        if self.read_only {
            return;
        }
        let end = (self.point + n).min(self.len());
        if self.point < end {
            self.text.remove(self.point..end);
            self.touch();
        }
        self.goal_column = None;
    }

    pub fn forward_char(&mut self, n: isize) {
        let p = self.point as isize + n;
        self.goto_char(p.clamp(0, self.len() as isize) as usize);
    }

    /// Length of a line without its line break.
    fn line_len(&self, line: usize) -> usize {
        let l = self.text.line(line);
        let mut n = l.len_chars();
        while n > 0 && matches!(l.char(n - 1), '\n' | '\r') {
            n -= 1;
        }
        n
    }

    pub fn forward_line(&mut self, n: isize) {
        let goal = self.goal_column.unwrap_or_else(|| self.column());
        let last = self.text.len_lines().saturating_sub(1) as isize;
        let target = (self.line() as isize + n).clamp(0, last) as usize;
        let col = goal.min(self.line_len(target));
        self.point = self.text.line_to_char(target) + col;
        self.goal_column = Some(goal);
    }

    pub fn beginning_of_line(&mut self) {
        self.goto_char(self.text.line_to_char(self.line()));
    }

    pub fn end_of_line(&mut self) {
        let line = self.line();
        self.goto_char(self.text.line_to_char(line) + self.line_len(line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_move() {
        let mut b = Buffer::new("t");
        b.insert("hello\nworld");
        assert_eq!(b.line(), 1);
        assert_eq!(b.column(), 5);
        b.forward_line(-1);
        assert_eq!((b.line(), b.column()), (0, 5));
        b.beginning_of_line();
        assert_eq!(b.point(), 0);
        b.end_of_line();
        assert_eq!(b.column(), 5);
    }

    #[test]
    fn goal_column_survives_short_lines() {
        let mut b = Buffer::new("t");
        b.insert("abcdef\nab\nabcdef");
        b.forward_line(-1);
        assert_eq!(b.column(), 2);
        b.forward_line(-1);
        assert_eq!(b.column(), 6);
    }

    #[test]
    fn files_are_stored_with_absolute_paths() {
        let b = Buffer::from_file(PathBuf::from("Cargo.toml")).unwrap();
        assert!(b.file().unwrap().is_absolute());
        assert_eq!(b.name(), "Cargo.toml");
    }

    #[test]
    fn read_only_refuses_edits_but_set_text_works() {
        let mut b = Buffer::new("t");
        b.set_read_only(true);
        b.insert("x");
        b.delete_backward(1);
        assert_eq!(b.text().to_string(), "");
        b.set_text("tree");
        b.add_face(0, 4, "blue".into());
        assert_eq!(b.text().to_string(), "tree");
        assert_eq!(b.faces().len(), 1);
        b.set_text("again");
        assert!(b.faces().is_empty(), "new text, no old faces");
    }

    #[test]
    fn undo_and_redo_go_between_boundaries() {
        let mut b = Buffer::new("t");
        b.undo_boundary();
        b.insert("one");
        b.undo_boundary();
        b.insert(" two");
        assert!(b.undo());
        assert_eq!(b.text().to_string(), "one");
        assert!(b.undo());
        assert_eq!(b.text().to_string(), "");
        assert!(!b.undo());
        assert!(b.redo());
        assert_eq!(b.text().to_string(), "one");
        assert!(b.redo());
        assert_eq!(b.text().to_string(), "one two");
    }

    #[test]
    fn delete_range_returns_text_and_moves_point() {
        let mut b = Buffer::new("t");
        b.insert("abc\ndef");
        assert_eq!(b.delete_range(0, 4), "abc\n");
        assert_eq!(b.text().to_string(), "def");
        assert_eq!(b.point(), 3, "point was at the end, 8; it moves back by 4");
    }
}
