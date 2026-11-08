//! Images and PDFs in a window: what `Content::Media` asks the window to draw.
//! An image is fitted to the window; a PDF's pages are stacked to scroll
//! through, each drawn (off the main thread) when it first comes into view.
//! Both are read again when their file changes, as LaTeX or Typst rebuild
//! it, once it has stopped changing (so not half written), keeping the
//! place and showing the last good version until the new one is ready.

use gpui_kit::component::ActiveTheme as _;
use crate::frame::MediaScroll;
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ListAlignment, ListOffset, ListState, ObjectFit,
    ParentElement as _, Render, RenderImage, ScrollHandle, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Window, div, img, list, point, prelude::FluentBuilder as _, px,
};
use crate::frame::{ROW_HEIGHT, WindowId};
use gpui_kit::{App, DispatchPhase, MouseButton, MouseDownEvent, ScrollWheelEvent, canvas};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// The widest a page is shown at zoom 1, in points on screen.
const PAGE_MAX_WIDTH: f32 = 900.;
/// The space around pages, and the most pixels a page is drawn across
/// (a 300% zoom of a wide page on a Retina display is near it).
const PAGE_MARGIN: f32 = 16.;
const MAX_PIXELS: f32 = 6000.;
/// Pages are drawn again for a new zoom in steps of this many pixels, not
/// for every pixel a window is resized by.
const PIXEL_STEP: f32 = 128.;
/// Pages kept drawn either side of the first one in view; further ones are
/// dropped and drawn again if you come back.
const KEEP_AROUND: usize = 4;
/// How far j and k scroll, in points.
const LINE_STEP: f32 = 48.;
/// How often a shown file is checked for changes.
const WATCH_POLL: Duration = Duration::from_millis(300);

