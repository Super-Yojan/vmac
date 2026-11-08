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
    Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Render, ScrollWheelEvent,
    Styled as _, Window, div, px,
};
use std::time::Duration;

const ROW_HEIGHT: f32 = 20.;
const FONT_SIZE: f32 = 14.;
const RELOAD_POLL: Duration = Duration::from_millis(400);
/// How often language servers are synced and read.
const LSP_POLL: Duration = Duration::from_millis(100);
/// How often terminals' screens are read.
const TERMINAL_POLL: Duration = Duration::from_millis(30);
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
    /// The light/dark mode last applied, and the subscription that hears
    /// the system's appearance change.
    applied_mode: Option<(gpui_kit::component::ThemeMode, String)>,
    _appearance: gpui_kit::Subscription,
    /// Markdown buffers shown rendered in normal state (opt-in, `SPC t m`;
    /// the default hides the markup instead): their gpui-kit
    /// view, the text it last got and the cursor line it last revealed.
    md_views: std::collections::HashMap<crate::editor::BufferId, MdView>,
    /// *agent-review*, drawn by gpui-kit's Diff: its state, the text it was
    /// parsed from, and the cursor line it last selected.
    review_view: Option<ReviewView>,
    /// Where the last frame drew each window's lines, for the mouse.
    hits: Vec<WindowHit>,
    /// The button held down since a press in a window.
    drag: Option<Drag>,
    /// Wheel movement short of a whole line, kept for the next event.
    wheel: f32,
}

/// A window as last drawn: its area in pixels and where each line is.
#[derive(Debug, Clone, Default)]
struct WindowHit {
    id: crate::editor::WindowId,
    buffer: crate::editor::BufferId,
    /// The whole window, modeline included.
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    /// Where text starts, after the padding and line numbers.
    text_left: f32,
    /// Each line on screen: its buffer line, top, height and screen
    /// column per character (`layout_line`'s map).
    lines: Vec<(usize, f32, f32, Vec<usize>)>,
    /// Drawn by gpui-kit (rendered Markdown, the review), which has its own mouse.
    custom: bool,
    terminal: bool,
}

impl WindowHit {
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

/// A button held down: in a window's text (dragging selects), or reported
/// to the terminal program that asked for the mouse.
#[derive(Debug, Clone, Copy)]
struct Drag {
    window: crate::editor::WindowId,
    /// The terminal's button, when the program has the mouse.
    terminal: Option<u8>,
    /// The last position reported: a buffer position, or a terminal cell.
    last: (usize, usize),
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
    pub fn new(editor: Editor, lisp: Lisp, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        // The installed fonts, and a Nerd Font to start with if there is one.
        {
            let mut fonts = cx.text_system().all_font_names();
            fonts.sort();
            fonts.dedup();
            let mut st = editor.lock();
            if st.font.is_none() {
                st.font = crate::editor::default_font(&fonts);
            }
            st.fonts = fonts;
        }
        // The themes: built in, then yours (~/.config/vmac/themes/*.json).
        {
            let mut files: Vec<String> = crate::themes::BUILT_IN.iter().map(|s| s.to_string()).collect();
            if let Some(dir) = crate::lisp::config_dir() {
                files.extend(crate::themes::user_themes(&dir.join("themes")));
            }
            let mut names = vec![("Default Light".to_string(), false), ("Default Dark".to_string(), true)];
            let registry = gpui_kit::component::ThemeRegistry::global_mut(cx);
            for file in &files {
                match registry.load_themes_from_str(file) {
                    Ok(()) => names.extend(crate::themes::names(file)),
                    Err(e) => editor.lock().message = format!("a theme file didn't load: {e}"),
                }
            }
            editor.lock().theme_names = names;
        }
        Self::watch_sources(cx);
        Self::drive_language_servers(cx);
        Self::drive_terminals(cx);
        // Light or dark as the system is, and again whenever it changes.
        let appearance = cx.observe_window_appearance(window, |shell, window, cx| {
            shell.applied_mode = None;
            shell.apply_theme(window, cx);
        });
        let mut shell = Self {
            editor,
            lisp,
            focus,
            syntax: Syntax::default(),
            applied_mode: None,
            _appearance: appearance,
            md_views: Default::default(),
            review_view: None,
            hits: Vec::new(),
            drag: None,
            wheel: 0.,
        };
        shell.apply_theme(window, cx);
        shell
    }

