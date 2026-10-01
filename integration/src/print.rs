//! Print-to-PDF: paginates a tab by re-flowing at A4 width, scrolling the
//! document and capturing engine frames — the same software pipeline the
//! screen uses, at print dimensions. The PDF embeds each page as a
//! Flate-compressed RGB image.

use std::path::Path;

use rowser_api::{BrowserApi, TabId};

/// A4 at 96 CSS dpi.
pub const A4_W: f32 = 794.0;
pub const A4_H: f32 = 1123.0;
/// A4 in PDF points.
const A4_PT_W: f32 = 595.28;
const A4_PT_H: f32 = 841.89;

/// One rendered print page (straight RGB rows, width*A4_W, height*A4_H).
pub struct PrintPage {
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// RGB bytes.
    pub rgb: Vec<u8>,
}

/// Renders the tab into A4 pages. `restore` is the viewport to reinstate
/// afterwards.
pub fn render_pages(
    browser: &BrowserApi,
    tab: TabId,
    restore: (f32, f32),
) -> anyhow::Result<Vec<PrintPage>> {
    // Reflow at print width.
    browser.set_viewport(tab, A4_W, A4_H);
    let base_id = wait_new_frame(browser, tab, None);
    let _ = base_id;

    let content_h = browser.content_size(tab).map(|(_, h)| h).unwrap_or(A4_H);
    let pages = ((content_h / A4_H).ceil() as usize).clamp(1, 50);

    let mut out = Vec::with_capacity(pages);
    for index in 0..pages {
        browser.scroll(tab, index as f32 * A4_H);
        wait_new_frame(browser, tab, None);
        let frame = browser
            .frame(tab)
            .ok_or_else(|| anyhow::anyhow!("no frame for page {}", index + 1))?;
        out.push(PrintPage {
            width: frame.width,
            height: frame.height,
            rgb: rgba_to_rgb(&frame.straight_rgba()),
        });
    }

    // Restore the interactive viewport.
    browser.set_viewport(tab, restore.0, restore.1);
    browser.scroll(tab, 0.0);
    Ok(out)
}

/// Writes the pages as a minimal PDF 1.4 file (image XObjects).
pub fn write_pdf(path: &Path, pages: &[PrintPage]) -> anyhow::Result<()> {
    use std::io::Write;

    let mut objects: Vec<Vec<u8>> = Vec::new();
    // obj 1: catalog, obj 2: page tree, then per page: page, image, content.
    let mut kids = String::new();
    for i in 0..pages.len() {
        let page_num = 3 + i * 3;
        kids.push_str(&format!("{page_num} 0 R "));
    }
    objects.push(format!("<</Type/Catalog/Pages 2 0 R>>").into_bytes());
    objects.push(format!("<</Type/Pages/Kids[{kids}]/Count {}>>", pages.len()).into_bytes());

    for (i, page) in pages.iter().enumerate() {
        let image_num = 3 + i * 3 + 1;
        let content_num = 3 + i * 3 + 2;
        objects.push(format!(
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 {A4_PT_W} {A4_PT_H}]\
             /Resources<</XObject<</Im{i} {image_num} 0 R>>>>/Contents {content_num} 0 R>>"
        )
        .into_bytes());

        let mut zrgb = Vec::with_capacity(page.rgb.len());
        let mut encoder = flate2::write::ZlibEncoder::new(&mut zrgb, flate2::Compression::default());
        encoder.write_all(&page.rgb)?;
        encoder.finish()?;
        let mut stream = format!(
            "<</Type/XObject/Subtype/Image/Width {}/Height {}/ColorSpace/DeviceRGB\
             /BitsPerComponent 8/Filter/FlateDecode/Length {}>>\nstream\n",
            page.width,
            page.height,
            zrgb.len()
        )
        .into_bytes();
        stream.extend_from_slice(&zrgb);
        stream.extend_from_slice(b"\nendstream");
        objects.push(stream);

        objects.push(
            format!("q {A4_PT_W} 0 0 {A4_PT_H} 0 0 cm /Im{i} Do Q").into_bytes()
        );
    }

    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objects.len() + 1);
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len() as u64);
        pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len() as u64;
    pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    std::fs::write(path, pdf)?;
    Ok(())
}

/// Convenience: render + write in one call, returning the page count.
pub fn print_to_pdf(
    browser: &BrowserApi,
    tab: TabId,
    path: &Path,
    restore: (f32, f32),
) -> anyhow::Result<usize> {
    let pages = render_pages(browser, tab, restore)?;
    write_pdf(path, &pages)?;
    Ok(pages.len())
}

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for chunk in rgba.chunks_exact(4) {
        // White page background makes straightening trivial.
        let a = chunk[3] as u16;
        for c in &chunk[..3] {
            rgb.push(((*c as u16 * a + 255 * (255 - a)) / 255) as u8);
        }
    }
    rgb
}

/// Waits (up to 1.5 s) for the tab's frame id to advance past `after`.
fn wait_new_frame(browser: &BrowserApi, tab: TabId, after: Option<u64>) -> Option<u64> {
    let start_id = after.or_else(|| browser.frame(tab).map(|f| f.frame.id));
    for _ in 0..150 {
        if let Some(frame) = browser.frame(tab) {
            match start_id {
                Some(start) if frame.frame.id <= start => {}
                _ => return Some(frame.frame.id),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    browser.frame(tab).map(|f| f.frame.id)
}
