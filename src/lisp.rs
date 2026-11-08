//! The Lua interpreter and the primitives it gets from the Rust core.
//!
//! Rust exposes small, fast primitives in the `vmac` table (`vmac.insert`,
//! `vmac.forward_char`, ...). Every command, keymap and the modeline are
//! defined in Lua (`lua/core/*.lua`, then plugins, then the user's
//! `~/.config/vmac/init.lua`) and can be redefined while the editor runs.

use crate::buffer::Buffer;
use crate::editor::{Editor, SessionRequest};
use crate::picker::Picker;
use mlua::Lua;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The core (lua/core/), one file per area, built into the binary for
/// running outside a checkout. They load in name order; a checkout's
/// lua/core/ is read instead, so edits there load on save.
const LUA_CORE_FILES: &[(&str, &str)] = &[
    ("00-core.lua", include_str!("../lua/core/00-core.lua")),
    ("10-motions.lua", include_str!("../lua/core/10-motions.lua")),
    ("20-editing.lua", include_str!("../lua/core/20-editing.lua")),
    ("30-commands.lua", include_str!("../lua/core/30-commands.lua")),
    ("40-tree.lua", include_str!("../lua/core/40-tree.lua")),
    ("50-dashboard.lua", include_str!("../lua/core/50-dashboard.lua")),
    ("60-lsp.lua", include_str!("../lua/core/60-lsp.lua")),
    ("70-agent.lua", include_str!("../lua/core/70-agent.lua")),
    ("80-looks.lua", include_str!("../lua/core/80-looks.lua")),
    ("90-bindings.lua", include_str!("../lua/core/90-bindings.lua")),
    ("agents.lua", include_str!("../lua/core/agents.lua")),
    ("options.lua", include_str!("../lua/core/options.lua")),
    ("review.lua", include_str!("../lua/core/review.lua")),
    ("sessions.lua", include_str!("../lua/core/sessions.lua")),
];

/// The checkout's lua/core/, when running from one.
fn core_dir() -> Option<PathBuf> {
    Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/core")).filter(|d| d.is_dir())
}

/// The built-in core's files, in name order: (name, code).
fn built_in_core() -> Vec<(String, String)> {
    LUA_CORE_FILES.iter().map(|(n, c)| (n.to_string(), c.to_string())).collect()
}

/// The core's files in `dir` (a checkout's lua/core/, so edits there
/// load), in name order: (name, code); else the ones built in.
fn lua_core_files(dir: Option<&Path>) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = dir
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lua"))
        .filter_map(|p| Some((p.file_name()?.to_string_lossy().into_owned(), std::fs::read_to_string(&p).ok()?)))
        .collect();
    if files.is_empty() {
        return built_in_core();
    }
    files.sort();
    files
}

/// When a folder's newest file changed.
fn newest_in(dir: &Path) -> Option<SystemTime> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()
}

/// How a face looks, from Lua's `core.face_specs()`. Colors are face/theme
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

/// `{ "name", color = ..., background = ..., scale = 1.2, bold = true,
/// italic = true, underline = true, sans = true }`.
fn parse_face_spec(t: &mlua::Table) -> Option<FaceSpec> {
    Some(FaceSpec {
        name: t.get::<String>(1).ok()?,
        color: t.get("color").ok().flatten(),
        background: t.get("background").ok().flatten(),
        scale: t.get::<Option<f32>>("scale").ok().flatten(),
        bold: t.get::<Option<bool>>("bold").ok().flatten().unwrap_or(false),
        italic: t.get::<Option<bool>>("italic").ok().flatten().unwrap_or(false),
        underline: t.get::<Option<bool>>("underline").ok().flatten().unwrap_or(false),
        sans: t.get::<Option<bool>>("sans").ok().flatten().unwrap_or(false),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub text: String,
    pub face: String,
}

pub struct Lisp {
    lua: Lua,
    editor: Editor,
    /// Where the core is read from (a checkout's lua/core/; None: the one
    /// built in), and when it last changed.
    core_dir: Option<PathBuf>,
    core_stamp: Option<SystemTime>,
    /// Each plugin's and init file's last text that loaded, to fall back on.
    last_good: std::collections::HashMap<String, String>,
    /// Lua files loaded after the core and plugins (init.lua), and when
    /// they last changed.
    lua_sources: Vec<PathBuf>,
    lua_stamps: Vec<Option<SystemTime>>,
    /// Where plugins live (~/.config/vmac/plugins), and their files' state.
    plugin_dir: Option<PathBuf>,
    plugin_signature: Vec<(PathBuf, Option<SystemTime>)>,
    /// When plugin-backed terminals were last read.
    last_terminal_poll: std::time::Instant,
}

impl Lisp {
    /// The built-in core alone.
    #[cfg(test)]
    pub fn new(editor: Editor) -> Self {
        Self::with_lua(editor, Vec::new())
    }

    #[cfg(test)]
    pub fn with_lua(editor: Editor, lua_sources: Vec<PathBuf>) -> Self {
        Self::load(editor, None, lua_sources, None)
    }

    /// The core in `dir` (as a checkout's), alone.
    #[cfg(test)]
    pub fn with_core_dir(editor: Editor, dir: PathBuf) -> Self {
        Self::load(editor, Some(dir), Vec::new(), None)
    }

    /// The core (the checkout's, when running from one), plugins from
    /// `plugin_dir`, then `lua_sources` (init.lua).
    pub fn with_config(editor: Editor, lua_sources: Vec<PathBuf>, plugin_dir: Option<PathBuf>) -> Self {
        Self::load(editor, core_dir(), lua_sources, plugin_dir)
    }

    fn load(editor: Editor, core_dir: Option<PathBuf>, lua_sources: Vec<PathBuf>, plugin_dir: Option<PathBuf>) -> Self {
        editor.lock().plugin_dir = plugin_dir.clone();
        let mut lisp = Self {
            lua: fresh_lua(&editor),
            editor,
            core_dir,
            core_stamp: None,
            last_good: Default::default(),
            lua_stamps: vec![None; lua_sources.len()],
            lua_sources,
            plugin_dir,
            plugin_signature: Vec::new(),
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

    /// Loads everything again into a fresh Lua (buffers are untouched): the
    /// core, then plugins, each able to `require` from its own folder, then
    /// `lua_sources`. A checkout's core that fails to load gives way to the
    /// one built in (it may call primitives added since this binary was
    /// built). A plugin or init file that fails leaves nothing half-defined:
    /// Lua is rebuilt from what loaded before it, and its last version that
    /// loaded is used, if there is one; a failing plugin is marked failed.
    /// The rest still load, and the first error is shown.
    pub fn reload(&mut self) {
        use crate::plugins::Status;
        let plugins = self.plugin_dir.as_deref().map(crate::plugins::discover).unwrap_or_default();
        self.plugin_signature = self.plugin_dir.as_deref().map(crate::plugins::signature).unwrap_or_default();
        self.core_stamp = self.core_dir.as_deref().and_then(newest_in);
        for (i, path) in self.lua_sources.clone().iter().enumerate() {
            self.lua_stamps[i] = path.metadata().ok().and_then(|m| m.modified().ok());
        }
        self.lua = fresh_lua(&self.editor);
        let mut errors: Vec<String> = Vec::new();
        let mut loaded = vec!["core".to_string()];
        // What loaded, in order (name, code): run again to rebuild after a failure.
        let mut ran: Vec<(String, String)> = Vec::new();
        for (name, code) in lua_core_files(self.core_dir.as_deref()) {
            match self.lua.load(code.as_str()).set_name(&name).exec() {
                Ok(()) => ran.push((name, code)),
                Err(e) => {
                    let note = if self.core_dir.is_some() {
                        " (using the core built into vmac; rebuild it to use the checkout's)"
                    } else {
                        ""
                    };
                    errors.push(format!("{name}: {}{note}", first_line(&e)));
                    self.lua = fresh_lua(&self.editor);
                    ran.clear();
                    for (name, code) in built_in_core() {
                        if self.lua.load(code.as_str()).set_name(&name).exec().is_ok() {
                            ran.push((name, code));
                        }
                    }
                    break;
                }
            }
        }
        let mut statuses: std::collections::HashMap<String, Status> = plugins
            .iter()
            .map(|p| (p.name.clone(), if p.disabled { Status::Disabled } else { Status::Loaded }))
            .collect();
        let plugin_files = plugins
            .iter()
            .filter(|p| !p.disabled)
            .filter_map(|p| Some((Some(p.name.clone()), p.lua.clone()?, Some(p.lua_root()))));
        let user_files = self.lua_sources.clone().into_iter().map(|p| (None, p, None));
        for (plugin, path, root) in plugin_files.chain(user_files).collect::<Vec<_>>() {
            let Ok(code) = std::fs::read_to_string(&path) else { continue };
            let label = plugin.clone().unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            if let Some(root) = &root {
                // require("x") finds x.lua in the plugin's folder or its lua/.
                let root = root.display().to_string().replace('\\', "/");
                let paths = format!("package.path = {root:?} .. '/?.lua;' .. {root:?} .. '/lua/?.lua;' .. package.path");
                if self.lua.load(paths.as_str()).exec().is_ok() {
                    ran.push(("package.path".into(), paths));
                }
            }
            match self.lua.load(code.as_str()).set_name(&label).exec() {
                Ok(()) => {
                    if plugin.is_none() {
                        loaded.push(label.clone());
                    }
                    self.last_good.insert(label.clone(), code.clone());
                    ran.push((label, code));
                }
                Err(e) => {
                    let mut error = format!("{label}: {}", first_line(&e));
                    self.lua = fresh_lua(&self.editor);
                    for (name, code) in &ran {
                        let _ = self.lua.load(code.as_str()).set_name(name).exec();
                    }
                    let kept = self.last_good.get(&label).filter(|old| **old != code).cloned();
                    if let Some(old) = kept.filter(|old| self.lua.load(old.as_str()).set_name(&label).exec().is_ok()) {
                        error.push_str(" (kept the last version that loaded)");
                        ran.push((label, old));
                    }
                    if let Some(name) = plugin {
                        statuses.insert(name, Status::Failed(error.clone()));
                    }
                    errors.push(error);
                }
            }
        }
        let enabled = plugins.iter().filter(|p| matches!(statuses.get(&p.name), Some(Status::Loaded))).count();
        let mut st = self.editor.lock();
        st.plugins = plugins
            .into_iter()
            .map(|p| {
                let status = statuses.remove(&p.name).unwrap_or(Status::Loaded);
                (p, status)
            })
            .collect();
        st.message = match errors.first() {
            Some(first) if errors.len() > 1 => format!("{first} (and {} more)", errors.len() - 1),
            Some(first) => first.clone(),
            None => {
                if enabled > 0 {
                    loaded.insert(1, format!("{enabled} plugin{}", if enabled == 1 { "" } else { "s" }));
                }
                format!("loaded {}", loaded.join(", "))
            }
        };
    }

    /// Reloads when the core, a plugin or the user's Lua changed since the
    /// last load (or a reload was asked for).
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
        let core_changed = self.core_dir.is_some() && self.core_dir.as_deref().and_then(newest_in) != self.core_stamp;
        let requested = std::mem::take(&mut self.editor.lock().reload_requested);
        let changed = requested || lua_changed || plugins_changed || core_changed;
        if changed {
            self.reload();
        }
        changed
    }

    /// Calls `core[name](args)`; an error as its first line.
    pub fn call<R: mlua::FromLuaMulti>(&self, name: &str, args: impl mlua::IntoLuaMulti) -> Result<R, String> {
        let core: mlua::Table = self.lua.globals().get("core").map_err(|e| first_line(&e))?;
        let f: mlua::Function = core.get(name).map_err(|_| format!("core.{name} is missing"))?;
        f.call::<R>(args).map_err(|e| first_line(&e))
    }

    /// `vmac.hooks[name]()` when there's such a hook, else `core[name]()`.
    fn hooked<R: mlua::FromLuaMulti>(&self, name: &str) -> Result<R, String> {
        let hook = self
            .lua
            .globals()
            .get::<mlua::Table>("vmac")
            .and_then(|g| g.get::<mlua::Table>("hooks"))
            .and_then(|h| h.get::<mlua::Function>(name));
        match hook {
            Ok(f) => f.call::<R>(()).map_err(|e| first_line(&e)),
            Err(_) => self.call(name, ()),
        }
    }

    /// Runs a command by name; an error goes to the echo area.
    pub fn run_command(&mut self, name: &str) {
        if let Err(e) = self.call::<()>("call_binding", name) {
            self.editor.lock().message = format!("{name}: {e}");
        }
    }

    /// Reads terminals into their buffers: local ones every call, plugin
    /// ones (which run Lua, often a process) every 150 ms. Returns whether
    /// a screen changed.
    pub fn pump_terminals(&mut self) -> bool {
        let follow = match self.call::<bool>("terminal_insert", ()) {
            Ok(true) => Some(self.editor.lock().window().buffer),
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
            let text = call_backend::<String>(&self.lua, &backend, "read", handle.clone());
            let alive = call_backend::<bool>(&self.lua, &backend, "alive", handle.clone()).unwrap_or(Ok(true));
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
            let _ = call_backend::<()>(&self.lua, &backend, "kill", handle);
        }
        changed
    }

    /// Hands a keystroke to Lua's `core.dispatch_key`. `key` is Emacs
    /// notation (`"C-x"`, `"M-:"`, `"RET"`); `text` is what it would type, or "".
    pub fn dispatch_key(&mut self, key: &str, text: &str) {
        // Any key closes the documentation K showed and the which-key popup.
        self.editor.lock().hover = None;
        self.editor.lock().whichkey = None;
        if let Err(e) = self.call::<()>("dispatch_key", (key, text)) {
            self.editor.lock().message = format!("{key}: {e}");
        }
    }

    /// Runs what Rust raised for Lua (server replies, the agent asking):
    /// each calls `core.events[name]` with a list of strings.
    pub fn run_events(&mut self) -> bool {
        let events = self.editor.lock().take_events();
        let any = !events.is_empty();
        for (name, args) in events {
            let handler = self
                .lua
                .globals()
                .get::<mlua::Table>("core")
                .and_then(|c| c.get::<mlua::Table>("events"))
                .and_then(|e| e.get::<mlua::Function>(name.as_str()));
            let result = match handler {
                Ok(f) => f.call::<()>(args).map_err(|e| first_line(&e)),
                Err(_) => Err("nothing handles it".into()),
            };
            if let Err(e) = result {
                self.editor.lock().message = format!("{name}: {e}");
            }
        }
        any
    }

    /// Evaluates Lua and returns what it shows: an expression's value, else "".
    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        eval_lua_in(&self.lua, code).map_err(|e| first_line(&e))
    }

    /// Evaluates Lua (as eval does).
    #[cfg(test)]
    pub fn eval_lua(&mut self, code: &str) -> Result<String, String> {
        self.eval(code)
    }

    /// Whether the selected window shows Markdown as typed (insert state)
    /// rather than rendered or with its markup hidden.
    pub fn markdown_raw(&mut self) -> bool {
        self.call::<bool>("markdown_raw", ()).unwrap_or(false)
    }

    /// "box" or "bar".
    pub fn cursor_shape(&mut self) -> String {
        self.call::<String>("cursor_shape", ()).unwrap_or_else(|_| "box".into())
    }

    /// The selected ranges `[start, end)` in characters (a visual block:
    /// one per line).
    pub fn selection(&mut self) -> Vec<(usize, usize)> {
        let ranges = self.call::<Vec<Vec<isize>>>("selection", ()).unwrap_or_default();
        ranges
            .into_iter()
            .filter_map(|r| Some((*r.first()?, *r.get(1)?)))
            .filter(|(a, b)| a < b && *a >= 0)
            .map(|(a, b)| (a as usize, b as usize))
            .collect()
    }

    /// The pattern whose matches the window highlights, or "" for none.
    pub fn search_highlight(&mut self) -> String {
        self.call::<String>("search_highlight", ()).unwrap_or_default()
    }

    /// (capture face) pairs saying how tree-sitter captures look.
    pub fn syntax_theme(&mut self) -> Vec<(String, String)> {
        let rows = self.hooked::<Vec<Vec<String>>>("syntax_theme").unwrap_or_default();
        rows.into_iter().filter_map(|r| Some((r.first()?.clone(), r.get(1)?.clone()))).collect()
    }

    /// `{language, command, arg...}` lists.
    pub fn lsp_servers(&mut self) -> Vec<crate::lsp::ServerConfig> {
        let rows = self.hooked::<Vec<Vec<String>>>("lsp_servers").unwrap_or_default();
        rows.into_iter()
            .filter_map(|r| {
                let mut words = r.into_iter();
                Some(crate::lsp::ServerConfig { language: words.next()?, command: words.next()?, args: words.collect() })
            })
            .collect()
    }

    /// How named faces look.
    pub fn face_specs(&mut self) -> Vec<FaceSpec> {
        let specs = self.call::<Vec<mlua::Table>>("face_specs", ()).unwrap_or_default();
        specs.iter().filter_map(parse_face_spec).collect()
    }

    /// `{{text, face}, ...}` segments.
    pub fn modeline(&mut self) -> Vec<Segment> {
        match self.hooked::<Vec<Vec<String>>>("modeline") {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|r| Some(Segment { text: r.first()?.clone(), face: r.get(1).cloned().unwrap_or("default".into()) }))
                .collect(),
            Err(e) => vec![Segment { text: format!("modeline: {e}"), face: "error".into() }],
        }
    }
}

pub fn config_dir() -> Option<PathBuf> {
    let config = PathBuf::from(std::env::var_os("HOME")?).join(".config");
    let (new, old) = (config.join("vmac"), config.join("gmacs"));
    Some(if !new.exists() && old.exists() { old } else { new })
}

/// Where primitives get registered: Lua's `vmac` table.
struct Registry<'a> {
    lua: &'a Lua,
    vmac: mlua::Table,
}

