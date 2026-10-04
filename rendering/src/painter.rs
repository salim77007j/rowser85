//! Software rasterization of display lists via tiny-skia, with swash glyph
//! rasterization.
//!
//! Group B extensions: rounded rects, gradients, shadows (manual separable
//! box blur — tiny-skia 0.12 has no mask-filter blur), background image
//! layers, 2D transforms, opacity/filter groups, page-fixed and sticky
//! anchors, and per-element scroll translation.

use cosmic_text::{FontSystem, SwashCache, SwashContent, SwashImage};
use tiny_skia::{FillRule, Paint, Pixmap, PixmapPaint, Shader, Transform};

use rowser_layout::text::TextRun;
use rowser_parsing::cascade::{
    BorderRadius, FilterSpec, GradientGeometry, GradientSpec, GradientStop, Rgba,
};

use crate::display_list::{DisplayList, DrawCmd, StickyInfo};
use crate::{to_skia_color, DecodedImage, Rect};

/// Options for one render pass.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Viewport width in CSS pixels.
    pub viewport_width: u32,
    /// Viewport height in CSS pixels.
    pub viewport_height: u32,
    /// Vertical scroll offset in document coordinates.
    pub scroll_y: f32,
    /// Device pixel ratio: the frame is rasterized at viewport x scale
    /// device pixels (HiDPI). 1.0 = CSS px == device px.
    pub scale: f32,
    /// Page background (used for the initial clear).
    pub background: Rgba,
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
            scale: 1.0,
            background: Rgba::new_opaque(255, 255, 255),
            find_matches: Vec::new(),
            active_match: None,
        }
    }
}

/// Row-major affine [a b c d e f]: x' = a*x + c*y + e, y' = b*x + d*y + f.
type Affine = [f32; 6];

