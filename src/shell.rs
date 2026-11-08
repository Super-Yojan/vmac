//! The window: draws the `Frame`s a `Session` lays out (each editor window,
//! its modeline, the echo area, popups and the picker) and sends it every
//! keystroke and mouse event as an `Input`. It decides nothing about what
//! keys do, and holds none of the editor's state.

use crate::frame::{
    CHAR_WIDTH, Content, Echo, FONT_SIZE, Face, Frame, FrameLine, FrameWindow, Input, Mods, PickerView, ROW_HEIGHT,
    ThemeRequest, Viewport, WindowId,
};
use crate::server::{Host, LSP_POLL, RELOAD_POLL, TERMINAL_POLL};
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Context, prelude::FluentBuilder as _, FocusHandle, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent,
    Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Render, ScrollWheelEvent,
    Styled as _, Window, div, px,
};
use std::time::Duration;

/// The documentation popup K shows.
const HOVER_WIDTH: f32 = 560.;
const HOVER_MAX_HEIGHT: f32 = 320.;
/// The completion popup.
const COMPLETION_WIDTH: f32 = 460.;

pub struct Shell {
    /// The sessions in this process, or a server's (`vmac attach`).
    session: Box<dyn Host>,
    focus: FocusHandle,
    /// The light/dark mode last applied, the system appearance last told
    /// to the session, and the subscription that hears it change.
    applied_mode: Option<(gpui_kit::component::ThemeMode, String)>,
    told_dark: Option<bool>,
    _appearance: gpui_kit::Subscription,
    /// Markdown buffers shown rendered in normal state (opt-in, `SPC t m`;
    /// the default hides the markup instead): their gpui-kit
    /// view, the text it last got and the cursor line it last revealed.
    md_views: std::collections::HashMap<crate::frame::BufferId, MdView>,
    /// *agent-review*, drawn by gpui-kit's Diff: its state, the text it was
    /// parsed from, and the cursor line it last selected.
    review_view: Option<ReviewView>,
    /// Where the last frame drew each window's lines, for the mouse, and
    /// whether a prompt or the picker had the screen.
    hits: Vec<WindowHit>,
    modal: bool,
    /// The window a button went down in, while it's held.
    dragging: Option<WindowId>,
    /// Wheel movement short of a whole line, kept for the next event.
    wheel: f32,
}

/// A window as last drawn: its area in pixels and where each line is.
#[derive(Debug, Clone, Default)]
struct WindowHit {
    id: WindowId,
    /// The whole window, modeline included.
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    /// Where text starts, after the padding and line numbers.
    text_left: f32,
    /// Each line on screen: its buffer line, top, height and screen
    /// column per character.
    lines: Vec<(usize, f32, f32, Vec<usize>)>,
    /// Drawn by gpui-kit (rendered Markdown, the review), which has its own mouse.
    custom: bool,
}

