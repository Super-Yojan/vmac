//! Git, the way magit does it: by running git. The status buffer, commits,
//! branches and the log are Lua (`lua/core/git.lua`), on top of `vmac.git`;
//! what's here is what Lua can't do well or fast:
//!
//! - Hunks, staged and unstaged, worked out from the texts themselves (HEAD,
//!   the index, the file) and staged by writing the index's new text as a
//!   blob, so there are no patches to get wrong.
//! - The signs in a file's gutter: its lines that differ from the index.
//! - Blame, in the background, for the line under the cursor.

use crate::review::{hunks, Hunk};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// The repository a folder is in: the nearest folder up with `.git`.
pub fn root_of(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// The repository's git folder: `.git`, or where a `.git` file points (a
/// worktree, a submodule).
pub fn git_dir(root: &Path) -> PathBuf {
    let dot = root.join(".git");
    if dot.is_file()
        && let Ok(text) = std::fs::read_to_string(&dot)
        && let Some(dir) = text.trim().strip_prefix("gitdir:")
    {
        let dir = PathBuf::from(dir.trim());
        return if dir.is_absolute() { dir } else { root.join(dir) };
    }
    dot
}

/// Runs git in `root`, with `stdin` fed to it: whether it worked, what it
/// printed, and what it printed on stderr. It never waits for a password.
pub fn run(root: &Path, args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, String::new(), format!("git: {e}")),
    };
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A thread, so a big input can't block against a full stdout pipe.
        let text = text.to_string();
        std::thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    match child.wait_with_output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).trim_end().to_string(),
        ),
        Err(e) => (false, String::new(), format!("git: {e}")),
    }
}

/// The branch checked out (read from HEAD, no git run), or the commit's
/// short hash when detached; "" outside a repository.
pub fn branch(root: &Path) -> String {
    let Ok(head) = std::fs::read_to_string(git_dir(root).join("HEAD")) else { return String::new() };
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(r) => r.strip_prefix("refs/heads/").unwrap_or(r).to_string(),
        None => head.chars().take(7).collect(),
    }
}

/// One of the three texts a file has.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Side {
    Head,
    Index,
    Worktree,
}

/// A file's text at HEAD, in the index or on disk (None: it isn't there).
pub fn read(root: &Path, rel: &str, side: Side) -> Option<String> {
    let spec = match side {
        Side::Worktree => return std::fs::read(root.join(rel)).ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
        Side::Head => format!("HEAD:{rel}"),
        Side::Index => format!(":{rel}"),
    };
    let (ok, out, _) = run(root, &["show", &spec], None);
    ok.then_some(out)
}

fn binary(text: &str) -> bool {
    text.contains('\0')
}

/// The two texts a hunk list compares: staged is HEAD to the index,
/// unstaged the index to the file.
fn pair(root: &Path, rel: &str, staged: bool) -> (String, String) {
    let (from, to) = if staged { (Side::Head, Side::Index) } else { (Side::Index, Side::Worktree) };
    (read(root, rel, from).unwrap_or_default(), read(root, rel, to).unwrap_or_default())
}

/// A file's hunks, staged or not; none for a binary file.
pub fn file_hunks(root: &Path, rel: &str, staged: bool) -> (String, String, Vec<Hunk>) {
    let (old, new) = pair(root, rel, staged);
    if binary(&old) || binary(&new) {
        return (old, new, Vec::new());
    }
    let h = hunks(&old, &new);
    (old, new, h)
}

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// `old` with one hunk of the way to `new` made: its old lines replaced by
/// its new ones.
pub fn apply(old: &str, hunk: &Hunk) -> String {
    let l = lines(old);
    let mut out: String = l[..hunk.old.start.min(l.len())].concat();
    out.push_str(&hunk.new_text);
    out.push_str(&l[hunk.old.end.min(l.len())..].concat());
    out
}

/// `new` with one hunk undone: its new lines replaced by its old ones.
pub fn revert(new: &str, hunk: &Hunk) -> String {
    let l = lines(new);
    let mut out: String = l[..hunk.new.start.min(l.len())].concat();
    out.push_str(&hunk.old_text);
    out.push_str(&l[hunk.new.end.min(l.len())..].concat());
    out
}

