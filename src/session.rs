//! An editing session: the editor's state and the scripting engine that
//! drives it, behind the plain-data line in `frame.rs`. It takes `Input`s
//! and lays out `Frame`s; it knows nothing of GPUI, so it can run in another
//! process from the window that shows it.
//!
//! (Not to be confused with `sessions.rs`: past agent conversations.)

use crate::editor::{Editor, Rect};
use crate::frame::*;
use crate::lisp::{FaceSpec, Lisp};
use crate::syntax::{Syntax, face_for, language_for_path};
use std::collections::{HashMap, HashSet};

pub struct Session {
    editor: Editor,
    lisp: Lisp,
    syntax: Syntax,
    /// The button held down since a press in a window.
    drag: Option<Drag>,
    /// Windows the last frame left for the window to draw itself (rendered
    /// Markdown, the review), where a click only selects the window.
    custom: HashSet<WindowId>,
}

/// A button held down: in a window's text (dragging selects), or reported
/// to the terminal program that asked for the mouse.
#[derive(Debug, Clone, Copy)]
struct Drag {
    window: WindowId,
    /// The terminal's button, when the program has the mouse.
    terminal: Option<u8>,
    /// The last position reported: a buffer position, or a terminal cell.
    last: (usize, usize),
}

impl Session {
    pub fn new(editor: Editor, lisp: Lisp) -> Self {
        Self { editor, lisp, syntax: Syntax::default(), drag: None, custom: HashSet::new() }
    }

