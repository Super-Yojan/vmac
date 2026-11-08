//! The editor's state, shared between the Steel primitives and the GPUI view:
//! the buffers, the windows showing them and how the windows are split.
//!
//! Steel requires native functions to be `Send + Sync`, so the state lives
//! behind `Arc<Mutex<_>>`. Everything runs on GPUI's main thread, so the lock
//! is never contended; it is only there to satisfy the bounds.

use crate::buffer::Buffer;
use crate::lsp::{CompletionItem, Diagnostic, Doc, Location, Lsp, Reply, RequestKind, ServerConfig, WorkspaceEdit};
use serde_json::{Value, json};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use crate::picker::Picker;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Compiles a search pattern: a regex, case-insensitive unless it has an
/// uppercase letter (smartcase). A pattern that isn't a valid regex is
/// searched for literally.
pub fn compile_search(pattern: &str) -> Option<Regex> {
    if pattern.is_empty() {
        return None;
    }
    let case = if pattern.chars().any(char::is_uppercase) { "" } else { "(?i)" };
    Regex::new(&format!("{case}{pattern}"))
        .or_else(|_| Regex::new(&format!("{case}{}", regex::escape(pattern))))
        .ok()
}

pub type BufferId = u64;
pub type WindowId = u64;

/// Where a terminal's program runs: a local PTY; a plugin's backend
/// polled through Lua (send/read) with its handle; or a plugin's terminal
/// shown through a local PTY running its attach command (`hc attach t7`),
/// which carries keys, screen and size itself.
pub enum TermBackend {
    Local(crate::terminal::LocalPty),
    Plugin { backend: String, handle: String },
    Attached { backend: String, handle: String, pty: crate::terminal::LocalPty },
}

impl std::fmt::Debug for TermBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TermBackend::Local(_) => write!(f, "local"),
            TermBackend::Plugin { backend, handle } => write!(f, "{backend}:{handle}"),
            TermBackend::Attached { backend, handle, .. } => write!(f, "{backend}:{handle} (attached)"),
        }
    }
}

/// A terminal, shown in its own read-only buffer.
#[derive(Debug)]
pub struct Terminal {
    pub buffer: BufferId,
    pub backend: TermBackend,
    pub exited: bool,
}

impl Terminal {
    pub fn backend_name(&self) -> String {
        match &self.backend {
            TermBackend::Local(_) => "local".into(),
            TermBackend::Plugin { backend, .. } | TermBackend::Attached { backend, .. } => backend.clone(),
        }
    }
}

/// The completion popup: the server's candidates, those matching what's
/// been typed since `start`, and the selected one.
#[derive(Debug, Clone)]
pub struct Completion {
    all: Vec<CompletionItem>,
    /// Indices into `all`, best match first.
    shown: Vec<usize>,
    selected: usize,
    /// Where the word being completed starts.
    start: usize,
}

impl Completion {
    pub fn items(&self) -> impl Iterator<Item = &CompletionItem> {
        self.shown.iter().map(|&i| &self.all[i])
    }
    pub fn selected(&self) -> usize {
        self.selected
    }
    pub fn len(&self) -> usize {
        self.shown.len()
    }

    /// Keeps the candidates matching `prefix`, best first (all, in the
    /// server's order, for an empty prefix).
    fn filter(&mut self, prefix: &str) {
        self.selected = 0;
        if prefix.is_empty() {
            self.shown = (0..self.all.len()).collect();
            return;
        }
        let matcher = SkimMatcherV2::default().smart_case();
        let mut scored: Vec<(i64, usize)> = self
            .all
            .iter()
            .enumerate()
            .filter_map(|(i, item)| matcher.fuzzy_match(&item.filter, prefix).map(|score| (score, i)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.shown = scored.into_iter().map(|(_, i)| i).collect();
    }
}

/// Monospace Nerd Fonts, most wanted first: they carry the icons programs
/// like eza and starship print.
const NERD_FONTS: &[&str] = &[
    "JetBrainsMono NFM",
    "JetBrainsMono Nerd Font Mono",
    "FiraCode Nerd Font Mono",
    "Hack Nerd Font Mono",
    "Iosevka NFM",
    "Iosevka Nerd Font Mono",
    "MesloLGS NF",
    "CaskaydiaCove Nerd Font Mono",
];

/// The font to start with: the first installed Nerd Font from the list,
/// else any installed monospace Nerd Font, else none (the theme's font).
pub fn default_font(installed: &[String]) -> Option<String> {
    NERD_FONTS
        .iter()
        .find(|f| installed.iter().any(|i| i == *f))
        .map(|f| f.to_string())
        .or_else(|| installed.iter().find(|f| f.ends_with("Nerd Font Mono") || f.ends_with(" NFM")).cloned())
}

/// fish if it's installed (on PATH or where Homebrew puts it), else $SHELL.
pub fn default_shell() -> String {
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join("fish"));
    let known = ["/opt/homebrew/bin/fish", "/usr/local/bin/fish", "/usr/bin/fish"].map(PathBuf::from);
    on_path
        .chain(known)
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/sh".into())
}

/// A window's text size (rows, columns) for a terminal: 24x80 before it is
/// laid out, and never under 2x10.
fn window_size(w: &Window) -> (u16, u16) {
    let rows = if w.rows == 0 { 24 } else { w.rows.max(2) };
    let cols = if w.cols == 0 { 80 } else { w.cols.max(10) };
    (rows as u16, cols as u16)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A view onto a buffer. Like Emacs, each window has its own point: the
/// selected window's point lives in its buffer while it is selected, and is
/// saved back here when another window is selected.
#[derive(Debug, Clone)]
pub struct Window {
    pub id: WindowId,
    pub buffer: BufferId,
    pub point: usize,
    /// First buffer line on screen.
    pub scroll_top: usize,
    /// Text rows on screen, set by the view when it lays the window out.
    pub rows: usize,
    /// Text columns on screen (monospace), set by the view too.
    pub cols: usize,
}

/// How windows divide the space: side by side (`Row`) or stacked (`Column`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Axis {
    Row,
    Column,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Layout {
    Leaf(WindowId),
    Split(Axis, Vec<Layout>),
}

/// A window's place, as fractions of the editing area.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Layout {
    fn leaves(&self, out: &mut Vec<WindowId>) {
        match self {
            Layout::Leaf(id) => out.push(*id),
            Layout::Split(_, children) => children.iter().for_each(|c| c.leaves(out)),
        }
    }

    fn rects(&self, r: Rect, out: &mut Vec<(WindowId, Rect)>) {
        match self {
            Layout::Leaf(id) => out.push((*id, r)),
            Layout::Split(axis, children) => {
                let n = children.len() as f32;
                for (i, c) in children.iter().enumerate() {
                    let i = i as f32;
                    let part = match axis {
                        Axis::Row => Rect { x: r.x + r.w * i / n, w: r.w / n, ..r },
                        Axis::Column => Rect { y: r.y + r.h * i / n, h: r.h / n, ..r },
                    };
                    c.rects(part, out);
                }
            }
        }
    }

    /// Puts `new` beside `target`, after it along `axis`.
    fn split(&mut self, target: WindowId, new: WindowId, axis: Axis) -> bool {
        match self {
            Layout::Leaf(id) if *id == target => {
                *self = Layout::Split(axis, vec![Layout::Leaf(target), Layout::Leaf(new)]);
                true
            }
            Layout::Leaf(_) => false,
            Layout::Split(a, children) => {
                if *a == axis
                    && let Some(i) = children.iter().position(|c| *c == Layout::Leaf(target))
                {
                    children.insert(i + 1, Layout::Leaf(new));
                    return true;
                }
                children.iter_mut().any(|c| c.split(target, new, axis))
            }
        }
    }

    /// Removes a window; a split left with one child becomes that child.
    fn remove(&mut self, target: WindowId) {
        if let Layout::Split(_, children) = self {
            children.retain(|c| *c != Layout::Leaf(target));
            children.iter_mut().for_each(|c| c.remove(target));
            if children.len() == 1 {
                *self = children.remove(0);
            }
        }
    }
}

#[derive(Debug)]
pub struct EditorState {
    buffers: Vec<(BufferId, Buffer)>,
    windows: Vec<Window>,
    layout: Layout,
    selected: WindowId,
    next_id: u64,
    /// A window fixed at the left edge, outside the split layout (the file tree).
    sidebar: Option<WindowId>,
    /// The sidebar's width as a share of the editor's, set by the view.
    pub sidebar_width: f32,
    /// The last selected window in the layout, where files open from the sidebar.
    last_main: WindowId,
    pub minibuffer: Buffer,
    pub minibuffer_prompt: Option<String>,
    pub message: String,
    pub kill_ring: Vec<String>,
    /// Scheme source queued by `eval-string` (the primitive can't re-enter the
    /// engine that is calling it), run by the host after the current command.
    pub pending_eval: Vec<String>,
    /// Scheme queued by Lua (vmac.scheme): run silently after the current
    /// command, or after init.lua loads.
    pub pending_scheme: Vec<String>,
    /// Where plugins live, the plugins found at the last load and how each
    /// went, and whether something (an install) asked for a reload.
    pub plugin_dir: Option<PathBuf>,
    pub plugins: Vec<(crate::plugins::Plugin, crate::plugins::Status)>,
    pub reload_requested: bool,
    pub quit_requested: bool,
    /// The open fuzzy picker, if any; its query is the minibuffer's text.
    pub picker: Option<Picker>,
    /// Language servers and what they report.
    pub lsp: Lsp,
    /// Documentation shown by K, until the next key.
    pub hover: Option<String>,
    /// Where gd and other jumps came from, for C-o: (buffer, point).
    jumps: Vec<(BufferId, usize)>,
    /// The completion popup while typing, if open.
    pub completion: Option<Completion>,
    /// Running terminals, and the backend new ones use ("local", or a
    /// plugin's). Plugin terminals to end, for the Lisp side (it runs Lua).
    pub terminals: Vec<Terminal>,
    pub terminal_backend: String,
    /// The shell a terminal runs when given no command: fish when it's
    /// installed, else the login shell ($SHELL).
    pub terminal_shell: String,
    /// The editor's font (None: the theme's monospace font), and the font
    /// families installed, which the view fills in.
    pub font: Option<String>,
    pub fonts: Vec<String>,
    /// "system" (follow light or dark mode as the system does), "light" or "dark".
    pub theme_mode: String,
    pub terminals_to_kill: Vec<(String, String)>,
    /// The coding agent (ACP), how to start it, its buffer, and the
    /// conversation revision that buffer shows.
    pub agent: Option<crate::agent::Agent>,
    pub agent_config: crate::agent::AgentConfig,
    agent_buffer: Option<BufferId>,
    agent_shown: u64,
    agent_shown_width: usize,
    /// Permissions already put to the person (so each is asked once).
    agent_asked: usize,
    /// What each line of the agent buffer points at.
    agent_lines: Vec<crate::agent::Target>,
    /// The code actions SPC c a last offered, as the server sent them.
    code_actions: Vec<Value>,
    /// Things for Scheme to handle (a function name and its arguments),
    /// raised where Rust can't call Scheme (server replies).
    events: Vec<(String, Vec<String>)>,
    search_cache: Option<(String, Regex)>,
}

impl EditorState {
    pub fn new(buffer: Buffer) -> Self {
        let window = Window { id: 2, buffer: 1, point: buffer.point(), scroll_top: 0, rows: 0, cols: 0 };
        Self {
            buffers: vec![(1, buffer)],
            windows: vec![window],
            layout: Layout::Leaf(2),
            selected: 2,
            next_id: 3,
            sidebar: None,
            sidebar_width: 0.22,
            last_main: 2,
            minibuffer: Buffer::new("*minibuffer*"),
            minibuffer_prompt: None,
            message: String::new(),
            kill_ring: Vec::new(),
            pending_eval: Vec::new(),
            pending_scheme: Vec::new(),
            plugin_dir: None,
            plugins: Vec::new(),
            reload_requested: false,
            quit_requested: false,
            picker: None,
            lsp: Lsp::default(),
            hover: None,
            jumps: Vec::new(),
            completion: None,
            code_actions: Vec::new(),
            terminals: Vec::new(),
            terminal_backend: "local".into(),
            terminal_shell: default_shell(),
            font: None,
            fonts: Vec::new(),
            theme_mode: "system".into(),
            terminals_to_kill: Vec::new(),
            agent: None,
            agent_config: crate::agent::AgentConfig::claude(),
            agent_buffer: None,
            agent_shown: 0,
            agent_shown_width: 0,
            agent_asked: 0,
            agent_lines: Vec::new(),
            events: Vec::new(),
            search_cache: None,
        }
    }

