//! PDF pages as pictures, drawn by macOS's CoreGraphics. Each call opens
//! the file itself, so pages can be drawn on any thread.

use std::path::Path;

/// Each page's size in points: its crop box, turned as the page says.
pub fn page_sizes(path: &Path) -> Result<Vec<(f32, f32)>, String> {
    imp::page_sizes(path)
}

/// Page `page` (from 0) drawn `width` pixels wide on white: its width,
/// height and BGRA pixels, top row first.
pub fn render_page(path: &Path, page: usize, width: u32) -> Result<(u32, u32, Vec<u8>), String> {
    imp::render_page(path, page, width)
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2_core_foundation::{CFRetained, CFURL, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality,
        CGPDFBox, CGPDFDocument, CGPDFPage,
    };
    use std::path::Path;

    fn open(path: &Path) -> Result<CFRetained<CGPDFDocument>, String> {
        let url = CFURL::from_file_path(path).ok_or_else(|| format!("can't name {}", path.display()))?;
        let doc = CGPDFDocument::with_url(Some(&url)).ok_or_else(|| format!("{} isn't a PDF this can read", path.display()))?;
        if CGPDFDocument::is_encrypted(Some(&doc)) && !CGPDFDocument::is_unlocked(Some(&doc)) {
            return Err(format!("{} is locked with a password", path.display()));
        }
        Ok(doc)
    }

    /// A page's crop box, and its size once turned.
    fn page_box(page: &CGPDFPage) -> (CGRect, f64, f64) {
        let rect = CGPDFPage::box_rect(Some(page), CGPDFBox::CropBox);
        let (w, h) = (rect.size.width, rect.size.height);
        if CGPDFPage::rotation_angle(Some(page)).rem_euclid(180) == 90 { (rect, h, w) } else { (rect, w, h) }
    }

    pub fn page_sizes(path: &Path) -> Result<Vec<(f32, f32)>, String> {
        let doc = open(path)?;
        let count = CGPDFDocument::number_of_pages(Some(&doc));
        Ok((1..=count)
            .map(|n| match CGPDFDocument::page(Some(&doc), n) {
                Some(page) => {
                    let (_, w, h) = page_box(&page);
                    (w as f32, h as f32)
                }
                None => (612., 792.),
            })
            .collect())
    }

    pub fn render_page(path: &Path, page: usize, width: u32) -> Result<(u32, u32, Vec<u8>), String> {
        let doc = open(path)?;
        let page = CGPDFDocument::page(Some(&doc), page + 1).ok_or("no such page")?;
        let (_, pw, ph) = page_box(&page);
        if pw <= 0. || ph <= 0. {
            return Err("the page has no size".into());
        }
        let width = width.max(1) as usize;
        let height = ((width as f64) * ph / pw).round().max(1.) as usize;
        let mut pixels = vec![0u8; width * height * 4];
        let space = CGColorSpace::new_device_rgb().ok_or("no RGB color space")?;
        // BGRA in memory, as GPUI draws pictures.
        let info = CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;
        // SAFETY: `pixels` holds width * height * 4 bytes and outlives the context.
        let ctx: CFRetained<CGContext> = unsafe {
            CGBitmapContextCreate(pixels.as_mut_ptr().cast(), width, height, 8, width * 4, Some(&space), info)
        }
        .ok_or("can't make a bitmap to draw in")?;
        let all = CGRect::new(CGPoint::new(0., 0.), CGSize::new(width as f64, height as f64));
        CGContext::set_rgb_fill_color(Some(&ctx), 1., 1., 1., 1.);
        CGContext::fill_rect(Some(&ctx), all);
        CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
        // Scaled up by hand: the drawing transform only ever scales down.
        let scale = width as f64 / pw;
        CGContext::scale_ctm(Some(&ctx), scale, scale);
        let at = CGRect::new(CGPoint::new(0., 0.), CGSize::new(pw, ph));
        let turn = CGPDFPage::drawing_transform(Some(&page), CGPDFBox::CropBox, at, 0, true);
        CGContext::concat_ctm(Some(&ctx), turn);
        CGContext::draw_pdf_page(Some(&ctx), Some(&page));
        drop(ctx);
        Ok((width as u32, height as u32, pixels))
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::path::Path;

    pub fn page_sizes(_: &Path) -> Result<Vec<(f32, f32)>, String> {
        Err("PDFs are shown on macOS only, for now".into())
    }

    pub fn render_page(_: &Path, _: usize, _: u32) -> Result<(u32, u32, Vec<u8>), String> {
        Err("PDFs are shown on macOS only, for now".into())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// A one-page PDF, 200 by 100 points, with a black square in it.
    fn sample() -> std::path::PathBuf {
        let content = "0 0 0 rg 10 10 50 50 re f";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf += &format!("{} 0 obj\n{o}\nendobj\n", i + 1);
        }
        let xref = pdf.len();
        pdf += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
        for o in offsets {
            pdf += &format!("{o:010} 00000 n \n");
        }
        pdf += &format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1);
        let path = std::env::temp_dir().join(format!("vmac-pdf-test-{}.pdf", std::process::id()));
        std::fs::write(&path, pdf).unwrap();
        path
    }

    #[test]
    fn pages_are_measured_and_drawn() {
        let path = sample();
        assert_eq!(page_sizes(&path).unwrap(), vec![(200., 100.)]);
        let (w, h, px) = render_page(&path, 0, 400).unwrap();
        assert_eq!((w, h, px.len()), (400, 200, 400 * 200 * 4));
        let at = |x: usize, y: usize| &px[(y * 400 + x) * 4..][..4];
        assert_eq!(at(10, 10), [255, 255, 255, 255], "top left: the white page");
        assert_eq!(at(60, 160), [0, 0, 0, 255], "bottom left: the square, the right way up");
        assert!(render_page(&path, 1, 400).is_err(), "no second page");
        std::fs::remove_file(path).ok();
        assert!(page_sizes(Path::new("/nonexistent.pdf")).is_err());
    }
}
