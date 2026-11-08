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
    /// The theme's text and background colors (0xRRGGBB) pages are drawn
    /// in, or None for black on white.
    colors: Option<(u32, u32)>,
    /// Bumped whenever drawn pages go stale (the file or the colors
    /// changed), so pages still being drawn from before are thrown away.
    generation: u64,
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
            colors: None,
            generation: 0,
        }
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

    fn forget_pages(&mut self) {
        self.generation += 1;
        self.pages.clear();
        self.failed.clear();
        self.drawing.clear();
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
                view.drawing.remove(&ix);
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
        let (paper, ink) = match self.colors {
            Some(_) => (theme.background, theme.foreground),
            None => (gpui_kit::white(), gpui_kit::black()),
        };
        let body = match (self.pages.get(&ix), self.failed.get(&ix)) {
            (Some(picture), _) => img(picture.clone()).size_full().object_fit(ObjectFit::Fill).into_any_element(),
            (None, Some(e)) => div().p(px(12.)).text_color(ink).child(e.clone()).into_any_element(),
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
