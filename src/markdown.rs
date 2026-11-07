//! Markdown highlighting: a line scanner that names each piece of a line the
//! way tree-sitter names code (`markup.heading.1`, `markup.bold`, ...), so
//! the same `(syntax-theme)` and faces style it. Headings keep their `#`
//! marks (named `markup.heading.N.marker`) so the cursor moves over the real
//! text.

use crate::syntax::Span;
use regex::Regex;
use ropey::Rope;
use std::ops::Range;
use std::sync::LazyLock;

static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ {0,3}(#{1,6})(?:[ \t]+|$)").unwrap());
static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ {0,3}(```|~~~)").unwrap());
static QUOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ {0,3}>[ ]?").unwrap());
static LIST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:[-*+]|\d+[.)])[ \t]+").unwrap());
static RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}(?:(?:\*[ \t]*){3,}|(?:-[ \t]*){3,}|(?:_[ \t]*){3,})$").unwrap());
static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`]+`").unwrap());
static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*[^*]+\*\*|__[^_]+__").unwrap());
static ITALIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^*\w])(\*[^*\s][^*]*\*)|(?:^|[^_\w])(_[^_\s][^_]*_)").unwrap());
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\[[^\]]+\])(\([^)]*\))").unwrap());
static TABLE_ROW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\|").unwrap());
static RULE_CELL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^:?-+:?$").unwrap());

/// Whether a code fence is open at the start of `line`.
fn fence_open_at(rope: &Rope, line: usize) -> bool {
    (0..line.min(rope.len_lines())).fold(false, |open, i| open ^ FENCE.is_match(&rope.line(i).to_string()))
}

/// Spans for `lines`, one list per line, columns in characters.
pub fn highlight(rope: &Rope, lines: Range<usize>) -> Vec<Vec<Span>> {
    let mut in_fence = fence_open_at(rope, lines.start);
    lines
        .clone()
        .map(|i| {
            if i >= rope.len_lines() {
                return Vec::new();
            }
            let text = rope.line(i).to_string();
            let text = text.trim_end_matches(['\n', '\r']);
            let fence = FENCE.is_match(text);
            let spans = if in_fence || fence {
                whole(text, "markup.raw.block")
            } else if is_table_row(text) {
                let next = (i + 1 < rope.len_lines()).then(|| rope.line(i + 1).to_string());
                let header = next.is_some_and(|n| is_rule_row(n.trim_end()));
                table_spans(text, header)
            } else {
                line_spans(text)
            };
            if fence {
                in_fence = !in_fence;
            }
            spans
        })
        .collect()
}

/// A table row: pipes dimmed, the header row (the one above `|---|`) named
/// `markup.table.header`, cells otherwise styled like any text.
fn table_spans(text: &str, header: bool) -> Vec<Span> {
    if is_rule_row(text) {
        return whole(text, "markup.table.delimiter");
    }
    let mut names = names_of(line_spans(text), text.chars().count());
    if header {
        for slot in names.iter_mut().filter(|s| s.is_none()) {
            *slot = Some("markup.table.header".into());
        }
    }
    for col in split_points(text) {
        names[col] = Some("markup.table.delimiter".into());
    }
    runs(names)
}

fn names_of(spans: Vec<Span>, n: usize) -> Vec<Option<String>> {
    let mut names = vec![None; n];
    for s in spans {
        for slot in names.iter_mut().take(s.end).skip(s.start) {
            *slot = Some(s.capture.clone());
        }
    }
    names
}

fn whole(text: &str, capture: &str) -> Vec<Span> {
    let n = text.chars().count();
    if n == 0 { vec![] } else { vec![Span { start: 0, end: n, capture: capture.into() }] }
}

/// One capture name per character, then runs of the same name.
fn line_spans(text: &str) -> Vec<Span> {
    let col = |byte: usize| text[..byte].chars().count();
    let n = text.chars().count();
    let mut names: Vec<Option<String>> = vec![None; n];
    let paint = |from: usize, to: usize, name: &str, names: &mut Vec<Option<String>>| {
        for slot in names.iter_mut().take(col(to)).skip(col(from)) {
            *slot = Some(name.to_string());
        }
    };

    if let Some(m) = HEADING.captures(text) {
        let level = m[1].len();
        let marker_end = m.get(0).unwrap().end();
        paint(0, marker_end, &format!("markup.heading.{level}.marker"), &mut names);
        paint(marker_end, text.len(), &format!("markup.heading.{level}"), &mut names);
        return runs(names);
    }
    if RULE.is_match(text) {
        return whole(text, "markup.rule");
    }
    let mut body = 0;
    if let Some(m) = QUOTE.find(text) {
        paint(0, m.end(), "markup.quote.marker", &mut names);
        paint(m.end(), text.len(), "markup.quote", &mut names);
        body = m.end();
    }
    if let Some(m) = LIST.find(&text[body..]) {
        paint(body, body + m.end(), "markup.list", &mut names);
    }

    // Inline markup. Code spans win over everything inside them.
    let mut taken = vec![false; text.len() + 1];
    let free = |a: usize, b: usize, taken: &[bool]| !taken[a..b].iter().any(|&t| t);
    let take = |a: usize, b: usize, taken: &mut Vec<bool>| taken[a..b].iter_mut().for_each(|t| *t = true);
    for m in CODE.find_iter(text) {
        paint(m.start(), m.end(), "markup.raw", &mut names);
        take(m.start(), m.end(), &mut taken);
    }
    for c in LINK.captures_iter(text) {
        let (t, u) = (c.get(1).unwrap(), c.get(2).unwrap());
        if free(t.start(), u.end(), &taken) {
            paint(t.start(), t.end(), "markup.link.text", &mut names);
            paint(u.start(), u.end(), "markup.link.url", &mut names);
            take(t.start(), u.end(), &mut taken);
        }
    }
    for m in BOLD.find_iter(text) {
        if free(m.start(), m.end(), &taken) {
            paint(m.start(), m.end(), "markup.bold", &mut names);
            take(m.start(), m.end(), &mut taken);
        }
    }
    for c in ITALIC.captures_iter(text) {
        if let Some(m) = c.get(1).or_else(|| c.get(2))
            && free(m.start(), m.end(), &taken)
        {
            paint(m.start(), m.end(), "markup.italic", &mut names);
        }
    }
    runs(names)
}

fn runs(names: Vec<Option<String>>) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (i, name) in names.into_iter().enumerate() {
        let Some(name) = name else { continue };
        match spans.last_mut() {
            Some(s) if s.end == i && s.capture == name => s.end = i + 1,
            _ => spans.push(Span { start: i, end: i + 1, capture: name }),
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> Vec<(String, String)> {
        let chars: Vec<char> = text.chars().collect();
        line_spans(text)
            .into_iter()
            .map(|s| (chars[s.start..s.end].iter().collect(), s.capture))
            .collect()
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn headings_keep_their_marks() {
        assert_eq!(
            line("## Keys"),
            vec![pair("## ", "markup.heading.2.marker"), pair("Keys", "markup.heading.2")]
        );
        assert_eq!(line("#hashtag"), vec![], "no space: not a heading");
    }

    #[test]
    fn inline_markup() {
        assert_eq!(
            line("a **b** *c* `d` [e](f)"),
            vec![
                pair("**b**", "markup.bold"),
                pair("*c*", "markup.italic"),
                pair("`d`", "markup.raw"),
                pair("[e]", "markup.link.text"),
                pair("(f)", "markup.link.url"),
            ]
        );
        assert_eq!(line("`**not bold**`"), vec![pair("`**not bold**`", "markup.raw")]);
    }

    #[test]
    fn lists_quotes_and_rules() {
        assert_eq!(line("- item"), vec![pair("- ", "markup.list")]);
        assert_eq!(line("12. item"), vec![pair("12. ", "markup.list")]);
        assert_eq!(line("> said"), vec![pair("> ", "markup.quote.marker"), pair("said", "markup.quote")]);
        assert_eq!(line("---"), vec![pair("---", "markup.rule")]);
    }

    #[test]
    fn code_fences_span_lines() {
        let rope = Rope::from_str("# Title\n```rust\nlet x = **y**;\n```\ntext **b**\n");
        let spans = highlight(&rope, 0..5);
        assert_eq!(spans[1][0].capture, "markup.raw.block");
        assert_eq!(spans[2], vec![Span { start: 0, end: 14, capture: "markup.raw.block".into() }]);
        assert_eq!(spans[3][0].capture, "markup.raw.block");
        assert_eq!(spans[4][0].capture, "markup.bold", "after the fence closes");
        let later = highlight(&rope, 2..3);
        assert_eq!(later[0][0].capture, "markup.raw.block", "knows the fence is open from earlier lines");
    }
}


// ---- Tables, aligned like Org mode does

pub fn is_table_row(line: &str) -> bool {
    TABLE_ROW.is_match(line)
}

fn is_rule_row(line: &str) -> bool {
    let cells = split_cells(line);
    is_table_row(line) && !cells.is_empty() && cells.iter().all(|c| RULE_CELL.is_match(c.trim()))
}

/// Character columns of the `|`s that split a row: not escaped (`\|`), not
/// inside a `code` span.
fn split_points(line: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let (mut code, mut escaped) = (false, false);
    for (col, ch) in line.chars().enumerate() {
        match ch {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '`' => code = !code,
            '|' if !code => out.push(col),
            _ => {}
        }
    }
    out
}

