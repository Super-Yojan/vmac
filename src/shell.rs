//! The window: draws each editor window (its buffer and modeline, laid out
//! by the split tree), the echo area and the picker, and sends every
//! keystroke to Scheme. It decides nothing about what keys do.

use crate::buffer::Buffer;
use crate::editor::{Editor, Rect};
use crate::lisp::{FaceSpec, Lisp, Segment};
use crate::picker::Picker;
use crate::syntax::{Syntax, face_for, language_for_path};
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Context, prelude::FluentBuilder as _, FocusHandle, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent,
    Keystroke, ParentElement as _, Render, Styled as _, Window, div, px,
};
use std::time::Duration;

const ROW_HEIGHT: f32 = 20.;
const FONT_SIZE: f32 = 14.;
const RELOAD_POLL: Duration = Duration::from_millis(400);
/// How often language servers are synced and read.
const LSP_POLL: Duration = Duration::from_millis(100);
/// How wide the sidebar (the file tree) is.
const SIDEBAR_WIDTH: f32 = 260.;
/// A monospace character's width, as a share of the font size (Menlo, SF Mono).
const CHAR_WIDTH: f32 = 0.6;
/// The documentation popup K shows.
const HOVER_WIDTH: f32 = 560.;
const HOVER_MAX_HEIGHT: f32 = 320.;
/// The completion popup.
const COMPLETION_WIDTH: f32 = 460.;
const COMPLETION_ROWS: usize = 10;

pub struct Shell {
    editor: Editor,
    lisp: Lisp,
    focus: FocusHandle,
    syntax: Syntax,
}

impl Shell {
    pub fn new(editor: Editor, lisp: Lisp, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self::watch_sources(cx);
        Self::drive_language_servers(cx);
        Self { editor, lisp, focus, syntax: Syntax::default() }
    }

