//! The Steel interpreter and the primitives it gets from the Rust core.
//!
//! Rust exposes small, fast primitives (`insert`, `forward-char`, ...). Every
//! command, keymap and the modeline are defined in Scheme (`lisp/core.scm`,
//! then the user's `~/.config/gmacs/init.scm`) and can be redefined while the
//! editor runs.

use crate::buffer::Buffer;
use crate::editor::Editor;
use crate::picker::Picker;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use steel::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

const CORE_EMBEDDED: &str = include_str!("../lisp/core.scm");

/// A Scheme file the editor loads, in order, at start and on every reload.
#[derive(Debug, Clone)]
pub struct Source {
    pub label: String,
    pub path: Option<PathBuf>,
}

impl Source {
    fn read(&self) -> Option<String> {
        match &self.path {
            Some(p) => std::fs::read_to_string(p).ok(),
            None => Some(CORE_EMBEDDED.to_string()),
        }
    }
    fn mtime(&self) -> Option<SystemTime> {
        self.path.as_ref()?.metadata().ok()?.modified().ok()
    }
}

/// One modeline segment: text plus a face name the view maps to a theme color.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub text: String,
    pub face: String,
}

pub struct Lisp {
    engine: Engine,
    editor: Editor,
    sources: Vec<Source>,
    stamps: Vec<Option<SystemTime>>,
    /// The source texts of the last load that succeeded, to fall back on.
    last_good: Option<Vec<String>>,
}

impl Lisp {
    pub fn new(editor: Editor, sources: Vec<Source>) -> Self {
        let mut lisp = Self {
            engine: fresh_engine(&editor),
            editor,
            stamps: vec![None; sources.len()],
            sources,
            last_good: None,
        };
        lisp.reload();
        lisp
    }

    /// The default sources: the core (the repo's `lisp/core.scm` when running
    /// from a checkout, so it live-reloads; else the copy built in), then the
    /// user's init file.
    pub fn default_sources() -> Vec<Source> {
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lisp/core.scm");
        let core = Source {
            label: "core.scm".into(),
            path: dev.exists().then_some(dev),
        };
        let mut out = vec![core];
        if let Some(home) = std::env::var_os("HOME") {
            out.push(Source {
                label: "init.scm".into(),
                path: Some(PathBuf::from(home).join(".config/gmacs/init.scm")),
            });
        }
        out
    }

    /// Re-evaluates every source in order. Redefinitions replace the old ones;
    /// the buffer and its text are untouched. When a source fails to load, the
    /// interpreter is rebuilt from the last texts that loaded, so a bad save
    /// never leaves the editor without its commands; the error is shown.
    pub fn reload(&mut self) {
        let texts: Vec<Option<String>> = self.sources.iter().map(Source::read).collect();
        for (i, src) in self.sources.iter().enumerate() {
            self.stamps[i] = src.mtime();
        }
        match self.load(&texts) {
            Ok(loaded) => {
                self.last_good = Some(texts.into_iter().flatten().collect());
                self.editor.lock().message = format!("loaded {}", loaded.join(", "));
            }
            Err(error) => {
                if let Some(good) = self.last_good.clone() {
                    self.engine = fresh_engine(&self.editor);
                    for code in good {
                        let _ = self.engine.run(code);
                    }
                    self.editor.lock().message = format!("{error} (kept the last version that loaded)");
                } else {
                    self.editor.lock().message = error;
                }
            }
        }
    }

    fn load(&mut self, texts: &[Option<String>]) -> Result<Vec<String>, String> {
        let mut loaded = Vec::new();
        for (src, code) in self.sources.iter().zip(texts) {
            let Some(code) = code else { continue };
            self.engine.run(code.clone()).map_err(|e| format!("{}: {e}", src.label))?;
            loaded.push(src.label.clone());
        }
        Ok(loaded)
    }

    /// Reloads when any source file changed since the last load.
    pub fn reload_if_changed(&mut self) -> bool {
        let changed = self
            .sources
            .iter()
            .zip(&self.stamps)
            .any(|(s, old)| s.mtime() != *old);
        if changed {
            self.reload();
        }
        changed
    }

    /// Hands a keystroke to Scheme's `dispatch-key`. `key` is Emacs notation
    /// (`"C-x"`, `"M-:"`, `"RET"`); `text` is what it would type, or "".
    pub fn dispatch_key(&mut self, key: &str, text: &str) {
        let args = vec![SteelVal::from(key.to_string()), SteelVal::from(text.to_string())];
        if let Err(e) = self.engine.call_function_by_name_with_args("dispatch-key", args) {
            self.editor.lock().message = format!("{key}: {e}");
        }
        self.run_pending_evals();
    }

