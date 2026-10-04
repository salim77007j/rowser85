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

pub use display_list::{
    build_display_list, BackgroundImageMap, DisplayList, DrawCmd, ElementScrollMap, ImageMap,
    PaintInputs,
};
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
    use crate::display_list::{build_display_list, PaintInputs};
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
            &PaintInputs::default(),
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
    /// overflow: hidden clips overflowing descendants to the padding box.
    #[test]
    fn overflow_hidden_clips_children() {
        // A 200x100 clipped container holding a 300x260 red box: red may
        // only appear within (10,10)-(210,110); blue page background outside.
        let html = br#"<html><body style="margin:0; background-color: #0000ff">
            <div style="width: 200px; height: 100px; overflow: hidden; margin: 10px">
                <div style="width: 300px; height: 260px; background-color: #ff0000"></div>
            </div>
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
            &PaintInputs::default(),
        );
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 400,
            viewport_height: 300,
            ..Default::default()
        };
        let frame = painter
            .render(&list, options, &mut engine.font_system)
            .expect("frame");
        let px = |x: usize, y: usize| {
            let i = (y * frame.width as usize + x) * 4;
            (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2])
        };
        assert_eq!(px(100, 60), (255, 0, 0), "inside clip is red");
        assert_eq!(
            px(100, 115),
            (0, 0, 255),
            "below clip (inside body) is blue, got {:?}",
            px(100, 115)
        );
        assert_eq!(
            px(300, 60),
            (0, 0, 255),
            "right of clip is blue, got {:?}",
            px(300, 60)
        );
    }

    /// opacity: 0 and visibility: hidden hide subtrees entirely.
    #[test]
    fn opacity_zero_and_visibility_hidden_hide() {
        let html = br#"<html><body style="margin:0">
            <div style="width: 100px; height: 50px; background-color: #ff0000; opacity: 0"></div>
            <div style="width: 100px; height: 50px; background-color: #00ff00; visibility: hidden; margin-top: 4px"></div>
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
            &PaintInputs::default(),
        );
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 200,
            viewport_height: 150,
            ..Default::default()
        };
        let frame = painter
            .render(&list, options, &mut engine.font_system)
            .expect("frame");
        for p in frame.pixels.chunks_exact(4) {
            let rgb = (p[0], p[1], p[2]);
            assert!(
                !(rgb == (255, 0, 0) || rgb == (0, 255, 0)),
                "hidden box painted: {rgb:?}"
            );
        }
    }

    /// border-radius rounds the corners of a solid background.
    #[test]
    fn border_radius_rounds_corners() {
        let html = br#"<html><body style="margin:0; background-color: #ffffff">
            <div style="width: 100px; height: 100px; background-color: #ff0000; border-radius: 20px"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext::default(),
            Viewport { width: 200.0, height: 200.0 },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 200,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter.render(&list, options, &mut engine.font_system).expect("frame");
        let px = |x: usize, y: usize| {
            let i = (y * frame.width as usize + x) * 4;
            (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2])
        };
        // Center is red.
        assert_eq!(px(50, 50), (255, 0, 0), "center should be red");
        // The exact corner (0,0) is outside the 20px arc: white.
        assert_eq!(px(2, 2), (255, 255, 255), "corner should be clipped white, got {:?}", px(2, 2));
        // Midpoint on the top edge between the arcs is red.
        assert_eq!(px(50, 2), (255, 0, 0), "top edge midpoint red");
    }

    /// linear-gradient paints a smooth color ramp.
    #[test]
    fn linear_gradient_paints_ramp() {
        let html = br#"<html><body style="margin:0">
            <div style="width: 200px; height: 100px; background: linear-gradient(to right, #000000, #ffffff)"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext::default(),
            Viewport { width: 300.0, height: 200.0 },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 300,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter.render(&list, options, &mut engine.font_system).expect("frame");
        let channel = |x: usize, y: usize| frame.pixels[(y * frame.width as usize + x) * 4];
        let left = channel(5, 50);
        let mid = channel(100, 50);
        let right = channel(195, 50);
        assert!(left < 40, "left should be near black, got {left}");
        assert!(right > 215, "right should be near white, got {right}");
        assert!(mid > 100 && mid < 160, "mid should be mid-gray, got {mid}");
    }

    /// box-shadow paints a blurred halo outside the border box.
    #[test]
    fn box_shadow_paints_halo() {
        let html = br#"<html><body style="margin:20px; background-color: #ffffff">
            <div style="width: 100px; height: 60px; background-color: #0000ff; box-shadow: 0 6px 12px rgba(0,0,0,0.6)"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext::default(),
            Viewport { width: 300.0, height: 300.0 },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 300,
            viewport_height: 300,
            ..Default::default()
        };
        let frame = painter.render(&list, options, &mut engine.font_system).expect("frame");
        // A point 8px below the box bottom (box: y 20..80; shadow at ~88).
        let i = (92usize * 300 + 70) * 4;
        let (r, g, b) = (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]);
        assert!(r < 250 || g < 250, "shadow below box should darken, got ({r},{g},{b})");
    }

    /// transform: translate moves the painted box.
    #[test]
    fn transform_translate_moves_box() {
        let html = br#"<html><body style="margin:0">
            <div style="width: 60px; height: 40px; background-color: #ff0000; transform: translate(40px, 30px)"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext::default(),
            Viewport { width: 200.0, height: 200.0 },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 200,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter.render(&list, options, &mut engine.font_system).expect("frame");
        let px = |x: usize, y: usize| {
            let i = (y * frame.width as usize + x) * 4;
            (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2])
        };
        assert_eq!(px(70, 50), (255, 0, 0), "box center moved to (70,50)");
        assert_eq!(px(10, 10), (255, 255, 255), "original position now white");
    }

    /// opacity: 0.5 blends the box against the background.
    #[test]
    fn opacity_half_blends() {
        let html = br#"<html><body style="margin:0; background-color: #ffffff">
            <div style="width: 100px; height: 100px; background-color: #000000; opacity: 0.5"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext::default(),
            Viewport { width: 200.0, height: 200.0 },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 200,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter.render(&list, options, &mut engine.font_system).expect("frame");
        let i = (50usize * 200 + 50) * 4;
        let v = frame.pixels[i];
        assert!((110.0..145.0).contains(&f32::from(v)), "half-opacity black on white should be ~127, got {v}");
    }

    /// position: fixed stays anchored under page scroll.
    #[test]
    fn fixed_element_ignores_page_scroll() {
        let html = br#"<html><body style="margin:0">
            <div style="height: 2000px"></div>
            <div style="position: fixed; top: 0; left: 0; width: 100px; height: 40px; background-color: #ff0000; z-index: 10"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext::default(),
            Viewport { width: 200.0, height: 400.0 },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        for scroll in [0.0f32, 500.0] {
            let options = RenderOptions {
                viewport_width: 200,
                viewport_height: 400,
                scroll_y: scroll,
                ..Default::default()
            };
            let frame = painter.render(&list, options, &mut engine.font_system).expect("frame");
            let i = (20usize * 200 + 50) * 4;
            let (r, g, b) = (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]);
            assert_eq!((r, g, b), (255, 0, 0), "fixed header visible at scroll {scroll}");
        }
    }

    /// overflow: scroll containers translate content by the element scroll
    /// offset (wired via PaintInputs.element_scroll).
    #[test]
    fn element_scroll_translates_content() {
        let html = br#"<html><body style="margin:0; background-color: #ffffff">
            <div style="width: 100px; height: 60px; overflow: scroll">
                <div style="width: 80px; height: 400px; background-color: #ff0000"></div>
            </div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext::default(),
            Viewport { width: 200.0, height: 200.0 },
            &Default::default(),
        );
        // Find the scrollable container node and scroll it by 100px.
        let mut container = None;
        for (node, style) in &styles.styles {
            if style.overflow_y.scrollable() {
                container = Some(*node);
            }
        }
        let container = container.expect("scrollable container");
        let scroll_map: std::collections::HashMap<_, (f32, f32)> =
            std::collections::HashMap::from([(container, (0.0, 100.0))]);
        let inputs = PaintInputs {
            element_scroll: &scroll_map,
            ..PaintInputs::default()
        };
        let list = build_display_list(&doc.dom, &styles, &layout, &inputs);
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 200,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter.render(&list, options, &mut engine.font_system).expect("frame");
        let px = |x: usize, y: usize| {
            let i = (y * frame.width as usize + x) * 4;
            (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2])
        };
        // Content scrolled by 100: at viewport y=55 the content that was at
        // y=155 now shows — still red (content is 400 tall). But the
        // container clips at y=60.
        assert_eq!(px(40, 30), (255, 0, 0), "inside container red after scroll");
        assert_eq!(px(40, 70), (255, 255, 255), "below container clipped to white");
    }

    /// position: sticky sticks within its containing block while scrolling.
    #[test]
    fn sticky_header_sticks() {
        let html = br#"<html><body style="margin:0">
            <div style="height: 1500px">
                <div style="position: sticky; top: 0; height: 30px; background-color: #00ff00; z-index: 5"></div>
                <div style="height: 1400px; background-color: #dddddd"></div>
            </div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext::default(),
            Viewport { width: 200.0, height: 300.0 },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        // At scroll 0 the header is at y=0 (in flow). At scroll 400 it must
        // remain at viewport top (sticky top: 0).
        for (scroll, expect_top) in [(0.0f32, 0.0f32), (400.0, 0.0)] {
            let options = RenderOptions {
                viewport_width: 200,
                viewport_height: 300,
                scroll_y: scroll,
                ..Default::default()
            };
            let frame = painter.render(&list, options, &mut engine.font_system).expect("frame");
            let y = (expect_top as usize + 15) * 200;
            let i = (y + 100) * 4;
            let (r, g, b) = (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]);
            assert_eq!((r, g, b), (0, 255, 0), "sticky header green at scroll {scroll}, got ({r},{g},{b})");
        }
    }

    /// ::before/::after generated text content renders (styled spans).
    #[test]
    fn pseudo_before_after_content_renders() {
        let html = br#"<html><head><style>
            .item::before { content: "[icon] "; color: #00aa00; display: block; width: 60px; height: 14px; background-color: #00aa00; }
            .item::after { content: " <<"; color: #aa0000; }
        </style></head><body style="margin:0">
            <p class="item">middle</p>
        </body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet(
            &doc.style_blocks().join("\n"),
            &MediaContext::default(),
        );
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 300.0, height: 200.0 },
            &Default::default(),
        );
        // The pseudo styles must exist and generate boxes with text runs.
        let p = doc
            .dom
            .subtree_elements(doc.dom.document())
            .find(|n| {
                doc.dom
                    .element(*n)
                    .map(|e| &*e.name.local == "p")
                    .unwrap_or(false)
            })
            .expect("p node");
        assert!(styles.pseudo_before.contains_key(&p), "::before style missing");
        assert!(styles.pseudo_after.contains_key(&p), "::after style missing");
        // Block-display ::before generates a box + text run.
        let (before_id, after_id) = layout
            .pseudo_ids
            .get(&p)
            .copied()
            .unwrap_or((0, 0));
        assert_ne!(before_id, 0, "::before box not generated");
        assert!(
            layout.text.iter().any(|r| r.node == before_id),
            "::before text run missing"
        );
        // Inline ::after content joins the element's own text (no box).
        assert_eq!(after_id, 0, "inline ::after should not generate a box");
        // The owner's own text run must contain the appended ::after text:
        // shaped glyph count for the p node > "middle" alone.
        let owner_glyphs: usize = layout
            .text
            .iter()
            .filter(|r| r.node == p)
            .map(|r| r.glyphs.len())
            .sum();
        assert!(owner_glyphs > 6, "inline ::after text not appended: {owner_glyphs} glyphs");
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 300,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter.render(&list, options, &mut engine.font_system).expect("frame");
        // Ink must exist (text drawn).
        let mut ink = 0usize;
        for px in frame.pixels.chunks_exact(4) {
            if px[0] < 250 || px[1] < 250 || px[2] < 250 {
                ink += 1;
            }
        }
        assert!(ink > 100, "too little ink: {ink}");
    }

    /// :hover state changes the applied background color.
    #[test]
    fn hover_state_restyles() {
        let html = br#"<html><head><style>
            .btn { background-color: #0000ff; }
            .btn:hover { background-color: #ff0000; }
        </style></head><body style="margin:0">
            <div class="btn" style="width: 80px; height: 40px"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet(
            &doc.style_blocks().join("\n"),
            &MediaContext::default(),
        );
        let mut engine = LayoutEngine::new();
        let (styles, _) = engine.layout_document(
            &doc.dom,
            &[sheet.clone()],
            &MediaContext::default(),
            Viewport { width: 200.0, height: 200.0 },
            &Default::default(),
        );
        // Find the .btn node.
        let btn = doc
            .dom
            .subtree_elements(doc.dom.document())
            .find(|n| {
                doc.dom
                    .element(*n)
                    .map(|e| e.classes.iter().any(|c| c == "btn"))
                    .unwrap_or(false)
            })
            .expect("btn node");
        let color_before = styles.get(btn).unwrap().background_color;
        assert_eq!((color_before.r, color_before.g, color_before.b), (0, 0, 255));
        // Simulate :hover.
        doc.dom
            .interaction_state
            .borrow_mut()
            .hover
            .push(btn);
        let (styles2, _) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 200.0, height: 200.0 },
            &Default::default(),
        );
        let color_after = styles2.get(btn).unwrap().background_color;
        assert_eq!((color_after.r, color_after.g, color_after.b), (255, 0, 0));
    }

    /// calc() percentage + px resolves via the two-pass layout.
    #[test]
    fn calc_percent_px_resolves() {
        let html = br#"<html><body style="margin:0; width: 500px">
            <div style="width: calc(100% - 100px); height: 50px; background-color: #ff0000"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext::default(),
            Viewport { width: 500.0, height: 200.0 },
            &Default::default(),
        );
        let _ = styles;
        // The body is 500 wide; calc(100% - 100px) = 400.
        let div = doc
            .dom
            .subtree_elements(doc.dom.document())
            .find(|n| {
                doc.dom
                    .element(*n)
                    .map(|e| &*e.name.local == "div")
                    .unwrap_or(false)
            })
            .expect("div");
        let rect = layout.rects.get(&div).copied().expect("rect");
        assert!(
            (rect.w - 400.0).abs() < 2.0,
            "calc width should be ~400, got {}",
            rect.w
        );
    }

    /// vw viewport units resolve at compute time.
    #[test]
    fn viewport_units_resolve() {
        let html = br#"<html><body style="margin:0">
            <div style="width: 50vw; height: 10px; background-color: #ff0000"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("", &MediaContext { width: 800.0, height: 600.0, dark_mode: false });
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext { width: 800.0, height: 600.0, dark_mode: false },
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let div = doc
            .dom
            .subtree_elements(doc.dom.document())
            .find(|n| {
                doc.dom
                    .element(*n)
                    .map(|e| &*e.name.local == "div")
                    .unwrap_or(false)
            })
            .expect("div");
        let rect = layout.rects.get(&div).copied().expect("rect");
        assert!((rect.w - 400.0).abs() < 1.0, "50vw should be 400, got {}", rect.w);
    }
}
