//! The Steel interpreter and the primitives it gets from the Rust core.
//!
//! Rust exposes small, fast primitives (`insert`, `forward-char`, ...). Every
//! command, keymap and the modeline are defined in Scheme (`lisp/core/*.scm`,
//! then the user's `~/.config/gmacs/init.scm`) and can be redefined while the
//! editor runs.

use crate::buffer::Buffer;
use crate::editor::Editor;
use crate::picker::Picker;
use mlua::Lua;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use steel::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

/// The core, one file per area (lisp/core/), built into the binary for
/// running outside a checkout. They load as one program, in name order, so
/// a function may call one defined in a later file.
const CORE_FILES: &[(&str, &str)] = &[
    ("00-helpers.scm", include_str!("../lisp/core/00-helpers.scm")),
    ("05-commands.scm", include_str!("../lisp/core/05-commands.scm")),
    ("10-keymaps.scm", include_str!("../lisp/core/10-keymaps.scm")),
    ("15-states.scm", include_str!("../lisp/core/15-states.scm")),
    ("20-dispatch.scm", include_str!("../lisp/core/20-dispatch.scm")),
    ("30-motions.scm", include_str!("../lisp/core/30-motions.scm")),
    ("35-search.scm", include_str!("../lisp/core/35-search.scm")),
    ("40-text-objects.scm", include_str!("../lisp/core/40-text-objects.scm")),
    ("45-operators.scm", include_str!("../lisp/core/45-operators.scm")),
    ("50-visual.scm", include_str!("../lisp/core/50-visual.scm")),
    ("55-editing.scm", include_str!("../lisp/core/55-editing.scm")),
    ("60-minibuffer.scm", include_str!("../lisp/core/60-minibuffer.scm")),
    ("65-picker.scm", include_str!("../lisp/core/65-picker.scm")),
    ("70-windows.scm", include_str!("../lisp/core/70-windows.scm")),
    ("75-file-tree.scm", include_str!("../lisp/core/75-file-tree.scm")),
    ("80-lsp.scm", include_str!("../lisp/core/80-lsp.scm")),
    ("85-bindings.scm", include_str!("../lisp/core/85-bindings.scm")),
    ("90-tables.scm", include_str!("../lisp/core/90-tables.scm")),
    ("92-syntax.scm", include_str!("../lisp/core/92-syntax.scm")),
    ("95-modeline.scm", include_str!("../lisp/core/95-modeline.scm")),
    ("97-lua.scm", include_str!("../lisp/core/97-lua.scm")),
];

/// The core files joined, each under a comment naming it (errors show the
/// line in the joined program).
fn join_core(files: impl Iterator<Item = (String, String)>) -> String {
    files.map(|(name, text)| format!(";;;; ==== {name} ====\n{text}")).collect::<Vec<_>>().join("\n")
}

/// The `.scm` files in a folder, by name.
fn scheme_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.retain(|p| p.extension().is_some_and(|e| e == "scm"));
    files.sort();
    files
}

/// Scheme the editor loads, in order, at start and on every reload: a file,
/// a folder of files (loaded as one program), or the built-in core.
#[derive(Debug, Clone)]
pub struct Source {
    pub label: String,
    pub path: Option<PathBuf>,
}

impl Source {
    fn read(&self) -> Option<String> {
        match &self.path {
            Some(p) if p.is_dir() => Some(join_core(scheme_files(p).into_iter().filter_map(|f| {
                let name = f.file_name()?.to_string_lossy().into_owned();
                Some((name, std::fs::read_to_string(&f).ok()?))
            }))),
            Some(p) => std::fs::read_to_string(p).ok(),
            None => Some(join_core(CORE_FILES.iter().map(|(n, t)| (n.to_string(), t.to_string())))),
        }
    }

    /// When it last changed: for a folder, its newest file (or the folder
    /// itself, when a file is added or removed).
    fn mtime(&self) -> Option<SystemTime> {
        let path = self.path.as_ref()?;
        let modified = |p: &std::path::Path| p.metadata().ok()?.modified().ok();
        if path.is_dir() {
            scheme_files(path).iter().filter_map(|f| modified(f)).chain(modified(path)).max()
        } else {
            modified(path)
        }
    }
}

/// How a face looks, from Scheme's `(face-specs)`. Colors are face/theme
/// color names (`accent`, `blue`, `muted`, ...).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FaceSpec {
    pub name: String,
    pub color: Option<String>,
    pub background: Option<String>,
    /// Text size relative to the editor's (1.0 = normal).
    pub scale: Option<f32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// The UI's proportional font instead of the monospace one.
    pub sans: bool,
}

fn symbol_text(v: &SteelVal) -> Option<String> {
    match v {
        SteelVal::SymbolV(s) | SteelVal::StringV(s) => Some(s.to_string()),
        _ => None,
    }
}

/// `("name" key value key value ...)`: keys `color`, `background`, `scale`,
/// `weight` (bold), `slant` (italic), `underline` (#t), `family` (sans).
fn parse_face_spec(v: &SteelVal) -> Option<FaceSpec> {
    let SteelVal::ListV(items) = v else { return None };
    let items: Vec<&SteelVal> = items.iter().collect();
    let mut spec = FaceSpec { name: symbol_text(items.first()?)?, ..Default::default() };
    for pair in items[1..].chunks(2) {
        let [key, value] = pair else { break };
        let key = symbol_text(key).unwrap_or_default();
        let text = symbol_text(value).unwrap_or_default();
        match key.as_str() {
            "color" => spec.color = Some(text),
            "background" => spec.background = Some(text),
            "scale" => {
                spec.scale = match value {
                    SteelVal::NumV(n) => Some(*n as f32),
                    SteelVal::IntV(n) => Some(*n as f32),
                    _ => None,
                }
            }
            "weight" => spec.bold = text == "bold",
            "slant" => spec.italic = text == "italic",
            "underline" => spec.underline = matches!(value, SteelVal::BoolV(true)),
            "family" => spec.sans = text == "sans",
            _ => {}
        }
    }
    Some(spec)
}

/// One modeline segment: text plus a face name the view maps to a theme color.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub text: String,
    pub face: String,
}

pub struct Lisp {
    engine: Engine,
    /// Lua, beside Scheme: same primitives, its own commands, keys and hooks.
    lua: Arc<Mutex<Lua>>,
    editor: Editor,
    sources: Vec<Source>,
    stamps: Vec<Option<SystemTime>>,
    /// Lua files loaded after the Scheme (init.lua), and when they last changed.
    lua_sources: Vec<PathBuf>,
    lua_stamps: Vec<Option<SystemTime>>,
    /// The source texts of the last load that succeeded, to fall back on.
    last_good: Option<Vec<String>>,
}

impl Lisp {
    #[cfg(test)]
    pub fn new(editor: Editor, sources: Vec<Source>) -> Self {
        Self::with_lua(editor, sources, Vec::new())
    }

    pub fn with_lua(editor: Editor, sources: Vec<Source>, lua_sources: Vec<PathBuf>) -> Self {
        let lua = Arc::new(Mutex::new(fresh_lua(&editor)));
        let mut lisp = Self {
            engine: fresh_engine(&editor, &lua),
            lua,
            editor,
            stamps: vec![None; sources.len()],
            sources,
            lua_stamps: vec![None; lua_sources.len()],
            lua_sources,
            last_good: None,
        };
        lisp.reload();
        lisp
    }

    /// The user's Lua: ~/.config/gmacs/init.lua.
    pub fn default_lua_sources() -> Vec<PathBuf> {
        std::env::var_os("HOME")
            .map(|home| vec![PathBuf::from(home).join(".config/gmacs/init.lua")])
            .unwrap_or_default()
    }

    /// Loads the Lua files into a fresh Lua (so a reload starts clean, like
    /// Scheme's), then runs the Scheme they queued. Returns the files loaded.
    fn load_lua(&mut self) -> Vec<String> {
        *self.lua.lock().unwrap_or_else(|e| e.into_inner()) = fresh_lua(&self.editor);
        let mut loaded = Vec::new();
        for (i, path) in self.lua_sources.clone().iter().enumerate() {
            self.lua_stamps[i] = path.metadata().ok().and_then(|m| m.modified().ok());
            let Ok(code) = std::fs::read_to_string(path) else { continue };
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let result = {
                let lua = self.lua.lock().unwrap_or_else(|e| e.into_inner());
                lua.load(code).set_name(&name).exec()
            };
            match result {
                Ok(()) => loaded.push(name),
                Err(e) => {
                    self.run_pending_scheme();
                    self.editor.lock().message = format!("{name}: {}", first_line(&e));
                    return loaded;
                }
            }
        }
        self.run_pending_scheme();
        loaded
    }

    /// Runs the Scheme Lua queued (gmacs.scheme), quietly unless it fails.
    fn run_pending_scheme(&mut self) {
        let pending = std::mem::take(&mut self.editor.lock().pending_scheme);
        for code in pending {
            if let Err(e) = self.engine.run(code.clone()) {
                self.editor.lock().message = format!("{code}: {e}");
            }
        }
    }

    /// Evaluates Lua and returns what it shows.
    #[cfg(test)]
    pub fn eval_lua(&mut self, code: &str) -> Result<String, String> {
        let result = {
            let lua = self.lua.lock().unwrap_or_else(|e| e.into_inner());
            eval_lua_in(&lua, code).map_err(|e| first_line(&e))
        };
        self.run_pending_scheme();
        result
    }

    /// A function Lua put in `gmacs.hooks` to replace Scheme's, if any.
    fn lua_hook<R: mlua::FromLuaMulti>(&self, name: &str) -> Option<mlua::Result<R>> {
        let lua = self.lua.lock().unwrap_or_else(|e| e.into_inner());
        let hook = lua
            .globals()
            .get::<mlua::Table>("gmacs")
            .and_then(|g| g.get::<mlua::Table>("hooks"))
            .and_then(|h| h.get::<mlua::Function>(name))
            .ok()?;
        Some(hook.call::<R>(()))
    }

