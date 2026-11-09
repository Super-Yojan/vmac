//! The line between the editor and the window that shows it.
//!
//! The window (`shell.rs`) sends `Input`s (keys, the mouse, what it knows
//! about the machine it runs on) and draws `Frame`s: everything on screen,
//! laid out, as plain data. The editor side (`session.rs`) owns the buffers,
//! the scripting engine, language servers and terminals, and never sees GPUI.
//! Both are plain data so that they can later be sent between processes (a
//! daemon, a remote machine).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A text row's height, the font size, and a monospace character's width
/// as a share of the font size (Menlo, SF Mono), in pixels.
pub const ROW_HEIGHT: f32 = 20.;
pub const FONT_SIZE: f32 = 14.;
pub const CHAR_WIDTH: f32 = 0.6;
/// How wide the sidebar (the file tree) is.
pub const SIDEBAR_WIDTH: f32 = 260.;
/// The completion popup's most rows.
pub const COMPLETION_ROWS: usize = 10;

pub type WindowId = u64;
pub type BufferId = u64;

/// The window's size in pixels.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

/// The modifier keys held with a mouse event.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

/// What the window sends. Mouse positions are already resolved to a window's
/// buffer line, screen column and character on that line (`ch`), from the
/// frame last drawn.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Input {
    /// A key in Emacs notation ("C-r", "SPC") and the text it types.
    Key { key: String, text: String },
    MouseDown { window: WindowId, button: u8, line: usize, col: usize, ch: usize, clicks: usize, mods: Mods },
    MouseDrag { window: WindowId, line: usize, col: usize, ch: usize, mods: Mods },
    MouseUp { window: WindowId, line: usize, col: usize, mods: Mods },
    /// The button came up somewhere the window didn't see: the drag is over.
    MouseLost,
    /// Whole lines of wheel movement over a window (negative: up).
    Wheel { window: WindowId, lines: isize, line: usize, col: usize, mods: Mods },
    /// A click on a row of the review's diff, and its + (a comment on it).
    ReviewSelect { path: String, modified: bool, line: usize },
    /// The Diff's + beside a line, in the window on `buffer`.
    ReviewCommentAt {
        path: String,
        modified: bool,
        line: usize,
        #[serde(default)]
        buffer: BufferId,
    },
    /// What the window's machine has: its fonts, the themes it loaded
    /// (name, dark?), and theme files that didn't load.
    Client { fonts: Vec<String>, themes: Vec<(String, bool)>, errors: Vec<String> },
    /// A double-click on a PDF's page: the page (from 1) and the point, in
    /// PDF points from its top left (SyncTeX finds the source there).
    PdfDoubleClick { window: WindowId, page: usize, x: f32, y: f32 },
    /// Whether the system is in dark mode.
    Appearance { dark: bool },
    /// The theme's text and background colors (0xRRGGBB), for terminal
    /// programs that ask.
    TerminalColors { foreground: u32, background: u32 },
}

/// Everything on screen.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Frame {
    /// The height the windows share (the echo area takes a row below).
    pub area: f32,
    /// The editor's font (None: the theme's monospace font).
    pub font: Option<String>,
    /// A thin bar cursor rather than a box.
    pub bar: bool,
    pub theme: ThemeRequest,
    pub windows: Vec<FrameWindow>,
    /// Where the cursor is on screen (x, line top, line height), for popups.
    pub cursor_at: Option<(f32, f32, f32)>,
    pub echo: Echo,
    /// A prompt or the picker has the keyboard (and the mouse does nothing).
    pub modal: bool,
    /// K's documentation (Markdown).
    pub hover: Option<String>,
    pub completion: Option<Vec<CompletionRow>>,
    pub whichkey: Option<Vec<(String, String)>>,
    pub picker: Option<PickerView>,
    pub preview: Option<PreviewPane>,
    /// The buffers that exist, so the window can drop what it kept for others.
    pub buffers: Vec<BufferId>,
    /// The server's sessions, and the one shown (for a window that shows
    /// several machines' sessions).
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
    #[serde(default)]
    pub session: String,
}

/// A session, as the switcher and `vmac ls` describe it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    /// Its project (or working) folder, with ~ for home.
    pub project: String,
    pub buffers: usize,
    /// Files changed and not saved.
    pub unsaved: usize,
}

/// The theme mode ("system", "light" or "dark") and the theme for each.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ThemeRequest {
    pub mode: String,
    pub light: String,
    pub dark: String,
}

/// An editor window, in pixels.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FrameWindow {
    pub id: WindowId,
    pub buffer: BufferId,
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    /// Where text starts, after the padding and line numbers.
    pub text_left: f32,
    pub selected: bool,
    pub sidebar: bool,
    pub terminal: bool,
    pub content: Content,
    pub modeline: Vec<Segment>,
}

/// Text in a face.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Segment {
    pub text: String,
    pub face: String,
}

/// What a window shows: lines of text, or a view the window draws itself.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Content {
    Text(Vec<FrameLine>),
    /// A Markdown file shown rendered: its text, which version of it, the
    /// cursor's line, and the folder images are found from.
    Markdown { source: String, revision: u64, cursor_line: usize, folder: PathBuf },
    Review(ReviewView),
    /// An image or a PDF, drawn by the window from the file; a PDF's pages
    /// in the theme's colors when `themed`.
    Media {
        path: PathBuf,
        kind: MediaKind,
        #[serde(default)]
        themed: bool,
        /// The latest scrolls asked for by keys, numbered so the window
        /// does each once (several may come between two frames).
        #[serde(default)]
        scroll: Vec<(u64, MediaScroll)>,
        /// The zoom: 1 fits the image, or the PDF's pages, to the window.
        #[serde(default = "default_scale")]
        zoom: f32,
    },
}