/// A hunk as a unified diff shows it: its "@@" line, then its lines with
/// `context` unchanged lines around them.
pub fn hunk_lines(new: &str, hunk: &Hunk, context: usize) -> Vec<String> {
    let l = lines(new);
    let before = hunk.new.start.saturating_sub(context);
    let after = (hunk.new.end + context).min(l.len());
    let lead = hunk.new.start - before;
    let trail = after - hunk.new.end.min(after);
    let old_len = hunk.old.len() + lead + trail;
    let new_len = hunk.new.len() + lead + trail;
    let mut out = vec![format!(
        "@@ -{},{old_len} +{},{new_len} @@",
        hunk.old.start - lead + usize::from(old_len > 0),
        before + usize::from(new_len > 0),
    )];
    let strip = |s: &str| s.trim_end_matches(['\n', '\r']).to_string();
    out.extend(l[before..hunk.new.start.min(l.len())].iter().map(|s| format!(" {}", strip(s))));
    out.extend(hunk.old_text.lines().map(|s| format!("-{}", strip(s))));
    out.extend(hunk.new_text.lines().map(|s| format!("+{}", strip(s))));
    out.extend(l[hunk.new.end.min(l.len())..after].iter().map(|s| format!(" {}", strip(s))));
    out
}

/// The mode a path has (or will have) in the index: "100644", "100755"...
fn index_mode(root: &Path, rel: &str) -> String {
    for args in [["ls-files", "-s", "--", rel], ["ls-tree", "HEAD", "--", rel]] {
        let (ok, out, _) = run(root, &args, None);
        if let Some(mode) = out.split_whitespace().next().filter(|_| ok) {
            return mode.to_string();
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(root.join(rel)).is_ok_and(|m| m.permissions().mode() & 0o111 != 0) {
            return "100755".into();
        }
    }
    "100644".into()
}

/// Makes `text` what the index has for `rel`.
pub fn write_index(root: &Path, rel: &str, text: &str) -> Result<(), String> {
    let (ok, sha, err) = run(root, &["hash-object", "-w", "--no-filters", "--stdin"], Some(text));
    if !ok {
        return Err(err);
    }
    let info = format!("{},{},{rel}", index_mode(root, rel), sha.trim());
    let (ok, _, err) = run(root, &["update-index", "--add", "--cacheinfo", &info], None);
    if ok { Ok(()) } else { Err(err) }
}

/// What to do with a hunk in the status buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HunkAction {
    /// An unstaged hunk into the index.
    Stage,
    /// A staged hunk out of the index (back to HEAD's lines).
    Unstage,
    /// An unstaged hunk thrown away: the file gets the index's lines back.
    Discard,
}

/// Stages, unstages or discards hunk `n` of a file (counted as `file_hunks`
/// lists them: unstaged for Stage and Discard, staged for Unstage).
pub fn hunk_action(root: &Path, rel: &str, n: usize, action: HunkAction) -> Result<(), String> {
    let staged = action == HunkAction::Unstage;
    let (old, new, list) = file_hunks(root, rel, staged);
    let hunk = list.get(n).ok_or("That change is gone (refresh with g r)")?;
    match action {
        HunkAction::Stage => write_index(root, rel, &apply(&old, hunk)),
        HunkAction::Unstage => write_index(root, rel, &revert(&new, hunk)),
        HunkAction::Discard => std::fs::write(root.join(rel), revert(&new, hunk)).map_err(|e| format!("{rel}: {e}")),
    }
}

// ---- Signs in the gutter

/// How a line differs from the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sign {
    Added,
    Changed,
    /// Lines were deleted above this one.
    Deleted,
}

/// The signs for each line, from a text's hunks against its base.
pub fn signs(hunks: &[Hunk], line_count: usize) -> Vec<(usize, Sign)> {
    let mut out = Vec::new();
    for h in hunks {
        if h.new.is_empty() {
            out.push((h.new.start.min(line_count.saturating_sub(1)), Sign::Deleted));
        } else {
            let sign = if h.old.is_empty() { Sign::Added } else { Sign::Changed };
            out.extend(h.new.clone().map(|l| (l, sign)));
        }
    }
    out
}

/// What a file is compared with for its signs, and when that was read.
#[derive(Clone)]
struct Base {
    /// The index file's time when this was read (it changes on stage,
    /// commit, checkout...).
    stamp: Option<SystemTime>,
    /// The index's text; None when the file isn't in a repository or is
    /// ignored (no signs).
    text: Option<Arc<String>>,
}

/// Blame for one line.
#[derive(Clone, Debug, PartialEq)]
pub struct BlameLine {
    pub hash: String,
    pub author: String,
    /// Seconds since 1970.
    pub time: i64,
    pub summary: String,
}