const IDENTITY: Affine = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn mul(a: Affine, b: Affine) -> Affine {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

fn apply(m: Affine, x: f32, y: f32) -> (f32, f32) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

/// True when the matrix is a pure translation (rects stay axis-aligned).
fn is_translation(m: Affine) -> bool {
    (m[0] - 1.0).abs() < 1e-6 && (m[3] - 1.0).abs() < 1e-6 && m[1].abs() < 1e-6 && m[2].abs() < 1e-6
}

fn to_skia_transform(m: Affine) -> Transform {
    Transform::from_row(m[0], m[1], m[2], m[3], m[4], m[5])
}

/// One clip shape in screen space (bbox + radii).
#[derive(Debug, Clone, Copy, PartialEq)]
struct ClipShape {
    rect: Rect,
    radius: BorderRadius,
}

/// Mutable paint state threaded through the command interpreter.
#[derive(Clone)]
struct PaintState {
    /// doc → screen affine (page scroll, element scroll, transforms).
    transform: Affine,
    /// Snapshot stack for PopTransform (PushFixed/PushSticky/PushTransform).
    transform_stack: Vec<Affine>,
    /// Screen-space clip stack.
    clips: Vec<ClipShape>,
    /// Device pixel ratio (glyph rasterization density).
    scale: f32,
}

impl PaintState {
    fn new(scroll_y: f32, scale: f32) -> Self {
        PaintState {
            transform: [scale, 0.0, 0.0, scale, 0.0, -scroll_y * scale],
            transform_stack: Vec::new(),
            clips: Vec::new(),
            scale,
        }
    }

    /// Current effective clip bbox (intersection), screen space.
    fn clip(&self) -> Option<Rect> {
        let mut acc: Option<Rect> = None;
        for c in &self.clips {
            acc = Some(match acc {
                None => c.rect,
                Some(a) => intersect(&a, &c.rect),
            });
        }
        acc
    }
}

/// Cached clip mask (rebuilds only when the clip stack changes).
struct MaskCache {
    key: Vec<ClipShape>,
    extra: Option<ClipShape>,
    mask: Option<tiny_skia::Mask>,
}

/// The software painter. Owns the swash glyph cache; borrow the engine's
/// `FontSystem` per render pass (see `docs/ARCHITECTURE.md`).
pub struct Painter {
    swash_cache: SwashCache,
    frame_id: u64,
    mask_cache: Option<MaskCache>,
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
            mask_cache: None,
        }
    }

    /// Rasterizes `list` into a fresh frame.
    pub fn render(
        &mut self,
        list: &DisplayList,
        options: RenderOptions,
        font_system: &mut FontSystem,
    ) -> Option<crate::Frame> {
        let scale = if options.scale.is_finite() && options.scale > 0.0 {
            options.scale
        } else {
            1.0
        };
        let w = (options.viewport_width as f32 * scale).ceil() as u32;
        let h = (options.viewport_height as f32 * scale).ceil() as u32;
        let mut pixmap = Pixmap::new(w, h)?;
        pixmap.fill(to_skia_color(options.background));
        let mut state = PaintState::new(options.scroll_y, scale);
        let viewport = (pixmap.width() as f32, pixmap.height() as f32);
        self.mask_cache = None;
        self.paint_commands(
            &list.commands,
            &mut pixmap,
            &mut state,
            font_system,
            viewport,
        );
        self.paint_find_highlights(&mut pixmap, &options);
        self.frame_id += 1;
        Some(crate::Frame {
            width: pixmap.width(),
            height: pixmap.height(),
            pixels: pixmap.take(),
            id: self.frame_id,
        })
    }

    /// Paints find-in-page match rectangles on top of the content.
    fn paint_find_highlights(&mut self, pixmap: &mut Pixmap, options: &RenderOptions) {
        if options.find_matches.is_empty() {
            return;
        }
        let viewport = (pixmap.width() as f32, pixmap.height() as f32);
        let scale = if options.scale.is_finite() && options.scale > 0.0 {
            options.scale
        } else {
            1.0
        };
        let scroll = (0.0f32, -options.scroll_y);
        let normal = Rgba::new(255, 170, 0, 90);
        let active = Rgba::new(255, 140, 0, 130);
        for (index, rect) in options.find_matches.iter().enumerate() {
            let color = if Some(index) == options.active_match {
                active
            } else {
                normal
            };
            // doc → device: scroll then scale.
            let mut r = translate(rect, scroll);
            r = Rect {
                x: r.x * scale,
                y: r.y * scale,
                w: r.w * scale,
                h: r.h * scale,
            };
            let rect = r.clipped(viewport);
            if rect.w <= 0.0 || rect.h <= 0.0 {
                continue;
            }
            fill_rect(pixmap, &rect, color);
        }
    }

    /// Interprets the command stream. Groups (opacity/filter) recurse into
    /// offscreen layers; clips and transforms push onto the state.
    fn paint_commands(
        &mut self,
        cmds: &[DrawCmd],
        pixmap: &mut Pixmap,
        state: &mut PaintState,
        font_system: &mut FontSystem,
        viewport: (f32, f32),
    ) {
        let mut i = 0usize;
        while i < cmds.len() {
            match &cmds[i] {
                DrawCmd::PushClip {
                    rect,
                    radius,
                    scroll,
                } => {
                    // The clip shape stays fixed; the content inside may
                    // translate by the element scroll offset.
                    let screen = transform_rect(state.transform, *rect).clipped(viewport);
                    state.clips.push(ClipShape {
                        rect: screen,
                        radius: *radius,
                    });
                    if scroll != &(0.0, 0.0) {
                        state.transform =
                            mul([1.0, 0.0, 0.0, 1.0, -scroll.0, -scroll.1], state.transform);
                    }
                }
                DrawCmd::PopClip => {
                    state.clips.pop();
                }
                DrawCmd::PushTransform { matrix } => {
                    state.transform_stack.push(state.transform);
                    state.transform = mul(*matrix, state.transform);
                }
                DrawCmd::PopTransform => {
                    state.transform = state.transform_stack.pop().unwrap_or(IDENTITY);
                }
                DrawCmd::PushFixed => {
                    // Cancel the page-scroll translation: net 0.
                    let sy = -state.transform[5];
                    state.transform_stack.push(state.transform);
                    state.transform = mul([1.0, 0.0, 0.0, 1.0, 0.0, sy], state.transform);
                }
                DrawCmd::PushSticky { info } => {
                    let sy = -state.transform[5];
                    // sticky_offset works in CSS px (rect/insets); the
                    // accumulated translation and viewport are device px —
                    // convert, then scale the offset back.
                    let dpr = state.scale.max(1e-3);
                    let offset = sticky_offset(*info, sy / dpr, viewport.1 / dpr) * dpr;
                    state.transform_stack.push(state.transform);
                    state.transform = mul([1.0, 0.0, 0.0, 1.0, 0.0, offset], state.transform);
                }
                DrawCmd::PushOpacity { alpha } => {
                    let end = matching_pop(cmds, i);
                    if let Some(mut layer) = Pixmap::new(viewport.0 as u32, viewport.1 as u32) {
                        let mut sub_state = state.clone();
                        self.paint_commands(
                            &cmds[i + 1..end],
                            &mut layer,
                            &mut sub_state,
                            font_system,
                            viewport,
                        );
                        let paint = PixmapPaint {
                            opacity: alpha.clamp(0.0, 1.0),
                            ..PixmapPaint::default()
                        };
                        let mask = self.build_mask(state, viewport, None);
                        pixmap.draw_pixmap(
                            0,
                            0,
                            layer.as_ref(),
                            &paint,
                            Transform::identity(),
                            mask.as_ref(),
                        );
                    }
                    i = end;
                }
                DrawCmd::PopOpacity => {}
                DrawCmd::PushFilter { filters, region } => {
                    let end = matching_pop(cmds, i);
                    if let Some(mut layer) = Pixmap::new(viewport.0 as u32, viewport.1 as u32) {
                        let mut sub_state = state.clone();
                        self.paint_commands(
                            &cmds[i + 1..end],
                            &mut layer,
                            &mut sub_state,
                            font_system,
                            viewport,
                        );
                        apply_filters(&mut layer, filters);
                        // Clip the composite to the effect region + clips.
                        let screen_region = transform_rect(state.transform, *region);
                        let extra = ClipShape {
                            rect: screen_region,
                            radius: BorderRadius::default(),
                        };
                        let paint = PixmapPaint::default();
                        let mask = self.build_mask(state, viewport, Some(extra));
                        pixmap.draw_pixmap(
                            0,
                            0,
                            layer.as_ref(),
                            &paint,
                            Transform::identity(),
                            mask.as_ref(),
                        );
                    }
                    i = end;
                }
                DrawCmd::PopFilter => {}
                DrawCmd::Rect {
                    rect,
                    color,
                    radius,
                } => {
                    self.paint_rect(pixmap, state, *rect, *color, *radius, viewport);
                }
                DrawCmd::Gradient { rect, radius, spec } => {
                    self.paint_gradient(pixmap, state, *rect, *radius, spec, viewport);
                }
                DrawCmd::BgImage {
                    rect,
                    image,
                    radius,
                } => {
                    self.paint_image(pixmap, state, *rect, image, *radius, viewport);
                }
                DrawCmd::Image {
                    rect,
                    image,
                    radius,
                } => {
                    self.paint_image(pixmap, state, *rect, image, *radius, viewport);
                }
                DrawCmd::Border {
                    rect,
                    widths,
                    colors,
                    radius,
                } => {
                    self.paint_border(pixmap, state, *rect, *widths, *colors, *radius, viewport);
                }
                DrawCmd::BoxShadow {
                    rect,
                    radius,
                    shadow,
                } => {
                    self.paint_box_shadow(pixmap, state, *rect, *radius, shadow, viewport);
                }
                DrawCmd::Text { run, shadows } => {
                    self.paint_run(pixmap, run, shadows, state, font_system, viewport);
                }
            }
            i += 1;
        }
    }

    /// Solid rect fill (rounded + transformed aware).
    fn paint_rect(
        &mut self,
        pixmap: &mut Pixmap,
        state: &PaintState,
        rect: Rect,
        color: Rgba,
        radius: BorderRadius,
        viewport: (f32, f32),
    ) {
        if color.a == 0 || rect.w <= 0.0 || rect.h <= 0.0 {
            return;
        }
        if radius.is_zero() && is_translation(state.transform) && !state.has_mask_clips() {
            // Fast path: axis-aligned bbox fill with rect clip intersect.
            let screen = transform_rect(state.transform, rect);
            let screen = match state.clip() {
                Some(c) => intersect(&screen.clipped(viewport), &c),
                None => screen.clipped(viewport),
            };
            if screen.w <= 0.0 || screen.h <= 0.0 {
                return;
            }
            fill_rect(pixmap, &screen, color);
            return;
        }
        let path = rounded_rect_path(rect, &radius);
        let paint = Paint {
            anti_alias: true,
            shader: Shader::SolidColor(to_skia_color(color)),
            ..Paint::default()
        };
        let mask = self.build_mask(state, viewport, None);
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            to_skia_transform(state.transform),
            mask.as_ref(),
        );
    }

    /// Gradient fill (linear/radial, rounded clip).
    fn paint_gradient(
        &mut self,
        pixmap: &mut Pixmap,
        state: &PaintState,
        rect: Rect,
        radius: BorderRadius,
        spec: &GradientSpec,
        viewport: (f32, f32),
    ) {
        if rect.w <= 0.0 || rect.h <= 0.0 || spec.stops.is_empty() {
            return;
        }
        let stops = skia_stops(&spec.stops);
        if stops.is_empty() {
            return;
        }
        let shader = match &spec.geometry {
            GradientGeometry::Linear { angle_deg } => {
                // CSS angle: 0 = to top, 90 = to right.
                let rad = angle_deg.to_radians();
                let (dx, dy) = (rad.sin(), -rad.cos());
                let cx = rect.x + rect.w / 2.0;
                let cy = rect.y + rect.h / 2.0;
                let half = (rect.w * dx.abs() + rect.h * dy.abs()) / 2.0;
                if half <= 0.0 {
                    return;
                }
                let start = tiny_skia::Point::from_xy(cx - dx * half, cy - dy * half);
                let end = tiny_skia::Point::from_xy(cx + dx * half, cy + dy * half);
                tiny_skia::LinearGradient::new(
                    start,
                    end,
                    stops,
                    tiny_skia::SpreadMode::Pad,
                    Transform::identity(),
                )
            }
            GradientGeometry::Radial { cx, cy } => {
                let center = tiny_skia::Point::from_xy(rect.x + cx * rect.w, rect.y + cy * rect.h);
                // farthest-corner radius
                let corners = [
                    (rect.x, rect.y),
                    (rect.right(), rect.y),
                    (rect.x, rect.bottom()),
                    (rect.right(), rect.bottom()),
                ];
                let r = corners
                    .iter()
                    .map(|(x, y)| ((x - center.x).powi(2) + (y - center.y).powi(2)).sqrt())
                    .fold(0.0f32, f32::max);
                if r <= 0.0 {
                    return;
                }
                let edge = tiny_skia::Point::from_xy(center.x + r, center.y);
                tiny_skia::RadialGradient::new(
                    center,
                    0.0,
                    edge,
                    r,
                    stops,
                    tiny_skia::SpreadMode::Pad,
                    Transform::identity(),
                )
            }
        };
        let Some(shader) = shader else { return };
        let paint = Paint {
            anti_alias: true,
            shader,
            ..Paint::default()
        };
        let path = rounded_rect_path(rect, &radius);
        let mask = self.build_mask(state, viewport, None);
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            to_skia_transform(state.transform),
            mask.as_ref(),
        );
    }

    /// Image blit (rounded clip + transform via tiny-skia).
    fn paint_image(
        &mut self,
        pixmap: &mut Pixmap,
        state: &PaintState,
        rect: Rect,
        image: &DecodedImage,
        radius: BorderRadius,
        viewport: (f32, f32),
    ) {
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
        let paint = PixmapPaint::default();
        let scale_x = rect.w / image.width as f32;
        let scale_y = rect.h / image.height as f32;
        let local = Transform::from_scale(scale_x, scale_y).pre_translate(rect.x, rect.y);
        let full = to_skia_transform(mul(state.transform, matrix_of(local)));
        // Fast path: translation-only, no radius, rect clips.
        if is_translation(state.transform) && radius.is_zero() && !state.has_mask_clips() {
            let screen = transform_rect(state.transform, rect);
            let screen = match state.clip() {
                Some(c) => intersect(&screen.clipped(viewport), &c),
                None => screen.clipped(viewport),
            };
            if screen.w <= 0.5 || screen.h <= 0.5 {
                return;
            }
            // Emulate the clip by drawing through a bounding-rect-limited
            // path: the visible region in DESTINATION pixels, content
            // mapped from the source via scale + crop. (Sizing the layer
            // in source pixels — screen.w / scale — cropped upscaled
            // images to their top-left quadrant: logos drew as tiny
            // slivers instead of filling their layout rect.)
            let sub_w = (screen.w.ceil() as u32).max(1);
            let sub_h = (screen.h.ceil() as u32).max(1);
            if let Some(mut sub) = Pixmap::new(sub_w, sub_h) {
                let crop_x =
                    (screen.x - (rect.x + state.transform[4])).max(0.0) / scale_x.max(1e-6);
                let crop_y =
                    (screen.y - (rect.y + state.transform[5])).max(0.0) / scale_y.max(1e-6);
                let inner = Transform::from_scale(scale_x, scale_y)
                    .pre_translate(-crop_x * scale_x, -crop_y * scale_y);
                sub.draw_pixmap(0, 0, src.as_ref(), &paint, inner, None);
                pixmap.draw_pixmap(
                    screen.x.round() as i32,
                    screen.y.round() as i32,
                    sub.as_ref(),
                    &paint,
                    Transform::identity(),
                    None,
                );
            }
            return;
        }
        let mask = self.build_mask(state, viewport, None);
        pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, full, mask.as_ref());
    }

    /// Border ring: uniform color → single ring path; mixed colors → ring
    /// in the first color plus straight per-edge bands (corners blended).
    #[allow(clippy::too_many_arguments)]
    fn paint_border(
        &mut self,
        pixmap: &mut Pixmap,
        state: &PaintState,
        rect: Rect,
        widths: [f32; 4],
        colors: [Rgba; 4],
        radius: BorderRadius,
        viewport: (f32, f32),
    ) {
        let (t, r, b, l) = (widths[0], widths[1], widths[2], widths[3]);
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return;
        }
        if radius.is_zero() && is_translation(state.transform) && !state.has_mask_clips() {
            paint_border_edges(
                pixmap,
                &transform_rect(state.transform, rect),
                widths,
                colors,
                viewport,
                state.clip(),
            );
            return;
        }
        let uniform = colors.iter().all(|c| *c == colors[0]);
        if let Some(path) = border_ring_path(rect, widths, &radius) {
            let mut paint = Paint {
                anti_alias: true,
                ..Paint::default()
            };
            paint.shader = Shader::SolidColor(to_skia_color(colors[0]));
            let mask = self.build_mask(state, viewport, None);
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::EvenOdd,
                to_skia_transform(state.transform),
                mask.as_ref(),
            );
        }
        if uniform {
            return;
        }
        // Per-edge straight bands between the corner arcs.
        let (tl, tr, br, bl) = (
            resolved(radius.top_left, rect).min(l),
            resolved(radius.top_right, rect).min(r),
            resolved(radius.bottom_right, rect).min(r),
            resolved(radius.bottom_left, rect).min(l),
        );
        let edges = [
            (
                Rect {
                    x: rect.x + tl,
                    y: rect.y,
                    w: (rect.w - tl - tr).max(0.0),
                    h: t,
                },
                colors[0],
            ),
            (
                Rect {
                    x: rect.right() - r,
                    y: rect.y + resolved(radius.top_right, rect).min(t),
                    w: r,
                    h: (rect.h
                        - resolved(radius.top_right, rect).min(t)
                        - resolved(radius.bottom_right, rect).min(b))
                    .max(0.0),
                },
                colors[1],
            ),
            (
                Rect {
                    x: rect.x + bl,
                    y: rect.bottom() - b,
                    w: (rect.w - bl - br).max(0.0),
                    h: b,
                },
                colors[2],
            ),
            (
                Rect {
                    x: rect.x,
                    y: rect.y + resolved(radius.top_left, rect).min(t),
                    w: l,
                    h: (rect.h
                        - resolved(radius.top_left, rect).min(t)
                        - resolved(radius.bottom_left, rect).min(b))
                    .max(0.0),
                },
                colors[3],
            ),
        ];
        for (edge, color) in edges {
            if edge.w <= 0.0 || edge.h <= 0.0 || color == colors[0] {
                continue;
            }
            let path = rounded_rect_path(edge, &BorderRadius::default());
            let mut paint = Paint {
                anti_alias: true,
                ..Paint::default()
            };
            paint.shader = Shader::SolidColor(to_skia_color(color));
            let mask = self.build_mask(state, viewport, None);
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                to_skia_transform(state.transform),
                mask.as_ref(),
            );
        }
    }

    /// Box shadow: shape fill + manual blur into a temp layer, composite.
    fn paint_box_shadow(
        &mut self,
        pixmap: &mut Pixmap,
        state: &PaintState,
        rect: Rect,
        radius: BorderRadius,
        shadow: &rowser_parsing::cascade::BoxShadowSpec,
        viewport: (f32, f32),
    ) {
        if shadow.color.a == 0 || rect.w <= 0.0 || rect.h <= 0.0 {
            return;
        }
        let blur = shadow.blur.max(0.0);
        let spread = shadow.spread;
        // Shadow shape in doc coords.
        let (shape, _shape_radius, path) = if shadow.inset {
            let shape = Rect {
                x: rect.x + shadow.x,
                y: rect.y + shadow.y,
                w: rect.w,
                h: rect.h,
            };
            // ring: outer = shape, inner shrunk.
            let shrink = (spread.max(0.0) + blur / 3.0).min(shape.w.min(shape.h) / 2.0 - 0.5);
            let inner = Rect {
                x: shape.x + shrink,
                y: shape.y + shrink,
                w: (shape.w - shrink * 2.0).max(0.0),
                h: (shape.h - shrink * 2.0).max(0.0),
            };
            let path = merge_paths(
                rounded_rect_path(shape, &radius),
                rounded_rect_path(inner, &grown_radius(radius, shrink)),
            );
            (shape, radius, path)
        } else {
            let shape = Rect {
                x: rect.x + shadow.x - spread,
                y: rect.y + shadow.y - spread,
                w: rect.w + spread * 2.0,
                h: rect.h + spread * 2.0,
            };
            let grown = grown_radius(radius, spread);
            let path = rounded_rect_path(shape, &grown);
            (shape, grown, path)
        };
        // Layer covering the shape bbox + blur pad (screen space).
        let pad = blur * 2.0 + 4.0;
        let screen_shape = transform_rect(state.transform, shape);
        let bounds = Rect {
            x: screen_shape.x - pad,
            y: screen_shape.y - pad,
            w: screen_shape.w + pad * 2.0,
            h: screen_shape.h + pad * 2.0,
        }
        .clipped(viewport);
        if bounds.w <= 1.0 || bounds.h <= 1.0 {
            return;
        }
        let Some(mut layer) = Pixmap::new(bounds.w as u32, bounds.h as u32) else {
            return;
        };
        let mut paint = Paint {
            anti_alias: true,
            ..Paint::default()
        };
        paint.shader = Shader::SolidColor(to_skia_color(shadow.color));
        // doc → layer coords: current transform then translate -bounds.xy.
        let layer_transform = to_skia_transform(mul(
            [1.0, 0.0, 0.0, 1.0, -bounds.x, -bounds.y],
            state.transform,
        ));
        layer.fill_path(&path, &paint, FillRule::EvenOdd, layer_transform, None);
        // Blur the layer.
        if blur > 0.5 {
            let radius = (blur / 2.0).round().max(1.0) as usize;
            blur_pixmap(&mut layer, radius);
        }
        // Composite (inset shadows clip to the element's rounded shape).
        let extra = if shadow.inset {
            Some(ClipShape {
                rect: transform_rect(state.transform, rect),
                radius,
            })
        } else {
            None
        };
        let paint = PixmapPaint::default();
        let mask = self.build_mask(state, viewport, extra);
        pixmap.draw_pixmap(
            bounds.x.round() as i32,
            bounds.y.round() as i32,
            layer.as_ref(),
            &paint,
            Transform::identity(),
            mask.as_ref(),
        );
    }

    fn paint_run(
        &mut self,
        pixmap: &mut Pixmap,
        run: &TextRun,
        shadows: &[rowser_parsing::cascade::TextShadowSpec],
        state: &PaintState,
        font_system: &mut FontSystem,
        viewport: (f32, f32),
    ) {
        let affine = state.transform;
        let clip = state.clip();
        // HiDPI: rasterize glyphs at font_size x scale (a NEW cache entry —
        // swash keys on the font size bits) so text is sharp at DPR > 1
        // instead of an upscaled 1x raster. Positions/placements are already
        // in device space via the affine transform.
        let dpr = if state.scale.is_finite() && state.scale > 0.0 {
            state.scale
        } else {
            1.0
        };
        let glyph_key = |glyph: &rowser_layout::text::PlacedGlyph| -> cosmic_text::CacheKey {
            if (dpr - 1.0).abs() < 1e-6 {
                return glyph.cache_key;
            }
            let mut key = glyph.cache_key;
            let size = f32::from_bits(key.font_size_bits) * dpr;
            key.font_size_bits = size.to_bits();
            key.x_bin = cosmic_text::SubpixelBin::Zero;
            key.y_bin = cosmic_text::SubpixelBin::Zero;
            key
        };
        // Shadow passes first.
        for shadow in shadows {
            for glyph in &run.glyphs {
                let (gx, gy) = apply(affine, glyph.x as f32, glyph.y as f32);
                let x = gx + shadow.x * dpr;
                let y = gy + shadow.y * dpr;
                if y < -300.0 || y > viewport.1 + 300.0 || x < -300.0 || x > viewport.0 + 300.0 {
                    continue;
                }
                let Some(image) = self.swash_cache.get_image(font_system, glyph_key(glyph)) else {
                    continue;
                };
                // Blur approximation: 4 extra taps at reduced alpha.
                let taps: [(f32, f32, u8); 5] = if shadow.blur > 0.5 {
                    [
                        (0.0, 0.0, 255),
                        (dpr, 0.0, 96),
                        (-dpr, 0.0, 96),
                        (0.0, dpr, 96),
                        (0.0, -dpr, 96),
                    ]
                } else {
                    [(0.0, 0.0, 255); 5]
                };
                for (ox, oy, alpha_scale) in taps {
                    blit_glyph(
                        pixmap,
                        glyph,
                        image,
                        (x + ox).round() as i32,
                        (y + oy).round() as i32,
                        clip,
                        Some((shadow.color.r, shadow.color.g, shadow.color.b, alpha_scale)),
                    );
                }
            }
        }
        // Main pass.
        for glyph in &run.glyphs {
            let (x, y) = apply(affine, glyph.x as f32, glyph.y as f32);
            if y < -200.0 || y > viewport.1 + 200.0 || x < -200.0 || x > viewport.0 + 200.0 {
                continue;
            }
            let Some(image) = self.swash_cache.get_image(font_system, glyph_key(glyph)) else {
                continue;
            };
            blit_glyph(
                pixmap,
                glyph,
                image,
                x.round() as i32,
                y.round() as i32,
                clip,
                None,
            );
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

    /// Builds (and caches) the clip mask for the current state. Returns
    /// None when no clips apply (fast bbox path covers rect clips).
    fn build_mask(
        &mut self,
        state: &PaintState,
        viewport: (f32, f32),
        extra: Option<ClipShape>,
    ) -> Option<tiny_skia::Mask> {
        let simple = state.clips.iter().all(|c| c.radius.is_zero())
            && extra.map(|e| e.radius.is_zero()).unwrap_or(true);
        // Only rect clips with no affine transform: masks unnecessary when
        // the fill command itself uses bbox clipping... but fill_path needs
        // SOME clip. For rect clips we still need a mask for fill_path.
        // Fast rejection: no clips at all.
        if state.clips.is_empty() && extra.is_none() {
            return None;
        }
        // Mask needed. Check the cache.
        let cache_hit = self
            .mask_cache
            .as_ref()
            .is_some_and(|c| c.key == state.clips && c.extra == extra);
        if cache_hit {
            return self.mask_cache.as_ref().unwrap().mask.clone();
        }
        // Build: rasterize each clip shape (white on transparent) and
        // min-merge the coverage buffers.
        let w = viewport.0 as u32;
        let h = viewport.1 as u32;
        let mut merged: Vec<u8> = Vec::new();
        let mut shapes: Vec<ClipShape> = state.clips.clone();
        if let Some(extra) = extra {
            shapes.push(extra);
        }
        let paint = Paint {
            anti_alias: !simple,
            shader: Shader::SolidColor(tiny_skia::Color::WHITE),
            ..Paint::default()
        };
        for shape in &shapes {
            let Some(mut pix) = Pixmap::new(w, h) else {
                continue;
            };
            let path = rounded_rect_path(shape.rect, &shape.radius);
            pix.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
            let coverage = pix.take();
            // The pixmap is RGBA premultiplied — white shape → a=255 inside;
            // use the alpha channel as coverage.
            if merged.is_empty() {
                merged = coverage.iter().skip(3).step_by(4).copied().collect();
            } else {
                for (m, c) in merged.iter_mut().zip(coverage.iter().skip(3).step_by(4)) {
                    *m = (*m).min(*c);
                }
            }
        }
        let mask = if merged.is_empty() {
            None
        } else {
            tiny_skia::Mask::from_vec(merged, tiny_skia::IntSize::from_wh(w, h)?)
        };
        self.mask_cache = Some(MaskCache {
            key: state.clips.clone(),
            extra,
            mask: mask.clone(),
        });
        mask
    }
}

impl PaintState {
    /// True when any clip shape needs a mask (rounded or transformed).
    fn has_mask_clips(&self) -> bool {
        self.clips.iter().any(|c| c.radius.is_zero().not())
    }
}

trait Not {
    fn not(self) -> bool;
}
impl Not for bool {
    fn not(self) -> bool {
        !self
    }
}

// ---------------------------------------------------------------------------
// Group matching.
// ---------------------------------------------------------------------------

/// Index of the Pop command closing the group opened at `start`.
/// Groups are strictly nested, so counting all Push/Pop pairs works.
fn matching_pop(cmds: &[DrawCmd], start: usize) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    while i < cmds.len() {
        match &cmds[i] {
            DrawCmd::PushClip { .. }
            | DrawCmd::PushOpacity { .. }
            | DrawCmd::PushTransform { .. }
            | DrawCmd::PushFixed
            | DrawCmd::PushSticky { .. }
            | DrawCmd::PushFilter { .. } => depth += 1,
            DrawCmd::PopClip | DrawCmd::PopOpacity | DrawCmd::PopTransform | DrawCmd::PopFilter => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    cmds.len()
}

// ---------------------------------------------------------------------------
// Geometry helpers.
// ---------------------------------------------------------------------------

fn matrix_of(t: Transform) -> Affine {
    [t.sx, t.ky, t.kx, t.sy, t.tx, t.ty]
}

fn transform_rect(m: Affine, rect: Rect) -> Rect {
    if is_translation(m) {
        return Rect {
            x: rect.x + m[4],
            y: rect.y + m[5],
            w: rect.w,
            h: rect.h,
        };
    }
    let (x0, y0) = apply(m, rect.x, rect.y);
    let (x1, y1) = apply(m, rect.right(), rect.y);
    let (x2, y2) = apply(m, rect.right(), rect.bottom());
    let (x3, y3) = apply(m, rect.x, rect.bottom());
    let min_x = x0.min(x1).min(x2).min(x3);
    let max_x = x0.max(x1).max(x2).max(x3);
    let min_y = y0.min(y1).min(y2).min(y3);
    let max_y = y0.max(y1).max(y2).max(y3);
    Rect {
        x: min_x,
        y: min_y,
        w: max_x - min_x,
        h: max_y - min_y,
    }
}

fn resolved(radius: rowser_parsing::cascade::RadiusLength, rect: Rect) -> f32 {
    radius.resolve(rect.w, rect.h)
}

/// Finishes a PathBuilder with a guaranteed-valid fallback path.
fn finish_path(pb: tiny_skia::PathBuilder) -> tiny_skia::Path {
    if let Some(path) = pb.finish() {
        return path;
    }
    let mut fallback = tiny_skia::PathBuilder::new();
    fallback.push_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, 1.0, 1.0).expect("unit rect"));
    fallback.finish().expect("unit path")
}

