//! Software rasterization of display lists via tiny-skia, with swash glyph
//! rasterization.

use cosmic_text::{FontSystem, SwashCache, SwashContent, SwashImage};
use tiny_skia::{Paint, Pixmap, Transform};

use rowser_layout::text::TextRun;

use crate::display_list::{DisplayList, DrawCmd};
use crate::{to_skia_color, Rect};

/// Options for one render pass.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Viewport width.
    pub viewport_width: u32,
    /// Viewport height.
    pub viewport_height: u32,
    /// Vertical scroll offset in document coordinates.
    pub scroll_y: f32,
    /// Page background (used for the initial clear).
    pub background: rowser_parsing::cascade::Rgba,
    /// Find-in-page match rectangles (document coordinates).
    pub find_matches: Vec<crate::Rect>,
    /// Index of the active match (painted more strongly).
    pub active_match: Option<usize>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            viewport_width: 1280,
            viewport_height: 800,
            scroll_y: 0.0,
            background: rowser_parsing::cascade::Rgba::new_opaque(255, 255, 255),
            find_matches: Vec::new(),
            active_match: None,
        }
    }
}

/// The software painter. Owns the swash glyph cache; borrow the engine's
/// `FontSystem` per render pass (see `docs/ARCHITECTURE.md`).
pub struct Painter {
    swash_cache: SwashCache,
    frame_id: u64,
}

impl Default for Painter {
    fn default() -> Self {
        Self::new()
    }
}

impl Painter {
    /// Creates the painter.
    pub fn new() -> Self {
        Painter {
            swash_cache: SwashCache::new(),
            frame_id: 0,
        }
    }

    /// Rasterizes `list` into a fresh frame.
    pub fn render(
        &mut self,
        list: &DisplayList,
        options: RenderOptions,
        font_system: &mut FontSystem,
    ) -> Option<crate::Frame> {
        let mut pixmap = Pixmap::new(options.viewport_width, options.viewport_height)?;
        pixmap.fill(to_skia_color(options.background));
        let scroll = (0.0f32, -options.scroll_y);
        self.paint(list, &mut pixmap, scroll, font_system);
        self.paint_find_highlights(&mut pixmap, &options, scroll);
        self.frame_id += 1;
        Some(crate::Frame {
            width: pixmap.width(),
            height: pixmap.height(),
            pixels: pixmap.take(),
            id: self.frame_id,
        })
    }

    /// Paints find-in-page match rectangles on top of the content.
    fn paint_find_highlights(
        &mut self,
        pixmap: &mut Pixmap,
        options: &RenderOptions,
        scroll: (f32, f32),
    ) {
        if options.find_matches.is_empty() {
            return;
        }
        let viewport = (pixmap.width() as f32, pixmap.height() as f32);
        let normal = rowser_parsing::cascade::Rgba::new(255, 170, 0, 90);
        let active = rowser_parsing::cascade::Rgba::new(255, 140, 0, 130);
        for (index, rect) in options.find_matches.iter().enumerate() {
            let color = if Some(index) == options.active_match {
                active
            } else {
                normal
            };
            let rect = translate(rect, scroll).clipped(viewport);
            if rect.w <= 0.0 || rect.h <= 0.0 {
                continue;
            }
            fill_rect(pixmap, &rect, color);
        }
    }

    fn paint(
        &mut self,
        list: &DisplayList,
        pixmap: &mut Pixmap,
        scroll: (f32, f32),
        font_system: &mut FontSystem,
    ) {
        let viewport = (pixmap.width() as f32, pixmap.height() as f32);
        // Clip stack: every command intersects the current clip.
        let mut clips: Vec<crate::Rect> = Vec::new();
        for cmd in &list.commands {
            match cmd {
                DrawCmd::PushClip { rect } => {
                    let rect = translate(rect, scroll).clipped(viewport);
                    let combined = match clips.last() {
                        Some(prev) => intersect(prev, &rect),
                        None => rect,
                    };
                    clips.push(combined);
                }
                DrawCmd::PopClip => {
                    clips.pop();
                }
                DrawCmd::Rect { rect, color } => {
                    let rect = translate(rect, scroll).clipped(viewport);
                    let rect = match clips.last() {
                        Some(c) => intersect(c, &rect),
                        None => rect,
                    };
                    if rect.w <= 0.0 || rect.h <= 0.0 || color.a == 0 {
                        continue;
                    }
                    fill_rect(pixmap, &rect, *color);
                }
                DrawCmd::Border {
                    rect,
                    widths,
                    colors,
                } => {
                    let rect = translate(rect, scroll);
                    paint_border(
                        pixmap,
                        &rect,
                        *widths,
                        *colors,
                        viewport,
                        clips.last().copied(),
                    );
                }
                DrawCmd::Text { run } => {
                    self.paint_run(
                        pixmap,
                        run,
                        scroll,
                        font_system,
                        viewport,
                        clips.last().copied(),
                    );
                }
                DrawCmd::Image { rect, image } => {
                    let rect = translate(rect, scroll).clipped(viewport);
                    let rect = match clips.last() {
                        Some(c) => intersect(c, &rect),
                        None => rect,
                    };
                    if rect.w <= 0.0 || rect.h <= 0.0 {
                        continue;
                    }
                    paint_image(pixmap, &rect, image);
                }
            }
        }
    }