    /// Applies the theme mode the editor asks for (`:theme`): the system's
    /// appearance for "system". Does nothing when it's already applied.
    fn apply_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
        let system: ThemeMode = window.appearance().into();
        let (wanted, name) = {
            let mut st = self.editor.lock();
            st.system_dark = system.is_dark();
            let mode = match st.theme_mode.as_str() {
                "light" => ThemeMode::Light,
                "dark" => ThemeMode::Dark,
                _ => system,
            };
            (mode, st.theme_name(mode.is_dark()))
        };
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
        crate::terminal::set_colors(hex(t.foreground), hex(t.background));
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

    /// Reads terminals' screens into their buffers, often enough that typing
    /// into one feels immediate.
    fn drive_terminals(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TERMINAL_POLL).await;
                let alive = this.update(cx, |shell, cx| {
                    if !shell.editor.lock().terminals.is_empty() && shell.lisp.pump_terminals() {
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
                    let changed = {
                        let mut editor = shell.editor.lock();
                        editor.lsp_pump(configs) | editor.agent_pump() | editor.dashboard_refresh()
                    };
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
        self.dispatch(&key, &text, cx);
    }

    fn dispatch(&mut self, key: &str, text: &str, cx: &mut Context<Self>) {
        self.lisp.dispatch_key(key, text);
        if self.editor.lock().quit_requested {
            cx.quit();
        }
        cx.notify();
    }

    /// Runs a mouse call in Scheme; its error, if any, in the echo area.
    fn mouse_call(&mut self, code: &str, cx: &mut Context<Self>) {
        if let Err(e) = self.lisp.eval(code) {
            self.editor.lock().message = format!("mouse: {}", e.lines().next().unwrap_or(""));
        }
        if self.editor.lock().quit_requested {
            cx.quit();
        }
        cx.notify();
    }

    /// The window under a point, unless a picker or prompt has the screen.
    fn hit_at(&self, x: f32, y: f32) -> Option<WindowHit> {
        let st = self.editor.lock();
        if st.picker.is_some() || st.minibuffer_prompt.is_some() {
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
        // A program that asked for the mouse gets it; shift selects its text instead.
        if hit.terminal && !event.modifiers.shift {
            let taken = {
                let mut st = self.editor.lock();
                let taken = st.terminal_mouse(hit.buffer, crate::terminal::MouseAction::Press(button), line, col, mouse_mods(&event.modifiers));
                if taken {
                    st.select_window(hit.id);
                }
                taken
            };
            if taken {
                self.drag = Some(Drag { window: hit.id, terminal: Some(button), last: (line, col) });
                // Typing goes to the program you clicked in.
                self.mouse_call("(set! evil-state 'insert)", cx);
                return;
            }
        }
        if button != 0 {
            return;
        }
        let pos = {
            let mut st = self.editor.lock();
            st.select_window(hit.id);
            let text = st.buffer().text();
            let line = line.min(text.len_lines().saturating_sub(1));
            let start = text.line_to_char(line);
            let len = text_line_len(text, line);
            start + ch.min(len)
        };
        if hit.custom {
            cx.notify();
            return;
        }
        self.drag = Some(Drag { window: hit.id, terminal: None, last: (pos, 0) });
        self.mouse_call(&format!("(mouse-click {pos} {})", event.click_count.max(1)), cx);
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        if event.pressed_button.is_none() {
            self.drag = None;
            return;
        }
        let Some(hit) = self.hits.iter().find(|h| h.id == drag.window).cloned() else { return };
        let (x, y): (f32, f32) = (event.position.x.into(), event.position.y.into());
        let (line, col, ch) = hit.at(x, y);
        if let Some(button) = drag.terminal {
            if (line, col) != drag.last {
                self.drag = Some(Drag { last: (line, col), ..drag });
                let action = crate::terminal::MouseAction::Drag(button);
                self.editor.lock().terminal_mouse(hit.buffer, action, line, col, mouse_mods(&event.modifiers));
            }
            return;
        }
        let pos = {
            let st = self.editor.lock();
            if st.selected_window() != drag.window {
                return;
            }
            let text = st.buffer().text();
            let line = line.min(text.len_lines().saturating_sub(1));
            let len = text_line_len(text, line);
            // Dragging past the end of a line selects up to its last character.
            text.line_to_char(line) + ch.min(len.saturating_sub(1))
        };
        if pos != drag.last.0 {
            self.drag = Some(Drag { last: (pos, 0), ..drag });
            self.mouse_call(&format!("(mouse-drag {pos})"), cx);
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else { return };
        if let Some(button) = drag.terminal
            && let Some(hit) = self.hits.iter().find(|h| h.id == drag.window)
        {
            let (line, col, _) = hit.at(event.position.x.into(), event.position.y.into());
            let action = crate::terminal::MouseAction::Release(button);
            self.editor.lock().terminal_mouse(hit.buffer, action, line, col, mouse_mods(&event.modifiers));
        }
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
        let n = n as isize;
        let mut st = self.editor.lock();
        if hit.terminal && !event.modifiers.shift {
            let (line, col, _) = hit.at(x, y);
            let action = if n < 0 { crate::terminal::MouseAction::WheelUp } else { crate::terminal::MouseAction::WheelDown };
            let mods = mouse_mods(&event.modifiers);
            if st.terminal_mouse(hit.buffer, action, line, col, mods) {
                for _ in 1..n.unsigned_abs() {
                    st.terminal_mouse(hit.buffer, action, line, col, mods);
                }
                return;
            }
        }
        st.scroll_window_by(hit.id, n);
        drop(st);
        cx.notify();
    }
}

/// A line's length in characters, without its line break.
fn text_line_len(text: &ropey::Rope, line: usize) -> usize {
    text.line(line).to_string().trim_end_matches(['\n', '\r']).chars().count()
}

/// The modifier keys of a mouse event, as terminals report them.
fn mouse_mods(m: &gpui_kit::Modifiers) -> crate::terminal::MouseMods {
    crate::terminal::MouseMods { shift: m.shift, alt: m.alt, control: m.control }
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
        .child(
            h_flex()
                .flex_wrap()
                .children(entries.iter().map(|(key, label)| {
                    let is_group = label.starts_with('+');
                    h_flex()
                        .w(px(col_width))
                        .h(px(ROW_HEIGHT))
                        .gap(px(4.))
                        .child(div().text_color(t.primary).min_w(px(24.)).child(key.clone()))
                        .child(
                            div()
                                .text_color(if is_group { t.primary } else { t.muted_foreground })
                                .child(label.clone()),
                        )
                })),
        )
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
    rope_line
        .to_string()
        .trim_end_matches(['\n', '\r'])
        .replace('\t', "    ")
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

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_theme(window, cx);
        let viewport = window.viewport_size();
        let (width, height): (f32, f32) = (viewport.width.into(), viewport.height.into());
        // The echo area takes the bottom row; windows share the rest.
        let area = (height - ROW_HEIGHT).max(ROW_HEIGHT * 2.);

        let bar = self.lisp.cursor_shape() == "bar";
        let markdown_raw = self.lisp.markdown_raw();
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
        let font = st.font.clone().map(gpui_kit::SharedString::from);
        // Markdown files shown rendered, like the preview: in every window on
        // one, except the window you're typing or selecting in.
        let mut rendered: std::collections::HashMap<crate::editor::WindowId, gpui_kit::Entity<gpui_kit::base::text::TextViewState>> =
            Default::default();
        let selected_now = st.selected_window();
        if st.markdown_rendered {
            // The line you are on, in the theme's selection color: plain to see.
            let highlight = cx.theme().selection;
            for (id, _) in &rects {
                let Some(w) = st.window_by_id(*id).cloned() else { continue };
                if st.is_sidebar(*id) || (*id == selected_now && markdown_raw) {
                    continue;
                }
                let Some(buffer) = st.buffer_by_id(w.buffer) else { continue };
                let is_md = buffer.file().and_then(|f| language_for_path(f)) == Some("markdown");
                if !is_md {
                    continue;
                }
                let source = buffer.text().to_string();
                let revision = buffer.revision();
                let cursor_line = buffer.text().char_to_line(st.window_point(*id).min(buffer.len()));
                let view = self.md_views.entry(w.buffer).or_insert_with(|| {
                    let state = gpui_kit::AppContext::new(cx, |cx| gpui_kit::base::text::TextViewState::markdown(&source, cx));
                    // Its parse lands later: draw again then.
                    let changed = cx.observe(&state, |_, _, cx| cx.notify());
                    MdView { state, revision, revealed: None, _changed: changed }
                });
                if view.revision != revision {
                    view.revision = revision;
                    view.revealed = None;
                    view.state.update(cx, |s, cx| s.set_text(&source, cx));
                }
                // The cursor's line: scrolled to and marked, once the parse matches the text.
                if view.revealed != Some(cursor_line) {
                    let text = view.state.read(cx).rendered_text();
                    if text.source() == source {
                        let rope = buffer.text();
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
                rendered.insert(*id, view.state.clone());
            }
        }
        let live: std::collections::HashSet<crate::editor::BufferId> = st.buffer_ids().into_iter().collect();
        self.md_views.retain(|id, _| live.contains(id));

        // *agent-review* as gpui-kit's Diff, the cursor's row selected in it.
        let review_state = st.review_buffer_id().filter(|id| rects.iter().any(|(w, _)| st.window_by_id(*w).is_some_and(|w| w.buffer == *id))).map(|id| {
            use gpui_kit::component::diff::{DiffFile, DiffLinePosition, DiffLineRange, DiffSide, DiffState};
            let buffer = st.buffer_by_id(id).expect("checked above");
            let revision = buffer.revision();
            let patch: String = buffer.text().to_string().lines().filter(|l| !l.is_empty() && *l != "Nothing to review.").map(|l| format!("{l}\n")).collect();
            let files = DiffFile::parse(&patch).unwrap_or_default();
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
            let window_line = rects
                .iter()
                .find_map(|(w, _)| st.window_by_id(*w).filter(|w| w.buffer == id).map(|w| w.id))
                .map(|w| buffer.text().char_to_line(st.window_point(w).min(buffer.len())))
                .unwrap_or(0);
            if view.selected != Some(window_line) {
                view.selected = Some(window_line);
                if let Some(Some(row)) = st.review_rows.get(window_line) {
                    let side = if row.modified { DiffSide::Modified } else { DiffSide::Original };
                    let (rel, line) = (row.rel.clone(), row.line);
                    view.state.update(cx, |s, cx| {
                        s.set_selected_lines(Some(DiffLineRange::new(rel.clone(), side, line, line)), cx);
                        s.scroll_to_line(DiffLinePosition::new(rel, side, line), cx);
                    });
                }
            }
            view.state.clone()
        });

        let syntax_theme = self.lisp.syntax_theme();
        let syntax = &mut self.syntax;
        let theme = cx.theme();

        let face_specs = self.lisp.face_specs();
        // Where each window ends up scrolled and how many lines it shows,
        // once line heights are known (headings are taller).
        let mut fitted: Vec<(crate::editor::WindowId, usize, usize)> = Vec::new();
        // Where the cursor is on screen (x, line top, line height), for K's popup.
        let mut cursor_at: Option<(f32, f32, f32)> = None;

        // Lines the agent changed and nobody reviewed yet, per window.
        let review_marks: std::collections::HashMap<crate::editor::WindowId, Vec<(usize, bool)>> = rects
            .iter()
            .filter_map(|(id, _)| {
                let buffer = st.window_by_id(*id)?.buffer;
                let marks = st.review_marks(buffer);
                (!marks.is_empty()).then_some((*id, marks))
            })
            .collect();

        // Lines with your comments on them, per window.
        let comment_marks: std::collections::HashMap<crate::editor::WindowId, Vec<usize>> = rects
            .iter()
            .filter_map(|(id, _)| {
                let lines = st.comment_lines(st.window_by_id(*id)?.buffer);
                (!lines.is_empty()).then_some((*id, lines))
            })
            .collect();
        let comments: Vec<crate::editor::ReviewComment> = st.review_comments.clone();
        let review_id = st.review_buffer_id();

        let mut hits: Vec<WindowHit> = Vec::new();
        let terminal_buffers: std::collections::HashSet<crate::editor::BufferId> = st.terminals.iter().map(|t| t.buffer).collect();
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
                // Line numbers, in a gutter: for files, when they're on.
                let numbers = match st.line_numbers.as_str() {
                    "on" | "relative" if buffer.file().is_some() && !is_sidebar && !rendered.contains_key(id) => Some(st.line_numbers == "relative"),
                    _ => None,
                };
                let gutter = numbers.map_or(0, |_| text.len_lines().max(1).to_string().len() + 1);

                let room = (r.h * area - ROW_HEIGHT).max(ROW_HEIGHT);
                let (top, last, rows) = fit_lines(w.scroll_top.min(cur_line), cur_line, hi, room, height_of);
                fitted.push((*id, top, rows));
                {
                    let mut y = r.y * area;
                    let lines = (top..last)
                        .map(|i| {
                            let raw = text.line(i).to_string();
                            let cols = layout_line(raw.trim_end_matches(['\n', '\r']), ops_of(i)).1;
                            let at = (i, y, height_of(i), cols);
                            y += height_of(i);
                            at
                        })
                        .collect();
                    hits.push(WindowHit {
                        id: *id,
                        buffer: w.buffer,
                        left: r.x * width,
                        top: r.y * area,
                        right: (r.x + r.w) * width,
                        bottom: (r.y + r.h) * area,
                        text_left: r.x * width + 8. + gutter as f32 * FONT_SIZE * CHAR_WIDTH,
                        lines,
                        custom: rendered.contains_key(id) || (review_state.is_some() && Some(w.buffer) == review_id),
                        terminal: terminal_buffers.contains(&w.buffer),
                    });
                }
                if is_selected && (top..last).contains(&cur_line) {
                    let raw = text.line(cur_line).to_string();
                    let col = layout_line(raw.trim_end_matches(['\n', '\r']), ops_of(cur_line))
                        .1
                        .get(cur_col)
                        .copied()
                        .unwrap_or(cur_col);
                    let x = r.x * width + 8. + (col + gutter) as f32 * FONT_SIZE * CHAR_WIDTH;
                    let y = r.y * area + (top..cur_line).map(height_of).sum::<f32>();
                    cursor_at = Some((x, y, height_of(cur_line)));
                }

                let lines: Vec<_> = (top..last)
                    .map(|i| {
                        let raw = text.line(i).to_string();
                        let (line, cols) = layout_line(raw.trim_end_matches(['\n', '\r']), ops_of(i));
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
                        // The number beside it: the current line's brighter; relative
                        // numbers count from the cursor (the current line its own).
                        let row = match numbers {
                            Some(relative) => {
                                let n = if relative && i != cur_line { i.abs_diff(cur_line) } else { i + 1 };
                                let shown = if i + 1 == text.len_lines() && text.line(i).len_chars() == 0 { String::new() } else { format!("{n:>width$} ", width = gutter - 1) };
                                h_flex()
                                    .items_end()
                                    .child(
                                        div()
                                            .h(px(height_of(i)))
                                            .flex_shrink_0()
                                            .whitespace_nowrap()
                                            .text_color(if i == cur_line { theme.foreground } else { theme.muted_foreground.opacity(0.7) })
                                            .child(shown),
                                    )
                                    .child(row)
                                    .into_any_element()
                            }
                            None => row.into_any_element(),
                        };
                        let current = is_sidebar && i == cur_line;
                        let marks = review_marks.get(id).map(Vec::as_slice).unwrap_or(&[]);
                        let added = marks.contains(&(i, true));
                        // Markdown code blocks: a band across the window.
                        let code_block = language == Some("markdown")
                            && spans.get(i.wrapping_sub(lo)).is_some_and(|l| l.iter().any(|sp| sp.capture == "markup.raw.block"));
                        let deleted_above = marks.contains(&(i, false));
                        div().relative()
                        .when(code_block, |d| d.bg(theme.foreground.opacity(0.06)))
                        .when(current, |d| {
                            d.bg(if is_selected { theme.list_active } else { theme.list_hover })
                        })
                        // The agent's unreviewed lines: a green band, and a
                        // red rule where it deleted lines.
                        .when(added, |d| d.bg(theme.green.opacity(0.14)).child(
                            div().absolute().left(px(-6.)).top_0().bottom_0().w(px(3.)).bg(theme.green),
                        ))
                        .when(deleted_above, |d| d.child(
                            div().absolute().left(px(-6.)).right_0().top_0().h(px(2.)).bg(theme.red),
                        ))
                        // A comment on this line: a yellow mark in the margin.
                        .when(comment_marks.get(id).is_some_and(|l| l.contains(&i)), |d| d.child(
                            div().absolute().left(px(-6.)).top(px(3.)).w(px(4.)).h(px(10.)).rounded(px(1.)).bg(theme.yellow),
                        ))
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
                        .child(match (rendered.get(id), review_state.as_ref().filter(|_| Some(w.buffer) == review_id)) {
                            (_, Some(state)) => {
                                use gpui_kit::component::diff::{Diff, DiffAnnotation, DiffLinePosition, DiffSide};
                                let annotations: Vec<DiffAnnotation> = comments
                                    .iter()
                                    .map(|c| {
                                        let side = if c.at.modified { DiffSide::Modified } else { DiffSide::Original };
                                        DiffAnnotation::line(gpui_kit::SharedString::from(format!("comment-{}", c.id)), DiffLinePosition::new(c.at.rel.clone(), side, c.at.line))
                                    })
                                    .collect();
                                let texts: std::collections::HashMap<String, String> =
                                    comments.iter().map(|c| (format!("comment-{}", c.id), c.text.clone())).collect();
                                let editor_add = self.editor.clone();
                                let editor_click = self.editor.clone();
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .child(
                                        Diff::new(state)
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
                                            .on_add_annotation(move |range, window, _| {
                                                let side = if range.side() == DiffSide::Modified { "modified" } else { "original" };
                                                editor_add.lock().push_event(
                                                    "lua-event",
                                                    vec!["review_comment_at".into(), range.path().to_string(), side.into(), range.start().to_string()],
                                                );
                                                window.refresh();
                                            })
                                            // A click puts the cursor on that row.
                                            .on_line_click(move |pos, _, window, _| {
                                                editor_click.lock().review_select(pos.path(), pos.side() == DiffSide::Modified, pos.line());
                                                window.refresh();
                                            }),
                                    )
                                    .into_any_element()
                            }
                            (Some(state), None) => {
                            // Rendered Markdown: images, code blocks, tables, HTML.

                                let folder = buffer.file().and_then(|f| f.parent()).map(|d| d.to_path_buf()).unwrap_or_default();
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .px(px(20.))
                                    .py(px(10.))
                                    .font_family(theme.font_family.clone())
                                    .text_size(px(15.))
                                    .line_height(px(22.))
                                    .child(
                                        gpui_kit::base::text::TextView::new(state)
                                            .scrollable(true)
                                            .selectable(false)
                                            .image_source(move |uri| match local_image(&folder, uri.as_ref()) {
                                                Some(path) => path.into(),
                                                None => uri.clone().into(),
                                            }),
                                    )
                                    .into_any_element()
                            }
                            (None, None) => v_flex().flex_1().overflow_hidden().px(px(8.)).children(lines).into_any_element(),
                        })
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

        self.hits = hits;
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
        // Which-key popup, just above the echo area when a prefix key is pending.
        let whichkey = st.whichkey.as_ref().map(|wk| whichkey_element(wk, width, cx));
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
        // The Markdown preview, rendered by gpui-kit, on the right.
        let preview_target = st.preview_buffer();
        if preview_target.is_some() {
            st.preview_last = preview_target;
        }
        let preview = st.preview_rect().zip(preview_target).map(|(r, id)| {
            let source = st.buffer_by_id(id).map(|b| b.text().to_string()).unwrap_or_default();
            div()
                .absolute()
                .left(px(r.x * width))
                .top_0()
                .w(px(r.w * width))
                .h(px(area))
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.background)
                .font_family(theme.font_family.clone())
                .text_size(px(15.))
                .line_height(px(22.))
                .px(px(20.))
                .py(px(12.))
                .child({
                    // Images by path load from the file's folder (README's assets/...).
                    let folder = st
                        .buffer_by_id(id)
                        .and_then(|b| b.file())
                        .and_then(|f| f.parent())
                        .map(|d| d.to_path_buf())
                        .unwrap_or_default();
                    gpui_kit::base::TextView::markdown(
                        gpui_kit::SharedString::from(format!("markdown-preview-{id}")),
                        source,
                    )
                    .scrollable(true)
                    .image_source(move |uri| match local_image(&folder, uri.as_ref()) {
                        Some(path) => path.into(),
                        None => uri.clone().into(),
                    })
                })
        });
        drop(st);

        v_flex()
            .relative()
            .id("vmac")
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_key))
            .on_action(cx.listener(|shell, _: &TabKey, _, cx| shell.dispatch("TAB", "", cx)))
            .on_action(cx.listener(|shell, _: &ShiftTabKey, _, cx| shell.dispatch("S-TAB", "", cx)))
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
        assert_eq!(fit_lines(0, 1, 10, room, h), (0, 3, 3), "heading + 2 lines fit");
        assert_eq!(fit_lines(0, 3, 10, room, h), (1, 5, 4), "the cursor on line 3 scrolls the heading off");
        assert_eq!(fit_lines(0, 0, 1, room, h), (0, 1, 3), "stops drawing at the buffer's end, with room below the heading");
        let huge = |_| 10. * ROW_HEIGHT;
        assert_eq!(fit_lines(0, 0, 10, room, huge), (0, 1, 1), "always at least one line");
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
            lines: vec![
                (10, 0., 40., display_columns("# Hi")),
                (11, 40., 20., display_columns("\tx")),
            ],
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
        let folder = std::path::Path::new("/notes");
        assert_eq!(local_image(folder, "assets/logo.svg"), Some("/notes/assets/logo.svg".into()));
        assert_eq!(local_image(folder, "file:///tmp/a%20b.png"), Some("/tmp/a b.png".into()));
        assert_eq!(local_image(folder, "https://x.dev/a.png"), None);
        // Hidden markup takes no room; a replacement takes one column.
        let (text, cols) = layout_line("## **Hi**\t-", &[(0, None), (1, None), (2, None), (3, None), (4, None), (7, None), (8, None), (10, Some('•'))]);
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
