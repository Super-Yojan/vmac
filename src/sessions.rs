//! Past agent sessions to reopen. Claude Code keeps every session (from the
//! CLI, the desktop app or vmac) in ~/.claude/projects/<project>/<id>.jsonl;
//! vmac lists them from there, newest first, titled as Claude titled them.
//! Other agents list theirs over ACP (`session/list`, in agent.rs).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A session that can be reopened.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub id: String,
    /// The folder it ran in.
    pub cwd: PathBuf,
    pub title: String,
    /// Seconds since 1970 it last changed (0: unknown).
    pub updated: u64,
}

/// Bytes read from each end of a session file: enough for its folder (at
/// the start) and its latest title (rewritten as it goes, so near the end).
const HEAD: u64 = 64 * 1024;
const TAIL: u64 = 512 * 1024;

/// Where Claude Code keeps its sessions.
pub fn claude_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude/projects"))
}

/// Claude Code's sessions, newest first: those that ran in `project` (or a
/// folder inside it), or all of them.
pub fn claude_sessions(dir: &Path, project: Option<&Path>) -> Vec<Session> {
    let mut files: Vec<(PathBuf, u64)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .flat_map(|e| std::fs::read_dir(e.path()).into_iter().flatten().filter_map(Result::ok))
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .map(|p| {
            let updated = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            (p, updated)
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    files
        .into_iter()
        .filter_map(|(path, updated)| cached(&path, updated))
        .filter(|s| project.is_none_or(|p| s.cwd.starts_with(p)))
        .collect()
}

/// Sessions read before, by file, kept while the file is unchanged.
type Cache = std::collections::HashMap<PathBuf, (u64, Option<Session>)>;
static CACHE: std::sync::LazyLock<std::sync::Mutex<Cache>> = std::sync::LazyLock::new(Default::default);

fn cached(path: &Path, updated: u64) -> Option<Session> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((when, session)) = cache.get(path)
        && *when == updated
    {
        return session.clone();
    }
    let session = read_claude_session(path, updated);
    cache.insert(path.to_path_buf(), (updated, session.clone()));
    session
}

fn read_claude_session(path: &Path, updated: u64) -> Option<Session> {
    let id = path.file_stem()?.to_string_lossy().into_owned();
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    // Cut at a byte count, so possibly mid-character: kept as far as valid.
    let mut bytes = Vec::new();
    (&mut file).take(HEAD).read_to_end(&mut bytes).ok()?;
    let head = String::from_utf8_lossy(&bytes).into_owned();
    let tail = if len > HEAD {
        let start = len.saturating_sub(TAIL).max(HEAD);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = Vec::new();
        file.take(TAIL).read_to_end(&mut bytes).ok()?;
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        String::new()
    };
    // Only lines that can hold what's wanted are parsed: most are long
    // replies and tool output, and parsing them is most of the time.
    let wanted = |l: &&str| {
        l.contains("-title\"") || l.contains("\"last-prompt\"") || (l.len() < 20_000 && (l.contains("\"cwd\"") || l.contains("\"user\"")))
    };
    let lines = || head.lines().chain(tail.lines()).filter(wanted).filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok());

    let cwd = lines().find_map(|v| v["cwd"].as_str().map(PathBuf::from))?;
    // The latest title it was given: yours, else Claude's, else the last prompt,
    // else the first thing asked.
    let latest = |key: &str, field: &str| lines().filter(|v| v["type"] == key).filter_map(|v| v[field].as_str().map(str::to_string)).next_back();
    let title = latest("custom-title", "customTitle")
        .or_else(|| latest("ai-title", "aiTitle"))
        .or_else(|| latest("last-prompt", "lastPrompt"))
        .or_else(|| lines().filter(|v| v["type"] == "user" && v["isSidechain"] != true).find_map(|v| first_text(&v["message"]["content"])))
        .unwrap_or_else(|| "(untitled)".into());
    let title: String = title.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(100).collect();
    Some(Session { id, cwd, title, updated })
}

/// The text of a message's content: a string, or its first text block.
fn first_text(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string()).filter(|s| !s.trim().is_empty() && !s.starts_with('<'));
    }
    content.as_array()?.iter().find_map(|b| {
        (b["type"] == "text").then(|| b["text"].as_str().map(str::to_string)).flatten().filter(|s| !s.starts_with('<'))
    })
}