/// A primitive's Lua name: `buffer-name` -> `buffer_name`, `pos->line` ->
/// `pos_to_line`, `minibuffer-active?` -> `minibuffer_active`.
pub fn lua_name(name: &str) -> String {
    name.replace("->", "_to_").trim_end_matches(['?', '!']).replace('-', "_")
}

/// Registers a primitive that gets the editor. `$args` are its arguments
/// with their Rust types.
macro_rules! prim {
    ($reg:ident, $editor:ident, $name:literal, |$st:ident $(, $a:ident : $t:ty)*| $body:expr) => {{
        let ed = $editor.clone();
        #[allow(unused_parens)]
        let f = $reg
            .lua
            .create_function(move |_, ($($a,)*): ($($t,)*)| {
                #[allow(unused_mut)]
                let mut $st = ed.lock();
                // An inner closure, so a `return` in the body returns its value.
                #[allow(clippy::redundant_closure_call)]
                let value = (|| $body)();
                Ok(value)
            })
            .expect("a Lua function");
        $reg.vmac.set(lua_name($name), f).expect("set in vmac");
    }};
}

/// Registers a primitive that doesn't need the editor.
macro_rules! pure {
    ($reg:ident, $name:literal, |$($a:ident : $t:ty),*| $body:expr) => {{
        let f = $reg.lua.create_function(move |_, ($($a,)*): ($($t,)*)| Ok($body)).expect("a Lua function");
        $reg.vmac.set(lua_name($name), f).expect("set in vmac");
    }};
}

/// A Lua state with the primitives in `vmac`.
fn fresh_lua(editor: &Editor) -> Lua {
    let lua = Lua::new();
    let vmac = lua.create_table().expect("a table");
    {
        let mut reg = Registry { lua: &lua, vmac: vmac.clone() };
        register_primitives(&mut reg, editor);
        register_terminals(&mut reg, editor);
    }
    lua.globals().set("vmac", vmac.clone()).expect("set vmac");
    // The editor's old name, so plugins written for it still load.
    lua.globals().set("gmacs", vmac).expect("set gmacs");
    lua
}