/// Builds a rounded-rect path (quadratic corner arcs).
fn rounded_rect_path(rect: Rect, radius: &BorderRadius) -> tiny_skia::Path {
    let mut pb = tiny_skia::PathBuilder::new();
    let tl = resolved(radius.top_left, rect)
        .min(rect.w / 2.0)
        .min(rect.h / 2.0);
    let tr = resolved(radius.top_right, rect)
        .min(rect.w / 2.0)
        .min(rect.h / 2.0);
    let br = resolved(radius.bottom_right, rect)
        .min(rect.w / 2.0)
        .min(rect.h / 2.0);
    let bl = resolved(radius.bottom_left, rect)
        .min(rect.w / 2.0)
        .min(rect.h / 2.0);
    let (x0, y0) = (rect.x, rect.y);
    let (x1, y1) = (rect.right(), rect.bottom());
    if tl <= 0.0 && tr <= 0.0 && br <= 0.0 && bl <= 0.0 {
        if let Some(r) = tiny_skia::Rect::from_xywh(x0, y0, rect.w.max(0.01), rect.h.max(0.01)) {
            pb.push_rect(r);
        }
        return finish_path(pb);
    }
    pb.move_to(x0 + tl, y0);
    pb.line_to(x1 - tr, y0);
    if tr > 0.0 {
        pb.quad_to(x1, y0, x1, y0 + tr);
    }
    pb.line_to(x1, y1 - br);
    if br > 0.0 {
        pb.quad_to(x1, y1, x1 - br, y1);
    }
    pb.line_to(x0 + bl, y1);
    if bl > 0.0 {
        pb.quad_to(x0, y1, x0, y1 - bl);
    }
    pb.line_to(x0, y0 + tl);
    if tl > 0.0 {
        pb.quad_to(x0, y0, x0 + tl, y0);
    }
    pb.close();
    finish_path(pb)
}