    /// Reloads lisp/core/ and init.scm when they change on disk.
    fn watch_sources(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(RELOAD_POLL).await;
                let alive = this.update(cx, |shell, cx| {
                    if shell.lisp.reload_if_changed() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Syncs files with their language servers and reads what they report.
    fn drive_language_servers(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(LSP_POLL).await;
                let alive = this.update(cx, |shell, cx| {
                    let configs = shell.lisp.lsp_servers();
                    let changed = shell.editor.lock().lsp_pump(configs);
                    // Replies that Scheme handles (the references picker).
                    if shell.lisp.run_events() || changed {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, text)) = key_name(&event.keystroke) else { return };
        self.lisp.dispatch_key(&key, &text);
        if self.editor.lock().quit_requested {
            cx.quit();
        }
        cx.notify();
    }
}

/// Emacs notation for a keystroke ("C-r", "ESC", "SPC", "<left>", "A") and
/// the text it would type ("" when it types nothing). None for keys that are
/// only modifiers.
pub fn key_name(ks: &Keystroke) -> Option<(String, String)> {
    let m = &ks.modifiers;
    let named = match ks.key.as_str() {
        "shift" | "control" | "alt" | "platform" | "function" | "capslock" | "" => return None,
        "enter" => Some("RET"),
        "escape" => Some("ESC"),
        "backspace" => Some("DEL"),
        "tab" => Some("TAB"),
        "space" => Some("SPC"),
        "delete" => Some("<delete>"),
        "left" => Some("<left>"),
        "right" => Some("<right>"),
        "up" => Some("<up>"),
        "down" => Some("<down>"),
        "home" => Some("<home>"),
        "end" => Some("<end>"),
        "pageup" => Some("<prior>"),
        "pagedown" => Some("<next>"),
        _ => None,
    };
    let typing = !(m.control || m.alt || m.platform || m.function);
    let mut prefix = String::new();
    if m.control {
        prefix.push_str("C-");
    }
    if m.alt {
        prefix.push_str("M-");
    }
    if m.platform {
        prefix.push_str("s-");
    }
    if let Some(name) = named {
        if m.shift {
            prefix.push_str("S-");
        }
        let text = if typing && name == "SPC" { " " } else { "" };
        return Some((format!("{prefix}{name}"), text.to_string()));
    }
    if typing && let Some(c) = ks.key_char.as_ref().filter(|c| !c.is_empty()) {
        return Some((c.clone(), c.clone()));
    }
    let key = if m.shift { ks.key.to_uppercase() } else { ks.key.clone() };
    Some((format!("{prefix}{key}"), String::new()))
}

fn face_color(face: &str, cx: &App) -> Hsla {
    let t = cx.theme();
    match face {
        "muted" => t.muted,
        "strong" => t.foreground,
        "dim" => t.muted_foreground,
        "accent" => t.primary,
        "error" => t.danger,
        "red" => t.red,
        "green" => t.green,
        "blue" => t.blue,
        "yellow" => t.yellow,
        "magenta" => t.magenta,
        "cyan" => t.cyan,
        _ => t.foreground,
    }
}

/// A diagnostic's color by severity: error, warning, information, hint.
fn severity_color(severity: u8, cx: &App) -> Hsla {
    let t = cx.theme();
    match severity {
        1 => t.danger,
        2 => t.warning,
        3 => t.info,
        _ => t.muted_foreground,
    }
}

/// How a run of text looks: a face resolved against the theme.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Style {
    color: Option<Hsla>,
    background: Option<Hsla>,
    scale: f32,
    bold: bool,
    italic: bool,
    underline: bool,
    sans: bool,
}

impl Style {
    fn color(color: Hsla) -> Self {
        Self { color: Some(color), background: None, scale: 1., bold: false, italic: false, underline: false, sans: false }
    }
}

/// A face by name: its spec from `(face-specs)` if it has one, else a color.
fn resolve_style(face: &str, specs: &[FaceSpec], cx: &App) -> Style {
    match specs.iter().find(|s| s.name == face) {
        Some(spec) => Style {
            color: spec.color.as_deref().map(|c| face_color(c, cx)),
            background: spec.background.as_deref().map(|c| face_color(c, cx)),
            scale: spec.scale.unwrap_or(1.),
            bold: spec.bold,
            italic: spec.italic,
            underline: spec.underline,
            sans: spec.sans,
        },
        None => Style::color(face_color(face, cx)),
    }
}

/// The tallest text on a line, as a multiple of the normal size.
fn line_scale(styles: &[(usize, usize, Style)]) -> f32 {
    styles.iter().map(|s| s.2.scale).fold(1., f32::max)
}

/// How one character cell is drawn.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Cell {
    selected: bool,
    matched: bool,
    cursor: bool,
    style: Option<Style>,
    /// A diagnostic's squiggle color.
    diagnostic: Option<Hsla>,
}

/// Column ranges where the search pattern matches in a line of text.
fn match_columns(text: &str, re: Option<&regex::Regex>) -> Vec<(usize, usize)> {
    let Some(re) = re else { return vec![] };
    let col = |byte: usize| text[..byte].chars().count();
    re.find_iter(text)
        .filter(|m| !m.is_empty())
        .map(|m| (col(m.start()), col(m.end())))
        .collect()
}

/// One line of text. `cursor` is a column; `selected` a column range, which
/// may include the line break (one past the last character), drawn as a
/// space like Vim does.
/// What one line shows, all in screen columns.
#[derive(Default)]
struct LineView {
    text: String,
    cursor: Option<usize>,
    /// The selected columns; may include the line break (one past the end).
    selected: Option<(usize, usize)>,
    /// Search matches.
    matches: Vec<(usize, usize)>,
    styles: Vec<(usize, usize, Style)>,
    /// Diagnostic squiggles.
    diagnostics: Vec<(usize, usize, Hsla)>,
}

impl LineView {
    /// Just text with a cursor (the minibuffer, the picker's query).
    fn input(text: String, cursor: usize) -> Self {
        Self { text, cursor: Some(cursor), ..Default::default() }
    }
}

fn line_element(view: LineView, bar: bool, cx: &App) -> impl IntoElement {
    let LineView { text, cursor, selected, matches, styles, diagnostics } = view;
    let height = ROW_HEIGHT * line_scale(&styles);
    let t = cx.theme();
    let chars: Vec<char> = text.chars().collect();
    let in_sel = |col: usize| selected.is_some_and(|(a, b)| col >= a && col < b);
    let in_match = |col: usize| matches.iter().any(|&(a, b)| col >= a && col < b);
    // Later styles win: Scheme's faces over syntax.
    let style = |col: usize| styles.iter().rev().find(|&&(a, b, _)| col >= a && col < b).map(|s| s.2);
    let cell = |col: usize| Cell {
        selected: in_sel(col),
        matched: in_match(col),
        cursor: cursor == Some(col),
        style: style(col),
        diagnostic: diagnostics.iter().find(|&&(a, b, _)| col >= a && col < b).map(|d| d.2),
    };
    // The cell after the last character exists when it is drawn.
    let end = chars.len();
    let extra = cell(end);
    let cols = if extra.selected || extra.matched || extra.cursor { end + 1 } else { end };

    // Runs of characters drawn the same way.
    let mut runs: Vec<(Cell, String)> = Vec::new();
    for col in 0..cols {
        let c = cell(col);
        let ch = chars.get(col).copied().unwrap_or(' ');
        match runs.last_mut() {
            Some((style, text)) if *style == c && !c.cursor => text.push(ch),
            _ => runs.push((c, ch.to_string())),
        }
    }

    let sans = t.font_family.clone();
    h_flex().h(px(height)).items_end().whitespace_nowrap().children(runs.into_iter().map(|(c, text)| {
        let st = c.style;
        let run = div()
            .when_some(st.and_then(|s| s.color), |d, color| d.text_color(color))
            .when_some(st.and_then(|s| s.background), |d, bg| d.bg(bg))
            .when_some(st.filter(|s| s.scale != 1.), |d, s| {
                d.text_size(px(FONT_SIZE * s.scale)).line_height(px(ROW_HEIGHT * s.scale))
            })
            .when(st.is_some_and(|s| s.bold), |d| d.font_weight(gpui_kit::FontWeight::BOLD))
            .when(st.is_some_and(|s| s.italic), |d| d.italic())
            .when(st.is_some_and(|s| s.underline), |d| d.underline())
            .when(st.is_some_and(|s| s.sans), |d| d.font_family(sans.clone()))
            .when_some(c.diagnostic, |d, color| {
                d.text_decoration_1().text_decoration_wavy().text_decoration_color(color)
            })
            .when(c.matched, |d| d.bg(t.warning.opacity(0.35)))
            .when(c.selected, |d| d.bg(t.selection))
            .child(text);
        match (c.cursor, bar) {
            (false, _) => run,
            (true, true) => run
                .relative()
                .child(div().absolute().left_0().top_0().w(px(2.)).h(px(height)).bg(t.caret)),
            (true, false) => run.bg(t.caret).text_color(t.background),
        }
    }))
}

/// A picker item, its matched letters in the accent color.
fn match_row(item: &str, positions: &[usize], selected: bool, cx: &App) -> impl IntoElement {
    let t = cx.theme();
    let mut runs: Vec<(bool, String)> = Vec::new();
    for (i, ch) in item.chars().enumerate() {
        let hit = positions.contains(&i);
        match runs.last_mut() {
            Some((h, text)) if *h == hit => text.push(ch),
            _ => runs.push((hit, ch.to_string())),
        }
    }
    h_flex()
        .h(px(ROW_HEIGHT))
        .px(px(8.))
        .whitespace_nowrap()
        .overflow_hidden()
        .when(selected, |d| d.bg(t.list_active))
        .child(div().w(px(16.)).text_color(t.primary).child(if selected { "▶" } else { "" }))
        .children(runs.into_iter().map(|(hit, text)| {
            div().when(hit, |d| d.text_color(t.primary).font_weight(gpui_kit::FontWeight::BOLD)).child(text)
        }))
}

/// The telescope-style panel: title and count, the query, results on the
/// left and a preview of the selected file on the right.
fn picker_element(
    picker: &Picker,
    query: &Buffer,
    bar: bool,
    window_height: f32,
    cx: &App,
) -> gpui_kit::AnyElement {
    let t = cx.theme();
    let panel_height = (window_height * 0.72).max(ROW_HEIGHT * 8.);
    // Title, query and their borders take about three rows.
    let rows = ((panel_height / ROW_HEIGHT) as usize).saturating_sub(3).max(1);
    let matches = picker.matches();
    let first = picker.selected().saturating_sub(rows / 2).min(matches.len().saturating_sub(rows));
    let results = matches.iter().enumerate().skip(first).take(rows).map(|(i, m)| {
        match_row(&m.item, &m.positions, i == picker.selected(), cx)
    });
    let preview = picker.preview();

    div().absolute().inset_0().flex().items_center().justify_center().child(
        v_flex()
            .w(gpui_kit::relative(0.84))
            .h(px(panel_height))
            .bg(t.popover)
            .text_color(t.popover_foreground)
            .border_1()
            .border_color(t.border)
            .rounded(t.radius_lg)
            .shadow_lg()
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(ROW_HEIGHT + 6.))
                    .px(px(10.))
                    .justify_between()
                    .border_b_1()
                    .border_color(t.border)
                    .child(div().text_color(t.primary).child(picker.title().to_string()))
                    .child(
                        div()
                            .text_color(t.muted_foreground)
                            .child(format!("{} / {}", matches.len(), picker.total())),
                    ),
            )
            .child(
                h_flex()
                    .h(px(ROW_HEIGHT + 6.))
                    .px(px(10.))
                    .border_b_1()
                    .border_color(t.border)
                    .child(div().text_color(t.primary).child("> "))
                    .child(line_element(LineView::input(display_line(query.text().slice(..)), query.column()), bar, cx)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        v_flex()
                            .w(gpui_kit::relative(if preview.is_some() { 0.45 } else { 1. }))
                            .h_full()
                            .overflow_hidden()
                            .children(results)
                            .when(matches.is_empty(), |d| {
                                d.child(div().px(px(10.)).text_color(t.muted_foreground).child("No matches"))
                            }),
                    )
                    .when_some(preview, |d, (lines, mark)| {
                        d.child(
                            v_flex()
                                .flex_1()
                                .h_full()
                                .px(px(10.))
                                .border_l_1()
                                .border_color(t.border)
                                .text_color(t.muted_foreground)
                                .overflow_hidden()
                                .children(lines.into_iter().take(rows).enumerate().map(|(i, l)| {
                                    // The line a reference points at stands out.
                                    let marked = mark == Some(i);
                                    div()
                                        .h(px(ROW_HEIGHT))
                                        .whitespace_nowrap()
                                        .when(marked, |d| d.bg(t.list_active).text_color(t.foreground))
                                        .child(l)
                                })),
                        )
                    }),
            ),
    )
    .into_any_element()
}