impl WindowHit {
    fn of(w: &FrameWindow) -> Self {
        let lines = match &w.content {
            Content::Text(lines) => lines.iter().map(|l| (l.line, l.top, l.height, l.cols.clone())).collect(),
            _ => Vec::new(),
        };
        Self {
            id: w.id,
            left: w.left,
            top: w.top,
            right: w.left + w.width,
            bottom: w.top + w.height,
            text_left: w.text_left,
            lines,
            custom: !matches!(w.content, Content::Text(_)),
        }
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// The buffer line and screen column at a point (outside the window,
    /// the nearest), and the character on that line: past the end of the
    /// text, the rows below continue the last line's numbering.
    fn at(&self, x: f32, y: f32) -> (usize, usize, usize) {
        let col = ((x - self.text_left) / (FONT_SIZE * CHAR_WIDTH)).max(0.) as usize;
        let Some(last) = self.lines.last() else { return (0, col, col) };
        let (line, cols) = match self.lines.iter().find(|l| y < l.1 + l.2) {
            Some(l) => (l.0, l.3.as_slice()),
            None => (last.0 + ((y - last.1 - last.2) / ROW_HEIGHT) as usize + 1, &[][..]),
        };
        // The last character starting at or before the column; the line's end after it.
        let chars = cols.len().saturating_sub(2);
        let ch = (0..=chars).rev().find(|&i| cols.get(i).is_some_and(|&c| c <= col)).unwrap_or(0);
        (line, col, ch)
    }
}

struct ReviewView {
    state: gpui_kit::Entity<gpui_kit::component::diff::DiffState>,
    revision: u64,
    selected: Option<usize>,
}

struct MdView {
    state: gpui_kit::Entity<gpui_kit::base::text::TextViewState>,
    revision: u64,
    revealed: Option<usize>,
    _changed: gpui_kit::Subscription,
}

impl Shell {
    pub fn new(mut session: Box<dyn Host>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        // What this machine has: its fonts, and the themes (built in, then
        // yours, ~/.config/vmac/themes/*.json).
        let mut fonts = cx.text_system().all_font_names();
        fonts.sort();
        fonts.dedup();
        let mut files: Vec<String> = crate::themes::BUILT_IN.iter().map(|s| s.to_string()).collect();
        if let Some(dir) = crate::lisp::config_dir() {
            files.extend(crate::themes::user_themes(&dir.join("themes")));
        }
        let mut themes = vec![("Default Light".to_string(), false), ("Default Dark".to_string(), true)];
        let mut errors = Vec::new();
        let registry = gpui_kit::component::ThemeRegistry::global_mut(cx);
        for file in &files {
            match registry.load_themes_from_str(file) {
                Ok(()) => themes.extend(crate::themes::names(file)),
                Err(e) => errors.push(e.to_string()),
            }
        }
        session.handle(Input::Client { fonts, themes, errors });
        Self::watch_sources(cx);
        Self::drive_language_servers(cx);
        Self::drive_terminals(cx);
        // Light or dark as the system is, and again whenever it changes.
        let appearance = cx.observe_window_appearance(window, |shell, _, cx| {
            shell.applied_mode = None;
            cx.notify();
        });
        Self {
            session,
            focus,
            applied_mode: None,
            told_dark: None,
            _appearance: appearance,
            md_views: Default::default(),
            review_view: None,
            hits: Vec::new(),
            modal: false,
            dragging: None,
            wheel: 0.,
        }
    }

    /// Tells the session whether the system is dark, when that changed.
    fn tell_appearance(&mut self, window: &Window) {
        let dark = gpui_kit::component::ThemeMode::from(window.appearance()).is_dark();
        if self.told_dark != Some(dark) {
            self.told_dark = Some(dark);
            self.session.handle(Input::Appearance { dark });
        }
    }

    /// Applies the theme the frame asks for (`:theme`): the system's
    /// appearance for "system". Does nothing when it's already applied.
    fn apply_theme(&mut self, request: &ThemeRequest, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
        let system: ThemeMode = window.appearance().into();
        let wanted = match request.mode.as_str() {
            "light" => ThemeMode::Light,
            "dark" => ThemeMode::Dark,
            _ => system,
        };
        let name = if wanted.is_dark() { request.dark.clone() } else { request.light.clone() };
        if self.applied_mode.as_ref() == Some(&(wanted, name.clone())) {
            return;
        }
        self.applied_mode = Some((wanted, name.clone()));
        // The named theme for this mode (the default when there's none such).
        let registry = ThemeRegistry::global(cx);
        let config = registry.themes().get(name.as_str()).filter(|c| c.mode.is_dark() == wanted.is_dark()).cloned();
        let config = config.unwrap_or_else(|| {
            if wanted.is_dark() { registry.default_dark_theme().clone() } else { registry.default_light_theme().clone() }
        });
        {
            let theme = Theme::global_mut(cx);
            if wanted.is_dark() {
                theme.dark_theme = config;
            } else {
                theme.light_theme = config;
            }
        }
        Theme::change(wanted, Some(window), cx);
        // Terminal programs that ask for the colors get the theme's.
        let hex = |c: gpui_kit::Hsla| {
            let c = c.to_rgb();
            ((c.r * 255.).round() as u32) << 16 | ((c.g * 255.).round() as u32) << 8 | (c.b * 255.).round() as u32
        };
        let t = cx.theme();
        let (foreground, background) = (hex(t.foreground), hex(t.background));
        self.session.handle(Input::TerminalColors { foreground, background });
    }

