//! Images and PDFs in a window: what `Content::Media` asks the window to draw.
//! An image is fitted to the window; a PDF's pages are stacked to scroll
//! through, each drawn (off the main thread) when it first comes into view.

use gpui_kit::component::ActiveTheme as _;
use crate::frame::MediaScroll;
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ListAlignment, ListOffset, ListState, ObjectFit,
    ParentElement as _, Render, RenderImage, ScrollHandle, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Window, div, img, list, point, prelude::FluentBuilder as _, px,
};
use crate::frame::{ROW_HEIGHT, WindowId};
use gpui_kit::{App, DispatchPhase, ScrollWheelEvent, canvas};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

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

/// An image file, as large as fits (never larger than it is).
pub fn image_element(path: &Path, cx: &gpui_kit::App) -> AnyElement {
    let theme = cx.theme();
    let shown = path.display().to_string();
    let muted = theme.muted_foreground;
    div()
        .flex_1()
        .size_full()
        .overflow_hidden()
        .p(px(16.))
        .flex()
        .items_center()
        .justify_center()
        .child(
            img(path.to_path_buf())
                .size_full()
                .object_fit(ObjectFit::ScaleDown)
                .with_fallback(move || {
                    div().text_color(muted).child(format!("can't show {shown}")).into_any_element()
                }),
        )
        .into_any_element()
}

/// A PDF's pages, drawn as they scroll into view.
pub struct PdfView {
    path: PathBuf,
    /// The file's modification time when it was read: a change reloads it.
    modified: Option<SystemTime>,
    /// Each page's size in points, or why the file can't be shown.
    sizes: Result<Vec<(f32, f32)>, String>,
    /// Drawn pages, and how many pixels wide each was drawn; a page drawn
    /// for another zoom is shown stretched until it's drawn again.
    pages: HashMap<usize, (u32, Arc<RenderImage>)>,
    drawing: HashSet<(usize, u32)>,
    failed: HashMap<usize, String>,
    list: ListState,
    /// The theme's text and background colors (0xRRGGBB) pages are drawn
    /// in, or None for black on white.
    colors: Option<(u32, u32)>,
    /// Bumped whenever drawn pages go stale (the file or the colors
    /// changed), so pages still being drawn from before are thrown away.
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
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl PdfView {
    pub fn new(path: PathBuf, scrolled: u64, on_zoom: OnZoom) -> Self {
        let sizes = crate::pdf::page_sizes(&path);
        let count = sizes.as_ref().map_or(0, Vec::len);
        Self {
            modified: modified(&path),
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
        }
    }

    pub fn set_window(&mut self, window: WindowId) {
        self.window = window;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the file again if it changed since (a PDF rebuilt by LaTeX...),
    /// keeping the place.
    pub fn refresh(&mut self, colors: Option<(u32, u32)>, cx: &mut Context<Self>) {
        if colors != self.colors {
            self.colors = colors;
            self.forget_pages();
            cx.notify();
        }
        let now = modified(&self.path);
        if now == self.modified {
            return;
        }
        let top = self.list.logical_scroll_top();
        self.modified = now;
        self.sizes = crate::pdf::page_sizes(&self.path);
        self.forget_pages();
        self.list.reset(self.sizes.as_ref().map_or(0, Vec::len));
        self.list.scroll_to(top);
        cx.notify();
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

    /// Does the scrolls asked for since the last one done.
    pub fn scroll(&mut self, asked: &[(u64, MediaScroll)]) {
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
            }
        }
    }

    fn forget_pages(&mut self) {
        self.generation += 1;
        self.pages.clear();
        self.failed.clear();
        self.drawing.clear();
    }

    /// Draws page `ix` off the main thread as wide as it's shown, unless it
    /// is drawn that wide or on its way.
    fn draw(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.sizes.is_err() {
            return;
        }
        let pixels = (self.page_width(ix) * self.pixels_per_point).min(MAX_PIXELS);
        let width = ((pixels / PIXEL_STEP).ceil() * PIXEL_STEP) as u32;
        let drawn = self.pages.get(&ix).is_some_and(|(w, _)| *w == width);
        if drawn || self.failed.contains_key(&ix) || !self.drawing.insert((ix, width)) {
            return;
        }
        let path = self.path.clone();
        let (generation, colors) = (self.generation, self.colors);
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
                // The file or the colors changed while it was drawn: it's stale.
                if view.generation != generation {
                    return;
                }
                view.drawing.remove(&(ix, width));
                match drawn.and_then(|(w, h, pixels)| {
                    let buffer = image::RgbaImage::from_raw(w, h, pixels).ok_or("bad bitmap")?;
                    Ok(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
                }) {
                    Ok(picture) => {
                        view.pages.insert(ix, (width, picture));
                    }
                    Err(e) => {
                        view.failed.insert(ix, e);
                    }
                }
                view.forget_far_pages();
                cx.notify();
            });
        })
        .detach();
    }

    /// Drops the pictures of pages far from the ones in view.
    fn forget_far_pages(&mut self) {
        let top = self.list.logical_scroll_top().item_ix;
        let keep = top.saturating_sub(KEEP_AROUND)..=top + 2 * KEEP_AROUND;
        self.pages.retain(|ix, _| keep.contains(ix));
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
            (Some((_, picture)), _) => img(picture.clone()).size_full().object_fit(ObjectFit::Fill).into_any_element(),
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
                    .w(px(shown))
                    .aspect_ratio(w / h.max(1.))
                    .bg(paper)
                    .border_1()
                    .border_color(border)
                    .shadow_sm()
                    .child(body),
            )
            .child(div().pt(px(4.)).text_xs().text_color(muted).child(format!("{}", ix + 1)))
            .into_any_element()
    }
}

