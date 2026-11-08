//! Reviewing what the agent changed. Each file it writes keeps the text it
//! had before (the base); the hunks are the differences between the base and
//! the buffer now. Accepting a hunk folds it into the base (it stops showing);
//! rejecting one puts the base's lines back in the buffer. A file is reviewed
//! when no hunks are left.

use similar::TextDiff;
use std::ops::Range;
use std::path::PathBuf;

/// One run of changed lines: lines `old` of the base became lines `new` of
/// the buffer (either may be empty: a pure insertion or deletion).
#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    pub old: Range<usize>,
    pub new: Range<usize>,
    pub old_text: String,
    pub new_text: String,
}

impl Hunk {
    /// Whether the cursor on `line` of the buffer is on this hunk (a
    /// deletion sits on the line after it).
    pub fn covers(&self, line: usize) -> bool {
        if self.new.is_empty() { line == self.new.start } else { self.new.contains(&line) }
    }
}

/// The hunks between two texts, line by line.
pub fn hunks(base: &str, current: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(base, current);
    let (old, new) = (diff.old_slices(), diff.new_slices());
    // Runs of changes between equal stretches, each one hunk.
    let mut out: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    let mut open: Option<(Range<usize>, Range<usize>)> = None;
    for op in diff.ops() {
        if matches!(op, similar::DiffOp::Equal { .. }) {
            out.extend(open.take());
            continue;
        }
        let (o, n) = (op.old_range(), op.new_range());
        open = Some(match open.take() {
            Some((po, pn)) => (po.start.min(o.start)..po.end.max(o.end), pn.start.min(n.start)..pn.end.max(n.end)),
            None => (o, n),
        });
    }
    out.extend(open);
    out.into_iter()
        .filter(|(o, n)| !(o.is_empty() && n.is_empty()))
        .map(|(o, n)| Hunk { old_text: old[o.clone()].concat(), new_text: new[n.clone()].concat(), old: o, new: n })
        .collect()
}

/// A file the agent changed, and its text before.
#[derive(Clone, Debug)]
pub struct Review {
    pub path: PathBuf,
    pub base: String,
    /// Hunks for a buffer revision, so drawing doesn't diff every frame.
    cache: Option<(u64, Vec<Hunk>)>,
}

impl Review {
    pub fn new(path: PathBuf, base: String) -> Self {
        Self { path, base, cache: None }
    }

    /// The hunks against the buffer's text at `revision`.
    pub fn hunks(&mut self, current: &str, revision: u64) -> &[Hunk] {
        if self.cache.as_ref().is_none_or(|(r, _)| *r != revision) {
            self.cache = Some((revision, hunks(&self.base, current)));
        }
        &self.cache.as_ref().expect("filled above").1
    }

    /// Takes a hunk into the base: the base gets the buffer's lines there.
    pub fn accept(&mut self, hunk: &Hunk) {
        let lines: Vec<&str> = self.base.split_inclusive('\n').collect();
        let end = hunk.old.end.min(lines.len());
        let start = hunk.old.start.min(end);
        self.base = format!("{}{}{}", lines[..start].concat(), hunk.new_text, lines[end..].concat());
        self.cache = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_cover_changes_insertions_and_deletions() {
        let base = "a\nb\nc\nd\n";
        let now = "a\nB\nc\nnew\nd\n";
        let h = hunks(base, now);
        assert_eq!(h.len(), 2, "{h:?}");
        assert_eq!((h[0].old.clone(), h[0].new.clone()), (1..2, 1..2));
        assert_eq!((h[0].old_text.as_str(), h[0].new_text.as_str()), ("b\n", "B\n"));
        assert_eq!((h[1].old.clone(), h[1].new.clone()), (3..3, 3..4));
        assert!(h[1].covers(3) && !h[1].covers(4));

        let gone = hunks("a\nb\nc\n", "a\nc\n");
        assert_eq!((gone[0].old.clone(), gone[0].new.clone()), (1..2, 1..1));
        assert!(gone[0].covers(1), "a deletion sits on the line after it");
        assert!(hunks("same\n", "same\n").is_empty());
    }

    #[test]
    fn accepting_folds_a_hunk_into_the_base() {
        let mut r = Review::new("f".into(), "a\nb\nc\nd\n".into());
        let now = "a\nB\nc\nnew\nd\n";
        let first = r.hunks(now, 1)[0].clone();
        r.accept(&first);
        assert_eq!(r.base, "a\nB\nc\nd\n");
        let left = r.hunks(now, 1).to_vec();
        assert_eq!(left.len(), 1, "the cache was dropped");
        r.accept(&left[0]);
        assert_eq!(r.base, now);
        assert!(r.hunks(now, 1).is_empty());

        let mut r = Review::new("f".into(), String::new());
        let all = r.hunks("new file\n", 1)[0].clone();
        r.accept(&all);
        assert_eq!(r.base, "new file\n");
    }
}