    /// Calls `work` on the session every `every`, drawing again when it says
    /// something changed.
    fn every(cx: &mut Context<Self>, every: Duration, work: fn(&mut dyn Host) -> bool) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(every).await;
                let alive = this.update(cx, |shell, cx| {
                    if work(shell.session.as_mut()) {
                        cx.notify();
                    }
                    // A server we're attached to can let go of us any time.
                    if shell.session.quit_requested() {
                        cx.quit();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Reloads lisp/core/ and init.scm when they change on disk.
    fn watch_sources(cx: &mut Context<Self>) {
        Self::every(cx, RELOAD_POLL, |host| host.reload_if_changed());
    }

    /// Reads terminals' screens into their buffers, often enough that typing
    /// into one feels immediate.
    fn drive_terminals(cx: &mut Context<Self>) {
        Self::every(cx, TERMINAL_POLL, |host| host.pump_terminals());
    }

    /// Syncs files with their language servers and reads what they report.
    fn drive_language_servers(cx: &mut Context<Self>) {
        Self::every(cx, LSP_POLL, |host| host.pump_servers());
    }

    /// Hands the session an input, then draws again (or quits).
    fn send(&mut self, input: Input, cx: &mut Context<Self>) {
        self.session.handle(input);
        if self.session.quit_requested() {
            cx.quit();
        }
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, text)) = key_name(&event.keystroke) else { return };
        self.send(Input::Key { key, text }, cx);
    }

    /// The window under a point, unless a picker or prompt has the screen.
    fn hit_at(&self, x: f32, y: f32) -> Option<WindowHit> {
        if self.modal {
            return None;
        }
        self.hits.iter().find(|h| h.contains(x, y)).cloned()
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let (x, y): (f32, f32) = (event.position.x.into(), event.position.y.into());
        let Some(hit) = self.hit_at(x, y) else { return };
        window.focus(&self.focus, cx);
        let button = match event.button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        let (line, col, ch) = hit.at(x, y);
        self.dragging = Some(hit.id);
        let mods = mouse_mods(&event.modifiers);
        self.send(Input::MouseDown { window: hit.id, button, line, col, ch, clicks: event.click_count, mods }, cx);
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.dragging else { return };
        if event.pressed_button.is_none() {
            self.dragging = None;
            self.session.handle(Input::MouseLost);
            return;
        }
        let Some(hit) = self.hits.iter().find(|h| h.id == id) else { return };
        let (line, col, ch) = hit.at(event.position.x.into(), event.position.y.into());
        self.send(Input::MouseDrag { window: id, line, col, ch, mods: mouse_mods(&event.modifiers) }, cx);
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.dragging.take() else { return };
        let Some(hit) = self.hits.iter().find(|h| h.id == id) else {
            self.session.handle(Input::MouseLost);
            return;
        };
        let (line, col, _) = hit.at(event.position.x.into(), event.position.y.into());
        self.send(Input::MouseUp { window: id, line, col, mods: mouse_mods(&event.modifiers) }, cx);
    }

    /// The wheel scrolls the window under the pointer, selected or not; a
    /// terminal program that asked for the mouse gets the wheel instead.
    fn on_scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let (x, y): (f32, f32) = (event.position.x.into(), event.position.y.into());
        let Some(hit) = self.hit_at(x, y).filter(|h| !h.custom) else { return };
        let dy: f32 = event.delta.pixel_delta(px(ROW_HEIGHT)).y.into();
        // Wheel up (content down) scrolls toward the top: negative lines.
        self.wheel -= dy / ROW_HEIGHT;
        let n = self.wheel.trunc();
        self.wheel -= n;
        if n == 0. {
            return;
        }
        let (line, col, _) = hit.at(x, y);
        let mods = mouse_mods(&event.modifiers);
        self.send(Input::Wheel { window: hit.id, lines: n as isize, line, col, mods }, cx);
    }