    fn paint_run(
        &mut self,
        pixmap: &mut Pixmap,
        run: &TextRun,
        scroll: (f32, f32),
        font_system: &mut FontSystem,
        viewport: (f32, f32),
        clip: Option<crate::Rect>,
    ) {
        for glyph in &run.glyphs {
            let x = glyph.x as f32 + scroll.0;
            let y = glyph.y as f32 + scroll.1;
            if y < -200.0 || y > viewport.1 + 200.0 || x < -200.0 || x > viewport.0 + 200.0 {
                continue;
            }
            let image = match self.swash_cache.get_image(font_system, glyph.cache_key) {
                Some(image) => image,
                None => continue,
            };
            blit_glyph(pixmap, glyph, image, x as i32, y as i32, clip);
        }
    }

    /// Clears the glyph cache (memory pressure valve for the engine).
    pub fn clear_glyph_cache(&mut self) {
        self.swash_cache.image_cache.clear();
        self.swash_cache.outline_command_cache.clear();
    }

    /// Current glyph cache entry count.
    pub fn glyph_cache_len(&self) -> usize {
        self.swash_cache.image_cache.len()
    }
}

/// Axis-aligned rectangle intersection.
fn intersect(a: &Rect, b: &Rect) -> Rect {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    if x1 <= x0 || y1 <= y0 {
        return Rect {
            x: x0,
            y: y0,
            w: 0.0,
            h: 0.0,
        };
    }
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

fn translate(rect: &Rect, scroll: (f32, f32)) -> Rect {
    Rect {
        x: rect.x + scroll.0,
        y: rect.y + scroll.1,
        w: rect.w,
        h: rect.h,
    }
}

fn fill_rect(pixmap: &mut Pixmap, rect: &Rect, color: rowser_parsing::cascade::Rgba) {
    let Some(sk_rect) = tiny_skia::Rect::from_xywh(rect.x, rect.y, rect.w, rect.h) else {
        return;
    };
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(to_skia_color(color));
    pixmap.fill_rect(sk_rect, &paint, Transform::identity(), None);
}

fn paint_border(
    pixmap: &mut Pixmap,
    rect: &Rect,
    widths: [f32; 4],
    colors: [rowser_parsing::cascade::Rgba; 4],
    viewport: (f32, f32),
    clip: Option<Rect>,
) {
    // top, right, bottom, left edge rects.
    let (t, r, b, l) = (widths[0], widths[1], widths[2], widths[3]);
    let edges = [
        (
            Rect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: t,
            },
            colors[0],
        ),
        (
            Rect {
                x: rect.right() - r,
                y: rect.y,
                w: r,
                h: rect.h,
            },
            colors[1],
        ),
        (
            Rect {
                x: rect.x,
                y: rect.bottom() - b,
                w: rect.w,
                h: b,
            },
            colors[2],
        ),
        (
            Rect {
                x: rect.x,
                y: rect.y,
                w: l,
                h: rect.h,
            },
            colors[3],
        ),
    ];
    for (edge, color) in edges {
        if edge.w <= 0.0 || edge.h <= 0.0 || color.a == 0 {
            continue;
        }
        let clipped = edge.clipped(viewport);
        let clipped = match clip {
            Some(c) => intersect(&c, &clipped),
            None => clipped,
        };
        if clipped.w <= 0.0 || clipped.h <= 0.0 {
            continue;
        }
        fill_rect(pixmap, &clipped, color);
    }
}

fn paint_image(pixmap: &mut Pixmap, rect: &Rect, image: &crate::DecodedImage) {
    if image.width == 0 || image.height == 0 || rect.w <= 0.0 || rect.h <= 0.0 {
        return;
    }
    // Premultiply into a source pixmap.
    let mut premul = vec![0u8; image.rgba.len()];
    for (dst, src) in premul.chunks_exact_mut(4).zip(image.rgba.chunks_exact(4)) {
        let a = src[3] as u32;
        dst[3] = src[3];
        if a == 0 {
            dst[0] = 0;
            dst[1] = 0;
            dst[2] = 0;
        } else {
            dst[0] = ((src[0] as u32 * a + 127) / 255) as u8;
            dst[1] = ((src[1] as u32 * a + 127) / 255) as u8;
            dst[2] = ((src[2] as u32 * a + 127) / 255) as u8;
        }
    }
    let Some(src) = Pixmap::from_vec(
        premul,
        tiny_skia::IntSize::from_wh(image.width, image.height)
            .unwrap_or(tiny_skia::IntSize::from_wh(1, 1).unwrap()),
    ) else {
        return;
    };
    let paint = tiny_skia::PixmapPaint::default();
    let scale_x = rect.w / image.width as f32;
    let scale_y = rect.h / image.height as f32;
    let transform = Transform::from_scale(scale_x, scale_y).pre_translate(rect.x, rect.y);
    pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
}

