//! The window: draws each editor window (its buffer and modeline, laid out
//! by the split tree), the echo area and the picker, and sends every
//! keystroke to Scheme. It decides nothing about what keys do.

use crate::buffer::Buffer;
use crate::editor::{Editor, Rect};
use crate::lisp::{Lisp, Segment};
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
/// How wide the sidebar (the file tree) is.
const SIDEBAR_WIDTH: f32 = 260.;

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
        Self { editor, lisp, focus, syntax: Syntax::default() }
    }

    /// Reloads core.scm / init.scm when they change on disk.
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

/// How one character cell is drawn.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Cell {
    selected: bool,
    matched: bool,
    cursor: bool,
    color: Option<Hsla>,
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
fn line_element(
    text: String,
    cursor: Option<usize>,
    selected: Option<(usize, usize)>,
    matches: Vec<(usize, usize)>,
    colors: Vec<(usize, usize, Hsla)>,
    bar: bool,
    cx: &App,
) -> impl IntoElement {
    let t = cx.theme();
    let chars: Vec<char> = text.chars().collect();
    let in_sel = |col: usize| selected.is_some_and(|(a, b)| col >= a && col < b);
    let in_match = |col: usize| matches.iter().any(|&(a, b)| col >= a && col < b);
    let color = |col: usize| colors.iter().find(|&&(a, b, _)| col >= a && col < b).map(|c| c.2);
    let cell = |col: usize| Cell {
        selected: in_sel(col),
        matched: in_match(col),
        cursor: cursor == Some(col),
        color: color(col),
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

    h_flex().h(px(ROW_HEIGHT)).whitespace_nowrap().children(runs.into_iter().map(|(c, text)| {
        let run = div()
            .when_some(c.color, |d, color| d.text_color(color))
            .when(c.matched, |d| d.bg(t.warning.opacity(0.35)))
            .when(c.selected, |d| d.bg(t.selection))
            .child(text);
        match (c.cursor, bar) {
            (false, _) => run,
            (true, true) => run
                .relative()
                .child(div().absolute().left_0().top_0().w(px(2.)).h(px(ROW_HEIGHT)).bg(t.caret)),
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
                    .child(line_element(
                        display_line(query.text().slice(..)),
                        Some(query.column()),
                        None,
                        vec![],
                        vec![],
                        bar,
                        cx,
                    )),
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
                    .when_some(preview, |d, lines| {
                        d.child(
                            v_flex()
                                .flex_1()
                                .h_full()
                                .px(px(10.))
                                .border_l_1()
                                .border_color(t.border)
                                .text_color(t.muted_foreground)
                                .overflow_hidden()
                                .children(lines.into_iter().take(rows).map(|l| {
                                    div().h(px(ROW_HEIGHT)).whitespace_nowrap().child(l)
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
                let rows = window_rows(r, area);
                let last = text.len_lines().min(w.scroll_top + rows);
                let spans = match buffer.file().and_then(|f| language_for_path(f)) {
                    Some(lang) => syntax.highlight(w.buffer, lang, buffer.revision(), text, w.scroll_top..last),
                    None => Vec::new(),
                };
                let lines: Vec<_> = (w.scroll_top..last)
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
                        let mut colors: Vec<(usize, usize, Hsla)> = spans
                            .get(i - w.scroll_top)
                            .map(|line_spans| {
                                line_spans
                                    .iter()
                                    .filter_map(|sp| {
                                        let face = face_for(&sp.capture, &syntax_theme)?;
                                        Some((to_screen(sp.start), to_screen(sp.end), face_color(face, cx)))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        // Faces Scheme put on the text (the tree's folders, ...).
                        colors.extend(buffer.faces().iter().filter(|f| f.0 < start + line_len && f.1 > start).map(
                            |(a, b, face)| {
                                (
                                    to_screen(a.saturating_sub(start)),
                                    to_screen(b.saturating_sub(start)),
                                    face_color(face, cx),
                                )
                            },
                        ));
                        let line = display_line(text.line(i));
                        let matches = match_columns(&line, search.as_ref());
                        let row = line_element(line, cursor, selected, matches, colors, bar, cx);
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

        syntax.retain(|id| st.buffer_by_id(id).is_some());
        // The picker draws its own query line; the echo area keeps the message.
        let overlay = st.picker.as_ref().map(|p| picker_element(p, &st.minibuffer, bar, height, cx));
        let echo = match &st.minibuffer_prompt {
            Some(prompt) if overlay.is_none() => {
                let mb = &st.minibuffer;
                h_flex()
                    .child(div().text_color(theme.primary).child(prompt.clone()))
                    .child(line_element(display_line(mb.text().slice(..)), Some(mb.column()), None, vec![], vec![], true, cx))
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
