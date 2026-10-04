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

pub mod canvas2d;
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

// ---------------------------------------------------------------------------
// SVG rasterization (Group C — rendering quality).
//
// resvg 0.44 shares the tiny-skia backend family with the painter (its own
// 0.11.4 instance, re-exported as `resvg::tiny_skia`). The engine rasterizes
// every inline `<svg>` element at its *layout rect* in device pixels (cache
// keyed by node + size + subtree signature), so logos are pixel-exact rather
// than upscaled rasters. `<img src="*.svg">` and CSS `url()` backgrounds use
// the natural-size path at decode time.
// ---------------------------------------------------------------------------

// usvg through resvg's re-export — guarantees the exact tree types that
// `resvg::render` consumes (same crate instance, no version drift).
use resvg::usvg;

/// System font database for SVG `<text>` elements (lazy, shared).
static SVG_FONTS: std::sync::LazyLock<std::sync::Arc<usvg::fontdb::Database>> =
    std::sync::LazyLock::new(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        std::sync::Arc::new(db)
    });

/// Parses SVG markup into a usvg tree (system fonts available for `<text>`).
pub fn parse_svg_tree(markup: &str) -> Option<usvg::Tree> {
    if !markup.trim().starts_with('<') {
        return None;
    }
    let opts = usvg::Options {
        fontdb: std::sync::Arc::clone(&SVG_FONTS),
        ..Default::default()
    };
    match usvg::Tree::from_str(markup, &opts) {
        Ok(tree) => Some(tree),
        Err(err) => {
            log::debug!("svg parse failed: {err}");
            None
        }
    }
}