impl BlameLine {
    pub fn uncommitted(&self) -> bool {
        self.hash.bytes().all(|b| b == b'0')
    }

    /// "Jane Doe, 3 days ago · Fix the parser" (what's drawn after the line).
    pub fn describe(&self, now: i64) -> String {
        if self.uncommitted() {
            return "You, uncommitted changes".into();
        }
        format!("{}, {} · {}", self.author, ago(now - self.time), self.summary)
    }
}

/// "just now", "5 minutes ago", "3 days ago", "2 years ago".
pub fn ago(seconds: i64) -> String {
    let s = seconds.max(0);
    let (n, unit) = match s {
        0..60 => return "just now".into(),
        60..3600 => (s / 60, "minute"),
        3600..86_400 => (s / 3600, "hour"),
        86_400..2_592_000 => (s / 86_400, "day"),
        2_592_000..31_536_000 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Reads `git blame --porcelain`: each line's commit, in order.
pub fn parse_blame(out: &str) -> Vec<Option<Arc<BlameLine>>> {
    let mut commits: HashMap<String, BlameLine> = HashMap::new();
    let mut result = Vec::new();
    let mut current: Option<(String, usize)> = None;
    for line in out.lines() {
        if line.starts_with('\t') {
            // The line's text ends its entry.
            if let Some((hash, final_line)) = current.take() {
                let info = commits.entry(hash.clone()).or_insert_with(|| BlameLine {
                    hash: hash.clone(),
                    author: String::new(),
                    time: 0,
                    summary: String::new(),
                });
                if result.len() < final_line {
                    result.resize(final_line, None);
                }
                result[final_line - 1] = Some(Arc::new(info.clone()));
            }
            continue;
        }
        let mut words = line.splitn(2, ' ');
        let (key, rest) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
        if key.len() == 40 && key.bytes().all(|b| b.is_ascii_hexdigit()) {
            let final_line = rest.split(' ').nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            current = (final_line > 0).then(|| (key.to_string(), final_line));
            commits.entry(key.to_string()).or_insert_with(|| BlameLine {
                hash: key.to_string(),
                author: String::new(),
                time: 0,
                summary: String::new(),
            });
            continue;
        }
        let Some((hash, _)) = &current else { continue };
        let Some(c) = commits.get_mut(hash) else { continue };
        match key {
            "author" => c.author = rest.to_string(),
            "author-time" => c.time = rest.parse().unwrap_or(0),
            "summary" => c.summary = rest.to_string(),
            _ => {}
        }
    }
    result
}

/// Signs per file: the buffer revision and index time they were made from.
type SignCache = HashMap<PathBuf, (u64, Option<SystemTime>, Arc<Vec<(usize, Sign)>>)>;

type BlameResults = Arc<Mutex<HashMap<PathBuf, (u64, Vec<Option<Arc<BlameLine>>>)>>>;

/// Git's state kept between frames: bases for signs, blame.
#[derive(Default)]
pub struct Git {
    bases: HashMap<PathBuf, Base>,
    /// Signs per file, for the buffer revision and base they were made from.
    signs: SignCache,
    /// Whether the line under the cursor shows who changed it last.
    pub blame_inline: bool,
    blames: BlameResults,
    running: Arc<Mutex<HashSet<PathBuf>>>,
    /// A blame finished (so redraw).
    fresh: Arc<AtomicBool>,
    /// When each file's buffer revision was first seen: blame waits for
    /// typing to stop.
    seen: HashMap<PathBuf, (u64, Instant)>,
}

fn index_stamp(root: &Path) -> Option<SystemTime> {
    std::fs::metadata(git_dir(root).join("index")).and_then(|m| m.modified()).ok()
}

/// A file's repository and its path in it ("src/main.rs").
pub fn locate(path: &Path) -> Option<(PathBuf, String)> {
    let root = root_of(path.parent()?)?;
    let rel = path.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/");
    Some((root, rel))
}

impl std::fmt::Debug for Git {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Git").field("blame_inline", &self.blame_inline).finish()
    }
}

impl Git {
    pub fn new() -> Self {
        Self { blame_inline: true, ..Default::default() }
    }

    fn base(&mut self, path: &Path) -> Option<Arc<String>> {
        let (root, rel) = locate(path)?;
        let stamp = index_stamp(&root);
        if let Some(b) = self.bases.get(path).filter(|b| b.stamp == stamp) {
            return b.text.clone();
        }
        let text = match read(&root, &rel, Side::Index) {
            Some(t) => Some(t),
            // Not in the index: new (all of it added), unless it's ignored.
            None => {
                let (ignored, _, _) = run(&root, &["check-ignore", "-q", "--", &rel], None);
                (!ignored).then(String::new)
            }
        };
        let text = text.filter(|t| !binary(t)).map(Arc::new);
        self.bases.insert(path.to_path_buf(), Base { stamp, text: text.clone() });
        text
    }

    /// The signs for a file's buffer: its lines that differ from the index.
    pub fn signs(&mut self, path: &Path, text: &ropey::Rope, revision: u64) -> Arc<Vec<(usize, Sign)>> {
        // The index may have changed: base() looks (a stat; git runs only
        // when it did).
        let base = self.base(path);
        let stamp = self.bases.get(path).and_then(|b| b.stamp);
        if let Some((rev, s, signs)) = self.signs.get(path)
            && *rev == revision
            && *s == stamp
        {
            return signs.clone();
        }
        let signs = match base {
            Some(base) => {
                let current = text.to_string();
                Arc::new(signs(&hunks(&base, &current), text.len_lines()))
            }
            None => Arc::new(Vec::new()),
        };
        self.signs.insert(path.to_path_buf(), (revision, stamp, signs.clone()));
        signs
    }

    /// The hunks of a buffer against the index (for ]c, staging and
    /// resetting from the file itself).
    pub fn buffer_hunks(&mut self, path: &Path, text: &str) -> Option<(Arc<String>, Vec<Hunk>)> {
        let base = self.base(path)?;
        let h = hunks(&base, text);
        Some((base, h))
    }

    /// Forgets what was read of a file's index text (after staging).
    pub fn forget(&mut self, path: &Path) {
        self.bases.remove(path);
        self.signs.remove(path);
    }

    /// Forgets everything read (after a commit, a checkout...).
    pub fn forget_all(&mut self) {
        self.bases.clear();
        self.signs.clear();
        self.blames.lock().map(|mut b| b.clear()).ok();
    }

    /// Blame for a line of a file, when it's known for this revision.
    pub fn blame_line(&self, path: &Path, revision: u64, line: usize) -> Option<Arc<BlameLine>> {
        let blames = self.blames.lock().ok()?;
        let (rev, lines) = blames.get(path)?;
        if *rev != revision {
            return None;
        }
        lines.get(line).cloned().flatten()
    }

    /// Blames a file's buffer in the background when its blame is stale and
    /// it hasn't changed for a moment. Returns whether a blame finished
    /// since last asked (so redraw).
    pub fn pump(&mut self, file: Option<(&Path, &ropey::Rope, u64)>) -> bool {
        if let Some((path, text, revision)) = file.filter(|_| self.blame_inline) {
            let stale = self.blames.lock().map(|b| b.get(path).is_none_or(|(r, _)| *r != revision)).unwrap_or(false);
            let since = match self.seen.get(path) {
                Some((r, at)) if *r == revision => at.elapsed(),
                _ => {
                    self.seen.insert(path.to_path_buf(), (revision, Instant::now()));
                    Duration::ZERO
                }
            };
            let running = self.running.lock().map(|r| r.contains(path)).unwrap_or(true);
            if stale && !running && since >= Duration::from_millis(300) {
                self.blame(path, text.to_string(), revision);
            }
        }
        self.fresh.swap(false, Ordering::Relaxed)
    }

    fn blame(&mut self, path: &Path, text: String, revision: u64) {
        let Some((root, rel)) = locate(path) else { return };
        let path = path.to_path_buf();
        if let Ok(mut r) = self.running.lock() {
            r.insert(path.clone());
        }
        let (blames, running, fresh) = (self.blames.clone(), self.running.clone(), self.fresh.clone());
        std::thread::spawn(move || {
            // --contents: blame what the buffer has, saved or not.
            let (ok, out, _) = run(&root, &["blame", "--porcelain", "--contents", "-", "--", &rel], Some(&text));
            // Not committed or not tracked: nothing to say (an empty list
            // still marks it done for this revision).
            let lines = if ok { parse_blame(&out) } else { Vec::new() };
            if let Ok(mut b) = blames.lock() {
                b.insert(path.clone(), (revision, lines));
            }
            if let Ok(mut r) = running.lock() {
                r.remove(&path);
            }
            fresh.store(true, Ordering::Relaxed);
        });
    }
}

/// Seconds since 1970, now.
pub fn now() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vmac-git-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| assert!(run(&dir, args, None).0, "git {args:?}");
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "Tester"]);
        git(&["config", "commit.gpgsign", "false"]);
        dir
    }

    fn rand_suffix() -> u128 {
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn hunks_stage_unstage_and_discard_one_at_a_time() {
        let dir = repo();
        let base = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
        std::fs::write(dir.join("f.txt"), base).unwrap();
        assert!(run(&dir, &["add", "f.txt"], None).0);
        assert!(run(&dir, &["commit", "-qm", "first"], None).0);
        // Two separate changes: the top and the bottom.
        std::fs::write(dir.join("f.txt"), "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\n").unwrap();
        let (_, _, unstaged) = file_hunks(&dir, "f.txt", false);
        assert_eq!(unstaged.len(), 2);

        hunk_action(&dir, "f.txt", 1, HunkAction::Stage).unwrap();
        assert_eq!(read(&dir, "f.txt", Side::Index).unwrap(), format!("{base}m\n"), "only the second change staged");
        assert_eq!(file_hunks(&dir, "f.txt", false).2.len(), 1);
        assert_eq!(file_hunks(&dir, "f.txt", true).2.len(), 1);

        hunk_action(&dir, "f.txt", 0, HunkAction::Unstage).unwrap();
        assert_eq!(read(&dir, "f.txt", Side::Index).unwrap(), base, "unstaged again");

        hunk_action(&dir, "f.txt", 0, HunkAction::Discard).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\n", "the A thrown away");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_new_file_is_staged_whole() {
        let dir = repo();
        std::fs::write(dir.join("new.sh"), "echo hi\n").unwrap();
        hunk_action(&dir, "new.sh", 0, HunkAction::Stage).unwrap();
        assert_eq!(read(&dir, "new.sh", Side::Index).as_deref(), Some("echo hi\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hunk_lines_read_like_a_unified_diff() {
        let old = "1\n2\n3\n4\n5\n6\n";
        let new = "1\n2\n3\nfour\n5\n6\n";
        let h = &hunks(old, new)[0];
        assert_eq!(hunk_lines(new, h, 2), vec!["@@ -2,5 +2,5 @@", " 2", " 3", "-4", "+four", " 5", " 6"]);
        let added = &hunks("", "x\n")[0];
        assert_eq!(hunk_lines("x\n", added, 3), vec!["@@ -0,0 +1,1 @@", "+x"]);
    }

    #[test]
    fn signs_say_added_changed_and_deleted() {
        let h = hunks("a\nb\nc\nd\n", "a\nB\nc\nnew\nd\n");
        assert_eq!(signs(&h, 6), vec![(1, Sign::Changed), (3, Sign::Added)]);
        let h = hunks("a\nb\nc\n", "a\nc\n");
        assert_eq!(signs(&h, 3), vec![(1, Sign::Deleted)]);
    }

    #[test]
    fn blame_porcelain_is_read() {
        let h = "0123456789012345678901234567890123456789";
        let out = format!(
            "{h} 1 1 2\nauthor Jane\nauthor-time 100\nsummary Fix it\nfilename f\n\tline one\n{h} 2 2\nfilename f\n\tline two\n"
        );
        let lines = parse_blame(&out);
        assert_eq!(lines.len(), 2);
        let l = lines[1].as_ref().unwrap();
        assert_eq!((l.author.as_str(), l.time, l.summary.as_str()), ("Jane", 100, "Fix it"));
        assert_eq!(l.describe(100 + 3 * 86_400), "Jane, 3 days ago · Fix it");
    }

    #[test]
    fn blame_and_branch_in_a_real_repository() {
        let dir = repo();
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        assert!(run(&dir, &["add", "."], None).0);
        assert!(run(&dir, &["commit", "-qm", "the first line"], None).0);
        assert_eq!(branch(&dir), "main");
        let (ok, out, err) = run(&dir, &["blame", "--porcelain", "--contents", "-", "--", "f.txt"], Some("one\ntwo\n"));
        assert!(ok, "{err}");
        let lines = parse_blame(&out);
        assert_eq!(lines[0].as_ref().unwrap().summary, "the first line");
        assert!(lines[1].as_ref().unwrap().uncommitted());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn times_read_as_ago() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(7200), "2 hours ago");
        assert_eq!(ago(400 * 86_400), "1 year ago");
    }
}