/// Border ring path: outer rounded rect + inner rounded rect (EvenOdd fill
/// produces the ring).
fn border_ring_path(
    rect: Rect,
    widths: [f32; 4],
    radius: &BorderRadius,
) -> Option<tiny_skia::Path> {
    let (t, r, b, l) = (widths[0], widths[1], widths[2], widths[3]);
    let inner = Rect {
        x: rect.x + l,
        y: rect.y + t,
        w: (rect.w - l - r).max(0.0),
        h: (rect.h - t - b).max(0.0),
    };
    if inner.w <= 0.0 || inner.h <= 0.0 {
        // Solid fill (border covers the whole box).
        return Some(rounded_rect_path(rect, radius));
    }
    let inner_radius = BorderRadius {
        top_left: shrink_radius(radius.top_left, t.max(l)),
        top_right: shrink_radius(radius.top_right, t.max(r)),
        bottom_right: shrink_radius(radius.bottom_right, b.max(r)),
        bottom_left: shrink_radius(radius.bottom_left, b.max(l)),
    };
    let outer = rounded_rect_path(rect, radius);
    let inner_path = rounded_rect_path(inner, &inner_radius);
    Some(merge_paths(outer, inner_path))
}

/// Merges two paths into one (both subpaths retained).
fn merge_paths(a: tiny_skia::Path, b: tiny_skia::Path) -> tiny_skia::Path {
    let mut pb = tiny_skia::PathBuilder::new();
    pb.push_path(&a);
    pb.push_path(&b);
    pb.finish().unwrap_or(a)
}

