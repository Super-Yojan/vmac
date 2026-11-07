//! The editor's state, shared between the Steel primitives and the GPUI view:
//! the buffers, the windows showing them and how the windows are split.
//!
//! Steel requires native functions to be `Send + Sync`, so the state lives
//! behind `Arc<Mutex<_>>`. Everything runs on GPUI's main thread, so the lock
//! is never contended; it is only there to satisfy the bounds.

use crate::buffer::Buffer;
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
    pub quit_requested: bool,
    /// The open fuzzy picker, if any; its query is the minibuffer's text.
    pub picker: Option<Picker>,
    search_cache: Option<(String, Regex)>,
}

impl EditorState {
    pub fn new(buffer: Buffer) -> Self {
        let window = Window { id: 2, buffer: 1, point: buffer.point(), scroll_top: 0, rows: 0 };
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
            quit_requested: false,
            picker: None,
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
                self.windows.push(Window { id, buffer, point: 0, scroll_top: 0, rows: 0 });
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