/// Renders a parsed usvg tree (whose canvas is expected to be exactly
/// `w` x `h` — see [`rasterize_svg`], which injects the root size) into a
/// `DecodedImage`. Identity transform; `preserveAspectRatio` was already
/// applied by usvg when mapping the viewBox into the root size.
pub fn render_svg_tree(tree: &usvg::Tree, w: u32, h: u32) -> Option<DecodedImage> {
    if w == 0 || h == 0 {
        return None;
    }
    let mut pm = resvg::tiny_skia::Pixmap::new(w, h)?;
    resvg::render(
        tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pm.as_mut(),
    );
    // tiny-skia pixels are premultiplied; DecodedImage is straight RGBA.
    let mut rgba = pm.take();
    for px in rgba.chunks_exact_mut(4) {
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
    Some(DecodedImage {
        width: w,
        height: h,
        rgba: std::sync::Arc::new(rgba),
    })
}

/// One-shot helper: parse markup and rasterize at exactly `w` x `h` device
/// pixels. The root element's `width`/`height` attributes are REPLACED with
/// the target size, so usvg maps the viewBox into that box honoring
/// `preserveAspectRatio` (meet/slice/none + alignment, full spec) — the
/// Chrome-equivalent layout-box mapping. Inline `<svg>` elements call this
/// with their layout rect; the result is pixel-exact, never upscaled.
pub fn rasterize_svg(markup: &str, w: u32, h: u32) -> Option<DecodedImage> {
    if w == 0 || h == 0 {
        return None;
    }
    let sized = inject_root_size(markup, w as f32, h as f32);
    let tree = parse_svg_tree(&sized)?;
    render_svg_tree(&tree, w, h)
}

/// Natural (intrinsic) size of an SVG document in CSS pixels — Chrome
/// semantics: width/height attrs when present; a missing dimension
/// resolves through the viewBox ratio; fully-unspecified falls back to the
/// 300x150 replaced-element default.
pub fn svg_natural_size(markup: &str) -> Option<(f32, f32)> {
    // Cheap pre-parse of the root tag: width/height/viewBox attributes.
    let root_end = markup.find('>').unwrap_or(markup.len());
    let root = &markup[..root_end];
    let attr = |name: &str| -> Option<&str> {
        let pat = format!("{name}=\"");
        let i = root.find(&pat)?;
        let rest = &root[i + pat.len()..];
        let j = rest.find('"')?;
        Some(&rest[..j])
    };
    let num = |v: &str| -> Option<f32> {
        v.trim()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
            .collect::<String>()
            .parse()
            .ok()
    };
    let w = attr("width").and_then(num);
    let h = attr("height").and_then(num);
    // viewBox="minx miny w h" → (w, h).
    let viewbox = attr("viewBox").and_then(|v| {
        let mut it = v.split_whitespace().filter_map(|t| t.parse::<f32>().ok());
        let _x = it.next()?;
        let _y = it.next()?;
        let vw = it.next()?;
        let vh = it.next()?;
        (vw > 0.0 && vh > 0.0).then_some((vw, vh))
    });
    match (w, h, viewbox) {
        (Some(w), Some(h), _) => Some((w, h)),
        (Some(w), None, Some((vw, vh))) => Some((w, w * vh / vw)),
        (Some(w), None, None) => Some((w, 150.0)),
        (None, Some(h), Some((vw, vh))) => Some((h * vw / vh, h)),
        (None, Some(h), None) => Some((300.0, h)),
        (None, None, Some((vw, vh))) => Some((vw, vh)),
        (None, None, None) => Some((300.0, 150.0)),
    }
}

/// Rasterizes SVG bytes (an `<img src>` or CSS background response) at their
/// natural size — the entry point used by the engine's image decode path.
/// Chrome semantics: one missing dimension resolves through the viewBox
/// ratio (usvg alone would keep the raw viewBox extent instead).
pub fn decode_svg_bytes(bytes: &[u8]) -> Option<DecodedImage> {
    let markup = std::str::from_utf8(bytes).ok()?;
    let (nw, nh) = svg_natural_size(markup)?;
    let w = (nw.ceil() as u32).max(1);
    let h = (nh.ceil() as u32).max(1);
    let sized = inject_root_size(markup, nw, nh);
    let tree = parse_svg_tree(&sized)?;
    render_svg_tree(&tree, w, h)
}

/// Rewrites the root `<svg>` tag of `markup` so its `width`/`height`
/// attributes equal exactly `(w, h)` (replacing whatever was there). XML
/// prologs (`<?xml ...?>`, DOCTYPE, comments) are preserved. usvg then
/// resolves the tree canvas to that size — and applies
/// `preserveAspectRatio` when a viewBox exists.
fn inject_root_size(markup: &str, w: f32, h: f32) -> String {
    let s = markup.trim_start();
    // Skip XML prolog nodes to reach the root element tag.
    let mut idx = 0usize;
    loop {
        let rest = &s[idx..];
        if rest.starts_with("<?") {
            match rest.find("?>") {
                Some(end) => idx += end + 2,
                None => break,
            }
        } else if rest.starts_with("<!--") {
            match rest.find("-->") {
                Some(end) => idx += end + 3,
                None => break,
            }
        } else if rest.starts_with("<!") {
            match rest.find('>') {
                Some(end) => idx += end + 1,
                None => break,
            }
        } else {
            break;
        }
        // Skip inter-prolog whitespace.
        let rest = &s[idx..];
        let ws = rest.len() - rest.trim_start().len();
        idx += ws;
    }
    let root_start = idx + (s[idx..].len() - s[idx..].trim_start().len());
    let Some(open_rel) = s[root_start..].find('>') else {
        // Malformed root: append a sized root tag around the content.
        return format!("<svg width=\"{w}\" height=\"{h}\">{s}</svg>");
    };
    let open_end = root_start + open_rel + 1;
    let head = &s[root_start..open_end - 1]; // "<svg ... (maybe '/')" without '>'
    let tail = &s[open_end..];
    let self_closing = head.trim_end().ends_with('/');
    let head_core = head.trim_end().trim_end_matches('/');
    let rebuilt = strip_size_attrs(head_core);
    let mut out = String::with_capacity(s.len() + 64);
    out.push_str(&s[..root_start]);
    out.push_str(&rebuilt);
    // Inline SVG in HTML carries no xmlns (html5ever assigns the SVG
    // namespace implicitly); usvg REQUIRES it on the root element.
    if !rebuilt.contains("xmlns=") {
        out.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
    }
    out.push_str(&format!(" width=\"{w}\" height=\"{h}\""));
    if self_closing {
        out.push('/');
    }
    out.push('>');
    if !self_closing {
        out.push_str(tail);
    }
    out
}

/// Removes `width`/`height` attributes from a root tag string of the form
/// `<svg attr=... attr=...`, re-emitting the remaining attributes verbatim
/// (unquoted values become quoted). Preserves quoted whitespace runs.
fn strip_size_attrs(root_tag: &str) -> String {
    let b = root_tag.as_bytes();
    let mut out = String::with_capacity(root_tag.len() + 16);
    out.push_str("<svg");
    let mut i = 4usize;
    while i < b.len() {
        // Skip whitespace between attributes.
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        // Attribute name (up to '=' or whitespace).
        let name_start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' {
            i += 1;
        }
        let name = &root_tag[name_start..i];
        // Optional '=' value.
        let mut value: Option<(u8, usize, usize)> = None;
        let mut j = i;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                let q = b[j];
                let vs = j + 1;
                j += 1;
                while j < b.len() && b[j] != q {
                    j += 1;
                }
                value = Some((q, vs, j.min(b.len())));
                j += 1;
            } else {
                let vs = j;
                while j < b.len() && !b[j].is_ascii_whitespace() {
                    j += 1;
                }
                value = Some((0, vs, j));
            }
            i = j;
        }
        let is_size = name.eq_ignore_ascii_case("width") || name.eq_ignore_ascii_case("height");
        if !is_size {
            out.push(' ');
            out.push_str(name);
            if let Some((q, vs, ve)) = value {
                out.push('=');
                if q == 0 {
                    out.push('"');
                    out.push_str(&root_tag[vs..ve]);
                    out.push('"');
                } else {
                    out.push(q as char);
                    out.push_str(&root_tag[vs..ve]);
                    out.push(q as char);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::display_list::{build_display_list, PaintInputs};
    use crate::painter::{Painter, RenderOptions};
    use rowser_dom::ns;
    use rowser_layout::{LayoutEngine, Viewport};
    use rowser_parsing::css::{parse_stylesheet, MediaContext};
    use rowser_parsing::html::parse_html;

    #[test]
    fn svg_raster_maps_viewbox_left_half() {
        // viewBox 2x1, left unit square black → at 200x100 the LEFT half
        // of the raster must be opaque (root size injected → usvg maps the
        // viewBox with uniform scale, default xMidYMid meet).
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 2 1">
            <rect x="0" y="0" width="1" height="1" fill="black"/>
        </svg>"#;
        let img = crate::rasterize_svg(svg, 200, 100).expect("raster");
        assert_eq!((img.width, img.height), (200, 100));
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 200 + x) * 4;
            [
                img.rgba[i],
                img.rgba[i + 1],
                img.rgba[i + 2],
                img.rgba[i + 3],
            ]
        };
        assert_eq!(px(50, 50)[3], 255, "left half must be opaque black");
        assert_eq!(px(150, 50)[3], 0, "right half must be transparent");
        assert_eq!(px(150, 50)[0], 0);
    }

    #[test]
    fn svg_raster_meet_centers_wide_content() {
        // Wide viewBox (2x1) into a square 100x100 canvas with meet: the
        // content strip must be vertically centered (y 25..75).
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 2 1">
            <rect x="0" y="0" width="2" height="1" fill="black"/>
        </svg>"#;
        let img = crate::rasterize_svg(svg, 100, 100).expect("raster");
        let alpha = |x: usize, y: usize| img.rgba[(y * 100 + x) * 4 + 3];
        assert_eq!(alpha(50, 50), 255, "center row painted");
        assert_eq!(alpha(50, 10), 0, "top margin transparent");
        assert_eq!(alpha(50, 90), 0, "bottom margin transparent");
    }

    #[test]
    fn svg_raster_stretch_fills() {
        // preserveAspectRatio="none": non-uniform scale fills the canvas.
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 2 1" preserveAspectRatio="none">
            <rect x="0" y="0" width="2" height="1" fill="black"/>
        </svg>"#;
        let img = crate::rasterize_svg(svg, 100, 100).expect("raster");
        for y in [0, 50, 99] {
            assert_eq!(
                img.rgba[(y * 100 + 50) * 4 + 3],
                255,
                "stretch fills all rows"
            );
        }
    }

    #[test]
    fn svg_raster_replaces_existing_size_attrs() {
        // Conflicting attrs on the root must be REPLACED, not doubled.
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="5" height="5" viewBox="0 0 2 1">
            <rect x="0" y="0" width="2" height="1" fill="black"/>
        </svg>"#;
        let img = crate::rasterize_svg(svg, 100, 50).expect("raster");
        assert_eq!((img.width, img.height), (100, 50));
        // Full-bleed rect: every center pixel opaque.
        for y in [0, 25, 49] {
            assert_eq!(img.rgba[(y * 100 + 50) * 4 + 3], 255);
        }
    }

    #[test]
    fn svg_natural_size_prefers_attrs_then_viewbox() {
        let both = crate::svg_natural_size(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32" viewBox="0 0 2 1"/>"#,
        );
        assert_eq!(both, Some((64.0, 32.0)));
        let vb_only = crate::svg_natural_size(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 144 72"/>"#,
        );
        assert_eq!(vb_only, Some((144.0, 72.0)));
        let w_only = crate::svg_natural_size(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" viewBox="0 0 2 1"/>"#,
        );
        assert_eq!(w_only, Some((100.0, 50.0)));
    }

    #[test]
    fn svg_decode_bytes_roundtrip() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
            <circle cx="5" cy="5" r="5" fill="#ff0000"/>
        </svg>"##;
        let img = crate::decode_svg_bytes(svg).expect("decode");
        assert!((img.width as i32 - 10).abs() <= 1 && (img.height as i32 - 10).abs() <= 1);
        // Center of the circle: red, opaque.
        let i = ((img.height as usize / 2) * img.width as usize + img.width as usize / 2) * 4;
        assert!(img.rgba[i] > 200, "red channel at center");
        assert_eq!(img.rgba[i + 3], 255, "opaque");
    }

    #[test]
    fn svg_layout_default_replaced_size() {
        // Chrome: inline svg without dimensions → 300x150 replaced default.
        let html = br#"<html><body><div><svg viewBox="0 0 16 16"><path d="M0 0h16v16H0z"/></svg></div></body></html>"#;
        let doc = parse_html(html);
        let mut engine = LayoutEngine::new();
        let (_styles, layout) = engine.layout_document(
            &doc.dom,
            &[],
            &MediaContext::default(),
            Viewport {
                width: 800.0,
                height: 600.0,
            },
            &Default::default(),
        );
        let svg_rect = layout
            .rects
            .iter()
            .find(|(id, _)| {
                doc.dom
                    .element(**id)
                    .is_some_and(|e| &*e.name.local == "svg")
            })
            .map(|(_, r)| *r)
            .expect("svg rect");
        assert!((svg_rect.w - 300.0).abs() < 0.5, "w = {}", svg_rect.w);
        assert!((svg_rect.h - 150.0).abs() < 0.5, "h = {}", svg_rect.h);
    }

    #[test]
    fn svg_layout_attr_and_ratio_sizing() {
        // width attr + viewBox ratio → height from ratio (Chrome).
        let html = br#"<html><body><div><svg width="100" viewBox="0 0 2 1"><rect width="2" height="1"/></svg></div></body></html>"#;
        let doc = parse_html(html);
        let mut engine = LayoutEngine::new();
        let (_styles, layout) = engine.layout_document(
            &doc.dom,
            &[],
            &MediaContext::default(),
            Viewport {
                width: 800.0,
                height: 600.0,
            },
            &Default::default(),
        );
        let svg_rect = layout
            .rects
            .iter()
            .find(|(id, _)| {
                doc.dom
                    .element(**id)
                    .is_some_and(|e| &*e.name.local == "svg")
            })
            .map(|(_, r)| *r)
            .expect("svg rect");
        assert!((svg_rect.w - 100.0).abs() < 0.5, "w = {}", svg_rect.w);
        assert!((svg_rect.h - 50.0).abs() < 1.0, "h = {}", svg_rect.h);
    }

    #[test]
    fn svg_layout_subtree_excluded_from_boxes() {
        // Vector children must not create layout boxes (no double paint).
        let html = br#"<html><body><div><svg width="40" height="40" viewBox="0 0 4 4"><g><path d="M0 0h4v4H0z"/><text>label</text></g></svg></div></body></html>"#;
        let doc = parse_html(html);
        let mut engine = LayoutEngine::new();
        let (_styles, layout) = engine.layout_document(
            &doc.dom,
            &[],
            &MediaContext::default(),
            Viewport {
                width: 800.0,
                height: 600.0,
            },
            &Default::default(),
        );
        assert!(
            layout.rects.keys().all(|id| doc
                .dom
                .element(*id)
                .is_some_and(|e| { e.name.ns != ns!(svg) || &*e.name.local == "svg" })),
            "svg descendants must not have boxes"
        );
        // And no text runs originate under the svg.
        assert!(layout.text.iter().all(|run| doc
            .dom
            .element(run.node)
            .is_some_and(|e| e.name.ns != ns!(svg))));
    }

    #[test]
    fn inline_svg_paints_through_image_path() {
        // Full pipeline: styled+ laid-out doc, svg raster merged into the
        // image map, painted via DrawCmd::Image. The engine does the merge;
        // this test reproduces it: rasterize the svg element at its layout
        // rect and paint.
        let html = br##"<html><body><div style="width: 90px; height: 90px;">
            <svg width="90" height="90" viewBox="0 0 90 90" style="display: block;">
                <circle cx="45" cy="45" r="40" fill="#cc0000"/>
            </svg>
        </div></body></html>"##;
        let doc = parse_html(html);
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[],
            &MediaContext::default(),
            Viewport {
                width: 400.0,
                height: 300.0,
            },
            &Default::default(),
        );
        // Find the svg node, serialize + rasterize at its layout rect (the
        // engine's rasterize_inline_svgs does exactly this).
        let svg_node = layout
            .rects
            .keys()
            .copied()
            .find(|id| {
                doc.dom
                    .element(*id)
                    .is_some_and(|e| &*e.name.local == "svg")
            })
            .expect("svg node");
        let rect = layout.rects[&svg_node];
        let markup = doc.dom.serialize_subtree(svg_node);
        let image = crate::rasterize_svg(&markup, rect.w.round() as u32, rect.h.round() as u32)
            .expect("raster");
        let mut images = crate::ImageMap::new();
        images.insert(svg_node, std::sync::Arc::new(image));
        let inputs = PaintInputs {
            images: &images,
            ..PaintInputs::default()
        };
        let list = build_display_list(&doc.dom, &styles, &layout, &inputs);
        let mut painter = Painter::new();
        let frame = painter
            .render(&list, RenderOptions::default(), &mut engine.font_system)
            .expect("frame");
        // Center of the circle (layout coords = frame coords, scroll 0).
        let cx = (rect.x + rect.w / 2.0).round() as usize;
        let cy = (rect.y + rect.h / 2.0).round() as usize;
        let i = (cy * frame.width as usize + cx) * 4;
        let px = &frame.pixels[i..i + 4];
        assert!(
            px[0] > 150 && px[1] < 90 && px[2] < 90 && px[3] == 255,
            "circle center should be red, got {:?}",
            px
        );
    }

    #[test]
    fn hidpi_render_scales_geometry_and_text() {
        // A CSS-px rect at (10,10,50,50) must cover device (20,20)-(120,120)
        // at DPR 2, and text must rasterize without panic (glyph keys are
        // re-binned at 2x font size).
        let html = br#"<html><body>
            <div style="position: absolute; left: 10px; top: 10px; width: 50px; height: 50px; background-color: #008000;"></div>
            <p>HiDPI text at two times density</p>
        </body></html>"#;
        let doc = parse_html(html);
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[],
            &MediaContext::default(),
            Viewport {
                width: 400.0,
                height: 300.0,
            },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let frame = painter
            .render(
                &list,
                RenderOptions {
                    viewport_width: 400,
                    viewport_height: 300,
                    scale: 2.0,
                    ..RenderOptions::default()
                },
                &mut engine.font_system,
            )
            .expect("frame");
        assert_eq!((frame.width, frame.height), (800, 600), "device px dims");
        // Deep inside the scaled rect (device 40..100 x 40..100): green.
        let i = (60 * frame.width as usize + 60) * 4;
        let px = &frame.pixels[i..i + 4];
        assert!(
            px[1] > 120 && px[0] < 60 && px[3] == 255,
            "scaled rect green, got {px:?}"
        );
        // Outside the rect (device 140, 60): not green.
        let j = (60 * frame.width as usize + 140) * 4;
        let q = &frame.pixels[j..j + 4];
        assert!(
            !(q[1] > 120 && q[0] < 60 && q[3] == 255),
            "outside rect must not be green, got {q:?}"
        );
        // Text painted (any non-white ink outside the green rect; the <p>
        // sits at the top of the body, CSS y ~ 8..40 → device 16..80).
        let mut text_ink = false;
        'scan: for y in 0..frame.height as usize {
            for x in 0..frame.width as usize {
                if (40..100).contains(&y) && (40..100).contains(&x) {
                    continue; // the green rect
                }
                let k = (y * frame.width as usize + x) * 4;
                let p = &frame.pixels[k..k + 4];
                if p[3] == 255 && (p[0] < 240 || p[1] < 240 || p[2] < 240) {
                    text_ink = true;
                    break 'scan;
                }
            }
        }
        assert!(text_ink, "text ink visible at DPR 2");
    }

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
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
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
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
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
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
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
            Viewport {
                width: 200.0,
                height: 200.0,
            },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 200,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter
            .render(&list, options, &mut engine.font_system)
            .expect("frame");
        let px = |x: usize, y: usize| {
            let i = (y * frame.width as usize + x) * 4;
            (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2])
        };
        // Center is red.
        assert_eq!(px(50, 50), (255, 0, 0), "center should be red");
        // The exact corner (0,0) is outside the 20px arc: white.
        assert_eq!(
            px(2, 2),
            (255, 255, 255),
            "corner should be clipped white, got {:?}",
            px(2, 2)
        );
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
            Viewport {
                width: 300.0,
                height: 200.0,
            },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 300,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter
            .render(&list, options, &mut engine.font_system)
            .expect("frame");
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
            Viewport {
                width: 300.0,
                height: 300.0,
            },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 300,
            viewport_height: 300,
            ..Default::default()
        };
        let frame = painter
            .render(&list, options, &mut engine.font_system)
            .expect("frame");
        // A point 8px below the box bottom (box: y 20..80; shadow at ~88).
        let i = (92usize * 300 + 70) * 4;
        let (r, g, b) = (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]);
        assert!(
            r < 250 || g < 250,
            "shadow below box should darken, got ({r},{g},{b})"
        );
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
            Viewport {
                width: 200.0,
                height: 200.0,
            },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 200,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter
            .render(&list, options, &mut engine.font_system)
            .expect("frame");
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
            Viewport {
                width: 200.0,
                height: 200.0,
            },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 200,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter
            .render(&list, options, &mut engine.font_system)
            .expect("frame");
        let i = (50usize * 200 + 50) * 4;
        let v = frame.pixels[i];
        assert!(
            (110.0..145.0).contains(&f32::from(v)),
            "half-opacity black on white should be ~127, got {v}"
        );
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
            Viewport {
                width: 200.0,
                height: 400.0,
            },
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
            let frame = painter
                .render(&list, options, &mut engine.font_system)
                .expect("frame");
            let i = (20usize * 200 + 50) * 4;
            let (r, g, b) = (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]);
            assert_eq!(
                (r, g, b),
                (255, 0, 0),
                "fixed header visible at scroll {scroll}"
            );
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
            Viewport {
                width: 200.0,
                height: 200.0,
            },
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
        let frame = painter
            .render(&list, options, &mut engine.font_system)
            .expect("frame");
        let px = |x: usize, y: usize| {
            let i = (y * frame.width as usize + x) * 4;
            (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2])
        };
        // Content scrolled by 100: at viewport y=55 the content that was at
        // y=155 now shows — still red (content is 400 tall). But the
        // container clips at y=60.
        assert_eq!(px(40, 30), (255, 0, 0), "inside container red after scroll");
        assert_eq!(
            px(40, 70),
            (255, 255, 255),
            "below container clipped to white"
        );
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
            Viewport {
                width: 200.0,
                height: 300.0,
            },
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
            let frame = painter
                .render(&list, options, &mut engine.font_system)
                .expect("frame");
            let y = (expect_top as usize + 15) * 200;
            let i = (y + 100) * 4;
            let (r, g, b) = (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]);
            assert_eq!(
                (r, g, b),
                (0, 255, 0),
                "sticky header green at scroll {scroll}, got ({r},{g},{b})"
            );
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
        let sheet = parse_stylesheet(&doc.style_blocks().join("\n"), &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport {
                width: 300.0,
                height: 200.0,
            },
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
        assert!(
            styles.pseudo_before.contains_key(&p),
            "::before style missing"
        );
        assert!(
            styles.pseudo_after.contains_key(&p),
            "::after style missing"
        );
        // Block-display ::before generates a box + text run.
        let (before_id, after_id) = layout.pseudo_ids.get(&p).copied().unwrap_or((0, 0));
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
        assert!(
            owner_glyphs > 6,
            "inline ::after text not appended: {owner_glyphs} glyphs"
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        let mut painter = Painter::new();
        let options = RenderOptions {
            viewport_width: 300,
            viewport_height: 200,
            ..Default::default()
        };
        let frame = painter
            .render(&list, options, &mut engine.font_system)
            .expect("frame");
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
        let sheet = parse_stylesheet(&doc.style_blocks().join("\n"), &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (styles, _) = engine.layout_document(
            &doc.dom,
            std::slice::from_ref(&sheet),
            &MediaContext::default(),
            Viewport {
                width: 200.0,
                height: 200.0,
            },
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
        assert_eq!(
            (color_before.r, color_before.g, color_before.b),
            (0, 0, 255)
        );
        // Simulate :hover.
        doc.dom.interaction_state.borrow_mut().hover.push(btn);
        let (styles2, _) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport {
                width: 200.0,
                height: 200.0,
            },
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
            Viewport {
                width: 500.0,
                height: 200.0,
            },
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
        let author = parse_stylesheet(
            "",
            &MediaContext {
                width: 800.0,
                height: 600.0,
                dark_mode: false,
            },
        );
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[author],
            &MediaContext {
                width: 800.0,
                height: 600.0,
                dark_mode: false,
            },
            Viewport {
                width: 800.0,
                height: 600.0,
            },
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
        assert!(
            (rect.w - 400.0).abs() < 1.0,
            "50vw should be 400, got {}",
            rect.w
        );
    }

    /// Group E — the scroll-blit fast path must produce (nearly) the same
    /// pixels as a full re-raster at the new scroll, and must actually run
    /// (scroll_stats). The page has NO fixed/sticky elements.
    #[test]
    fn scroll_blit_matches_full_render() {
        // Tall document with distinct colored bands so every scroll
        // position is visually unique.
        let mut html = String::from("<html><body style=\"margin:0\">");
        for i in 0..12 {
            let shade = 20 + i * 18;
            html.push_str(&format!(
                "<div style=\"width:100%;height:100px;background-color:rgb({shade}, {shade}, {shade})\"></div>"
            ));
        }
        html.push_str("</body></html>");
        let doc = parse_html(html.as_bytes());
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[],
            &MediaContext::default(),
            Viewport {
                width: 300.0,
                height: 200.0,
            },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        assert!(!list.has_fixed_or_sticky, "test page must be blit-eligible");

        // Path A: incremental scroll via the SAME painter (blit path).
        let mut a = Painter::new();
        let opts = |scroll: f32| RenderOptions {
            viewport_width: 300,
            viewport_height: 200,
            scroll_y: scroll,
            scale: 1.0,
            ..RenderOptions::default()
        };
        let f0 = a
            .render(&list, opts(0.0), &mut engine.font_system)
            .expect("frame 0");
        let f1 = a
            .render(&list, opts(120.0), &mut engine.font_system)
            .expect("frame 1 (blit)");
        let (blits, fulls) = a.scroll_stats();
        assert!(blits >= 1, "the 120px scroll must have blitted");
        assert_eq!(fulls, 1, "only the initial frame full-rasters");

        // Path B: fresh painter, full raster directly at scroll 120.
        let mut b = Painter::new();
        let g1 = b
            .render(&list, opts(120.0), &mut engine.font_system)
            .expect("frame 1 (full)");
        assert_eq!((f1.width, f1.height), (g1.width, g1.height));

        // Compare: overlapping region = shifted old pixels (exact); the
        // exposed band re-rasters the same commands (sub-pixel identical
        // in theory; allow a tiny tolerance for float rounding).
        let mut worst: i32 = 0;
        let mut mismatched = 0usize;
        for (p, q) in f1.pixels.iter().zip(g1.pixels.iter()) {
            let d = (*p as i32 - *q as i32).abs();
            if d > worst {
                worst = d;
            }
            if d > 2 {
                mismatched += 1;
            }
        }
        assert!(
            worst <= 2 && mismatched == 0,
            "blit vs full render differ: worst={worst}, mismatched={mismatched}/{}",
            g1.pixels.len()
        );
        let _ = f0;
    }

    /// Group E — pages with position:fixed content must NOT take the blit
    /// path (fixed elements don't translate with the page scroll).
    #[test]
    fn scroll_blit_refuses_fixed_elements() {
        let html = br#"<html><body style="margin:0">
            <div style="height:2000px;background-color:#101010"></div>
            <div style="position:fixed;top:0;left:0;width:50px;height:50px;background-color:#ff0000"></div>
        </body></html>"#;
        let doc = parse_html(html);
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &[],
            &MediaContext::default(),
            Viewport {
                width: 300.0,
                height: 200.0,
            },
            &Default::default(),
        );
        let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
        assert!(
            list.has_fixed_or_sticky,
            "the fixed div must set the blit-refusal flag"
        );
        let mut painter = Painter::new();
        let opts = |scroll: f32| RenderOptions {
            viewport_width: 300,
            viewport_height: 200,
            scroll_y: scroll,
            scale: 1.0,
            ..RenderOptions::default()
        };
        let _ = painter
            .render(&list, opts(0.0), &mut engine.font_system)
            .expect("frame 0");
        let _ = painter
            .render(&list, opts(80.0), &mut engine.font_system)
            .expect("frame 1");
        let (blits, _) = painter.scroll_stats();
        assert_eq!(blits, 0, "fixed elements must force full re-raster");
    }

    /// Group E — DrawCmd::Canvas resolves pixels LAZILY per raster: a
    /// canvas redraw between two renders of the same list must show up in
    /// the frame.
    #[test]
    fn canvas_command_pulls_fresh_pixels_per_raster() {
        use crate::canvas2d::{new_registry, Canvas2D};
        use crate::display_list::DrawCmd;
        use crate::Rect;
        use rowser_parsing::cascade::BorderRadius;
        use std::cell::RefCell;
        use std::rc::Rc;

        let registry = new_registry();
        let node = 7u32;
        registry
            .borrow_mut()
            .insert(node, Rc::new(RefCell::new(Canvas2D::new(40, 40))));
        // Paint the canvas red.
        {
            let reg = registry.borrow();
            let canvas = reg.get(&node).cloned().unwrap();
            let mut c = canvas.borrow_mut();
            c.set_fill_color(crate::canvas2d::parse_color("#ff0000").unwrap());
            c.begin_path();
            c.rect(0.0, 0.0, 40.0, 40.0);
            c.fill(false);
        }

        let list = crate::DisplayList {
            commands: vec![DrawCmd::Canvas {
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 40.0,
                    h: 40.0,
                },
                radius: BorderRadius::default(),
                node,
            }],
            ..Default::default()
        };
        let mut painter = Painter::new();
        let mut engine = LayoutEngine::new();
        let frame1 = painter
            .render_ctx(
                &list,
                RenderOptions {
                    viewport_width: 60,
                    viewport_height: 60,
                    ..RenderOptions::default()
                },
                &mut engine.font_system,
                Some(&registry),
            )
            .expect("frame 1");
        let i = (20 * frame1.width as usize + 20) * 4;
        assert!(
            frame1.pixels[i] > 150 && frame1.pixels[i + 1] < 90 && frame1.pixels[i + 3] == 255,
            "canvas should paint red, got {:?}",
            &frame1.pixels[i..i + 4]
        );

        // Redraw blue WITHOUT touching the display list.
        {
            let reg = registry.borrow();
            let canvas = reg.get(&node).cloned().unwrap();
            let mut c = canvas.borrow_mut();
            c.set_fill_color(crate::canvas2d::parse_color("#0000ff").unwrap());
            c.begin_path();
            c.rect(0.0, 0.0, 40.0, 40.0);
            c.fill(false);
        }
        let frame2 = painter
            .render_ctx(
                &list,
                RenderOptions {
                    viewport_width: 60,
                    viewport_height: 60,
                    ..RenderOptions::default()
                },
                &mut engine.font_system,
                Some(&registry),
            )
            .expect("frame 2");
        let i = (20 * frame2.width as usize + 20) * 4;
        assert!(
            frame2.pixels[i + 2] > 150 && frame2.pixels[i + 1] < 90,
            "same list must paint the NEW canvas pixels (blue), got {:?}",
            &frame2.pixels[i..i + 4]
        );
    }
}