fn shrink_radius(
    radius: rowser_parsing::cascade::RadiusLength,
    by: f32,
) -> rowser_parsing::cascade::RadiusLength {
    rowser_parsing::cascade::RadiusLength {
        px: (radius.px - by).max(0.0),
        pct: radius.pct,
    }
}

fn grown_radius(radius: BorderRadius, by: f32) -> BorderRadius {
    BorderRadius {
        top_left: grown_one(radius.top_left, by),
        top_right: grown_one(radius.top_right, by),
        bottom_right: grown_one(radius.bottom_right, by),
        bottom_left: grown_one(radius.bottom_left, by),
    }
}

fn grown_one(
    radius: rowser_parsing::cascade::RadiusLength,
    by: f32,
) -> rowser_parsing::cascade::RadiusLength {
    rowser_parsing::cascade::RadiusLength {
        px: (radius.px + by).max(0.0),
        pct: radius.pct,
    }
}

/// Sticky offset from the current page scroll.
fn sticky_offset(info: StickyInfo, scroll_y: f32, viewport_h: f32) -> f32 {
    let StickyInfo {
        rect,
        cb_rect,
        insets,
    } = info;
    let max_stick_down = (cb_rect.bottom() - rect.h - rect.y).max(0.0);
    let max_stick_up = (rect.y - cb_rect.y).max(0.0);
    if let Some(top) = insets[0] {
        // Keep rect.y >= scroll_y + top while inside the containing block.
        let desired = (scroll_y + top) - rect.y;
        return desired.clamp(-max_stick_up, max_stick_down);
    }
    if let Some(bottom) = insets[2] {
        // Keep rect.bottom <= scroll_y + viewport_h - bottom.
        let desired = (scroll_y + viewport_h - bottom) - rect.bottom();
        return desired.clamp(-max_stick_up, max_stick_down);
    }
    0.0
}

