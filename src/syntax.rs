//! Syntax highlighting with tree-sitter.
//!
//! Rust parses (once per change) and runs the language's highlight query over
//! the lines on screen; what each capture (`keyword`, `function.method`, ...)
//! looks like is decided in Lua by `core.syntax_theme()`.

use crate::editor::BufferId;
use ropey::Rope;
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator, Tree};

/// The language for a file, by its extension.
pub fn language_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?;
    Some(match ext {
        "rs" => "rust",
        "json" => "json",
        "toml" => "toml",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "c" | "h" => "c",
        "go" => "go",
        "sh" | "bash" | "zsh" => "bash",
        "md" | "markdown" => "markdown",
        // Grammars vmac installs when such a file opens (packages.rs).
        "lua" => "lua",
        "yml" | "yaml" => "yaml",
        "html" | "htm" => "html",
        "css" => "css",
        "java" => "java",
        "rb" => "ruby",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "zig" => "zig",
        "nix" => "nix",
        "php" => "php",
        "hs" => "haskell",
        "ex" | "exs" => "elixir",
        "scala" | "sc" => "scala",
        "cs" => "c_sharp",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        _ => return None,
    })
}

struct LanguageConfig {
    language: Language,
    query: Query,
}

/// A built-in highlight query, for an installed grammar that adds to it.
fn builtin_query(name: &str) -> Option<&'static str> {
    Some(match name {
        "c" => tree_sitter_c::HIGHLIGHT_QUERY,
        "javascript" => tree_sitter_javascript::HIGHLIGHT_QUERY,
        _ => return None,
    })
}

/// A grammar vmac installed (packages.rs); asks for it when it's missing.
fn load_installed(name: &str) -> Option<LanguageConfig> {
    if !crate::packages::grammar_installed(name) {
        crate::packages::want_grammar(name);
        return None;
    }
    let (language, own) = match crate::packages::load_grammar(name) {
        Ok(loaded) => loaded,
        Err(e) => {
            crate::packages::notify(format!("the {name} grammar didn't load: {e}"));
            return None;
        }
    };
    let inherited = crate::packages::grammar_for(name).and_then(|g| g.inherits).and_then(builtin_query);
    let source = match inherited {
        Some(base) => format!("{own}\n{base}"),
        None => own.clone(),
    };
    // With the inherited query first if that works, else the grammar's own alone.
    match Query::new(&language, &source).or_else(|_| Query::new(&language, &own)) {
        Ok(query) => Some(LanguageConfig { language, query }),
        Err(e) => {
            crate::packages::notify(format!("the {name} highlights didn't load: {e}"));
            None
        }
    }
}

fn load_language(name: &str) -> Option<LanguageConfig> {
    let js = tree_sitter_javascript::HIGHLIGHT_QUERY;
    let ts = tree_sitter_typescript::HIGHLIGHTS_QUERY;
    let (language, queries): (Language, Vec<String>) = match name {
        "rust" => (tree_sitter_rust::LANGUAGE.into(), vec![tree_sitter_rust::HIGHLIGHTS_QUERY.into()]),
        "json" => (tree_sitter_json::LANGUAGE.into(), vec![tree_sitter_json::HIGHLIGHTS_QUERY.into()]),
        "toml" => (tree_sitter_toml_ng::LANGUAGE.into(), vec![tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into()]),
        "python" => (tree_sitter_python::LANGUAGE.into(), vec![tree_sitter_python::HIGHLIGHTS_QUERY.into()]),
        "javascript" => (
            tree_sitter_javascript::LANGUAGE.into(),
            vec![format!("{}\n{}", tree_sitter_javascript::JSX_HIGHLIGHT_QUERY, js), js.into()],
        ),
        // TypeScript's query only adds to JavaScript's; use both, else its own.
        "typescript" => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            vec![format!("{ts}\n{js}"), ts.into()],
        ),
        "tsx" => (tree_sitter_typescript::LANGUAGE_TSX.into(), vec![format!("{ts}\n{js}"), ts.into()]),
        "c" => (tree_sitter_c::LANGUAGE.into(), vec![tree_sitter_c::HIGHLIGHT_QUERY.into()]),
        "go" => (tree_sitter_go::LANGUAGE.into(), vec![tree_sitter_go::HIGHLIGHTS_QUERY.into()]),
        "bash" => (tree_sitter_bash::LANGUAGE.into(), vec![tree_sitter_bash::HIGHLIGHT_QUERY.into()]),
        _ => return load_installed(name),
    };
    let query = queries.iter().find_map(|q| Query::new(&language, q).ok())?;
    Some(LanguageConfig { language, query })
}