/// Blits one swash glyph onto the pixmap with manual alpha blending.
///
/// Operates directly on the premultiplied RGBA byte buffer.
fn blit_glyph(
    pixmap: &mut Pixmap,
    glyph: &rowser_layout::text::PlacedGlyph,
    image: &SwashImage,
    x: i32,
    y: i32,
    clip: Option<Rect>,
) {
    let base_x = x + image.placement.left;
    let base_y = y - image.placement.top;
    // Glyph clip window [gx0, gx1) x [gy0, gy1) in pixmap coordinates;
    // out-of-window pixels are clamped to the window edge (zero-width when
    // fully outside, which the base bounds checks then discard).
    let (gx0, gx1, gy0, gy1) = match clip {
        Some(c) => (
            (c.x.ceil() as i32).max(0),
            ((c.x + c.w).floor() as i32).min(pixmap.width() as i32),
            (c.y.ceil() as i32).max(0),
            ((c.y + c.h).floor() as i32).min(pixmap.height() as i32),
        ),
        None => (0, pixmap.width() as i32, 0, pixmap.height() as i32),
    };
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    let stride = pixmap.width() as usize * 4;
    match image.content {
        SwashContent::Mask => {
            let color = glyph.color;
            let mut i = 0usize;
            for off_y in 0..image.placement.height as i32 {
                let row_y = base_y + off_y;
                if row_y < 0 || row_y >= h || row_y < gy0 || row_y >= gy1 {
                    i += image.placement.width as usize;
                    continue;
                }
                for off_x in 0..image.placement.width as i32 {
                    let alpha = image.data[i];
                    if alpha > 0 {
                        let px = base_x + off_x;
                        if px >= 0 && px < w && px >= gx0 && px < gx1 {
                            blend_pixel(
                                pixmap,
                                row_y as usize * stride + px as usize * 4,
                                color.r,
                                color.g,
                                color.b,
                                alpha,
                            );
                        }
                    }
                    i += 1;
                }
            }
        }
        SwashContent::Color => {
            let mut i = 0usize;
            for off_y in 0..image.placement.height as i32 {
                let row_y = base_y + off_y;
                if row_y < 0 || row_y >= h || row_y < gy0 || row_y >= gy1 {
                    i += image.placement.width as usize * 4;
                    continue;
                }
                for off_x in 0..image.placement.width as i32 {
                    let (r, g, b, a) = (
                        image.data[i],
                        image.data[i + 1],
                        image.data[i + 2],
                        image.data[i + 3],
                    );
                    if a > 0 {
                        let px = base_x + off_x;
                        if px >= 0 && px < w && px >= gx0 && px < gx1 {
                            blend_pixel(
                                pixmap,
                                row_y as usize * stride + px as usize * 4,
                                r,
                                g,
                                b,
                                a,
                            );
                        }
                    }
                    i += 4;
                }
            }
        }
        _ => {}
    }
}

/// Src-over blend of a straight-alpha color onto premultiplied pixels.
fn blend_pixel(pixmap: &mut Pixmap, offset: usize, r: u8, g: u8, b: u8, a: u8) {
    let data = pixmap.data_mut();
    let Some(dst) = data.get_mut(offset..offset + 4) else {
        return;
    };
    if a == 255 {
        dst[0] = r;
        dst[1] = g;
        dst[2] = b;
        dst[3] = 255;
        return;
    }
    // Premultiplied src-over: out = src + dst * (1 - sa).
    let sa = a as u32;
    let inv = 255 - sa;
    let dr = (r as u32 * sa + 127) / 255;
    let dg = (g as u32 * sa + 127) / 255;
    let db = (b as u32 * sa + 127) / 255;
    dst[0] = (dr + (dst[0] as u32 * inv + 127) / 255).min(255) as u8;
    dst[1] = (dg + (dst[1] as u32 * inv + 127) / 255).min(255) as u8;
    dst[2] = (db + (dst[2] as u32 * inv + 127) / 255).min(255) as u8;
    dst[3] = (sa + (dst[3] as u32 * inv + 127) / 255).min(255) as u8;
}