/// Auto-distributes stop positions and converts to tiny-skia stops.
fn skia_stops(stops: &[GradientStop]) -> Vec<tiny_skia::GradientStop> {
    if stops.is_empty() {
        return Vec::new();
    }
    // Fill missing positions: even distribution between anchors.
    let mut positions: Vec<Option<f32>> = stops.iter().map(|s| s.pos).collect();
    if positions[0].is_none() {
        positions[0] = Some(0.0);
    }
    let last = positions.len() - 1;
    if positions[last].is_none() {
        positions[last] = Some(1.0);
    }
    let mut i = 0;
    while i < positions.len() {
        if positions[i].is_none() {
            let prev = positions[..i].iter().flatten().next_back().copied();
            let mut next = None;
            if let Some(p) = positions[i..].iter().flatten().next() {
                next = Some(*p);
            }
            let (a, b) = (prev.unwrap_or(0.0), next.unwrap_or(1.0));
            let mut j = i;
            let mut count = 0;
            while j < positions.len() && positions[j].is_none() {
                count += 1;
                j += 1;
            }
            for (k, pos) in positions.iter_mut().enumerate().take(j).skip(i) {
                let frac = (k - i + 1) as f32 / (count + 1) as f32;
                *pos = Some(a + (b - a) * frac);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(stops.len());
    let mut last_pos = -1.0;
    for (stop, pos) in stops.iter().zip(positions.iter()) {
        let mut p = pos.unwrap_or(1.0).clamp(0.0, 1.0);
        if p < last_pos {
            p = last_pos;
        }
        last_pos = p;
        out.push(tiny_skia::GradientStop::new(p, to_skia_color(stop.color)));
    }
    out
}

// ---------------------------------------------------------------------------
// Blur + filter kernels.
// ---------------------------------------------------------------------------

/// Box blur on a premultiplied RGBA8 pixmap (3 passes ≈ Gaussian).
pub(crate) fn blur_pixmap(pixmap: &mut Pixmap, radius: usize) {
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;
    if w == 0 || h == 0 || radius == 0 {
        return;
    }
    let mut data = pixmap.data_mut().to_vec();
    for _ in 0..3 {
        box_blur_pass(&mut data, w, h, radius, true);
        box_blur_pass(&mut data, w, h, radius, false);
    }
    pixmap.data_mut().copy_from_slice(&data);
}

/// One horizontal/vertical box blur pass (sliding window, all 4 channels).
fn box_blur_pass(data: &mut [u8], w: usize, h: usize, radius: usize, horizontal: bool) {
    let count = if horizontal { w } else { h };
    let lines = if horizontal { h } else { w };
    let stride = if horizontal { w * 4 } else { 4 };
    let step = if horizontal { 4 } else { w * 4 };
    if count == 0 {
        return;
    }
    let mut out = data.to_vec();
    for line in 0..lines {
        let base = line * stride;
        if base + (count - 1) * step + 3 >= data.len() {
            continue;
        }
        // Prime: window centered at index 0 (left edge clamped).
        let mut sums = [0u32; 4];
        for k in 0..4 {
            sums[k] = data[base + k] as u32 * (radius as u32 + 1);
        }
        for i in 1..=radius.min(count - 1) {
            for k in 0..4 {
                sums[k] += data[base + i * step + k] as u32;
            }
        }
        for i in 0..count {
            let lo = i.saturating_sub(radius);
            let hi = (i + radius).min(count - 1);
            let div = (hi - lo + 1) as u32;
            for k in 0..4 {
                out[base + i * step + k] = (((sums[k] + div / 2) / div).min(255)) as u8;
            }
            // Slide to i+1.
            if i + 1 < count {
                let add = i + 1 + radius;
                if add < count {
                    for k in 0..4 {
                        sums[k] = sums[k].saturating_add(data[base + add * step + k] as u32);
                    }
                }
                if i >= radius {
                    let rem = base + (i - radius) * step;
                    for k in 0..4 {
                        sums[k] = sums[k].saturating_sub(data[rem + k] as u32);
                    }
                }
            }
        }
    }
    data.copy_from_slice(&out);
}

/// Applies a CSS filter chain to a premultiplied pixmap.
fn apply_filters(pixmap: &mut Pixmap, filters: &[FilterSpec]) {
    let mut blur_radius = 0.0f32;
    let mut color_ops: Vec<FilterSpec> = Vec::new();
    for f in filters {
        match f {
            FilterSpec::Blur(r) => blur_radius = blur_radius.max(*r),
            other => color_ops.push(*other),
        }
    }
    if blur_radius > 0.5 {
        let r = (blur_radius / 2.0).round().max(1.0) as usize;
        blur_pixmap(pixmap, r);
    }
    if !color_ops.is_empty() {
        apply_color_filters(pixmap, &color_ops);
    }
}

/// Per-pixel color filter ops (straight-alpha math under the hood).
fn apply_color_filters(pixmap: &mut Pixmap, filters: &[FilterSpec]) {
    let mut matrix = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    let mut brightness = 1.0f32;
    let mut contrast = 1.0f32;
    let mut opacity = 1.0f32;
    for f in filters {
        match f {
            FilterSpec::Grayscale(v) => {
                let v = v.clamp(0.0, 1.0);
                let m = [
                    0.2126 + 0.7874 * (1.0 - v),
                    0.7152 - 0.7152 * (1.0 - v),
                    0.0722 - 0.0722 * (1.0 - v), //
                    0.2126 - 0.2126 * (1.0 - v),
                    0.7152 + 0.2848 * (1.0 - v),
                    0.0722 - 0.0722 * (1.0 - v), //
                    0.2126 - 0.2126 * (1.0 - v),
                    0.7152 - 0.7152 * (1.0 - v),
                    0.0722 + 0.9278 * (1.0 - v),
                ];
                matrix = mat_mul(&m, &matrix);
            }
            FilterSpec::Sepia(v) => {
                let v = v.clamp(0.0, 1.0);
                let m = [
                    0.393 + 0.607 * (1.0 - v),
                    0.769 - 0.769 * (1.0 - v),
                    0.189 - 0.189 * (1.0 - v), //
                    0.349 - 0.349 * (1.0 - v),
                    0.686 + 0.314 * (1.0 - v),
                    0.168 - 0.168 * (1.0 - v), //
                    0.272 - 0.272 * (1.0 - v),
                    0.534 - 0.534 * (1.0 - v),
                    0.131 + 0.869 * (1.0 - v),
                ];
                matrix = mat_mul(&m, &matrix);
            }
            FilterSpec::Saturate(v) => {
                let v = v.max(0.0);
                let m = [
                    0.213 + 0.787 * v,
                    0.715 - 0.715 * v,
                    0.072 - 0.072 * v, //
                    0.213 - 0.213 * v,
                    0.715 + 0.285 * v,
                    0.072 - 0.072 * v, //
                    0.213 - 0.213 * v,
                    0.715 - 0.715 * v,
                    0.072 + 0.928 * v,
                ];
                matrix = mat_mul(&m, &matrix);
            }
            FilterSpec::HueRotate(deg) => {
                let rad = deg.to_radians();
                let (s, c) = rad.sin_cos();
                let m = [
                    0.213 + c * 0.787 - s * 0.213,
                    0.715 - c * 0.715 - s * 0.715,
                    0.072 - c * 0.072 + s * 0.928, //
                    0.213 - c * 0.213 + s * 0.143,
                    0.715 + c * 0.285 + s * 0.140,
                    0.072 - c * 0.072 - s * 0.283, //
                    0.213 - c * 0.213 - s * 0.787,
                    0.715 - c * 0.715 + s * 0.715,
                    0.072 + c * 0.928 + s * 0.072,
                ];
                matrix = mat_mul(&m, &matrix);
            }
            FilterSpec::Brightness(v) => brightness *= v.max(0.0),
            FilterSpec::Contrast(v) => contrast *= *v,
            FilterSpec::Opacity(v) => opacity *= v.clamp(0.0, 1.0),
            FilterSpec::Blur(_) => {}
        }
    }
    let data = pixmap.data_mut();
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as f32 / 255.0;
        if a == 0.0 {
            continue;
        }
        // Demultiply.
        let r = px[0] as f32 / 255.0 / a;
        let g = px[1] as f32 / 255.0 / a;
        let b = px[2] as f32 / 255.0 / a;
        // Matrix.
        let mut nr = matrix[0] * r + matrix[1] * g + matrix[2] * b;
        let mut ng = matrix[3] * r + matrix[4] * g + matrix[5] * b;
        let mut nb = matrix[6] * r + matrix[7] * g + matrix[8] * b;
        // Brightness / contrast.
        nr = (nr * brightness - 0.5) * contrast + 0.5;
        ng = (ng * brightness - 0.5) * contrast + 0.5;
        nb = (nb * brightness - 0.5) * contrast + 0.5;
        let na = a * opacity;
        // Premultiply back.
        px[0] = (nr.clamp(0.0, 1.0) * na * 255.0).round() as u8;
        px[1] = (ng.clamp(0.0, 1.0) * na * 255.0).round() as u8;
        px[2] = (nb.clamp(0.0, 1.0) * na * 255.0).round() as u8;
        px[3] = (na * 255.0).round() as u8;
    }
}

fn mat_mul(a: &[f32; 9], b: &[f32; 9]) -> [f32; 9] {
    let mut out = [0.0; 9];
    for i in 0..3 {
        for j in 0..3 {
            out[i * 3 + j] = a[i * 3] * b[j] + a[i * 3 + 1] * b[3 + j] + a[i * 3 + 2] * b[6 + j];
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Legacy fast helpers.
// ---------------------------------------------------------------------------

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

fn fill_rect(pixmap: &mut Pixmap, rect: &Rect, color: Rgba) {
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

fn paint_border_edges(
    pixmap: &mut Pixmap,
    rect: &Rect,
    widths: [f32; 4],
    colors: [Rgba; 4],
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

/// Blits one swash glyph onto the pixmap with manual alpha blending.
///
/// Operates directly on the premultiplied RGBA byte buffer.
#[allow(clippy::too_many_arguments)]
fn blit_glyph(
    pixmap: &mut Pixmap,
    glyph: &rowser_layout::text::PlacedGlyph,
    image: &SwashImage,
    x: i32,
    y: i32,
    clip: Option<Rect>,
    color_override: Option<(u8, u8, u8, u8)>,
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
            let (cr, cg, cb) = match color_override {
                Some((r, g, b, _)) => (r, g, b),
                None => (glyph.color.r, glyph.color.g, glyph.color.b),
            };
            let alpha_scale = color_override.map(|c| c.3).unwrap_or(255) as u32;
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
                            let scaled = ((alpha as u32 * alpha_scale + 127) / 255).min(255) as u8;
                            blend_pixel(
                                pixmap,
                                row_y as usize * stride + px as usize * 4,
                                cr,
                                cg,
                                cb,
                                scaled,
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