    fn fresh_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    // ---- Buffers

    /// The selected window's buffer.
    pub fn buffer(&self) -> &Buffer {
        self.buffer_by_id(self.window().buffer).expect("a window shows a live buffer")
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        let id = self.window().buffer;
        &mut self.buffers.iter_mut().find(|(b, _)| *b == id).expect("a window shows a live buffer").1
    }

    pub fn buffer_by_id(&self, id: BufferId) -> Option<&Buffer> {
        self.buffers.iter().find(|(b, _)| *b == id).map(|(_, buf)| buf)
    }

    /// The buffer that editing commands act on: the minibuffer while it is open.
    pub fn current(&mut self) -> &mut Buffer {
        if self.minibuffer_prompt.is_some() {
            &mut self.minibuffer
        } else {
            self.buffer_mut()
        }
    }

    pub fn buffer_id_named(&self, name: &str) -> Option<BufferId> {
        self.buffers.iter().find(|(_, b)| b.name() == name).map(|(id, _)| *id)
    }

    pub fn buffer_named_mut(&mut self, name: &str) -> Option<&mut Buffer> {
        self.buffers.iter_mut().find(|(_, b)| b.name() == name).map(|(_, b)| b)
    }

    /// The buffer with this name, made (empty) if there isn't one.
    pub fn ensure_buffer(&mut self, name: &str) -> BufferId {
        match self.buffer_id_named(name) {
            Some(id) => id,
            None => self.add_buffer(Buffer::new(name)),
        }
    }

    pub fn buffer_names(&self) -> Vec<String> {
        self.buffers.iter().map(|(_, b)| b.name().to_string()).collect()
    }

    /// Adds a buffer, renaming it `name<2>`, `name<3>`... if the name is taken.
    pub fn add_buffer(&mut self, mut buffer: Buffer) -> BufferId {
        let base = buffer.name().to_string();
        let mut n = 2;
        while self.buffers.iter().any(|(_, b)| b.name() == buffer.name()) {
            buffer.set_name(format!("{base}<{n}>"));
            n += 1;
        }
        let id = self.fresh_id();
        self.buffers.push((id, buffer));
        id
    }

    /// Shows a buffer in the selected window, at the buffer's own point.
    pub fn show_buffer(&mut self, id: BufferId) {
        if self.buffer_by_id(id).is_none() || self.window().buffer == id {
            return;
        }
        let point = self.buffer().point();
        self.window_mut().point = point;
        let w = self.window_mut();
        w.buffer = id;
        w.scroll_top = 0;
        let point = self.buffer().point();
        self.window_mut().point = point;
    }

    /// Opens a file in the selected window: its buffer if it is open already,
    /// else a new one.
    pub fn open_file(&mut self, path: &Path) -> std::io::Result<()> {
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let want = canonical(path);
        let existing = self
            .buffers
            .iter()
            .find(|(_, b)| b.file().is_some_and(|f| canonical(f) == want))
            .map(|(id, _)| *id);
        let id = match existing {
            Some(id) => id,
            None => self.add_buffer(Buffer::from_file(PathBuf::from(path))?),
        };
        self.show_buffer(id);
        Ok(())
    }

