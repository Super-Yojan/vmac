//! The Steel interpreter and the primitives it gets from the Rust core.
//!
//! Rust exposes small, fast primitives (`insert`, `forward-char`, ...). Every
//! command, keymap and the modeline are defined in Scheme (`lisp/core/*.scm`,
//! then the user's `~/.config/vmac/init.scm`) and can be redefined while the
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
    ("72-terminal.scm", include_str!("../lisp/core/72-terminal.scm")),
    ("75-file-tree.scm", include_str!("../lisp/core/75-file-tree.scm")),
    ("78-dashboard.scm", include_str!("../lisp/core/78-dashboard.scm")),
    ("80-lsp.scm", include_str!("../lisp/core/80-lsp.scm")),
    ("82-agent.scm", include_str!("../lisp/core/82-agent.scm")),
    ("85-bindings.scm", include_str!("../lisp/core/85-bindings.scm")),
    ("90-tables.scm", include_str!("../lisp/core/90-tables.scm")),
    ("92-syntax.scm", include_str!("../lisp/core/92-syntax.scm")),
    ("95-modeline.scm", include_str!("../lisp/core/95-modeline.scm")),
    ("97-lua.scm", include_str!("../lisp/core/97-lua.scm")),
    ("98-plugins.scm", include_str!("../lisp/core/98-plugins.scm")),
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
    /// Where plugins live (~/.config/vmac/plugins), and their files' state.
    plugin_dir: Option<PathBuf>,
    plugin_signature: Vec<(PathBuf, Option<SystemTime>)>,
    /// Each source's last text that loaded, by label, to fall back on.
    last_good: std::collections::HashMap<String, String>,
    /// When plugin-backed terminals were last read.
    last_terminal_poll: std::time::Instant,
}

impl Lisp {
    #[cfg(test)]
    pub fn new(editor: Editor, sources: Vec<Source>) -> Self {
        Self::with_lua(editor, sources, Vec::new())
    }

    #[cfg(test)]
    pub fn with_lua(editor: Editor, sources: Vec<Source>, lua_sources: Vec<PathBuf>) -> Self {
        Self::with_config(editor, sources, lua_sources, None)
    }

    /// `sources[0]` is the core; plugins load after it, then the rest
    /// (init.scm), then plugins' Lua, then `lua_sources` (init.lua).
    pub fn with_config(
        editor: Editor,
        sources: Vec<Source>,
        lua_sources: Vec<PathBuf>,
        plugin_dir: Option<PathBuf>,
    ) -> Self {
        editor.lock().plugin_dir = plugin_dir.clone();
        let lua = Arc::new(Mutex::new(fresh_lua(&editor)));
        let mut lisp = Self {
            engine: fresh_engine(&editor, &lua),
            lua,
            editor,
            stamps: vec![None; sources.len()],
            sources,
            lua_stamps: vec![None; lua_sources.len()],
            lua_sources,
            plugin_dir,
            plugin_signature: Vec::new(),
            last_good: Default::default(),
            last_terminal_poll: std::time::Instant::now(),
        };
        lisp.reload();
        lisp
    }

    /// ~/.config/vmac/plugins.
    pub fn default_plugin_dir() -> Option<PathBuf> {
        config_dir().map(|dir| dir.join("plugins"))
    }

    /// The user's Lua: ~/.config/vmac/init.lua.
    pub fn default_lua_sources() -> Vec<PathBuf> {
        config_dir().map(|dir| vec![dir.join("init.lua")]).unwrap_or_default()
    }

    /// Loads Lua into a fresh Lua (so a reload starts clean, like Scheme's):
    /// the plugins' first, each able to `require` from its own folder, then
    /// `lua_sources`. Runs the Scheme they queue. Returns what loaded, and
    /// errors (a plugin's name with its error) without stopping.
    fn load_lua(&mut self, plugins: &[crate::plugins::Plugin]) -> (Vec<String>, Vec<(Option<String>, String)>) {
        *self.lua.lock().unwrap_or_else(|e| e.into_inner()) = fresh_lua(&self.editor);
        let mut loaded = Vec::new();
        let mut errors = Vec::new();
        let plugin_files = plugins
            .iter()
            .filter(|p| !p.disabled)
            .filter_map(|p| Some((Some(p.name.clone()), p.lua.clone()?, Some(p.lua_root()))));
        for (i, path) in self.lua_sources.clone().iter().enumerate() {
            self.lua_stamps[i] = path.metadata().ok().and_then(|m| m.modified().ok());
        }
        let user_files = self.lua_sources.clone().into_iter().map(|p| (None, p, None));
        for (plugin, path, root) in plugin_files.chain(user_files).collect::<Vec<_>>() {
            let Ok(code) = std::fs::read_to_string(&path) else { continue };
            let label = plugin.clone().unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            let result = {
                let lua = self.lua.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(root) = &root {
                    // require("x") finds x.lua in the plugin's folder or its lua/.
                    let root = root.display().to_string().replace('\\', "/");
                    let _ = lua
                        .load(format!("package.path = {root:?} .. '/?.lua;' .. {root:?} .. '/lua/?.lua;' .. package.path"))
                        .exec();
                }
                lua.load(code).set_name(&label).exec()
            };
            self.run_pending_scheme();
            match result {
                Ok(()) => loaded.push(label),
                Err(e) => errors.push((plugin, format!("{label}: {}", first_line(&e)))),
            }
        }
        (loaded, errors)
    }

    /// Runs the Scheme Lua queued (vmac.scheme), quietly unless it fails.
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

    /// A function Lua put in `vmac.hooks` to replace Scheme's, if any.
    fn lua_hook<R: mlua::FromLuaMulti>(&self, name: &str) -> Option<mlua::Result<R>> {
        let lua = self.lua.lock().unwrap_or_else(|e| e.into_inner());
        let hook = lua
            .globals()
            .get::<mlua::Table>("vmac")
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
        if let Some(dir) = config_dir() {
            out.push(Source { label: "init.scm".into(), path: Some(dir.join("init.scm")) });
        }
        out
    }

    /// Loads everything again, in order: the core, plugins, the user's
    /// Scheme, then plugins' Lua and the user's Lua. Buffers are untouched.
    /// When something fails to load, the interpreter is rebuilt from what
    /// loaded before it (so the failure leaves nothing half-defined) and that
    /// source's last version that loaded is used, if there is one; the rest
    /// still load, and the error is shown. A failing plugin is marked failed.
    pub fn reload(&mut self) {
        use crate::plugins::Status;
        let plugins = self.plugin_dir.as_deref().map(crate::plugins::discover).unwrap_or_default();
        self.plugin_signature = self.plugin_dir.as_deref().map(crate::plugins::signature).unwrap_or_default();
        for (i, src) in self.sources.iter().enumerate() {
            self.stamps[i] = src.mtime();
        }

        // The Scheme to load, in order: (label, text, plugin name).
        let mut items: Vec<(String, Option<String>, Option<String>)> = Vec::new();
        if let Some(core) = self.sources.first() {
            items.push((core.label.clone(), core.read(), None));
        }
        for p in plugins.iter().filter(|p| !p.disabled) {
            if let Some(file) = &p.scheme {
                items.push((p.name.clone(), std::fs::read_to_string(file).ok(), Some(p.name.clone())));
            }
        }
        for src in self.sources.iter().skip(1) {
            items.push((src.label.clone(), src.read(), None));
        }

        let mut statuses: std::collections::HashMap<String, Status> = plugins
            .iter()
            .map(|p| (p.name.clone(), if p.disabled { Status::Disabled } else { Status::Loaded }))
            .collect();
        let mut errors: Vec<String> = Vec::new();
        let mut good: Vec<String> = Vec::new();
        let mut loaded: Vec<String> = Vec::new();
        for (label, text, plugin) in items {
            let Some(text) = text else { continue };
            match self.engine.run(text.clone()) {
                Ok(_) => {
                    good.push(text.clone());
                    self.last_good.insert(label.clone(), text);
                    if plugin.is_none() {
                        loaded.push(label);
                    }
                }
                Err(e) => {
                    let error = format!("{label}: {e}");
                    self.engine = fresh_engine(&self.editor, &self.lua);
                    for t in &good {
                        let _ = self.engine.run(t.clone());
                    }
                    let kept = self.last_good.get(&label).cloned().filter(|old| self.engine.run(old.clone()).is_ok());
                    match kept {
                        Some(old) => {
                            good.push(old);
                            errors.push(format!("{error} (kept the last version that loaded)"));
                        }
                        None => errors.push(error),
                    }
                    if let Some(name) = plugin {
                        statuses.insert(name, Status::Failed(first_line(&e)));
                    }
                }
            }
        }

        let (lua_loaded, lua_errors) = self.load_lua(&plugins);
        for (plugin, error) in lua_errors {
            if let Some(name) = plugin {
                statuses.insert(name, Status::Failed(error.clone()));
            }
            errors.push(error);
        }
        loaded.extend(lua_loaded.into_iter().filter(|l| !plugins.iter().any(|p| &p.name == l)));

        let enabled = plugins.iter().filter(|p| matches!(statuses.get(&p.name), Some(Status::Loaded))).count();
        let mut st = self.editor.lock();
        st.plugins = plugins.into_iter().map(|p| {
            let status = statuses.remove(&p.name).unwrap_or(Status::Loaded);
            (p, status)
        }).collect();
        st.message = match errors.first() {
            Some(first) if errors.len() > 1 => format!("{first} (and {} more)", errors.len() - 1),
            Some(first) => first.clone(),
            None => {
                if enabled > 0 {
                    loaded.insert(1.min(loaded.len()), format!("{enabled} plugin{}", if enabled == 1 { "" } else { "s" }));
                }
                format!("loaded {}", loaded.join(", "))
            }
        };
    }