    pub fn quit_requested(&self) -> bool {
        self.editor.lock().quit_requested
    }

    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// Evaluates Lua in this session.
    #[cfg(test)]
    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        self.lisp.eval(code)
    }

    /// Runs a command by name, as a key bound to it would.
    pub fn run_command(&mut self, name: &str) {
        self.lisp.run_command(name);
    }

    // ---- Work that happens on its own (the window calls these on timers)

    /// Reloads the core and init files when they change on disk.
    pub fn reload_if_changed(&mut self) -> bool {
        self.lisp.reload_if_changed()
    }

    /// Reads terminals' screens into their buffers.
    pub fn pump_terminals(&mut self) -> bool {
        !self.editor.lock().terminals.is_empty() && self.lisp.pump_terminals()
    }

    /// Syncs files with their language servers, reads what they and the
    /// agent report, and runs the events their replies raise.
    pub fn pump_servers(&mut self) -> bool {
        let configs = self.lisp.lsp_servers();
        let changed = {
            let mut editor = self.editor.lock();
            editor.lsp_pump(configs) | editor.agent_pump() | editor.dashboard_refresh() | editor.git_pump()
        };
        let ran = self.lisp.run_events();
        self.lisp.tick() || ran || changed
    }

    // ---- Input

    pub fn handle(&mut self, input: Input) {
        match input {
            Input::Key { key, text } => self.lisp.dispatch_key(&key, &text),
            Input::MouseDown { window, button, line, col, ch, clicks, mods } => {
                self.mouse_down(window, button, (line, col, ch), clicks, mods)
            }
            Input::MouseDrag { window, line, col, ch, mods } => self.mouse_drag(window, (line, col, ch), mods),
            Input::MouseUp { window, line, col, mods } => {
                let Some(drag) = self.drag.take() else { return };
                let Some(button) = drag.terminal.filter(|_| drag.window == window) else { return };
                let mut st = self.editor.lock();
                let Some(buffer) = st.window_by_id(window).map(|w| w.buffer) else { return };
                st.terminal_mouse(buffer, crate::terminal::MouseAction::Release(button), line, col, mouse_mods(mods));
            }
            Input::MouseLost => self.drag = None,
            Input::Wheel { window, lines, line, col, mods } => {
                let mut st = self.editor.lock();
                let Some(buffer) = st.window_by_id(window).map(|w| w.buffer) else { return };
                // Ctrl and the wheel over an image or PDF zoom it: up (away) in, down out.
                let media = st.buffer_by_id(buffer).and_then(|b| b.file()).and_then(|f| crate::frame::media_kind(f));
                if media.is_some() && mods.control {
                    st.zoom_media(buffer, WHEEL_ZOOM.powi(-lines as i32));
                    return;
                }
                // A terminal program that asked for the mouse gets the wheel.
                if is_terminal(&st, buffer) && !mods.shift {
                    use crate::terminal::MouseAction::{WheelDown, WheelUp};
                    let action = if lines < 0 { WheelUp } else { WheelDown };
                    if st.terminal_mouse(buffer, action, line, col, mouse_mods(mods)) {
                        for _ in 1..lines.unsigned_abs() {
                            st.terminal_mouse(buffer, action, line, col, mouse_mods(mods));
                        }
                        return;
                    }
                }
                st.scroll_window_by(window, lines);
            }
            Input::PdfDoubleClick { window, page, x, y } => {
                let mut st = self.editor.lock();
                let Some(buffer) = st.window_by_id(window).map(|w| w.buffer) else { return };
                let Some(pdf) = st.buffer_by_id(buffer).and_then(|b| b.file()).map(|f| f.display().to_string()) else {
                    return;
                };
                // The PDF's window is the one acted from.
                st.select_window(window);
                st.push_event("lua-event", vec!["pdf_double_click".into(), pdf, page.to_string(), x.to_string(), y.to_string()]);
                drop(st);
                // Now, not at the next tick: the jump follows the click.
                self.lisp.run_events();
            }
            Input::ReviewSelect { path, modified, line } => self.editor.lock().review_select(&path, modified, line),
            Input::ReviewCommentAt { path, modified, line, buffer } => {
                let side = if modified { "modified" } else { "original" };
                let mut st = self.editor.lock();
                let name = st.buffer_by_id(buffer).map(|b| b.name().to_string()).unwrap_or_default();
                st.push_event("lua-event", vec!["review_comment_at".into(), path, side.into(), line.to_string(), name]);
            }
            Input::Client { fonts, themes, errors } => {
                let mut st = self.editor.lock();
                if st.font.is_none() {
                    st.font = crate::editor::default_font(&fonts);
                }
                st.fonts = fonts;
                for e in errors {
                    st.message = format!("a theme file didn't load: {e}");
                }
                st.theme_names = themes;
            }
            Input::Appearance { dark } => self.editor.lock().system_dark = dark,
            Input::TerminalColors { foreground, background } => crate::terminal::set_colors(foreground, background),
        }
    }

    /// Runs a mouse call in Lua (`core[name]`); its error, if any, in the echo area.
    fn mouse_call(&mut self, name: &str, args: impl mlua::IntoLuaMulti) {
        if let Err(e) = self.lisp.call::<()>(name, args) {
            self.editor.lock().message = format!("mouse: {}", e.lines().next().unwrap_or(""));
        }
    }

    fn mouse_down(&mut self, window: WindowId, button: u8, (line, col, ch): (usize, usize, usize), clicks: usize, mods: Mods) {
        let Some(buffer) = self.editor.lock().window_by_id(window).map(|w| w.buffer) else { return };
        // A program that asked for the mouse gets it; shift selects its text instead.
        if is_terminal(&self.editor.lock(), buffer) && !mods.shift {
            let taken = {
                let mut st = self.editor.lock();
                let taken =
                    st.terminal_mouse(buffer, crate::terminal::MouseAction::Press(button), line, col, mouse_mods(mods));
                if taken {
                    st.select_window(window);
                }
                taken
            };
            if taken {
                self.drag = Some(Drag { window, terminal: Some(button), last: (line, col) });
                // Typing goes to the program you clicked in.
                self.mouse_call("enter_state", "insert");
                return;
            }
        }
        if button != 0 {
            return;
        }
        let pos = {
            let mut st = self.editor.lock();
            st.select_window(window);
            let text = st.buffer().text();
            let line = line.min(text.len_lines().saturating_sub(1));
            text.line_to_char(line) + ch.min(text_line_len(text, line))
        };
        if self.custom.contains(&window) {
            return;
        }
        self.drag = Some(Drag { window, terminal: None, last: (pos, 0) });
        self.mouse_call("mouse_click", (pos, clicks.max(1)));
    }

    fn mouse_drag(&mut self, window: WindowId, (line, col, ch): (usize, usize, usize), mods: Mods) {
        let Some(drag) = self.drag.filter(|d| d.window == window) else { return };
        if let Some(button) = drag.terminal {
            if (line, col) != drag.last {
                self.drag = Some(Drag { last: (line, col), ..drag });
                let mut st = self.editor.lock();
                let Some(buffer) = st.window_by_id(window).map(|w| w.buffer) else { return };
                st.terminal_mouse(buffer, crate::terminal::MouseAction::Drag(button), line, col, mouse_mods(mods));
            }
            return;
        }
        let pos = {
            let st = self.editor.lock();
            if st.selected_window() != window {
                return;
            }
            let text = st.buffer().text();
            let line = line.min(text.len_lines().saturating_sub(1));
            // Dragging past the end of a line selects up to its last character.
            text.line_to_char(line) + ch.min(text_line_len(text, line).saturating_sub(1))
        };
        if pos != drag.last.0 {
            self.drag = Some(Drag { last: (pos, 0), ..drag });
            self.mouse_call("mouse_drag", pos);
        }
    }

    // ---- The frame

    /// Lays out everything on screen for a window `viewport` in size, and
    /// keeps what that decides (where windows scroll, how many rows and
    /// columns each has, for terminals).
    pub fn frame(&mut self, viewport: Viewport) -> Frame {
        let Viewport { width, height } = viewport;
        // The echo area takes the bottom row; windows share the rest.
        let area = (height - ROW_HEIGHT).max(ROW_HEIGHT * 2.);

        let bar = self.lisp.cursor_shape() == "bar";
        let markdown_raw = self.lisp.markdown_raw();
        let selection = self.lisp.selection();
        let pattern = self.lisp.search_highlight();
        let mut modeline = Some(self.lisp.modeline());
        let syntax_theme = self.lisp.syntax_theme();
        let face_specs = self.lisp.face_specs();
        let syntax = &mut self.syntax;

        let mut st = self.editor.lock();
        st.sidebar_width = (SIDEBAR_WIDTH / width.max(1.)).clamp(0.12, 0.45);
        let rects = st.window_rects();
        for (id, r) in &rects {
            st.fit_window(*id, window_rows(r, area));
        }
        let search = st.search_regex(&pattern);
        let minibuffer_open = st.minibuffer_prompt.is_some();
        let selected = st.selected_window();

        // Markdown files shown rendered, like the preview: in every window on
        // one, except the window you're typing or selecting in.
        let rendered: HashSet<WindowId> = rects
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| st.markdown_rendered && !st.is_sidebar(*id) && !(*id == selected && markdown_raw))
            .filter(|id| {
                st.window_by_id(*id)
                    .and_then(|w| st.buffer_by_id(w.buffer))
                    .and_then(|b| b.file())
                    .and_then(|f| language_for_path(f))
                    == Some("markdown")
            })
            .collect();

        // *agent-review* as a diff, when a window shows it, the cursor's row selected.
        let review_id = st
            .review_buffer_id()
            .filter(|id| rects.iter().any(|(w, _)| st.window_by_id(*w).is_some_and(|w| w.buffer == *id)));
        let review = review_id.map(|id| {
            let buffer = st.buffer_by_id(id).expect("checked above");
            let patch: String = buffer
                .text()
                .to_string()
                .lines()
                .filter(|l| !l.is_empty() && *l != "Nothing to review.")
                .map(|l| format!("{l}\n"))
                .collect();
            let cursor_line = rects
                .iter()
                .find_map(|(w, _)| st.window_by_id(*w).filter(|w| w.buffer == id).map(|w| w.id))
                .map(|w| buffer.text().char_to_line(st.window_point(w).min(buffer.len())))
                .unwrap_or(0);
            let row = st.review_rows.get(cursor_line).cloned().flatten().map(|r| DiffRow { rel: r.rel, modified: r.modified, line: r.line });
            let comments = st
                .review_comments
                .iter()
                .map(|c| DiffComment {
                    id: c.id,
                    at: DiffRow { rel: c.at.rel.clone(), modified: c.at.modified, line: c.at.line },
                    text: c.text.clone(),
                    label: String::new(),
                })
                .collect();
            ReviewView { header: String::new(), patch, revision: buffer.revision(), cursor_line, row, comments }
        });
        // Every buffer drawn as a Diff, by buffer: *agent-review*, and the
        // diff buffers (a commit: its header above, the diff below).
        let mut diffs: HashMap<BufferId, ReviewView> = HashMap::new();
        if let (Some(id), Some(view)) = (review_id, review) {
            diffs.insert(id, view);
        }
        for (wid, _) in &rects {
            let Some(w) = st.window_by_id(*wid) else { continue };
            if !st.diff_buffers.contains(&w.buffer) || diffs.contains_key(&w.buffer) {
                continue;
            }
            let Some(buffer) = st.buffer_by_id(w.buffer) else { continue };
            let text = buffer.text().to_string();
            let (header, patch) = match text.find("diff --git ") {
                Some(0) => (String::new(), text.clone()),
                Some(at) => (text[..at].trim_end().to_string(), text[at..].to_string()),
                None => (text.trim_end().to_string(), String::new()),
            };
            let cursor_line = buffer.text().char_to_line(st.window_point(*wid).min(buffer.len()));
            let row = crate::git::diff_rows(&text)
                .get(cursor_line)
                .cloned()
                .flatten()
                .map(|(rel, modified, line)| DiffRow { rel, modified, line });
            let comments = st
                .diff_comments
                .get(&w.buffer)
                .map(|list| {
                    list.iter()
                        .enumerate()
                        .map(|(i, (rel, modified, line, label, text))| DiffComment {
                            id: i as u64,
                            at: DiffRow { rel: rel.clone(), modified: *modified, line: *line },
                            text: text.clone(),
                            label: label.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            diffs.insert(w.buffer, ReviewView { header, patch, revision: buffer.revision(), cursor_line, row, comments });
        }

        // Lines the agent changed and nobody reviewed yet, and lines with
        // your comments on them, per window.
        let mut review_marks: std::collections::HashMap<WindowId, Vec<(usize, bool)>> = Default::default();
        let mut comment_marks: std::collections::HashMap<WindowId, Vec<usize>> = Default::default();
        // Lines that differ from git's index, and who last changed the
        // cursor's line (the selected window's).
        let mut git_marks: std::collections::HashMap<WindowId, std::collections::HashMap<usize, u8>> = Default::default();
        let mut blame_at: Option<(WindowId, usize, String)> = None;
        let mut band_marks: std::collections::HashMap<WindowId, std::collections::HashMap<usize, (usize, usize, String)>> =
            Default::default();
        for (id, _) in &rects {
            let Some(buffer) = st.window_by_id(*id).map(|w| w.buffer) else { continue };
            let marks = st.review_marks(buffer);
            if !marks.is_empty() {
                review_marks.insert(*id, marks);
            }
            let lines = st.comment_lines(buffer);
            if !lines.is_empty() {
                comment_marks.insert(*id, lines);
            }
            let signs = st.git_signs(buffer);
            if !signs.is_empty() {
                use crate::git::Sign;
                let code = |s: &Sign| match s {
                    Sign::Added => 1,
                    Sign::Changed => 2,
                    Sign::Deleted => 3,
                };
                git_marks.insert(*id, signs.iter().map(|(l, s)| (*l, code(s))).collect());
            }
            if st.git.blame_inline && *id == st.selected_window() {
                let line = st.buffer_by_id(buffer).map(|b| b.line()).unwrap_or(0);
                if let Some(text) = st.git_blame_text(buffer, line) {
                    blame_at = Some((*id, line, text));
                }
            }
            let bands = st.line_bands(buffer);
            if !bands.is_empty() {
                band_marks.insert(*id, bands.into_iter().map(|(line, from, to, kind)| (line, (from, to, kind))).collect());
            }
        }

        // Where each window ends up scrolled and how many lines it shows,
        // once line heights are known (headings are taller).
        let mut fitted: Vec<(WindowId, usize, usize)> = Vec::new();
        let mut cursor_at: Option<(f32, f32, f32)> = None;
        let mut custom = HashSet::new();
        let terminal_buffers: HashSet<BufferId> = st.terminals.iter().map(|t| t.buffer).collect();

        let windows: Vec<FrameWindow> = rects
            .iter()
            .filter_map(|(id, r)| {
                let w = st.window_by_id(*id)?.clone();
                let buffer = st.buffer_by_id(w.buffer)?;
                let text = buffer.text();
                let is_selected = *id == selected;
                // The sidebar marks its cursor's line instead of drawing a cursor.
                let is_sidebar = st.is_sidebar(*id);
                let point = st.window_point(*id);
                let cur_line = text.char_to_line(point);
                let cur_col = point - text.line_to_char(cur_line);

                // Styles for every line that could be on screen: from the top (or
                // the cursor, if above) through as many normal-height rows as fit.
                let estimate = window_rows(r, area);
                let lo = w.scroll_top.min(cur_line);
                let hi = (w.scroll_top + estimate).max(cur_line + 1).min(text.len_lines());
                let language = buffer.language().or_else(|| buffer.file().and_then(|f| language_for_path(f)));
                let spans = match language {
                    Some(lang) => syntax.highlight(w.buffer, lang, buffer.revision(), text, lo..hi),
                    None => Vec::new(),
                };
                // Markdown with its markup hidden, except where you're typing.
                let conceal_here = st.markdown_conceal && language == Some("markdown") && !(is_selected && markdown_raw);
                let conceals = if conceal_here { crate::markdown::conceal(text, lo..hi) } else { Vec::new() };
                let ops_of = |i: usize| conceals.get(i.wrapping_sub(lo)).map(Vec::as_slice).unwrap_or(&[]);
                // Styles of line i, in character columns (not yet screen columns).
                let styles_of = |i: usize| -> Vec<(usize, usize, Face)> {
                    let start = text.line_to_char(i);
                    let line_len = text.line(i).len_chars();
                    let mut out: Vec<(usize, usize, Face)> = spans
                        .get(i - lo)
                        .map(|line_spans| {
                            line_spans
                                .iter()
                                .filter_map(|sp| {
                                    let face = face_for(&sp.capture, &syntax_theme)?;
                                    Some((sp.start, sp.end, resolve_face(face, &face_specs)))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // Faces Lua put on the text (the tree's folders, ...).
                    out.extend(buffer.faces().iter().filter(|f| f.0 < start + line_len && f.1 > start).map(
                        |(a, b, face)| (a.saturating_sub(start), b.saturating_sub(start), resolve_face(face, &face_specs)),
                    ));
                    out
                };
                let styles: Vec<_> = (lo..hi).map(styles_of).collect();
                // Language server diagnostics, as character ranges and severities.
                let diagnostics: Vec<(usize, usize, u8)> = buffer
                    .file()
                    .map(|f| {
                        st.lsp
                            .diagnostics(f)
                            .iter()
                            .map(|d| {
                                let (a, b) = d.to_chars(text);
                                (a, b, d.severity)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let height_of = |i: usize| ROW_HEIGHT * line_scale(&styles[i - lo]);
                let media = buffer.file().and_then(|f| crate::frame::media_kind(f).map(|k| (f.clone(), k)));
                let is_custom =
                    rendered.contains(id) || media.is_some() || diffs.contains_key(&w.buffer);
                // Line numbers, in a gutter: for files, when they're on.
                let numbers = match st.line_numbers.as_str() {
                    "on" | "relative" if buffer.file().is_some() && !is_sidebar && !is_custom => {
                        Some(st.line_numbers == "relative")
                    }
                    _ => None,
                };
                let gutter = numbers.map_or(0, |_| text.len_lines().max(1).to_string().len() + 1);

                let room = (r.h * area - ROW_HEIGHT).max(ROW_HEIGHT);
                let (top, last, rows) = fit_lines(w.scroll_top.min(cur_line), cur_line, hi, room, height_of);
                fitted.push((*id, top, rows));
                let text_left = r.x * width + 8. + gutter as f32 * FONT_SIZE * CHAR_WIDTH;
                if is_selected && (top..last).contains(&cur_line) {
                    let raw = text.line(cur_line).to_string();
                    let col = layout_line(raw.trim_end_matches(['\n', '\r']), ops_of(cur_line))
                        .1
                        .get(cur_col)
                        .copied()
                        .unwrap_or(cur_col);
                    let x = text_left + col as f32 * FONT_SIZE * CHAR_WIDTH;
                    let y = r.y * area + (top..cur_line).map(height_of).sum::<f32>();
                    cursor_at = Some((x, y, height_of(cur_line)));
                }

                let content = if let Some(review) = diffs.get(&w.buffer) {
                    Content::Review(review.clone())
                } else if let Some((path, kind)) = media {
                    Content::Media { path, kind, themed: st.pdf_themed, scroll: st.media_scroll.get(&w.buffer).cloned().unwrap_or_default(),
                        zoom: st.media_zoom.get(&w.buffer).copied().unwrap_or(1.),
                    }
                } else if rendered.contains(id) {
                    Content::Markdown {
                        source: text.to_string(),
                        revision: buffer.revision(),
                        cursor_line: text.char_to_line(point.min(buffer.len())),
                        folder: buffer.file().and_then(|f| f.parent()).map(|d| d.to_path_buf()).unwrap_or_default(),
                    }
                } else {
                    let marks = review_marks.get(id).map(Vec::as_slice).unwrap_or(&[]);
                    let comments = comment_marks.get(id).map(Vec::as_slice).unwrap_or(&[]);
                    let git_signs = git_marks.get(id);
                    let blame = blame_at.as_ref().filter(|(w, _, _)| w == id && !minibuffer_open);
                    let bands = band_marks.get(id);
                    // A line's box, joined with the same box on the lines around it.
                    let band_of = |i: usize| {
                        let bands = bands?;
                        let b = bands.get(&i)?;
                        let same = |j: usize| bands.get(&j) == Some(b);
                        Some(crate::frame::Band {
                            from: b.0,
                            to: b.1,
                            kind: b.2.clone(),
                            first: i == 0 || !same(i - 1),
                            last: !same(i + 1),
                        })
                    };
                    let mut y = r.y * area;
                    let lines = (top..last)
                        .map(|i| {
                            let raw = text.line(i).to_string();
                            let (line, cols) = layout_line(raw.trim_end_matches(['\n', '\r']), ops_of(i));
                            let to_screen = |col: usize| cols.get(col).copied().unwrap_or(col);
                            let start = text.line_to_char(i);
                            // The selected range touching this line (a block has one per line);
                            // it may run past the line's end, over its line break.
                            let line_len = text.line(i).len_chars();
                            let touches = |&&(a, b): &&(usize, usize)| a < start + line_len.max(1) && b > start;
                            let selected = selection
                                .iter()
                                .filter(|_| is_selected)
                                .find(touches)
                                .map(|&(a, b)| (to_screen(a.saturating_sub(start)), to_screen(b.saturating_sub(start))));
                            let line_diagnostics = diagnostics
                                .iter()
                                .filter(|&&(a, b, _)| a < start + line_len.max(1) && b > start)
                                .map(|&(a, b, s)| (to_screen(a.saturating_sub(start)), to_screen(b.saturating_sub(start)), s))
                                .collect();
                            // The number beside it: relative numbers count from the
                            // cursor (the current line its own).
                            let number = numbers.map(|relative| {
                                let n = if relative && i != cur_line { i.abs_diff(cur_line) } else { i + 1 };
                                if i + 1 == text.len_lines() && line_len == 0 {
                                    String::new()
                                } else {
                                    format!("{n:>width$} ", width = gutter - 1)
                                }
                            });
                            let height = height_of(i);
                            let out = FrameLine {
                                line: i,
                                top: y,
                                height,
                                matches: match_columns(&line, search.as_ref()),
                                cursor: (is_selected && !is_sidebar && i == cur_line && !minibuffer_open)
                                    .then_some(to_screen(cur_col)),
                                selected,
                                styles: styles[i - lo].iter().map(|(a, b, f)| (to_screen(*a), to_screen(*b), f.clone())).collect(),
                                diagnostics: line_diagnostics,
                                number,
                                number_current: i == cur_line,
                                current: is_sidebar && i == cur_line,
                                added: marks.contains(&(i, true)),
                                deleted_above: marks.contains(&(i, false)),
                                comment: comments.contains(&i),
                                git_sign: git_signs.and_then(|g| g.get(&i)).copied().unwrap_or(0),
                                annotation: blame.filter(|(_, l, _)| *l == i).map(|(_, _, t)| t.clone()),
                                // Markdown code blocks: a band across the window.
                                code_block: language == Some("markdown")
                                    && spans
                                        .get(i.wrapping_sub(lo))
                                        .is_some_and(|l| l.iter().any(|sp| sp.capture == "markup.raw.block")),
                                band: band_of(i),
                                // 0, 1, 2... is left for the window to know.
                                cols: if cols.len() == line.chars().count() + 1 && cols.iter().enumerate().all(|(i, c)| i == *c) {
                                    Vec::new()
                                } else {
                                    cols
                                },
                                text: line,
                            };
                            y += height;
                            out
                        })
                        .collect();
                    Content::Text(lines)
                };
                if is_custom {
                    custom.insert(*id);
                }
                // The selected window's modeline comes from Lua; others show their buffer.
                let modeline = match is_selected.then(|| modeline.take()).flatten() {
                    Some(segments) => segments.into_iter().map(|s| Segment { text: s.text, face: s.face }).collect(),
                    None => vec![Segment { text: buffer.name().to_string(), face: "dim".into() }],
                };
                Some(FrameWindow {
                    id: *id,
                    buffer: w.buffer,
                    left: r.x * width,
                    top: r.y * area,
                    width: r.w * width,
                    height: r.h * area,
                    text_left,
                    selected: is_selected,
                    sidebar: is_sidebar,
                    terminal: terminal_buffers.contains(&w.buffer),
                    content,
                    modeline,
                })
            })
            .collect();

        for (id, top, rows) in fitted {
            if let Some(w) = st.window_by_id_mut(id) {
                w.scroll_top = top;
                w.rows = rows.max(1);
            }
        }
        // Columns of monospace text each window fits (terminals size to it).
        for (id, r) in &rects {
            if let Some(w) = st.window_by_id_mut(*id) {
                w.cols = ((r.w * width - 16.) / (FONT_SIZE * CHAR_WIDTH)).max(10.) as usize;
            }
        }
        syntax.retain(|id| st.buffer_by_id(id).is_some());

        // Completions, under the word being typed: the rows that fit, scrolled.
        let completion = st.completion.as_ref().filter(|_| cursor_at.is_some()).map(|c| {
            let rows = COMPLETION_ROWS.min(c.len());
            let first = c.selected().saturating_sub(rows.saturating_sub(1)).min(c.len() - rows);
            c.items()
                .enumerate()
                .skip(first)
                .take(rows)
                .map(|(i, item)| CompletionRow {
                    label: item.label.clone(),
                    detail: item.detail.clone(),
                    kind: item.kind.to_string(),
                    selected: i == c.selected(),
                })
                .collect()
        });
        let picker = st.picker.as_ref().map(|p| {
            let panel_height = (height * 0.72).max(ROW_HEIGHT * 8.);
            // Title, query and their borders take about three rows.
            let rows = ((panel_height / ROW_HEIGHT) as usize).saturating_sub(3).max(1);
            let matches = p.matches();
            let first = p.selected().saturating_sub(rows / 2).min(matches.len().saturating_sub(rows));
            let mb = &st.minibuffer;
            PickerView {
                title: p.title().to_string(),
                matched: matches.len(),
                total: p.total(),
                query: display_line(mb.text().slice(..)),
                cursor: mb.column(),
                panel_height,
                rows: matches
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(rows)
                    .map(|(i, m)| (m.item.clone(), m.positions.clone(), i == p.selected()))
                    .collect(),
                preview: p.preview().map(|(lines, mark)| (lines.into_iter().take(rows).collect(), mark)),
            }
        });
        // The picker draws its own query line; the echo area keeps the message.
        let echo = match &st.minibuffer_prompt {
            Some(prompt) if picker.is_none() => {
                let mb = &st.minibuffer;
                Echo::Prompt { prompt: prompt.clone(), text: display_line(mb.text().slice(..)), cursor: mb.column() }
            }
            _ if st.message.is_empty() && st.minibuffer_prompt.is_none() => {
                // With nothing else to say, the diagnostic under the cursor.
                let point = st.buffer().point();
                let here = st.buffer_diagnostics().into_iter().find(|((a, b), _)| point >= *a && point < *b);
                match here {
                    Some((_, d)) => Echo::Diagnostic {
                        severity: d.severity,
                        label: d.severity_name().to_string(),
                        message: d.message.lines().next().unwrap_or("").to_string(),
                    },
                    None => Echo::Empty,
                }
            }
            _ => Echo::Message(st.message.clone()),
        };
        // The Markdown preview on the right.
        let preview_target = st.preview_buffer();
        if preview_target.is_some() {
            st.preview_last = preview_target;
        }
        let preview = st.preview_rect().zip(preview_target).map(|(r, id)| {
            let buffer = st.buffer_by_id(id);
            PreviewPane {
                left: r.x * width,
                width: r.w * width,
                buffer: id,
                source: buffer.map(|b| b.text().to_string()).unwrap_or_default(),
                // Images by path load from the file's folder (README's assets/...).
                folder: buffer.and_then(|b| b.file()).and_then(|f| f.parent()).map(|d| d.to_path_buf()).unwrap_or_default(),
            }
        });
        let frame = Frame {
            area,
            font: st.font.clone(),
            bar,
            theme: ThemeRequest { mode: st.theme_mode.clone(), light: st.theme_name(false), dark: st.theme_name(true) },
            windows,
            cursor_at,
            echo,
            modal: st.picker.is_some() || minibuffer_open,
            hover: st.hover.clone().filter(|_| cursor_at.is_some()),
            completion,
            whichkey: st.whichkey.clone(),
            picker,
            preview,
            buffers: st.buffer_ids(),
            sessions: Vec::new(),
            session: String::new(),
        };
        drop(st);
        self.custom = custom;
        frame
    }
}

/// How much each line of Ctrl and the wheel zooms an image or PDF.
const WHEEL_ZOOM: f32 = 1.05;

fn is_terminal(st: &crate::editor::EditorState, buffer: BufferId) -> bool {
    st.terminals.iter().any(|t| t.buffer == buffer)
}

fn mouse_mods(m: Mods) -> crate::terminal::MouseMods {
    crate::terminal::MouseMods { shift: m.shift, alt: m.alt, control: m.control }
}

/// A face by name: its spec from `(face-specs)` if it has one, else a color.
fn resolve_face(face: &str, specs: &[FaceSpec]) -> Face {
    match specs.iter().find(|s| s.name == face) {
        Some(spec) => Face {
            color: spec.color.clone(),
            background: spec.background.clone(),
            scale: spec.scale.unwrap_or(1.),
            bold: spec.bold,
            italic: spec.italic,
            underline: spec.underline,
            sans: spec.sans,
        },
        None => Face {
            color: Some(face.to_string()),
            background: None,
            scale: 1.,
            bold: false,
            italic: false,
            underline: false,
            sans: false,
        },
    }
}

/// The tallest text on a line, as a multiple of the normal size.
fn line_scale(styles: &[(usize, usize, Face)]) -> f32 {
    styles.iter().map(|s| s.2.scale).fold(1., f32::max)
}

/// A line's length in characters, without its line break.
fn text_line_len(text: &ropey::Rope, line: usize) -> usize {
    text.line(line).to_string().trim_end_matches(['\n', '\r']).chars().count()
}

/// Column ranges where the search pattern matches in a line of text.
fn match_columns(text: &str, re: Option<&regex::Regex>) -> Vec<(usize, usize)> {
    let Some(re) = re else { return vec![] };
    let col = |byte: usize| text[..byte].chars().count();
    re.find_iter(text).filter(|m| !m.is_empty()).map(|m| (col(m.start()), col(m.end()))).collect()
}

/// Display columns of a line's characters: a tab takes four.
/// `map[col]` is where character `col` starts on screen; one extra entry
/// covers the position after the last character.
pub fn display_columns(raw: &str) -> Vec<usize> {
    let mut map = Vec::with_capacity(raw.len() + 2);
    let mut at = 0;
    for ch in raw.chars() {
        map.push(at);
        at += if ch == '\t' { 4 } else { 1 };
    }
    map.push(at);
    map.push(at + 1);
    map
}

/// A line as shown, and each character's screen column (and one past the
/// end): tabs as four spaces, and Markdown's hidden markup (`ops`: column,
/// replacement or None to hide) taking no room.
fn layout_line(raw: &str, ops: &[(usize, Option<char>)]) -> (String, Vec<usize>) {
    if ops.is_empty() {
        return (raw.replace('\t', "    "), display_columns(raw));
    }
    let mut text = String::with_capacity(raw.len());
    let mut map = Vec::with_capacity(raw.len() + 2);
    let mut at = 0;
    let mut op = ops.iter().peekable();
    for (i, ch) in raw.chars().enumerate() {
        map.push(at);
        while op.peek().is_some_and(|o| o.0 < i) {
            op.next();
        }
        match op.peek().filter(|o| o.0 == i).map(|o| o.1) {
            Some(None) => {}
            Some(Some(r)) => {
                text.push(r);
                at += 1;
            }
            None if ch == '\t' => {
                text.push_str("    ");
                at += 4;
            }
            None => {
                text.push(ch);
                at += 1;
            }
        }
    }
    map.push(at);
    map.push(at + 1);
    (text, map)
}

fn display_line(rope_line: ropey::RopeSlice<'_>) -> String {
    rope_line.to_string().trim_end_matches(['\n', '\r']).replace('\t', "    ")
}

/// Which lines fill `room` pixels: scrolls down from `top` until the
/// cursor's line fits, then takes lines while they fit (at least one), up to
/// `end`. Returns the first and one-past-last lines, and the row capacity.
fn fit_lines(mut top: usize, cursor: usize, end: usize, room: f32, height: impl Fn(usize) -> f32) -> (usize, usize, usize) {
    while top < cursor && (top..=cursor).map(&height).sum::<f32>() > room {
        top += 1;
    }
    let mut last = top;
    let mut used = 0.;
    while last < end && (last == top || used + height(last) <= room) {
        used += height(last);
        last += 1;
    }
    // Blank space below a short buffer still fits terminal rows. Counting
    // only drawn lines would resize a newly opened PTY to one or two rows.
    let rows = last - top + ((room - used).max(0.) / ROW_HEIGHT) as usize;
    (top, last, rows.max(1))
}

/// Text rows a window of this size shows: its height less its modeline.
fn window_rows(r: &Rect, area_height: f32) -> usize {
    ((r.h * area_height / ROW_HEIGHT) as usize).saturating_sub(1).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn session_with(text: &str) -> Session {
        let mut buf = Buffer::new("test");
        buf.insert(text);
        buf.goto_char(0);
        let editor = Editor::new(buf);
        Session::new(editor.clone(), Lisp::new(editor))
    }

    fn key(s: &mut Session, k: &str) {
        let text = if k.chars().count() == 1 { k } else { "" };
        s.handle(Input::Key { key: k.into(), text: text.into() });
    }

    const VIEW: Viewport = Viewport { width: 800., height: 600. };

    fn lines(frame: &Frame) -> &[FrameLine] {
        match &frame.windows[0].content {
            Content::Text(lines) => lines,
            other => panic!("not text: {other:?}"),
        }
    }

    #[test]
    fn the_agent_prompt_is_drawn_in_one_box() {
        let mut s = session_with("");
        s.editor.lock().agent_show();
        let frame = s.frame(VIEW);
        let lines = frame
            .windows
            .iter()
            .find_map(|w| match &w.content {
                Content::Text(lines) if lines.iter().any(|l| l.text.contains("SPC a s")) => Some(lines),
                _ => None,
            })
            .expect("a window on *agent*");
        let boxed: Vec<_> = lines.iter().filter_map(|l| Some((l.text.as_str(), l.band.clone()?))).collect();
        let [(hint, top), (typed, bottom)] = &boxed[..] else { panic!("{boxed:?}") };
        assert!(hint.starts_with("Ask, or @"), "{hint}");
        assert_eq!(*typed, "");
        assert_eq!((top.kind.as_str(), top.first, top.last), ("input", true, false));
        assert_eq!((bottom.kind.as_str(), bottom.first, bottom.last), ("input", false, true));
        assert!(lines.iter().filter(|l| l.band.is_none()).any(|l| l.text.contains("SPC a s")), "the keys to start with");
    }

    #[test]
    fn git_signs_and_blame_reach_the_frame() {
        let dir = std::env::temp_dir().join(format!("vmac-session-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "Jane"],
            &["config", "commit.gpgsign", "false"],
        ] {
            assert!(crate::git::run(&dir, args, None).0);
        }
        std::fs::write(dir.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        assert!(crate::git::run(&dir, &["add", "."], None).0);
        assert!(crate::git::run(&dir, &["commit", "-qm", "Count to three"], None).0);
        std::fs::write(dir.join("f.txt"), "one\nTWO\nthree\nfour\n").unwrap();

        let mut s = session_with("");
        s.editor.lock().open_file(&dir.join("f.txt")).unwrap();
        // Blame waits for the buffer to sit still, then runs in the background.
        let start = std::time::Instant::now();
        while s.editor.lock().git_blame_here().is_none() && start.elapsed().as_secs() < 10 {
            s.editor.lock().git_pump();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let frame = s.frame(VIEW);
        let lines = lines(&frame);
        let signs: Vec<u8> = lines.iter().take(4).map(|l| l.git_sign).collect();
        assert_eq!(signs, vec![0, 2, 0, 1], "unchanged, changed, unchanged, added");
        let note = lines[0].annotation.as_deref().unwrap_or("");
        assert!(note.starts_with("Jane, just now · Count to three"), "{note:?}");
        assert!(lines[1].annotation.is_none(), "only the cursor's line");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn images_and_pdfs_are_left_for_the_window_to_draw() {
        use crate::frame::MediaKind;
        let mut s = session_with("");
        let path = std::env::temp_dir().join(format!("vmac-session-{}.pdf", std::process::id()));
        std::fs::write(&path, b"%PDF-1.4").unwrap();
        s.editor.lock().open_file(&path).unwrap();
        let frame = s.frame(VIEW);
        match &frame.windows[0].content {
            Content::Media { path: shown, kind, .. } => {
                assert_eq!((shown.as_path(), *kind), (std::path::absolute(&path).unwrap().as_path(), MediaKind::Pdf))
            }
            other => panic!("not media: {other:?}"),
        }
        std::fs::remove_file(path).ok();
    }

    /// A three-paragraph LaTeX file (paragraphs on lines 3, 5 and 7) built
    /// with SyncTeX in its own folder, or None without pdflatex and synctex.
    fn latex_sample(tag: &str) -> Option<std::path::PathBuf> {
        let have = |c: &str| std::process::Command::new(c).arg("--version").output().is_ok();
        if !have("pdflatex") || !have("synctex") {
            eprintln!("skipped: needs pdflatex and synctex");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("vmac-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tex = dir.join("paper.tex");
        std::fs::write(
            &tex,
            "\\documentclass{article}\n\\begin{document}\nFirst paragraph here.\n\nSecond paragraph, a bit longer.\n\nThird paragraph is the one we jump to.\n\\end{document}\n",
        )
        .unwrap();
        let built = std::process::Command::new("pdflatex")
            .args(["-synctex=1", "-interaction=nonstopmode", "paper.tex"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(dir.join("paper.synctex.gz").exists(), "built: {}", String::from_utf8_lossy(&built.stdout));
        Some(dir)
    }

    #[test]
    fn spc_m_s_shows_the_cursors_line_in_the_pdf() {
        use crate::frame::MediaScroll;
        let Some(dir) = latex_sample("forward") else { return };
        let tex = dir.join("paper.tex");
        let mut s = session_with("");
        s.editor.lock().open_file(&tex).unwrap();
        for k in ["7", "G", "SPC", "m", "s"] {
            key(&mut s, k);
        }
        let frame = s.frame(VIEW);
        assert_eq!(frame.windows.len(), 2, "the PDF opened beside it");
        assert_eq!(s.editor.lock().buffer().name(), "paper.tex", "the cursor stays in the source");
        let reveal = match &frame.windows[1].content {
            Content::Media { scroll, .. } => scroll.last().map(|(_, how)| *how),
            other => panic!("not the PDF: {other:?}"),
        };
        match reveal {
            // Line 7's paragraph: page 1, its baseline near 160 points down.
            Some(MediaScroll::Reveal { page: 0, y, height, width, .. }) => {
                assert!((140. ..165.).contains(&y) && height > 0. && width > 0., "{y} {width} {height}")
            }
            other => panic!("not revealed: {other:?}"),
        }
        assert_eq!(s.editor.lock().message, "Page 1");

        // Again, with the PDF showing: no second window.
        for k in ["g", "g", "SPC", "m", "s"] {
            key(&mut s, k);
        }
        assert_eq!(s.frame(VIEW).windows.len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_pdf_follows_the_cursor_once_it_rests() {
        use crate::frame::MediaScroll;
        let Some(dir) = latex_sample("follow") else { return };
        let mut s = session_with("");
        s.editor.lock().open_file(&dir.join("paper.tex")).unwrap();
        let reveals = |s: &mut Session| -> Vec<(f32, bool)> {
            let frame = s.frame(VIEW);
            let Some(Content::Media { scroll, .. }) = frame.windows.get(1).map(|w| &w.content) else { return Vec::new() };
            scroll.iter().filter_map(|(_, how)| match how {
                MediaScroll::Reveal { y, follow, .. } => Some((*y, *follow)),
                _ => None,
            }).collect()
        };
        let ticks = |s: &mut Session, n: usize| (0..n).for_each(|_| { s.pump_servers(); });

        // The PDF closed: nothing to follow.
        for k in ["7", "G"] {
            key(&mut s, k);
        }
        ticks(&mut s, 5);
        assert!(reveals(&mut s).is_empty());

        // Beside it: resting on line 7 shows it there, following (only if out of view).
        for k in ["SPC", "m", "p"] {
            key(&mut s, k);
        }
        ticks(&mut s, 5);
        let found = reveals(&mut s);
        assert_eq!(found.len(), 1, "once, not every tick: {found:?}");
        assert!((140. ..165.).contains(&found[0].0) && found[0].1, "{found:?}");

        // Moving through lines without resting: no search for each.
        for k in ["k", "k", "k"] {
            key(&mut s, k);
            ticks(&mut s, 1);
        }
        assert_eq!(reveals(&mut s).len(), 1);
        ticks(&mut s, 5);
        assert_eq!(reveals(&mut s).len(), 2, "rested on line 4: shown");

        // Off: it stays put.
        for k in ["SPC", "t", "s", "3", "G"] {
            key(&mut s, k);
        }
        ticks(&mut s, 5);
        assert_eq!(reveals(&mut s).len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_double_click_in_a_latex_pdf_goes_to_its_source_line() {
        let Some(dir) = latex_sample("synctex") else { return };
        let tex = dir.join("paper.tex");
        let mut s = session_with("");
        s.editor.lock().open_file(&tex).unwrap();
        s.editor.lock().toggle_file_beside(&dir.join("paper.pdf")).unwrap();
        let frame = s.frame(VIEW);
        assert_eq!(frame.windows.len(), 2);
        let pdf_window = frame.windows[1].id;
        let at = |s: &Session| {
            let st = s.editor.lock();
            (st.buffer().name().to_string(), st.buffer().line() + 1, st.window_count())
        };

        // Line 7's paragraph, about 158 points down page 1 (synctex view says).
        s.handle(Input::PdfDoubleClick { window: pdf_window, page: 1, x: 180., y: 158. });
        assert_eq!(at(&s), ("paper.tex".into(), 7, 2), "in the source's window, on its line");
        s.handle(Input::PdfDoubleClick { window: pdf_window, page: 1, x: 150., y: 130. });
        assert_eq!(at(&s), ("paper.tex".into(), 3, 2), "the first paragraph");

        // The PDF alone: the source opens beside it.
        s.editor.lock().select_window(pdf_window);
        s.editor.lock().only_window();
        s.handle(Input::PdfDoubleClick { window: pdf_window, page: 1, x: 180., y: 158. });
        assert_eq!(at(&s), ("paper.tex".into(), 7, 2));

        // Nowhere near any text: said so, and nothing moves.
        s.handle(Input::PdfDoubleClick { window: pdf_window, page: 9, x: 1., y: 1. });
        assert!(s.editor.lock().message.starts_with("No source found"), "{}", s.editor.lock().message);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn ctrl_and_the_wheel_zoom_an_image() {
        let mut s = session_with("");
        let path = std::env::temp_dir().join(format!("vmac-wheel-{}.png", std::process::id()));
        std::fs::write(&path, b"\x89PNG").unwrap();
        s.editor.lock().open_file(&path).unwrap();
        let window = s.frame(VIEW).windows[0].id;
        let mods = Mods { control: true, ..Default::default() };
        s.handle(Input::Wheel { window, lines: -1, line: 0, col: 0, mods });
        match &s.frame(VIEW).windows[0].content {
            Content::Media { zoom, .. } => assert!((zoom - WHEEL_ZOOM).abs() < 1e-4),
            other => panic!("not media: {other:?}"),
        }
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn ctrl_and_the_wheel_zoom_a_pdf() {
        let mut s = session_with("");
        let path = std::env::temp_dir().join(format!("vmac-wheel-{}.pdf", std::process::id()));
        std::fs::write(&path, b"%PDF-1.4").unwrap();
        s.editor.lock().open_file(&path).unwrap();
        let window = s.frame(VIEW).windows[0].id;
        let zoom = |s: &mut Session| match &s.frame(VIEW).windows[0].content {
            Content::Media { zoom, .. } => *zoom,
            other => panic!("not media: {other:?}"),
        };
        let wheel = |s: &mut Session, lines: isize, control: bool| {
            let mods = Mods { control, ..Default::default() };
            s.handle(Input::Wheel { window, lines, line: 0, col: 0, mods });
        };
        wheel(&mut s, -2, true);
        assert!((zoom(&mut s) - WHEEL_ZOOM.powi(2)).abs() < 1e-4, "up zooms in: {}", zoom(&mut s));
        wheel(&mut s, 4, true);
        assert!((zoom(&mut s) - WHEEL_ZOOM.powi(-2)).abs() < 1e-4, "down zooms out: {}", zoom(&mut s));
        let before = zoom(&mut s);
        wheel(&mut s, 3, false);
        assert_eq!(zoom(&mut s), before, "without Ctrl the wheel doesn't zoom");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn keys_in_and_a_frame_out_without_a_window() {
        let mut s = session_with("one\ntwo\nthree\n");
        let frame = s.frame(VIEW);
        assert_eq!(frame.windows.len(), 1);
        let shown: Vec<&str> = lines(&frame).iter().map(|l| l.text.as_str()).collect();
        assert_eq!(shown, ["one", "two", "three", ""]);
        assert_eq!(lines(&frame)[0].cursor, Some(0));

        for k in ["j", "l", "x"] {
            key(&mut s, k);
        }
        let frame = s.frame(VIEW);
        assert_eq!(lines(&frame)[1].text, "to", "jlx deleted the w");
        assert_eq!(lines(&frame)[1].cursor, Some(1));
        assert_eq!(lines(&frame)[0].cursor, None);
        assert!(!frame.modal);

        key(&mut s, ":");
        let frame = s.frame(VIEW);
        assert!(frame.modal, "the : prompt has the keyboard");
        assert!(matches!(frame.echo, Echo::Prompt { .. }), "{:?}", frame.echo);
    }

    #[test]
    fn a_click_moves_the_cursor_and_a_drag_selects() {
        let mut s = session_with("hello world\nsecond line\n");
        let frame = s.frame(VIEW);
        let window = frame.windows[0].id;
        let mods = Mods::default();
        s.handle(Input::MouseDown { window, button: 0, line: 1, col: 3, ch: 3, clicks: 1, mods });
        assert_eq!(s.editor.lock().buffer().point(), 12 + 3);
        s.handle(Input::MouseDrag { window, line: 1, col: 6, ch: 6, mods });
        s.handle(Input::MouseUp { window, line: 1, col: 6, mods });
        let frame = s.frame(VIEW);
        assert_eq!(lines(&frame)[1].selected, Some((3, 7)), "the drag selected 'ond '");
    }

    #[test]
    fn frames_survive_being_sent() {
        let mut s = session_with("fn main() {}\n");
        let frame = s.frame(VIEW);
        let json = serde_json::to_string(&frame).unwrap();
        assert_eq!(serde_json::from_str::<Frame>(&json).unwrap(), frame);
        let input = Input::Key { key: "SPC".into(), text: " ".into() };
        assert_eq!(serde_json::from_str::<Input>(&serde_json::to_string(&input).unwrap()).unwrap(), input);
    }

    #[test]
    fn tall_lines_scroll_the_cursor_into_view() {
        // Line 0 is a heading twice as tall; room for 4 normal rows.
        let h = |i: usize| if i == 0 { 2. * ROW_HEIGHT } else { ROW_HEIGHT };
        let room = 4. * ROW_HEIGHT;
        assert_eq!(fit_lines(0, 1, 10, room, h), (0, 3, 3), "heading + 2 lines fit");
        assert_eq!(fit_lines(0, 3, 10, room, h), (1, 5, 4), "the cursor on line 3 scrolls the heading off");
        assert_eq!(fit_lines(0, 0, 1, room, h), (0, 1, 3), "stops drawing at the buffer's end, with room below the heading");
        let huge = |_| 10. * ROW_HEIGHT;
        assert_eq!(fit_lines(0, 0, 10, room, huge), (0, 1, 1), "always at least one line");
    }

    #[test]
    fn an_empty_terminal_keeps_the_panes_full_height() {
        // The first frame has one empty buffer line, but space for 40 rows.
        // Feeding the content count back to the PTY used to shrink it to 2.
        let (top, last, rows) = fit_lines(0, 0, 1, 800., |_| ROW_HEIGHT);
        assert_eq!((top, last), (0, 1), "only existing buffer lines are drawn");
        let mut st = crate::editor::EditorState::new(Buffer::new("*agent*"));
        let id = st.selected_window();
        st.fit_window(id, rows);
        assert_eq!(st.selected_size().0, 40, "the PTY must get the pane's capacity");

        // Output arriving and a later pane resize must preserve the same rule.
        for (lines, room, expected) in [(2, 800., 40), (40, 800., 40), (2, 200., 10)] {
            let (_, _, rows) = fit_lines(0, 0, lines, room, |_| ROW_HEIGHT);
            st.fit_window(id, rows);
            assert_eq!(st.selected_size().0, expected);
        }
    }

    #[test]
    fn tabs_take_four_columns() {
        assert_eq!(display_columns("a\tb"), vec![0, 1, 5, 6, 7]);
        // Hidden markup takes no room; a replacement takes one column.
        let (text, cols) = layout_line(
            "## **Hi**\t-",
            &[(0, None), (1, None), (2, None), (3, None), (4, None), (7, None), (8, None), (10, Some('•'))],
        );
        assert_eq!(text, "Hi    •");
        assert_eq!(cols, vec![0, 0, 0, 0, 0, 0, 1, 2, 2, 2, 6, 7, 8]);
        assert_eq!(layout_line("a\tb", &[]), ("a    b".to_string(), display_columns("a\tb")));
    }

    #[test]
    fn match_columns_are_in_characters() {
        let re = regex::Regex::new("b+").unwrap();
        assert_eq!(match_columns("éabbxb", Some(&re)), vec![(2, 4), (5, 6)]);
        assert_eq!(match_columns("abc", None), vec![]);
    }
}