/// "3m", "2h", "5d": how long ago, roughly.
pub fn ago(updated: u64) -> String {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if updated == 0 {
        return String::new();
    }
    let s = now.saturating_sub(updated);
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// When an RFC 3339 time ("2026-11-07T22:10:00Z", as agents send it) was,
/// in seconds since 1970; 0 when it can't be read.
pub fn parse_time(s: &str) -> u64 {
    let digits = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) =
        (digits(0, 4), digits(5, 7), digits(8, 10), digits(11, 13), digits(14, 16), digits(17, 19))
    else {
        return 0;
    };
    // Days from 1970-01-01 to the date (civil calendar).
    let (y, mo) = if mo <= 2 { (y - 1, mo + 12) } else { (y, mo) };
    let days = 365 * y + y / 4 - y / 100 + y / 400 + (153 * (mo - 3) + 2) / 5 + d - 719469;
    (days * 86400 + h * 3600 + mi * 60 + se).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_sessions_list_newest_first_with_their_titles() {
        let dir = std::env::temp_dir().join(format!("vmac-claude-projects-{}", std::process::id()));
        let (a, b) = (dir.join("-work-alpha"), dir.join("-work-beta"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let line = |v: serde_json::Value| format!("{v}\n");
        std::fs::write(
            a.join("s1.jsonl"),
            [
                line(serde_json::json!({ "type": "user", "cwd": "/work/alpha", "message": { "content": "Fix the build please" } })),
                line(serde_json::json!({ "type": "ai-title", "aiTitle": "Old title" })),
                line(serde_json::json!({ "type": "ai-title", "aiTitle": "Fix the CI build" })),
            ]
            .concat(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(
            a.join("s2.jsonl"),
            [
                line(serde_json::json!({ "type": "user", "cwd": "/work/alpha/sub", "message": { "content": [{ "type": "text", "text": "Explain   the\nparser" }] } })),
            ]
            .concat(),
        )
        .unwrap();
        std::fs::write(
            b.join("s3.jsonl"),
            [
                line(serde_json::json!({ "type": "user", "cwd": "/work/beta", "message": { "content": "hi" } })),
                line(serde_json::json!({ "type": "custom-title", "customTitle": "Mine" })),
                line(serde_json::json!({ "type": "ai-title", "aiTitle": "Claude's" })),
            ]
            .concat(),
        )
        .unwrap();

        let all = claude_sessions(&dir, None);
        let ids: Vec<&str> = all.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[2], "s1", "oldest last: {ids:?}");
        let s1 = &all[2];
        assert_eq!((s1.title.as_str(), s1.cwd.as_path()), ("Fix the CI build", Path::new("/work/alpha")));
        assert_eq!(all.iter().find(|s| s.id == "s2").unwrap().title, "Explain the parser", "the first question, tidied");
        assert_eq!(all.iter().find(|s| s.id == "s3").unwrap().title, "Mine", "your title beats Claude's");

        let alpha = claude_sessions(&dir, Some(Path::new("/work/alpha")));
        assert_eq!(alpha.len(), 2, "sessions in a folder inside it count");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Your real Claude Code sessions: how long listing them takes.
    #[test]
    #[ignore]
    fn lists_the_real_claude_sessions_quickly() {
        let Some(dir) = claude_dir() else { return };
        let t = std::time::Instant::now();
        let all = claude_sessions(&dir, None);
        let took = t.elapsed();
        for s in all.iter().take(6) {
            println!("{}  {}  {}", ago(s.updated), s.cwd.display(), s.title);
        }
        let t = std::time::Instant::now();
        claude_sessions(&dir, None);
        println!("{} sessions in {took:?}, again in {:?}", all.len(), t.elapsed());
        assert!(took < std::time::Duration::from_secs(5), "debug builds are slower; release lists them in about 0.1s");
    }

    #[test]
    fn times_parse_and_read_as_ago() {
        assert_eq!(parse_time("1970-01-02T00:00:00Z"), 86400);
        assert_eq!(parse_time("2026-11-07T22:10:05.123Z"), 1794089405);
        assert_eq!(parse_time("garbage"), 0);
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(ago(now - 7200), "2h ago");
        assert_eq!(ago(0), "");
    }
}