/// Terminals: start one with a backend ("" for the default), and send a key
/// to the selected one. Plugin backends run Lua, so no editor lock is held
/// while they do.
fn register_terminals(reg: &mut Registry, editor: &Editor) {
    let ed = editor.clone();
    let open = reg
        .lua
        .create_function(move |lua, (backend, command): (String, String)| {
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
                return Ok(match crate::terminal::LocalPty::start(&command, &cwd, rows, cols) {
                    Ok(pty) => {
                        ed.lock().add_terminal(crate::editor::TermBackend::Local(pty));
                        true
                    }
                    Err(e) => {
                        ed.lock().message = format!("terminal: {e}");
                        false
                    }
                });
            }
            // start returns a handle (then send/read are polled), or
            // { handle = ..., command = ... }: a command that attaches to the
            // terminal, run here on a PTY (keys, screen and size go through it).
            let started = call_backend::<mlua::Value>(lua, &backend, "start", (command, cwd.display().to_string(), rows, cols))
                .map(|r| {
                    r.and_then(|v| match v {
                        mlua::Value::Table(t) => Ok((t.get::<String>("handle")?, t.get::<Option<String>>("command")?)),
                        mlua::Value::String(s) => Ok((s.to_string_lossy().to_string(), None)),
                        mlua::Value::Nil => Ok((String::new(), None)),
                        other => Err(mlua::Error::runtime(format!("start returned a {}", other.type_name()))),
                    })
                });
            let mut st = ed.lock();
            Ok(match started {
                Some(Ok((handle, Some(attach)))) if !handle.is_empty() => {
                    match crate::terminal::LocalPty::start(&attach, &cwd, rows, cols) {
                        Ok(pty) => {
                            st.add_terminal(crate::editor::TermBackend::Attached { backend, handle, pty });
                            true
                        }
                        Err(e) => {
                            st.message = format!("{backend}: {attach}: {e}");
                            false
                        }
                    }
                }
                Some(Ok((handle, None))) if !handle.is_empty() => {
                    st.add_terminal(crate::editor::TermBackend::Plugin { backend, handle });
                    true
                }
                Some(Ok(_)) => {
                    st.message = format!("{backend}: started nothing");
                    false
                }
                Some(Err(e)) => {
                    st.message = format!("{backend}: {}", first_line(&e));
                    false
                }
                None => {
                    st.message = format!("no terminal backend named {backend}");
                    false
                }
            })
        })
        .expect("a Lua function");
    reg.vmac.set("terminal_open", open).expect("set in vmac");

    let ed = editor.clone();
    let send = reg
        .lua
        .create_function(move |lua, (key, text): (String, String)| {
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
                if let Some(Err(e)) = call_backend::<()>(lua, &backend, "send", (handle, text, name)) {
                    ed.lock().message = format!("{backend}: {}", first_line(&e));
                }
            }
            Ok(())
        })
        .expect("a Lua function");
    reg.vmac.set("terminal_send", send).expect("set in vmac");
}

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
    // The fuzzy picker. Lua opens it and decides what picking does.
    prim!(reg, editor, "project-root", |s| {
        let start = s
            .buffer()
            .file()
            .cloned()
            .or_else(|| s.project.clone())
            .unwrap_or_else(|| s.cwd.clone());
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
    // Which-key: show available keys when a prefix is pending.
    prim!(reg, editor, "whichkey-show", |s, entries: Vec<String>| {
        let pairs = entries.chunks(2)
            .filter_map(|c| Some((c.first()?.clone(), c.get(1)?.clone())))
            .collect();
        s.whichkey = Some(pairs);
    });
    prim!(reg, editor, "whichkey-hide", |s| s.whichkey = None);
    prim!(reg, editor, "undo-boundary", |s| s.current().undo_boundary());
    prim!(reg, editor, "undo", |s| s.current().undo());
    prim!(reg, editor, "redo", |s| s.current().redo());
    // The register: the last deleted or yanked text.
    prim!(reg, editor, "register-set", |s, text: String| s.register_set(text));
    prim!(reg, editor, "register-get", |s| s.register_get());
    // The system clipboard itself (Cmd-V, Cmd-C), and whether yanks and p use it.
    prim!(reg, editor, "clipboard-get", |s| s.clipboard_get());
    prim!(reg, editor, "clipboard-set", |s, text: String| s.clipboard_set(text));
    prim!(reg, editor, "set-clipboard-sync", |s, on: bool| {
        s.clipboard_sync = on;
        s.message = if on { "Yanks and p use the system clipboard".into() } else { "vmac's register is its own".into() };
    });
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
    // A file in a window other than this one, selected: "" or what went wrong.
    prim!(reg, editor, "show-file-elsewhere", |s, path: String| {
        match s.show_file_elsewhere(&expand_home(&path)) {
            Ok(()) => String::new(),
            Err(e) => format!("{path}: {e}"),
        }
    });
    // A file in a window on the right, the cursor staying here, or that
    // window closed if there is one: "opened", "closed", or what went wrong.
    prim!(reg, editor, "toggle-file-beside", |s, path: String| {
        match s.toggle_file_beside(&expand_home(&path)) {
            Ok(true) => "opened".to_string(),
            Ok(false) => "closed".to_string(),
            Err(e) => format!("{path}: {e}"),
        }
    });
    prim!(reg, editor, "delete-window", |s| s.close_window());
    prim!(reg, editor, "delete-other-windows", |s| s.only_window());
    prim!(reg, editor, "window-count", |s| s.window_count() as isize);
    prim!(reg, editor, "window-height", |s| s.window().rows.max(1) as isize);
    prim!(reg, editor, "window-top-line", |s| s.window().scroll_top as isize);
    prim!(reg, editor, "scroll-window", |s, n: isize| s.scroll(n));
    prim!(reg, editor, "recenter", |s, place: String| s.recenter(&place));

    // Buffers by name, for buffers Lua draws itself (the file tree).
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
    prim!(reg, editor, "sidebar-shown?", |s| s.sidebar().is_some());
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
    prim!(reg, editor, "lsp-status", |s| {
        // What servers are doing, and what's being installed for them.
        let installs = crate::packages::status();
        match (s.lsp.status(), installs.as_str()) {
            (lsp, "") => lsp.to_string(),
            ("", installs) => installs.to_string(),
            (lsp, installs) => format!("{lsp} · {installs}"),
        }
    });
    // Language servers and grammars vmac installs (packages.rs).
    prim!(reg, editor, "package-list", |_s| crate::packages::list());
    prim!(reg, editor, "package-name", |_s, line: String| crate::packages::name_in_line(&line));
    prim!(reg, editor, "package-installed?", |_s, name: String| crate::packages::is_installed(&name));
    prim!(reg, editor, "package-install", |s, name: String| {
        if let Err(e) = crate::packages::install(&name) {
            s.message = e;
        }
    });
    prim!(reg, editor, "package-uninstall", |s, name: String| {
        s.message = match crate::packages::uninstall(&name) {
            Ok(()) => format!("Removed {name}"),
            Err(e) => e,
        };
        // A removed server stops; a grammar goes on until vmac restarts.
        s.lsp.restart();
    });
    prim!(reg, editor, "set-auto-install", |_s, on: bool| crate::packages::set_auto_install(on));
    prim!(reg, editor, "auto-install?", |_s| crate::packages::auto_install());
    prim!(reg, editor, "lsp-goto-definition", |s| s.lsp_request(crate::lsp::RequestKind::Definition));
    prim!(reg, editor, "lsp-hover", |s| s.lsp_request(crate::lsp::RequestKind::Hover));
    prim!(reg, editor, "lsp-references", |s| s.lsp_request(crate::lsp::RequestKind::References));
    prim!(reg, editor, "goto-location", |s, item: String| s.goto_location(&item));
    prim!(reg, editor, "lsp-rename", |s, name: String| s.lsp_rename(&name));
    prim!(reg, editor, "lsp-format", |s| s.lsp_format(None));
    prim!(reg, editor, "lsp-format-range", |s, start: isize, end: isize| s
        .lsp_format(Some((start.max(0) as usize, end.max(0) as usize))));
    prim!(reg, editor, "lsp-restart", |s| {
        let n = s.lsp.restart();
        s.message = match n {
            0 => "Starting the language servers again".into(),
            1 => "Restarting the language server".into(),
            n => format!("Restarting {n} language servers"),
        };
    });
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
        let f = reg.lua.create_function(move |_, url: String| {
            install(url);
            Ok(())
        });
        reg.vmac.set("plugin_install", f.expect("a Lua function")).expect("set in vmac");
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
        let f = reg.lua.create_function(move |_, ()| {
            update();
            Ok(())
        });
        reg.vmac.set("plugin_update", f.expect("a Lua function")).expect("set in vmac");
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
    prim!(reg, editor, "set-theme", |s, name: String| s.set_theme(&name));
    prim!(reg, editor, "set-themes", |s, light: String, dark: String| s.set_themes(&light, &dark));
    prim!(reg, editor, "theme-list", |s| s.theme_names.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>());
    prim!(reg, editor, "theme-name", |s| {
        let dark = match s.theme_mode.as_str() { "dark" => true, "light" => false, _ => s.system_dark };
        s.theme_name(dark)
    });
    prim!(reg, editor, "theme", |s| s.theme_mode.clone());
    // Line numbers beside files: "on", "relative" or "off".
    prim!(reg, editor, "set-line-numbers", |s, mode: String| {
        if matches!(mode.as_str(), "on" | "relative" | "off") {
            s.line_numbers = mode;
        } else {
            s.message = format!("{mode}: line numbers are on, relative or off");
        }
    });
    prim!(reg, editor, "line-numbers", |s| s.line_numbers.clone());
    // Markdown: the preview pane, and markup hidden outside insert state.
    prim!(reg, editor, "markdown-preview-toggle", |s| {
        s.markdown_preview = !s.markdown_preview;
        s.message = match (s.markdown_preview, s.preview_buffer().is_some()) {
            (false, _) => "Preview closed".into(),
            (true, true) => "Preview: the Markdown file you're in".into(),
            (true, false) => "Preview on: open a Markdown file".into(),
        };
    });
    prim!(reg, editor, "set-markdown-preview", |s, on: bool| s.markdown_preview = on);
    prim!(reg, editor, "markdown-preview?", |s| s.markdown_preview);
    // PDFs in the theme's colors (the default), or black on white.
    prim!(reg, editor, "set-pdf-themed", |s, on: bool| s.pdf_themed = on);
    prim!(reg, editor, "pdf-themed", |s| s.pdf_themed);
    // Keys in an image or PDF: zoom (times `by`, or back to fitting the
    // window with 0), whether this is one, and moving through it ("lines",
    // "screens", "sideways", "pages", "page" (from 1), or "end").
    prim!(reg, editor, "media-zoom", |s, by: f64| {
        let id = s.window().buffer;
        s.zoom_media(id, by as f32);
    });
    prim!(reg, editor, "media-zoom-level", |s| s.media_zoom.get(&s.window().buffer).copied().unwrap_or(1.) as f64);
    prim!(reg, editor, "media-here?", |s| s.buffer().file().and_then(|f| crate::frame::media_kind(f)).is_some());
    prim!(reg, editor, "media-scroll", |s, how: String, n: f64| {
        use crate::frame::MediaScroll;
        let scroll = match how.as_str() {
            "lines" => MediaScroll::Lines(n as i32),
            "screens" => MediaScroll::Screens(n as f32),
            "sideways" => MediaScroll::Sideways(n as i32),
            "pages" => MediaScroll::Pages(n as i32),
            "page" => MediaScroll::Page((n as usize).saturating_sub(1)),
            "end" => MediaScroll::End,
            _ => {
                s.message = format!("{how}: images and PDFs scroll by lines, screens, sideways, pages, to a page or the end");
                return;
            }
        };
        let id = s.window().buffer;
        let queue = s.media_scroll.entry(id).or_default();
        let n = queue.last().map_or(0, |(n, _)| *n) + 1;
        queue.push((n, scroll));
        // Plenty for the keys typed between two frames.
        if queue.len() > 64 {
            queue.remove(0);
        }
    });
    prim!(reg, editor, "set-markdown-conceal", |s, on: bool| s.markdown_conceal = on);
    // SPC t m: markup hidden (the default) -> as typed -> rendered, like the preview.
    prim!(reg, editor, "markdown-view-cycle", |s| {
        let (rendered, conceal, name) = match (s.markdown_rendered, s.markdown_conceal) {
            (false, true) => (false, false, "as typed"),
            (false, false) => (true, true, "rendered, like the preview"),
            (true, _) => (false, true, "markup hidden"),
        };
        s.markdown_rendered = rendered;
        s.markdown_conceal = conceal;
        s.message = format!("Markdown: {name} (as typed while you type)");
    });
    prim!(reg, editor, "set-markdown-view", |s, view: String| {
        match view.as_str() {
            "rendered" => (s.markdown_rendered, s.markdown_conceal) = (true, true),
            "hidden" => (s.markdown_rendered, s.markdown_conceal) = (false, true),
            "raw" => (s.markdown_rendered, s.markdown_conceal) = (false, false),
            other => s.message = format!("{other}: the Markdown view is rendered, hidden or raw"),
        }
    });
    // The coding agent (ACP): prompts, permission answers, how to start it.
    prim!(reg, editor, "agent-prompt", |s, text: String| s.agent_prompt(&text));
    prim!(reg, editor, "agent-show", |s| s.agent_show());
    prim!(reg, editor, "agent-focus-prompt", |s| s.agent_focus_prompt());
    prim!(reg, editor, "agent-here?", |s| s.agent_here());
    prim!(reg, editor, "agent-in-prompt?", |s| s.agent_in_prompt());
    prim!(reg, editor, "agent-activate", |s| s.agent_activate());
    prim!(reg, editor, "agent-toggle-at-point", |s| s.agent_toggle_at_point());
    prim!(reg, editor, "agent-new-session", |s| s.agent_new_session());
    prim!(reg, editor, "agent-sessions", |s, all: bool| s.agent_sessions(all));
    prim!(reg, editor, "agent-open-session", |s, n: isize| s.agent_open_session(n.max(0) as usize));
    prim!(reg, editor, "agent-root", |s| {
        s.agent.as_ref().map(|a| a.root.clone()).unwrap_or_else(|| s.agent_root()).display().to_string()
    });
    prim!(reg, editor, "agent-ask-about-problems", |s, question: String| s.agent_ask_about_problems(&question));
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
            Ok(p) => p.map(|p| p.display().to_string()).unwrap_or_default(),
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
    // Comments on lines, sent to the agent to revise by.
    prim!(reg, editor, "review-comment-add", |s, text: String| s.review_comment_add(&text));
    prim!(reg, editor, "review-comment-at-line", |s, rel: String, modified: bool, line: isize, text: String| {
        s.review_comment_at_line(&rel, modified, line.max(1) as usize, &text)
    });
    prim!(reg, editor, "review-comment-delete", |s| s.review_comment_delete());
    prim!(reg, editor, "review-comments-send", |s| s.review_comments_send());
    prim!(reg, editor, "review-comment-count", |s| s.review_comments.len() as isize);
    prim!(reg, editor, "review-comment-text-here", |s| {
        // The comments on the line under the cursor, joined.
        let line = s.buffer().line();
        let file = s.buffer().file().cloned();
        let row = if s.review_here() { s.review_rows.get(line).cloned().flatten() } else { None };
        s.review_comments
            .iter()
            .filter(|c| match &row {
                Some(r) => c.at.path == r.path && c.at.modified == r.modified && c.at.line == r.line,
                None => c.at.modified && Some(&c.at.path) == file.as_ref() && c.at.line == line + 1,
            })
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join(" · ")
    });
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
    prim!(reg, editor, "agent-permission-question", |s| s.agent_permission_question());
    prim!(reg, editor, "agent-permission-waiting?", |s| s.agent_permission_waiting());
    prim!(reg, editor, "agent-show-permission", |s| s.agent_show_permission());
    prim!(reg, editor, "agent-permit", |s, index: isize| s.agent_permit(index.max(0) as usize));
    prim!(reg, editor, "set-agent-command", |s, line: String| {
        if s.agent_define("custom", &line) {
            s.agent_use("custom");
            s.message = format!("agent: {line} (from the next start)");
        } else {
            s.message = "set-agent-command needs a command".into();
        }
    });
    // Agent backends by name: (agent-define "codex" "codex-acp"), (agent-use "codex").
    prim!(reg, editor, "agent-define", |s, name: String, line: String| {
        if !s.agent_define(&name, &line) {
            s.message = format!("agent-define {name}: needs a command");
        }
    });
    prim!(reg, editor, "agent-use", |s, name: String| s.agent_use(&name));
    // CLI agents: (agent-define-cli "pi" "pi" "") — run in a terminal beside your code.
    prim!(reg, editor, "agent-define-cli", |s, name: String, line: String, resume: String| {
        if !s.agent_define_cli(&name, &line, &resume) {
            s.message = format!("agent-define-cli {name}: needs a command");
        }
    });
    prim!(reg, editor, "agent-kind", |s| if s.agent_is_cli() { "cli".to_string() } else { "acp".to_string() });
    prim!(reg, editor, "agent-cli-open", |s| s.agent_cli_open(None, None));
    prim!(reg, editor, "agent-cli-here?", |s| s.agent_cli_here());
    prim!(reg, editor, "agent-cli-send", |s, text: String| s.agent_cli_send(&text));
    prim!(reg, editor, "agent-backends", |s| s.agent_backends.iter().map(|b| b.name.clone()).collect::<Vec<_>>());
    prim!(reg, editor, "agent-backend", |s| s.agent_config.name.clone());
    prim!(reg, editor, "agent-permit-kind", |s, kind: String| s.agent_permit_kind(&kind));
    prim!(reg, editor, "agent-command", |s| s.agent_config.line());
    prim!(reg, editor, "font-list", |s| s.fonts.clone());
    prim!(reg, editor, "terminal-shell", |s| s.terminal_shell.clone());
    // Sessions: the server acts on what these ask after the current command.
    prim!(reg, editor, "session-name", |s| s.session_name.clone());
    prim!(reg, editor, "session-list", |s| s.session_names.clone());
    // {name, project, buffers, unsaved} for each, as strings.
    prim!(reg, editor, "session-details", |s| s
        .session_details
        .iter()
        .map(|d| vec![d.name.clone(), d.project.clone(), d.buffers.to_string(), d.unsaved.to_string()])
        .collect::<Vec<Vec<String>>>());
    prim!(reg, editor, "session-switch", |s, name: String| s.session_requests.push(SessionRequest::Switch(name)));
    prim!(reg, editor, "session-last", |s| s.session_requests.push(SessionRequest::Last));
    prim!(reg, editor, "session-new", |s, name: String, root: String| {
        let root = (!root.is_empty()).then(|| s.resolve(&crate::projects::expand(&root)));
        s.session_requests.push(SessionRequest::New { name, root });
    });
    prim!(reg, editor, "session-kill", |s, name: String, force: bool| {
        s.session_requests.push(SessionRequest::Kill { name, force })
    });
    prim!(reg, editor, "session-rename", |s, name: String| s.session_requests.push(SessionRequest::Rename(name)));
    prim!(reg, editor, "set-session-per-project", |s, on: bool| s.session_per_project = on);
    // The machines the window knows: {host, how it is, "connected" or ""}.
    prim!(reg, editor, "machines", |s| s
        .machines
        .iter()
        .map(|m| vec![m.host.clone(), m.state.describe(), if m.connected { "connected".into() } else { String::new() }])
        .collect::<Vec<Vec<String>>>());
    prim!(reg, editor, "machines-check", |s| s.session_requests.push(SessionRequest::CheckMachines));
    prim!(reg, editor, "machine-forget", |s, host: String| s.session_requests.push(SessionRequest::ForgetMachine(host)));
    prim!(reg, editor, "picker-title", |s| s.picker.as_ref().map(|p| p.title().to_string()).unwrap_or_default());

    // Adds another machine's sessions to the window (`vmac connect`'s).
    prim!(reg, editor, "connect", |s, host: String, target: String| {
        s.session_requests.push(SessionRequest::Connect { host, target })
    });

    // Runs a program, returning what it printed (for plugins: hc, rex, ...).
    prim!(reg, editor, "exec", |s, command: String, args: Vec<String>| {
        std::process::Command::new(&command)
            .args(&args)
            .current_dir(&s.cwd)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    });

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
        (Lisp::new(editor.clone()), editor)
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
        lisp.eval(r#"vmac.command("delete-char-under", function() vmac.insert("!") end)"#).unwrap();
        typed(&mut lisp, "x");
        assert_eq!(text(&editor), "!abc");
    }

    #[test]
    fn binding_a_key_at_runtime() {
        let (mut lisp, editor) = lisp_with("");
        lisp.eval(r#"vmac.map("normal", "Q", function() vmac.insert("tea") end)"#).unwrap();
        typed(&mut lisp, "Q");
        assert_eq!(text(&editor), "tea");
    }

    #[test]
    fn ex_commands_from_colon() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, ":lua 1 + 2\n");
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
    fn spc_semicolon_evaluates_lua() {
        let (mut lisp, editor) = lisp_with("");
        typed(&mut lisp, " ;6 * 7\n");
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
core.define_motion("next-blank-line", "linewise", function(p)
  for l = vmac.pos_to_line(p) + 1, vmac.line_count() - 1 do
    if core.empty_line(vmac.line_to_pos(l)) then return vmac.line_to_pos(l) end
  end
  return vmac.line_to_pos(vmac.line_count() - 1)
end)
vmac.map("normal", "}", function() core.move_by("next-blank-line") end)
vmac.map("operator", "}", core.operator_motion("next-blank-line"))
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
        typed(&mut lisp, "80looks");
        assert_eq!(editor.lock().picker.as_ref().unwrap().selection(), Some("lua/core/80-looks.lua"));
        lisp.dispatch_key("RET", "");
        let st = editor.lock();
        assert!(st.picker.is_none(), "picking closes it");
        assert_eq!(st.buffer().name(), "80-looks.lua");
    }

    #[test]
    fn picker_keys_move_and_escape_closes() {
        let (mut lisp, editor) = lisp_with("");
        lisp.eval(r#"vmac.pick("Fruit", {"apple", "banana", "cherry"}, function(x) vmac.message(x) end)"#).unwrap();
        lisp.dispatch_key("C-n", "");
        lisp.dispatch_key("<down>", "");
        lisp.dispatch_key("C-p", "");
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().message, "banana");
        lisp.eval(r#"vmac.pick("Fruit", {"apple", "banana"}, function(x) vmac.message(x) end)"#).unwrap();
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
        let init = dir.join("init.lua");
        std::fs::write(&init, r#"vmac.command("hi", function() vmac.insert("hi") end)"#).unwrap();
        let editor = Editor::new(Buffer::new("t"));
        let mut lisp = Lisp::with_lua(editor.clone(), vec![init.clone()]);
        std::fs::write(&init, "vmac.map('normal', 'Q', 'x') vmac.no_such_function()").unwrap();
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
    fn syntax_theme_comes_from_lua() {
        let (mut lisp, _editor) = lisp_with("");
        let theme = lisp.syntax_theme();
        assert!(theme.iter().any(|(c, f)| c == "keyword" && !f.is_empty()), "{theme:?}");
        lisp.eval(r#"function core.syntax_theme() return {{"keyword", "red"}} end"#).unwrap();
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
            r#"vmac.pick("Fruit", {"apple", "apricot", "banana", "cherry", "grape"},
                       function(x) vmac.message("picked " .. x) end)"#,
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
    fn core_lengths_count_characters() {
        let (mut lisp, _editor) = lisp_with("");
        assert_eq!(lisp.eval(r#"core.length("▾ é")"#).unwrap(), "3");
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
    fn face_specs_come_from_lua() {
        let (mut lisp, _editor) = lisp_with("");
        lisp.eval(
            r#"function core.face_specs()
                 return { { "big", color = "accent", scale = 1.5, bold = true, sans = true },
                          { "code", background = "muted", italic = true, underline = true } }
               end"#,
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
            r#"vmac.set_faces({ { "md-h1", color = "magenta", scale = 2.2, bold = true, sans = true },
                                 { "md-bold", bold = true, color = "yellow" } })"#,
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
    fn lsp_servers_come_from_lua() {
        let (mut lisp, _editor) = lisp_with("");
        let servers = lisp.lsp_servers();
        let rust = servers.iter().find(|s| s.language == "rust").unwrap();
        assert_eq!(rust.command, "rust-analyzer");
        let py = servers.iter().find(|s| s.language == "python").unwrap();
        assert_eq!(py.args, vec!["--stdio"]);
    }

    #[test]
    fn diagnostics_jump_list_and_count() {
        let dir = std::env::temp_dir().join(format!("vmac-lsp-diagnostics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let server = dir.join("server.py");
        std::fs::write(&server, crate::lsp::tests::FAKE_SERVER).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, "fn main() {}\nlet x = 1;\n").unwrap();

        let (mut lisp, editor) = lisp_with("");
        lisp.eval(&format!(r#"function core.lsp_servers() return {{ {{ "rust", "python3", "{}" }} }} end"#, server.display())).unwrap();
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
        // It asks: *agent* shows the question, what it's about, and the choices.
        assert!(pump(&mut lisp, &|s| s.agent_permission_waiting()), "{}", editor.lock().message);
        assert!(editor.lock().picker.is_none(), "no picker over it");
        let shown = editor.lock().buffer().text().to_string();
        assert!(shown.contains("⚠ Allow Edit notes.txt?\n\n  Shout the file.\n\n  - read it\n  - upper-case it\n\n  1  Allow\n  2  Reject\n"), "{shown}");
        let line = editor.lock().buffer().line();
        assert!(shown.lines().nth(line).unwrap().starts_with("⚠ Allow Edit"), "the cursor is on the question");
        assert!(editor.lock().message.contains("Allow Edit notes.txt?"));
        typed(&mut lisp, "1");
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
        assert!(pump(&mut lisp, &|s| s.agent_permission_waiting()));
        // RET on a choice answers.
        let allow = text().lines().position(|l| l == "  1  Allow").unwrap();
        let pos = editor.lock().buffer().line_to_pos(allow);
        editor.lock().buffer_mut().goto_char(pos);
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
        assert!(review.contains("diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -3,3 +3,4 @@\n three\n four\n five\n+added"), "{review}");
        assert!(review.contains("@@ -1,5 +1,5 @@\n one\n-two\n+TWO\n three\n-four"), "{review}");
        // gpui-kit's Diff reads it: one file each.
        let patch: String = review.lines().filter(|l| !l.is_empty()).map(|l| format!("{l}\n")).collect();
        let files = gpui_kit::component::diff::DiffFile::parse(&patch).expect("a patch gpui-kit parses");
        assert_eq!(files.iter().map(|f| f.path().to_string()).collect::<Vec<_>>(), vec!["a.txt", "b.txt"]);
        typed(&mut lisp, "gg]h");
        typed(&mut lisp, "gx");
        assert_eq!(disk(&file), "one\nTWO\nthree\nfour\nfive\n", "rejected from the list");
        assert!(!text(&editor).contains("a.txt"), "{}", text(&editor));
        typed(&mut lisp, "gg]h");
        assert_eq!(text(&editor).lines().nth(editor.lock().buffer().line()), Some("-two"));
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().buffer().name(), "b.txt");
        assert_eq!(editor.lock().buffer().line(), 1, "RET on a removed line goes to where it was");
        typed(&mut lisp, " ar");
        let added = text(&editor).lines().position(|l| l == "+added").unwrap();
        let pos = editor.lock().buffer().line_to_pos(added);
        editor.lock().buffer_mut().goto_char(pos);
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().buffer().line(), 4, "RET on an added line goes to that line");

        // SPC a A takes the rest.
        typed(&mut lisp, " aA");
        assert_eq!(editor.lock().review_count(), 0);
        assert_eq!(disk(&other), patched);
        editor.lock().agent_show();
        editor.lock().agent_pump();
        assert!(!text(&editor).contains("to review"), "{}", text(&editor));

        // With review off, edits just land.
        lisp.eval("vmac.set_agent_review(false)").unwrap();
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
        assert_eq!(lisp.eval("core.dashboard_on_start").unwrap(), "true");

        lisp.eval("core.call_binding(\'dashboard\')").unwrap();
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
        lisp.eval(&format!("core.project_switch_to({:?})", two.display().to_string())).unwrap();
        assert_eq!(editor.lock().project.as_ref(), Some(&two));
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().buffer().file(), Some(&two.join("main.rs")));
        lisp.eval("core.call_binding(\'dashboard\')").unwrap();
        let shown = text(&editor);
        let first = shown.find(&crate::projects::abbreviate(&two)).unwrap();
        let second = shown.find(&crate::projects::abbreviate(&one)).unwrap();
        assert!(first < second, "the latest project comes first: {shown}");

        // Not a folder: says so.
        lisp.eval("core.project_switch_to('/no/such/folder')").unwrap();
        assert!(editor.lock().message.contains("not a folder"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_agent_gets_files_problems_and_runs_commands_in_terminals() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-context-{}", std::process::id()));
        let config = crate::agent::tests::fake_config(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "saved\n").unwrap();
        let (mut lisp, editor) = lisp_with("");
        editor.lock().agent_config = config;
        editor.lock().open_file(&file).unwrap();
        typed(&mut lisp, "ccunsaved");
        lisp.dispatch_key("ESC", "");
        let pump = |lisp: &mut Lisp, done: &dyn Fn(&mut EditorState) -> bool| {
            for _ in 0..300 {
                lisp.pump_terminals();
                editor.lock().agent_pump();
                lisp.run_events();
                if done(&mut editor.lock()) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        };
        let said = |s: &mut EditorState, what: &str| {
            s.agent.as_ref().is_some_and(|a| a.state == crate::agent::State::Idle && a.transcript().contains(what))
        };

        // @a.txt sends the buffer's text along.
        editor.lock().agent_prompt("what is in @a.txt?");
        assert!(pump(&mut lisp, &|s| said(s, "You said")), "{:?}", editor.lock().agent.as_ref().map(|a| a.transcript()));
        assert!(editor.lock().agent.as_ref().unwrap().transcript().contains("Attached: a.txt=unsaved."));

        // In the prompt, @ picks a file to attach.
        typed(&mut lisp, " aa");
        typed(&mut lisp, "see ");
        lisp.dispatch_key("@", "@");
        let title = editor.lock().picker.as_ref().map(|p| p.title().to_string());
        assert_eq!(title.as_deref(), Some("Attach file"));
        lisp.dispatch_key("RET", "");
        assert!(text(&editor).ends_with("─\nsee @a.txt "), "{:?}", text(&editor));
        typed(&mut lisp, "x@y");
        assert!(editor.lock().picker.is_none(), "not at a word's start: just @");
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "0D");

        // SPC a e: the file's problems, with the file.
        editor.lock().lsp.set_diagnostics(file.clone(), vec![crate::lsp::Diagnostic {
            start: (0, 0), end: (0, 3), severity: 1, message: "boom".into(), source: None, raw: serde_json::Value::Null,
        }]);
        editor.lock().open_file(&file).unwrap();
        typed(&mut lisp, " ae");
        lisp.dispatch_key("RET", "");
        assert!(pump(&mut lisp, &|s| said(s, "a.txt:1:1: error: boom")));
        let t = editor.lock().agent.as_ref().unwrap().transcript();
        assert!(t.contains("› Fix these problems.  [1 problem in a.txt]"), "{t}");

        // A command runs in a vmac terminal the agent waits on and reads.
        editor.lock().agent_prompt("run echo $GREETING; echo second; exit 3");
        assert!(pump(&mut lisp, &|s| said(s, "exit 3")), "{:?}", editor.lock().agent.as_ref().map(|a| a.transcript()));
        let t = editor.lock().agent.as_ref().unwrap().transcript();
        assert!(t.contains("exit 3: hi there / second"), "{t}");
        let names = editor.lock().buffer_names();
        let term = names.iter().find(|n| n.starts_with("*agent: echo $GREETING")).cloned().expect("a terminal buffer");
        editor.lock().switch_to_buffer(&term);
        assert!(text(&editor).contains("hi there\nsecond"), "{}", text(&editor));

        // Opened, the tool call shows the terminal and its lines.
        let id = editor.lock().agent.as_ref().unwrap().entries.iter().find_map(|e| match e {
            crate::agent::Entry::Tool(t) if t.title == "Run it" => Some(t.id.clone()),
            _ => None,
        }).unwrap();
        editor.lock().agent.as_mut().unwrap().toggle(&id);
        let t = editor.lock().agent.as_ref().unwrap().transcript();
        assert!(t.contains(&format!("    ⌁ {term}  (RET opens it)\n    hi there\n    second")), "{t}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn agents_are_chosen_by_name_and_rules_and_hooks_answer() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-hooks-{}", std::process::id()));
        let config = crate::agent::tests::fake_config(&dir);
        let file = dir.join("a.txt");
        std::fs::write(&file, "quiet\n").unwrap();
        let (mut lisp, editor) = lisp_with("");
        editor.lock().open_file(&file).unwrap();

        // Backends by name.
        let names = lisp.eval("table.concat(vmac.agent_backends(), ' ')").unwrap();
        assert!(names.contains("claude") && names.contains("codex") && names.contains("gemini"), "{names}");
        lisp.eval(&format!("vmac.agent_define(\'fake\', {:?})", config.line())).unwrap();
        lisp.eval_lua("vmac.agent_backend('other', { command = 'other-acp --flag' })").unwrap();
        assert!(lisp.eval("table.concat(vmac.agent_backends(), ' ')").unwrap().contains("other"));
        typed(&mut lisp, ":agent-use fake");
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().agent_config, config);
        assert_eq!(lisp.eval("vmac.agent_backend()").unwrap(), "fake");

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
        let idle = |s: &mut EditorState| s.agent.as_ref().is_some_and(|a| a.state == crate::agent::State::Idle && a.transcript().contains("✓ Edit") || a.transcript().contains("✗ Edit"));

        // A rule allows edits: no one is asked; hooks hear it all.
        lisp.eval(r#"
            vmac.hooks.agent_permission = function(title, kind) if kind == "edit" then return "allow" end end
            heard = {}
            vmac.hooks.agent_tool_call = function(title, kind, status) heard[#heard + 1] = kind .. ":" .. status end
            vmac.hooks.agent_turn_end = function(reason) heard[#heard + 1] = "done:" .. reason end
        "#).unwrap();
        editor.lock().agent_prompt(&format!("edit {}", file.display()));
        assert!(pump(&mut lisp, &|s| idle(s) && s.agent_status() == "ready"), "{:?}", editor.lock().agent.as_ref().map(|a| a.transcript()));
        assert!(editor.lock().picker.is_none(), "nobody was asked");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "QUIET\n");
        let heard = lisp.eval("table.concat(heard, ' ')").unwrap();
        assert!(heard.contains("done:end_turn") && heard.contains("edit:completed") && heard.contains("edit:pending"), "{heard}");

        // Another rule rejects.
        lisp.eval_lua("vmac.hooks.agent_permission = function(title, kind) return 'reject' end").unwrap();
        lisp.eval_lua("ended = nil; vmac.hooks.agent_turn_end = function(reason) ended = reason end").unwrap();
        lisp.eval("core.call_binding(\'agent-new\')").unwrap();
        editor.lock().agent_prompt(&format!("edit {}", file.display()));
        assert!(pump(&mut lisp, &|s| s.agent.as_ref().is_some_and(|a| a.transcript().contains("✗ Edit")) && s.agent_status() == "ready"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "QUIET\n", "rejected: unchanged");
        assert_eq!(lisp.eval_lua("return ended").unwrap(), "end_turn");

        // The dashboard's items can be redefined too.
        lisp.eval(r#"function core.dashboard_items() return { { "z", "Zebra", "quit" } } end"#).unwrap();
        lisp.eval("core.call_binding(\'dashboard\')").unwrap();
        assert!(text(&editor).contains("z   Zebra"), "{}", text(&editor));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn yanks_and_pastes_go_through_the_system_clipboard() {
        let (mut lisp, editor) = lisp_with("one two\n");
        let board = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        editor.lock().clipboard = Some(crate::editor::SystemClipboard(Box::new(crate::clipboard::Memory(board.clone()))));

        // A yank lands on the clipboard.
        typed(&mut lisp, "yiw");
        assert_eq!(*board.lock().unwrap(), "one");
        // Copied in another app: p pastes that, not the older yank.
        *board.lock().unwrap() = "FROM MAC".into();
        typed(&mut lisp, "$p");
        assert_eq!(text(&editor), "one twoFROM MAC\n");
        typed(&mut lisp, "0yiw");
        typed(&mut lisp, "$p");
        assert_eq!(text(&editor), "one twoFROM MACone\n", "then vmac's own yank again");

        // Cmd-V in insert state and in a prompt; Cmd-C copies the selection.
        *board.lock().unwrap() = "pasted".into();
        typed(&mut lisp, "o");
        lisp.dispatch_key("s-v", "");
        lisp.dispatch_key("ESC", "");
        assert_eq!(text(&editor), "one twoFROM MACone\npasted\n");
        typed(&mut lisp, "ggviw");
        lisp.dispatch_key("s-c", "");
        assert_eq!(*board.lock().unwrap(), "one");
        typed(&mut lisp, ":");
        lisp.dispatch_key("s-v", "");
        assert_eq!(editor.lock().minibuffer.text().to_string(), "one");
        lisp.dispatch_key("ESC", "");

        // Unsynced, p uses vmac's register; Cmd-V still the clipboard.
        lisp.eval("vmac.set_clipboard_sync(false)").unwrap();
        *board.lock().unwrap() = "elsewhere".into();
        typed(&mut lisp, "Gddu");
        typed(&mut lisp, "ggyy");
        assert_eq!(*board.lock().unwrap(), "elsewhere", "the yank stayed in vmac");
        typed(&mut lisp, "P");
        assert!(text(&editor).starts_with("one twoFROM MACone\none twoFROM MACone\n"), "{}", text(&editor));
    }

    #[test]
    fn themes_by_name_and_the_markdown_views() {
        let dir = std::env::temp_dir().join(format!("vmac-md-views-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut lisp, editor) = lisp_with("");
        editor.lock().theme_names = vec![("Default Light".into(), false), ("Catppuccin Latte".into(), false), ("Catppuccin Mocha".into(), true)];

        // A theme by name switches to its mode; system follows again.
        typed(&mut lisp, ":theme catppuccin mocha");
        lisp.dispatch_key("RET", "");
        let (mode, dark) = { let st = editor.lock(); (st.theme_mode.clone(), st.dark_theme.clone()) };
        assert_eq!((mode.as_str(), dark.as_str()), ("dark", "Catppuccin Mocha"));
        lisp.eval(r#"vmac.set_themes("Catppuccin Latte", "Catppuccin Mocha")"#).unwrap();
        assert_eq!(editor.lock().theme_mode, "system");
        assert_eq!(editor.lock().theme_name(false), "Catppuccin Latte");
        lisp.eval(r#"vmac.set_theme("Nope")"#).unwrap();
        assert!(editor.lock().message.contains("No theme called Nope"));
        lisp.eval("core.call_binding(\'theme-picker\')").unwrap();
        assert_eq!(editor.lock().picker.as_ref().map(|p| p.title().to_string()).as_deref(), Some("Theme"));
        lisp.dispatch_key("ESC", "");
        lisp.dispatch_key("ESC", "");

        // Markup is hidden outside insert state; raw while typing.
        assert_eq!(lisp.eval("core.markdown_raw()").unwrap(), "false");
        typed(&mut lisp, "i");
        assert_eq!(lisp.eval("core.markdown_raw()").unwrap(), "true");
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "v");
        assert_eq!(lisp.eval("core.markdown_raw()").unwrap(), "true", "selecting shows the text too");
        lisp.dispatch_key("ESC", "");
        // SPC t m: markup hidden -> as typed -> rendered -> markup hidden.
        let view = || { let st = editor.lock(); (st.markdown_rendered, st.markdown_conceal) };
        assert_eq!(view(), (false, true), "markup hidden, not rendered, by default");
        typed(&mut lisp, " tm");
        assert_eq!(view(), (false, false));
        typed(&mut lisp, " tm");
        assert_eq!(view(), (true, true));
        typed(&mut lisp, " tm");
        assert_eq!(view(), (false, true));
        lisp.eval(r#"vmac.set_markdown_view("raw")"#).unwrap();
        assert_eq!(view(), (false, false));

        // The preview takes the right of the editor while a Markdown file shows.
        let notes = dir.join("notes.md");
        std::fs::write(&notes, "# Notes\n").unwrap();
        assert!(editor.lock().preview_rect().is_none(), "nothing to preview yet");
        editor.lock().open_file(&notes).unwrap();
        typed(&mut lisp, " mp");
        let (rect, widest) = {
            let st = editor.lock();
            (st.preview_rect(), st.window_rects().iter().map(|(_, r)| r.x + r.w).fold(0., f32::max))
        };
        assert!(rect.is_some());
        assert!((widest - (1. - crate::editor::PREVIEW_WIDTH)).abs() < 1e-4, "windows make room: {widest}");
        let code = dir.join("main.rs");
        std::fs::write(&code, "fn main() {}\n").unwrap();
        editor.lock().open_file(&code).unwrap();
        assert!(editor.lock().preview_rect().is_some(), "from another file, it keeps showing the last Markdown file");
        typed(&mut lisp, " mp");
        assert!(editor.lock().preview_rect().is_none(), "closed");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn past_sessions_are_listed_and_reopened() {
        let dir = std::env::temp_dir().join(format!("vmac-agent-sessions-{}", std::process::id()));
        let config = crate::agent::tests::fake_config(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        let (mut lisp, editor) = lisp_with("");
        editor.lock().open_file(&dir.join("a.txt")).unwrap();
        lisp.eval(&format!("vmac.agent_define(\'fake\', {:?})", config.line())).unwrap();
        lisp.eval("vmac.agent_use('fake')").unwrap();
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
        let picker_items = |editor: &Editor| editor.lock().picker.as_ref().map(|p| p.title().to_string());

        // An agent that lists its own (ACP session/list).
        typed(&mut lisp, " ah");
        assert!(pump(&mut lisp, &|s| s.picker.is_some()), "{}", editor.lock().message);
        assert_eq!(picker_items(&editor).as_deref(), Some("Agent sessions"));
        let first = editor.lock().picker.as_ref().unwrap().selection().unwrap().to_string();
        assert!(first.starts_with("1. Second session  ·  "), "{first}");
        lisp.dispatch_key("RET", "");
        assert!(pump(&mut lisp, &|s| s.agent.as_ref().is_some_and(|a| a.transcript().contains("reopened the session"))));
        let t = editor.lock().agent.as_ref().unwrap().transcript();
        assert!(t.contains("— Reopening “Second session”"), "{t}");
        assert!(t.contains("› earlier question s-new\n\n▸ ✓ Read old.txt\n"), "the history, replayed: {t}");
        assert!(t.contains("earlier answer"), "{t}");
        editor.lock().agent_prompt("and now?");
        assert!(pump(&mut lisp, &|s| s.agent.as_ref().is_some_and(|a| a.transcript().contains("You said: and now?"))), "carries on");

        // Claude Code's sessions come from its files, any project's.
        let projects = dir.join("claude-projects");
        std::fs::create_dir_all(projects.join("-proj")).unwrap();
        let entry = |v: serde_json::Value| format!("{v}\n");
        std::fs::write(
            projects.join("-proj").join("cli-session-7.jsonl"),
            entry(serde_json::json!({ "type": "user", "cwd": dir.display().to_string(), "message": { "content": "from the CLI" } }))
                + &entry(serde_json::json!({ "type": "ai-title", "aiTitle": "Work done in the terminal" })),
        )
        .unwrap();
        editor.lock().claude_dir = Some(projects);
        lisp.eval(&format!("vmac.agent_define(\'claude\', {:?})", config.line())).unwrap();
        lisp.eval("vmac.agent_use('claude')").unwrap();
        lisp.eval("core.call_binding(\'agent-history-all\')").unwrap();
        lisp.run_events();
        let first = editor.lock().picker.as_ref().map(|p| p.selection().unwrap_or("").to_string());
        assert!(first.as_deref().is_some_and(|f| f.starts_with("1. Work done in the terminal  ·  ")), "{first:?}");
        lisp.dispatch_key("RET", "");
        assert!(pump(&mut lisp, &|s| s.agent.as_ref().is_some_and(|a| a.transcript().contains("earlier question cli-session-7"))),
            "the agent was asked to load Claude's session by its id: {:?}", editor.lock().agent.as_ref().map(|a| a.transcript()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cli_agents_run_beside_the_code_and_their_edits_wait_for_review() {
        let dir = std::env::temp_dir().join(format!("vmac-cli-agent-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        // A CLI agent: says what it got; "edit F" rewrites F; --resume ID says so.
        let script = dir.join("cli.py");
        std::fs::write(&script, r#"
import sys
if len(sys.argv) > 2 and sys.argv[1] == "--resume":
    print("resumed " + sys.argv[2], flush=True)
for line in sys.stdin:
    line = line.strip()
    print("got: " + line, flush=True)
    if line.startswith("edit "):
        open(line[5:], "w").write("EDITED\n")
"#).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let (mut lisp, editor) = lisp_with("");
        editor.lock().open_file(&file).unwrap();

        // The Lua core defines the CLI agents.
        let names = lisp.eval("table.concat(vmac.agent_backends(), ' ')").unwrap();
        assert!(names.contains("claude-cli") && names.contains("codex-cli"), "{names}");
        lisp.eval_lua(&format!("vmac.agent_backend('fakecli', {{ terminal = 'python3 {}' }})", script.display())).unwrap();
        lisp.eval("vmac.agent_use('fakecli')").unwrap();
        assert_eq!(lisp.eval("vmac.agent_kind()").unwrap(), "cli");

        let pump = |lisp: &mut Lisp, done: &dyn Fn(&mut EditorState) -> bool| {
            for _ in 0..250 {
                lisp.pump_terminals();
                editor.lock().agent_pump();
                lisp.run_events();
                if done(&mut editor.lock()) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        };
        let cli_text = |s: &mut EditorState| s.buffer_text_named("*fakecli*").unwrap_or_default();

        // SPC a a: its pane on the right, typing to it.
        typed(&mut lisp, " aa");
        assert_eq!(editor.lock().buffer().name(), "*fakecli*");
        assert_eq!(lisp.eval("core.terminal_insert()").unwrap(), "true");
        assert_eq!(editor.lock().agent_status(), "running");
        lisp.dispatch_key("ESC", "");

        // SPC a s on a line of a.txt: the question and the place, in one line.
        editor.lock().open_file(&file).unwrap();
        typed(&mut lisp, "j as");
        lisp.dispatch_key("RET", "");
        assert!(pump(&mut lisp, &|s| cli_text(s).contains("got: Explain this code. (in a.txt line 2)")), "{}", cli_text(&mut editor.lock()));

        // It edits a.txt on disk: the buffer follows, and the change waits for review.
        editor.lock().agent_cli_send("edit a.txt");
        assert!(pump(&mut lisp, &|s| s.buffer_text_named("a.txt").as_deref() == Some("EDITED\n")), "{:?}", editor.lock().buffer_text_named("a.txt"));
        assert!(editor.lock().review_count() > 0, "its change waits for review");
        editor.lock().open_file(&file).unwrap();
        assert!(!editor.lock().buffer().is_modified(), "reloaded, not an unsaved edit");
        typed(&mut lisp, "gg]h");
        typed(&mut lisp, "gx");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\n", "rejected: the old text is back");

        // SPC a t hides its pane (from it), and shows it again.
        lisp.eval("vmac.agent_cli_open()").unwrap();
        assert!(editor.lock().agent_cli_here());
        typed(&mut lisp, " at");
        assert!(!editor.lock().agent_cli_here());
        typed(&mut lisp, " at");
        assert!(editor.lock().agent_cli_here());
        typed(&mut lisp, " ak");
        assert_eq!(editor.lock().agent_status(), "off", "stopped, as an ACP agent's stop reads");

        // A Claude-style CLI reopens a session with its resume command.
        let projects = dir.join("claude-projects");
        std::fs::create_dir_all(projects.join("-p")).unwrap();
        std::fs::write(
            projects.join("-p").join("sess-42.jsonl"),
            format!("{}\n", serde_json::json!({ "type": "user", "cwd": dir.display().to_string(), "message": { "content": "earlier" } })),
        )
        .unwrap();
        editor.lock().claude_dir = Some(projects);
        let line = format!("python3 {}", script.display());
        lisp.eval_lua(&format!("vmac.agent_backend('claude-cli', {{ terminal = '{line}', resume = '{line} --resume {{id}}' }})")).unwrap();
        lisp.eval("vmac.agent_use('claude-cli')").unwrap();
        lisp.eval("core.call_binding(\'agent-history-all\')").unwrap();
        lisp.run_events();
        lisp.dispatch_key("RET", "");
        assert!(pump(&mut lisp, &|s| s.buffer_text_named("*claude-cli*").is_some_and(|t| t.contains("resumed sess-42"))),
            "{:?}", editor.lock().buffer_text_named("*claude-cli*"));
        typed(&mut lisp, " ak");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn comments_on_changes_go_to_the_agent() {
        let dir = std::env::temp_dir().join(format!("vmac-review-comments-{}", std::process::id()));
        let config = crate::agent::tests::fake_config(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let file = dir.join("b.txt");
        std::fs::write(&file, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let (mut lisp, editor) = lisp_with("");
        editor.lock().agent_config = config;
        editor.lock().open_file(&file).unwrap();
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
        editor.lock().agent_prompt(&format!("patch {}", file.display()));
        assert!(pump(&mut lisp, &|s| s.agent_status() == "ready" && s.review_count() == 3));

        // The review's rows know their file line: "-two" is old line 2, "+TWO" new line 2.
        typed(&mut lisp, " ar");
        let review = text(&editor);
        let row_of = |needle: &str| review.lines().position(|l| l == needle).unwrap();
        let rows = editor.lock().review_rows.clone();
        let at = |r: &Option<crate::editor::ReviewRow>| r.as_ref().map(|r| (r.rel.clone(), r.modified, r.line));
        assert_eq!(at(&rows[row_of("-two")]), Some(("b.txt".into(), false, 2)));
        assert_eq!(at(&rows[row_of("+TWO")]), Some(("b.txt".into(), true, 2)));

        // c in the review comments on the row under the cursor.
        let pos = editor.lock().buffer().line_to_pos(row_of("+TWO"));
        editor.lock().buffer_mut().goto_char(pos);
        typed(&mut lisp, "c");
        typed(&mut lisp, "keep it lowercase");
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().review_comments.len(), 1, "{}", editor.lock().message);

        // gc in the file, on line 5; the + button in the Diff (through Lua).
        editor.lock().open_file(&file).unwrap();
        typed(&mut lisp, "4jgc");
        typed(&mut lisp, "why move this?");
        lisp.dispatch_key("RET", "");
        let id = editor.lock().window().buffer;
        assert_eq!(editor.lock().comment_lines(id), vec![1, 4], "marked in the file");
        editor.lock().push_event("lua-event", vec!["review_comment_at".into(), "b.txt".into(), "original".into(), "4".into()]);
        lisp.run_events();
        typed(&mut lisp, "bring four back");
        lisp.dispatch_key("RET", "");
        assert_eq!(editor.lock().review_comments.len(), 3);
        // gC deletes the one on this line.
        typed(&mut lisp, "gC");
        assert_eq!(editor.lock().review_comments.len(), 2);
        typed(&mut lisp, "gg4jgc");
        typed(&mut lisp, "why add this?");
        lisp.dispatch_key("RET", "");

        // SPC a S: sent to the agent, with the code they're on; then cleared.
        typed(&mut lisp, " aS");
        assert!(editor.lock().review_comments.is_empty());
        assert!(pump(&mut lisp, &|s| s.agent.as_ref().is_some_and(|a| a.transcript().contains("You said: Please revise"))));
        let t = editor.lock().agent.as_ref().unwrap().transcript();
        assert!(t.contains("› Revise by my 3 review comments"), "{t}");
        assert!(t.contains("- `b.txt:2`: keep it lowercase"), "{t}");
        assert!(t.contains("> `TWO`"), "with the code: {t}");
        assert!(t.contains("- `b.txt:4` (the old version): bring four back"), "{t}");
        assert!(t.contains("> `four`"), "the old side's code: {t}");
        assert!(t.contains("- `b.txt:5`: why add this?"), "{t}");
        typed(&mut lisp, " aS");
        assert!(editor.lock().message.contains("No comments to send"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_mouse_clicks_drags_and_double_clicks() {
        let (mut lisp, editor) = lisp_with("alpha beta\ngamma delta\n");
        let state = |lisp: &mut Lisp| lisp.eval("core.state").unwrap();
        lisp.eval("core.mouse_click(8, 1)").unwrap();
        assert_eq!(editor.lock().buffer().point(), 8);
        lisp.eval("core.mouse_click(10, 1)").unwrap();
        assert_eq!(editor.lock().buffer().point(), 9, "past the line's end: its last character");
        // Dragging from beta into gamma selects, as v does.
        lisp.eval("core.mouse_click(6, 1)").unwrap();
        lisp.eval("core.mouse_drag(13)").unwrap();
        assert_eq!(state(&mut lisp), "visual");
        assert_eq!(lisp.selection(), vec![(6, 14)]);
        lisp.eval("core.mouse_click(0, 1)").unwrap();
        assert_eq!(state(&mut lisp), "normal", "a click ends the selection");
        lisp.eval("core.mouse_click(13, 2)").unwrap();
        assert_eq!(lisp.selection(), vec![(11, 16)], "a double click selects the word");
        assert!(editor.lock().whichkey.is_none(), "and leaves no which-key popup open");
        lisp.eval("core.mouse_click(13, 3)").unwrap();
        assert_eq!(state(&mut lisp), "visual-line", "a triple click, the line");
        // Typing: a click moves the cursor and keeps inserting.
        lisp.eval("core.mouse_click(0, 1)").unwrap();
        typed(&mut lisp, "i");
        lisp.eval("core.mouse_click(10, 1)").unwrap();
        assert_eq!((state(&mut lisp).as_str(), editor.lock().buffer().point()), ("insert", 10));
    }

    #[test]
    fn the_wheel_scrolls_a_window_and_keeps_its_cursor_on_screen() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let (_lisp, editor) = lisp_with(&text);
        let mut st = editor.lock();
        let id = st.selected_window();
        st.fit_window(id, 10);
        st.scroll_window_by(id, 30);
        assert_eq!(st.window().scroll_top, 30);
        assert_eq!(st.buffer().line(), 30, "the cursor comes along");
        st.scroll_window_by(id, -25);
        assert_eq!((st.window().scroll_top, st.buffer().line()), (5, 14), "to the bottom row, scrolling up");
        st.scroll_window_by(id, -50);
        assert_eq!(st.window().scroll_top, 0, "not above the top");
    }

    #[test]
    fn terminal_programs_that_ask_for_the_mouse_get_it() {
        let (mut lisp, editor) = lisp_with("");
        // Turns on click tracking with SGR reports; the tty echoes what it reads.
        typed(&mut lisp, ":term sh\n");
        typed(&mut lisp, "printf '\\033[?1000h\\033[?1006h'; cat\n");
        let buffer = editor.lock().window().buffer;
        let mode = |st: &EditorState| {
            st.terminals.iter().find(|t| t.buffer == buffer).and_then(|t| match &t.backend {
                crate::editor::TermBackend::Local(pty) => Some(pty.mouse_mode()),
                _ => None,
            })
        };
        let asked = pump_terminals_until(&mut lisp, &editor, |st| mode(st).is_some_and(|m| m.event != crate::terminal::MouseEvent::None));
        assert!(asked, "the program asked for the mouse");
        let mods = crate::terminal::MouseMods::default();
        let top = mode(&editor.lock()).unwrap().screen_top;
        assert!(editor.lock().terminal_mouse(buffer, crate::terminal::MouseAction::Press(0), top + 2, 4, mods));
        // Column 5, row 3 (1-based), left button down.
        assert!(
            pump_terminals_until(&mut lisp, &editor, |st| st.buffer().text().to_string().contains("[<0;5;3M")),
            "{}",
            editor.lock().buffer().text()
        );
    }

    #[test]
    fn a_checkout_core_this_binary_cant_run_falls_back_to_the_built_in_one() {
        // A core folder calling a primitive this binary doesn't have.
        let dir = std::env::temp_dir().join(format!("vmac-newer-core-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("00-core.lua"), "vmac.primitive_from_the_future()").unwrap();
        let editor = Editor::new(Buffer::new("*scratch*"));
        let mut lisp = Lisp::with_core_dir(editor.clone(), dir.clone());
        let message = editor.lock().message.clone();
        assert!(message.contains("using the core built into vmac"), "{message}");
        assert_eq!(lisp.eval("type(core.dispatch_key)").unwrap(), "function", "the built-in core runs");
        assert!(!lisp.modeline().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn line_numbers_are_set_from_lua_and_toggled() {
        let (mut lisp, editor) = lisp_with("one\ntwo\n");
        assert_eq!(editor.lock().line_numbers, "off");
        lisp.eval_lua("vmac.set_line_numbers('on')").unwrap();
        assert_eq!(editor.lock().line_numbers, "on");
        typed(&mut lisp, " tl");
        assert_eq!(editor.lock().line_numbers, "relative");
        typed(&mut lisp, " tl");
        assert_eq!(editor.lock().line_numbers, "off");
        lisp.eval_lua("vmac.set_line_numbers('sideways')").unwrap();
        assert_eq!(editor.lock().line_numbers, "off", "unknown modes are refused");
    }

    #[test]
    fn pdfs_follow_the_theme_until_toggled() {
        let (mut lisp, editor) = lisp_with("");
        assert!(editor.lock().pdf_themed, "on by default");
        typed(&mut lisp, " tp");
        assert!(!editor.lock().pdf_themed);
        lisp.eval_lua("vmac.set_pdf_themed(not vmac.pdf_themed())").unwrap();
        assert!(editor.lock().pdf_themed);
    }

    #[test]
    fn keys_in_a_pdf_scroll_it() {
        use crate::frame::MediaScroll::*;
        let (mut lisp, editor) = lisp_with("one\ntwo\n");
        let path = std::env::temp_dir().join(format!("vmac-keys-{}.pdf", std::process::id()));
        std::fs::write(&path, b"%PDF-1.4").unwrap();
        editor.lock().open_file(&path).unwrap();
        typed(&mut lisp, "jk3j");
        lisp.dispatch_key("C-d", "");
        typed(&mut lisp, "JK5Ggg");
        typed(&mut lisp, "G");
        let id = editor.lock().window().buffer;
        let asked: Vec<_> = editor.lock().media_scroll[&id].iter().map(|(_, how)| *how).collect();
        assert_eq!(
            asked,
            [Lines(1), Lines(-1), Lines(1), Lines(1), Lines(1), Screens(0.5), Pages(1), Pages(-1), Page(4), Page(0), End]
        );
        let numbers: Vec<u64> = editor.lock().media_scroll[&id].iter().map(|(n, _)| *n).collect();
        assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1), "numbered in order: {numbers:?}");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn images_take_the_same_keys() {
        let (mut lisp, editor) = lisp_with("");
        let path = std::env::temp_dir().join(format!("vmac-image-keys-{}.png", std::process::id()));
        std::fs::write(&path, b"\x89PNG").unwrap();
        editor.lock().open_file(&path).unwrap();
        typed(&mut lisp, "+jl");
        let st = editor.lock();
        let id = st.window().buffer;
        assert_eq!(st.media_zoom[&id], 1.25);
        let asked: Vec<_> = st.media_scroll[&id].iter().map(|(_, how)| *how).collect();
        assert_eq!(asked, [crate::frame::MediaScroll::Lines(1), crate::frame::MediaScroll::Sideways(1)]);
        drop(st);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pdfs_zoom_in_steps_and_back_to_fitting() {
        let (mut lisp, editor) = lisp_with("");
        let path = std::env::temp_dir().join(format!("vmac-zoom-{}.pdf", std::process::id()));
        std::fs::write(&path, b"%PDF-1.4").unwrap();
        editor.lock().open_file(&path).unwrap();
        let zoom = |e: &Editor| {
            let st = e.lock();
            st.media_zoom.get(&st.window().buffer).copied().unwrap_or(1.)
        };
        typed(&mut lisp, "++");
        assert!((zoom(&editor) - 1.5625).abs() < 1e-4, "{}", zoom(&editor));
        assert_eq!(editor.lock().message, "Zoom 156%");
        typed(&mut lisp, "-");
        assert!((zoom(&editor) - 1.25).abs() < 1e-4);
        lisp.dispatch_key("s-0", "");
        assert_eq!(zoom(&editor), 1.);
        typed(&mut lisp, "20-");
        assert_eq!(zoom(&editor), 0.25, "no smaller than a quarter");
        typed(&mut lisp, "=hl");
        assert_eq!(zoom(&editor), 1.);
        let id = editor.lock().window().buffer;
        let last: Vec<_> = editor.lock().media_scroll[&id].iter().map(|(_, how)| *how).collect();
        assert_eq!(last, [crate::frame::MediaScroll::Sideways(-1), crate::frame::MediaScroll::Sideways(1)]);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn spc_m_p_shows_the_pdf_built_from_a_file_beside_it() {
        let dir = std::env::temp_dir().join(format!("vmac-pdf-preview-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("build")).unwrap();
        let (mut lisp, editor) = lisp_with("");
        let typ = dir.join("paper.typ");
        std::fs::write(&typ, "= Hello\n").unwrap();
        editor.lock().open_file(&typ).unwrap();
        let shown = |e: &Editor| -> Vec<String> {
            let st = e.lock();
            st.window_order()
                .into_iter()
                .filter_map(|w| st.window_by_id(w).and_then(|w| st.buffer_by_id(w.buffer)))
                .map(|b| b.name().to_string())
                .collect()
        };

        // Not built yet: it opens anyway, to show up when it is.
        typed(&mut lisp, " mp");
        assert_eq!(shown(&editor), ["paper.typ", "paper.pdf"]);
        assert_eq!(editor.lock().buffer().name(), "paper.typ", "the cursor stays in the source");
        assert!(editor.lock().message.starts_with("Not built yet"), "{}", editor.lock().message);

        // Again: closed.
        typed(&mut lisp, " mp");
        assert_eq!(shown(&editor), ["paper.typ"]);
        assert_eq!(editor.lock().message, "PDF closed");

        // Built into build/: that one.
        std::fs::write(dir.join("build/paper.pdf"), b"%PDF-1.4").unwrap();
        typed(&mut lisp, " mp");
        let st = editor.lock();
        let pdf = st.window_order().into_iter().filter_map(|w| st.window_by_id(w)).nth(1).map(|w| w.buffer);
        let file = pdf.and_then(|b| st.buffer_by_id(b)).and_then(|b| b.file().cloned());
        assert_eq!(file, Some(std::path::absolute(dir.join("build/paper.pdf")).unwrap()));
        drop(st);

        // In Markdown, SPC m p is its preview.
        typed(&mut lisp, " mp");
        let md = dir.join("notes.md");
        std::fs::write(&md, "# Notes\n").unwrap();
        editor.lock().open_file(&md).unwrap();
        typed(&mut lisp, " mp");
        assert!(editor.lock().markdown_preview);
        assert_eq!(shown(&editor), ["notes.md"]);
        std::fs::remove_dir_all(dir).ok();
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
        lisp.eval(&format!(r#"function core.lsp_servers() return {{ {{ "rust", "python3", "{}" }} }} end"#, server.display())).unwrap();
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
    fn the_built_in_core_matches_lua_core() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/core");
        assert_eq!(lua_core_files(Some(&dir)), built_in_core(), "add new lua/core files to LUA_CORE_FILES in lisp.rs");
    }

    /// Every vmac.x / core.x the core calls exists (a typo would only show
    /// when that command runs).
    #[test]
    fn the_core_calls_only_functions_that_exist() {
        let (mut lisp, _editor) = lisp_with("");
        let re = regex::Regex::new(r"\b(V|vmac|core)\.([a-z_][a-z0-9_]*)\s*\(").unwrap();
        let mut missing = Vec::new();
        for (name, code) in built_in_core() {
            // Set while it runs: the operator waiting for its motion.
            for c in re.captures_iter(&code).filter(|c| &c[2] != "pending_operator") {
                let table = if &c[1] == "core" { "core" } else { "vmac" };
                let check = format!("type({table}.{})", &c[2]);
                if lisp.eval(&check).unwrap() != "function" {
                    missing.push(format!("{name}: {table}.{}", &c[2]));
                }
            }
        }
        missing.dedup();
        assert!(missing.is_empty(), "{missing:#?}");
    }

    #[test]
    fn plugins_using_the_old_name_still_work() {
        let (mut lisp, editor) = lisp_with("");
        lisp.eval_lua(r#"gmacs.command("old", function() gmacs.insert("still works") end)"#).unwrap();
        typed(&mut lisp, " :old\n");
        assert_eq!(text(&editor), "still works");
    }

    #[test]
    fn lua_names_of_primitives() {
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
    fn lua_commands_and_keys() {
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
        // A key runs a command by name.
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
    fn init_lua_loads_after_the_core_and_reloads() {
        let dir = std::env::temp_dir().join(format!("vmac-init-lua-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let init = dir.join("init.lua");
        std::fs::write(&init, r#"vmac.map("normal", "Q", function() vmac.insert("one") end)"#).unwrap();
        let editor = Editor::new(Buffer::new("t"));
        let mut lisp = Lisp::with_lua(editor.clone(), vec![init.clone()]);
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
        { let m = editor.lock().message.clone(); assert!(m.starts_with("init.lua:") && m.contains("kept the last version"), "{m}"); }
        typed(&mut lisp, "0Q");
        assert_eq!(text(&editor), "twotwoone", "the last init.lua that loaded is kept");
        typed(&mut lisp, "ix");
        assert!(text(&editor).contains('x'), "the core still works when init.lua is broken");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_example_init_lua_works() {
        let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/init.lua");
        let editor = Editor::new(Buffer::new("notes"));
        let mut lisp = Lisp::with_lua(editor.clone(), vec![example]);
        assert_eq!(editor.lock().message, "loaded core, init.lua");
        typed(&mut lisp, " it");
        assert_eq!(text(&editor), "TODO: ");
        assert_eq!(lisp.cursor_shape(), "bar", "the command it ran put it in insert state");
        lisp.dispatch_key("ESC", "");
        typed(&mut lisp, "0 id");
        assert_eq!(text(&editor).len(), "TODO: ".len() + "2026-01-01".len());
        let modeline = lisp.modeline();
        assert_eq!(modeline[0].text, "notes ●");
    }

    /// A plugins folder with: `zen`, `wc` (requiring a module of its own),
    /// `broken`, `off` (disabled), and `stamp.lua`.
    fn plugin_setup(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("vmac-plugin-load-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("plugins");
        let write = |rel: &str, text: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("zen/init.lua", r#"vmac.command("zen-insert", function() vmac.insert("zen") end) vmac.map("normal", "Z", "zen-insert")"#);
        write("wc/init.lua", r#"local util = require("wc_util")
vmac.command("word-count", function() vmac.message(util.count(vmac.buffer_string()) .. " words") end)"#);
        write("wc/lua/wc_util.lua", "return { count = function(s) local n = 0 for _ in s:gmatch('%S+') do n = n + 1 end return n end }");
        write("broken/init.lua", "vmac.no_such_function()");
        write("off/init.lua", r#"vmac.command("off-insert", function() vmac.insert("off") end)"#);
        write("off/disabled", "");
        write("stamp.lua", r#"vmac.command("stamp", function() vmac.insert("stamped") end)"#);
        (base, dir)
    }

    fn with_plugins(dir: &Path, init: Option<&Path>) -> (Lisp, Editor) {
        let editor = Editor::new(Buffer::new("t"));
        let lisp = Lisp::load(editor.clone(), None, init.into_iter().map(Path::to_path_buf).collect(), Some(dir.to_path_buf()));
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
        assert_eq!(text(&editor), "zen", "a plugin's key");
        typed(&mut lisp, " :stamp\n");
        assert!(text(&editor).contains("stamped"), "a single-file Lua plugin");
        typed(&mut lisp, " :word-count\n");
        assert_eq!(editor.lock().message, "1 words", "a Lua plugin requiring its own module");
        typed(&mut lisp, " :off-insert\n");
        assert_eq!(editor.lock().message, "off-insert is not a command", "disabled plugins don't load");
        typed(&mut lisp, "0x");
        assert_eq!(text(&editor).chars().count(), "zenstamped".len() - 1, "the core still works");

        let list = lisp.eval("table.concat(vmac.plugin_list(), '\\n')").unwrap();
        assert!(list.contains("broken  (lua)  failed:"), "{list}");
        assert!(list.contains("off  (lua)  off") && list.contains("wc  (lua)  loaded"), "{list}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_users_init_loads_after_plugins() {
        let (base, dir) = plugin_setup("order");
        std::fs::remove_dir_all(dir.join("broken")).unwrap();
        let init = base.join("init.lua");
        std::fs::write(&init, r#"vmac.map("normal", "Z", function() vmac.insert("mine") end)"#).unwrap();
        let (mut lisp, editor) = with_plugins(&dir, Some(&init));
        assert_eq!(editor.lock().message, "loaded core, 3 plugins, init.lua");
        typed(&mut lisp, "Z");
        assert_eq!(text(&editor), "mine", "init.lua overrides the plugin's key");
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

        lisp.eval(r#"vmac.plugin_toggle("zen")"#).unwrap();
        assert!(lisp.reload_if_changed());
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("zen/init.lua"), r#"vmac.command("zen-insert", function() vmac.insert("zen 2") end)"#).unwrap();
        assert!(lisp.reload_if_changed(), "editing a plugin reloads it");
        typed(&mut lisp, " :zen-insert\n");
        assert_eq!(text(&editor), "zen 2");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_plugin_fixed_on_disk_comes_back() {
        let (base, dir) = plugin_setup("fix");
        let (mut lisp, editor) = with_plugins(&dir, None);
        std::fs::write(dir.join("broken/init.lua"), r#"vmac.command("fixed", function() vmac.insert("fixed") end)"#).unwrap();
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
        assert_eq!(lisp.eval("vmac.font()").unwrap(), "Menlo");
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
        assert_eq!(lisp.eval("vmac.theme()").unwrap(), "system");
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
    fn modeline_comes_from_lua() {
        let (mut lisp, _editor) = lisp_with("");
        let segs = lisp.modeline();
        assert!(segs.iter().any(|s| s.text.contains("test")), "{segs:?}");
        lisp.eval(r#"function core.modeline() return { { "custom", "accent" } } end"#).unwrap();
        assert_eq!(
            lisp.modeline(),
            vec![Segment { text: "custom".into(), face: "accent".into() }]
        );
    }
}