    /// Keeps the rendered Markdown view of a buffer in step with its text,
    /// and the cursor's line revealed and marked in it.
    fn markdown_view(
        &mut self,
        buffer: crate::frame::BufferId,
        source: &str,
        revision: u64,
        cursor_line: usize,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Entity<gpui_kit::base::text::TextViewState> {
        // The line you are on, in the theme's selection color: plain to see.
        let highlight = cx.theme().selection;
        let view = self.md_views.entry(buffer).or_insert_with(|| {
            let state = gpui_kit::AppContext::new(cx, |cx| gpui_kit::base::text::TextViewState::markdown(source, cx));
            // Its parse lands later: draw again then.
            let changed = cx.observe(&state, |_, _, cx| cx.notify());
            MdView { state, revision, revealed: None, _changed: changed }
        });
        if view.revision != revision {
            view.revision = revision;
            view.revealed = None;
            view.state.update(cx, |s, cx| s.set_text(source, cx));
        }
        // The cursor's line: scrolled to and marked, once the parse matches the text.
        if view.revealed != Some(cursor_line) {
            let text = view.state.read(cx).rendered_text();
            if text.source() == source {
                let rope = ropey::Rope::from_str(source);
                let found = (cursor_line..rope.len_lines().min(cursor_line + 40)).find_map(|l| {
                    let a = rope.line_to_byte(l);
                    let b = rope.line_to_byte((l + 1).min(rope.len_lines()));
                    text.range_for_source(a..b)
                });
                if let Some(range) = found {
                    view.state.update(cx, |s, cx| {
                        let _ = s.reveal_range(range.clone(), cx);
                        let _ = s.set_range_highlights([gpui_kit::base::text::RangeHighlight::new(range, highlight)], cx);
                    });
                }
                view.revealed = Some(cursor_line);
            }
        }
        view.state.clone()
    }

    /// Keeps *agent-review*'s diff in step with the frame, the cursor's row selected.
    fn review_state(
        &mut self,
        review: &crate::frame::ReviewView,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Entity<gpui_kit::component::diff::DiffState> {
        use gpui_kit::component::diff::{DiffFile, DiffLinePosition, DiffLineRange, DiffSide, DiffState};
        let files = DiffFile::parse(&review.patch).unwrap_or_default();
        let revision = review.revision;
        let view = self.review_view.get_or_insert_with(|| {
            let files = files.clone();
            let state = gpui_kit::AppContext::new(cx, |cx| DiffState::new(files, cx));
            ReviewView { state, revision, selected: None }
        });
        if view.revision != revision {
            view.revision = revision;
            view.selected = None;
            view.state.update(cx, |s, cx| s.set_files(files, cx));
        }
        if view.selected != Some(review.cursor_line) {
            view.selected = Some(review.cursor_line);
            if let Some(row) = &review.row {
                let side = if row.modified { DiffSide::Modified } else { DiffSide::Original };
                let (rel, line) = (row.rel.clone(), row.line);
                view.state.update(cx, |s, cx| {
                    s.set_selected_lines(Some(DiffLineRange::new(rel.clone(), side, line, line)), cx);
                    s.scroll_to_line(DiffLinePosition::new(rel, side, line), cx);
                });
            }
        }
        view.state.clone()
    }
}

/// The modifier keys of a mouse event.
fn mouse_mods(m: &gpui_kit::Modifiers) -> Mods {
    Mods { shift: m.shift, alt: m.alt, control: m.control }
}

gpui_kit::actions!(vmac, [TabKey, ShiftTabKey]);

/// The key context of vmac's window. Bindings here win over gpui-kit's
/// Root, which is further out.
const KEY_CONTEXT: &str = "Vmac";

/// Keys gpui-kit's Root binds for itself (TAB and shift-TAB move focus
/// between controls) and the editor needs instead. GPUI matches bindings
/// before key handlers run, so the editor binds them too, deeper in, and
/// passes them on as ordinary keys.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui_kit::KeyBinding::new("tab", TabKey, Some(KEY_CONTEXT)),
        gpui_kit::KeyBinding::new("shift-tab", ShiftTabKey, Some(KEY_CONTEXT)),
    ]);
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
    // "#rrggbb": an exact color (terminals' 256 and true colors).
    if let Some(hex) = face.strip_prefix('#')
        && let Ok(n) = u32::from_str_radix(hex, 16)
        && hex.len() == 6
    {
        return gpui_kit::rgb(n).into();
    }
    match face {
        "muted" => t.muted,
        // A tint of the text color: shows on any theme (code's background).
        "subtle" => t.foreground.opacity(0.09),
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

fn resolve_style(face: &Face, cx: &App) -> Style {
    Style {
        color: face.color.as_deref().map(|c| face_color(c, cx)),
        background: face.background.as_deref().map(|c| face_color(c, cx)),
        scale: face.scale,
        bold: face.bold,
        italic: face.italic,
        underline: face.underline,
        sans: face.sans,
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

/// What one line shows, all in screen columns.
#[derive(Default)]
struct LineView {
    text: String,
    cursor: Option<usize>,
    /// The selected columns; may include the line break (one past the end),
    /// drawn as a space like Vim does.
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

    /// A window's line, its faces resolved against the theme.
    fn of(line: &FrameLine, cx: &App) -> Self {
        Self {
            text: line.text.clone(),
            cursor: line.cursor,
            selected: line.selected,
            matches: line.matches.clone(),
            styles: line.styles.iter().map(|(a, b, f)| (*a, *b, resolve_style(f, cx))).collect(),
            diagnostics: line.diagnostics.iter().map(|&(a, b, s)| (a, b, severity_color(s, cx))).collect(),
        }
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

/// A window's line with what goes around it: its number, the sidebar's
/// cursor band, code-block band, and the review and comment marks.
fn window_line_element(line: &FrameLine, selected: bool, bar: bool, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let row = line_element(LineView::of(line, cx), bar, cx);
    let row = match &line.number {
        Some(shown) => h_flex()
            .items_end()
            .child(
                div()
                    .h(px(line.height))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_color(if line.number_current { theme.foreground } else { theme.muted_foreground.opacity(0.7) })
                    .child(shown.clone()),
            )
            .child(row)
            .into_any_element(),
        None => row.into_any_element(),
    };
    div()
        .relative()
        .when(line.code_block, |d| d.bg(theme.foreground.opacity(0.06)))
        .when(line.current, |d| d.bg(if selected { theme.list_active } else { theme.list_hover }))
        // The agent's unreviewed lines: a green band, and a red rule where
        // it deleted lines.
        .when(line.added, |d| {
            d.bg(theme.green.opacity(0.14))
                .child(div().absolute().left(px(-6.)).top_0().bottom_0().w(px(3.)).bg(theme.green))
        })
        .when(line.deleted_above, |d| {
            d.child(div().absolute().left(px(-6.)).right_0().top_0().h(px(2.)).bg(theme.red))
        })
        // A comment on this line: a yellow mark in the margin.
        .when(line.comment, |d| {
            d.child(div().absolute().left(px(-6.)).top(px(3.)).w(px(4.)).h(px(10.)).rounded(px(1.)).bg(theme.yellow))
        })
        .child(row)
}

/// The which-key popup: a panel just above the echo area listing available keys.
fn whichkey_element(entries: &[(String, String)], width: f32, cx: &App) -> gpui_kit::AnyElement {
    let t = cx.theme();
    let col_width: f32 = 220.0;
    let cols = ((width - 16.) / col_width).floor().max(1.) as usize;
    let rows = entries.len().div_ceil(cols);
    let max_rows = 8;
    let panel_rows = rows.min(max_rows);
    let panel_height = panel_rows as f32 * ROW_HEIGHT + 12.;

    div()
        .absolute()
        .bottom(px(ROW_HEIGHT + 4.))
        .left(px(0.))
        .right(px(0.))
        .mx(px(8.))
        .h(px(panel_height))
        .overflow_hidden()
        .bg(t.popover)
        .text_color(t.popover_foreground)
        .border_1()
        .border_color(t.border)
        .rounded(t.radius)
        .shadow_lg()
        .px(px(10.))
        .py(px(6.))
        .child(h_flex().flex_wrap().children(entries.iter().map(|(key, label)| {
            let is_group = label.starts_with('+');
            h_flex()
                .w(px(col_width))
                .h(px(ROW_HEIGHT))
                .gap(px(4.))
                .child(div().text_color(t.primary).min_w(px(24.)).child(key.clone()))
                .child(div().text_color(if is_group { t.primary } else { t.muted_foreground }).child(label.clone()))
        })))
        .into_any_element()
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
fn picker_element(picker: &PickerView, bar: bool, cx: &App) -> gpui_kit::AnyElement {
    let t = cx.theme();
    let results = picker.rows.iter().map(|(item, positions, selected)| match_row(item, positions, *selected, cx));
    let preview = picker.preview.clone();

    div().absolute().inset_0().flex().items_center().justify_center().child(
        v_flex()
            .w(gpui_kit::relative(0.84))
            .h(px(picker.panel_height))
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
                    .child(div().text_color(t.primary).child(picker.title.clone()))
                    .child(div().text_color(t.muted_foreground).child(format!("{} / {}", picker.matched, picker.total))),
            )
            .child(
                h_flex()
                    .h(px(ROW_HEIGHT + 6.))
                    .px(px(10.))
                    .border_b_1()
                    .border_color(t.border)
                    .child(div().text_color(t.primary).child("> "))
                    .child(line_element(LineView::input(picker.query.clone(), picker.cursor), bar, cx)),
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
                            .when(picker.matched == 0, |d| {
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
                                .children(lines.into_iter().enumerate().map(|(i, l)| {
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

/// Where a Markdown image is on disk: a path (relative to the file's
/// folder, or absolute, or file://). None for web addresses and data.
fn local_image(folder: &std::path::Path, url: &str) -> Option<std::path::PathBuf> {
    let url = url.trim();
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("data:") {
        return None;
    }
    let path = url.strip_prefix("file://").unwrap_or(url);
    // Spaces written as %20 in Markdown links.
    let path = path.replace("%20", " ");
    let path = std::path::Path::new(&path);
    Some(if path.is_absolute() { path.to_path_buf() } else { folder.join(path) })
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tell_appearance(window);
        let viewport = window.viewport_size();
        let (width, height): (f32, f32) = (viewport.width.into(), viewport.height.into());
        let frame: Frame = self.session.frame(Viewport { width, height });
        self.apply_theme(&frame.theme, window, cx);
        let area = frame.area;
        let bar = frame.bar;

        self.hits = frame.windows.iter().map(WindowHit::of).collect();
        self.modal = frame.modal;
        let live: std::collections::HashSet<_> = frame.buffers.iter().copied().collect();
        self.md_views.retain(|id, _| live.contains(id));

        let this = cx.entity().downgrade();
        let windows: Vec<_> = frame
            .windows
            .iter()
            .map(|w| {
                let body = match &w.content {
                    Content::Review(review) => {
                        use gpui_kit::component::diff::{Diff, DiffAnnotation, DiffLinePosition, DiffSide};
                        let state = self.review_state(review, cx);
                        let annotations: Vec<DiffAnnotation> = review
                            .comments
                            .iter()
                            .map(|c| {
                                let side = if c.at.modified { DiffSide::Modified } else { DiffSide::Original };
                                DiffAnnotation::line(
                                    gpui_kit::SharedString::from(format!("comment-{}", c.id)),
                                    DiffLinePosition::new(c.at.rel.clone(), side, c.at.line),
                                )
                            })
                            .collect();
                        let texts: std::collections::HashMap<String, String> =
                            review.comments.iter().map(|c| (format!("comment-{}", c.id), c.text.clone())).collect();
                        let (on_add, on_click) = (this.clone(), this.clone());
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                Diff::new(&state)
                                    .size_full()
                                    .annotations(annotations)
                                    .render_annotation(move |a, _, cx| {
                                        let t = cx.theme();
                                        let text = texts.get(&a.id().to_string()).cloned().unwrap_or_default();
                                        h_flex()
                                            .gap(px(8.))
                                            .px(px(10.))
                                            .py(px(6.))
                                            .border_l_2()
                                            .border_color(t.yellow)
                                            .bg(t.yellow.opacity(0.08))
                                            .child(div().text_color(t.yellow).child("comment"))
                                            .child(div().child(text))
                                    })
                                    // The + beside a hovered line: a comment on it.
                                    .on_add_annotation(move |range, _, cx| {
                                        let input = Input::ReviewCommentAt {
                                            path: range.path().to_string(),
                                            modified: range.side() == DiffSide::Modified,
                                            line: range.start(),
                                        };
                                        let _ = on_add.update(cx, |shell, cx| shell.send(input, cx));
                                    })
                                    // A click puts the cursor on that row.
                                    .on_line_click(move |pos, _, _, cx| {
                                        let input = Input::ReviewSelect {
                                            path: pos.path().to_string(),
                                            modified: pos.side() == DiffSide::Modified,
                                            line: pos.line(),
                                        };
                                        let _ = on_click.update(cx, |shell, cx| shell.send(input, cx));
                                    }),
                            )
                            .into_any_element()
                    }
                    Content::Markdown { source, revision, cursor_line, folder } => {
                        // Rendered Markdown: images, code blocks, tables, HTML.
                        let state = self.markdown_view(w.buffer, source, *revision, *cursor_line, cx);
                        let folder = folder.clone();
                        let theme = cx.theme();
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .px(px(20.))
                            .py(px(10.))
                            .font_family(theme.font_family.clone())
                            .text_size(px(15.))
                            .line_height(px(22.))
                            .child(
                                gpui_kit::base::text::TextView::new(&state)
                                    .scrollable(true)
                                    .selectable(false)
                                    .image_source(move |uri| match local_image(&folder, uri.as_ref()) {
                                        Some(path) => path.into(),
                                        None => uri.clone().into(),
                                    }),
                            )
                            .into_any_element()
                    }
                    Content::Text(lines) => v_flex()
                        .flex_1()
                        .overflow_hidden()
                        .px(px(8.))
                        .children(lines.iter().map(|l| window_line_element(l, w.selected, bar, cx)))
                        .into_any_element(),
                };
                let theme = cx.theme();
                v_flex()
                    .absolute()
                    .left(px(w.left))
                    .top(px(w.top))
                    .w(px(w.width))
                    .h(px(w.height))
                    .when(w.left > 0., |d| d.border_l_1().border_color(theme.border))
                    .when(w.sidebar, |d| d.bg(theme.sidebar))
                    .child(body)
                    .child(
                        h_flex()
                            .h(px(ROW_HEIGHT))
                            .px(px(8.))
                            .gap(px(14.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .bg(if w.selected { theme.status_bar } else { theme.background })
                            .border_t_1()
                            .border_color(theme.status_bar_border)
                            .children(
                                w.modeline
                                    .iter()
                                    .filter(|s| !s.text.is_empty())
                                    .map(|s| div().text_color(face_color(&s.face, cx)).child(s.text.clone())),
                            ),
                    )
            })
            .collect();

        let theme = cx.theme();
        // K's documentation, by the cursor: below its line, or above when near the bottom.
        let hover = frame.hover.clone().zip(frame.cursor_at).map(|(doc, (x, y, line_height))| {
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
        let completion = frame.completion.as_ref().zip(frame.cursor_at).map(|(rows, (x, y, line_height))| {
            let w = COMPLETION_WIDTH.min(width - 16.);
            let left = x.min(width - w - 8.).max(8.);
            let below = y + line_height + rows.len() as f32 * ROW_HEIGHT + 8. < area;
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
                .children(rows.iter().map(|item| {
                    h_flex()
                        .h(px(ROW_HEIGHT))
                        .px(px(8.))
                        .gap(px(10.))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .when(item.selected, |d| d.bg(theme.list_active))
                        .child(div().flex_none().text_color(theme.foreground).child(item.label.clone()))
                        .child(div().flex_1().overflow_hidden().text_color(theme.muted_foreground).child(item.detail.clone()))
                        .child(div().flex_none().text_color(theme.primary).child(item.kind.clone()))
                }))
        });
        // Which-key popup, just above the echo area when a prefix key is pending.
        let whichkey = frame.whichkey.as_ref().map(|wk| whichkey_element(wk, width, cx));
        let overlay = frame.picker.as_ref().map(|p| picker_element(p, bar, cx));
        let echo = match &frame.echo {
            Echo::Prompt { prompt, text, cursor } => h_flex()
                .child(div().text_color(theme.primary).child(prompt.clone()))
                .child(line_element(LineView::input(text.clone(), *cursor), true, cx)),
            Echo::Diagnostic { severity, label, message } => h_flex()
                .gap(px(6.))
                .whitespace_nowrap()
                .overflow_hidden()
                .child(div().text_color(severity_color(*severity, cx)).child(format!("{label}:")))
                .child(div().text_color(theme.foreground).child(message.clone())),
            Echo::Message(message) => h_flex().text_color(theme.muted_foreground).child(message.clone()),
            Echo::Empty => h_flex(),
        };
        // The Markdown preview, rendered by gpui-kit, on the right.
        let preview = frame.preview.as_ref().map(|p| {
            let folder = p.folder.clone();
            div()
                .absolute()
                .left(px(p.left))
                .top_0()
                .w(px(p.width))
                .h(px(area))
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.background)
                .font_family(theme.font_family.clone())
                .text_size(px(15.))
                .line_height(px(22.))
                .px(px(20.))
                .py(px(12.))
                .child(
                    gpui_kit::base::TextView::markdown(
                        gpui_kit::SharedString::from(format!("markdown-preview-{}", p.buffer)),
                        p.source.clone(),
                    )
                    .scrollable(true)
                    .image_source(move |uri| match local_image(&folder, uri.as_ref()) {
                        Some(path) => path.into(),
                        None => uri.clone().into(),
                    }),
                )
        });
        let font = frame.font.clone().map(gpui_kit::SharedString::from);

        v_flex()
            .relative()
            .id("vmac")
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_key))
            .on_action(cx.listener(|shell, _: &TabKey, _, cx| shell.send(Input::Key { key: "TAB".into(), text: String::new() }, cx)))
            .on_action(
                cx.listener(|shell, _: &ShiftTabKey, _, cx| shell.send(Input::Key { key: "S-TAB".into(), text: String::new() }, cx)),
            )
            .on_any_mouse_down(cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(font.unwrap_or_else(|| theme.mono_font_family.clone()))
            .text_size(px(FONT_SIZE))
            .line_height(px(ROW_HEIGHT))
            .child(div().relative().w_full().h(px(area)).children(windows).children(preview))
            .child(div().h(px(ROW_HEIGHT)).px(px(8.)).child(echo))
            .children(whichkey)
            .children(hover)
            .children(completion)
            .children(overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::display_columns;

    fn ks(s: &str, key_char: Option<&str>) -> Keystroke {
        let mut k = Keystroke::parse(s).unwrap();
        k.key_char = key_char.map(Into::into);
        k
    }

    #[test]
    fn the_mouse_finds_the_line_and_character_under_it() {
        let cw = FONT_SIZE * CHAR_WIDTH;
        let hit = WindowHit {
            left: 0.,
            top: 0.,
            right: 400.,
            bottom: 200.,
            text_left: 8. + 3. * cw,
            // A heading twice as tall, then a line with a tab (four columns).
            lines: vec![(10, 0., 40., display_columns("# Hi")), (11, 40., 20., display_columns("\tx"))],
            ..Default::default()
        };
        assert!(hit.contains(10., 10.) && !hit.contains(410., 10.));
        let x = |col: f32| 8. + (3. + col) * cw + 1.;
        assert_eq!(hit.at(x(2.), 30.), (10, 2, 2), "inside the tall line");
        assert_eq!(hit.at(x(2.), 45.), (11, 2, 0), "inside the tab: the tab");
        assert_eq!(hit.at(x(4.), 45.), (11, 4, 1), "after it, the x");
        assert_eq!(hit.at(x(30.), 45.), (11, 30, 2), "past the end: the line's end");
        assert_eq!(hit.at(0., 45.).2, 0, "on the line numbers: the first character");
        assert_eq!(hit.at(x(1.), 105.).0, 14, "below the text, rows keep counting (terminals)");
    }

    #[test]
    fn images_load_from_the_files_folder() {
        let folder = std::path::Path::new("/notes");
        assert_eq!(local_image(folder, "assets/logo.svg"), Some("/notes/assets/logo.svg".into()));
        assert_eq!(local_image(folder, "file:///tmp/a%20b.png"), Some("/tmp/a b.png".into()));
        assert_eq!(local_image(folder, "https://x.dev/a.png"), None);
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
