//! Images and PDFs in a window: what `Content::Media` asks the window to draw.
//! An image is fitted to the window; a PDF's pages are stacked to scroll
//! through, each drawn (off the main thread) when it first comes into view.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    AnyElement, Context, IntoElement, ListAlignment, ListState, ObjectFit, ParentElement as _, Render, RenderImage,
    Styled as _, StyledImage as _, Window, div, img, list, prelude::FluentBuilder as _, px,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// The widest a page is shown, in points on screen, and how many pixels
/// each of those gets (sharp on a Retina display).
const PAGE_MAX_WIDTH: f32 = 900.;
const PAGE_PIXELS_PER_POINT: f32 = 2.;
/// Pages kept drawn either side of the first one in view; further ones are
/// dropped and drawn again if you come back.
const KEEP_AROUND: usize = 4;

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
    pages: HashMap<usize, Arc<RenderImage>>,
    drawing: HashSet<usize>,
    failed: HashMap<usize, String>,
    list: ListState,
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl PdfView {
    pub fn new(path: PathBuf) -> Self {
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
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the file again if it changed since (a PDF rebuilt by LaTeX...),
    /// keeping the place.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let now = modified(&self.path);
        if now == self.modified {
            return;
        }
        let top = self.list.logical_scroll_top();
        self.modified = now;
        self.sizes = crate::pdf::page_sizes(&self.path);
        self.pages.clear();
        self.failed.clear();
        self.list.reset(self.sizes.as_ref().map_or(0, Vec::len));
        self.list.scroll_to(top);
        cx.notify();
    }

    /// Draws page `ix` off the main thread, unless it is drawn or on its way.
    fn draw(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.pages.contains_key(&ix) || self.failed.contains_key(&ix) || !self.drawing.insert(ix) {
            return;
        }
        let Ok(sizes) = &self.sizes else { return };
        let points = sizes.get(ix).map_or(612., |s| s.0).min(PAGE_MAX_WIDTH);
        let width = (points * PAGE_PIXELS_PER_POINT).round() as u32;
        let path = self.path.clone();
        let modified = self.modified;
        cx.spawn(async move |this, cx| {
            let drawn = cx.background_executor().spawn(async move { crate::pdf::render_page(&path, ix, width) }).await;
            let _ = this.update(cx, |view, cx| {
                view.drawing.remove(&ix);
                // The file changed while it was drawn: this page is stale.
                if view.modified != modified {
                    return;
                }
                match drawn.and_then(|(w, h, pixels)| {
                    let buffer = image::RgbaImage::from_raw(w, h, pixels).ok_or("bad bitmap")?;
                    Ok(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
                }) {
                    Ok(picture) => {
                        view.pages.insert(ix, picture);
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
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let body = match (self.pages.get(&ix), self.failed.get(&ix)) {
            (Some(picture), _) => img(picture.clone()).size_full().object_fit(ObjectFit::Fill).into_any_element(),
            (None, Some(e)) => div().p(px(12.)).text_color(gpui_kit::black()).child(e.clone()).into_any_element(),
            (None, None) => div().into_any_element(),
        };
        div()
            .w_full()
            .px(px(16.))
            .pt(px(if ix == 0 { 16. } else { 0. }))
            .pb(px(16.))
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .w_full()
                    .max_w(px(w.min(PAGE_MAX_WIDTH)))
                    .aspect_ratio(w / h.max(1.))
                    .bg(gpui_kit::white())
                    .border_1()
                    .border_color(border)
                    .shadow_sm()
                    .child(body),
            )
            .child(div().pt(px(4.)).text_xs().text_color(muted).child(format!("{}", ix + 1)))
            .into_any_element()
    }
}

impl Render for PdfView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        div().flex_1().size_full().overflow_hidden().bg(theme.muted).map(|d| match &self.sizes {
            Err(e) => d.flex().items_center().justify_center().text_color(muted).child(e.clone()),
            Ok(_) => d.child(list(self.list.clone(), cx.processor(Self::page)).size_full()),
        })
    }
}