    /// Reloads when any source file changed since the last load.
    pub fn reload_if_changed(&mut self) -> bool {
        let lua_changed = self
            .lua_sources
            .iter()
            .zip(&self.lua_stamps)
            .any(|(p, old)| p.metadata().ok().and_then(|m| m.modified().ok()) != *old);
        let plugins_changed = self
            .plugin_dir
            .as_deref()
            .is_some_and(|dir| crate::plugins::signature(dir) != self.plugin_signature);
        let requested = std::mem::take(&mut self.editor.lock().reload_requested);
        let changed = requested
            || lua_changed
            || plugins_changed
            || self.sources.iter().zip(&self.stamps).any(|(s, old)| s.mtime() != *old);
        if changed {
            self.reload();
        }
        changed
    }

    /// Reads terminals into their buffers: local ones every call, plugin
    /// ones (which run Lua, often a process) every 150 ms. Returns whether
    /// a screen changed.
    pub fn pump_terminals(&mut self) -> bool {
        let follow = match self.engine.call_function_by_name_with_args("terminal-insert?", vec![]) {
            Ok(SteelVal::BoolV(true)) => Some(self.editor.lock().window().buffer),
            _ => None,
        };
        let mut changed = self.editor.lock().pump_local_terminals(follow);
        if self.last_terminal_poll.elapsed() < std::time::Duration::from_millis(150) {
            return changed;
        }
        self.last_terminal_poll = std::time::Instant::now();
        let (plugins, kills) = {
            let mut st = self.editor.lock();
            let plugins: Vec<(crate::editor::BufferId, String, String)> = st
                .terminals
                .iter()
                .filter(|t| !t.exited)
                .filter_map(|t| match &t.backend {
                    crate::editor::TermBackend::Plugin { backend, handle } => Some((t.buffer, backend.clone(), handle.clone())),
                    _ => None,
                })
                .collect();
            (plugins, std::mem::take(&mut st.terminals_to_kill))
        };
        for (buffer, backend, handle) in plugins {
            let (text, alive) = {
                let lua = self.lua.lock().unwrap_or_else(|e| e.into_inner());
                let text = call_backend::<String>(&lua, &backend, "read", handle.clone());
                let alive = call_backend::<bool>(&lua, &backend, "alive", handle.clone()).unwrap_or(Ok(true));
                (text, alive)
            };
            let mut st = self.editor.lock();
            match text {
                Some(Ok(text)) => {
                    let unchanged = st.buffer_by_id(buffer).is_some_and(|b| *b.text() == *text.trim_end_matches('\n'));
                    if !unchanged {
                        st.show_screen(buffer, &crate::terminal::Screen::from_text(&text), follow == Some(buffer));
                        changed = true;
                    }
                }
                Some(Err(e)) => st.message = format!("{backend}: {}", first_line(&e)),
                None => {}
            }
            if matches!(alive, Ok(false))
                && let Some(t) = st.terminal_for(buffer)
            {
                t.exited = true;
                changed = true;
            }
        }
        for (backend, handle) in kills {
            let lua = self.lua.lock().unwrap_or_else(|e| e.into_inner());
            let _ = call_backend::<()>(&lua, &backend, "kill", handle);
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
    /// tree-sitter captures look. A Lua `vmac.hooks.syntax_theme` replaces it.
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
    /// Lua `vmac.hooks.lsp_servers` replaces it.
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
    /// `vmac.hooks.modeline` returning `{{text, face}, ...}` replaces it.
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
/// The user's config folder: ~/.config/vmac, or ~/.config/gmacs (the
/// editor's old name) while only that one exists.
pub fn config_dir() -> Option<PathBuf> {
    let config = PathBuf::from(std::env::var_os("HOME")?).join(".config");
    let (new, old) = (config.join("vmac"), config.join("gmacs"));
    Some(if !new.exists() && old.exists() { old } else { new })
}

/// Where primitives get registered: Steel, Lua's `vmac` table, or both.
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
        if let Some((lua, vmac)) = $reg.lua.as_ref() {
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
            vmac.set(lua_name($name), f).expect("set in vmac");
        }
    }};
}