impl PdfView {
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
    /// Its size in pixels (drawn a point each at zoom 1), or None when it
    /// can't be read: then it's only fitted, as GPUI manages.
    size: Option<(f32, f32)>,
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
        let size = image::image_dimensions(&path).ok().map(|(w, h)| (w as f32, h as f32));
        Self {
            path,
            size,
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
            }
        }
    }
}

impl Render for ImageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.size.is_none() {
            return image_element(&self.path, cx);
        }
        let ((w, h), (cw, ch)) = (self.shown(), self.content());
        let muted = cx.theme().muted_foreground;
        let shown = self.path.display().to_string();
        div()
            .flex_1()
            .size_full()
            .relative()
            .overflow_hidden()
            .child(zoom_catcher(self.window, self.on_zoom.clone(), self.zoom_wheel.clone()))
            .child(
                div().id("image-scroll").size_full().overflow_scroll().track_scroll(&self.scroll).child(
                    div().w(px(cw)).h(px(ch)).flex().items_center().justify_center().child(
                        img(self.path.clone()).flex_none().w(px(w)).h(px(h)).with_fallback(move || {
                            div().text_color(muted).child(format!("can't show {shown}")).into_any_element()
                        }),
                    ),
                ),
            )
            .into_any_element()
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
        let mut view = ImageView::new(path.clone(), 0, Rc::new(|_, _, _| {}));
        assert_eq!(view.size, Some((400., 200.)), "its size, read from the file");
        (view.width, view.height) = (2000., 1000.);
        assert_eq!(view.shown(), (400., 200.), "never larger than it is");
        assert_eq!(view.content(), (2000., 1000.));
        view.width = 232.;
        assert_eq!(view.shown(), (200., 100.), "fitted to a narrow window");
        view.zoom = 3.;
        assert_eq!(view.shown(), (600., 300.));
        assert_eq!(view.content(), (600. + 2. * PAGE_MARGIN, 1000.), "wider than the window: scrolls sideways");
        std::fs::remove_file(path).ok();
        assert_eq!(ImageView::new(PathBuf::from("/nonexistent.png"), 0, Rc::new(|_, _, _| {})).size, None);
    }

    #[test]
    fn pages_fit_the_window_then_grow_with_the_zoom() {
        let mut view = PdfView::new(PathBuf::from("/nonexistent.pdf"), 0, Rc::new(|_, _, _| {}));
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