/// A run of highlighted characters on one line: columns `start..end`, and
/// the capture name that colors them.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub capture: String,
}

struct Parsed {
    language: &'static str,
    revision: u64,
    tree: Tree,
    text: String,
}

#[derive(Default)]
pub struct Syntax {
    /// Each language's grammar, or None with the install generation it was
    /// missing in (looked for again after an install).
    languages: HashMap<&'static str, (Option<LanguageConfig>, u64)>,
    parsed: HashMap<BufferId, Parsed>,
}

impl Syntax {
    /// Highlight spans for `lines` of a buffer, one list per line. Parses
    /// again only when `revision` changed. Empty when the language has no
    /// grammar here.
    pub fn highlight(
        &mut self,
        buffer: BufferId,
        language: &'static str,
        revision: u64,
        rope: &Rope,
        lines: Range<usize>,
    ) -> Vec<Vec<Span>> {
        // Markdown has its own line scanner (markdown.rs), no parse tree.
        if language == "markdown" {
            return crate::markdown::highlight(rope, lines);
        }
        let empty = || vec![Vec::new(); lines.len()];
        let generation = crate::packages::generation();
        let entry = self.languages.entry(language).or_insert_with(|| (load_language(language), generation));
        if entry.0.is_none() && entry.1 != generation {
            *entry = (load_language(language), generation);
        }
        let Some(config) = entry.0.as_ref() else { return empty() };

        let stale = self
            .parsed
            .get(&buffer)
            .is_none_or(|p| p.revision != revision || p.language != language);
        if stale {
            let text = rope.to_string();
            let mut parser = Parser::new();
            if parser.set_language(&config.language).is_err() {
                return empty();
            }
            let Some(tree) = parser.parse(&text, None) else { return empty() };
            self.parsed.insert(buffer, Parsed { language, revision, tree, text });
        }
        let parsed = &self.parsed[&buffer];
        highlight_lines(&config.query, parsed, rope, lines)
    }

    /// Forgets parse trees of buffers that are gone.
    pub fn retain(&mut self, live: impl Fn(BufferId) -> bool) {
        self.parsed.retain(|id, _| live(*id));
    }
}

fn highlight_lines(query: &Query, parsed: &Parsed, rope: &Rope, lines: Range<usize>) -> Vec<Vec<Span>> {
    let first = lines.start.min(rope.len_lines());
    let last = lines.end.min(rope.len_lines());
    let start_char = rope.line_to_char(first);
    let end_char = if last >= rope.len_lines() { rope.len_chars() } else { rope.line_to_char(last) };
    let (start_byte, end_byte) = (rope.char_to_byte(start_char), rope.char_to_byte(end_char));

    // Every capture in range: (start char, end char, pattern, capture).
    let mut captures: Vec<(usize, usize, usize, u32)> = Vec::new();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(start_byte..end_byte);
    let mut it = cursor.captures(query, parsed.tree.root_node(), parsed.text.as_bytes());
    while let Some((m, index)) = it.next() {
        let c = m.captures[*index];
        if query.capture_names()[c.index as usize].starts_with('_') {
            continue;
        }
        let (a, b) = (c.node.start_byte(), c.node.end_byte());
        if b > a {
            captures.push((rope.byte_to_char(a), rope.byte_to_char(b), m.pattern_index, c.index));
        }
    }
    // Paint the widest first, so nested nodes win over the ones around them;
    // for the same node, the earliest pattern paints last and wins, as in
    // tree-sitter's own highlighter.
    captures.sort_by(|x, y| (y.1 - y.0).cmp(&(x.1 - x.0)).then(y.2.cmp(&x.2)));
    let mut paint: Vec<Option<u32>> = vec![None; end_char - start_char];
    for (a, b, _, capture) in captures {
        let (a, b) = (a.max(start_char) - start_char, b.min(end_char).saturating_sub(start_char));
        for slot in paint.iter_mut().take(b).skip(a) {
            *slot = Some(capture);
        }
    }

    (first..last)
        .map(|line| {
            let line_start = rope.line_to_char(line) - start_char;
            let len = rope.line(line).len_chars();
            let mut spans: Vec<Span> = Vec::new();
            for col in 0..len {
                let Some(Some(capture)) = paint.get(line_start + col) else { continue };
                let name = &query.capture_names()[*capture as usize];
                match spans.last_mut() {
                    Some(s) if s.end == col && s.capture == *name => s.end = col + 1,
                    _ => spans.push(Span { start: col, end: col + 1, capture: name.to_string() }),
                }
            }
            spans
        })
        .collect()
}