    /// The default sources: the core (the repo's `lisp/core/` when running
    /// from a checkout, so it live-reloads; else the copy built in), then the
    /// user's init file.
    pub fn default_sources() -> Vec<Source> {
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lisp/core");
        let core = Source {
            label: "core".into(),
            path: dev.is_dir().then_some(dev),
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
            Ok(mut loaded) => {
                self.last_good = Some(texts.into_iter().flatten().collect());
                self.editor.lock().message.clear();
                loaded.extend(self.load_lua());
                let mut st = self.editor.lock();
                if st.message.is_empty() {
                    st.message = format!("loaded {}", loaded.join(", "));
                }
            }
            Err(error) => {
                if let Some(good) = self.last_good.clone() {
                    self.engine = fresh_engine(&self.editor, &self.lua);
                    for code in good {
                        let _ = self.engine.run(code);
                    }
                    self.load_lua();
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
        let lua_changed = self
            .lua_sources
            .iter()
            .zip(&self.lua_stamps)
            .any(|(p, old)| p.metadata().ok().and_then(|m| m.modified().ok()) != *old);
        let changed = lua_changed || self.sources.iter().zip(&self.stamps).any(|(s, old)| s.mtime() != *old);
        if changed {
            self.reload();
        }
        changed
    }

    /// Hands a keystroke to Scheme's `dispatch-key`. `key` is Emacs notation
    /// (`"C-x"`, `"M-:"`, `"RET"`); `text` is what it would type, or "".
    pub fn dispatch_key(&mut self, key: &str, text: &str) {
        // Any key closes the documentation K showed.
        self.editor.lock().hover = None;
        let args = vec![SteelVal::from(key.to_string()), SteelVal::from(text.to_string())];
        if let Err(e) = self.engine.call_function_by_name_with_args("dispatch-key", args) {
            self.editor.lock().message = format!("{key}: {e}");
        }
        self.run_pending_scheme();
        self.run_pending_evals();
    }

    /// Runs what Rust queued for Scheme (server replies that need Scheme,
    /// like the references picker): each calls the Scheme function of that
    /// name with a list of strings.
    pub fn run_events(&mut self) -> bool {
        let events = self.editor.lock().take_events();
        let any = !events.is_empty();
        for (name, args) in events {
            let list = SteelVal::ListV(args.into_iter().map(SteelVal::from).collect());
            if let Err(e) = self.engine.call_function_by_name_with_args(&name, vec![list]) {
                self.editor.lock().message = format!("{name}: {e}");
            }
        }
        any
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

    /// Calls Scheme's `(selection)`: the selected ranges `[start, end)` in
    /// characters. Scheme returns one range `(start end)`, a list of them
    /// (a visual block: one per line), or `()` for none.
    pub fn selection(&mut self) -> Vec<(usize, usize)> {
        let Ok(v) = self.engine.call_function_by_name_with_args("selection", vec![]) else {
            return vec![];
        };
        let pair = |v: &SteelVal| -> Option<(usize, usize)> {
            let SteelVal::ListV(items) = v else { return None };
            let mut it = items.iter();
            match (it.next()?, it.next()?) {
                (SteelVal::IntV(a), SteelVal::IntV(b)) => Some((*a as usize, *b as usize)),
                _ => None,
            }
        };
        let ranges = match &v {
            SteelVal::ListV(items) if matches!(items.iter().next(), Some(SteelVal::ListV(_))) => {
                items.iter().filter_map(pair).collect()
            }
            other => pair(other).into_iter().collect::<Vec<_>>(),
        };
        ranges.into_iter().filter(|(a, b)| a < b).collect()
    }

    /// Calls Scheme's `(search-highlight)`: the pattern whose matches the
    /// window highlights, or "" for none.
    pub fn search_highlight(&mut self) -> String {
        match self.engine.call_function_by_name_with_args("search-highlight", vec![]) {
            Ok(SteelVal::StringV(s)) => s.to_string(),
            _ => String::new(),
        }
    }

    /// A Lua hook's list of lists of strings (`{{"keyword", "magenta"}, ...}`).
    fn lua_hook_rows(&self, name: &str) -> Option<Result<Vec<Vec<String>>, String>> {
        let result = self.lua_hook::<mlua::Table>(name)?;
        Some(
            result
                .and_then(|rows| {
                    rows.sequence_values::<mlua::Table>()
                        .map(|row| row.and_then(|r| r.sequence_values::<String>().collect()))
                        .collect()
                })
                .map_err(|e| first_line(&e)),
        )
    }

    /// Calls Scheme's `(syntax-theme)`: (capture face) pairs saying how
    /// tree-sitter captures look. A Lua `gmacs.hooks.syntax_theme` replaces it.
    pub fn syntax_theme(&mut self) -> Vec<(String, String)> {
        if let Some(Ok(rows)) = self.lua_hook_rows("syntax_theme") {
            return rows.into_iter().filter_map(|r| Some((r.first()?.clone(), r.get(1)?.clone()))).collect();
        }
        match self.engine.call_function_by_name_with_args("syntax-theme", vec![]) {
            Ok(v) => parse_segments(&v).into_iter().map(|s| (s.text, s.face)).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Calls Scheme's `(lsp-servers)`: `(language command arg ...)` lists. A
    /// Lua `gmacs.hooks.lsp_servers` replaces it.
    pub fn lsp_servers(&mut self) -> Vec<crate::lsp::ServerConfig> {
        if let Some(Ok(rows)) = self.lua_hook_rows("lsp_servers") {
            return rows
                .into_iter()
                .filter_map(|r| {
                    let mut words = r.into_iter();
                    Some(crate::lsp::ServerConfig { language: words.next()?, command: words.next()?, args: words.collect() })
                })
                .collect();
        }
        let Ok(SteelVal::ListV(items)) = self.engine.call_function_by_name_with_args("lsp-servers", vec![]) else {
            return vec![];
        };
        items
            .iter()
            .filter_map(|item| {
                let SteelVal::ListV(parts) = item else { return None };
                let mut words = parts.iter().filter_map(symbol_text);
                Some(crate::lsp::ServerConfig {
                    language: words.next()?,
                    command: words.next()?,
                    args: words.collect(),
                })
            })
            .collect()
    }

    /// Calls Scheme's `(face-specs)`: how named faces look.
    pub fn face_specs(&mut self) -> Vec<FaceSpec> {
        match self.engine.call_function_by_name_with_args("face-specs", vec![]) {
            Ok(SteelVal::ListV(items)) => items.iter().filter_map(parse_face_spec).collect(),
            _ => Vec::new(),
        }
    }

    /// Calls Scheme's `(modeline)`: a list of `(text face)` pairs. A Lua
    /// `gmacs.hooks.modeline` returning `{{text, face}, ...}` replaces it.
    pub fn modeline(&mut self) -> Vec<Segment> {
        match self.lua_hook_rows("modeline") {
            Some(Ok(rows)) => {
                return rows
                    .into_iter()
                    .filter_map(|r| {
                        Some(Segment { text: r.first()?.clone(), face: r.get(1).cloned().unwrap_or("default".into()) })
                    })
                    .collect();
            }
            Some(Err(e)) => return vec![Segment { text: format!("modeline (lua): {e}"), face: "error".into() }],
            None => {}
        }
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
/// Where primitives get registered: Steel, Lua's `gmacs` table, or both.
struct Registry<'a> {
    engine: Option<&'a mut Engine>,
    lua: Option<(&'a Lua, mlua::Table)>,
}

/// A primitive's Lua name: `buffer-name` -> `buffer_name`, `pos->line` ->
/// `pos_to_line`, `minibuffer-active?` -> `minibuffer_active`.
pub fn lua_name(scheme: &str) -> String {
    scheme.replace("->", "_to_").trim_end_matches(['?', '!']).replace('-', "_")
}

/// Registers a primitive that gets the editor, in each interpreter of the
/// registry. `$args` are its arguments with their Rust types.
macro_rules! prim {
    ($reg:ident, $editor:ident, $name:literal, |$st:ident $(, $a:ident : $t:ty)*| $body:expr) => {{
        if let Some(engine) = $reg.engine.as_mut() {
            let ed = $editor.clone();
            engine.register_fn($name, move |$($a: $t),*| {
                #[allow(unused_mut)]
                let mut $st = ed.lock();
                $body
            });
        }
        if let Some((lua, gmacs)) = $reg.lua.as_ref() {
            let ed = $editor.clone();
            #[allow(unused_parens)]
            let f = lua
                .create_function(move |_, ($($a,)*): ($($t,)*)| {
                    #[allow(unused_mut)]
                    let mut $st = ed.lock();
                    // An inner closure, so a `return` in the body returns its value.
                    #[allow(clippy::redundant_closure_call)]
                    let value = (|| $body)();
                    Ok(value)
                })
                .expect("a Lua function");
            gmacs.set(lua_name($name), f).expect("set in gmacs");
        }
    }};
}

/// Registers a primitive that doesn't need the editor, in each interpreter.
macro_rules! pure {
    ($reg:ident, $name:literal, |$($a:ident : $t:ty),*| $body:expr) => {{
        if let Some(engine) = $reg.engine.as_mut() {
            engine.register_fn($name, move |$($a: $t),*| $body);
        }
        if let Some((lua, gmacs)) = $reg.lua.as_ref() {
            let f = lua.create_function(move |_, ($($a,)*): ($($t,)*)| Ok($body)).expect("a Lua function");
            gmacs.set(lua_name($name), f).expect("set in gmacs");
        }
    }};
}

/// A Steel engine with the primitives, and the bridge to Lua.
fn fresh_engine(editor: &Editor, lua: &Arc<Mutex<Lua>>) -> Engine {
    let mut engine = Engine::new();
    {
        let mut reg = Registry { engine: Some(&mut engine), lua: None };
        register_primitives(&mut reg, editor);
    }
    // These hold no editor lock while Lua runs: Lua calls primitives, which take it.
    let (ed, l) = (editor.clone(), lua.clone());
    engine.register_fn("lua-call", move |name: String| {
        let result = {
            let lua = l.lock().unwrap_or_else(|e| e.into_inner());
            lua_command(&lua, &name)
        };
        if let Err(e) = result {
            ed.lock().message = format!("{name}: {}", first_line(&e));
        }
    });
    let (ed, l) = (editor.clone(), lua.clone());
    engine.register_fn("lua-eval", move |code: String| {
        let result = {
            let lua = l.lock().unwrap_or_else(|e| e.into_inner());
            eval_lua_in(&lua, &code)
        };
        let mut st = ed.lock();
        match result {
            // No value (print, statements): keep whatever the code showed.
            Ok(v) if v.is_empty() => {}
            Ok(v) => st.message = v,
            Err(e) => st.message = format!("lua: {}", first_line(&e)),
        }
    });
    engine
}

/// A Lua state with the primitives in `gmacs`, and the Lua side of the
/// bridge (`gmacs.command`, `gmacs.map`, `gmacs.hooks`, ...).
fn fresh_lua(editor: &Editor) -> Lua {
    let lua = Lua::new();
    let gmacs = lua.create_table().expect("a table");
    {
        let mut reg = Registry { engine: None, lua: Some((&lua, gmacs.clone())) };
        register_primitives(&mut reg, editor);
    }
    lua.globals().set("gmacs", gmacs).expect("set gmacs");
    lua.load(LUA_PRELUDE).set_name("prelude.lua").exec().expect("the Lua prelude runs");
    lua
}

/// The Lua side of the bridge to Scheme.
const LUA_PRELUDE: &str = r##"
gmacs.commands = {}
-- Functions that replace Scheme's: modeline, lsp_servers, syntax_theme.
gmacs.hooks = {}

local function quote(s)
  return '"' .. (s:gsub('\\', '\\\\'):gsub('"', '\\"')) .. '"'
end

-- A command callable by name from anywhere (keys, SPC :, Scheme).
function gmacs.command(name, fn)
  gmacs.commands[name] = fn
  gmacs.scheme("(register-lua-command! '" .. name .. ")")
end

-- Binds keys ("SPC i d") in a state (normal, insert, visual, operator)
-- to a command name or a function.
local anonymous = 0
function gmacs.map(state, keys, command)
  if type(command) == "function" then
    anonymous = anonymous + 1
    local name = "lua-command-" .. anonymous
    gmacs.command(name, command)
    command = name
  end
  gmacs.scheme("(lua-define-key '" .. state .. " " .. quote(keys) .. " '" .. command .. ")")
end

-- Runs a command (Scheme's or Lua's) right after this one.
function gmacs.run(name)
  gmacs.scheme("(call-binding '" .. name .. ")")
end

-- print shows in the echo area.
function print(...)
  local parts = {}
  for i = 1, select("#", ...) do parts[#parts + 1] = tostring(select(i, ...)) end
  gmacs.message(table.concat(parts, " "))
end
"##;

fn first_line(e: &impl std::fmt::Display) -> String {
    e.to_string().lines().next().unwrap_or("").to_string()
}

/// Runs a Lua command registered with gmacs.command.
fn lua_command(lua: &Lua, name: &str) -> mlua::Result<()> {
    let gmacs: mlua::Table = lua.globals().get("gmacs")?;
    let commands: mlua::Table = gmacs.get("commands")?;
    let f: mlua::Function = commands.get(name)?;
    f.call::<()>(())
}

/// Evaluates Lua and shows the result: an expression's value, else "".
fn eval_lua_in(lua: &Lua, code: &str) -> mlua::Result<String> {
    // Try it as an expression first (:lua 1 + 2 shows 3), then as statements.
    let value: mlua::Value = match lua.load(format!("return {code}")).set_name("eval").eval() {
        Ok(v) => v,
        Err(_) => lua.load(code).set_name("eval").eval()?,
    };
    Ok(match value {
        mlua::Value::Nil => String::new(),
        mlua::Value::String(s) => s.to_string_lossy().to_string(),
        other => other.to_string()?,
    })
}

fn register_primitives(reg: &mut Registry, editor: &Editor) {
    // Editing the current buffer (the minibuffer while it is open).
    prim!(reg, editor, "insert", |s, text: String| s.current().insert(&text));
    prim!(reg, editor, "delete-backward-char", |s, n: isize| s
        .current()
        .delete_backward(n.max(0) as usize));
    prim!(reg, editor, "delete-char", |s, n: isize| s
        .current()
        .delete_forward(n.max(0) as usize));
    prim!(reg, editor, "forward-char", |s, n: isize| s.current().forward_char(n));
    prim!(reg, editor, "forward-line", |s, n: isize| s.current().forward_line(n));
    prim!(reg, editor, "beginning-of-line", |s| s.current().beginning_of_line());
    prim!(reg, editor, "end-of-line", |s| s.current().end_of_line());
    prim!(reg, editor, "goto-char", |s, pos: isize| s.current().goto_char(pos.max(0) as usize));
    prim!(reg, editor, "point", |s| s.current().point() as isize);
    prim!(reg, editor, "point-max", |s| s.current().len() as isize);
    prim!(reg, editor, "char-at", |s, pos: isize| s.current().char_at(pos.max(0) as usize));
    prim!(reg, editor, "buffer-substring", |s, a: isize, b: isize| s
        .current()
        .substring(a.max(0) as usize, b.max(0) as usize));
    prim!(reg, editor, "line-beginning-position", |s, pos: isize| s
        .current()
        .line_start(pos.max(0) as usize) as isize);
    prim!(reg, editor, "line-end-position", |s, pos: isize| s
        .current()
        .line_end(pos.max(0) as usize) as isize);
    prim!(reg, editor, "delete-region", |s, a: isize, b: isize| s
        .current()
        .delete_range(a.max(0) as usize, b.max(0) as usize));
    prim!(reg, editor, "char-class", |s, pos: isize| s
        .current()
        .char_class(pos.max(0) as usize)
        .to_string());
    prim!(reg, editor, "pos->line", |s, pos: isize| s.current().line_of(pos.max(0) as usize) as isize);
    prim!(reg, editor, "line->pos", |s, line: isize| s
        .current()
        .line_to_pos(line.max(0) as usize) as isize);
    prim!(reg, editor, "line-count", |s| s.current().line_count() as isize);
    // Search, always in the file buffer: regex, smartcase. -1 when nothing matches.
    prim!(reg, editor, "search-forward", |s, pattern: String, from: isize| {
        let Some(re) = s.search_regex(&pattern) else { return -1 };
        s.buffer().search_forward(&re, from.max(0) as usize).map_or(-1, |p| p as isize)
    });
    prim!(reg, editor, "search-backward", |s, pattern: String, before: isize| {
        let Some(re) = s.search_regex(&pattern) else { return -1 };
        s.buffer().search_backward(&re, before.max(0) as usize).map_or(-1, |p| p as isize)
    });
    pure!(reg, "regex-quote", |text: String| regex::escape(&text));
    // The fuzzy picker. Scheme opens it and decides what picking does.
    prim!(reg, editor, "project-root", |s| {
        let start = s
            .buffer()
            .file()
            .cloned()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        crate::picker::project_root(&start).display().to_string()
    });
    pure!(reg, "project-files", |root: String| crate::picker::project_files(Path::new(&root)));
    prim!(reg, editor, "picker-open", |s, title: String, items: Vec<String>, root: String| {
        let root = (!root.is_empty()).then(|| PathBuf::from(root));
        s.picker = Some(Picker::new(title, items, root));
    });
    prim!(reg, editor, "picker-close", |s| s.picker = None);
    prim!(reg, editor, "picker-active?", |s| s.picker.is_some());
    prim!(reg, editor, "picker-filter", |s, query: String| {
        if let Some(p) = s.picker.as_mut() {
            p.filter(&query);
        }
    });
    prim!(reg, editor, "picker-move", |s, n: isize| {
        if let Some(p) = s.picker.as_mut() {
            p.move_selection(n);
        }
    });
    prim!(reg, editor, "picker-select", |s, index: isize| {
        if let Some(p) = s.picker.as_mut() {
            p.select(index);
        }
    });
    prim!(reg, editor, "picker-selection", |s| s
        .picker
        .as_ref()
        .and_then(|p| p.selection().map(str::to_string))
        .unwrap_or_default());
    prim!(reg, editor, "undo-boundary", |s| s.current().undo_boundary());
    prim!(reg, editor, "undo", |s| s.current().undo());
    prim!(reg, editor, "redo", |s| s.current().redo());
    // The register: the last deleted or yanked text.
    prim!(reg, editor, "register-set", |s, text: String| s.kill_ring.push(text));
    prim!(reg, editor, "register-get", |s| s.kill_ring.last().cloned().unwrap_or_default());
    prim!(reg, editor, "buffer-string", |s| s.current().text().to_string());

    // About the file buffer (not the minibuffer).
    prim!(reg, editor, "buffer-name", |s| s.buffer().name().to_string());
    prim!(reg, editor, "buffer-file-name", |s| s
        .buffer()
        .file()
        .map(|p| p.display().to_string())
        .unwrap_or_default());
    prim!(reg, editor, "buffer-read-only?", |s| s.buffer().is_read_only());
    prim!(reg, editor, "buffer-revision", |s| s.buffer().revision() as isize);
    prim!(reg, editor, "buffer-modified?", |s| s.buffer().is_modified());
    prim!(reg, editor, "buffer-language", |s| s
        .buffer()
        .file()
        .and_then(|f| crate::syntax::language_for_path(f))
        .unwrap_or("text")
        .to_string());
    prim!(reg, editor, "line-number", |s| s.buffer().line() as isize + 1);
    prim!(reg, editor, "column-number", |s| s.buffer().column() as isize);
    prim!(reg, editor, "find-file", |s, path: String| {
        let path = expand_home(&path);
        s.message = match s.open_file(&path) {
            Ok(()) => format!("opened {}", path.display()),
            Err(e) => format!("{}: {e}", path.display()),
        };
    });
    prim!(reg, editor, "save-buffer", |s| {
        s.message = match s.buffer_mut().save() {
            Ok(p) => format!("wrote {}", p.display()),
            Err(e) => format!("not saved: {e}"),
        };
    });

    // Buffers.
    prim!(reg, editor, "buffer-list", |s| s.buffer_names());
    prim!(reg, editor, "switch-to-buffer", |s, name: String| s.switch_to_buffer(&name));
    prim!(reg, editor, "next-buffer", |s, n: isize| s.next_buffer(n));
    prim!(reg, editor, "new-buffer", |s, name: String| {
        let id = s.add_buffer(Buffer::new(name));
        s.show_buffer(id);
    });
    prim!(reg, editor, "kill-buffer", |s, force: bool| {
        s.message = match s.kill_buffer(force) {
            Ok(name) => format!("closed {name}"),
            Err(e) => e,
        };
    });

    // Windows: split, move between, close; scrolling.
    prim!(reg, editor, "split-window", |s, side: String| s.split_window(
        if side == "below" { crate::editor::Axis::Column } else { crate::editor::Axis::Row }
    ));
    prim!(reg, editor, "window-in-direction?", |s, dir: String| s.window_in_direction(&dir).is_some());
    prim!(reg, editor, "select-window-in-direction", |s, dir: String| {
        match s.window_in_direction(&dir) {
            Some(id) => {
                s.select_window(id);
                true
            }
            None => false,
        }
    });
    prim!(reg, editor, "other-window", |s, n: isize| s.other_window(n));
    prim!(reg, editor, "delete-window", |s| s.close_window());
    prim!(reg, editor, "delete-other-windows", |s| s.only_window());
    prim!(reg, editor, "window-count", |s| s.window_count() as isize);
    prim!(reg, editor, "window-height", |s| s.window().rows.max(1) as isize);
    prim!(reg, editor, "window-top-line", |s| s.window().scroll_top as isize);
    prim!(reg, editor, "scroll-window", |s, n: isize| s.scroll(n));
    prim!(reg, editor, "recenter", |s, place: String| s.recenter(&place));

    // Buffers by name, for buffers Scheme draws itself (the file tree).
    prim!(reg, editor, "ensure-buffer", |s, name: String| {
        s.ensure_buffer(&name);
    });
    prim!(reg, editor, "set-buffer-text", |s, name: String, text: String| {
        if let Some(b) = s.buffer_named_mut(&name) {
            b.set_text(&text);
        }
    });
    prim!(reg, editor, "set-buffer-read-only", |s, name: String, read_only: bool| {
        if let Some(b) = s.buffer_named_mut(&name) {
            b.set_read_only(read_only);
        }
    });
    prim!(reg, editor, "add-face", |s, name: String, start: isize, end: isize, face: String| {
        if let Some(b) = s.buffer_named_mut(&name) {
            b.add_face(start.max(0) as usize, end.max(0) as usize, face);
        }
    });

    // The sidebar: a window at the left edge, outside the splits.
    prim!(reg, editor, "sidebar-open", |s, name: String| {
        let id = s.ensure_buffer(&name);
        s.open_sidebar(id);
    });
    prim!(reg, editor, "sidebar-close", |s| s.close_sidebar());
    prim!(reg, editor, "sidebar-open?", |s| s.sidebar().is_some());
    prim!(reg, editor, "sidebar-selected?", |s| s.is_sidebar(s.selected_window()));
    prim!(reg, editor, "select-main-window", |s| s.select_main_window());

    // Files and folders.
    pure!(reg, "directory-entries", |dir: String| directory_entries(Path::new(&dir)));
    prim!(reg, editor, "create-path", |s, path: String| {
        let path = PathBuf::from(path);
        let made = if path.to_string_lossy().ends_with('/') {
            std::fs::create_dir_all(&path)
        } else if path.exists() {
            Err(std::io::Error::other("already exists"))
        } else {
            path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, ""))
        };
        match made {
            Ok(()) => true,
            Err(e) => {
                s.message = format!("{}: {e}", path.display());
                false
            }
        }
    });
    prim!(reg, editor, "rename-path", |s, from: String, to: String| {
        match std::fs::rename(&from, &to) {
            Ok(()) => true,
            Err(e) => {
                s.message = format!("{from}: {e}");
                false
            }
        }
    });
    prim!(reg, editor, "delete-path", |s, path: String| {
        let p = Path::new(&path);
        let gone = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
        match gone {
            Ok(()) => true,
            Err(e) => {
                s.message = format!("{path}: {e}");
                false
            }
        }
    });

    // Markdown tables, aligned like Org mode.
    prim!(reg, editor, "table-at-point?", |s| {
        let b = s.current();
        let line = b.line();
        crate::markdown::is_table_row(&b.text().line(line).to_string())
    });
    prim!(reg, editor, "align-table", |s, move_cells: isize| align_table_at_point(s.current(), move_cells));

    // Language servers.
    prim!(reg, editor, "lsp-status", |s| s.lsp.status().to_string());
    prim!(reg, editor, "lsp-goto-definition", |s| s.lsp_request(crate::lsp::RequestKind::Definition));
    prim!(reg, editor, "lsp-hover", |s| s.lsp_request(crate::lsp::RequestKind::Hover));
    prim!(reg, editor, "lsp-references", |s| s.lsp_request(crate::lsp::RequestKind::References));
    prim!(reg, editor, "goto-location", |s, item: String| s.goto_location(&item));
    prim!(reg, editor, "lsp-rename", |s, name: String| s.lsp_rename(&name));
    prim!(reg, editor, "lsp-code-actions", |s, start: isize, end: isize| s
        .lsp_code_actions(start.max(0) as usize, end.max(0) as usize));
    prim!(reg, editor, "apply-code-action", |s, index: isize| s.apply_code_action(index.max(0) as usize));
    prim!(reg, editor, "word-at-point", |s| {
        let b = s.buffer();
        let word = |p: usize| b.char_at(p).chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_');
        let (mut a, mut z) = (b.point(), b.point());
        while a > 0 && word(a - 1) {
            a -= 1;
        }
        while word(z) {
            z += 1;
        }
        b.substring(a, z)
    });
    // Completion while typing.
    prim!(reg, editor, "lsp-complete", |s| s.lsp_complete());
    prim!(reg, editor, "completion-active?", |s| s.completion.is_some());
    prim!(reg, editor, "completion-refresh", |s| s.completion_refresh());
    prim!(reg, editor, "completion-move", |s, n: isize| s.completion_move(n));
    prim!(reg, editor, "completion-accept", |s| s.completion_accept());
    prim!(reg, editor, "completion-close", |s| s.completion = None);
    pure!(reg, "word-char?", |text: String| text.chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_'));
    prim!(reg, editor, "jump-back", |s| {
        if !s.jump_back() {
            s.message = "No earlier position".into();
        }
    });
    prim!(reg, editor, "lsp-servers-here", |s| s
        .buffer()
        .file()
        .map(|f| s.lsp.servers_for(f).join(" "))
        .unwrap_or_default());
    prim!(reg, editor, "diagnostic-summary", |s| {
        let list = s.buffer_diagnostics();
        let count = |sev: u8| list.iter().filter(|(_, d)| d.severity == sev).count();
        let (errors, warnings) = (count(1), count(2));
        let mut parts = Vec::new();
        if errors > 0 {
            parts.push(format!("E{errors}"));
        }
        if warnings > 0 {
            parts.push(format!("W{warnings}"));
        }
        parts.join(" ")
    });
    // Jumps to the next (1) or previous (-1) diagnostic, wrapping, and shows it.
    prim!(reg, editor, "diagnostic-goto", |s, dir: isize| {
        let list = s.buffer_diagnostics();
        let point = s.buffer().point();
        let found = if dir >= 0 {
            list.iter().find(|((a, _), _)| *a > point).or(list.first())
        } else {
            list.iter().rev().find(|((a, _), _)| *a < point).or(list.last())
        };
        match found.cloned() {
            Some(((a, _), d)) => {
                s.buffer_mut().goto_char(a);
                s.message = format!("{}: {}", d.severity_name(), d.message);
                true
            }
            None => {
                s.message = "No diagnostics".into();
                false
            }
        }
    });
    // "line:col  severity: message" for each, for the picker.
    prim!(reg, editor, "diagnostic-items", |s| {
        let rope = s.buffer().text().clone();
        s.buffer_diagnostics()
            .into_iter()
            .map(|((a, _), d)| {
                let line = rope.char_to_line(a);
                let col = a - rope.line_to_char(line);
                format!("{}:{}  {}: {}", line + 1, col + 1, d.severity_name(), d.message.lines().next().unwrap_or(""))
            })
            .collect::<Vec<_>>()
    });
    prim!(reg, editor, "goto-line-column", |s, line: isize, col: isize| {
        let b = s.buffer_mut();
        let pos = b.line_to_pos((line - 1).max(0) as usize) + (col - 1).max(0) as usize;
        let end = b.line_end(pos.min(b.len()));
        b.goto_char(pos.min(end));
    });

    // The echo area and minibuffer.
    prim!(reg, editor, "message", |s, text: String| s.message = text);
    prim!(reg, editor, "current-message", |s| s.message.clone());
    prim!(reg, editor, "minibuffer-open", |s, prompt: String| {
        s.minibuffer.set_text("");
        s.minibuffer_prompt = Some(prompt);
    });
    prim!(reg, editor, "minibuffer-close", |s| s.minibuffer_prompt = None);
    prim!(reg, editor, "minibuffer-active?", |s| s.minibuffer_prompt.is_some());
    prim!(reg, editor, "minibuffer-contents", |s| s.minibuffer.text().to_string());

    // Scheme for Lua to run (gmacs.scheme), silently, after the current command.
    prim!(reg, editor, "scheme", |s, code: String| s.pending_scheme.push(code));
    // Runs Scheme source after the current command (see `pending_eval`).
    prim!(reg, editor, "eval-string", |s, code: String| s.pending_eval.push(code));
    prim!(reg, editor, "quit-editor", |s| s.quit_requested = true);
}

/// Aligns the table around the point (its rows are the lines around it that
/// start with `|`), moving the cursor `move_cells` cells. One edit, so one
/// undo step; nothing changes if it was aligned already.
fn align_table_at_point(b: &mut Buffer, move_cells: isize) -> bool {
    let row_at = |b: &Buffer, line: usize| crate::markdown::is_table_row(&b.text().line(line).to_string());
    let here = b.line();
    if !row_at(b, here) {
        return false;
    }
    let mut first = here;
    while first > 0 && row_at(b, first - 1) {
        first -= 1;
    }
    let mut last = here;
    while last + 1 < b.line_count() && row_at(b, last + 1) {
        last += 1;
    }
    let lines: Vec<String> = (first..=last)
        .map(|l| b.text().line(l).to_string().trim_end_matches(['\n', '\r']).to_string())
        .collect();
    let aligned = crate::markdown::align_table(&lines, (here - first, b.column()), move_cells);
    let (start, end) = (b.line_to_pos(first), b.line_end(b.line_to_pos(last)));
    if aligned.lines != lines {
        b.delete_range(start, end);
        b.goto_char(start);
        b.insert(&aligned.lines.join("\n"));
    }
    let (row, col) = aligned.cursor;
    let target = b.line_to_pos(first + row) + col;
    b.goto_char(target);
    true
}

/// A folder's entries for the file tree: folders first (with a trailing
/// `/`), then files, each sorted; hidden by .gitignore and `.git` left out,
/// other dotfiles kept.
fn directory_entries(dir: &Path) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let walk = ignore::WalkBuilder::new(dir).max_depth(Some(1)).hidden(false).build();
    for entry in walk.filter_map(Result::ok) {
        if entry.depth() == 0 || entry.file_name() == ".git" {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            dirs.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    dirs.sort_by_key(|d| d.to_lowercase());
    files.sort_by_key(|f| f.to_lowercase());
    dirs.extend(files);
    dirs
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
    use crate::editor::EditorState;

    fn lisp_with(text: &str) -> (Lisp, Editor) {
        let mut buf = Buffer::new("test");
        buf.insert(text);
        buf.goto_char(0);
        let editor = Editor::new(buf);
        let core = Source { label: "core".into(), path: None };
        (Lisp::new(editor.clone(), vec![core]), editor)
    }

    fn text(editor: &Editor) -> String {
        editor.lock().buffer().text().to_string()
    }

    #[test]
    fn core_loads() {
        let (_lisp, editor) = lisp_with("");
        assert_eq!(editor.lock().message, "loaded core");
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
        assert_eq!(lisp.selection(), vec![(1, 4)]);
        typed(&mut lisp, "d");
        assert_eq!(text(&editor), "aef");
        assert_eq!(lisp.selection(), vec![], "back in normal state");
    }

    #[test]
    fn visual_line_and_swap_ends() {
        let (mut lisp, editor) = lisp_with("one\ntwo\nthree\nfour");
        typed(&mut lisp, "jVj");
        assert_eq!(lisp.selection(), vec![(4, 14)]);
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
        assert_eq!(lisp.selection(), vec![(6, 10)]);
        typed(&mut lisp, "cgamma");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "alpha gamma");
    }

    #[test]
    fn visual_escape_and_toggle_leave() {
        let (mut lisp, _editor) = lisp_with("abc");
        typed(&mut lisp, "vl");
        lisp.dispatch_key("ESC", "");
        assert_eq!(lisp.selection(), vec![]);
        typed(&mut lisp, "vlv");
        assert_eq!(lisp.selection(), vec![]);
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
        typed(&mut lisp, "95modeline");
        assert_eq!(editor.lock().picker.as_ref().unwrap().selection(), Some("lisp/core/95-modeline.scm"));
        lisp.dispatch_key("RET", "");
        let st = editor.lock();
        assert!(st.picker.is_none(), "picking closes it");
        assert_eq!(st.buffer().name(), "95-modeline.scm");
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
            Source { label: "core".into(), path: None },
            Source { label: "init.scm".into(), path: Some(init.clone()) },
        ];
        let mut lisp = Lisp::new(editor.clone(), sources);
        std::fs::write(&init, "(define (broken) (no-such-function))").unwrap();
        lisp.reload();
        { let m = editor.lock().message.clone(); assert!(m.contains("kept the last version"), "{m}"); }
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
        { let m = editor.lock().message.clone(); assert!(m.contains("unsaved"), "{m}"); }
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
    fn dot_repeats_operators_and_counts() {
        let (mut lisp, editor) = lisp_with("a b c d e f g");
        typed(&mut lisp, "dw");
        typed(&mut lisp, ".");
        assert_eq!(text(&editor), "c d e f g");
        typed(&mut lisp, "2dw");
        assert_eq!(text(&editor), "e f g");
        typed(&mut lisp, ".");
        assert_eq!(text(&editor), "g", "repeats with its own count");
        typed(&mut lisp, "u");
        typed(&mut lisp, "1.");
        assert_eq!(text(&editor), "f g", "a count on . replaces it");
    }

    #[test]
    fn dot_repeats_an_insert_session() {
        let (mut lisp, editor) = lisp_with("one\ntwo");
        typed(&mut lisp, "A!");
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "j.");
        assert_eq!(text(&editor), "one!\ntwo!");
        typed(&mut lisp, "0ciwTWO");
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "k0.");
        assert_eq!(text(&editor), "TWO!\nTWO!");
    }

    #[test]
    fn dot_ignores_motions_undo_and_ex() {
        let (mut lisp, editor) = lisp_with("abcdef");
        typed(&mut lisp, "x");
        typed(&mut lisp, "ll");
        typed(&mut lisp, "u");
        typed(&mut lisp, ":noh\n");
        typed(&mut lisp, "0.");
        assert_eq!(text(&editor), "bcdef", "x is still the last change");
        typed(&mut lisp, "3.");
        assert_eq!(text(&editor), "ef");
    }

    #[test]
    fn dot_repeats_visual_and_f_changes() {
        let (mut lisp, editor) = lisp_with("1\n2\n3\n4\n5");
        typed(&mut lisp, "Vjd");
        assert_eq!(text(&editor), "3\n4\n5");
        typed(&mut lisp, ".");
        assert_eq!(text(&editor), "5");
        let (mut lisp, editor) = lisp_with("a,b,c,d");
        typed(&mut lisp, "dt,");
        typed(&mut lisp, "l.");
        assert_eq!(text(&editor), ",,c,d");
    }

    #[test]
    fn dot_without_a_change() {
        let (mut lisp, editor) = lisp_with("abc");
        typed(&mut lisp, ".");
        assert_eq!(editor.lock().message, "No change to repeat");
    }

    #[test]
    fn visual_block_selects_a_rectangle() {
        let (mut lisp, editor) = lisp_with("abcd\nefgh\nij\nklmn");
        at(&mut lisp, &editor, 1);
        lisp.dispatch_key("C-v", "");
        typed(&mut lisp, "jjjl");
        assert_eq!(lisp.selection(), vec![(1, 3), (6, 8), (11, 12), (14, 16)]);
        assert!(lisp.modeline().iter().any(|s| s.text == "V-BLOCK"));
        typed(&mut lisp, "$");
        assert_eq!(lisp.selection(), vec![(1, 4), (6, 9), (11, 12), (14, 17)], "$ reaches every line's end");
    }

    #[test]
    fn visual_block_delete_yank_and_paste() {
        let (mut lisp, editor) = lisp_with("abcd\nefgh\nijkl");
        at(&mut lisp, &editor, 1);
        lisp.dispatch_key("C-v", "");
        typed(&mut lisp, "jld");
        assert_eq!(text(&editor), "ad\neh\nijkl");
        assert_eq!(point(&editor), 1);
        typed(&mut lisp, "$p");
        assert_eq!(text(&editor), "adbc\nehfg\nijkl", "pasted as a column");
        typed(&mut lisp, "u");
        at(&mut lisp, &editor, 0);
        lisp.dispatch_key("C-v", "");
        typed(&mut lisp, "jjy");
        typed(&mut lisp, "P");
        assert_eq!(text(&editor), "aad\neeh\niijkl");
    }

    #[test]
    fn visual_block_insert_and_append_on_every_line() {
        let (mut lisp, editor) = lisp_with("one\ntwo\nthree");
        lisp.dispatch_key("C-v", "");
        typed(&mut lisp, "jjI// ");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "// one\n// two\n// three");
        typed(&mut lisp, "u");
        assert_eq!(text(&editor), "one\ntwo\nthree", "one undo step");
        at(&mut lisp, &editor, 0);
        lisp.dispatch_key("C-v", "");
        typed(&mut lisp, "jj$A;");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "one;\ntwo;\nthree;");
    }

    #[test]
    fn visual_block_change_and_append_pads_short_lines() {
        let (mut lisp, editor) = lisp_with("abc\nd\nefg");
        at(&mut lisp, &editor, 1);
        lisp.dispatch_key("C-v", "");
        typed(&mut lisp, "jjlcX");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "aX\nd\neX", "a line too short is left alone");
        typed(&mut lisp, "u");
        at(&mut lisp, &editor, 2);
        lisp.dispatch_key("C-v", "");
        typed(&mut lisp, "jjA|");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "abc|\nd  |\nefg|", "A pads to the column");
    }

    #[test]
    fn visual_block_swap_corners() {
        let (mut lisp, editor) = lisp_with("abcd\nefgh");
        at(&mut lisp, &editor, 1);
        lisp.dispatch_key("C-v", "");
        typed(&mut lisp, "jl");
        typed(&mut lisp, "O");
        assert_eq!(point(&editor), 6, "same line, other side");
        assert_eq!(lisp.selection(), vec![(1, 3), (6, 8)]);
        typed(&mut lisp, "o");
        assert_eq!(point(&editor), 2);
    }

    #[test]
    fn steel_strings_count_characters() {
        let (mut lisp, _editor) = lisp_with("");
        assert_eq!(lisp.eval(r#"(string-length "▾ é")"#).unwrap(), "3");
        assert_eq!(lisp.eval(r#"(substring "▾ é" 2 3)"#).unwrap(), "\"é\"");
    }

    /// A throwaway project: src/main.rs, src/util/, README.md, an ignored
    /// target/, and gmacs opened on src/main.rs.
    fn tree_project(tag: &str) -> (Lisp, Editor, PathBuf) {
        let root = std::env::temp_dir().join(format!("gmacs-tree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/util")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("README.md"), "# hi\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("src/util/mod.rs"), "").unwrap();
        let (lisp, editor) = lisp_with("");
        editor.lock().open_file(&root.join("src/main.rs")).unwrap();
        (lisp, editor, root)
    }

    fn tree_text(editor: &Editor) -> String {
        let st = editor.lock();
        let id = st.buffer_id_named("*tree*").unwrap();
        st.buffer_by_id(id).unwrap().text().to_string()
    }

    #[test]
    fn spc_o_p_opens_the_tree_on_the_current_file_and_closes_it() {
        let (mut lisp, editor, root) = tree_project("open");
        typed(&mut lisp, " op");
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            tree_text(&editor),
            format!("{name}\n▾ src/\n  ▸ util/\n    main.rs\n  .gitignore\n  README.md"),
            "folders first, ignored target/ and .git hidden, src opened to show main.rs"
        );
        {
            let st = editor.lock();
            assert!(st.is_sidebar(st.selected_window()));
            assert_eq!(st.buffer().line(), 3, "the cursor is on main.rs");
        }
        typed(&mut lisp, " op");
        assert!(editor.lock().sidebar().is_none());
        assert_eq!(editor.lock().buffer().name(), "main.rs");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tree_folders_open_close_and_files_open_in_the_main_window() {
        let (mut lisp, editor, root) = tree_project("nav");
        typed(&mut lisp, " op");
        typed(&mut lisp, "k");
        lisp.dispatch_key("RET", "");
        assert!(tree_text(&editor).contains("▾ util/\n      mod.rs"), "{}", tree_text(&editor));
        typed(&mut lisp, "h");
        assert!(tree_text(&editor).contains("▸ util/"));
        typed(&mut lisp, "h");
        assert_eq!(editor.lock().buffer().line(), 1, "h on a closed folder goes to its parent");
        typed(&mut lisp, "G");
        lisp.dispatch_key("RET", "");
        let st = editor.lock();
        assert_eq!(st.buffer().name(), "README.md");
        assert!(!st.is_sidebar(st.selected_window()), "the file opened in the main window");
        assert!(st.sidebar().is_some(), "the tree stays open");
        drop(st);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tree_is_read_only_and_motions_work() {
        let (mut lisp, editor, root) = tree_project("ro");
        typed(&mut lisp, " op");
        let before = tree_text(&editor);
        typed(&mut lisp, "x");
        typed(&mut lisp, "ix");
        lisp.dispatch_key("ESC", "");
        assert_eq!(tree_text(&editor), before);
        typed(&mut lisp, "gg/readme\n");
        assert_eq!(editor.lock().buffer().line(), 5);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tree_adds_renames_and_deletes() {
        let (mut lisp, editor, root) = tree_project("ops");
        typed(&mut lisp, " op");
        typed(&mut lisp, "anew.txt\n");
        assert!(root.join("src/new.txt").exists(), "added next to main.rs, in src/");
        assert!(tree_text(&editor).contains("    new.txt"));
        typed(&mut lisp, "rrenamed.txt\n");
        assert!(root.join("src/renamed.txt").exists());
        assert!(!root.join("src/new.txt").exists());
        typed(&mut lisp, "dn\n");
        assert!(root.join("src/renamed.txt").exists(), "n keeps it");
        typed(&mut lisp, "dy\n");
        assert!(!root.join("src/renamed.txt").exists());
        typed(&mut lisp, "gganotes/\n");
        assert!(root.join("notes").is_dir(), "a trailing / makes a folder");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tree_keys_dont_become_the_last_change() {
        let (mut lisp, editor, root) = tree_project("dot");
        typed(&mut lisp, "ihi");
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, " op");
        typed(&mut lisp, "kR");
        lisp.dispatch_key("C-w", "");
        lisp.dispatch_key("l", "l");
        typed(&mut lisp, "0.");
        assert_eq!(editor.lock().buffer().text().to_string(), "hihifn main() {}\n", ". repeats the insert, not a tree key");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn face_specs_come_from_scheme() {
        let (mut lisp, _editor) = lisp_with("");
        lisp.eval(
            r#"(define (face-specs)
                 (list (list "big" 'color 'accent 'scale 1.5 'weight 'bold 'family 'sans)
                       (list "code" 'background 'muted 'slant 'italic 'underline #t)))"#,
        )
        .unwrap();
        let specs = lisp.face_specs();
        assert_eq!(specs[0].name, "big");
        assert_eq!(specs[0].color.as_deref(), Some("accent"));
        assert_eq!(specs[0].scale, Some(1.5));
        assert!(specs[0].bold && specs[0].sans && !specs[0].italic);
        assert_eq!(specs[1].background.as_deref(), Some("muted"));
        assert!(specs[1].italic && specs[1].underline);
    }

    #[test]
    fn markdown_headings_get_their_faces() {
        let (mut lisp, _editor) = lisp_with("");
        let theme = lisp.syntax_theme();
        let specs = lisp.face_specs();
        let face = crate::syntax::face_for("markup.heading.1", &theme).unwrap().to_string();
        let spec = specs.iter().find(|f| f.name == face).expect("the h1 face is defined");
        assert!(spec.scale.unwrap() > 1.5 && spec.bold && spec.sans);
        let marker = crate::syntax::face_for("markup.heading.1.marker", &theme).unwrap().to_string();
        let marker = specs.iter().find(|f| f.name == marker).unwrap();
        assert_eq!(marker.scale, spec.scale, "the # marks are as big as the heading");
    }

    #[test]
    fn readme_face_override_example() {
        let (mut lisp, _editor) = lisp_with("");
        lisp.eval(
            r#"(set-faces! (append (heading-faces 1 2.2 'magenta)
                                   (list (list "md-bold" 'weight 'bold 'color 'yellow))))"#,
        )
        .unwrap();
        let specs = lisp.face_specs();
        let first = |name: &str| specs.iter().find(|f| f.name == name).cloned().unwrap();
        assert_eq!(first("md-h1").scale, Some(2.2));
        assert_eq!(first("md-bold").color.as_deref(), Some("yellow"));
        assert_eq!(first("md-h2").scale, Some(1.5), "the rest keep their defaults");
    }

    fn markdown_buffer(text: &str) -> (Lisp, Editor) {
        let (lisp, editor) = lisp_with("");
        let path = std::env::temp_dir().join(format!("gmacs-table-{}-{}.md", std::process::id(), text.len()));
        std::fs::write(&path, text).unwrap();
        editor.lock().open_file(&path).unwrap();
        std::fs::remove_file(&path).ok();
        (lisp, editor)
    }

    #[test]
    fn leaving_insert_in_a_table_aligns_it() {
        let (mut lisp, editor) = markdown_buffer("# T\n\n| a | b |\n|-|-|\n| c | d |\n");
        typed(&mut lisp, "jjjjfca longer");
        lisp.dispatch_key("ESC", "");
        assert_eq!(
            editor.lock().buffer().text().to_string(),
            "# T\n\n| a        | b   |\n|----------|-----|\n| c longer | d   |\n"
        );
        typed(&mut lisp, "u");
        assert_eq!(editor.lock().buffer().text().to_string(), "# T\n\n| a | b |\n|-|-|\n| c | d |\n", "typing and aligning are one undo step");
    }

    #[test]
    fn tab_moves_through_cells_and_adds_a_row() {
        let (mut lisp, editor) = markdown_buffer("| name | n |\n|-|-|\n| x | 1 |");
        typed(&mut lisp, "jjli");
        lisp.dispatch_key("TAB", "");
        typed(&mut lisp, "2");
        lisp.dispatch_key("TAB", "");
        typed(&mut lisp, "y");
        lisp.dispatch_key("ESC", "");
        assert_eq!(
            editor.lock().buffer().text().to_string(),
            "| name | n   |\n|------|-----|\n| x    | 21  |\n| y    |     |"
        );
        typed(&mut lisp, "i");
        lisp.dispatch_key("S-TAB", "");
        typed(&mut lisp, "!");
        lisp.dispatch_key("ESC", "");
        assert!(
            editor.lock().buffer().text().to_string().contains("| x    | !21 |"),
            "S-TAB goes to the start of the previous cell"
        );
    }

    #[test]
    fn spc_m_a_aligns_and_tab_outside_tables_indents() {
        let (mut lisp, editor) = markdown_buffer("text\n|a|bb|\n");
        typed(&mut lisp, "j ma");
        assert_eq!(editor.lock().buffer().text().to_string(), "text\n| a   | bb  |\n");
        typed(&mut lisp, "ggi");
        lisp.dispatch_key("TAB", "");
        assert_eq!(editor.lock().buffer().text().to_string(), "    text\n| a   | bb  |\n");
    }

    #[test]
    fn tables_outside_markdown_are_left_alone() {
        let (mut lisp, editor) = lisp_with("| a | bb |");
        typed(&mut lisp, "A x");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "| a | bb | x");
    }

    fn with_diagnostics(text: &str) -> (Lisp, Editor) {
        let (lisp, editor) = lisp_with("");
        let path = std::env::temp_dir().join(format!("gmacs-diag-{}.rs", std::process::id()));
        std::fs::write(&path, text).unwrap();
        editor.lock().open_file(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let d = |line, col, severity: u8, message: &str| crate::lsp::Diagnostic {
            start: (line, col),
            end: (line, col + 3),
            severity,
            message: message.into(),
            source: None,
            raw: serde_json::Value::Null,
        };
        let abs = std::path::absolute(&path).unwrap();
        editor
            .lock()
            .lsp
            .set_diagnostics(abs, vec![d(2, 4, 2, "unused variable"), d(0, 3, 1, "mismatched types")]);
        (lisp, editor)
    }

    #[test]
    fn bracket_d_jumps_between_diagnostics() {
        let (mut lisp, editor) = with_diagnostics("fn main() {\n    let a = 1;\n    let b = 2;\n}\n");
        typed(&mut lisp, "]d");
        assert_eq!(editor.lock().buffer().point(), 3);
        assert_eq!(editor.lock().message, "error: mismatched types");
        typed(&mut lisp, "]d");
        {
            // One lock at a time: a second lock in the same expression would wait forever.
            let st = editor.lock();
            assert_eq!((st.buffer().line(), st.buffer().column()), (2, 4));
        }
        typed(&mut lisp, "]d");
        assert_eq!(editor.lock().buffer().point(), 3, "wraps around");
        typed(&mut lisp, "[d");
        assert_eq!(editor.lock().buffer().line(), 2, "and back");
    }

    #[test]
    fn diagnostics_show_in_the_modeline_and_picker() {
        let (mut lisp, editor) = with_diagnostics("fn main() {\n    let a = 1;\n    let b = 2;\n}\n");
        assert!(lisp.modeline().iter().any(|s| s.text == "E1 W1"), "{:?}", lisp.modeline());
        typed(&mut lisp, " cx");
        {
            let st = editor.lock();
            let p = st.picker.as_ref().expect("diagnostics picker");
            assert_eq!(p.title(), "Diagnostics");
            assert_eq!(p.selection(), Some("1:4  error: mismatched types"));
        }
        lisp.dispatch_key("C-n", "");
        lisp.dispatch_key("RET", "");
        let st = editor.lock();
        assert_eq!((st.buffer().line(), st.buffer().column()), (2, 4));
    }

    #[test]
    fn lsp_servers_come_from_scheme() {
        let (mut lisp, _editor) = lisp_with("");
        let servers = lisp.lsp_servers();
        let rust = servers.iter().find(|s| s.language == "rust").unwrap();
        assert_eq!(rust.command, "rust-analyzer");
        let py = servers.iter().find(|s| s.language == "python").unwrap();
        assert_eq!(py.args, vec!["--stdio"]);
    }

    #[test]
    fn diagnostics_jump_list_and_count_from_scheme() {
        let dir = std::env::temp_dir().join(format!("gmacs-lsp-scheme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let server = dir.join("server.py");
        std::fs::write(&server, crate::lsp::tests::FAKE_SERVER).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, "fn main() {}\nlet x = 1;\n").unwrap();

        let (mut lisp, editor) = lisp_with("");
        lisp.eval(&format!(r#"(define (lsp-servers) (list (list "rust" "python3" "{}")))"#, server.display())).unwrap();
        editor.lock().open_file(&file).unwrap();
        let mut got = false;
        for _ in 0..200 {
            let configs = lisp.lsp_servers();
            editor.lock().lsp_pump(configs);
            if !editor.lock().buffer_diagnostics().is_empty() {
                got = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(got, "the fake server published a diagnostic");
        assert!(lisp.modeline().iter().any(|s| s.text == "E1"), "{:?}", lisp.modeline());

        typed(&mut lisp, "jw]d");
        assert_eq!(editor.lock().buffer().point(), 0, "]d wraps around to the diagnostic at the top");
        { let m = editor.lock().message.clone(); assert!(m.starts_with("error: length"), "{m}"); }

        typed(&mut lisp, "j");
        typed(&mut lisp, " cx");
        assert!(editor.lock().picker.is_some());
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().buffer().point(), 0, "picking goes to it");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// gmacs on a file served by the fake language server (lsp.rs's tests).
    fn with_fake_server(tag: &str, text: &str) -> (Lisp, Editor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("gmacs-lsp-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let server = dir.join("server.py");
        std::fs::write(&server, crate::lsp::tests::FAKE_SERVER).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, text).unwrap();
        let (mut lisp, editor) = lisp_with("");
        lisp.eval(&format!(r#"(define (lsp-servers) (list (list "rust" "python3" "{}")))"#, server.display())).unwrap();
        editor.lock().open_file(&file).unwrap();
        (lisp, editor, dir)
    }

    /// Pumps the language servers until `done`, for up to four seconds.
    fn pump_until(lisp: &mut Lisp, editor: &Editor, done: impl Fn(&EditorState) -> bool) -> bool {
        for _ in 0..200 {
            let configs = lisp.lsp_servers();
            editor.lock().lsp_pump(configs);
            if done(&editor.lock()) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn gd_jumps_to_the_definition_and_ctrl_o_comes_back() {
        let (mut lisp, editor, dir) = with_fake_server("gd", "fn main() {}\nlet x = 1;\nx\n");
        assert!(pump_until(&mut lisp, &editor, |st| !st.buffer_diagnostics().is_empty()), "server up");
        typed(&mut lisp, "jj");
        typed(&mut lisp, "gd");
        assert!(
            pump_until(&mut lisp, &editor, |st| st.buffer().line() == 1),
            "went to line 2, where the fake server says it's defined"
        );
        assert_eq!(editor.lock().buffer().column(), 4);
        lisp.dispatch_key("C-o", "");
        let st = editor.lock();
        assert_eq!((st.buffer().line(), st.buffer().column()), (2, 0), "back where gd was pressed");
        drop(st);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn k_shows_documentation_until_the_next_key() {
        let (mut lisp, editor, dir) = with_fake_server("hover", "fn main() {}\nlet x = 1;\n");
        assert!(pump_until(&mut lisp, &editor, |st| !st.buffer_diagnostics().is_empty()), "server up");
        typed(&mut lisp, "jK");
        assert!(pump_until(&mut lisp, &editor, |st| st.hover.is_some()), "the hover arrived");
        assert_eq!(editor.lock().hover.as_deref(), Some("**doc** for line 1"));
        typed(&mut lisp, "l");
        assert!(editor.lock().hover.is_none(), "the next key closes it");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gd_without_a_server_says_so() {
        let (mut lisp, editor) = lisp_with("abc");
        typed(&mut lisp, "gd");
        assert_eq!(editor.lock().message, "This buffer has no file");
        typed(&mut lisp, "K");
        assert_eq!(editor.lock().message, "This buffer has no file");
    }

    #[test]
    fn gr_lists_references_in_the_picker() {
        let (mut lisp, editor, dir) = with_fake_server("gr", "fn main() {}\nlet x = 1;\nx\n");
        assert!(pump_until(&mut lisp, &editor, |st| !st.buffer_diagnostics().is_empty()), "server up");
        typed(&mut lisp, "gr");
        assert!(pump_until(&mut lisp, &editor, |st| !st.take_events_peek().is_empty()), "references came back");
        lisp.run_events();
        {
            let st = editor.lock();
            let p = st.picker.as_ref().expect("references picker");
            assert_eq!(p.title(), "References");
            assert_eq!(p.selection(), Some("main.rs:2:5  let x = 1;"));
        }
        lisp.dispatch_key("C-n", "");
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().buffer().line(), 2, "went to the second reference");
        lisp.dispatch_key("C-o", "");
        assert_eq!(editor.lock().buffer().line(), 0, "and C-o comes back");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn completion_pops_up_filters_and_accepts() {
        let (mut lisp, editor, dir) = with_fake_server("complete", "fn main() {}\n");
        assert!(pump_until(&mut lisp, &editor, |st| !st.buffer_diagnostics().is_empty()), "server up");
        typed(&mut lisp, "Gi");
        typed(&mut lisp, "g");
        assert!(pump_until(&mut lisp, &editor, |st| st.completion.is_some()), "completions came back");
        typed(&mut lisp, "re");
        {
            let st = editor.lock();
            let labels: Vec<_> = st.completion.as_ref().unwrap().items().map(|i| i.label.clone()).collect();
            assert_eq!(labels, vec!["greeting", "great()"], "filtered by \"gre\"");
        }
        lisp.dispatch_key("C-n", "");
        lisp.dispatch_key("RET", "");
        assert!(editor.lock().completion.is_none());
        assert_eq!(editor.lock().buffer().text().to_string(), "fn main() {}\ngreat(n)", "the snippet, as text");
        assert_eq!(lisp.cursor_shape(), "bar", "still typing");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn completion_closes_on_escape_and_non_word_keys() {
        let (mut lisp, editor, dir) = with_fake_server("complete-close", "fn main() {}\n");
        assert!(pump_until(&mut lisp, &editor, |st| !st.buffer_diagnostics().is_empty()), "server up");
        typed(&mut lisp, "Gig");
        assert!(pump_until(&mut lisp, &editor, |st| st.completion.is_some()));
        typed(&mut lisp, " ");
        assert!(editor.lock().completion.is_none(), "a space ends the word");
        typed(&mut lisp, "o");
        assert!(pump_until(&mut lisp, &editor, |st| st.completion.is_some()));
        lisp.dispatch_key("ESC", "");
        assert!(editor.lock().completion.is_none());
        assert_eq!(lisp.cursor_shape(), "box", "ESC also leaves insert state");
        typed(&mut lisp, "a");
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().buffer().text().to_string(), "fn main() {}\ng o\n", "RET is a newline with no popup");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spc_c_r_renames_with_the_current_name_to_start_from() {
        let (mut lisp, editor, dir) = with_fake_server("rename", "fn main() {}\nlet x = 1;\nx\n");
        assert!(pump_until(&mut lisp, &editor, |st| !st.buffer_diagnostics().is_empty()), "server up");
        typed(&mut lisp, "jw");
        typed(&mut lisp, " cr");
        assert_eq!(editor.lock().minibuffer.text().to_string(), "x", "starts from the current name");
        lisp.dispatch_key("DEL", "");
        typed(&mut lisp, "count\n");
        assert!(
            pump_until(&mut lisp, &editor, |st| st.buffer().text().to_string().contains("count")),
            "the rename's edits arrived"
        );
        let st = editor.lock();
        assert_eq!(st.buffer().text().to_string(), "fn main() {}\nlet count = 1;\ncount\n");
        assert_eq!(st.message, "2 edits in 1 file");
        drop(st);
        typed(&mut lisp, "u");
        assert_eq!(editor.lock().buffer().text().to_string(), "fn main() {}\nlet x = 1;\nx\n", "one undo step");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Opens SPC c a and picks the action titled starting with `prefix`.
    fn pick_code_action(lisp: &mut Lisp, editor: &Editor, prefix: &str) {
        typed(lisp, " ca");
        assert!(pump_until(lisp, editor, |st| !st.take_events_peek().is_empty()), "actions came back");
        lisp.run_events();
        let titles: Vec<String> = {
            let st = editor.lock();
            let p = st.picker.as_ref().expect("code actions picker");
            assert_eq!(p.title(), "Code actions");
            p.matches().iter().map(|m| m.item.clone()).collect()
        };
        let index = titles.iter().position(|t| t[3..].starts_with(prefix)).unwrap_or_else(|| panic!("{titles:?}"));
        for _ in 0..index {
            lisp.dispatch_key("C-n", "");
        }
        lisp.dispatch_key("RET", "");
    }

    #[test]
    fn spc_c_a_applies_an_action_with_its_edit() {
        let (mut lisp, editor, dir) = with_fake_server("action", "fn main() {}\nlet x = 1\n");
        assert!(pump_until(&mut lisp, &editor, |st| !st.buffer_diagnostics().is_empty()), "server up");
        typed(&mut lisp, "gg");
        pick_code_action(&mut lisp, &editor, "Add semicolon (1 diagnostics)");
        assert_eq!(editor.lock().buffer().text().to_string(), "fn main() {}\nlet x = 1;\n", "the diagnostic there was sent along");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spc_c_a_resolves_or_runs_actions_without_an_edit() {
        let (mut lisp, editor, dir) = with_fake_server("resolve", "fn main() {}\n");
        assert!(pump_until(&mut lisp, &editor, |st| !st.buffer_diagnostics().is_empty()), "server up");
        pick_code_action(&mut lisp, &editor, "Resolve me");
        assert!(
            pump_until(&mut lisp, &editor, |st| st.buffer().text().to_string().starts_with("// resolved\n")),
            "asked for the edit, then applied it"
        );
        pick_code_action(&mut lisp, &editor, "Run a command");
        assert!(
            pump_until(&mut lisp, &editor, |st| st.buffer().text().to_string().starts_with("// ran\n")),
            "the server ran it and sent the edit back"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_built_in_core_matches_lisp_core() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lisp/core");
        let on_disk: Vec<String> =
            scheme_files(&dir).iter().map(|f| f.file_name().unwrap().to_string_lossy().into_owned()).collect();
        let built_in: Vec<String> = CORE_FILES.iter().map(|(n, _)| n.to_string()).collect();
        assert_eq!(on_disk, built_in, "add new lisp/core files to CORE_FILES in lisp.rs");
        let from_dir = Source { label: "core".into(), path: Some(dir) }.read().unwrap();
        let embedded = Source { label: "core".into(), path: None }.read().unwrap();
        assert_eq!(from_dir, embedded);
    }

    #[test]
    fn lua_names_follow_scheme_names() {
        assert_eq!(lua_name("buffer-name"), "buffer_name");
        assert_eq!(lua_name("minibuffer-active?"), "minibuffer_active");
        assert_eq!(lua_name("pos->line"), "pos_to_line");
        assert_eq!(lua_name("set-faces!"), "set_faces");
    }

    #[test]
    fn lua_has_the_same_primitives() {
        let (mut lisp, editor) = lisp_with("hello");
        assert_eq!(lisp.eval_lua("gmacs.buffer_name()").unwrap(), "test");
        assert_eq!(lisp.eval_lua("gmacs.char_at(1)").unwrap(), "e");
        lisp.eval_lua("gmacs.goto_char(5); gmacs.insert(' world')").unwrap();
        assert_eq!(text(&editor), "hello world");
        assert_eq!(lisp.eval_lua("gmacs.pos_to_line(3)").unwrap(), "0");
        assert_eq!(lisp.eval_lua("#gmacs.buffer_list()").unwrap(), "1", "lists become tables");
    }

    #[test]
    fn lua_commands_and_keys_work_beside_scheme() {
        let (mut lisp, editor) = lisp_with("");
        lisp.eval_lua(r#"
            gmacs.command("insert-greeting", function() gmacs.insert("hi from lua") end)
            gmacs.map("normal", "SPC i g", "insert-greeting")
            gmacs.map("normal", "Q", function() gmacs.insert("Q!") end)
        "#)
        .unwrap();
        typed(&mut lisp, " ig");
        assert_eq!(text(&editor), "hi from lua");
        typed(&mut lisp, "Q");
        assert_eq!(text(&editor), "hi from luaQ!", "an anonymous function works too");
        typed(&mut lisp, "u");
        typed(&mut lisp, "0 :insert-greeting\n");
        assert_eq!(text(&editor).matches("hi from lua").count(), 2, "Lua commands run from M-x too: {:?}", text(&editor));
        // Scheme binds a Lua command by name, and Lua runs a Scheme command.
        lisp.eval(r#"(define-key normal-map "Z" 'insert-greeting)"#).unwrap();
        lisp.eval_lua(r#"gmacs.map("normal", "W", function() gmacs.run("delete-char-under") end)"#).unwrap();
        let before = text(&editor).len();
        typed(&mut lisp, "0W");
        assert_eq!(text(&editor).len(), before - 1);
    }

    #[test]
    fn colon_lua_evaluates_and_errors_are_shown() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, ":lua 1 + 2\n");
        assert_eq!(editor.lock().message, "3");
        typed(&mut lisp, ":lua print('a', 1)\n");
        assert_eq!(editor.lock().message, "a 1", "print shows in the echo area");
        typed(&mut lisp, ":lua nope(\n");
        assert!(editor.lock().message.starts_with("lua:"), "{}", editor.lock().message.clone());
        lisp.eval_lua(r#"gmacs.command("broken", function() error("oops") end)"#).unwrap();
        typed(&mut lisp, " :broken\n");
        { let m = editor.lock().message.clone(); assert!(m.contains("oops"), "{m}"); }
        typed(&mut lisp, "ix");
        assert_eq!(text(&editor), "x", "the editor keeps working");
    }

    #[test]
    fn lua_hooks_replace_the_modeline_servers_and_theme() {
        let (mut lisp, _editor) = lisp_with("");
        lisp.eval_lua(r#"
            gmacs.hooks.modeline = function() return { {gmacs.buffer_name(), "accent"}, {"lua!", "dim"} } end
            gmacs.hooks.lsp_servers = function() return { {"python", "pylsp"}, {"rust", "ra", "--flag"} } end
            gmacs.hooks.syntax_theme = function() return { {"keyword", "red"} } end
        "#)
        .unwrap();
        assert_eq!(
            lisp.modeline(),
            vec![Segment { text: "test".into(), face: "accent".into() }, Segment { text: "lua!".into(), face: "dim".into() }]
        );
        let servers = lisp.lsp_servers();
        assert_eq!(servers[1].command, "ra");
        assert_eq!(servers[1].args, vec!["--flag"]);
        assert_eq!(lisp.syntax_theme(), vec![("keyword".to_string(), "red".to_string())]);
        lisp.eval_lua("gmacs.hooks.modeline = function() error('bad') end").unwrap();
        assert!(lisp.modeline()[0].text.contains("bad"), "a broken hook shows its error");
    }

    #[test]
    fn init_lua_loads_after_scheme_and_reloads() {
        let dir = std::env::temp_dir().join(format!("gmacs-init-lua-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let init = dir.join("init.lua");
        std::fs::write(&init, r#"gmacs.map("normal", "Q", function() gmacs.insert("one") end)"#).unwrap();
        let editor = Editor::new(Buffer::new("t"));
        let core = Source { label: "core".into(), path: None };
        let mut lisp = Lisp::with_lua(editor.clone(), vec![core], vec![init.clone()]);
        assert_eq!(editor.lock().message, "loaded core, init.lua");
        typed(&mut lisp, "Q");
        assert_eq!(text(&editor), "one");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&init, r#"gmacs.map("normal", "Q", function() gmacs.insert("two") end)"#).unwrap();
        assert!(lisp.reload_if_changed());
        typed(&mut lisp, "0Q");
        assert_eq!(text(&editor), "twoone");
        std::fs::write(&init, "this is not lua").unwrap();
        lisp.reload();
        { let m = editor.lock().message.clone(); assert!(m.starts_with("init.lua:"), "{m}"); }
        typed(&mut lisp, "ix");
        assert!(text(&editor).contains('x'), "Scheme still works when init.lua is broken");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_example_init_lua_works() {
        let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/init.lua");
        let editor = Editor::new(Buffer::new("notes"));
        let mut lisp = Lisp::with_lua(editor.clone(), vec![Source { label: "core".into(), path: None }], vec![example]);
        assert_eq!(editor.lock().message, "loaded core, init.lua");
        typed(&mut lisp, " it");
        assert_eq!(text(&editor), "TODO: ");
        assert_eq!(lisp.cursor_shape(), "bar", "the Scheme command it ran put it in insert state");
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "0 id");
        assert_eq!(text(&editor).len(), "TODO: ".len() + "2026-01-01".len());
        let modeline = lisp.modeline();
        assert_eq!(modeline[0].text, "notes ●");
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