    /// Evaluates source and returns its printed result.
    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        self.engine
            .run(code.to_string())
            .map(|vals| vals.last().map(|v| v.to_string()).unwrap_or_default())
            .map_err(|e| e.to_string())
    }

    fn run_pending_evals(&mut self) {
        let pending = std::mem::take(&mut self.editor.lock().pending_eval);
        for code in pending {
            let msg = match self.eval(&code) {
                Ok(v) => v,
                Err(e) => format!("error: {e}"),
            };
            self.editor.lock().message = msg;
        }
    }

    /// Calls Scheme's `(cursor-shape)`: "box" or "bar".
    pub fn cursor_shape(&mut self) -> String {
        match self.engine.call_function_by_name_with_args("cursor-shape", vec![]) {
            Ok(SteelVal::SymbolV(s)) | Ok(SteelVal::StringV(s)) => s.to_string(),
            _ => "box".into(),
        }
    }

    /// Calls Scheme's `(selection)`: the selected range `[start, end)` in
    /// characters, or None when nothing is selected.
    pub fn selection(&mut self) -> Option<(usize, usize)> {
        let v = self.engine.call_function_by_name_with_args("selection", vec![]).ok()?;
        let SteelVal::ListV(items) = v else { return None };
        let mut it = items.iter();
        match (it.next()?, it.next()?) {
            (SteelVal::IntV(a), SteelVal::IntV(b)) if a < b => Some((*a as usize, *b as usize)),
            _ => None,
        }
    }

    /// Calls Scheme's `(search-highlight)`: the pattern whose matches the
    /// window highlights, or "" for none.
    pub fn search_highlight(&mut self) -> String {
        match self.engine.call_function_by_name_with_args("search-highlight", vec![]) {
            Ok(SteelVal::StringV(s)) => s.to_string(),
            _ => String::new(),
        }
    }

    /// Calls Scheme's `(syntax-theme)`: (capture face) pairs saying how
    /// tree-sitter captures look.
    pub fn syntax_theme(&mut self) -> Vec<(String, String)> {
        match self.engine.call_function_by_name_with_args("syntax-theme", vec![]) {
            Ok(v) => parse_segments(&v).into_iter().map(|s| (s.text, s.face)).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Calls Scheme's `(modeline)`: a list of `(text face)` pairs.
    pub fn modeline(&mut self) -> Vec<Segment> {
        match self.engine.call_function_by_name_with_args("modeline", vec![]) {
            Ok(v) => parse_segments(&v),
            Err(e) => vec![Segment {
                text: format!("modeline: {e}"),
                face: "error".into(),
            }],
        }
    }
}

fn parse_segments(v: &SteelVal) -> Vec<Segment> {
    let SteelVal::ListV(items) = v else { return vec![] };
    items
        .iter()
        .filter_map(|item| {
            let SteelVal::ListV(pair) = item else { return None };
            let mut it = pair.iter();
            let text = match it.next()? {
                SteelVal::StringV(s) => s.to_string(),
                other => other.to_string(),
            };
            let face = match it.next() {
                Some(SteelVal::SymbolV(s)) | Some(SteelVal::StringV(s)) => s.to_string(),
                _ => "default".to_string(),
            };
            Some(Segment { text, face })
        })
        .collect()
}

/// Registers a primitive that gets the editor. `$args` are the Scheme
/// arguments with their Rust types.
macro_rules! prim {
    ($engine:ident, $editor:ident, $name:literal, |$st:ident $(, $a:ident : $t:ty)*| $body:expr) => {{
        let ed = $editor.clone();
        $engine.register_fn($name, move |$($a: $t),*| {
            #[allow(unused_mut)]
            let mut $st = ed.lock();
            $body
        });
    }};
}

fn fresh_engine(editor: &Editor) -> Engine {
    let mut engine = Engine::new();
    register_primitives(&mut engine, editor);
    engine
}

fn register_primitives(engine: &mut Engine, editor: &Editor) {
    // Editing the current buffer (the minibuffer while it is open).
    prim!(engine, editor, "insert", |s, text: String| s.current().insert(&text));
    prim!(engine, editor, "delete-backward-char", |s, n: isize| s
        .current()
        .delete_backward(n.max(0) as usize));
    prim!(engine, editor, "delete-char", |s, n: isize| s
        .current()
        .delete_forward(n.max(0) as usize));
    prim!(engine, editor, "forward-char", |s, n: isize| s.current().forward_char(n));
    prim!(engine, editor, "forward-line", |s, n: isize| s.current().forward_line(n));
    prim!(engine, editor, "beginning-of-line", |s| s.current().beginning_of_line());
    prim!(engine, editor, "end-of-line", |s| s.current().end_of_line());
    prim!(engine, editor, "goto-char", |s, pos: isize| s.current().goto_char(pos.max(0) as usize));
    prim!(engine, editor, "point", |s| s.current().point() as isize);
    prim!(engine, editor, "point-max", |s| s.current().len() as isize);
    prim!(engine, editor, "char-at", |s, pos: isize| s.current().char_at(pos.max(0) as usize));
    prim!(engine, editor, "buffer-substring", |s, a: isize, b: isize| s
        .current()
        .substring(a.max(0) as usize, b.max(0) as usize));
    prim!(engine, editor, "line-beginning-position", |s, pos: isize| s
        .current()
        .line_start(pos.max(0) as usize) as isize);
    prim!(engine, editor, "line-end-position", |s, pos: isize| s
        .current()
        .line_end(pos.max(0) as usize) as isize);
    prim!(engine, editor, "delete-region", |s, a: isize, b: isize| s
        .current()
        .delete_range(a.max(0) as usize, b.max(0) as usize));
    prim!(engine, editor, "char-class", |s, pos: isize| s
        .current()
        .char_class(pos.max(0) as usize)
        .to_string());
    prim!(engine, editor, "pos->line", |s, pos: isize| s.current().line_of(pos.max(0) as usize) as isize);
    prim!(engine, editor, "line->pos", |s, line: isize| s
        .current()
        .line_to_pos(line.max(0) as usize) as isize);
    prim!(engine, editor, "line-count", |s| s.current().line_count() as isize);
    // Search, always in the file buffer: regex, smartcase. -1 when nothing matches.
    prim!(engine, editor, "search-forward", |s, pattern: String, from: isize| {
        let Some(re) = s.search_regex(&pattern) else { return -1 };
        s.buffer().search_forward(&re, from.max(0) as usize).map_or(-1, |p| p as isize)
    });
    prim!(engine, editor, "search-backward", |s, pattern: String, before: isize| {
        let Some(re) = s.search_regex(&pattern) else { return -1 };
        s.buffer().search_backward(&re, before.max(0) as usize).map_or(-1, |p| p as isize)
    });
    engine.register_fn("regex-quote", |text: String| regex::escape(&text));
    // The fuzzy picker. Scheme opens it and decides what picking does.
    prim!(engine, editor, "project-root", |s| {
        let start = s
            .buffer()
            .file()
            .cloned()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        crate::picker::project_root(&start).display().to_string()
    });
    engine.register_fn("project-files", |root: String| crate::picker::project_files(Path::new(&root)));
    prim!(engine, editor, "picker-open", |s, title: String, items: Vec<String>, root: String| {
        let root = (!root.is_empty()).then(|| PathBuf::from(root));
        s.picker = Some(Picker::new(title, items, root));
    });
    prim!(engine, editor, "picker-close", |s| s.picker = None);
    prim!(engine, editor, "picker-active?", |s| s.picker.is_some());
    prim!(engine, editor, "picker-filter", |s, query: String| {
        if let Some(p) = s.picker.as_mut() {
            p.filter(&query);
        }
    });
    prim!(engine, editor, "picker-move", |s, n: isize| {
        if let Some(p) = s.picker.as_mut() {
            p.move_selection(n);
        }
    });
    prim!(engine, editor, "picker-select", |s, index: isize| {
        if let Some(p) = s.picker.as_mut() {
            p.select(index);
        }
    });
    prim!(engine, editor, "picker-selection", |s| s
        .picker
        .as_ref()
        .and_then(|p| p.selection().map(str::to_string))
        .unwrap_or_default());
    prim!(engine, editor, "undo-boundary", |s| s.current().undo_boundary());
    prim!(engine, editor, "undo", |s| s.current().undo());
    prim!(engine, editor, "redo", |s| s.current().redo());
    // The register: the last deleted or yanked text.
    prim!(engine, editor, "register-set", |s, text: String| s.kill_ring.push(text));
    prim!(engine, editor, "register-get", |s| s.kill_ring.last().cloned().unwrap_or_default());
    prim!(engine, editor, "buffer-string", |s| s.current().text().to_string());

    // About the file buffer (not the minibuffer).
    prim!(engine, editor, "buffer-name", |s| s.buffer().name().to_string());
    prim!(engine, editor, "buffer-file-name", |s| s
        .buffer()
        .file()
        .map(|p| p.display().to_string())
        .unwrap_or_default());
    prim!(engine, editor, "buffer-modified?", |s| s.buffer().is_modified());
    prim!(engine, editor, "buffer-language", |s| s
        .buffer()
        .file()
        .and_then(|f| crate::syntax::language_for_path(f))
        .unwrap_or("text")
        .to_string());
    prim!(engine, editor, "line-number", |s| s.buffer().line() as isize + 1);
    prim!(engine, editor, "column-number", |s| s.buffer().column() as isize);
    prim!(engine, editor, "find-file", |s, path: String| {
        let path = expand_home(&path);
        s.message = match s.open_file(&path) {
            Ok(()) => format!("opened {}", path.display()),
            Err(e) => format!("{}: {e}", path.display()),
        };
    });
    prim!(engine, editor, "save-buffer", |s| {
        s.message = match s.buffer_mut().save() {
            Ok(p) => format!("wrote {}", p.display()),
            Err(e) => format!("not saved: {e}"),
        };
    });

    // Buffers.
    prim!(engine, editor, "buffer-list", |s| s.buffer_names());
    prim!(engine, editor, "switch-to-buffer", |s, name: String| s.switch_to_buffer(&name));
    prim!(engine, editor, "next-buffer", |s, n: isize| s.next_buffer(n));
    prim!(engine, editor, "new-buffer", |s, name: String| {
        let id = s.add_buffer(Buffer::new(name));
        s.show_buffer(id);
    });
    prim!(engine, editor, "kill-buffer", |s, force: bool| {
        s.message = match s.kill_buffer(force) {
            Ok(name) => format!("closed {name}"),
            Err(e) => e,
        };
    });

    // Windows: split, move between, close; scrolling.
    prim!(engine, editor, "split-window", |s, side: String| s.split_window(
        if side == "below" { crate::editor::Axis::Column } else { crate::editor::Axis::Row }
    ));
    prim!(engine, editor, "window-in-direction?", |s, dir: String| s.window_in_direction(&dir).is_some());
    prim!(engine, editor, "select-window-in-direction", |s, dir: String| {
        match s.window_in_direction(&dir) {
            Some(id) => {
                s.select_window(id);
                true
            }
            None => false,
        }
    });
    prim!(engine, editor, "other-window", |s, n: isize| s.other_window(n));
    prim!(engine, editor, "delete-window", |s| s.close_window());
    prim!(engine, editor, "delete-other-windows", |s| s.only_window());
    prim!(engine, editor, "window-count", |s| s.window_count() as isize);
    prim!(engine, editor, "window-height", |s| s.window().rows.max(1) as isize);
    prim!(engine, editor, "window-top-line", |s| s.window().scroll_top as isize);
    prim!(engine, editor, "scroll-window", |s, n: isize| s.scroll(n));
    prim!(engine, editor, "recenter", |s, place: String| s.recenter(&place));

    // The echo area and minibuffer.
    prim!(engine, editor, "message", |s, text: String| s.message = text);
    prim!(engine, editor, "current-message", |s| s.message.clone());
    prim!(engine, editor, "minibuffer-open", |s, prompt: String| {
        s.minibuffer.set_text("");
        s.minibuffer_prompt = Some(prompt);
    });
    prim!(engine, editor, "minibuffer-close", |s| s.minibuffer_prompt = None);
    prim!(engine, editor, "minibuffer-active?", |s| s.minibuffer_prompt.is_some());
    prim!(engine, editor, "minibuffer-contents", |s| s.minibuffer.text().to_string());

    // Runs Scheme source after the current command (see `pending_eval`).
    prim!(engine, editor, "eval-string", |s, code: String| s.pending_eval.push(code));
    prim!(engine, editor, "quit-editor", |s| s.quit_requested = true);
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lisp_with(text: &str) -> (Lisp, Editor) {
        let mut buf = Buffer::new("test");
        buf.insert(text);
        buf.goto_char(0);
        let editor = Editor::new(buf);
        let core = Source { label: "core.scm".into(), path: None };
        (Lisp::new(editor.clone(), vec![core]), editor)
    }

    fn keys(lisp: &mut Lisp, seq: &[(&str, &str)]) {
        for (k, t) in seq {
            lisp.dispatch_key(k, t);
        }
    }

    fn text(editor: &Editor) -> String {
        editor.lock().buffer().text().to_string()
    }

    #[test]
    fn core_loads() {
        let (_lisp, editor) = lisp_with("");
        assert_eq!(editor.lock().message, "loaded core.scm");
    }

    fn typed(lisp: &mut Lisp, text: &str) {
        for c in text.chars() {
            let s = c.to_string();
            let key = match c { ' ' => "SPC".to_string(), '\n' => "RET".to_string(), _ => s.clone() };
            lisp.dispatch_key(&key, if c == '\n' { "" } else { &s });
        }
    }

    fn point(editor: &Editor) -> usize {
        editor.lock().buffer().point()
    }

    #[test]
    fn starts_in_normal_state_where_keys_do_not_type() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, "q");
        assert_eq!(text(&editor), "");
        assert_eq!(lisp.cursor_shape(), "box");
    }

    #[test]
    fn i_types_and_escape_returns_to_normal() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, "ihi\nyo");
        assert_eq!(lisp.cursor_shape(), "bar");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "hi\nyo");
        assert_eq!(lisp.cursor_shape(), "box");
        assert_eq!(point(&editor), 4, "escape steps back onto the last character");
    }

    #[test]
    fn hjkl_motions_and_counts() {
        let (mut lisp, editor) = lisp_with("abcdef\nabcdef\nabcdef");
        typed(&mut lisp, "3l");
        assert_eq!(point(&editor), 3);
        typed(&mut lisp, "2j");
        assert_eq!(editor.lock().buffer().line(), 2);
        typed(&mut lisp, "0");
        assert_eq!(editor.lock().buffer().column(), 0);
        typed(&mut lisp, "$");
        assert_eq!(editor.lock().buffer().column(), 5);
        typed(&mut lisp, "gg");
        assert_eq!(point(&editor), 0);
        typed(&mut lisp, "G");
        assert_eq!(editor.lock().buffer().line(), 2);
    }

    #[test]
    fn w_and_b_move_by_words() {
        let (mut lisp, editor) = lisp_with("foo bar  baz");
        typed(&mut lisp, "w");
        assert_eq!(point(&editor), 4);
        typed(&mut lisp, "w");
        assert_eq!(point(&editor), 9);
        typed(&mut lisp, "b");
        assert_eq!(point(&editor), 4);
    }

    #[test]
    fn dd_p_and_undo() {
        let (mut lisp, editor) = lisp_with("one\ntwo\nthree");
        typed(&mut lisp, "dd");
        assert_eq!(text(&editor), "two\nthree");
        typed(&mut lisp, "p");
        assert_eq!(text(&editor), "two\none\nthree");
        typed(&mut lisp, "u");
        assert_eq!(text(&editor), "two\nthree");
        typed(&mut lisp, "u");
        assert_eq!(text(&editor), "one\ntwo\nthree");
        lisp.dispatch_key("C-r", "");
        assert_eq!(text(&editor), "two\nthree");
    }

    #[test]
    fn x_o_and_a_insert_session_is_one_undo_step() {
        let (mut lisp, editor) = lisp_with("abc");
        typed(&mut lisp, "x");
        assert_eq!(text(&editor), "bc");
        typed(&mut lisp, "onew");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "bc\nnew");
        typed(&mut lisp, "u");
        assert_eq!(text(&editor), "bc");
    }

    #[test]
    fn cw_changes_a_word() {
        let (mut lisp, editor) = lisp_with("foo bar");
        typed(&mut lisp, "cwbaz");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "baz bar");
    }

    #[test]
    fn prefix_keys_wait_for_the_next_key() {
        let (mut lisp, editor) = lisp_with("");
        lisp.dispatch_key("SPC", " ");
        assert_eq!(editor.lock().message, "SPC-");
        lisp.dispatch_key("z", "z");
        assert_eq!(editor.lock().message, "SPC z is undefined");
    }

    #[test]
    fn redefining_a_command_changes_its_key() {
        let (mut lisp, editor) = lisp_with("abc");
        lisp.eval(r#"(defcommand (delete-char-under) (insert "!"))"#).unwrap();
        typed(&mut lisp, "x");
        assert_eq!(text(&editor), "!abc");
    }

    #[test]
    fn binding_a_key_at_runtime() {
        let (mut lisp, editor) = lisp_with("");
        lisp.eval(r#"(define-key normal-map "Q" (lambda () (insert "tea")))"#).unwrap();
        typed(&mut lisp, "Q");
        assert_eq!(text(&editor), "tea");
    }

    #[test]
    fn ex_commands_from_colon() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, ":eval (+ 1 2)\n");
        assert!(editor.lock().minibuffer_prompt.is_none());
        assert_eq!(editor.lock().message, "3");
        typed(&mut lisp, ":nope\n");
        assert_eq!(editor.lock().message, "Not an editor command: nope");
    }

    #[test]
    fn spc_colon_runs_a_command_by_name() {
        let (mut lisp, editor) = lisp_with("abc");
        typed(&mut lisp, " :delete-char-under\n");
        assert_eq!(text(&editor), "bc");
    }

    #[test]
    fn spc_semicolon_evaluates_scheme() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, " ;(* 6 7)\n");
        assert_eq!(text(&editor), "", "typing went to the minibuffer");
        assert_eq!(editor.lock().message, "42");
    }

    #[test]
    fn escape_cancels_the_minibuffer() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, ":");
        lisp.dispatch_key("ESC", "");
        assert!(editor.lock().minibuffer_prompt.is_none());
        typed(&mut lisp, "x");
        assert_eq!(text(&editor), "", "back in normal state");
    }

    fn at(lisp: &mut Lisp, editor: &Editor, pos: usize) {
        editor.lock().buffer_mut().goto_char(pos);
        let _ = lisp;
    }

    #[test]
    fn operators_take_motions() {
        let (mut lisp, editor) = lisp_with("one two three\nfour\nfive");
        typed(&mut lisp, "dw");
        assert_eq!(text(&editor), "two three\nfour\nfive");
        typed(&mut lisp, "d2w");
        assert_eq!(text(&editor), "\nfour\nfive", "dw stops at the line's end");
        typed(&mut lisp, "u");
        typed(&mut lisp, "de");
        assert_eq!(text(&editor), " three\nfour\nfive");
        typed(&mut lisp, "dj");
        assert_eq!(text(&editor), "five");
        typed(&mut lisp, "u");
        typed(&mut lisp, "jdG");
        assert_eq!(text(&editor), " three");
    }

    #[test]
    fn counts_before_and_after_operators() {
        let (mut lisp, editor) = lisp_with("a\nb\nc\nd\ne");
        typed(&mut lisp, "2dd");
        assert_eq!(text(&editor), "c\nd\ne");
        typed(&mut lisp, "d2j");
        assert_eq!(text(&editor), "");
    }

    #[test]
    fn w_stops_at_punctuation() {
        let (mut lisp, editor) = lisp_with("foo.bar baz");
        typed(&mut lisp, "w");
        assert_eq!(point(&editor), 3);
        typed(&mut lisp, "w");
        assert_eq!(point(&editor), 4);
        typed(&mut lisp, "0W");
        assert_eq!(point(&editor), 8);
    }

    #[test]
    fn text_objects_words() {
        let (mut lisp, editor) = lisp_with("say hello there");
        at(&mut lisp, &editor, 6);
        typed(&mut lisp, "diw");
        assert_eq!(text(&editor), "say  there");
        typed(&mut lisp, "u");
        at(&mut lisp, &editor, 6);
        typed(&mut lisp, "daw");
        assert_eq!(text(&editor), "say there");
    }

    #[test]
    fn text_objects_quotes_and_brackets() {
        let (mut lisp, editor) = lisp_with(r#"f(a, g(b), "x y") end"#);
        at(&mut lisp, &editor, 13);
        typed(&mut lisp, "ci\"");
        typed(&mut lisp, "z");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), r#"f(a, g(b), "z") end"#);
        at(&mut lisp, &editor, 7);
        typed(&mut lisp, "di(");
        assert_eq!(text(&editor), r#"f(a, g(), "z") end"#);
        at(&mut lisp, &editor, 3);
        typed(&mut lisp, "da(");
        assert_eq!(text(&editor), "f end");
        typed(&mut lisp, "u");
        at(&mut lisp, &editor, 3);
        typed(&mut lisp, "yib");
        typed(&mut lisp, "$p");
        assert_eq!(text(&editor), r#"f(a, g(), "z") enda, g(), "z""#);
    }

    #[test]
    fn nested_brackets_find_the_enclosing_pair() {
        let (mut lisp, editor) = lisp_with("{ a { b } c }");
        at(&mut lisp, &editor, 10);
        typed(&mut lisp, "di{");
        assert_eq!(text(&editor), "{}");
    }

    #[test]
    fn paragraph_object() {
        let (mut lisp, editor) = lisp_with("a\nb\n\nc\nd");
        typed(&mut lisp, "dap");
        assert_eq!(text(&editor), "c\nd");
    }

    #[test]
    fn visual_charwise_selects_inclusively() {
        let (mut lisp, editor) = lisp_with("abcdef");
        at(&mut lisp, &editor, 1);
        typed(&mut lisp, "vll");
        assert_eq!(lisp.selection(), Some((1, 4)));
        typed(&mut lisp, "d");
        assert_eq!(text(&editor), "aef");
        assert_eq!(lisp.selection(), None, "back in normal state");
    }

    #[test]
    fn visual_line_and_swap_ends() {
        let (mut lisp, editor) = lisp_with("one\ntwo\nthree\nfour");
        typed(&mut lisp, "jVj");
        assert_eq!(lisp.selection(), Some((4, 14)));
        typed(&mut lisp, "o");
        assert_eq!(editor.lock().buffer().line(), 1);
        typed(&mut lisp, "d");
        assert_eq!(text(&editor), "one\nfour");
    }

    #[test]
    fn visual_yank_change_and_objects() {
        let (mut lisp, editor) = lisp_with("alpha beta");
        typed(&mut lisp, "veyP");
        assert_eq!(text(&editor), "alphaalpha beta");
        typed(&mut lisp, "u");
        at(&mut lisp, &editor, 7);
        typed(&mut lisp, "viw");
        assert_eq!(lisp.selection(), Some((6, 10)));
        typed(&mut lisp, "cgamma");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "alpha gamma");
    }

    #[test]
    fn visual_escape_and_toggle_leave() {
        let (mut lisp, _editor) = lisp_with("abc");
        typed(&mut lisp, "vl");
        lisp.dispatch_key("ESC", "");
        assert_eq!(lisp.selection(), None);
        typed(&mut lisp, "vlv");
        assert_eq!(lisp.selection(), None);
    }

    #[test]
    fn indent_and_outdent_lines() {
        let (mut lisp, editor) = lisp_with("a\nb");
        typed(&mut lisp, ">j");
        assert_eq!(text(&editor), "    a\n    b");
        typed(&mut lisp, "<<");
        assert_eq!(text(&editor), "a\n    b");
    }

    #[test]
    fn escape_cancels_a_pending_operator() {
        let (mut lisp, editor) = lisp_with("abc");
        typed(&mut lisp, "d");
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "l");
        assert_eq!(text(&editor), "abc");
        assert_eq!(point(&editor), 1);
    }

    #[test]
    fn readme_custom_motion_example() {
        let (mut lisp, editor) = lisp_with("a\nb\n\nc");
        lisp.eval(r#"
(define-motion 'next-blank-line 'linewise
  (lambda (p) (let loop ((l (+ (pos->line p) 1)))
                (cond ((>= l (line-count)) (line->pos (- (line-count) 1)))
                      ((empty-line? (line->pos l)) (line->pos l))
                      (else (loop (+ l 1)))))))
(define-key normal-map "}" (lambda () (move-by 'next-blank-line)))
(define-key operator-map "}" (operator-motion 'next-blank-line))
"#).unwrap();
        typed(&mut lisp, "}");
        assert_eq!(editor.lock().buffer().line(), 2);
        typed(&mut lisp, "ggd}");
        assert_eq!(text(&editor), "c");
    }

    #[test]
    fn f_t_and_repeat() {
        let (mut lisp, editor) = lisp_with("a,b,c,d");
        typed(&mut lisp, "f,");
        assert_eq!(point(&editor), 1);
        typed(&mut lisp, ";");
        assert_eq!(point(&editor), 3);
        typed(&mut lisp, ",");
        assert_eq!(point(&editor), 1);
        typed(&mut lisp, "0t,");
        assert_eq!(point(&editor), 0, "t stops before an adjacent match");
        typed(&mut lisp, ";");
        assert_eq!(point(&editor), 2, "; after t moves on to the next");
        typed(&mut lisp, "$Fb");
        assert_eq!(point(&editor), 2);
        typed(&mut lisp, "Ta");
        assert_eq!(point(&editor), 1);
        typed(&mut lisp, "fz");
        assert_eq!(point(&editor), 1, "no such character: no move");
    }

    #[test]
    fn f_t_with_counts_and_operators() {
        let (mut lisp, editor) = lisp_with("call(a, b) + x");
        typed(&mut lisp, "2f,");
        assert_eq!(point(&editor), 0, "only one comma: no move");
        typed(&mut lisp, "dt(");
        assert_eq!(text(&editor), "(a, b) + x");
        typed(&mut lisp, "lcf,z");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "(z b) + x");
        typed(&mut lisp, "0dfq");
        assert_eq!(text(&editor), "(z b) + x", "failed f cancels the operator");
        typed(&mut lisp, "vt)d");
        assert_eq!(text(&editor), ") + x");
    }

    #[test]
    fn search_forward_backward_and_wrap() {
        let (mut lisp, editor) = lisp_with("foo bar\nbaz foo\nqux FOO");
        typed(&mut lisp, "/foo\n");
        assert_eq!(point(&editor), 12);
        assert_eq!(lisp.search_highlight(), "foo");
        typed(&mut lisp, "n");
        assert_eq!(point(&editor), 20, "smartcase: lowercase matches FOO");
        typed(&mut lisp, "n");
        assert_eq!(point(&editor), 0);
        assert_eq!(editor.lock().message, "search wrapped");
        typed(&mut lisp, "N");
        assert_eq!(point(&editor), 20);
        typed(&mut lisp, "?ba\n");
        assert_eq!(point(&editor), 8);
        typed(&mut lisp, "n");
        assert_eq!(point(&editor), 4, "n keeps ?'s direction");
        typed(&mut lisp, "/FOO\n");
        assert_eq!(point(&editor), 20, "a capital makes it case-sensitive");
    }

    #[test]
    fn search_regex_and_not_found() {
        let (mut lisp, editor) = lisp_with("x1 y22 z333");
        typed(&mut lisp, "/\\d{3}\n");
        assert_eq!(point(&editor), 8);
        typed(&mut lisp, "/nope\n");
        assert_eq!(point(&editor), 8);
        assert_eq!(editor.lock().message, "Pattern not found: nope");
    }

    #[test]
    fn search_highlights_while_typing_and_esc_clears() {
        let (mut lisp, editor) = lisp_with("abc abd");
        typed(&mut lisp, "/ab");
        assert_eq!(lisp.search_highlight(), "ab");
        assert_eq!(point(&editor), 0, "typing doesn't move yet");
        lisp.dispatch_key("ESC", "");
        assert_eq!(lisp.search_highlight(), "");
        typed(&mut lisp, "/abd\n");
        assert_eq!(lisp.search_highlight(), "abd");
        typed(&mut lisp, ":noh\n");
        assert_eq!(lisp.search_highlight(), "");
        typed(&mut lisp, "gg/abd\n");
        lisp.dispatch_key("ESC", "");
        assert_eq!(lisp.search_highlight(), "");
    }

    #[test]
    fn star_searches_the_word_and_operators_take_search() {
        let (mut lisp, editor) = lisp_with("foo food foo.x");
        typed(&mut lisp, "*");
        assert_eq!(point(&editor), 9, "whole word: skips food");
        typed(&mut lisp, "#");
        assert_eq!(point(&editor), 0);
        typed(&mut lisp, "d/food\n");
        assert_eq!(text(&editor), "food foo.x");
        typed(&mut lisp, "/foo\n");
        assert_eq!(point(&editor), 5);
        typed(&mut lisp, "0dn");
        assert_eq!(text(&editor), "foo.x", "dn deletes up to the next match");
    }

    #[test]
    fn spc_f_f_opens_a_picker_over_the_project() {
        let (mut lisp, editor) = lisp_with("");
        let readme = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
        editor.lock().open_file(&readme).unwrap();
        typed(&mut lisp, " ff");
        {
            let st = editor.lock();
            let p = st.picker.as_ref().expect("picker open");
            assert_eq!(p.title(), "Find file");
            assert!(p.total() > 5);
        }
        typed(&mut lisp, "corescm");
        assert_eq!(editor.lock().picker.as_ref().unwrap().selection(), Some("lisp/core.scm"));
        lisp.dispatch_key("RET", "");
        let st = editor.lock();
        assert!(st.picker.is_none(), "picking closes it");
        assert_eq!(st.buffer().name(), "core.scm");
    }

    #[test]
    fn picker_keys_move_and_escape_closes() {
        let (mut lisp, editor) = lisp_with("");
        lisp.eval(r#"(pick "Fruit" (list "apple" "banana" "cherry") "" (lambda (x) (message x)))"#).unwrap();
        lisp.dispatch_key("C-n", "");
        lisp.dispatch_key("<down>", "");
        lisp.dispatch_key("C-p", "");
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().message, "banana");
        lisp.eval(r#"(pick "Fruit" (list "apple" "banana") "" (lambda (x) (message x)))"#).unwrap();
        lisp.dispatch_key("ESC", "");
        assert!(editor.lock().picker.is_some(), "the first ESC leaves typing");
        lisp.dispatch_key("ESC", "");
        assert!(editor.lock().picker.is_none());
        assert!(editor.lock().minibuffer_prompt.is_none());
        typed(&mut lisp, "x");
        assert_eq!(text(&editor), "", "back in normal state");
    }

    #[test]
    fn a_broken_reload_keeps_the_last_good_version() {
        let dir = std::env::temp_dir().join(format!("gmacs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let init = dir.join("init.scm");
        std::fs::write(&init, "(defcommand (hi) (insert \"hi\"))").unwrap();
        let editor = Editor::new(Buffer::new("t"));
        let sources = vec![
            Source { label: "core.scm".into(), path: None },
            Source { label: "init.scm".into(), path: Some(init.clone()) },
        ];
        let mut lisp = Lisp::new(editor.clone(), sources);
        std::fs::write(&init, "(define (broken) (no-such-function))").unwrap();
        lisp.reload();
        assert!(editor.lock().message.contains("kept the last version"), "{}", editor.lock().message);
        typed(&mut lisp, ":hi\n");
        assert_eq!(text(&editor), "hi", "keys and the old init still work");
        assert!(!lisp.modeline().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn set_rows(editor: &Editor, rows: usize) {
        let mut st = editor.lock();
        let id = st.selected_window();
        st.fit_window(id, rows);
    }

    #[test]
    fn ctrl_d_and_ctrl_u_scroll_half_a_window() {
        let lines: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let (mut lisp, editor) = lisp_with(&lines);
        set_rows(&editor, 20);
        lisp.dispatch_key("C-d", "");
        assert_eq!(editor.lock().buffer().line(), 10);
        assert_eq!(editor.lock().window().scroll_top, 10);
        lisp.dispatch_key("C-d", "");
        assert_eq!(editor.lock().buffer().line(), 20);
        lisp.dispatch_key("C-u", "");
        assert_eq!(editor.lock().buffer().line(), 10);
        lisp.dispatch_key("C-f", "");
        assert_eq!(editor.lock().buffer().line(), 28);
        lisp.dispatch_key("C-b", "");
        assert_eq!(editor.lock().buffer().line(), 10);
        typed(&mut lisp, "zt");
        assert_eq!(editor.lock().window().scroll_top, 10);
        typed(&mut lisp, "zz");
        assert_eq!(editor.lock().window().scroll_top, 0);
    }

    #[test]
    fn ctrl_e_scrolls_and_drags_the_cursor_along() {
        let lines: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let (mut lisp, editor) = lisp_with(&lines);
        set_rows(&editor, 20);
        lisp.dispatch_key("C-e", "");
        assert_eq!(editor.lock().window().scroll_top, 1);
        assert_eq!(editor.lock().buffer().line(), 1, "the cursor left the top, so it follows");
        lisp.dispatch_key("C-y", "");
        assert_eq!(editor.lock().window().scroll_top, 0);
        assert_eq!(editor.lock().buffer().line(), 1);
    }

    #[test]
    fn splits_and_moving_between_windows() {
        let (mut lisp, editor) = lisp_with("one\ntwo");
        lisp.dispatch_key("C-w", "");
        lisp.dispatch_key("v", "v");
        assert_eq!(editor.lock().window_count(), 2);
        let right = editor.lock().selected_window();
        typed(&mut lisp, "j");
        lisp.dispatch_key("C-w", "");
        lisp.dispatch_key("h", "h");
        assert_ne!(editor.lock().selected_window(), right);
        assert_eq!(editor.lock().buffer().line(), 0, "the left window kept its cursor");
        typed(&mut lisp, " wl");
        assert_eq!(editor.lock().selected_window(), right);
        assert_eq!(editor.lock().buffer().line(), 1);
        typed(&mut lisp, " ws");
        assert_eq!(editor.lock().window_count(), 3);
        typed(&mut lisp, ":q\n");
        assert_eq!(editor.lock().window_count(), 2, ":q closes a window");
        assert!(!editor.lock().quit_requested);
        typed(&mut lisp, ":only\n");
        assert_eq!(editor.lock().window_count(), 1);
        typed(&mut lisp, ":q\n");
        assert!(editor.lock().quit_requested, ":q in the last window quits");
    }

    #[test]
    fn buffers_open_switch_and_close() {
        let (mut lisp, editor) = lisp_with("first");
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let cargo = root.join("Cargo.toml").display().to_string();
        let readme = root.join("README.md").display().to_string();
        typed(&mut lisp, &format!(":e {cargo}\n"));
        typed(&mut lisp, &format!(":e {readme}\n"));
        assert_eq!(editor.lock().buffer_names(), vec!["test", "Cargo.toml", "README.md"]);
        assert_eq!(editor.lock().buffer().name(), "README.md");
        typed(&mut lisp, ":bp\n");
        assert_eq!(editor.lock().buffer().name(), "Cargo.toml");
        typed(&mut lisp, "]b");
        assert_eq!(editor.lock().buffer().name(), "README.md");
        typed(&mut lisp, &format!(":e {cargo}\n"));
        assert_eq!(editor.lock().buffer_names().len(), 3, "an open file isn't opened twice");
        typed(&mut lisp, " ,test\n");
        assert_eq!(editor.lock().buffer().name(), "test");
        typed(&mut lisp, "x");
        typed(&mut lisp, ":bd\n");
        assert!(editor.lock().message.contains("unsaved"), "{}", editor.lock().message);
        typed(&mut lisp, ":bd!\n");
        assert_eq!(editor.lock().buffer_names(), vec!["Cargo.toml", "README.md"]);
    }

    #[test]
    fn vsplit_opens_a_file_beside() {
        let (mut lisp, editor) = lisp_with("first");
        let cargo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        typed(&mut lisp, &format!(":vs {}\n", cargo.display()));
        let st = editor.lock();
        assert_eq!(st.window_count(), 2);
        assert_eq!(st.buffer().name(), "Cargo.toml");
    }

    #[test]
    fn syntax_theme_comes_from_scheme() {
        let (mut lisp, _editor) = lisp_with("");
        let theme = lisp.syntax_theme();
        assert!(theme.iter().any(|(c, f)| c == "keyword" && !f.is_empty()), "{theme:?}");
        lisp.eval(r#"(define (syntax-theme) (list (list "keyword" 'red)))"#).unwrap();
        assert_eq!(lisp.syntax_theme(), vec![("keyword".to_string(), "red".to_string())]);
    }

    #[test]
    fn a_finished_key_sequence_clears_its_echo() {
        let (mut lisp, editor) = lisp_with("abc");
        typed(&mut lisp, "g");
        assert_eq!(editor.lock().message, "g-");
        typed(&mut lisp, "g");
        assert_eq!(editor.lock().message, "");
    }

    fn picker_selection(editor: &Editor) -> Option<String> {
        editor.lock().picker.as_ref().and_then(|p| p.selection().map(str::to_string))
    }

    fn open_fruit_picker(lisp: &mut Lisp) {
        lisp.eval(
            r#"(pick "Fruit" (list "apple" "apricot" "banana" "cherry" "grape") ""
                     (lambda (x) (message (string-append "picked " x))))"#,
        )
        .unwrap();
    }

    #[test]
    fn picker_escape_goes_to_normal_mode_and_jk_move() {
        let (mut lisp, editor) = lisp_with("");
        open_fruit_picker(&mut lisp);
        assert_eq!(lisp.cursor_shape(), "bar", "the picker opens typing");
        lisp.dispatch_key("ESC", "");
        assert!(editor.lock().picker.is_some(), "ESC doesn't close it");
        assert_eq!(lisp.cursor_shape(), "box");
        typed(&mut lisp, "jj");
        assert_eq!(picker_selection(&editor).as_deref(), Some("banana"));
        typed(&mut lisp, "k");
        assert_eq!(picker_selection(&editor).as_deref(), Some("apricot"));
        typed(&mut lisp, "G");
        assert_eq!(picker_selection(&editor).as_deref(), Some("grape"));
        typed(&mut lisp, "gg");
        assert_eq!(picker_selection(&editor).as_deref(), Some("apple"));
        typed(&mut lisp, "3j");
        assert_eq!(picker_selection(&editor).as_deref(), Some("cherry"));
        lisp.dispatch_key("RET", "");
        assert!(editor.lock().picker.is_none());
        assert_eq!(editor.lock().message, "picked cherry");
        assert_eq!(lisp.cursor_shape(), "box", "back in normal state in the buffer");
    }

    #[test]
    fn picker_normal_mode_edits_the_query_with_vim_keys() {
        let (mut lisp, editor) = lisp_with("");
        open_fruit_picker(&mut lisp);
        typed(&mut lisp, "ap");
        lisp.dispatch_key("ESC", "");
        assert_eq!(picker_selection(&editor).as_deref(), Some("apple"));
        typed(&mut lisp, "x");
        assert_eq!(editor.lock().minibuffer.text().to_string(), "a");
        typed(&mut lisp, "A");
        assert_eq!(lisp.cursor_shape(), "bar", "A goes back to typing");
        typed(&mut lisp, "nana");
        assert_eq!(picker_selection(&editor).as_deref(), Some("banana"));
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "0cwche");
        assert_eq!(editor.lock().minibuffer.text().to_string(), "che");
        assert_eq!(picker_selection(&editor).as_deref(), Some("cherry"));
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "u");
        assert_eq!(editor.lock().minibuffer.text().to_string(), "anana");
    }

    #[test]
    fn picker_closes_from_normal_mode_with_escape_or_q() {
        let (mut lisp, editor) = lisp_with("");
        open_fruit_picker(&mut lisp);
        lisp.dispatch_key("ESC", "");
        lisp.dispatch_key("ESC", "");
        assert!(editor.lock().picker.is_none());
        assert!(editor.lock().minibuffer_prompt.is_none());
        open_fruit_picker(&mut lisp);
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "q");
        assert!(editor.lock().picker.is_none());
        typed(&mut lisp, "x");
        assert_eq!(text(&editor), "", "back in the buffer's normal state");
        assert_eq!(lisp.cursor_shape(), "box");
    }

    #[test]
    fn picker_opened_from_insert_state_returns_to_it() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, "i");
        open_fruit_picker(&mut lisp);
        lisp.dispatch_key("RET", "");
        typed(&mut lisp, "z");
        assert_eq!(text(&editor), "z");
    }

    #[test]
    fn modeline_comes_from_scheme() {
        let (mut lisp, _editor) = lisp_with("");
        let segs = lisp.modeline();
        assert!(segs.iter().any(|s| s.text.contains("test")), "{segs:?}");
        lisp.eval(r#"(define (modeline) (list (list "custom" 'accent)))"#).unwrap();
        assert_eq!(
            lisp.modeline(),
            vec![Segment { text: "custom".into(), face: "accent".into() }]
        );
    }
}