    pub fn switch_to_buffer(&mut self, name: &str) -> bool {
        match self.buffers.iter().find(|(_, b)| b.name() == name).map(|(id, _)| *id) {
            Some(id) => {
                self.show_buffer(id);
                true
            }
            None => false,
        }
    }

    /// Shows the buffer `n` places after this one in the buffer list.
    pub fn next_buffer(&mut self, n: isize) {
        let len = self.buffers.len() as isize;
        let here = self.buffers.iter().position(|(id, _)| *id == self.window().buffer).unwrap_or(0);
        let id = self.buffers[(here as isize + n).rem_euclid(len) as usize].0;
        self.show_buffer(id);
    }

    /// Closes the selected window's buffer. Windows showing it show another
    /// buffer (a new *scratch* when it was the last). Refuses a modified
    /// buffer unless `force`.
    pub fn kill_buffer(&mut self, force: bool) -> Result<String, String> {
        let id = self.window().buffer;
        let name = self.buffer().name().to_string();
        if self.buffer().is_modified() && !force {
            return Err(format!("{name} has unsaved changes (:bd! to discard)"));
        }
        self.buffers.retain(|(b, _)| *b != id);
        // A terminal's program ends with its buffer.
        if let Some(i) = self.terminals.iter().position(|t| t.buffer == id) {
            match self.terminals.remove(i).backend {
                TermBackend::Local(mut pty) => pty.kill(),
                TermBackend::Plugin { backend, handle } => self.terminals_to_kill.push((backend, handle)),
                TermBackend::Attached { backend, handle, mut pty } => {
                    pty.kill();
                    self.terminals_to_kill.push((backend, handle));
                }
            }
        }
        if self.buffers.is_empty() {
            self.add_buffer(Buffer::new("*scratch*"));
        }
        let fallback = self.buffers[0].0;
        let point = self.buffer_by_id(fallback).map(Buffer::point).unwrap_or(0);
        for w in self.windows.iter_mut().filter(|w| w.buffer == id) {
            w.buffer = fallback;
            w.point = point;
            w.scroll_top = 0;
        }
        Ok(name)
    }

    // ---- Windows

    pub fn window(&self) -> &Window {
        self.window_by_id(self.selected).expect("the selected window exists")
    }

    pub fn window_mut(&mut self) -> &mut Window {
        let id = self.selected;
        self.windows.iter_mut().find(|w| w.id == id).expect("the selected window exists")
    }