/// The cells of a row, between its pipes, untrimmed.
fn split_cells(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.trim_end().chars().collect();
    let line: String = chars.iter().collect();
    let pipes = split_points(&line);
    let mut cells: Vec<String> =
        pipes.windows(2).map(|w| chars[w[0] + 1..w[1]].iter().collect()).collect();
    // Text after the last pipe (a row being typed, missing its closing |).
    if let Some(&last) = pipes.last() {
        let rest: String = chars[last + 1..].iter().collect();
        if !rest.trim().is_empty() {
            cells.push(rest);
        }
    }
    cells
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Align {
    Left,
    Center,
    Right,
}

/// An aligned table and where the cursor goes in it.
#[derive(Debug, PartialEq)]
pub struct Aligned {
    pub lines: Vec<String>,
    /// Row (index in `lines`) and column of the cursor.
    pub cursor: (usize, usize),
}

/// Which cell a column of a row is in, and how far into its text.
fn cell_at(line: &str, col: usize) -> (usize, usize) {
    let pipes = split_points(line);
    let chars: Vec<char> = line.chars().collect();
    let cell = pipes.iter().filter(|&&p| p < col).count().max(1) - 1;
    let start = pipes.get(cell).map_or(0, |p| p + 1);
    let lead = chars[start.min(chars.len())..].iter().take_while(|c| **c == ' ').count();
    (cell, col.saturating_sub(start + lead))
}

/// Pads every column to its widest cell and redraws the `|---|` row.
/// `cursor` is (row, column) in `lines`; it stays in the same cell at the
/// same place in its text, then moves `move_cells` cells (TAB, S-TAB),
/// skipping the rule row. Moving past the last cell adds an empty row.
pub fn align_table(lines: &[String], cursor: (usize, usize), move_cells: isize) -> Aligned {
    let indent: String = lines.first().map_or("".into(), |l| l.chars().take_while(|c| c.is_whitespace()).collect());
    let mut rows: Vec<Vec<String>> = lines.iter().map(|l| split_cells(l).iter().map(|c| c.trim().to_string()).collect()).collect();
    let rule: Vec<bool> = lines.iter().map(|l| is_rule_row(l)).collect();
    let cols = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for r in rows.iter_mut() {
        r.resize(cols, String::new());
    }
    let aligns: Vec<Align> = (0..cols)
        .map(|c| match rows.iter().zip(&rule).find(|(_, is)| **is).map(|(r, _)| r[c].as_str()) {
            Some(cell) if cell.starts_with(':') && cell.ends_with(':') && cell.len() > 1 => Align::Center,
            Some(cell) if cell.ends_with(':') => Align::Right,
            _ => Align::Left,
        })
        .collect();

    let (cur_row, cur_col) = cursor;
    let (mut cell, offset) = lines.get(cur_row).map_or((0, 0), |l| cell_at(l, cur_col));
    let mut row = cur_row.min(rows.len().saturating_sub(1));
    let mut offset = offset;

    // Move between cells, skipping rule rows; past the end, add a row.
    if move_cells != 0 {
        offset = 0;
        let mut steps = move_cells.unsigned_abs();
        while steps > 0 {
            if move_cells > 0 {
                if cell + 1 < cols {
                    cell += 1;
                } else if let Some(next) = (row + 1..rows.len()).find(|&r| !rule[r]) {
                    (row, cell) = (next, 0);
                } else {
                    rows.push(vec![String::new(); cols]);
                    (row, cell) = (rows.len() - 1, 0);
                }
            } else if cell > 0 {
                cell -= 1;
            } else if let Some(prev) = (0..row).rev().find(|&r| !rule[r]) {
                (row, cell) = (prev, cols - 1);
            }
            steps -= 1;
        }
    }
    let rule_at = |r: usize| rule.get(r).copied().unwrap_or(false);

    let width = |c: usize| {
        rows.iter().enumerate().filter(|(r, _)| !rule_at(*r)).map(|(_, row)| row[c].chars().count()).max().unwrap_or(0).max(3)
    };
    let widths: Vec<usize> = (0..cols).map(width).collect();
    let pad = |text: &str, w: usize, align: Align| {
        let gap = w - text.chars().count();
        match align {
            Align::Left => format!("{text}{}", " ".repeat(gap)),
            Align::Right => format!("{}{text}", " ".repeat(gap)),
            Align::Center => format!("{}{text}{}", " ".repeat(gap / 2), " ".repeat(gap - gap / 2)),
        }
    };

    let mut out = Vec::new();
    let mut cursor_out = (row, indent.chars().count());
    for (r, cells) in rows.iter().enumerate() {
        if rule_at(r) {
            let dashes: Vec<String> = (0..cols)
                .map(|c| {
                    let w = widths[c] + 2;
                    match aligns[c] {
                        Align::Left => "-".repeat(w),
                        Align::Right => format!("{}:", "-".repeat(w - 1)),
                        Align::Center => format!(":{}:", "-".repeat(w - 2)),
                    }
                })
                .collect();
            out.push(format!("{indent}|{}|", dashes.join("|")));
            continue;
        }
        let mut line = format!("{indent}|");
        for (c, text) in cells.iter().enumerate() {
            let content_start = line.chars().count() + 1;
            if r == row && c == cell {
                let gap = widths[c] - text.chars().count();
                let lead = match aligns[c] {
                    Align::Left => 0,
                    Align::Right => gap,
                    Align::Center => gap / 2,
                };
                let within = if text.is_empty() { 0 } else { offset.min(text.chars().count()) };
                cursor_out = (r, content_start + lead + within);
            }
            line.push_str(&format!(" {} |", pad(text, widths[c], aligns[c])));
        }
        out.push(line);
    }
    Aligned { lines: out, cursor: cursor_out }
}

#[cfg(test)]
mod table_tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn pads_columns_and_redraws_the_rule() {
        let t = lines("| Layer | Where | What |\n|---|---|---|\n| Text storage | `src/buffer.rs` | a rope |");
        let a = align_table(&t, (0, 0), 0);
        assert_eq!(
            a.lines,
            vec![
                "| Layer        | Where           | What   |",
                "|--------------|-----------------|--------|",
                "| Text storage | `src/buffer.rs` | a rope |",
            ]
        );
    }

    #[test]
    fn keeps_alignment_markers() {
        let t = lines("| a | b | c |\n|:-|:-:|-:|\n| left | mid | right |");
        assert_eq!(
            align_table(&t, (0, 0), 0).lines,
            vec!["| a    |  b  |     c |", "|------|:---:|------:|", "| left | mid | right |"]
        );
    }

    #[test]
    fn pipes_in_code_and_escapes_dont_split() {
        let t = lines("| `a|b` | x\\|y |\n|-|-|");
        assert_eq!(align_table(&t, (0, 0), 0).lines[0], "| `a|b` | x\\|y |");
    }

    #[test]
    fn the_cursor_stays_in_its_cell() {
        let t = lines("| a | b |\n|-|-|\n| long text | x |");
        // On the "x" in the last row.
        let a = align_table(&t, (2, 14), 0);
        assert_eq!(a.lines[2], "| long text | x   |");
        assert_eq!(a.cursor, (2, 14));
        // Inside "long text", 5 characters in.
        let a = align_table(&t, (2, 7), 0);
        assert_eq!(a.cursor, (2, 7));
        let a = align_table(&lines("|a|bb|"), (0, 3), 0);
        assert_eq!(a.lines[0], "| a   | bb  |");
        assert_eq!(a.cursor, (0, 8), "at the start of bb");
    }

    #[test]
    fn tab_moves_between_cells_and_adds_rows() {
        let t = lines("| a | b |\n|-|-|\n| c | d |");
        let a = align_table(&t, (0, 2), 1);
        assert_eq!(a.cursor, (0, 8), "a -> b");
        let a = align_table(&t, (0, 8), 1);
        assert_eq!(a.cursor, (2, 2), "b -> c, over the rule row");
        let a = align_table(&t, (2, 8), 1);
        assert_eq!(a.lines.len(), 4, "past the last cell: a new row");
        assert_eq!(a.lines[3], "|     |     |");
        assert_eq!(a.cursor, (3, 2));
        let a = align_table(&t, (2, 2), -1);
        assert_eq!(a.cursor, (0, 8), "S-TAB goes back over the rule row");
    }

    #[test]
    fn a_row_being_typed_gets_its_closing_pipe() {
        let t = lines("| name | n |\n|-|-|\n| x | 12");
        assert_eq!(align_table(&t, (0, 0), 0).lines[2], "| x    | 12  |");
    }

    #[test]
    fn tables_are_highlighted() {
        let rope = Rope::from_str("| h | i |\n|---|---|\n| a | `b` |\n");
        let spans = highlight(&rope, 0..3);
        assert_eq!(spans[0][0].capture, "markup.table.delimiter");
        assert!(spans[0].iter().any(|s| s.capture == "markup.table.header"));
        assert_eq!(spans[1], vec![Span { start: 0, end: 9, capture: "markup.table.delimiter".into() }]);
        assert!(spans[2].iter().any(|s| s.capture == "markup.raw"), "cells keep inline markup");
        assert!(!spans[2].iter().any(|s| s.capture == "markup.table.header"));
    }
}