/// Registers a primitive that doesn't need the editor, in each interpreter.
macro_rules! pure {
    ($reg:ident, $name:literal, |$($a:ident : $t:ty),*| $body:expr) => {{
        if let Some(engine) = $reg.engine.as_mut() {
            engine.register_fn($name, move |$($a: $t),*| $body);
        }
        if let Some((lua, vmac)) = $reg.lua.as_ref() {
            let f = lua.create_function(move |_, ($($a,)*): ($($t,)*)| Ok($body)).expect("a Lua function");
            vmac.set(lua_name($name), f).expect("set in vmac");
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
    // Terminals: start one with a backend ("" for the default), and send a
    // key to the selected one. Plugin backends run Lua, so no lock is held.
    let (ed, l) = (editor.clone(), lua.clone());
    engine.register_fn("terminal-open", move |backend: String, command: String| -> bool {
        let (backend, cwd, rows, cols, command) = {
            let st = ed.lock();
            let backend = if backend.is_empty() { st.terminal_backend.clone() } else { backend };
            // No command: the terminal shell, as a login shell (its config and PATH load).
            let command = if command.trim().is_empty() && !st.terminal_shell.is_empty() {
                format!("{} -l", st.terminal_shell)
            } else {
                command
            };
            let cwd = st
                .buffer()
                .file()
                .map(|f| crate::picker::project_root(f))
                .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("/"));
            let (rows, cols) = st.selected_size();
            (backend, cwd, rows, cols, command)
        };
        if backend == "local" {
            return match crate::terminal::LocalPty::start(&command, &cwd, rows, cols) {
                Ok(pty) => {
                    ed.lock().add_terminal(crate::editor::TermBackend::Local(pty));
                    true
                }
                Err(e) => {
                    ed.lock().message = format!("terminal: {e}");
                    false
                }
            };
        }
        // start returns a handle (then send/read are polled), or
        // { handle = ..., command = ... }: a command that attaches to the
        // terminal, run here on a PTY (keys, screen and size go through it).
        let started = {
            let lua = l.lock().unwrap_or_else(|e| e.into_inner());
            call_backend::<mlua::Value>(&lua, &backend, "start", (command, cwd.display().to_string(), rows, cols))
                .map(|r| {
                    r.and_then(|v| match v {
                        mlua::Value::Table(t) => Ok((t.get::<String>("handle")?, t.get::<Option<String>>("command")?)),
                        mlua::Value::String(s) => Ok((s.to_string_lossy().to_string(), None)),
                        mlua::Value::Nil => Ok((String::new(), None)),
                        other => Err(mlua::Error::runtime(format!("start returned a {}", other.type_name()))),
                    })
                })
        };
        match started {
            Some(Ok((handle, Some(attach)))) if !handle.is_empty() => {
                match crate::terminal::LocalPty::start(&attach, &cwd, rows, cols) {
                    Ok(pty) => {
                        ed.lock().add_terminal(crate::editor::TermBackend::Attached { backend, handle, pty });
                        true
                    }
                    Err(e) => {
                        ed.lock().message = format!("{backend}: {attach}: {e}");
                        false
                    }
                }
            }
            Some(Ok((handle, None))) if !handle.is_empty() => {
                ed.lock().add_terminal(crate::editor::TermBackend::Plugin { backend, handle });
                true
            }
            Some(Ok(_)) => {
                ed.lock().message = format!("{backend}: started nothing");
                false
            }
            Some(Err(e)) => {
                ed.lock().message = format!("{backend}: {}", first_line(&e));
                false
            }
            None => {
                ed.lock().message = format!("no terminal backend named {backend}");
                false
            }
        }
    });
    let (ed, l) = (editor.clone(), lua.clone());
    engine.register_fn("terminal-send", move |key: String, text: String| {
        let plugin = {
            let mut st = ed.lock();
            match st.terminal_here() {
                Some(t) if !t.exited => match &mut t.backend {
                    crate::editor::TermBackend::Local(pty) | crate::editor::TermBackend::Attached { pty, .. } => {
                        pty.send(&crate::terminal::key_bytes(&key, &text));
                        None
                    }
                    crate::editor::TermBackend::Plugin { backend, handle } => Some((backend.clone(), handle.clone())),
                },
                _ => None,
            }
        };
        if let Some((backend, handle)) = plugin {
            // A named key (enter, ctrl+c) or the text it types.
            let (text, name) = match crate::terminal::key_name(&key) {
                Some(name) => (None, Some(name)),
                None => (Some(text), None),
            };
            let result = {
                let lua = l.lock().unwrap_or_else(|e| e.into_inner());
                call_backend::<()>(&lua, &backend, "send", (handle, text, name))
            };
            if let Some(Err(e)) = result {
                ed.lock().message = format!("{backend}: {}", first_line(&e));
            }
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

/// A Lua state with the primitives in `vmac`, and the Lua side of the
/// bridge (`vmac.command`, `vmac.map`, `vmac.hooks`, ...).
fn fresh_lua(editor: &Editor) -> Lua {
    let lua = Lua::new();
    let vmac = lua.create_table().expect("a table");
    {
        let mut reg = Registry { engine: None, lua: Some((&lua, vmac.clone())) };
        register_primitives(&mut reg, editor);
    }
    lua.globals().set("vmac", vmac.clone()).expect("set vmac");
    // The editor's old name, so plugins written for it still load.
    lua.globals().set("gmacs", vmac).expect("set gmacs");
    lua.load(LUA_PRELUDE).set_name("prelude.lua").exec().expect("the Lua prelude runs");
    lua
}

/// The Lua side of the bridge to Scheme.
const LUA_PRELUDE: &str = r##"
vmac.commands = {}
-- Functions that replace Scheme's: modeline, lsp_servers, syntax_theme.
vmac.hooks = {}

local function quote(s)
  return '"' .. (s:gsub('\\', '\\\\'):gsub('"', '\\"')) .. '"'
end

-- A command callable by name from anywhere (keys, SPC :, Scheme).
function vmac.command(name, fn)
  vmac.commands[name] = fn
  vmac.scheme("(register-lua-command! '" .. name .. ")")
end

-- Binds keys ("SPC i d") in a state (normal, insert, visual, operator)
-- to a command name or a function.
local anonymous = 0
function vmac.map(state, keys, command)
  if type(command) == "function" then
    anonymous = anonymous + 1
    local name = "lua-command-" .. anonymous
    vmac.command(name, command)
    command = name
  end
  vmac.scheme("(lua-define-key '" .. state .. " " .. quote(keys) .. " '" .. command .. ")")
end

-- Terminal backends: where a terminal's program runs, beside the built-in
-- "local" PTY. A backend is a table of functions:
--   start(command, cwd, rows, cols)   (command "" means a shell) returns
--     a handle, then vmac polls send and read; or
--     { handle = h, command = "tool attach h" }: vmac runs that command on
--     a local PTY, which carries keys, screen and size (tmux, ssh, hc...)
--   send(handle, text, key)   text it types, or a key name (enter, ctrl+c, up)
--   read(handle) -> the screen as text
--   alive(handle) -> bool     (optional)
--   kill(handle)              (optional; also called for attached terminals)
vmac.terminal_backends = {}
function vmac.terminal_backend(name, backend)
  vmac.terminal_backends[name] = backend
end

-- Runs a command (Scheme's or Lua's) right after this one.
function vmac.run(name)
  vmac.scheme("(call-binding '" .. name .. ")")
end

-- print shows in the echo area.
function print(...)
  local parts = {}
  for i = 1, select("#", ...) do parts[#parts + 1] = tostring(select(i, ...)) end
  vmac.message(table.concat(parts, " "))
end
"##;

/// Calls a function of a plugin's terminal backend
/// (`vmac.terminal_backends[backend][name]`); None when it has no such function.
fn call_backend<R: mlua::FromLuaMulti>(
    lua: &Lua,
    backend: &str,
    name: &str,
    args: impl mlua::IntoLuaMulti,
) -> Option<mlua::Result<R>> {
    let f = lua
        .globals()
        .get::<mlua::Table>("vmac")
        .and_then(|g| g.get::<mlua::Table>("terminal_backends"))
        .and_then(|b| b.get::<mlua::Table>(backend))
        .and_then(|b| b.get::<mlua::Function>(name))
        .ok()?;
    Some(f.call::<R>(args))
}

fn first_line(e: &impl std::fmt::Display) -> String {
    e.to_string().lines().next().unwrap_or("").to_string()
}

/// Runs a Lua command registered with vmac.command.
fn lua_command(lua: &Lua, name: &str) -> mlua::Result<()> {
    let vmac: mlua::Table = lua.globals().get("vmac")?;
    let commands: mlua::Table = vmac.get("commands")?;
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
            .or_else(|| s.project.clone())
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
    prim!(reg, editor, "buffer-language", |s| {
        if let Some(t) = s.terminal_here() {
            let exited = if t.exited { ", exited" } else { "" };
            return format!("terminal ({}{exited})", t.backend_name());
        }
        if s.agent_here() {
            return format!("agent ({})", s.agent_status());
        }
        if s.dashboard_here() {
            return "dashboard".into();
        }
        if s.review_here() {
            return "review".into();
        }
        s.buffer().file().and_then(|f| crate::syntax::language_for_path(f)).unwrap_or("text").to_string()
    });
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
    prim!(reg, editor, "lsp-format", |s| s.lsp_format(None));
    prim!(reg, editor, "lsp-format-range", |s, start: isize, end: isize| s
        .lsp_format(Some((start.max(0) as usize, end.max(0) as usize))));
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

    // Plugins.
    prim!(reg, editor, "plugin-list", |s| s
        .plugins
        .iter()
        .map(|(p, status)| {
            let state = match status {
                crate::plugins::Status::Loaded => "loaded".to_string(),
                crate::plugins::Status::Disabled => "off".to_string(),
                crate::plugins::Status::Failed(e) => format!("failed: {e}"),
            };
            format!("{}  ({})  {state}", p.name, p.languages())
        })
        .collect::<Vec<_>>());
    prim!(reg, editor, "plugin-dir", |s| s.plugin_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default());
    // Turns a plugin on or off, then reloads.
    prim!(reg, editor, "plugin-toggle", |s, name: String| {
        let Some((plugin, _)) = s.plugins.iter().find(|(p, _)| p.name == name).cloned() else {
            s.message = format!("no plugin named {name}");
            return;
        };
        match crate::plugins::set_disabled(&plugin, !plugin.disabled) {
            Ok(()) => {
                s.message = format!("{name} {}", if plugin.disabled { "on" } else { "off" });
                s.reload_requested = true;
            }
            Err(e) => s.message = format!("{name}: {e}"),
        }
    });
    prim!(reg, editor, "plugin-remove", |s, name: String| {
        let Some(dir) = s.plugin_dir.clone() else { return };
        s.message = match crate::plugins::remove(&dir, &name) {
            Ok(()) => {
                s.reload_requested = true;
                format!("removed {name}")
            }
            Err(e) => e,
        };
    });
    // git clone and pull run off the UI thread; they reload when done.
    {
        let ed = editor.clone();
        let install = move |url: String| {
            let Some(dir) = ed.lock().plugin_dir.clone() else { return };
            ed.lock().message = format!("installing {url}...");
            let ed = ed.clone();
            std::thread::spawn(move || {
                let result = crate::plugins::install(&dir, &url);
                let mut st = ed.lock();
                st.message = match result {
                    Ok(name) => {
                        st.reload_requested = true;
                        format!("installed {name}")
                    }
                    Err(e) => format!("install failed: {e}"),
                };
            });
        };
        if let Some(engine) = reg.engine.as_mut() {
            engine.register_fn("plugin-install", install.clone());
        }
        if let Some((lua, vmac)) = reg.lua.as_ref() {
            let f = lua.create_function(move |_, url: String| {
                install(url);
                Ok(())
            });
            vmac.set("plugin_install", f.expect("a Lua function")).expect("set in vmac");
        }
    }
    {
        let ed = editor.clone();
        let update = move || {
            let Some(dir) = ed.lock().plugin_dir.clone() else { return };
            ed.lock().message = "updating plugins...".into();
            let ed = ed.clone();
            std::thread::spawn(move || {
                let results = crate::plugins::update_all(&dir);
                let failed: Vec<String> =
                    results.iter().filter_map(|(n, r)| r.as_ref().err().map(|e| format!("{n}: {e}"))).collect();
                let mut st = ed.lock();
                st.reload_requested = true;
                st.message = if failed.is_empty() {
                    format!("updated {} plugin{}", results.len(), if results.len() == 1 { "" } else { "s" })
                } else {
                    failed.join("; ")
                };
            });
        };
        if let Some(engine) = reg.engine.as_mut() {
            engine.register_fn("plugin-update", update.clone());
        }
        if let Some((lua, vmac)) = reg.lua.as_ref() {
            let f = lua.create_function(move |_, ()| {
                update();
                Ok(())
            });
            vmac.set("plugin_update", f.expect("a Lua function")).expect("set in vmac");
        }
    }

    // Terminals.
    prim!(reg, editor, "terminal-here?", |s| s.terminal_here().is_some());
    prim!(reg, editor, "terminal-buffer-name", |s| {
        let id = s.terminals.iter().find(|t| !t.exited).map(|t| t.buffer);
        id.and_then(|id| s.buffer_by_id(id)).map(|b| b.name().to_string()).unwrap_or_default()
    });
    prim!(reg, editor, "set-terminal-backend", |s, name: String| s.terminal_backend = name);
    prim!(reg, editor, "terminal-backend", |s| s.terminal_backend.clone());
    prim!(reg, editor, "set-terminal-shell", |s, shell: String| s.terminal_shell = shell);
    // The editor's font: a family name ("" for the theme's monospace font).
    prim!(reg, editor, "set-font", |s, name: String| {
        if name.is_empty() {
            s.font = None;
            s.message = "the theme's font".into();
        } else if !s.fonts.is_empty() && !s.fonts.contains(&name) {
            s.message = format!("no font named {name} (:fonts lists them)");
        } else {
            s.message = format!("font: {name}");
            s.font = Some(name);
        }
    });
    prim!(reg, editor, "font", |s| s.font.clone().unwrap_or_default());
    // Light or dark: "system" follows the system's appearance as it changes.
    prim!(reg, editor, "set-theme", |s, mode: String| {
        if matches!(mode.as_str(), "system" | "light" | "dark") {
            s.message = format!("theme: {mode}");
            s.theme_mode = mode;
        } else {
            s.message = format!("{mode}: the theme is system, light or dark");
        }
    });
    prim!(reg, editor, "theme", |s| s.theme_mode.clone());
    // The coding agent (ACP): prompts, permission answers, how to start it.
    prim!(reg, editor, "agent-prompt", |s, text: String| s.agent_prompt(&text));
    prim!(reg, editor, "agent-show", |s| s.agent_show());
    prim!(reg, editor, "agent-focus-prompt", |s| s.agent_focus_prompt());
    prim!(reg, editor, "agent-here?", |s| s.agent_here());
    prim!(reg, editor, "agent-in-prompt?", |s| s.agent_in_prompt());
    prim!(reg, editor, "agent-activate", |s| s.agent_activate());
    prim!(reg, editor, "agent-toggle-at-point", |s| s.agent_toggle_at_point());
    prim!(reg, editor, "agent-new-session", |s| s.agent_new_session());
    // Projects, recent files and the dashboard.
    pure!(reg, "file-exists?", |path: String| expand_home(&path).exists());
    prim!(reg, editor, "config-dir", |_s| config_dir().map(|d| d.display().to_string()).unwrap_or_default());
    prim!(reg, editor, "recent-files", |s| s.history.files().iter().map(|p| p.display().to_string()).collect::<Vec<_>>());
    prim!(reg, editor, "known-projects", |s| s.history.projects().iter().map(|p| p.display().to_string()).collect::<Vec<_>>());
    prim!(reg, editor, "project-candidates", |s| {
        s.project_candidates().iter().map(|p| crate::projects::abbreviate(p)).collect::<Vec<_>>()
    });
    // Opens a folder as the project; its full path, or "" (and why, in the echo area).
    prim!(reg, editor, "project-open", |s, path: String| {
        match s.project_open(&crate::projects::expand(&path)) {
            Ok(p) => p.display().to_string(),
            Err(e) => {
                s.message = e;
                String::new()
            }
        }
    });
    prim!(reg, editor, "project-forget", |s, path: String| s.history.forget_project(&crate::projects::expand(&path)));
    pure!(reg, "abbreviate-path", |path: String| crate::projects::abbreviate(Path::new(&path)));
    prim!(reg, editor, "dashboard-show", |s, banner: Vec<String>, items: Vec<Vec<String>>| {
        let items = items
            .into_iter()
            .filter_map(|i| Some((i.first()?.clone(), i.get(1)?.clone(), i.get(2)?.clone())))
            .collect();
        s.dashboard_show(banner, items)
    });
    prim!(reg, editor, "dashboard-here?", |s| s.dashboard_here());
    prim!(reg, editor, "dashboard-action", |s| s.dashboard_action());
    prim!(reg, editor, "dashboard-project", |s, n: isize| s.dashboard_project(n.max(0) as usize));
    // Reviewing the agent's changes.
    prim!(reg, editor, "review-show", |s| s.review_show());
    prim!(reg, editor, "review-here?", |s| s.review_here());
    prim!(reg, editor, "review-visit", |s| s.review_visit());
    prim!(reg, editor, "review-decide", |s, accept: bool| s.review_decide(accept));
    prim!(reg, editor, "review-decide-all", |s, accept: bool, everywhere: bool| s.review_decide_all(accept, everywhere));
    prim!(reg, editor, "review-jump", |s, forward: bool| s.review_jump(forward));
    prim!(reg, editor, "review-count", |s| s.review_count() as isize);
    prim!(reg, editor, "set-agent-review", |s, on: bool| {
        s.agent_review = on;
        s.message = if on { "The agent's changes wait for review".into() } else { "The agent's changes are accepted as made".into() };
    });
    prim!(reg, editor, "agent-ask-about", |s, start: isize, end: isize, question: String| {
        s.agent_ask_about(start.max(0) as usize, end.max(0) as usize, &question)
    });
    prim!(reg, editor, "agent-cancel", |s| s.agent_cancel());
    prim!(reg, editor, "agent-stop", |s| s.agent_stop());
    prim!(reg, editor, "agent-status", |s| s.agent_status());
    prim!(reg, editor, "agent-permission-options", |s| s.agent_permission_options());
    prim!(reg, editor, "agent-permission-title", |s| s.agent_permission_title());
    prim!(reg, editor, "agent-permit", |s, index: isize| s.agent_permit(index.max(0) as usize));
    prim!(reg, editor, "set-agent-command", |s, line: String| {
        match crate::agent::AgentConfig::parse("agent", &line) {
            Some(config) => {
                s.message = format!("agent: {line} (from the next start)");
                s.agent_config = config;
            }
            None => s.message = "set-agent-command needs a command".into(),
        }
    });
    prim!(reg, editor, "agent-command", |s| {
        std::iter::once(s.agent_config.command.clone()).chain(s.agent_config.args.iter().cloned()).collect::<Vec<_>>().join(" ")
    });
    prim!(reg, editor, "font-list", |s| s.fonts.clone());
    prim!(reg, editor, "terminal-shell", |s| s.terminal_shell.clone());
    // Runs a program, returning what it printed (for plugins: hc, rex, ...).
    pure!(reg, "exec", |command: String, args: Vec<String>| {
        std::process::Command::new(&command)
            .args(&args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    });

    // Scheme for Lua to run (vmac.scheme), silently, after the current command.
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
        let dir = std::env::temp_dir().join(format!("vmac-test-{}", std::process::id()));
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
    /// target/, and vmac opened on src/main.rs.
    fn tree_project(tag: &str) -> (Lisp, Editor, PathBuf) {
        let root = std::env::temp_dir().join(format!("vmac-tree-{tag}-{}", std::process::id()));
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
        let path = std::env::temp_dir().join(format!("vmac-table-{}-{}.md", std::process::id(), text.len()));
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
        let path = std::env::temp_dir().join(format!("vmac-diag-{}.rs", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("vmac-lsp-scheme-{}", std::process::id()));
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

    #[test]
    fn the_agent_reads_unsaved_text_asks_and_writes() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-e2e-{}", std::process::id()));
        let config = crate::agent::tests::fake_config(&dir);
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hello\n").unwrap();

        let (mut lisp, editor) = lisp_with("");
        editor.lock().agent_config = config;
        editor.lock().open_file(&file).unwrap();
        typed(&mut lisp, "Aworld");
        lisp.dispatch_key("ESC", "");
        assert_eq!(editor.lock().buffer().text().to_string(), "helloworld\n");

        typed(&mut lisp, &format!(":agent edit {}", file.display()));
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().buffer().name(), "*agent*");
        let pump = |lisp: &mut Lisp, done: &dyn Fn(&EditorState) -> bool| {
            for _ in 0..250 {
                editor.lock().agent_pump();
                lisp.run_events();
                if done(&editor.lock()) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        };
        assert!(pump(&mut lisp, &|s| s.picker.is_some()), "the permission picker opened: {}", editor.lock().message);
        assert!(editor.lock().picker.as_ref().unwrap().title().contains("Allow Edit"));
        lisp.dispatch_key("RET", "");
        assert!(pump(&mut lisp, &|s| s.agent_status() == "ready" && s.buffer().text().to_string().contains("✓ Edit")));

        let transcript = editor.lock().buffer().text().to_string();
        assert!(transcript.contains("› edit"), "{transcript}");
        assert!(transcript.contains("Reading it."), "{transcript}");
        assert!(transcript.contains("  ✓ Read it\n  ○ Shout it"), "{transcript}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "HELLOWORLD\n", "it read the unsaved text");
        editor.lock().open_file(&file).unwrap();
        assert_eq!(editor.lock().buffer().text().to_string(), "HELLOWORLD\n");
        assert!(!editor.lock().buffer().is_modified(), "and saved it");
        typed(&mut lisp, "u");
        assert_eq!(editor.lock().buffer().text().to_string(), "helloworld\n", "one u undoes the agent's edit");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_agent_buffer_has_a_prompt_folds_and_questions_about_code() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-chat-{}", std::process::id()));
        let config = crate::agent::tests::fake_config(&dir);
        let file = dir.join("lib.rs");
        std::fs::write(&file, "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        let (mut lisp, editor) = lisp_with("");
        editor.lock().agent_config = config;
        editor.lock().open_file(&file).unwrap();
        let pump = |lisp: &mut Lisp, done: &dyn Fn(&EditorState) -> bool| {
            for _ in 0..250 {
                editor.lock().agent_pump();
                lisp.run_events();
                if done(&editor.lock()) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        };
        let text = || editor.lock().buffer().text().to_string();

        // SPC a a: the prompt, in insert state; RET (normal) sends it.
        typed(&mut lisp, " aa");
        assert_eq!(editor.lock().buffer().name(), "*agent*");
        assert!(text().contains("Ask fake anything"), "{}", text());
        typed(&mut lisp, "hello there");
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "bdw");
        assert!(text().ends_with("─\nhello "), "Vim keys edit the prompt: {:?}", text());
        typed(&mut lisp, "gg");
        typed(&mut lisp, "dd");
        assert!(text().starts_with("Ask fake"), "the conversation can't be changed");
        typed(&mut lisp, "G$x");
        assert!(text().ends_with("─\nhello"), "{:?}", text());
        lisp.dispatch_key("RET", "");
        assert!(pump(&mut lisp, &|s| s.buffer().text().to_string().contains("You said: hello")), "{} / {}", text(), editor.lock().message);
        assert!(text().contains("› hello\n"), "{}", text());
        assert!(text().ends_with("─\n"), "the prompt was cleared: {:?}", text());

        // Typing while it answers keeps the prompt below the conversation.
        typed(&mut lisp, "inext");
        lisp.dispatch_key("ESC", "");
        assert!(text().ends_with("─\nnext"));

        // A tool call: TAB opens it; RET on its file visits it.
        let path = file.display().to_string();
        editor.lock().agent.as_mut().unwrap().prompt(&format!("edit {path}"));
        assert!(pump(&mut lisp, &|s| s.picker.is_some()));
        lisp.dispatch_key("RET", "");
        assert!(pump(&mut lisp, &|s| s.agent_status() == "ready" && s.buffer().text().to_string().contains("▸ ✓ Edit lib.rs")), "{}", text());
        assert!(text().ends_with("─\nnext"), "the prompt survived: {:?}", text());
        let line = text().lines().position(|l| l.contains("▸ ✓ Edit")).unwrap();
        let pos = editor.lock().buffer().line_to_pos(line);
        editor.lock().buffer_mut().goto_char(pos);
        lisp.dispatch_key("TAB", "");
        assert!(text().contains("▾ ✓ Edit lib.rs\n    (no output)"), "{}", text());
        assert_eq!(editor.lock().buffer().line(), line, "the point stays on the tool");
        lisp.dispatch_key("TAB", "");
        assert!(text().contains("▸ ✓ Edit lib.rs"));

        // SPC a s on a selection: the question, its place and its code.
        editor.lock().open_file(&file).unwrap();
        typed(&mut lisp, "jVj");
        typed(&mut lisp, " as");
        typed(&mut lisp, "why");
        lisp.dispatch_key("RET", "");
        assert!(pump(&mut lisp, &|s| s.buffer().text().to_string().contains("You said: why")), "{}", text());
        assert!(text().contains("› why  [lib.rs:2-3]"), "{}", text());
        assert!(text().contains("FN B() {}\nFN C() {}"), "the code went with it: {}", text());

        // SPC a n: a new conversation.
        typed(&mut lisp, " an");
        assert!(pump(&mut lisp, &|s| s.agent_status() == "ready"));
        assert!(!text().contains("You said"), "{}", text());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_agents_edits_wait_for_review() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-review-{}", std::process::id()));
        let config = crate::agent::tests::fake_config(&dir);
        let file = dir.join("a.txt");
        let other = dir.join("b.txt");
        let original = "one\ntwo\nthree\nfour\nfive\n";
        std::fs::write(&file, original).unwrap();
        std::fs::write(&other, original).unwrap();
        let (mut lisp, editor) = lisp_with("");
        editor.lock().agent_config = config;
        editor.lock().open_file(&file).unwrap();
        typed(&mut lisp, "2jl");
        let pump = |lisp: &mut Lisp, done: &dyn Fn(&mut EditorState) -> bool| {
            for _ in 0..250 {
                editor.lock().agent_pump();
                lisp.run_events();
                if done(&mut editor.lock()) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        };
        let disk = |p: &Path| std::fs::read_to_string(p).unwrap();

        // The agent patches a.txt (open) and b.txt (not open).
        editor.lock().agent_prompt(&format!("patch {} {}", file.display(), other.display()));
        assert!(pump(&mut lisp, &|s| s.agent_status() == "ready" && s.review_count() == 6), "{}", editor.lock().review_count());
        let patched = "one\nTWO\nthree\nfive\nadded\n";
        assert_eq!(disk(&file), patched, "made and saved at once");
        assert_eq!(disk(&other), patched);
        assert_eq!(editor.lock().buffer().name(), "a.txt");
        let at = { let st = editor.lock(); (st.buffer().line(), st.buffer().column()) };
        assert_eq!(at, (2, 1), "the cursor stayed put");
        let id = editor.lock().window().buffer;
        assert_eq!(editor.lock().review_marks(id), vec![(1, true), (3, false), (4, true)]);
        editor.lock().agent_show();
        assert!(text(&editor).contains("◆ 6 changes to review"), "{}", text(&editor));
        editor.lock().open_file(&file).unwrap();

        // ]h to the first, gx rejects it (saved), u brings it back.
        typed(&mut lisp, "gg]h");
        assert_eq!(editor.lock().buffer().line(), 1);
        typed(&mut lisp, "gx");
        assert_eq!(disk(&file), "one\ntwo\nthree\nfive\nadded\n");
        assert_eq!(editor.lock().review_count(), 5);
        typed(&mut lisp, "u");
        assert_eq!(text(&editor), patched);
        assert_eq!(editor.lock().review_count(), 6, "undone, it waits again");

        // ga accepts; the deletion, then rejected: "four" comes back.
        typed(&mut lisp, "gg]h");
        typed(&mut lisp, "ga");
        assert_eq!(editor.lock().review_count(), 5);
        typed(&mut lisp, "]h");
        assert_eq!(editor.lock().buffer().line(), 3);
        typed(&mut lisp, "gx");
        assert_eq!(disk(&file), "one\nTWO\nthree\nfour\nfive\nadded\n");

        // SPC a r lists the rest; keys work there too.
        typed(&mut lisp, " ar");
        assert_eq!(editor.lock().buffer().name(), "*agent-review*");
        let review = text(&editor);
        assert!(review.contains("a.txt  (1 change)\n@@ line 6\n+ added"), "{review}");
        assert!(review.contains("b.txt  (3 changes)\n@@ line 2\n- two\n+ TWO"), "{review}");
        typed(&mut lisp, "gg]h");
        typed(&mut lisp, "gx");
        assert_eq!(disk(&file), "one\nTWO\nthree\nfour\nfive\n", "rejected from the list");
        assert!(!text(&editor).contains("a.txt"), "{}", text(&editor));
        typed(&mut lisp, "]h");
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().buffer().name(), "b.txt");
        assert_eq!(editor.lock().buffer().line(), 1, "RET went to the change");

        // SPC a A takes the rest.
        typed(&mut lisp, " aA");
        assert_eq!(editor.lock().review_count(), 0);
        assert_eq!(disk(&other), patched);
        editor.lock().agent_show();
        editor.lock().agent_pump();
        assert!(!text(&editor).contains("to review"), "{}", text(&editor));

        // With review off, edits just land.
        lisp.eval("(set-agent-review #f)").unwrap();
        let fresh = dir.join("c.txt");
        std::fs::write(&fresh, original).unwrap();
        editor.lock().agent_prompt(&format!("patch {}", fresh.display()));
        assert!(pump(&mut lisp, &|s| s.agent_status() == "ready" && disk(&fresh) == patched));
        assert_eq!(editor.lock().review_count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_dashboard_opens_recent_files_and_projects() {
        let dir = std::env::temp_dir().join(format!("vmac-dashboard-{}", std::process::id()));
        let (one, two) = (dir.join("one"), dir.join("two"));
        for repo in [&one, &two] {
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            std::fs::write(repo.join("main.rs"), "fn main() {}\n").unwrap();
        }
        let (mut lisp, editor) = lisp_with("");
        editor.lock().open_file(&one.join("main.rs")).unwrap();
        assert!(lisp.eval("dashboard-on-start").unwrap().contains("#t"));

        lisp.eval("(call-binding 'dashboard)").unwrap();
        let shown = text(&editor);
        assert_eq!(editor.lock().buffer().name(), "*dashboard*");
        assert!(shown.contains("███"), "the banner");
        assert!(shown.contains("f   Find file"), "{shown}");
        assert!(shown.contains("Projects"), "{shown}");
        let listed = format!("1   {}", crate::projects::abbreviate(&one));
        assert!(shown.contains(&listed), "{shown}");
        assert!(text(&editor).lines().nth(editor.lock().buffer().line()).unwrap().contains("Find file"), "the cursor starts on the first item");

        // RET on "Recent files" (one line down) lists the file opened.
        typed(&mut lisp, "j");
        lisp.dispatch_key("RET", "");
        let picker = editor.lock().picker.as_ref().map(|p| p.title().to_string());
        assert_eq!(picker.as_deref(), Some("Recent files"));
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().buffer().file(), Some(&one.join("main.rs")));

        // 1 on the dashboard opens the first project's file picker.
        typed(&mut lisp, " bh");
        typed(&mut lisp, "1");
        let picker = editor.lock().picker.as_ref().map(|p| (p.title().to_string(), p.selection().map(str::to_string)));
        assert_eq!(picker, Some(("Find file".into(), Some("main.rs".into()))));
        lisp.dispatch_key("ESC", "");
        lisp.dispatch_key("ESC", "");

        // Another folder by path (SPC p a): it becomes the project.
        lisp.eval(&format!("(project-switch-to \"{}\")", two.display())).unwrap();
        assert_eq!(editor.lock().project.as_ref(), Some(&two));
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().buffer().file(), Some(&two.join("main.rs")));
        lisp.eval("(call-binding 'dashboard)").unwrap();
        let shown = text(&editor);
        let first = shown.find(&crate::projects::abbreviate(&two)).unwrap();
        let second = shown.find(&crate::projects::abbreviate(&one)).unwrap();
        assert!(first < second, "the latest project comes first: {shown}");

        // Not a folder: says so.
        lisp.eval("(project-switch-to \"/no/such/folder\")").unwrap();
        assert!(editor.lock().message.contains("not a folder"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// vmac on a file served by the fake language server (lsp.rs's tests).
    fn with_fake_server(tag: &str, text: &str) -> (Lisp, Editor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("vmac-lsp-{tag}-{}", std::process::id()));
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
    fn plugins_using_the_old_name_still_work() {
        let (mut lisp, editor) = lisp_with("");
        lisp.eval_lua(r#"gmacs.command("old", function() gmacs.insert("still works") end)"#).unwrap();
        typed(&mut lisp, " :old\n");
        assert_eq!(text(&editor), "still works");
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
        assert_eq!(lisp.eval_lua("vmac.buffer_name()").unwrap(), "test");
        assert_eq!(lisp.eval_lua("vmac.char_at(1)").unwrap(), "e");
        lisp.eval_lua("vmac.goto_char(5); vmac.insert(' world')").unwrap();
        assert_eq!(text(&editor), "hello world");
        assert_eq!(lisp.eval_lua("vmac.pos_to_line(3)").unwrap(), "0");
        assert_eq!(lisp.eval_lua("#vmac.buffer_list()").unwrap(), "1", "lists become tables");
    }

    #[test]
    fn lua_commands_and_keys_work_beside_scheme() {
        let (mut lisp, editor) = lisp_with("");
        lisp.eval_lua(r#"
            vmac.command("insert-greeting", function() vmac.insert("hi from lua") end)
            vmac.map("normal", "SPC i g", "insert-greeting")
            vmac.map("normal", "Q", function() vmac.insert("Q!") end)
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
        lisp.eval_lua(r#"vmac.map("normal", "W", function() vmac.run("delete-char-under") end)"#).unwrap();
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
        lisp.eval_lua(r#"vmac.command("broken", function() error("oops") end)"#).unwrap();
        typed(&mut lisp, " :broken\n");
        { let m = editor.lock().message.clone(); assert!(m.contains("oops"), "{m}"); }
        typed(&mut lisp, "ix");
        assert_eq!(text(&editor), "x", "the editor keeps working");
    }

    #[test]
    fn lua_hooks_replace_the_modeline_servers_and_theme() {
        let (mut lisp, _editor) = lisp_with("");
        lisp.eval_lua(r#"
            vmac.hooks.modeline = function() return { {vmac.buffer_name(), "accent"}, {"lua!", "dim"} } end
            vmac.hooks.lsp_servers = function() return { {"python", "pylsp"}, {"rust", "ra", "--flag"} } end
            vmac.hooks.syntax_theme = function() return { {"keyword", "red"} } end
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
        lisp.eval_lua("vmac.hooks.modeline = function() error('bad') end").unwrap();
        assert!(lisp.modeline()[0].text.contains("bad"), "a broken hook shows its error");
    }

    #[test]
    fn init_lua_loads_after_scheme_and_reloads() {
        let dir = std::env::temp_dir().join(format!("vmac-init-lua-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let init = dir.join("init.lua");
        std::fs::write(&init, r#"vmac.map("normal", "Q", function() vmac.insert("one") end)"#).unwrap();
        let editor = Editor::new(Buffer::new("t"));
        let core = Source { label: "core".into(), path: None };
        let mut lisp = Lisp::with_lua(editor.clone(), vec![core], vec![init.clone()]);
        assert_eq!(editor.lock().message, "loaded core, init.lua");
        typed(&mut lisp, "Q");
        assert_eq!(text(&editor), "one");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&init, r#"vmac.map("normal", "Q", function() vmac.insert("two") end)"#).unwrap();
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

    /// A plugins folder with: `zen` (Scheme), `wc` (Lua, requiring a module
    /// of its own), `broken` (bad Scheme), `off` (disabled), and `stamp.lua`.
    fn plugin_setup(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("vmac-plugin-load-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("plugins");
        let write = |rel: &str, text: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("zen/init.scm", r#"(defcommand (zen-insert) (insert "zen")) (define-key normal-map "Z" 'zen-insert)"#);
        write("wc/init.lua", r#"local util = require("wc_util")
vmac.command("word-count", function() vmac.message(util.count(vmac.buffer_string()) .. " words") end)"#);
        write("wc/lua/wc_util.lua", "return { count = function(s) local n = 0 for _ in s:gmatch('%S+') do n = n + 1 end return n end }");
        write("broken/init.scm", "(define (oops) (no-such-function))");
        write("off/init.scm", r#"(defcommand (off-insert) (insert "off"))"#);
        write("off/disabled", "");
        write("stamp.lua", r#"vmac.command("stamp", function() vmac.insert("stamped") end)"#);
        (base, dir)
    }

    fn with_plugins(dir: &Path, init_scm: Option<&Path>) -> (Lisp, Editor) {
        let editor = Editor::new(Buffer::new("t"));
        let mut sources = vec![Source { label: "core".into(), path: None }];
        if let Some(init) = init_scm {
            sources.push(Source { label: "init.scm".into(), path: Some(init.to_path_buf()) });
        }
        let lisp = Lisp::with_config(editor.clone(), sources, vec![], Some(dir.to_path_buf()));
        (lisp, editor)
    }

    #[test]
    fn plugins_load_and_a_broken_one_is_skipped() {
        let (base, dir) = plugin_setup("load");
        let (mut lisp, editor) = with_plugins(&dir, None);
        {
            let m = editor.lock().message.clone();
            assert!(m.starts_with("broken: "), "the broken plugin's error is shown: {m}");
        }
        typed(&mut lisp, "Z");
        assert_eq!(text(&editor), "zen", "a Scheme plugin's key");
        typed(&mut lisp, " :stamp\n");
        assert!(text(&editor).contains("stamped"), "a single-file Lua plugin");
        typed(&mut lisp, " :word-count\n");
        assert_eq!(editor.lock().message, "1 words", "a Lua plugin requiring its own module");
        typed(&mut lisp, " :off-insert\n");
        assert_eq!(editor.lock().message, "off-insert is not a command", "disabled plugins don't load");
        typed(&mut lisp, "0x");
        assert_eq!(text(&editor).chars().count(), "zenstamped".len() - 1, "the core still works");

        let list = lisp.eval("(plugin-list)").unwrap();
        assert!(list.contains("broken  (scheme)  failed:"), "{list}");
        assert!(list.contains("off  (scheme)  off") && list.contains("wc  (lua)  loaded"), "{list}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_users_init_loads_after_plugins() {
        let (base, dir) = plugin_setup("order");
        std::fs::remove_dir_all(dir.join("broken")).unwrap();
        let init = base.join("init.scm");
        std::fs::write(&init, r#"(define-key normal-map "Z" (lambda () (insert "mine")))"#).unwrap();
        let (mut lisp, editor) = with_plugins(&dir, Some(&init));
        assert_eq!(editor.lock().message, "loaded core, 3 plugins, init.scm");
        typed(&mut lisp, "Z");
        assert_eq!(text(&editor), "mine", "init.scm overrides the plugin's key");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn spc_h_p_toggles_plugins_and_edits_reload() {
        let (base, dir) = plugin_setup("toggle");
        std::fs::remove_dir_all(dir.join("broken")).unwrap();
        let (mut lisp, editor) = with_plugins(&dir, None);
        typed(&mut lisp, " hp");
        assert!(editor.lock().picker.is_some());
        typed(&mut lisp, "zen\n");
        assert!(editor.lock().reload_requested);
        assert!(lisp.reload_if_changed());
        typed(&mut lisp, " :zen-insert\n");
        assert_eq!(editor.lock().message, "zen-insert is not a command", "turned off");
        assert!(dir.join("zen/disabled").exists());

        lisp.eval(r#"(plugin-toggle "zen")"#).unwrap();
        assert!(lisp.reload_if_changed());
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("zen/init.scm"), r#"(defcommand (zen-insert) (insert "zen 2"))"#).unwrap();
        assert!(lisp.reload_if_changed(), "editing a plugin reloads it");
        typed(&mut lisp, " :zen-insert\n");
        assert_eq!(text(&editor), "zen 2");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_plugin_fixed_on_disk_comes_back() {
        let (base, dir) = plugin_setup("fix");
        let (mut lisp, editor) = with_plugins(&dir, None);
        std::fs::write(dir.join("broken/init.scm"), r#"(defcommand (fixed) (insert "fixed"))"#).unwrap();
        lisp.reload();
        typed(&mut lisp, " :fixed\n");
        assert_eq!(text(&editor), "fixed");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_example_plugins_load() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins");
        let (mut lisp, editor) = with_plugins(&dir, None);
        assert_eq!(editor.lock().message, "loaded core, 3 plugins");
        typed(&mut lisp, "ione two\nthree");
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, " tw");
        assert_eq!(editor.lock().message, "3 words, 2 lines");
        typed(&mut lisp, " wv tz");
        assert_eq!(editor.lock().window_count(), 1);
        assert_eq!(editor.lock().message, "zen");
    }

    #[test]
    fn spc_c_f_formats_the_file_keeping_the_cursor() {
        let (mut lisp, editor, dir) = with_fake_server("format", "fn main() {}\nlet  x=1\nx\n");
        assert!(pump_until(&mut lisp, &editor, |st| !st.buffer_diagnostics().is_empty()), "server up");
        typed(&mut lisp, "G");
        let line = editor.lock().buffer().line();
        typed(&mut lisp, " cf");
        assert!(pump_until(&mut lisp, &editor, |st| st.message == "formatted"), "{}", editor.lock().message.clone());
        let st = editor.lock();
        assert_eq!(st.buffer().text().to_string(), "fn main() {}\nlet x = 1;\nx\n");
        assert_eq!(st.buffer().line(), line, "the cursor stayed on its line");
        drop(st);
        typed(&mut lisp, "u");
        assert_eq!(editor.lock().buffer().text().to_string(), "fn main() {}\nlet  x=1\nx\n", "one undo step");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spc_c_f_in_visual_formats_the_selection() {
        let (mut lisp, editor, dir) = with_fake_server("format-range", "fn main() {}\nlet  x=1\nlet  x=1\n");
        assert!(pump_until(&mut lisp, &editor, |st| !st.buffer_diagnostics().is_empty()), "server up");
        typed(&mut lisp, "jjV cf");
        assert!(pump_until(&mut lisp, &editor, |st| st.message == "formatted"));
        assert_eq!(
            editor.lock().buffer().text().to_string(),
            "fn main() {}\nlet  x=1\nlet x = 1;\n",
            "only the selected line"
        );
        assert_eq!(lisp.cursor_shape(), "box", "out of visual state");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Pumps terminals until `done`, for up to four seconds.
    fn pump_terminals_until(lisp: &mut Lisp, editor: &Editor, done: impl Fn(&EditorState) -> bool) -> bool {
        for _ in 0..200 {
            lisp.pump_terminals();
            if done(&editor.lock()) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_local_terminal_runs_a_shell_you_type_into() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, ":term sh\n");
        assert!(editor.lock().terminal_here().is_some(), "{}", editor.lock().message.clone());
        assert_eq!(lisp.cursor_shape(), "bar", "typing into it");
        typed(&mut lisp, "echo vmac-$((40 + 2))\n");
        assert!(
            pump_terminals_until(&mut lisp, &editor, |st| st.buffer().text().to_string().contains("vmac-42")),
            "{}",
            editor.lock().buffer().text()
        );
        lisp.dispatch_key("ESC", "");
        assert_eq!(lisp.cursor_shape(), "box", "ESC: normal state, over the screen's text");
        typed(&mut lisp, "gg");
        assert_eq!(editor.lock().buffer().point(), 0, "motions work on the screen");
        assert!(lisp.modeline().iter().any(|s| s.text == "terminal (local)"));
        typed(&mut lisp, "x");
        assert!(editor.lock().buffer().text().to_string().contains("vmac-42"), "read-only: x does nothing");
        typed(&mut lisp, "i");
        typed(&mut lisp, "exit\n");
        assert!(
            pump_terminals_until(&mut lisp, &editor, |st| st.terminals.first().is_some_and(|t| t.exited)),
            "exit ends it"
        );
    }

    #[test]
    fn spc_o_t_opens_a_terminal_below_and_closes_it() {
        let (mut lisp, editor) = lisp_with("text");
        typed(&mut lisp, " ot");
        assert_eq!(editor.lock().window_count(), 2);
        assert!(editor.lock().terminal_here().is_some());
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, " ot");
        assert_eq!(editor.lock().window_count(), 1, "from the terminal, SPC o t closes its window");
        assert_eq!(editor.lock().buffer().name(), "test");
        typed(&mut lisp, " ot");
        assert_eq!(editor.lock().terminals.len(), 1, "the running terminal is reused");
    }

    #[test]
    fn plugins_add_terminal_backends() {
        let (mut lisp, editor) = lisp_with("");
        lisp.eval_lua(r#"
            local screens = {}
            killed = {}
            vmac.terminal_backend("fake", {
              start = function(command, cwd, rows, cols)
                screens.h1 = "fake$ " .. command
                return "h1"
              end,
              send = function(h, text, key)
                screens[h] = screens[h] .. (text or ("<" .. key .. ">"))
              end,
              read = function(h) return screens[h] end,
              kill = function(h) killed[#killed + 1] = h end,
            })
            vmac.set_terminal_backend("fake")
        "#)
        .unwrap();
        typed(&mut lisp, ":term top\n");
        typed(&mut lisp, "ab");
        lisp.dispatch_key("RET", "");
        lisp.dispatch_key("C-c", "");
        lisp.last_terminal_poll -= std::time::Duration::from_secs(1);
        assert!(pump_terminals_until(&mut lisp, &editor, |st| st.buffer().text() == "fake$ topab<enter><ctrl+c>"));
        assert!(lisp.modeline().iter().any(|s| s.text == "terminal (fake)"));
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, ":bd\n");
        lisp.last_terminal_poll -= std::time::Duration::from_secs(1);
        lisp.pump_terminals();
        assert_eq!(lisp.eval_lua("killed[1]").unwrap(), "h1", "closing its buffer ends it");
    }

    #[test]
    fn a_backend_can_attach_through_a_local_pty() {
        let (mut lisp, editor) = lisp_with("");
        lisp.eval_lua(r#"
            killed = {}
            vmac.terminal_backend("attaching", {
              -- A stand-in for `hc attach t7`: the "attach" command is a shell.
              start = function(command, cwd, rows, cols) return { handle = "t7", command = "sh" } end,
              kill = function(h) killed[#killed + 1] = h end,
            })
            vmac.set_terminal_backend("attaching")
        "#)
        .unwrap();
        typed(&mut lisp, ":term\n");
        assert!(lisp.modeline().iter().any(|s| s.text == "terminal (attaching)"), "{}", editor.lock().message.clone());
        typed(&mut lisp, "echo attached-$((40 + 2))\n");
        assert!(
            pump_terminals_until(&mut lisp, &editor, |st| st.buffer().text().to_string().contains("attached-42")),
            "keys and screen go through the PTY: {}",
            editor.lock().buffer().text()
        );
        {
            // The PTY takes the window's size.
            let mut st = editor.lock();
            let id = st.selected_window();
            st.window_by_id_mut(id).unwrap().cols = 50;
        }
        // Apply the size before typing (resizing mid-output re-wraps lines,
        // as in any terminal).
        lisp.pump_terminals();
        std::thread::sleep(std::time::Duration::from_millis(100));
        typed(&mut lisp, "tput cols\n");
        assert!(
            pump_terminals_until(&mut lisp, &editor, |st| st.buffer().text().to_string().lines().any(|l| l.trim() == "50")),
            "{:?}",
            editor.lock().buffer().text().to_string()
        );
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, ":bd\n");
        lisp.last_terminal_poll -= std::time::Duration::from_secs(1);
        lisp.pump_terminals();
        assert_eq!(lisp.eval_lua("killed[1]").unwrap(), "t7", "closing it asks the backend to end it");
    }

    #[test]
    fn fish_is_the_default_shell_when_installed() {
        let shell = crate::editor::default_shell();
        if std::path::Path::new("/opt/homebrew/bin/fish").exists() || shell.ends_with("/fish") {
            assert!(shell.ends_with("/fish"), "{shell}");
        }
        let (mut lisp, editor) = lisp_with("");
        assert_eq!(lisp.eval_lua("vmac.terminal_shell()").unwrap(), shell);
        // A shell with no config to load, so the test doesn't depend on yours.
        lisp.eval_lua(r#"vmac.set_terminal_shell("/bin/sh")"#).unwrap();
        typed(&mut lisp, ":term\n");
        typed(&mut lisp, "echo $0\n");
        assert!(
            pump_terminals_until(&mut lisp, &editor, |st| st.buffer().text().to_string().lines().any(|l| l.contains("sh") && !l.contains("echo"))),
            "{}",
            editor.lock().buffer().text()
        );
    }

    #[test]
    fn the_font_can_be_set_and_unknown_fonts_are_refused() {
        let (mut lisp, editor) = lisp_with("");
        editor.lock().fonts = vec!["Menlo".into(), "Hack Nerd Font Mono".into()];
        typed(&mut lisp, ":font Hack Nerd Font Mono\n");
        assert_eq!(editor.lock().font.as_deref(), Some("Hack Nerd Font Mono"), "a name with spaces");
        typed(&mut lisp, ":font Nope\n");
        assert_eq!(editor.lock().font.as_deref(), Some("Hack Nerd Font Mono"));
        assert_eq!(editor.lock().message, "no font named Nope (:fonts lists them)");
        lisp.eval_lua(r#"vmac.set_font("Menlo")"#).unwrap();
        assert_eq!(lisp.eval("(font)").unwrap(), "\"Menlo\"");
        typed(&mut lisp, ":fonts\n");
        assert!(editor.lock().picker.is_some());
        typed(&mut lisp, "hack\n");
        assert_eq!(editor.lock().font.as_deref(), Some("Hack Nerd Font Mono"), "picked from the list");
    }

    #[test]
    fn the_theme_follows_the_system_unless_set() {
        let (mut lisp, editor) = lisp_with("");
        assert_eq!(editor.lock().theme_mode, "system");
        typed(&mut lisp, ":theme dark\n");
        assert_eq!(editor.lock().theme_mode, "dark");
        typed(&mut lisp, ":theme purple\n");
        assert_eq!(editor.lock().theme_mode, "dark", "only system, light or dark");
        lisp.eval_lua(r#"vmac.set_theme("system")"#).unwrap();
        assert_eq!(lisp.eval("(theme)").unwrap(), "\"system\"");
    }

    #[test]
    fn an_unknown_backend_says_so() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, ":terminal-backend nope\n");
        typed(&mut lisp, ":term\n");
        assert_eq!(editor.lock().message, "no terminal backend named nope");
        assert!(editor.lock().terminals.is_empty());
    }

    /// Against the real herdr-core: `cargo test -- --ignored herdr`. It makes
    /// a terminal there and ends it.
    #[test]
    #[ignore]
    fn the_herdr_backend_runs_terminals_in_herdr_core() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins");
        let (mut lisp, editor) = with_plugins(&dir, None);
        lisp.eval_lua(r#"vmac.set_terminal_backend("herdr")"#).unwrap();
        typed(&mut lisp, ":term sh\n");
        assert!(editor.lock().terminal_here().is_some(), "{}", editor.lock().message.clone());
        typed(&mut lisp, "echo herdr-$((40 + 2))\n");
        let found = (0..100).any(|_| {
            lisp.last_terminal_poll -= std::time::Duration::from_secs(1);
            lisp.pump_terminals();
            std::thread::sleep(std::time::Duration::from_millis(50));
            editor.lock().buffer().text().to_string().contains("herdr-42")
        });
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, ":bd\n");
        lisp.last_terminal_poll -= std::time::Duration::from_secs(1);
        lisp.pump_terminals();
        assert!(found, "{}", editor.lock().buffer().text());
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