    pub fn window_by_id(&self, id: WindowId) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn window_by_id_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    pub fn selected_window(&self) -> WindowId {
        self.selected
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// The point a window shows: the buffer's own for the selected window.
    pub fn window_point(&self, id: WindowId) -> usize {
        if id == self.selected {
            return self.buffer().point();
        }
        let w = self.window_by_id(id).expect("window exists");
        let len = self.buffer_by_id(w.buffer).map(Buffer::len).unwrap_or(0);
        w.point.min(len)
    }

    pub fn select_window(&mut self, id: WindowId) {
        if id == self.selected || self.window_by_id(id).is_none() {
            return;
        }
        if Some(id) != self.sidebar {
            self.last_main = id;
        }
        let point = self.buffer().point();
        self.window_mut().point = point;
        self.selected = id;
        let point = self.window().point;
        self.buffer_mut().goto_char(point);
    }

    // ---- The sidebar

    pub fn sidebar(&self) -> Option<WindowId> {
        self.sidebar
    }

    pub fn is_sidebar(&self, id: WindowId) -> bool {
        self.sidebar == Some(id)
    }

    /// Shows a buffer in the sidebar (opening it if needed) and selects it.
    pub fn open_sidebar(&mut self, buffer: BufferId) {
        let id = match self.sidebar {
            Some(id) => id,
            None => {
                let id = self.fresh_id();
                self.windows.push(Window { id, buffer, point: 0, scroll_top: 0, rows: 0, cols: 0 });
                self.sidebar = Some(id);
                id
            }
        };
        if self.selected == id {
            self.show_buffer(buffer);
        } else {
            if let Some(w) = self.window_by_id_mut(id) {
                w.buffer = buffer;
            }
            self.select_window(id);
        }
    }

    pub fn close_sidebar(&mut self) {
        let Some(id) = self.sidebar else { return };
        if self.selected == id {
            self.select_main_window();
        }
        self.windows.retain(|w| w.id != id);
        self.sidebar = None;
    }

    /// Selects the layout window last used (when the sidebar is selected).
    pub fn select_main_window(&mut self) {
        let target = if self.window_by_id(self.last_main).is_some() {
            self.last_main
        } else {
            self.window_order().into_iter().find(|&w| !self.is_sidebar(w)).unwrap_or(self.selected)
        };
        self.select_window(target);
    }

    /// Splits the selected window: the new one, on the same buffer and line,
    /// goes right of it (`Row`) or below it (`Column`) and is selected.
    pub fn split_window(&mut self, axis: Axis) {
        if self.is_sidebar(self.selected) {
            self.select_main_window();
        }
        let id = self.fresh_id();
        let mut new = self.window().clone();
        new.id = id;
        new.point = self.buffer().point();
        self.windows.push(new);
        let target = self.selected;
        self.layout.split(target, id, axis);
        self.select_window(id);
    }

    /// Closes the selected window. The last one in the layout can't be closed.
    pub fn close_window(&mut self) -> bool {
        if self.is_sidebar(self.selected) {
            self.close_sidebar();
            return true;
        }
        if self.window_order().iter().filter(|&&w| !self.is_sidebar(w)).count() < 2 {
            return false;
        }
        let order = self.window_order();
        let i = order.iter().position(|&w| w == self.selected).unwrap_or(0);
        let next = if i > 0 { order[i - 1] } else { order[1] };
        let old = self.selected;
        self.select_window(next);
        self.windows.retain(|w| w.id != old);
        self.layout.remove(old);
        true
    }

    /// Closes every window but the selected one (the sidebar too).
    pub fn only_window(&mut self) {
        if self.is_sidebar(self.selected) {
            self.select_main_window();
        }
        self.sidebar = None;
        let keep = self.selected;
        self.windows.retain(|w| w.id == keep);
        self.layout = Layout::Leaf(keep);
    }

    /// Windows in order: the sidebar, then the layout left to right, top to bottom.
    pub fn window_order(&self) -> Vec<WindowId> {
        let mut out: Vec<WindowId> = self.sidebar.into_iter().collect();
        self.layout.leaves(&mut out);
        out
    }

    pub fn other_window(&mut self, n: isize) {
        let order = self.window_order();
        let i = order.iter().position(|&w| w == self.selected).unwrap_or(0) as isize;
        let next = order[(i + n).rem_euclid(order.len() as isize) as usize];
        self.select_window(next);
    }

    /// Where each window goes: the sidebar at the left edge, the layout in
    /// the rest.
    pub fn window_rects(&self) -> Vec<(WindowId, Rect)> {
        let mut out = Vec::new();
        let left = match self.sidebar {
            Some(id) => {
                out.push((id, Rect { x: 0., y: 0., w: self.sidebar_width, h: 1. }));
                self.sidebar_width
            }
            None => 0.,
        };
        self.layout.rects(Rect { x: left, y: 0., w: 1. - left, h: 1. }, &mut out);
        out
    }

    /// The window next to the selected one in a direction ("left", "right",
    /// "up", "down"): the one touching that edge that overlaps it most.
    pub fn window_in_direction(&self, dir: &str) -> Option<WindowId> {
        const EPS: f32 = 1e-4;
        let rects = self.window_rects();
        let (_, me) = *rects.iter().find(|(id, _)| *id == self.selected)?;
        let overlap = |a0: f32, a1: f32, b0: f32, b1: f32| (a1.min(b1) - a0.max(b0)).max(0.);
        rects
            .iter()
            .filter(|(id, _)| *id != self.selected)
            .filter_map(|(id, r)| {
                let (touches, shared) = match dir {
                    "left" => ((r.x + r.w - me.x).abs() < EPS, overlap(r.y, r.y + r.h, me.y, me.y + me.h)),
                    "right" => ((me.x + me.w - r.x).abs() < EPS, overlap(r.y, r.y + r.h, me.y, me.y + me.h)),
                    "up" => ((r.y + r.h - me.y).abs() < EPS, overlap(r.x, r.x + r.w, me.x, me.x + me.w)),
                    "down" => ((me.y + me.h - r.y).abs() < EPS, overlap(r.x, r.x + r.w, me.x, me.x + me.w)),
                    _ => (false, 0.),
                };
                (touches && shared > EPS).then_some((*id, shared))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    }

    /// Scrolls the selected window by `n` lines, keeping it inside the buffer.
    pub fn scroll(&mut self, n: isize) {
        let last = self.buffer().line_count().saturating_sub(1) as isize;
        let w = self.window_mut();
        w.scroll_top = (w.scroll_top as isize + n).clamp(0, last) as usize;
    }

    /// Scrolls so the cursor's line is at the top, center or bottom.
    pub fn recenter(&mut self, place: &str) {
        let line = self.buffer().line();
        let w = self.window_mut();
        let rows = w.rows.max(1);
        w.scroll_top = match place {
            "top" => line,
            "bottom" => line.saturating_sub(rows - 1),
            _ => line.saturating_sub(rows / 2),
        };
    }

    /// Records a window's size and scrolls it so its cursor is on screen.
    pub fn fit_window(&mut self, id: WindowId, rows: usize) {
        if self.window_by_id(id).is_none() {
            return;
        }
        let point = self.window_point(id);
        let buffer = self.window_by_id(id).map(|w| w.buffer).unwrap_or_default();
        let line = self.buffer_by_id(buffer).map(|b| b.line_of(point)).unwrap_or(0);
        let w = self.window_by_id_mut(id).expect("window exists");
        w.rows = rows;
        if line < w.scroll_top {
            w.scroll_top = line;
        } else if rows > 0 && line >= w.scroll_top + rows {
            w.scroll_top = line + 1 - rows;
        }
    }

    // ---- Language servers

    /// Syncs open files with their language servers and reads what came
    /// back. Returns whether anything visible changed.
    pub fn lsp_pump(&mut self, configs: Vec<ServerConfig>) -> bool {
        self.lsp.set_configs(configs);
        let mut changed = self.lsp_sync();
        for reply in self.lsp.take_replies() {
            self.act_on(reply);
            changed = true;
        }
        changed
    }

    /// Sends servers the files' latest text and reads what they sent.
    fn lsp_sync(&mut self) -> bool {
        let docs: Vec<Doc> = self
            .buffers
            .iter()
            .filter_map(|(_, b)| {
                let path = b.file()?.clone();
                let language = crate::syntax::language_for_path(&path)?;
                Some(Doc { path, language, revision: b.revision(), saves: b.saves(), text: b.text().clone() })
            })
            .collect();
        let mut changed = self.lsp.pump(&docs, crate::picker::project_root);
        if let Some(notice) = self.lsp.take_notice() {
            self.message = notice;
            changed = true;
        }
        changed
    }

    /// Asks the selected buffer's server about the point (gd, K, gr). The
    /// answer comes back later, through `lsp_pump`.
    pub fn lsp_request(&mut self, kind: RequestKind) {
        let Some(path) = self.buffer().file().cloned() else {
            self.message = "This buffer has no file".into();
            return;
        };
        self.lsp_sync(); // so the server answers about the text as it is now
        let position = crate::lsp::char_to_utf16(self.buffer().text(), self.buffer().point());
        if let Err(e) = self.lsp.request(kind, &path, position) {
            self.message = e;
        }
    }

    /// Asks the server to rename what's under the point everywhere.
    pub fn lsp_rename(&mut self, new_name: &str) {
        let Some(path) = self.buffer().file().cloned() else {
            self.message = "This buffer has no file".into();
            return;
        };
        self.lsp_sync();
        let (line, character) = crate::lsp::char_to_utf16(self.buffer().text(), self.buffer().point());
        let params = json!({ "textDocument": { "uri": crate::lsp::path_to_uri(&path) },
                             "position": { "line": line, "character": character }, "newName": new_name });
        if let Err(e) = self.lsp.send(&path, RequestKind::Rename, "textDocument/rename", params) {
            self.message = e;
        }
    }

    /// Asks the server to format the file, or just `range` (a selection).
    pub fn lsp_format(&mut self, range: Option<(usize, usize)>) {
        let Some(path) = self.buffer().file().cloned() else {
            self.message = "This buffer has no file".into();
            return;
        };
        self.lsp_sync();
        let options = json!({ "tabSize": 4, "insertSpaces": true, "trimTrailingWhitespace": true });
        let mut params = json!({ "textDocument": { "uri": crate::lsp::path_to_uri(&path) }, "options": options });
        let method = match range {
            Some((start, end)) => {
                let rope = self.buffer().text();
                let (a, b) = (crate::lsp::char_to_utf16(rope, start), crate::lsp::char_to_utf16(rope, end));
                params["range"] = json!({ "start": { "line": a.0, "character": a.1 },
                                          "end": { "line": b.0, "character": b.1 } });
                "textDocument/rangeFormatting"
            }
            None => "textDocument/formatting",
        };
        if let Err(e) = self.lsp.send(&path, RequestKind::Format, method, params) {
            self.message = e;
        }
    }

    /// Asks for code actions for `start..end` (the cursor, or a selection),
    /// sending the diagnostics there so the server can offer fixes.
    pub fn lsp_code_actions(&mut self, start: usize, end: usize) {
        let Some(path) = self.buffer().file().cloned() else {
            self.message = "This buffer has no file".into();
            return;
        };
        self.lsp_sync();
        let rope = self.buffer().text();
        let (a, b) = (crate::lsp::char_to_utf16(rope, start), crate::lsp::char_to_utf16(rope, end));
        let line_of = |p: usize| rope.char_to_line(p.min(rope.len_chars()));
        let (first, last) = (line_of(start), line_of(end));
        let diagnostics: Vec<Value> = self
            .lsp
            .diagnostics(&path)
            .iter()
            .filter(|d| d.start.0 <= last && d.end.0 >= first)
            .map(|d| d.raw.clone())
            .collect();
        let params = json!({
            "textDocument": { "uri": crate::lsp::path_to_uri(&path) },
            "range": { "start": { "line": a.0, "character": a.1 }, "end": { "line": b.0, "character": b.1 } },
            "context": { "diagnostics": diagnostics, "triggerKind": 1 } });
        if let Err(e) = self.lsp.send(&path, RequestKind::CodeActions, "textDocument/codeAction", params) {
            self.message = e;
        }
    }

    /// Carries out the code action at `index` of the last offered: applies
    /// its edit, runs its command, or first asks the server for its edit.
    pub fn apply_code_action(&mut self, index: usize) {
        let Some(action) = self.code_actions.get(index).cloned() else { return };
        let Some(path) = self.buffer().file().cloned() else { return };
        let has_edit = action["edit"].is_object();
        // A bare Command has "command" as a string; a CodeAction may carry one as an object.
        let command = if action["command"].is_string() { Some(action.clone()) } else { action.get("command").cloned() };
        if !has_edit && command.is_none() && action.get("data").is_some() {
            let _ = self.lsp.send(&path, RequestKind::ResolveAction, "codeAction/resolve", action);
            return;
        }
        if has_edit {
            let edit = crate::lsp::parse_workspace_edit(&action["edit"]);
            self.apply_workspace_edit(&edit);
        }
        if let Some(command) = command.filter(|c| c["command"].is_string()) {
            let params = json!({ "command": command["command"], "arguments": command["arguments"].clone() });
            let _ = self.lsp.send(&path, RequestKind::ExecuteCommand, "workspace/executeCommand", params);
        }
    }

    /// The buffer for a file, loading it (without showing it) if needed.
    fn file_buffer(&mut self, path: &Path) -> Option<BufferId> {
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let want = canonical(path);
        if let Some((id, _)) = self.buffers.iter().find(|(_, b)| b.file().is_some_and(|f| canonical(f) == want)) {
            return Some(*id);
        }
        Buffer::from_file(path.to_path_buf()).ok().map(|b| self.add_buffer(b))
    }

    /// Makes edits across files: each file's edits are one undo step, applied
    /// from the end back so positions stay valid. Edited files stay unsaved.
    pub fn apply_workspace_edit(&mut self, edit: &WorkspaceEdit) {
        let mut files = 0;
        let mut count = 0;
        for (path, edits) in edit {
            if edits.is_empty() {
                continue;
            }
            let Some(id) = self.file_buffer(path) else {
                self.message = format!("couldn't open {}", path.display());
                continue;
            };
            let Some((_, buffer)) = self.buffers.iter_mut().find(|(b, _)| *b == id) else { continue };
            buffer.undo_boundary();
            let rope = buffer.text().clone();
            let mut ranges: Vec<(usize, usize, &str)> = edits
                .iter()
                .map(|e| {
                    let a = crate::lsp::utf16_to_char(&rope, e.start);
                    (a, crate::lsp::utf16_to_char(&rope, e.end).max(a), e.text.as_str())
                })
                .collect();
            ranges.sort_by(|x, y| y.0.cmp(&x.0).then(y.1.cmp(&x.1)));
            for (a, b, text) in ranges {
                buffer.replace(a, b, text);
            }
            files += 1;
            count += edits.len();
        }
        if files > 0 {
            self.message = format!("{count} edit{} in {files} file{}", plural(count), plural(files));
        }
    }

    /// Asks for completions at the point, saying nothing when there's no
    /// server (it happens on every word character typed).
    pub fn lsp_complete(&mut self) {
        let Some(path) = self.buffer().file().cloned() else { return };
        self.lsp_sync();
        let position = crate::lsp::char_to_utf16(self.buffer().text(), self.buffer().point());
        let _ = self.lsp.request(RequestKind::Completion, &path, position);
    }

    #[cfg(test)]
    pub fn take_events_peek(&self) -> &[(String, Vec<String>)] {
        &self.events
    }

    // ---- The coding agent

    /// Where the agent works: the project of the selected buffer's file, or
    /// of the current folder.
    fn agent_root(&self) -> PathBuf {
        // From *agent* itself: the file in another window, else any file open.
        let shown = self.windows.iter().filter_map(|w| self.buffer_by_id(w.buffer)?.file());
        let start = self
            .buffer()
            .file()
            .or_else(|| self.window_by_id(self.last_main).and_then(|w| self.buffer_by_id(w.buffer)?.file()))
            .or_else(|| shown.clone().next())
            .or_else(|| self.buffers.iter().find_map(|(_, b)| b.file()))
            .cloned()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        crate::picker::project_root(&start)
    }

    /// Starts the agent unless it is running. Err says why it couldn't.
    pub fn agent_start(&mut self) -> Result<(), String> {
        if self.agent.as_ref().is_some_and(|a| a.state != crate::agent::State::Exited) {
            return Ok(());
        }
        let config = self.agent_config.clone();
        let root = self.agent_root();
        let agent = crate::agent::Agent::start(config.clone(), &root)
            .map_err(|e| format!("{}: {e}", config.command))?;
        self.agent = Some(agent);
        self.agent_shown = u64::MAX;
        self.agent_asked = 0;
        Ok(())
    }

    /// The agent buffer, made if it doesn't exist.
    fn agent_buffer_id(&mut self) -> BufferId {
        if let Some(id) = self.agent_buffer.filter(|&id| self.buffer_by_id(id).is_some()) {
            return id;
        }
        let mut buffer = Buffer::new("*agent*");
        buffer.set_language(Some("markdown"));
        let id = self.add_buffer(buffer);
        self.agent_buffer = Some(id);
        self.agent_shown = u64::MAX;
        id
    }

    /// Shows the agent buffer: in the window already showing it, else in a
    /// new window on the right.
    pub fn agent_show(&mut self) {
        let id = self.agent_buffer_id();
        self.agent_refresh();
        if let Some(w) = self.windows.iter().find(|w| w.buffer == id && !self.is_sidebar(w.id)).map(|w| w.id) {
            self.select_window(w);
            return;
        }
        if self.window().buffer != id {
            self.split_window(Axis::Row);
            self.show_buffer(id);
        }
    }

    /// Sends the agent a prompt, starting it if needed.
    pub fn agent_prompt(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if let Err(e) = self.agent_start() {
            self.message = e;
            return;
        }
        if let Some(agent) = self.agent.as_mut() {
            agent.prompt(text);
        }
        self.agent_refresh();
    }

    pub fn agent_cancel(&mut self) {
        if let Some(agent) = self.agent.as_mut() {
            agent.cancel();
            self.message = "Cancelling".into();
        }
    }

    /// Stops the agent; the next prompt starts a new one.
    pub fn agent_stop(&mut self) {
        if self.agent.take().is_some() {
            self.message = "Agent stopped".into();
        }
    }

    pub fn agent_status(&self) -> String {
        use crate::agent::State;
        match self.agent.as_ref().map(|a| a.state) {
            None => "off".into(),
            Some(State::Starting) => "starting".into(),
            Some(State::Idle) => "ready".into(),
            Some(State::Running) => "running".into(),
            Some(State::Exited) => "stopped".into(),
        }
    }

    /// The choices of the permission the agent is waiting for, numbered.
    pub fn agent_permission_options(&self) -> Vec<String> {
        let Some(p) = self.agent.as_ref().and_then(|a| a.permissions.first()) else { return Vec::new() };
        p.options.iter().enumerate().map(|(i, (_, name, _))| format!("{}. {name}", i + 1)).collect()
    }

    pub fn agent_permission_title(&self) -> String {
        self.agent.as_ref().and_then(|a| a.permissions.first()).map(|p| p.title.clone()).unwrap_or_default()
    }

    pub fn agent_permit(&mut self, index: usize) {
        if let Some(agent) = self.agent.as_mut() {
            agent.permit(index);
        }
        self.agent_refresh();
    }

    /// Reads what the agent sent, answers its requests, and redraws its
    /// buffer. Returns whether anything changed.
    pub fn agent_pump(&mut self) -> bool {
        let Some(agent) = self.agent.as_mut() else { return false };
        let mut changed = agent.pump();
        let requests = agent.take_requests();
        for request in requests {
            self.agent_answer(request);
            changed = true;
        }
        // Put each new permission to the person once.
        let waiting = self.agent.as_ref().map_or(0, |a| a.permissions.len());
        if waiting > self.agent_asked {
            self.agent_asked = waiting;
            self.events.push(("agent-ask-permission".into(), vec![self.agent_permission_title()]));
        } else if waiting < self.agent_asked {
            self.agent_asked = waiting;
        }
        changed | self.agent_refresh()
    }

    /// The open buffer visiting `path`, if any.
    fn buffer_visiting(&self, path: &Path) -> Option<BufferId> {
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let want = canonical(path);
        self.buffers.iter().find(|(_, b)| b.file().is_some_and(|f| canonical(f) == want)).map(|(id, _)| *id)
    }

    fn agent_answer(&mut self, request: crate::agent::Request) {
        let Some(agent) = self.agent.as_mut() else { return };
        let path = request.params["path"].as_str().map(PathBuf::from);
        match (request.method.as_str(), path) {
            ("fs/read_text_file", Some(path)) => {
                // The buffer's text, saved or not; else the file.
                let text = match self.buffers.iter().find(|(id, _)| Some(*id) == self.buffer_visiting(&path)) {
                    Some((_, b)) => Ok(b.text().to_string()),
                    None => std::fs::read_to_string(&path),
                };
                let agent = self.agent.as_mut().expect("checked above");
                match text {
                    Ok(text) => {
                        let content = crate::agent::slice_lines(
                            &text,
                            request.params["line"].as_u64(),
                            request.params["limit"].as_u64(),
                        );
                        agent.respond(request.id, json!({ "content": content }));
                    }
                    Err(e) => agent.respond_error(request.id, &format!("{}: {e}", path.display())),
                }
            }
            ("fs/write_text_file", Some(path)) => {
                let content = request.params["content"].as_str().unwrap_or("").to_string();
                let result = self.agent_write(&path, &content);
                let agent = self.agent.as_mut().expect("checked above");
                match result {
                    Ok(()) => agent.respond(request.id, Value::Null),
                    Err(e) => agent.respond_error(request.id, &format!("{}: {e}", path.display())),
                }
            }
            // Answered by the person (agent-permit).
            ("session/request_permission", _) => {}
            (method, _) => agent.respond_error(request.id, &format!("vmac doesn't support {method}")),
        }
    }

    /// Writes what the agent wrote: into the file's buffer when it is open
    /// (one undo step, then saved), else to disk.
    fn agent_write(&mut self, path: &Path, content: &str) -> std::io::Result<()> {
        if let Some(id) = self.buffer_visiting(path) {
            let b = &mut self.buffers.iter_mut().find(|(b, _)| *b == id).expect("found above").1;
            if !b.is_read_only() {
                b.undo_boundary();
                let len = b.len();
                b.replace(0, len, content);
                b.undo_boundary();
                b.save()?;
                return Ok(());
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, content)
    }

    /// Puts the conversation in the agent buffer when it changed, above
    /// the prompt you're typing (which it keeps). Windows reading the end of
    /// the conversation follow it.
    fn agent_refresh(&mut self) -> bool {
        let Some(id) = self.agent_buffer.filter(|&id| self.buffer_by_id(id).is_some()) else { return false };
        // Wrapped to the widest window showing it (re-wrapped on resize).
        let width = self.windows.iter().filter(|w| w.buffer == id).map(|w| w.cols).max().unwrap_or(0);
        let width = if width == 0 { 80 } else { width.saturating_sub(2) };
        let revision = self.agent.as_ref().map_or(0, |a| a.revision + 1);
        if (revision, width) == (self.agent_shown, self.agent_shown_width) {
            return false;
        }
        self.agent_shown = revision;
        self.agent_shown_width = width;
        let mut rendered = match self.agent.as_ref() {
            Some(agent) => agent.render(width),
            None => crate::agent::Rendered::default(),
        };
        if rendered.text.is_empty() {
            rendered.text = format!(
                "Ask {} anything about this project: type below, then RET in normal state.\n\n",
                self.agent_config.name
            );
            rendered.faces.push((0, rendered.text.chars().count(), "dim"));
            rendered.lines = vec![crate::agent::Target::None; 2];
        }
        // The rule above the prompt.
        let rule = "─".repeat(40);
        let at = rendered.text.chars().count();
        rendered.text.push_str(&rule);
        rendered.text.push('\n');
        rendered.faces.push((at, at + 40, "dim"));
        rendered.lines.push(crate::agent::Target::None);

        let b = &mut self.buffers.iter_mut().find(|(b, _)| *b == id).expect("checked above").1;
        let old = b.protected();
        b.set_protected_text(&rendered.text);
        for (start, end, face) in &rendered.faces {
            b.add_face(*start, *end, face.to_string());
        }
        let new = b.protected();
        // Windows on the conversation: those in the prompt keep their place
        // in it; those on its last lines follow it; others stay.
        let windows: Vec<usize> =
            self.windows.iter().enumerate().filter(|(_, w)| w.buffer == id).map(|(i, _)| i).collect();
        for i in windows {
            let w = &mut self.windows[i];
            w.point = if w.point >= old { w.point - old + new } else { w.point.min(new) };
        }
        self.agent_lines = rendered.lines;
        true
    }

    /// Whether the selected window shows the agent buffer.
    pub fn agent_here(&self) -> bool {
        self.agent_buffer == Some(self.window().buffer)
    }

    /// Whether the selected window is on the agent buffer's prompt.
    pub fn agent_in_prompt(&self) -> bool {
        self.agent_buffer == Some(self.window().buffer) && self.buffer().point() >= self.buffer().protected()
    }

    /// Shows the agent buffer with the point at the end of the prompt.
    pub fn agent_focus_prompt(&mut self) {
        self.agent_show();
        let b = self.buffer_mut();
        let end = b.len();
        b.goto_char(end);
        self.window_mut().point = end;
    }

    /// RET in the agent buffer: sends the prompt (from the prompt), opens
    /// or closes a tool call (on one), visits a file (on one).
    pub fn agent_activate(&mut self) {
        if self.agent_in_prompt() {
            let text = self.buffer().unprotected_text();
            if text.trim().is_empty() {
                self.message = "Type a prompt first".into();
                return;
            }
            self.buffer_mut().clear_unprotected();
            self.agent_prompt(text.trim());
            return;
        }
        let line = self.buffer().line();
        match self.agent_lines.get(line).cloned() {
            Some(crate::agent::Target::Tool(id)) => self.agent_toggle_tool(&id),
            Some(crate::agent::Target::File(path, line)) => {
                self.agent_visit(&path, line);
            }
            _ => {}
        }
    }

    /// TAB in the agent buffer: opens or closes the tool call on this line.
    pub fn agent_toggle_at_point(&mut self) {
        let line = self.buffer().line();
        if let Some(crate::agent::Target::Tool(id)) = self.agent_lines.get(line).cloned() {
            self.agent_toggle_tool(&id);
        }
    }

    fn agent_toggle_tool(&mut self, id: &str) {
        if let Some(agent) = self.agent.as_mut() {
            agent.toggle(id);
        }
        // Keep the point on the tool's line as lines open below it.
        let line = self.buffer().line();
        self.agent_refresh();
        let pos = self.buffer().line_to_pos(line);
        self.buffer_mut().goto_char(pos);
        self.window_mut().point = pos;
    }

    /// Opens a file the agent touched in another window, at its line.
    fn agent_visit(&mut self, path: &Path, line: Option<u64>) {
        let here = self.selected;
        let other = self.window_order().into_iter().find(|&w| w != here && !self.is_sidebar(w));
        match other {
            Some(w) => self.select_window(w),
            None => self.split_window(Axis::Row),
        }
        if let Err(e) = self.open_file(path) {
            self.message = format!("{}: {e}", path.display());
            return;
        }
        if let Some(n) = line {
            let pos = self.buffer().line_to_pos((n as usize).saturating_sub(1));
            self.buffer_mut().goto_char(pos);
            self.window_mut().point = pos;
        }
    }

    /// Asks about lines of the selected buffer: sends the question with the
    /// file, the line numbers and the text.
    pub fn agent_ask_about(&mut self, start: usize, end: usize, question: &str) {
        let b = self.buffer();
        let (first, last) = (b.line_of(start), b.line_of(end.saturating_sub(1).max(start)));
        let code = b.substring(b.line_to_pos(first), b.line_end(b.line_to_pos(last)));
        let root = self.agent_root();
        let name = match b.file() {
            Some(f) => f.strip_prefix(&root).unwrap_or(f).display().to_string(),
            None => b.name().to_string(),
        };
        let language = b.file().and_then(|f| crate::syntax::language_for_path(f)).unwrap_or("");
        let place = if first == last { format!("{name}:{}", first + 1) } else { format!("{name}:{}-{}", first + 1, last + 1) };
        let question = if question.trim().is_empty() { "Explain this code." } else { question.trim() };
        let text = format!("{question}\n\nFrom `{place}`:\n\n```{language}\n{code}\n```\n");
        let shown = format!("{question}  [{place}]");
        if let Err(e) = self.agent_start() {
            self.message = e;
            return;
        }
        self.agent_show();
        if let Some(agent) = self.agent.as_mut() {
            agent.prompt_as(&shown, &text);
        }
        self.agent_refresh();
    }

    /// A new conversation: stops the agent and starts it again.
    pub fn agent_new_session(&mut self) {
        self.agent = None;
        if let Err(e) = self.agent_start() {
            self.message = e;
        }
        self.agent_refresh();
    }

    pub fn take_events(&mut self) -> Vec<(String, Vec<String>)> {
        std::mem::take(&mut self.events)
    }

    /// Where the word before the point starts.
    fn word_start(&self) -> usize {
        let b = self.buffer();
        let mut p = b.point();
        while p > 0 && b.char_at(p - 1).chars().next().is_some_and(is_word_char) {
            p -= 1;
        }
        p
    }

    /// Filters the popup by what's been typed; closes it when nothing matches
    /// or the cursor left the word.
    pub fn completion_refresh(&mut self) {
        let point = self.buffer().point();
        let prefix = match &self.completion {
            Some(c) if point >= c.start && self.buffer().line_of(point) == self.buffer().line_of(c.start) => {
                self.buffer().substring(c.start, point)
            }
            _ => {
                self.completion = None;
                return;
            }
        };
        if let Some(c) = self.completion.as_mut() {
            c.filter(&prefix);
            if c.shown.is_empty() {
                self.completion = None;
            }
        }
    }

    pub fn completion_move(&mut self, n: isize) {
        if let Some(c) = self.completion.as_mut()
            && !c.shown.is_empty()
        {
            c.selected = (c.selected as isize + n).rem_euclid(c.shown.len() as isize) as usize;
        }
    }

    /// Replaces the word being completed with the selected candidate.
    pub fn completion_accept(&mut self) -> bool {
        let Some(c) = self.completion.take() else { return false };
        let Some(&i) = c.shown.get(c.selected) else { return false };
        let text = c.all[i].text.clone();
        let point = self.buffer().point();
        let b = self.buffer_mut();
        b.delete_range(c.start, point);
        b.goto_char(c.start);
        b.insert(&text);
        true
    }

    /// A reference as the picker shows it: `path:line:col  code`, the path
    /// relative to the project.
    fn location_item(&self, loc: &Location, root: &Path) -> String {
        let text = std::fs::read_to_string(&loc.path).unwrap_or_default();
        let rope = ropey::Rope::from_str(&text);
        let line_text = if loc.position.0 < rope.len_lines() { rope.line(loc.position.0).to_string() } else { String::new() };
        let col = crate::lsp::utf16_to_char(&rope, loc.position) - rope.line_to_char(loc.position.0.min(rope.len_lines().saturating_sub(1)));
        let shown = loc.path.strip_prefix(root).unwrap_or(&loc.path);
        format!("{}:{}:{}  {}", shown.display(), loc.position.0 + 1, col + 1, line_text.trim())
    }

    /// Opens a `path:line:col` item (relative to the project, or absolute)
    /// at its place, remembering where it came from for C-o.
    pub fn goto_location(&mut self, item: &str) -> bool {
        let Some((path, line, col)) = crate::picker::parse_location(item) else { return false };
        let root = crate::picker::project_root(self.buffer().file().map(PathBuf::as_path).unwrap_or(Path::new(".")));
        let path = if Path::new(&path).is_absolute() { PathBuf::from(&path) } else { root.join(&path) };
        let from = (self.window().buffer, self.buffer().point());
        match self.open_file(&path) {
            Ok(()) => {
                self.jumps.push(from);
                let b = self.buffer_mut();
                let start = b.line_to_pos(line.saturating_sub(1));
                let end = b.line_end(start);
                b.goto_char((start + col.saturating_sub(1)).min(end));
                true
            }
            Err(e) => {
                self.message = format!("{}: {e}", path.display());
                false
            }
        }
    }

    fn act_on(&mut self, reply: Reply) {
        match reply {
            Reply::Definition(locations) => match locations.first() {
                None => self.message = "No definition found".into(),
                Some(loc) => {
                    let from = (self.window().buffer, self.buffer().point());
                    match self.open_file(&loc.path) {
                        Ok(()) => {
                            self.jumps.push(from);
                            let pos = crate::lsp::utf16_to_char(self.buffer().text(), loc.position);
                            self.buffer_mut().goto_char(pos);
                            if locations.len() > 1 {
                                self.message = format!("{} definitions; went to the first", locations.len());
                            }
                        }
                        Err(e) => self.message = format!("{}: {e}", loc.path.display()),
                    }
                }
            },
            Reply::Hover(Some(text)) => self.hover = Some(text),
            Reply::Hover(None) => self.message = "No documentation here".into(),
            Reply::References(locations) => {
                let root = crate::picker::project_root(self.buffer().file().map(PathBuf::as_path).unwrap_or(Path::new(".")));
                let items: Vec<String> = locations.iter().map(|l| self.location_item(l, &root)).collect();
                match items.len() {
                    0 => self.message = "No references found".into(),
                    1 => {
                        self.goto_location(&items[0]);
                    }
                    _ => self.events.push(("lsp-show-references".into(), items)),
                }
            }
            Reply::Edit(edit) => {
                if edit.iter().all(|(_, e)| e.is_empty()) {
                    self.message = "Nothing to change".into();
                } else {
                    self.apply_workspace_edit(&edit);
                }
            }
            Reply::Format(path, edits) => {
                if edits.is_empty() {
                    self.message = "Already formatted".into();
                } else {
                    self.apply_workspace_edit(&vec![(path, edits)]);
                    self.message = "formatted".into();
                }
            }
            Reply::CodeActions(actions) => {
                if actions.is_empty() {
                    self.message = "No code actions here".into();
                } else {
                    let titles = actions
                        .iter()
                        .enumerate()
                        .map(|(i, a)| format!("{}. {}", i + 1, crate::lsp::action_title(a)))
                        .collect();
                    self.code_actions = actions;
                    self.events.push(("lsp-show-code-actions".into(), titles));
                }
            }
            Reply::ResolvedAction(action) => {
                if action["edit"].is_object() {
                    self.apply_workspace_edit(&crate::lsp::parse_workspace_edit(&action["edit"]));
                } else {
                    self.message = "The server gave no edit for that action".into();
                }
            }
            Reply::Completion(items) => {
                if items.is_empty() {
                    self.completion = None;
                } else {
                    let start = self.word_start();
                    self.completion = Some(Completion { all: items, shown: Vec::new(), selected: 0, start });
                    self.completion_refresh();
                }
            }
        }
    }

    /// Goes back to where the last jump (gd) came from.
    pub fn jump_back(&mut self) -> bool {
        while let Some((buffer, point)) = self.jumps.pop() {
            if self.buffer_by_id(buffer).is_some() {
                self.show_buffer(buffer);
                self.buffer_mut().goto_char(point);
                return true;
            }
        }
        false
    }

    /// The selected buffer's diagnostics, with their character ranges, in order.
    pub fn buffer_diagnostics(&self) -> Vec<((usize, usize), Diagnostic)> {
        let Some(path) = self.buffer().file() else { return vec![] };
        let rope = self.buffer().text();
        let mut list: Vec<_> = self.lsp.diagnostics(path).iter().map(|d| (d.to_chars(rope), d.clone())).collect();
        list.sort_by_key(|(range, d)| (range.0, d.severity));
        list
    }

    // ---- Terminals

    pub fn terminal_for(&mut self, buffer: BufferId) -> Option<&mut Terminal> {
        self.terminals.iter_mut().find(|t| t.buffer == buffer)
    }

    /// The selected buffer's terminal, if it is one.
    pub fn terminal_here(&mut self) -> Option<&mut Terminal> {
        let id = self.window().buffer;
        self.terminal_for(id)
    }

    /// The size of the window showing a buffer (rows, columns); 24x80
    /// until the view has laid it out.
    pub fn size_for(&self, buffer: BufferId) -> (u16, u16) {
        let w = self.windows.iter().find(|w| w.buffer == buffer).unwrap_or_else(|| self.window());
        window_size(w)
    }

    /// The selected window's size, for a terminal about to open in it.
    pub fn selected_size(&self) -> (u16, u16) {
        window_size(self.window())
    }

    /// Makes a terminal buffer (`*terminal*`, `*terminal*<2>`...) for a
    /// backend and shows it in the selected window.
    pub fn add_terminal(&mut self, backend: TermBackend) -> BufferId {
        let mut buffer = Buffer::new("*terminal*");
        buffer.set_read_only(true);
        let id = self.add_buffer(buffer);
        self.terminals.push(Terminal { buffer: id, backend, exited: false });
        self.show_buffer(id);
        id
    }

    /// Shows a terminal's screen in its buffer. `follow` puts the point on
    /// the terminal's cursor (while typing into it).
    pub fn show_screen(&mut self, buffer: BufferId, screen: &crate::terminal::Screen, follow: bool) {
        let Some((_, b)) = self.buffers.iter_mut().find(|(id, _)| *id == buffer) else { return };
        b.set_text(&screen.lines.join("\n"));
        let rope = b.text().clone();
        for (line, start, end, face) in &screen.colors {
            if *line < rope.len_lines() {
                let base = rope.line_to_char(*line);
                b.add_face(base + start, base + end, face.clone());
            }
        }
        if follow {
            let line = screen.cursor.0.min(rope.len_lines().saturating_sub(1));
            b.goto_char(rope.line_to_char(line) + screen.cursor.1.min(b.line_end(rope.line_to_char(line)) - rope.line_to_char(line)));
        }
    }

    /// Reads local terminals, resizing each to its window. Returns whether
    /// any screen changed. `follow` is the buffer being typed into, if any.
    pub fn pump_local_terminals(&mut self, follow: Option<BufferId>) -> bool {
        let mut screens = Vec::new();
        let sizes: Vec<(u16, u16)> = self.terminals.iter().map(|t| self.size_for(t.buffer)).collect();
        for (t, (rows, cols)) in self.terminals.iter_mut().zip(sizes) {
            if let TermBackend::Local(pty) | TermBackend::Attached { pty, .. } = &mut t.backend {
                pty.resize(rows, cols);
                if let Some(screen) = pty.screen() {
                    screens.push((t.buffer, screen));
                }
                if !t.exited && !pty.alive() {
                    t.exited = true;
                }
            }
        }
        let changed = !screens.is_empty();
        for (buffer, screen) in screens {
            self.show_screen(buffer, &screen, follow == Some(buffer));
        }
        changed
    }

    // ---- Search

    /// The compiled pattern, reused while the pattern stays the same.
    pub fn search_regex(&mut self, pattern: &str) -> Option<Regex> {
        if let Some((p, re)) = &self.search_cache
            && p == pattern
        {
            return Some(re.clone());
        }
        let re = compile_search(pattern)?;
        self.search_cache = Some((pattern.to_string(), re.clone()));
        Some(re)
    }
}

#[derive(Clone, Debug)]
pub struct Editor(Arc<Mutex<EditorState>>);

impl Editor {
    pub fn new(buffer: Buffer) -> Self {
        Self(Arc::new(Mutex::new(EditorState::new(buffer))))
    }

    pub fn lock(&self) -> MutexGuard<'_, EditorState> {
        // A panic inside a primitive poisons the lock; the state is still
        // usable, so keep editing rather than taking the editor down.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(text: &str) -> EditorState {
        let mut b = Buffer::new("a");
        b.insert(text);
        b.goto_char(0);
        EditorState::new(b)
    }

    #[test]
    fn splits_place_windows_and_find_neighbors() {
        let mut s = state("x");
        let first = s.selected_window();
        s.split_window(Axis::Row);
        let right = s.selected_window();
        assert_ne!(first, right);
        s.split_window(Axis::Column);
        let below = s.selected_window();
        let rects = s.window_rects();
        let r = |id| rects.iter().find(|(w, _)| *w == id).unwrap().1;
        assert_eq!(r(first), Rect { x: 0., y: 0., w: 0.5, h: 1. });
        assert_eq!(r(right), Rect { x: 0.5, y: 0., w: 0.5, h: 0.5 });
        assert_eq!(r(below), Rect { x: 0.5, y: 0.5, w: 0.5, h: 0.5 });
        assert_eq!(s.window_in_direction("up"), Some(right));
        assert_eq!(s.window_in_direction("left"), Some(first));
        assert_eq!(s.window_in_direction("right"), None);
    }

    #[test]
    fn closing_collapses_the_split() {
        let mut s = state("x");
        let first = s.selected_window();
        s.split_window(Axis::Row);
        assert!(s.close_window());
        assert_eq!(s.selected_window(), first);
        assert_eq!(s.layout, Layout::Leaf(first));
        assert!(!s.close_window(), "the last window stays");
    }

    #[test]
    fn each_window_keeps_its_own_point() {
        let mut s = state("one\ntwo\nthree");
        let first = s.selected_window();
        s.split_window(Axis::Column);
        s.buffer_mut().goto_char(8);
        s.select_window(first);
        assert_eq!(s.buffer().point(), 0);
        assert_eq!(s.window_point(s.window_order()[1]), 8);
        s.other_window(1);
        assert_eq!(s.buffer().point(), 8);
    }

    #[test]
    fn buffers_get_unique_names_and_kill_falls_back() {
        let mut s = state("x");
        let b = s.add_buffer(Buffer::new("a"));
        assert_eq!(s.buffer_names(), vec!["a", "a<2>"]);
        s.show_buffer(b);
        assert_eq!(s.buffer().name(), "a<2>");
        s.buffer_mut().insert("!");
        assert!(s.kill_buffer(false).is_err(), "unsaved changes");
        assert_eq!(s.kill_buffer(true).unwrap(), "a<2>");
        assert_eq!(s.buffer().name(), "a");
        s.kill_buffer(true).unwrap();
        assert_eq!(s.buffer_names(), vec!["*scratch*"]);
    }

    #[test]
    fn the_sidebar_sits_left_of_the_layout() {
        let mut s = state("x");
        let main = s.selected_window();
        let tree = s.add_buffer(Buffer::new("*tree*"));
        s.sidebar_width = 0.25;
        s.open_sidebar(tree);
        let side = s.selected_window();
        assert!(s.is_sidebar(side));
        assert_eq!(s.buffer().name(), "*tree*");
        let rects = s.window_rects();
        assert_eq!(rects[0], (side, Rect { x: 0., y: 0., w: 0.25, h: 1. }));
        assert_eq!(rects[1], (main, Rect { x: 0.25, y: 0., w: 0.75, h: 1. }));
        assert_eq!(s.window_in_direction("right"), Some(main));
        s.select_window(main);
        assert_eq!(s.window_in_direction("left"), Some(side));
        assert!(!s.close_window(), "the last layout window stays while the sidebar is open");
        s.select_window(side);
        s.split_window(Axis::Row);
        assert!(!s.is_sidebar(s.selected_window()), "splitting from the sidebar splits the main window");
        s.select_window(side);
        s.close_sidebar();
        assert_eq!(s.sidebar(), None);
        assert!(!s.is_sidebar(s.selected_window()));
    }

    #[test]
    fn a_nerd_font_is_the_default_when_installed() {
        let names = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(default_font(&names(&["Menlo", "Hack Nerd Font Mono", "JetBrainsMono NFM"])).as_deref(), Some("JetBrainsMono NFM"));
        assert_eq!(default_font(&names(&["Menlo", "Agave Nerd Font Mono"])).as_deref(), Some("Agave Nerd Font Mono"));
        assert_eq!(default_font(&names(&["Menlo", "Monaco"])), None);
    }

    #[test]
    fn scroll_and_fit() {
        let mut s = state(&"line\n".repeat(100));
        let w = s.selected_window();
        s.fit_window(w, 10);
        s.scroll(20);
        assert_eq!(s.window().scroll_top, 20);
        s.buffer_mut().goto_char(0);
        s.fit_window(w, 10);
        assert_eq!(s.window().scroll_top, 0, "the cursor is kept on screen");
        s.buffer_mut().forward_line(50);
        s.recenter("center");
        assert_eq!(s.window().scroll_top, 45);
    }
}