/// A move through an image or PDF, asked for by keys (j, C-d, G...).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum MediaScroll {
    /// Steps of a couple of rows (negative: up).
    Lines(i32),
    /// Window heights.
    Screens(f32),
    /// Steps sideways, across pages zoomed wider than the window
    /// (negative: left).
    Sideways(i32),
    /// Pages from the one at the top.
    Pages(i32),
    /// To a page (from 0), or the end.
    Page(usize),
    End,
    /// To a spot on a page (from 0), marked for a moment: a box in PDF
    /// points from the page's top left (SyncTeX's forward search). With
    /// `follow` (following the cursor), only when it's out of view.
    Reveal { page: usize, x: f32, y: f32, width: f32, height: f32, #[serde(default)] follow: bool },
}

/// Files shown as what they are rather than as text.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Pdf,
}

/// Whether a file is an image or a PDF, by its extension. SVG is left out:
/// it is text, and edited as such.
pub fn media_kind(path: &std::path::Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "ico" => Some(MediaKind::Image),
        "pdf" => Some(MediaKind::Pdf),
        _ => None,
    }
}

/// *agent-review*, drawn as a diff.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ReviewView {
    /// What's above the diff (a commit's message and stats).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub header: String,
    pub patch: String,
    pub revision: u64,
    /// The cursor's line, and the diff row it is on.
    pub cursor_line: usize,
    pub row: Option<DiffRow>,
    pub comments: Vec<DiffComment>,
}

/// A line of a file in the diff: its path, new (modified) or old side, 1-based line.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DiffRow {
    pub rel: String,
    pub modified: bool,
    pub line: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DiffComment {
    pub id: u64,
    pub at: DiffRow,
    pub text: String,
    /// Who or what it is ("comment" when empty): an author, "pending".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
}

/// One line of a window, in screen columns.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct FrameLine {
    /// Its buffer line, top and height in pixels, and the screen column
    /// each character starts at (one more past the end): for the mouse.
    /// Empty when that's just 0, 1, 2... (see `columns`).
    pub line: usize,
    pub top: f32,
    pub height: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cols: Vec<usize>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<usize>,
    /// The selected columns; may include the line break (one past the end).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<(usize, usize)>,
    /// Search matches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<(usize, usize)>,
    /// Faces over columns; later ones win.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub styles: Vec<(usize, usize, Face)>,
    /// Diagnostic squiggles, by severity (1 error ... 4 hint).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<(usize, usize, u8)>,
    /// The line number beside it (when they're on), and whether it's the cursor's line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<String>,
    #[serde(default, skip_serializing_if = "no")]
    pub number_current: bool,
    /// The sidebar's cursor line.
    #[serde(default, skip_serializing_if = "no")]
    pub current: bool,
    /// Agent changes not reviewed yet: an added line, deleted lines above it.
    #[serde(default, skip_serializing_if = "no")]
    pub added: bool,
    #[serde(default, skip_serializing_if = "no")]
    pub deleted_above: bool,
    /// One of your comments is on it.
    #[serde(default, skip_serializing_if = "no")]
    pub comment: bool,
    /// How it differs from git's index: 0 not, 1 added, 2 changed, 3
    /// lines deleted above it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub git_sign: u8,
    /// Dim text after the line's end (who last changed it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
    /// Inside a Markdown code block.
    #[serde(default, skip_serializing_if = "no")]
    pub code_block: bool,
    /// A box behind some of its columns (a chat bubble, a card, a prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<Band>,
}

/// A tinted box behind columns `from`..`to` of a line. Lines in a row with
/// the same box join into one, rounded at its first and last.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Band {
    pub from: usize,
    pub to: usize,
    /// bubble, card, ask or input.
    pub kind: String,
    pub first: bool,
    pub last: bool,
}

/// How a run of text looks. Colors are names the window resolves against
/// its theme ("accent", "dim", "red"...) or "#rrggbb".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Face {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(default = "default_scale", skip_serializing_if = "one")]
    pub scale: f32,
    #[serde(default, skip_serializing_if = "no")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "no")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "no")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "no")]
    pub sans: bool,
}

fn no(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

fn one(x: &f32) -> bool {
    *x == 1.
}

fn default_scale() -> f32 {
    1.
}

impl FrameLine {
    /// The screen column each character starts at (one more past the end).
    pub fn columns(&self) -> Vec<usize> {
        if self.cols.is_empty() { (0..=self.text.chars().count()).collect() } else { self.cols.clone() }
    }
}

/// What the bottom row says.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Echo {
    Empty,
    Message(String),
    /// A minibuffer prompt, its text and the cursor's column.
    Prompt { prompt: String, text: String, cursor: usize },
    /// The diagnostic under the cursor.
    Diagnostic { severity: u8, label: String, message: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CompletionRow {
    pub label: String,
    pub detail: String,
    pub kind: String,
    pub selected: bool,
}

/// The fuzzy picker: the rows that fit, already scrolled.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PickerView {
    pub title: String,
    pub matched: usize,
    pub total: usize,
    pub query: String,
    pub cursor: usize,
    pub panel_height: f32,
    /// Items, their matched characters, and whether each is selected.
    pub rows: Vec<(String, Vec<usize>, bool)>,
    /// The selected file's lines, and the one a reference points at.
    pub preview: Option<(Vec<String>, Option<usize>)>,
}

/// The rendered Markdown pane on the right.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PreviewPane {
    pub left: f32,
    pub width: f32,
    pub buffer: BufferId,
    pub source: String,
    pub folder: PathBuf,
}
