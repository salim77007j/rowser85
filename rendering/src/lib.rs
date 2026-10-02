//! Rrowser rendering: display lists, tiny-skia rasterization, glyph atlas
//! and frame composition.
//!
//! Rendering is a pure CPU pipeline in v1: the display list is built from
//! the styled + laid-out DOM, then rasterized by tiny-skia into a
//! premultiplied RGBA [`Frame`]. Glyphs are rasterized through cosmic-text's
//! swash integration with an in-memory cache; the compositor keeps the last
//! frame per tab and only re-rasterizes when the document version changes
//! (idle CPU is zero).
//!
//! A GPU backend (vello/wgpu) can slot in behind the same display list via
//! the documented backend trait — see `docs/ARCHITECTURE.md`.

pub mod display_list;
pub mod painter;

pub use display_list::{build_display_list, DisplayList, DrawCmd};
pub use painter::{Painter, RenderOptions};

use rowser_parsing::cascade::Rgba;

/// A rendered frame: premultiplied RGBA8 pixels.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Frame width in px.
    pub width: u32,
    /// Frame height in px.
    pub height: u32,
    /// Premultiplied RGBA8 pixel data (tiny-skia layout).
    pub pixels: Vec<u8>,
    /// Monotonic frame id for damage tracking.
    pub id: u64,
}

impl Frame {
    /// Returns straight (non-premultiplied) RGBA8 pixels.
    pub fn to_straight_rgba(&self) -> Vec<u8> {
        let mut out = self.pixels.clone();
        for px in out.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a == 0 {
                px[0] = 0;
                px[1] = 0;
                px[2] = 0;
            } else if a < 255 {
                px[0] = ((px[0] as u32 * 255 + a / 2) / a).min(255) as u8;
                px[1] = ((px[1] as u32 * 255 + a / 2) / a).min(255) as u8;
                px[2] = ((px[2] as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
        out
    }

    /// Saves the frame as a PNG file.
    pub fn save_png(&self, path: &str) -> std::io::Result<()> {
        let rgba = self.to_straight_rgba();
        image::save_buffer(
            path,
            &rgba,
            self.width,
            self.height,
            image::ColorType::Rgba8,
        )
        .map_err(std::io::Error::other)
    }

    /// Approximate memory footprint in bytes.
    pub fn memory_usage(&self) -> usize {
        self.pixels.len()
    }
}

/// A decoded image ready for painting.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Straight RGBA8 pixels.
    pub rgba: std::sync::Arc<Vec<u8>>,
}

impl DecodedImage {
    /// Decodes PNG/JPEG/GIF/WebP bytes.
    pub fn decode(bytes: &[u8]) -> Option<DecodedImage> {
        let img = image::load_from_memory(bytes).ok()?;
        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        Some(DecodedImage {
            width,
            height,
            rgba: std::sync::Arc::new(rgba.into_raw()),
        })
    }

    /// Memory footprint in bytes.
    pub fn memory_usage(&self) -> usize {
        self.rgba.len()
    }
}

/// A rectangle in frame coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// Right edge.
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    /// Bottom edge.
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    /// Returns the rect intersected with the viewport (clipped).
    pub fn clipped(&self, viewport: (f32, f32)) -> Rect {
        let (vw, vh) = viewport;
        let x0 = self.x.max(0.0);
        let y0 = self.y.max(0.0);
        let x1 = self.right().min(vw);
        let y1 = self.bottom().min(vh);
        Rect {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0.0),
            h: (y1 - y0).max(0.0),
        }
    }
}

/// Converts a style color to a tiny-skia color.
pub fn to_skia_color(color: Rgba) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(color.r, color.g, color.b, color.a)
}

#[cfg(test)]
mod tests {
    use crate::display_list::build_display_list;
    use crate::painter::{Painter, RenderOptions};
    use rowser_layout::{LayoutEngine, Viewport};
    use rowser_parsing::css::{parse_stylesheet, MediaContext};
    use rowser_parsing::html::parse_html;

    #[test]
    fn renders_html_to_frame() {
        let html = br#"<html><body>
            <h1 style="color: #3366cc">Rrowser Engine</h1>
            <p>Hello <b>bold</b> world - rendering pipeline works.</p>
            <div style="width: 200px; height: 100px; background-color: #ff8800;"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext::default(),
            Viewport {
                width: 800.0,
                height: 600.0,
            },
            &Default::default(),
        );
        let list = build_display_list(
            &doc.dom,
            &styles,
            &layout,
            &Default::default(),
            &Default::default(),
            &Default::default(),
        );
        let mut painter = Painter::new();
        let frame = painter
            .render(&list, RenderOptions::default(), &mut engine.font_system)
            .expect("frame");
        assert_eq!(frame.width, 1280);
        // The colored div must be present.
        let mut found_orange = false;
        for px in frame.pixels.chunks_exact(4) {
            if px[0] == 255 && px[1] == 136 && px[2] == 0 && px[3] == 255 {
                found_orange = true;
                break;
            }
        }
        assert!(found_orange, "orange background missing");
        // Text must produce non-white pixels.
        let mut ink = 0usize;
        for px in frame.pixels.chunks_exact(4) {
            if px[0] < 250 || px[1] < 250 || px[2] < 250 {
                ink += 1;
            }
        }
        assert!(ink > 200, "too little ink: {ink} px");
    }
}