/// A file's modification time and length: it changed when this did.
type Stamp = (SystemTime, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// When a file being rewritten is ready to read again: it changed from the
/// version shown, and has stayed the same since the last look (a compiler
/// writing it has finished). A version that wouldn't read isn't tried
/// again; the next change is.
#[derive(Debug, Default)]
struct Watch {
    shown: Option<Stamp>,
    seen: Option<Stamp>,
    failed: Option<Stamp>,
}

impl Watch {
    /// Given the file's stamp now: whether to read it.
    fn poll(&mut self, now: Option<Stamp>) -> bool {
        let settled = now == self.seen;
        self.seen = now;
        now.is_some() && settled && now != self.shown && now != self.failed
    }

    fn read(&mut self, stamp: Option<Stamp>, ok: bool) {
        if ok {
            self.shown = stamp;
        } else {
            self.failed = stamp;
        }
    }
}

/// Calls `poll` on a view every `WATCH_POLL` while it lives.
fn watch<T: 'static>(cx: &mut Context<T>, poll: fn(&mut T, &mut Context<T>)) {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(WATCH_POLL).await;
            if this.update(cx, |view, cx| poll(view, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// An image file's frames (one, or a GIF's), BGRA as GPUI draws them.
fn decode(path: &Path) -> Result<Vec<image::Frame>, String> {
    use image::AnimationDecoder as _;
    let bytes = std::fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let bad = |e: image::ImageError| format!("can't show {}: {e}", path.display());
    let format = image::guess_format(&bytes).map_err(bad)?;
    let mut frames = if format == image::ImageFormat::Gif {
        image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&bytes))
            .and_then(|d| d.into_frames().collect_frames())
            .map_err(bad)?
    } else {
        vec![image::Frame::new(image::load_from_memory_with_format(&bytes, format).map_err(bad)?.into_rgba8())]
    };
    for frame in &mut frames {
        for pixel in frame.buffer_mut().pixels_mut() {
            pixel.0.swap(0, 2);
        }
    }
    if frames.is_empty() {
        return Err(format!("{} has no picture in it", path.display()));
    }
    Ok(frames)
}

/// A drawn PDF page: how many pixels wide, for which version of the file
/// and colors, and the picture.
struct Page {
    width: u32,
    generation: u64,
    picture: Arc<RenderImage>,
}

/// A PDF's pages, drawn as they scroll into view.
pub struct PdfView {
    path: PathBuf,
    watch: Watch,
    /// Each page's size in points, or why the file can't be shown.
    sizes: Result<Vec<(f32, f32)>, String>,
    /// Drawn pages. One drawn for another zoom, or before the file or the
    /// colors changed, is shown (stretched) until it's drawn again.
    pages: HashMap<usize, Page>,
    /// Pages being drawn: which, how wide, for which generation.
    drawing: HashSet<(usize, u32, u64)>,
    failed: HashMap<usize, String>,
    list: ListState,
    /// The theme's text and background colors (0xRRGGBB) pages are drawn
    /// in, or None for black on white.
    colors: Option<(u32, u32)>,
    /// Bumped whenever drawn pages go stale (the file or the colors
    /// changed): they're drawn again, and ones being drawn are thrown away.
    generation: u64,
    /// The number of the last scroll done (`Content::Media`'s `scroll`).
    scrolled: u64,
    /// The zoom (1 fits pages to the window), the window's width, and the
    /// display's pixels per point.
    zoom: f32,
    width: f32,
    pixels_per_point: f32,
    /// Sideways, across pages wider than the window.
    sideways: ScrollHandle,
    /// The window it was last shown in, and what to call with Ctrl and the
    /// wheel over it (whole lines of it; negative: up).
    window: WindowId,
    on_zoom: OnZoom,
    /// Ctrl-wheel movement short of a whole line, kept for the next event.
    zoom_wheel: Rc<Cell<f32>>,
    /// What to call with a double-click on a page (SyncTeX).
    on_double_click: OnPageClick,
    /// A box marked on a page for a moment (forward search): the page, and
    /// x, y, width, height in PDF points; numbered, so the timer that clears
    /// it leaves a newer one.
    highlight: Option<(usize, (f32, f32, f32, f32))>,
    highlights: u64,
}

/// What a PDF view calls with a double-click on a page: the window, the
/// page (from 1), and the point in PDF points from the page's top left.
pub type OnPageClick = Rc<dyn Fn(WindowId, usize, f32, f32, &mut App)>;

impl PdfView {
    pub fn new(path: PathBuf, scrolled: u64, on_zoom: OnZoom, on_double_click: OnPageClick) -> Self {
        let now = stamp(&path);
        let sizes = crate::pdf::page_sizes(&path);
        let count = sizes.as_ref().map_or(0, Vec::len);
        let mut watch = Watch { seen: now, ..Default::default() };
        watch.read(now, sizes.is_ok());
        Self {
            watch,
            path,
            sizes,
            pages: HashMap::new(),
            drawing: HashSet::new(),
            failed: HashMap::new(),
            list: ListState::new(count, ListAlignment::Top, px(1200.)),
            colors: None,
            generation: 0,
            scrolled,
            zoom: 1.,
            width: 800.,
            pixels_per_point: 2.,
            sideways: ScrollHandle::new(),
            window: 0,
            on_zoom,
            zoom_wheel: Rc::new(Cell::new(0.)),
            on_double_click,
            highlight: None,
            highlights: 0,
        }
    }

    pub fn set_window(&mut self, window: WindowId) {
        self.window = window;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Checks its file for changes from now on.
    pub fn start_watching(cx: &mut Context<Self>) {
        watch(cx, Self::poll);
    }

    /// Reads the file again once it changed and settled (a PDF rebuilt by
    /// LaTeX or Typst), keeping the place. One that doesn't read leaves
    /// the last good version shown.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let now = stamp(&self.path);
        if !self.watch.poll(now) {
            return;
        }
        let sizes = crate::pdf::page_sizes(&self.path).and_then(|s| {
            if s.is_empty() { Err(format!("{} has no pages", self.path.display())) } else { Ok(s) }
        });
        self.watch.read(now, sizes.is_ok());
        match sizes {
            Ok(sizes) => self.reload(sizes, cx),
            // Nothing shown yet (it didn't exist, or didn't read): say why.
            Err(e) if self.sizes.is_err() => {
                self.sizes = Err(e);
                cx.notify();
            }
            Err(_) => {}
        }
    }

    fn reload(&mut self, sizes: Vec<(f32, f32)>, cx: &mut Context<Self>) {
        let top = self.list.logical_scroll_top();
        let count = sizes.len();
        let same_count = self.sizes.as_ref().is_ok_and(|s| s.len() == count);
        self.sizes = Ok(sizes);
        self.stale();
        for ix in self.pages.keys().copied().filter(|ix| *ix >= count).collect::<Vec<_>>() {
            if let Some(page) = self.pages.remove(&ix) {
                cx.drop_image(page.picture, None);
            }
        }
        if same_count {
            self.list.remeasure();
        } else {
            self.list.reset(count);
        }
        let item_ix = top.item_ix.min(count.saturating_sub(1));
        self.list.scroll_to(ListOffset { item_ix, offset_in_item: top.offset_in_item });
        cx.notify();
    }

    /// Takes the theme's colors (None: black on white); the pages are drawn
    /// again in them.
    pub fn set_colors(&mut self, colors: Option<(u32, u32)>, cx: &mut Context<Self>) {
        if colors != self.colors {
            self.colors = colors;
            self.stale();
            cx.notify();
        }
    }

    /// Every drawn page is out of date: each is drawn again when shown.
    fn stale(&mut self) {
        self.generation += 1;
        self.failed.clear();
        self.drawing.clear();
    }

    /// How wide page `ix` is shown, in points: fitted to the window (no
    /// wider than 900 or its own width), times the zoom.
    fn page_width(&self, ix: usize) -> f32 {
        let own = self.sizes.as_ref().ok().and_then(|s| s.get(ix)).map_or(612., |s| s.0);
        let room = (self.width - 2. * PAGE_MARGIN).max(100.);
        own.min(PAGE_MAX_WIDTH).min(room) * self.zoom
    }

    /// How wide the pages are laid out: the widest page, or the window.
    fn content_width(&self) -> f32 {
        let count = self.sizes.as_ref().map_or(0, Vec::len);
        let widest = (0..count).map(|ix| self.page_width(ix)).fold(0., f32::max);
        (widest + 2. * PAGE_MARGIN).max(self.width)
    }

    /// Takes a new zoom or window width, keeping the place: the same spot
    /// on the page at the top, and in the middle sideways.
    pub fn set_size(&mut self, zoom: f32, width: f32, cx: &mut Context<Self>) {
        if zoom == self.zoom && width == self.width {
            return;
        }
        let before = self.page_width(self.list.logical_scroll_top().item_ix);
        let middle = -f32::from(self.sideways.offset().x) + self.width / 2.;
        let content_before = self.content_width();
        (self.zoom, self.width) = (zoom, width);
        let top = self.list.logical_scroll_top();
        let ratio = self.page_width(top.item_ix) / before.max(1.);
        // Pages are taller or shorter now.
        self.list.remeasure();
        self.list.scroll_to(ListOffset { item_ix: top.item_ix, offset_in_item: top.offset_in_item * ratio });
        let middle = middle * self.content_width() / content_before.max(1.);
        let x = (middle - self.width / 2.).clamp(0., (self.content_width() - self.width).max(0.));
        self.sideways.set_offset(point(px(-x), px(0.)));
        cx.notify();
    }

    /// Scrolls a box on a page into view, a third of the way down the
    /// window (and to its middle sideways when zoomed wide), and marks it
    /// for a moment.
    fn reveal(&mut self, ix: usize, b: (f32, f32, f32, f32), cx: &mut Context<Self>) {
        let Some(&(own, _)) = self.sizes.as_ref().ok().and_then(|s| s.get(ix)) else { return };
        let (x, y, width, _) = b;
        let scale = self.page_width(ix) / own.max(1.);
        let top = if ix == 0 { PAGE_MARGIN } else { 0. };
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let viewport = if viewport > 0. { viewport } else { 600. };
        let offset = (top + y * scale - viewport / 3.).max(0.);
        self.list.scroll_to(ListOffset { item_ix: ix, offset_in_item: px(offset) });
        let content = self.content_width();
        let room = (content - self.width).max(0.);
        if room > 0. {
            let middle = (content - self.page_width(ix)) / 2. + (x + width / 2.) * scale;
            self.sideways.set_offset(point(px(-(middle - self.width / 2.).clamp(0., room)), px(0.)));
        }
        self.highlight = Some((ix, b));
        self.highlights += 1;
        let n = self.highlights;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            let _ = this.update(cx, |view, cx| {
                if view.highlights == n {
                    view.highlight = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Does the scrolls asked for since the last one done.
    pub fn scroll(&mut self, asked: &[(u64, MediaScroll)], cx: &mut Context<Self>) {
        let done = self.scrolled;
        for &(n, how) in asked.iter().filter(|(n, _)| *n > done) {
            self.scrolled = n;
            let count = self.list.item_count();
            let top = self.list.logical_scroll_top();
            let page = |item_ix: usize| ListOffset { item_ix: item_ix.min(count.saturating_sub(1)), offset_in_item: px(0.) };
            match how {
                MediaScroll::Lines(lines) => self.list.scroll_by(px(lines as f32 * LINE_STEP)),
                MediaScroll::Screens(screens) => {
                    let height: f32 = self.list.viewport_bounds().size.height.into();
                    self.list.scroll_by(px(screens * height.max(LINE_STEP)));
                }
                MediaScroll::Sideways(steps) => {
                    let room = (self.content_width() - self.width).max(0.);
                    let x = (-f32::from(self.sideways.offset().x) + steps as f32 * LINE_STEP).clamp(0., room);
                    self.sideways.set_offset(point(px(-x), px(0.)));
                }
                // Back from partway down a page: that page's top first.
                MediaScroll::Pages(n) if n < 0 && top.offset_in_item > px(1.) => {
                    self.list.scroll_to(page((top.item_ix + 1).saturating_sub(n.unsigned_abs() as usize)))
                }
                MediaScroll::Pages(n) => self.list.scroll_to(page(top.item_ix.saturating_add_signed(n as isize))),
                MediaScroll::Page(ix) => self.list.scroll_to(page(ix)),
                MediaScroll::End => self.list.scroll_to_end(),
                MediaScroll::Reveal { page, x, y, width, height } => self.reveal(page, (x, y, width, height), cx),
            }
        }
    }

    /// Draws page `ix` off the main thread as wide as it's shown, unless it
    /// is drawn that wide or on its way.
    fn draw(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.sizes.is_err() {
            return;
        }
        let pixels = (self.page_width(ix) * self.pixels_per_point).min(MAX_PIXELS);
        let width = ((pixels / PIXEL_STEP).ceil() * PIXEL_STEP) as u32;
        let generation = self.generation;
        let drawn = self.pages.get(&ix).is_some_and(|p| p.width == width && p.generation == generation);
        if drawn || self.failed.contains_key(&ix) || !self.drawing.insert((ix, width, generation)) {
            return;
        }
        let path = self.path.clone();
        let colors = self.colors;
        cx.spawn(async move |this, cx| {
            let drawn = cx
                .background_executor()
                .spawn(async move {
                    let mut drawn = crate::pdf::render_page(&path, ix, width)?;
                    if let Some((foreground, background)) = colors {
                        crate::pdf::recolor(&mut drawn.2, foreground, background);
                    }
                    Ok::<_, String>(drawn)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.drawing.remove(&(ix, width, generation));
                // The file or the colors changed while it was drawn: it's stale.
                if view.generation != generation {
                    return;
                }
                match drawn.and_then(|(w, h, pixels)| {
                    let buffer = image::RgbaImage::from_raw(w, h, pixels).ok_or("bad bitmap")?;
                    Ok(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
                }) {
                    Ok(picture) => {
                        if let Some(old) = view.pages.insert(ix, Page { width, generation, picture }) {
                            cx.drop_image(old.picture, None);
                        }
                    }
                    Err(e) => {
                        view.failed.insert(ix, e);
                    }
                }
                view.forget_far_pages(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Drops the pictures of pages far from the ones in view (and frees
    /// what the GPU kept of them).
    fn forget_far_pages(&mut self, cx: &mut Context<Self>) {
        let top = self.list.logical_scroll_top().item_ix;
        let keep = top.saturating_sub(KEEP_AROUND)..=top + 2 * KEEP_AROUND;
        let far: Vec<usize> = self.pages.keys().copied().filter(|ix| !keep.contains(ix)).collect();
        for ix in far {
            if let Some(page) = self.pages.remove(&ix) {
                cx.drop_image(page.picture, None);
            }
        }
    }

    fn page(&mut self, ix: usize, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.draw(ix, cx);
        let (w, h) = self.sizes.as_ref().ok().and_then(|s| s.get(ix)).copied().unwrap_or((612., 792.));
        let shown = self.page_width(ix);
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let (paper, ink) = match self.colors {
            Some(_) => (theme.background, theme.foreground),
            None => (gpui_kit::white(), gpui_kit::black()),
        };
        let body = match (self.pages.get(&ix), self.failed.get(&ix)) {
            (Some(page), _) => img(page.picture.clone()).size_full().object_fit(ObjectFit::Fill).into_any_element(),
            (None, Some(e)) => div().p(px(12.)).text_color(ink).child(e.clone()).into_any_element(),
            (None, None) => div().into_any_element(),
        };
        div()
            .w_full()
            .px(px(PAGE_MARGIN))
            .pt(px(if ix == 0 { PAGE_MARGIN } else { 0. }))
            .pb(px(PAGE_MARGIN))
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .flex_none()
                    .relative()
                    .w(px(shown))
                    .aspect_ratio(w / h.max(1.))
                    .bg(paper)
                    .border_1()
                    .border_color(border)
                    .shadow_sm()
                    .child(body)
                    .children(self.highlight.filter(|(page, _)| *page == ix).map(|(_, (x, y, bw, bh))| {
                        // The box forward search found, a little larger than its text.
                        let scale = shown / w.max(1.);
                        let accent = cx.theme().primary;
                        div()
                            .absolute()
                            .left(px(x * scale - 3.))
                            .top(px(y * scale - 3.))
                            .w(px(bw * scale + 6.))
                            .h(px(bh * scale + 6.))
                            .rounded(px(3.))
                            .bg(accent.opacity(0.25))
                            .border_1()
                            .border_color(accent)
                    }))
                    .child(self.double_click_catcher(ix, (w, h))),
            )
            .child(div().pt(px(4.)).text_xs().text_color(muted).child(format!("{}", ix + 1)))
            .into_any_element()
    }
}

impl PdfView {
    /// Hears double-clicks on page `ix` (`size` in points), where it's drawn
    /// and in view, and hands on the point on the page.
    fn double_click_catcher(&self, ix: usize, size: (f32, f32)) -> impl IntoElement {
        let (window_id, on_click) = (self.window, self.on_double_click.clone());
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                // Only the part of the page in view (it's clipped to the list).
                let visible = bounds.intersect(&window.content_mask().bounds);
                window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                    let hit = event.button == MouseButton::Left && event.click_count == 2;
                    if phase != DispatchPhase::Bubble || !hit || !visible.contains(&event.position) {
                        return;
                    }
                    let width = f32::from(bounds.size.width).max(1.);
                    let height = f32::from(bounds.size.height).max(1.);
                    let x = f32::from(event.position.x - bounds.origin.x) / width * size.0;
                    let y = f32::from(event.position.y - bounds.origin.y) / height * size.1;
                    on_click(window_id, ix + 1, x, y, cx);
                });
            },
        )
        .absolute()
        .size_full()
    }

    fn zoom_catcher(&self) -> impl IntoElement {
        zoom_catcher(self.window, self.on_zoom.clone(), self.zoom_wheel.clone())
    }
}

/// Catches Ctrl and the wheel over a view before what's in it sees it (it
/// would scroll), and hands it on as whole lines to zoom by.
fn zoom_catcher(
    window_id: WindowId,
    on_zoom: Rc<dyn Fn(WindowId, isize, &mut App)>,
    pending: Rc<Cell<f32>>,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| {
                if phase != DispatchPhase::Capture || !event.modifiers.control || !bounds.contains(&event.position) {
                    return;
                }
                cx.stop_propagation();
                let dy: f32 = event.delta.pixel_delta(px(ROW_HEIGHT)).y.into();
                // Up (content down) is negative lines, as the session counts them.
                let lines = pending.get() - dy / ROW_HEIGHT;
                let whole = lines.trunc();
                pending.set(lines - whole);
                if whole != 0. {
                    on_zoom(window_id, whole as isize, cx);
                }
            });
        },
    )
    .absolute()
    .size_full()
}

/// What a view calls with Ctrl and the wheel over it: the window, and whole
/// lines of wheel (negative: up).
pub type OnZoom = Rc<dyn Fn(WindowId, isize, &mut App)>;

/// An image, fitted to the window (never larger than it is) times its zoom;
/// larger than the window, it scrolls both ways.
pub struct ImageView {
    path: PathBuf,
    watch: Watch,
    /// The picture, decoded here (so a new version replaces it, rather than
    /// GPUI's copy kept by path), and its size in pixels (drawn a point
    /// each at zoom 1). None until it's read.
    picture: Option<Arc<RenderImage>>,
    size: Option<(f32, f32)>,
    /// Why it can't be shown, when there's no picture.
    error: Option<String>,
    reading: bool,
    zoom: f32,
    /// The window's width and the height under its modeline.
    width: f32,
    height: f32,
    scroll: ScrollHandle,
    scrolled: u64,
    window: WindowId,
    on_zoom: OnZoom,
    zoom_wheel: Rc<Cell<f32>>,
}

impl ImageView {
    pub fn new(path: PathBuf, scrolled: u64, on_zoom: OnZoom) -> Self {
        Self {
            path,
            watch: Watch::default(),
            picture: None,
            size: None,
            error: None,
            reading: false,
            zoom: 1.,
            width: 800.,
            height: 600.,
            scroll: ScrollHandle::new(),
            scrolled,
            window: 0,
            on_zoom,
            zoom_wheel: Rc::new(Cell::new(0.)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_window(&mut self, window: WindowId) {
        self.window = window;
    }

    /// Reads the file now, and again whenever it changes and settles.
    pub fn start_watching(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |view, cx| {
                let now = stamp(&view.path);
                view.watch.seen = now;
                view.read(now, cx);
            });
        })
        .detach();
        watch(cx, Self::poll);
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let now = stamp(&self.path);
        if !self.reading && self.watch.poll(now) {
            self.read(now, cx);
        }
    }

    /// Decodes the file off the main thread. A version that won't decode
    /// leaves the last good one shown.
    fn read(&mut self, now: Option<Stamp>, cx: &mut Context<Self>) {
        self.reading = true;
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let frames = cx.background_executor().spawn(async move { decode(&path) }).await;
            let _ = this.update(cx, |view, cx| {
                view.reading = false;
                view.watch.read(now, frames.is_ok());
                match frames {
                    Ok(frames) => {
                        let (w, h) = frames[0].buffer().dimensions();
                        view.size = Some((w as f32, h as f32));
                        view.error = None;
                        if let Some(old) = view.picture.replace(Arc::new(RenderImage::new(frames))) {
                            cx.drop_image(old, None);
                        }
                    }
                    Err(e) if view.picture.is_none() => view.error = Some(e),
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// How large it's shown, in points.
    fn shown(&self) -> (f32, f32) {
        let Some((w, h)) = self.size else { return (self.width, self.height) };
        let room = |r: f32| (r - 2. * PAGE_MARGIN).max(16.);
        let fit = 1f32.min(room(self.width) / w.max(1.)).min(room(self.height) / h.max(1.));
        (w * fit * self.zoom, h * fit * self.zoom)
    }

    /// The area it scrolls over: the image and its margin, or the window.
    fn content(&self) -> (f32, f32) {
        let (w, h) = self.shown();
        ((w + 2. * PAGE_MARGIN).max(self.width), (h + 2. * PAGE_MARGIN).max(self.height))
    }

    fn offset(&self) -> (f32, f32) {
        let o = self.scroll.offset();
        (-f32::from(o.x), -f32::from(o.y))
    }

    fn set_offset(&self, x: f32, y: f32) {
        let (cw, ch) = self.content();
        let x = x.clamp(0., (cw - self.width).max(0.));
        let y = y.clamp(0., (ch - self.height).max(0.));
        self.scroll.set_offset(point(px(-x), px(-y)));
    }

    /// Takes a new zoom or window size, keeping the middle of what's in view.
    pub fn set_size(&mut self, zoom: f32, width: f32, height: f32, cx: &mut Context<Self>) {
        if (zoom, width, height) == (self.zoom, self.width, self.height) {
            return;
        }
        let (x, y) = self.offset();
        let (cw, ch) = self.content();
        let middle = ((x + self.width / 2.) / cw.max(1.), (y + self.height / 2.) / ch.max(1.));
        (self.zoom, self.width, self.height) = (zoom, width, height);
        let (cw, ch) = self.content();
        self.set_offset(middle.0 * cw - width / 2., middle.1 * ch - height / 2.);
        cx.notify();
    }

    /// Does the scrolls asked for since the last one done.
    pub fn scroll(&mut self, asked: &[(u64, MediaScroll)]) {
        let done = self.scrolled;
        for &(n, how) in asked.iter().filter(|(n, _)| *n > done) {
            self.scrolled = n;
            let (x, y) = self.offset();
            match how {
                MediaScroll::Lines(n) => self.set_offset(x, y + n as f32 * LINE_STEP),
                MediaScroll::Screens(n) => self.set_offset(x, y + n * self.height),
                MediaScroll::Pages(n) => self.set_offset(x, y + n as f32 * self.height),
                MediaScroll::Sideways(n) => self.set_offset(x + n as f32 * LINE_STEP, y),
                MediaScroll::Page(0) => self.set_offset(x, 0.),
                MediaScroll::Page(_) | MediaScroll::End => self.set_offset(x, f32::MAX),
                // A spot in a PDF: not for an image.
                MediaScroll::Reveal { .. } => {}
            }
        }
    }
}

impl Render for ImageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let Some(picture) = self.picture.clone() else {
            // Not read yet, or it can't be.
            let why = self.error.clone().unwrap_or_default();
            return div().flex_1().size_full().flex().items_center().justify_center().text_color(muted).child(why);
        };
        let ((w, h), (cw, ch)) = (self.shown(), self.content());
        div()
            .flex_1()
            .size_full()
            .relative()
            .overflow_hidden()
            .child(zoom_catcher(self.window, self.on_zoom.clone(), self.zoom_wheel.clone()))
            .child(
                div().id("image-scroll").size_full().overflow_scroll().track_scroll(&self.scroll).child(
                    div().w(px(cw)).h(px(ch)).flex().items_center().justify_center().child(
                        img(picture).flex_none().w(px(w)).h(px(h)),
                    ),
                ),
            )
    }
}

impl Render for PdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.pixels_per_point = window.scale_factor();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let content = self.content_width();
        div().flex_1().size_full().overflow_hidden().bg(theme.muted).map(|d| match &self.sizes {
            Err(e) => d.flex().items_center().justify_center().text_color(muted).child(e.clone()),
            // Pages scroll up and down in the list, and the list sideways
            // when they're zoomed wider than the window.
            Ok(_) => d.relative().child(self.zoom_catcher()).child(
                div()
                    .id("pdf-sideways")
                    .size_full()
                    .overflow_x_scroll()
                    .track_scroll(&self.sideways)
                    .child(
                        div()
                            .w(px(content))
                            .h_full()
                            .child(list(self.list.clone(), cx.processor(Self::page)).size_full()),
                    ),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_fit_never_grow_and_zoom_from_there() {
        let path = std::env::temp_dir().join(format!("vmac-image-{}.png", std::process::id()));
        image::RgbaImage::new(400, 200).save(&path).unwrap();
        let frames = decode(&path).unwrap();
        assert_eq!(frames[0].buffer().dimensions(), (400, 200), "decoded");
        let mut view = ImageView::new(path.clone(), 0, Rc::new(|_, _, _| {}));
        view.size = Some((400., 200.));
        (view.width, view.height) = (2000., 1000.);
        assert_eq!(view.shown(), (400., 200.), "never larger than it is");
        assert_eq!(view.content(), (2000., 1000.));
        view.width = 232.;
        assert_eq!(view.shown(), (200., 100.), "fitted to a narrow window");
        view.zoom = 3.;
        assert_eq!(view.shown(), (600., 300.));
        assert_eq!(view.content(), (600. + 2. * PAGE_MARGIN, 1000.), "wider than the window: scrolls sideways");
        std::fs::write(&path, b"not a picture").unwrap();
        assert!(decode(&path).is_err());
        std::fs::remove_file(path).ok();
        assert!(decode(Path::new("/nonexistent.png")).is_err());
    }

    #[test]
    fn a_changed_file_is_read_once_it_settles() {
        let t = |s: u64| Some((SystemTime::UNIX_EPOCH + Duration::from_secs(s), s));
        let mut w = Watch { seen: t(1), ..Default::default() };
        w.read(t(1), true);
        assert!(!w.poll(t(1)), "unchanged");
        assert!(!w.poll(t(2)), "changed: wait a look, it may be half written");
        assert!(!w.poll(t(3)), "still changing");
        assert!(w.poll(t(3)), "settled: read it");
        w.read(t(3), false);
        assert!(!w.poll(t(3)), "it didn't read: not again");
        assert!(!w.poll(t(4)));
        assert!(w.poll(t(4)), "the next version is tried");
        w.read(t(4), true);
        assert!(!w.poll(t(4)));
        assert!(!w.poll(None) && !w.poll(None), "gone (being replaced): wait for it");
        assert!(!w.poll(t(5)) && w.poll(t(5)), "back");
    }

    #[test]
    fn pages_fit_the_window_then_grow_with_the_zoom() {
        let mut view = PdfView::new(PathBuf::from("/nonexistent.pdf"), 0, Rc::new(|_, _, _| {}), Rc::new(|_, _, _, _, _| {}));
        view.sizes = Ok(vec![(612., 792.), (1200., 800.)]);
        view.width = 2000.;
        assert_eq!(view.page_width(0), 612., "its own width when there's room");
        assert_eq!(view.page_width(1), PAGE_MAX_WIDTH, "no wider than 900 at zoom 1");
        assert_eq!(view.content_width(), 2000., "narrower pages: the window's width");
        view.width = 400.;
        assert_eq!(view.page_width(0), 400. - 2. * PAGE_MARGIN, "fitted to a narrow window");
        view.zoom = 2.;
        assert_eq!(view.page_width(0), 2. * (400. - 2. * PAGE_MARGIN));
        assert_eq!(view.content_width(), view.page_width(0) + 2. * PAGE_MARGIN, "wider than the window: scrolls sideways");
    }
}