/// The face for a capture: the theme entry naming it or its nearest parent
/// (`function.method` falls back to `function`), else None.
pub fn face_for<'a>(capture: &str, theme: &'a [(String, String)]) -> Option<&'a str> {
    let mut name = capture;
    loop {
        if let Some((_, face)) = theme.iter().find(|(c, _)| c == name) {
            return Some(face);
        }
        name = &name[..name.rfind('.')?];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_for(lang: &'static str, source: &str) -> Vec<Vec<Span>> {
        let rope = Rope::from_str(source);
        let mut syntax = Syntax::default();
        let lines = rope.len_lines();
        syntax.highlight(1, lang, 0, &rope, 0..lines)
    }

    fn capture_at(spans: &[Span], col: usize) -> Option<&str> {
        spans.iter().find(|s| col >= s.start && col < s.end).map(|s| s.capture.as_str())
    }

    #[test]
    fn rust_keywords_strings_and_functions() {
        let spans = spans_for("rust", "fn main() {\n    let s = \"hi\";\n}\n");
        assert_eq!(capture_at(&spans[0], 0), Some("keyword"), "{:?}", spans[0]);
        assert_eq!(capture_at(&spans[0], 3).map(|c| c.starts_with("function")), Some(true));
        assert_eq!(capture_at(&spans[1], 4), Some("keyword"));
        assert_eq!(capture_at(&spans[1], 13), Some("string"), "{:?}", spans[1]);
    }

    #[test]
    fn every_language_loads_its_query() {
        for lang in ["rust", "json", "toml", "python", "javascript", "typescript", "tsx", "c", "go", "bash"] {
            assert!(load_language(lang).is_some(), "{lang}");
        }
    }

    #[test]
    fn highlights_only_the_requested_lines() {
        let rope = Rope::from_str("let a = 1;\nlet b = 2;\nlet c = 3;\n");
        let mut syntax = Syntax::default();
        let spans = syntax.highlight(1, "rust", 0, &rope, 1..2);
        assert_eq!(spans.len(), 1);
        assert_eq!(capture_at(&spans[0], 0), Some("keyword"));
    }

    #[test]
    fn languages_by_extension() {
        assert_eq!(language_for_path(Path::new("a/b.rs")), Some("rust"));
        assert_eq!(language_for_path(Path::new("x.tsx")), Some("tsx"));
        assert_eq!(language_for_path(Path::new("README.md")), Some("markdown"));
        assert_eq!(language_for_path(Path::new("App/ContentView.swift")), Some("swift"));
        assert_eq!(language_for_path(Path::new("MainActivity.kt")), Some("kotlin"));
        assert_eq!(language_for_path(Path::new("build.gradle.kts")), Some("kotlin"));
        assert_eq!(language_for_path(Path::new("notes.txt")), None);
    }

    #[test]
    fn faces_fall_back_to_parent_captures() {
        let theme = vec![("function".to_string(), "blue".to_string())];
        assert_eq!(face_for("function.method", &theme), Some("blue"));
        assert_eq!(face_for("function", &theme), Some("blue"));
        assert_eq!(face_for("keyword", &theme), None);
    }
}