/// Display columns of a line's characters: a tab takes four.
/// `map[col]` is where character `col` starts on screen; one extra entry
/// covers the position after the last character.
fn display_columns(raw: &str) -> Vec<usize> {
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

fn display_line(rope_line: ropey::RopeSlice<'_>) -> String {
    rope_line
        .to_string()
        .trim_end_matches(['\n', '\r'])
        .replace('\t', "    ")
}

/// Which lines fill `room` pixels: scrolls down from `top` until the
/// cursor's line fits, then takes lines while they fit (at least one), up to
/// `end`. Returns the first and one-past-last lines.
fn fit_lines(mut top: usize, cursor: usize, end: usize, room: f32, height: impl Fn(usize) -> f32) -> (usize, usize) {
    while top < cursor && (top..=cursor).map(&height).sum::<f32>() > room {
        top += 1;
    }
    let mut last = top;
    let mut used = 0.;
    while last < end && (last == top || used + height(last) <= room) {
        used += height(last);
        last += 1;
    }
    (top, last)
}

/// Text rows a window of this size shows: its height less its modeline.
fn window_rows(r: &Rect, area_height: f32) -> usize {
    ((r.h * area_height / ROW_HEIGHT) as usize).saturating_sub(1).max(1)
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let (width, height): (f32, f32) = (viewport.width.into(), viewport.height.into());
        // The echo area takes the bottom row; windows share the rest.
        let area = (height - ROW_HEIGHT).max(ROW_HEIGHT * 2.);

        let bar = self.lisp.cursor_shape() == "bar";
        let selection = self.lisp.selection();
        let pattern = self.lisp.search_highlight();
        let mut modeline = Some(self.lisp.modeline());
        let mut st = self.editor.lock();
        st.sidebar_width = (SIDEBAR_WIDTH / width.max(1.)).clamp(0.12, 0.45);
        let rects = st.window_rects();
        for (id, r) in &rects {
            st.fit_window(*id, window_rows(r, area));
        }
        let search = st.search_regex(&pattern);
        let minibuffer_open = st.minibuffer_prompt.is_some();
        let selected = st.selected_window();
        let syntax_theme = self.lisp.syntax_theme();
        let syntax = &mut self.syntax;
        let theme = cx.theme();

        let face_specs = self.lisp.face_specs();
        // Where each window ends up scrolled and how many lines it shows,
        // once line heights are known (headings are taller).
        let mut fitted: Vec<(crate::editor::WindowId, usize, usize)> = Vec::new();
        // Where the cursor is on screen (x, line top, line height), for K's popup.
        let mut cursor_at: Option<(f32, f32, f32)> = None;

        let windows: Vec<_> = rects
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
                let spans = match buffer.file().and_then(|f| language_for_path(f)) {
                    Some(lang) => syntax.highlight(w.buffer, lang, buffer.revision(), text, lo..hi),
                    None => Vec::new(),
                };
                // Styles of line i, in character columns (not yet screen columns).
                let styles_of = |i: usize| -> Vec<(usize, usize, Style)> {
                    let start = text.line_to_char(i);
                    let line_len = text.line(i).len_chars();
                    let mut out: Vec<(usize, usize, Style)> = spans
                        .get(i - lo)
                        .map(|line_spans| {
                            line_spans
                                .iter()
                                .filter_map(|sp| {
                                    let face = face_for(&sp.capture, &syntax_theme)?;
                                    Some((sp.start, sp.end, resolve_style(face, &face_specs, cx)))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // Faces Scheme put on the text (the tree's folders, ...).
                    out.extend(buffer.faces().iter().filter(|f| f.0 < start + line_len && f.1 > start).map(
                        |(a, b, face)| {
                            (a.saturating_sub(start), b.saturating_sub(start), resolve_style(face, &face_specs, cx))
                        },
                    ));
                    out
                };
                let styles: Vec<_> = (lo..hi).map(styles_of).collect();
                // Language server diagnostics, as character ranges and squiggle colors.
                let diagnostics: Vec<(usize, usize, Hsla)> = buffer
                    .file()
                    .map(|f| {
                        st.lsp
                            .diagnostics(f)
                            .iter()
                            .map(|d| {
                                let (a, b) = d.to_chars(text);
                                (a, b, severity_color(d.severity, cx))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let height_of = |i: usize| ROW_HEIGHT * line_scale(&styles[i - lo]);

                let room = (r.h * area - ROW_HEIGHT).max(ROW_HEIGHT);
                let (top, last) = fit_lines(w.scroll_top.min(cur_line), cur_line, hi, room, height_of);
                fitted.push((*id, top, last - top));
                if is_selected && (top..last).contains(&cur_line) {
                    let raw = text.line(cur_line).to_string();
                    let col = display_columns(raw.trim_end_matches(['\n', '\r']))
                        .get(cur_col)
                        .copied()
                        .unwrap_or(cur_col);
                    let x = r.x * width + 8. + col as f32 * FONT_SIZE * CHAR_WIDTH;
                    let y = r.y * area + (top..cur_line).map(height_of).sum::<f32>();
                    cursor_at = Some((x, y, height_of(cur_line)));
                }

                let lines: Vec<_> = (top..last)
                    .map(|i| {
                        let raw = text.line(i).to_string();
                        let cols = display_columns(raw.trim_end_matches(['\n', '\r']));
                        let to_screen = |col: usize| cols.get(col).copied().unwrap_or(col);
                        let cursor = (is_selected && !is_sidebar && i == cur_line && !minibuffer_open)
                            .then_some(to_screen(cur_col));
                        let start = text.line_to_char(i);
                        // The selected range touching this line (a block has one per line);
                        // it may run past the line's end, over its line break.
                        let line_len = text.line(i).len_chars();
                        let selected = selection
                            .iter()
                            .filter(|_| is_selected)
                            .find(|&&(a, b)| a < start + line_len.max(1) && b > start)
                            .map(|&(a, b)| {
                                (to_screen(a.saturating_sub(start)), to_screen(b.saturating_sub(start)))
                            });
                        let line_styles = styles[i - lo]
                            .iter()
                            .map(|&(a, b, style)| (to_screen(a), to_screen(b), style))
                            .collect();
                        let line = display_line(text.line(i));
                        let matches = match_columns(&line, search.as_ref());
                        let line_diagnostics = diagnostics
                            .iter()
                            .filter(|&&(a, b, _)| a < start + line_len.max(1) && b > start)
                            .map(|&(a, b, color)| {
                                (to_screen(a.saturating_sub(start)), to_screen(b.saturating_sub(start)), color)
                            })
                            .collect();
                        let view = LineView {
                            text: line,
                            cursor,
                            selected,
                            matches,
                            styles: line_styles,
                            diagnostics: line_diagnostics,
                        };
                        let row = line_element(view, bar, cx);
                        let current = is_sidebar && i == cur_line;
                        div().when(current, |d| {
                            d.bg(if is_selected { theme.list_active } else { theme.list_hover })
                        })
                        .child(row)
                    })
                    .collect();
                // The selected window's modeline comes from Scheme; others show their buffer.
                let status: Vec<Segment> = match is_selected.then(|| modeline.take()).flatten() {
                    Some(segments) => segments,
                    None => vec![Segment { text: buffer.name().to_string(), face: "dim".into() }],
                };
                Some(
                    v_flex()
                        .absolute()
                        .left(px(r.x * width))
                        .top(px(r.y * area))
                        .w(px(r.w * width))
                        .h(px(r.h * area))
                        .when(r.x > 0., |d| d.border_l_1().border_color(theme.border))
                        .when(is_sidebar, |d| d.bg(theme.sidebar))
                        .child(v_flex().flex_1().overflow_hidden().px(px(8.)).children(lines))
                        .child(
                            h_flex()
                                .h(px(ROW_HEIGHT))
                                .px(px(8.))
                                .gap(px(14.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .bg(if is_selected { theme.status_bar } else { theme.background })
                                .border_t_1()
                                .border_color(theme.status_bar_border)
                                .children(
                                    status
                                        .into_iter()
                                        .filter(|s| !s.text.is_empty())
                                        .map(|s| div().text_color(face_color(&s.face, cx)).child(s.text)),
                                ),
                        ),
                )
            })
            .collect();

        for (id, top, rows) in fitted {
            if let Some(w) = st.window_by_id_mut(id) {
                w.scroll_top = top;
                w.rows = rows.max(1);
            }
        }
        syntax.retain(|id| st.buffer_by_id(id).is_some());
        // K's documentation, by the cursor: below its line, or above when near the bottom.
        let hover = st.hover.clone().zip(cursor_at).map(|(doc, (x, y, line_height))| {
            let w = HOVER_WIDTH.min(width - 16.);
            let left = x.min(width - w - 8.).max(8.);
            let below = y + line_height + HOVER_MAX_HEIGHT < area;
            div()
                .absolute()
                .left(px(left))
                .when(below, |d| d.top(px(y + line_height + 4.)))
                .when(!below, |d| d.bottom(px(height - y + 4.)))
                .w(px(w))
                .max_h(px(HOVER_MAX_HEIGHT))
                .overflow_hidden()
                .p(px(10.))
                .bg(theme.popover)
                .text_color(theme.popover_foreground)
                .border_1()
                .border_color(theme.border)
                .rounded(theme.radius_lg)
                .shadow_lg()
                .font_family(theme.font_family.clone())
                .text_size(px(13.))
                .line_height(px(19.))
                .child(gpui_kit::component::text::TextView::markdown("hover-doc", doc))
        });
        // Completions, under the word being typed.
        let completion = st.completion.as_ref().zip(cursor_at).map(|(c, (x, y, line_height))| {
            let rows = COMPLETION_ROWS.min(c.len());
            let first = c.selected().saturating_sub(rows - 1).min(c.len() - rows);
            let w = COMPLETION_WIDTH.min(width - 16.);
            let left = x.min(width - w - 8.).max(8.);
            let below = y + line_height + rows as f32 * ROW_HEIGHT + 8. < area;
            div()
                .absolute()
                .left(px(left))
                .when(below, |d| d.top(px(y + line_height + 2.)))
                .when(!below, |d| d.bottom(px(height - y + 2.)))
                .w(px(w))
                .py(px(4.))
                .bg(theme.popover)
                .border_1()
                .border_color(theme.border)
                .rounded(theme.radius)
                .shadow_lg()
                .children(c.items().enumerate().skip(first).take(rows).map(|(i, item)| {
                    h_flex()
                        .h(px(ROW_HEIGHT))
                        .px(px(8.))
                        .gap(px(10.))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .when(i == c.selected(), |d| d.bg(theme.list_active))
                        .child(div().flex_none().text_color(theme.foreground).child(item.label.clone()))
                        .child(div().flex_1().overflow_hidden().text_color(theme.muted_foreground).child(item.detail.clone()))
                        .child(div().flex_none().text_color(theme.primary).child(item.kind))
                }))
        });
        // The picker draws its own query line; the echo area keeps the message.
        let overlay = st.picker.as_ref().map(|p| picker_element(p, &st.minibuffer, bar, height, cx));
        let echo = match &st.minibuffer_prompt {
            Some(prompt) if overlay.is_none() => {
                let mb = &st.minibuffer;
                h_flex()
                    .child(div().text_color(theme.primary).child(prompt.clone()))
                    .child(line_element(LineView::input(display_line(mb.text().slice(..)), mb.column()), true, cx))
            }
            _ if st.message.is_empty() && st.minibuffer_prompt.is_none() => {
                // With nothing else to say, the diagnostic under the cursor.
                let point = st.buffer().point();
                let here = st.buffer_diagnostics().into_iter().find(|((a, b), _)| point >= *a && point < *b);
                match here {
                    Some((_, d)) => h_flex()
                        .gap(px(6.))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(div().text_color(severity_color(d.severity, cx)).child(format!("{}:", d.severity_name())))
                        .child(div().text_color(theme.foreground).child(d.message.lines().next().unwrap_or("").to_string())),
                    None => h_flex(),
                }
            }
            _ => h_flex().text_color(theme.muted_foreground).child(st.message.clone()),
        };
        drop(st);

        v_flex()
            .relative()
            .id("gmacs")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(theme.mono_font_family.clone())
            .text_size(px(FONT_SIZE))
            .line_height(px(ROW_HEIGHT))
            .child(div().relative().w_full().h(px(area)).children(windows))
            .child(div().h(px(ROW_HEIGHT)).px(px(8.)).child(echo))
            .children(hover)
            .children(completion)
            .children(overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(s: &str, key_char: Option<&str>) -> Keystroke {
        let mut k = Keystroke::parse(s).unwrap();
        k.key_char = key_char.map(Into::into);
        k
    }

    #[test]
    fn tall_lines_scroll_the_cursor_into_view() {
        // Line 0 is a heading twice as tall; room for 4 normal rows.
        let h = |i: usize| if i == 0 { 2. * ROW_HEIGHT } else { ROW_HEIGHT };
        let room = 4. * ROW_HEIGHT;
        assert_eq!(fit_lines(0, 1, 10, room, h), (0, 3), "heading + 2 lines fit");
        assert_eq!(fit_lines(0, 3, 10, room, h), (1, 5), "the cursor on line 3 scrolls the heading off");
        assert_eq!(fit_lines(0, 0, 1, room, h), (0, 1), "stops at the buffer's end");
        let huge = |_| 10. * ROW_HEIGHT;
        assert_eq!(fit_lines(0, 0, 10, room, huge), (0, 1), "always at least one line");
    }

    #[test]
    fn tabs_take_four_columns() {
        assert_eq!(display_columns("a\tb"), vec![0, 1, 5, 6, 7]);
    }

    #[test]
    fn match_columns_are_in_characters() {
        let re = regex::Regex::new("b+").unwrap();
        assert_eq!(match_columns("éabbxb", Some(&re)), vec![(2, 4), (5, 6)]);
        assert_eq!(match_columns("abc", None), vec![]);
    }

    #[test]
    fn keys_in_emacs_notation() {
        assert_eq!(key_name(&ks("a", Some("a"))), Some(("a".into(), "a".into())));
        assert_eq!(key_name(&ks("shift-a", Some("A"))), Some(("A".into(), "A".into())));
        assert_eq!(key_name(&ks("shift-;", Some(":"))), Some((":".into(), ":".into())));
        assert_eq!(key_name(&ks("ctrl-r", None)), Some(("C-r".into(), "".into())));
        assert_eq!(key_name(&ks("space", None)), Some(("SPC".into(), " ".into())));
        assert_eq!(key_name(&ks("escape", None)), Some(("ESC".into(), "".into())));
        assert_eq!(key_name(&ks("enter", None)), Some(("RET".into(), "".into())));
        assert_eq!(key_name(&ks("backspace", None)), Some(("DEL".into(), "".into())));
        assert_eq!(key_name(&ks("cmd-s", None)), Some(("s-s".into(), "".into())));
    }
}
