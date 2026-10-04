//! Display list construction: styled DOM + layout → paint commands in
//! document order.

use std::sync::Arc;

use rowser_dom::{Dom, NodeId};
use rowser_layout::LayoutResult;
use rowser_parsing::cascade::{
    BackgroundImageSpec, BackgroundSizeMode, BorderRadius, ComputedStyle, DisplayMode,
    PositionMode, StyleMap, TransformOp,
};

use crate::{DecodedImage, Rect};

/// One paint operation.
#[derive(Debug, Clone)]
pub enum DrawCmd {
    /// Filled rectangle (background) with optional rounded corners.
    Rect {
        /// Destination rectangle in document coordinates.
        rect: Rect,
        /// Fill color.
        color: rowser_parsing::cascade::Rgba,
        /// Corner radii.
        radius: BorderRadius,
    },
    /// Gradient fill (one background layer).
    Gradient {
        /// Destination rectangle (border box).
        rect: Rect,
        /// Corner radii.
        radius: BorderRadius,
        /// Gradient definition.
        spec: rowser_parsing::cascade::GradientSpec,
    },
    /// Background image layer (decoded raster painted into `rect`).
    BgImage {
        /// Destination rectangle for this layer.
        rect: Rect,
        /// Decoded image.
        image: Arc<DecodedImage>,
        /// Corner radii (clip shape).
        radius: BorderRadius,
    },
    /// A border edge set, optionally rounded.
    Border {
        /// The border rectangle (full border box).
        rect: Rect,
        /// Border widths: top, right, bottom, left.
        widths: [f32; 4],
        /// Border colors: top, right, bottom, left.
        colors: [rowser_parsing::cascade::Rgba; 4],
        /// Corner radii.
        radius: BorderRadius,
    },
    /// One box shadow (outset painted before backgrounds, inset after).
    BoxShadow {
        /// The element border box.
        rect: Rect,
        /// Corner radii.
        radius: BorderRadius,
        /// Shadow spec (offsets/blur/spread resolved).
        shadow: rowser_parsing::cascade::BoxShadowSpec,
    },
    /// A text run (already positioned glyphs).
    Text {
        /// Glyphs.
        run: Arc<rowser_layout::text::TextRun>,
        /// Text shadows applied to the run.
        shadows: Vec<rowser_parsing::cascade::TextShadowSpec>,
    },
    /// An image scaled into `rect`.
    Image {
        /// Destination rectangle.
        rect: Rect,
        /// Decoded image.
        image: Arc<DecodedImage>,
        /// Corner radii (rounded img).
        radius: BorderRadius,
    },
    /// A `<canvas>` element: pixels resolve LAZILY at raster time from
    /// the live canvas registry (Group E). Embedding snapshots in the list
    /// (the old path) forced a full display-list rebuild on every canvas
    /// draw op — JS-animated canvases (games, charts) repainted the whole
    /// document walk per frame. Referencing the node keeps the list
    /// stable across draws; the painter pulls fresh pixels per raster.
    Canvas {
        /// Destination rectangle (the canvas's layout box).
        rect: Rect,
        /// Corner radii.
        radius: BorderRadius,
        /// The canvas element's node id (registry key).
        node: NodeId,
    },
    /// Pushes a clip rectangle (intersected with the current clip): all
    /// commands until the matching [`DrawCmd::PopClip`] are clipped to it.
    /// Emitted for `overflow`-clipping containers. An optional per-element
    /// scroll offset translates the clipped content.
    PushClip {
        /// Clip rectangle in document coordinates.
        rect: Rect,
        /// Corner radii for the clip shape.
        radius: BorderRadius,
        /// Per-element scroll offset (content translated by -offset).
        scroll: (f32, f32),
    },
    /// Pops the most recent [`DrawCmd::PushClip`].
    PopClip,
    /// Group opacity: renders the enclosed commands into an offscreen layer
    /// and composites it at `alpha`.
    PushOpacity {
        /// Group alpha 0..1.
        alpha: f32,
    },
    /// Ends an opacity group.
    PopOpacity,
    /// Affine transform for the enclosed subtree. Matrix [a,b,c,d,e,f]
    /// in document coordinates (pre-multiplied with the current transform).
    PushTransform {
        /// Affine matrix (a, b, c, d, e, f).
        matrix: [f32; 6],
    },
    /// Ends a transform group.
    PopTransform,
    /// Page-scroll anchor (position: fixed): the enclosed subtree is
    /// translated by +scroll at paint time so it stays viewport-anchored.
    PushFixed,
    /// Sticky anchor (position: sticky): the painter computes the clamped
    /// offset from the page scroll and translates the subtree.
    PushSticky {
        /// Sticky constraints (element rect, containing block, insets).
        info: StickyInfo,
    },
    /// Filter group: renders the enclosed commands offscreen, applies the
    /// filter chain, composites. `region` bounds the effect (the element's
    /// border box inflated by any blur radius).
    PushFilter {
        /// Filter operations.
        filters: Vec<rowser_parsing::cascade::FilterSpec>,
        /// Effect region (padded border box) in document coordinates.
        region: Rect,
    },
    /// Ends a filter group.
    PopFilter,
}

/// An ordered list of paint commands.
#[derive(Debug, Default, Clone)]
pub struct DisplayList {
    /// Commands in paint order.
    pub commands: Vec<DrawCmd>,
    /// True when the list contains any page-fixed or sticky-anchored
    /// subtree. Those elements do NOT translate with the page scroll, so
    /// the painter's scroll-blit fast path must not run (it shifts prior
    /// pixels, which would drag fixed/sticky content along).
    pub has_fixed_or_sticky: bool,
    /// Monotonic list identity (built lists get distinct ids). The painter
    /// compares it against the last rendered frame to detect list changes
    /// that make a scroll blit invalid (content changed beyond scrolling).
    pub version: u64,
}

/// Display-list identity counter.
static LIST_VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Decoded images keyed by DOM node id, produced by the engine (network or
/// data URLs) before display list construction.
pub type ImageMap = std::collections::HashMap<NodeId, Arc<DecodedImage>>;

/// Decoded background-layer images per node (one entry per layer).
pub type BackgroundImageMap = std::collections::HashMap<NodeId, Vec<Option<Arc<DecodedImage>>>>;

/// Per-element scroll offsets (overflow: scroll/auto containers).
pub type ElementScrollMap = std::collections::HashMap<NodeId, (f32, f32)>;

/// Native-controls state for a media element (progress bar, play state).
#[derive(Debug, Clone, Copy, Default)]
pub struct MediaOverlay {
    /// Current presentation time.
    pub time: f64,
    /// Duration (0 = unknown/live).
    pub duration: f64,
    /// Paused.
    pub paused: bool,
    /// Muted.
    pub muted: bool,
}

/// Media overlay state keyed by node id.
pub type MediaOverlays = std::collections::HashMap<NodeId, MediaOverlay>;

/// Inputs the display list needs beyond the DOM/styles/layout.
pub struct PaintInputs<'a> {
    /// `<img>` decoded images.
    pub images: &'a ImageMap,
    /// Background-layer decoded images (per node, per layer).
    pub background_images: &'a BackgroundImageMap,
    /// Latest decoded video frames.
    pub video_frames: &'a ImageMap,
    /// Media control overlays.
    pub media: &'a MediaOverlays,
    /// Per-element scroll offsets.
    pub element_scroll: &'a ElementScrollMap,
}

static EMPTY_IMAGES: std::sync::LazyLock<ImageMap> =
    std::sync::LazyLock::new(std::collections::HashMap::new);
static EMPTY_BG_IMAGES: std::sync::LazyLock<BackgroundImageMap> =
    std::sync::LazyLock::new(std::collections::HashMap::new);
static EMPTY_OVERLAYS: std::sync::LazyLock<MediaOverlays> =
    std::sync::LazyLock::new(std::collections::HashMap::new);
static EMPTY_ELEMENT_SCROLL: std::sync::LazyLock<ElementScrollMap> =
    std::sync::LazyLock::new(std::collections::HashMap::new);

impl<'a> Default for PaintInputs<'a> {
    fn default() -> Self {
        PaintInputs {
            images: &EMPTY_IMAGES,
            background_images: &EMPTY_BG_IMAGES,
            video_frames: &EMPTY_IMAGES,
            media: &EMPTY_OVERLAYS,
            element_scroll: &EMPTY_ELEMENT_SCROLL,
        }
    }
}

/// Builds the display list for a laid-out document.
///
/// Paint order implements a simplified CSS stacking model: the in-flow
/// layer paints in document order (background → border → text → children
/// per element), then every *positioned* subtree (position ≠ static, or
/// z-index set) paints after the in-flow layer, ordered by (z-index, DOM
/// order). That is why an absolutely positioned dropdown or a relative
/// nav overlay no longer ends up UNDER later content.
pub fn build_display_list(
    dom: &Dom,
    styles: &StyleMap,
    layout: &LayoutResult,
    inputs: &PaintInputs<'_>,
) -> DisplayList {
    let mut list = DisplayList::default();
    let Some(root) = layout_root(dom) else {
        return list;
    };
    // Text runs grouped by their owning element.
    let mut runs: std::collections::HashMap<NodeId, Vec<Arc<rowser_layout::text::TextRun>>> =
        std::collections::HashMap::new();
    for run in &layout.text {
        runs.entry(run.node)
            .or_default()
            .push(Arc::new(run.clone()));
    }
    let mut ctx = WalkCtx {
        styles,
        layout,
        inputs,
        runs: &runs,
        order: 0,
        positioned: Vec::new(),
        clip: None,
        containing_rect: None,
    };
    walk(dom, &mut ctx, root, &mut list);
    // Positioned layer: sorted by (z, DOM order); painted above in-flow.
    // Each entry replays the clip chain captured where it was collected.
    ctx.positioned.sort_by_key(|a| (a.z, a.order));
    for entry in ctx.positioned {
        let mut prefix: Vec<DrawCmd> = Vec::new();
        let mut suffix: Vec<DrawCmd> = Vec::new();
        match entry.anchor {
            // Fixed: cancel the page scroll translation at paint time.
            PositionedAnchor::Fixed => {
                list.has_fixed_or_sticky = true;
                prefix.push(DrawCmd::PushFixed);
                suffix.push(DrawCmd::PopTransform);
            }
            // Sticky: the painter computes the clamped offset from scroll.
            PositionedAnchor::Sticky(info) => {
                list.has_fixed_or_sticky = true;
                prefix.push(DrawCmd::PushSticky { info });
                suffix.push(DrawCmd::PopTransform);
            }
            PositionedAnchor::Normal => {}
        }
        let mut commands = std::mem::take(&mut prefix);
        if let Some(clip) = entry.clip {
            commands.push(DrawCmd::PushClip {
                rect: clip,
                radius: BorderRadius::default(),
                scroll: (0.0, 0.0),
            });
            commands.extend(entry.commands);
            commands.push(DrawCmd::PopClip);
        } else {
            commands.extend(entry.commands);
        }
        commands.extend(suffix);
        list.commands.extend(commands);
    }
    list.version = LIST_VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    list
}

/// Sticky constraints recorded at display list build time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StickyInfo {
    /// Element border box in document coordinates.
    pub rect: Rect,
    /// Containing block (parent) border box in document coordinates.
    pub cb_rect: Rect,
    /// Resolved sticky insets (px): top/right/bottom/left (None = auto).
    pub insets: [Option<f32>; 4],
}

/// How a positioned subtree is anchored during page scrolling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PositionedAnchor {
    /// Scrolls with the page.
    Normal,
    /// Anchored to the viewport (position: fixed).
    Fixed,
    /// Scroll-clamped within the containing block (position: sticky).
    Sticky(StickyInfo),
}

/// One collected positioned subtree.
struct PositionedEntry {
    z: i32,
    order: u32,
    commands: Vec<DrawCmd>,
    /// Clip chain captured at collection time (a positioned descendant of a
    /// clipping container is clipped to it).
    clip: Option<Rect>,
    /// Scroll anchoring mode.
    anchor: PositionedAnchor,
}

/// Shared immutable inputs plus the positioned-subtree collector.
struct WalkCtx<'a> {
    styles: &'a StyleMap,
    layout: &'a LayoutResult,
    inputs: &'a PaintInputs<'a>,
    runs: &'a std::collections::HashMap<NodeId, Vec<Arc<rowser_layout::text::TextRun>>>,
    /// Monotonic DOM order counter (stacking tiebreak).
    order: u32,
    /// Positioned subtrees collected during the walk.
    positioned: Vec<PositionedEntry>,
    /// Active clip chain (document coords); None = unclipped.
    clip: Option<Rect>,
    /// Border box of the nearest ancestor element (sticky containing block).
    containing_rect: Option<Rect>,
}

/// Intersects two optional clip rectangles.
fn intersect_clip(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => {
            let x0 = a.x.max(b.x);
            let y0 = a.y.max(b.y);
            let x1 = (a.x + a.w).min(b.x + b.w);
            let y1 = (a.y + a.h).min(b.y + b.h);
            if x1 <= x0 || y1 <= y0 {
                return Some(Rect {
                    x: x0,
                    y: y0,
                    w: 0.0,
                    h: 0.0,
                });
            }
            Some(Rect {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            })
        }
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

fn layout_root(dom: &Dom) -> Option<NodeId> {
    for node in dom.subtree_elements(dom.document()) {
        if let Some(el) = dom.element(node) {
            if &*el.name.local == "body" {
                return Some(node);
            }
        }
    }
    dom.subtree_elements(dom.document()).next()
}

/// True when the element participates in the positioned (upper) paint
/// layer: positioned elements, floated elements, or anything carrying an
/// explicit z-index.
///
/// Floats paint above the backgrounds of in-flow block boxes they overlap
/// (CSS 2.1 Appendix E) — routing them through the z=0 positioned layer
/// (DOM-order tiebreak) achieves exactly that interleaving while real
/// positioned overlays (z > 0) still paint above them.
fn is_positioned(style: &ComputedStyle) -> bool {
    style.position != PositionMode::Static
        || style.z_index.is_some()
        || style.float != rowser_parsing::cascade::FloatMode::None
}

fn walk(dom: &Dom, ctx: &mut WalkCtx<'_>, node: NodeId, list: &mut DisplayList) {
    let Some(style) = ctx.styles.get(node) else {
        return;
    };
    if style.display == DisplayMode::None {
        return;
    }
    // Paint suppression: opacity 0 hides the subtree entirely.
    if style.opacity <= 0.01 {
        return;
    }
    if style.visibility == rowser_parsing::cascade::VisibilityMode::Hidden {
        // visibility: hidden hides this element's boxes but children with
        // `visibility: visible` still paint — recurse with suppression of
        // this element's own background only (approximated by recursing into
        // children without painting this element).
        for child in dom.flat_children(node) {
            if dom.element(child).is_some() {
                walk(dom, ctx, child, list);
            }
        }
        return;
    }
    // display:contents — no box of its own (no rect, no background):
    // its children paint HERE, in document order.
    if style.display == DisplayMode::Contents {
        for child in dom.flat_children(node) {
            if dom.element(child).is_some() {
                walk(dom, ctx, child, list);
            }
        }
        return;
    }
    let Some(rect) = ctx.layout.rects.get(&node) else {
        return;
    };
    let rect = Rect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    };

    let order = ctx.order;
    ctx.order += 1;

    // Clip chain: an overflow-clipping element confines its descendants to
    // its PADDING box (border box inset by border widths). Scrollable
    // elements also translate their content by the element scroll offset.
    let scrollable = style.overflow_x.scrollable() || style.overflow_y.scrollable();
    let clip_rect = if style.overflow_x.clips() || style.overflow_y.clips() {
        let b = &style.borders;
        Some(Rect {
            x: rect.x + b.left.width,
            y: rect.y + b.top.width,
            w: (rect.w - b.left.width - b.right.width).max(0.0),
            h: (rect.h - b.top.width - b.bottom.width).max(0.0),
        })
    } else {
        None
    };
    let element_scroll = if scrollable {
        ctx.inputs
            .element_scroll
            .get(&node)
            .copied()
            .unwrap_or((0.0, 0.0))
    } else {
        (0.0, 0.0)
    };
    let saved_clip = ctx.clip;
    ctx.clip = intersect_clip(saved_clip, clip_rect);
    let saved_containing = ctx.containing_rect;
    ctx.containing_rect = Some(rect);

    // Positioned subtrees paint into their own layer, not the in-flow list.
    // Group commands (transform/opacity/filter) wrap the subtree INSIDE the
    // entry so the pushes stay balanced.
    if is_positioned(style) && ctx.order > 1 {
        let mut sub = DisplayList::default();
        push_groups(&mut sub, style, rect);
        paint_element(dom, ctx, node, style, rect, &mut sub);
        pop_groups(&mut sub, style);
        let clip = ctx.clip;
        ctx.clip = saved_clip;
        ctx.containing_rect = saved_containing;
        let anchor = match style.position {
            PositionMode::Fixed => PositionedAnchor::Fixed,
            PositionMode::Sticky => {
                let insets = [
                    resolved_inset(style.top, ctx, node),
                    resolved_inset(style.right, ctx, node),
                    resolved_inset(style.bottom, ctx, node),
                    resolved_inset(style.left, ctx, node),
                ];
                PositionedAnchor::Sticky(StickyInfo {
                    rect,
                    cb_rect: saved_containing.unwrap_or(rect),
                    insets,
                })
            }
            _ => PositionedAnchor::Normal,
        };
        ctx.positioned.push(PositionedEntry {
            z: style.z_index.unwrap_or(0),
            order,
            commands: sub.commands,
            clip,
            anchor,
        });
        return;
    }

    // Non-positioned: the element paints in-flow. Group commands wrap the
    // entire contribution (clip + element + descendants).
    push_groups(list, style, rect);
    let mut scrollbar_info = None;
    if clip_rect.is_some() {
        // The element's own background and border are not clipped by its
        // overflow box (they live inside it); the DESCENDANTS are.
        list.commands.push(DrawCmd::PushClip {
            rect: ctx.clip.unwrap_or(rect),
            radius: style.border_radius,
            scroll: element_scroll,
        });
        paint_element(dom, ctx, node, style, rect, list);
        list.commands.push(DrawCmd::PopClip);
        if scrollable {
            // Content extent for the scrollbar thumb: max descendant bottom.
            let dom_ref = dom;
            let mut content_bottom = rect.y + rect.h;
            for descendant in dom_ref.descendants(node) {
                if let Some(dr) = ctx.layout.rects.get(&descendant) {
                    content_bottom = content_bottom.max(dr.y + dr.h);
                }
            }
            let b = &style.borders;
            let clip_h = (rect.h - b.top.width - b.bottom.width).max(1.0);
            scrollbar_info = Some((clip_h, content_bottom - rect.y));
        }
    } else {
        paint_element(dom, ctx, node, style, rect, list);
    }
    pop_groups(list, style);
    // Native scrollbar for scrollable containers (drawn above the content).
    if let Some((clip_h, content_h)) = scrollbar_info {
        if content_h > clip_h + 2.0 {
            let b = &style.borders;
            let clip_y = rect.y + b.top.width;
            let track_w = 10.0f32.min(rect.w * 0.25);
            let track_x = rect.right() - b.right.width - track_w;
            let scroll = element_scroll.1;
            let thumb_h = ((clip_h * clip_h / content_h).clamp(24.0, clip_h)).round();
            let scrollable_h = clip_h - thumb_h;
            let max_scroll = (content_h - clip_h).max(1.0);
            let thumb_y = clip_y + (scroll / max_scroll).clamp(0.0, 1.0) * scrollable_h;
            let track_radius = BorderRadius {
                top_left: rowser_parsing::cascade::RadiusLength {
                    px: track_w / 2.0,
                    pct: 0.0,
                },
                ..BorderRadius::default()
            };
            list.commands.push(DrawCmd::Rect {
                rect: Rect {
                    x: track_x,
                    y: clip_y,
                    w: track_w,
                    h: clip_h,
                },
                color: rowser_parsing::cascade::Rgba::new(0, 0, 0, 26),
                radius: track_radius,
            });
            list.commands.push(DrawCmd::Rect {
                rect: Rect {
                    x: track_x + 2.0,
                    y: thumb_y + 1.0,
                    w: track_w - 4.0,
                    h: (thumb_h - 2.0).max(2.0),
                },
                color: rowser_parsing::cascade::Rgba::new(0, 0, 0, 120),
                radius: track_radius,
            });
        }
    }
    ctx.clip = saved_clip;
    ctx.containing_rect = saved_containing;
}

/// Emits the group-opening commands (transform / opacity / filter) for an
/// element's paint contribution.
fn push_groups(list: &mut DisplayList, style: &ComputedStyle, rect: Rect) {
    if !style.transform.is_empty() {
        list.commands.push(DrawCmd::PushTransform {
            matrix: transform_matrix(&style.transform, style.transform_origin, rect),
        });
    }
    if (0.01..0.99).contains(&style.opacity) {
        list.commands.push(DrawCmd::PushOpacity {
            alpha: style.opacity,
        });
    }
    if !style.filters.is_empty() {
        let blur_pad = style
            .filters
            .iter()
            .filter_map(|f| match f {
                rowser_parsing::cascade::FilterSpec::Blur(r) => Some(*r),
                _ => None,
            })
            .fold(0.0f32, f32::max);
        let pad = blur_pad * 2.0 + 4.0;
        list.commands.push(DrawCmd::PushFilter {
            filters: style.filters.clone(),
            region: Rect {
                x: rect.x - pad,
                y: rect.y - pad,
                w: rect.w + pad * 2.0,
                h: rect.h + pad * 2.0,
            },
        });
    }
}

/// Emits the group-closing commands (reverse order of [`push_groups`]).
fn pop_groups(list: &mut DisplayList, style: &ComputedStyle) {
    if !style.filters.is_empty() {
        list.commands.push(DrawCmd::PopFilter);
    }
    if (0.01..0.99).contains(&style.opacity) {
        list.commands.push(DrawCmd::PopOpacity);
    }
    if !style.transform.is_empty() {
        list.commands.push(DrawCmd::PopTransform);
    }
}

/// Resolves a sticky inset to px (em resolved via the element's font size).
fn resolved_inset(
    inset: rowser_parsing::cascade::LengthOrAuto,
    ctx: &WalkCtx<'_>,
    node: NodeId,
) -> Option<f32> {
    match inset {
        rowser_parsing::cascade::LengthOrAuto::Length(l) => {
            let font = ctx.styles.get(node).map(|s| s.font_size).unwrap_or(16.0);
            match l {
                rowser_parsing::cascade::Length::Px(n) => Some(n),
                rowser_parsing::cascade::Length::Em(n) => Some(n * font),
                rowser_parsing::cascade::Length::Rem(n) => Some(n * 16.0),
                rowser_parsing::cascade::Length::Percent(_) => None,
                // Sticky calc insets: px component (approximation).
                rowser_parsing::cascade::Length::Calc { px, .. } => Some(px),
            }
        }
        rowser_parsing::cascade::LengthOrAuto::Auto => None,
    }
}

/// Computes the affine matrix for a transform op list against a rect.
pub fn transform_matrix(ops: &[TransformOp], origin: (f32, f32), rect: Rect) -> [f32; 6] {
    // Start at identity translated so the origin maps to (0,0).
    let ox = rect.x + origin.0 * rect.w;
    let oy = rect.y + origin.1 * rect.h;
    // [a b c d e f] row-major: x' = a*x + c*y + e; y' = b*x + d*y + f.
    let mut m = [1.0f32, 0.0, 0.0, 1.0, -ox, -oy];
    for op in ops {
        let step = match op {
            TransformOp::Translate { px, pct } => {
                let dx = px.0 + pct.0 * rect.w;
                let dy = px.1 + pct.1 * rect.h;
                [1.0, 0.0, 0.0, 1.0, dx, dy]
            }
            TransformOp::Rotate(rad) => {
                let (s, c) = rad.sin_cos();
                [c, s, -s, c, 0.0, 0.0]
            }
            TransformOp::Scale(sx, sy) => [*sx, 0.0, 0.0, *sy, 0.0, 0.0],
            TransformOp::Skew(ax, ay) => [1.0, ay.tan(), ax.tan(), 1.0, 0.0, 0.0],
            TransformOp::Matrix(m) => *m,
        };
        m = mul(step, m);
    }
    // Translate back to the origin point.
    m = mul([1.0, 0.0, 0.0, 1.0, ox, oy], m);
    m
}

/// Row-major 2D affine multiply: `a * b`.
fn mul(a: [f32; 6], b: [f32; 6]) -> [f32; 6] {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

/// Paints one element (shadows, background, replaced content, border,
/// text, then children in DOM order) into `list`.
fn paint_element(
    dom: &Dom,
    ctx: &mut WalkCtx<'_>,
    node: NodeId,
    style: &ComputedStyle,
    rect: Rect,
    list: &mut DisplayList,
) {
    // Outset box shadows: painted below the background (CSS 2.1 paint
    // order §E.2 step 5 vs backgrounds).
    for shadow in &style.box_shadows {
        if !shadow.inset {
            list.commands.push(DrawCmd::BoxShadow {
                rect,
                radius: style.border_radius,
                shadow: *shadow,
            });
        }
    }

    // Background color.
    if style.background_color.a > 0 {
        list.commands.push(DrawCmd::Rect {
            rect,
            color: style.background_color,
            radius: style.border_radius,
        });
    }

    // Background layers (gradients first — first layer paints on top, so
    // emit in REVERSE order).
    if !style.background_layers.is_empty() {
        let decoded = ctx.inputs.background_images.get(&node);
        for (i, layer) in style.background_layers.iter().enumerate().rev() {
            match &layer.image {
                BackgroundImageSpec::Gradient(spec) => {
                    list.commands.push(DrawCmd::Gradient {
                        rect,
                        radius: style.border_radius,
                        spec: spec.clone(),
                    });
                }
                BackgroundImageSpec::Url(url) => {
                    if url.is_empty() {
                        continue;
                    }
                    let image = decoded.and_then(|layers| {
                        layers.get(i).and_then(|img| img.as_ref().map(Arc::clone))
                    });
                    let Some(image) = image else { continue };
                    let dest = background_dest_rect(rect, layer.size, layer.position, &image);
                    list.commands.push(DrawCmd::BgImage {
                        rect: dest,
                        image,
                        radius: style.border_radius,
                    });
                }
            }
        }
    }

    // Inset box shadows: above the background, below the content.
    for shadow in &style.box_shadows {
        if shadow.inset {
            list.commands.push(DrawCmd::BoxShadow {
                rect,
                radius: style.border_radius,
                shadow: *shadow,
            });
        }
    }

    // Replaced content: <img> with a decoded image, or <canvas> whose
    // pixels resolve lazily at raster time (see DrawCmd::Canvas).
    let tag = dom.element(node).map(|e| &*e.name.local);
    if tag == Some("canvas") {
        list.commands.push(DrawCmd::Canvas {
            rect,
            radius: style.border_radius,
            node,
        });
    } else if let Some(image) = ctx.inputs.images.get(&node) {
        list.commands.push(DrawCmd::Image {
            rect,
            image: Arc::clone(image),
            radius: style.border_radius,
        });
    }

    // Media elements: blit the latest decoded video frame aspect-preserving
    // (letterboxed, like real browsers); before the first frame arrives,
    // paint the standard letterbox black.
    if let Some(element) = dom.element(node) {
        let tag = &*element.name.local;
        if tag == "video" || tag == "audio" {
            if let Some(image) = ctx.inputs.video_frames.get(&node) {
                let fitted =
                    fit_rect_aspect(rect, image.width.max(1) as f32, image.height.max(1) as f32);
                if fitted.w < rect.w || fitted.h < rect.h {
                    list.commands.push(DrawCmd::Rect {
                        rect,
                        color: rowser_parsing::cascade::Rgba::new_opaque(0, 0, 0),
                        radius: style.border_radius,
                    });
                }
                list.commands.push(DrawCmd::Image {
                    rect: fitted,
                    image: Arc::clone(image),
                    radius: BorderRadius::default(),
                });
            } else if tag == "video" && rect.w > 1.0 && rect.h > 1.0 {
                list.commands.push(DrawCmd::Rect {
                    rect,
                    color: rowser_parsing::cascade::Rgba::new_opaque(0, 0, 0),
                    radius: style.border_radius,
                });
            }
            // Native controls bar (controls attribute).
            if dom.get_attr(node, "controls").is_some() && rect.w > 60.0 && rect.h > 40.0 {
                draw_media_controls(list, rect, ctx.inputs.media.get(&node));
            }
        }
    }

    // Borders.
    let b = &style.borders;
    let has_border =
        b.top.width > 0.0 || b.right.width > 0.0 || b.bottom.width > 0.0 || b.left.width > 0.0;
    if has_border {
        list.commands.push(DrawCmd::Border {
            rect,
            widths: [b.top.width, b.right.width, b.bottom.width, b.left.width],
            colors: [b.top.color, b.right.color, b.bottom.color, b.left.color],
            radius: style.border_radius,
        });
    }

    // The element's own text.
    if let Some(element_runs) = ctx.runs.get(&node) {
        for run in element_runs {
            list.commands.push(DrawCmd::Text {
                run: Arc::clone(run),
                shadows: style.text_shadows.clone(),
            });
        }
    }

    // ::before box (block-level pseudo box): painted before the children.
    if let Some((before_id, _)) = ctx.layout.pseudo_ids.get(&node) {
        if *before_id != 0 {
            if let Some(pseudo_style) = ctx.styles.pseudo_before.get(&node) {
                paint_pseudo_box(ctx, *before_id, pseudo_style, rect, list);
            }
        }
    }

    // Children (flat tree: shadow content composes in at its host).
    for child in dom.flat_children(node) {
        if dom.element(child).is_some() {
            walk(dom, ctx, child, list);
        }
    }

    // ::after box: painted after the children.
    if let Some((_, after_id)) = ctx.layout.pseudo_ids.get(&node) {
        if *after_id != 0 {
            if let Some(pseudo_style) = ctx.styles.pseudo_after.get(&node) {
                paint_pseudo_box(ctx, *after_id, pseudo_style, rect, list);
            }
        }
    }
}

/// Paints one pseudo-element box: background, border, text runs — using the
/// synthetic id's layout rect and text runs.
fn paint_pseudo_box(
    ctx: &mut WalkCtx<'_>,
    pseudo_id: NodeId,
    style: &ComputedStyle,
    owner_rect: Rect,
    list: &mut DisplayList,
) {
    let Some(rect) = ctx.layout.rects.get(&pseudo_id) else {
        return;
    };
    let rect = Rect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    };
    let _ = owner_rect;
    if style.background_color.a > 0 {
        list.commands.push(DrawCmd::Rect {
            rect,
            color: style.background_color,
            radius: style.border_radius,
        });
    }
    if !style.background_layers.is_empty() {
        for layer in style.background_layers.iter().rev() {
            if let BackgroundImageSpec::Gradient(spec) = &layer.image {
                list.commands.push(DrawCmd::Gradient {
                    rect,
                    radius: style.border_radius,
                    spec: spec.clone(),
                });
            }
        }
    }
    let b = &style.borders;
    let has_border =
        b.top.width > 0.0 || b.right.width > 0.0 || b.bottom.width > 0.0 || b.left.width > 0.0;
    if has_border {
        list.commands.push(DrawCmd::Border {
            rect,
            widths: [b.top.width, b.right.width, b.bottom.width, b.left.width],
            colors: [b.top.color, b.right.color, b.bottom.color, b.left.color],
            radius: style.border_radius,
        });
    }
    if let Some(runs) = ctx.runs.get(&pseudo_id) {
        for run in runs {
            list.commands.push(DrawCmd::Text {
                run: Arc::clone(run),
                shadows: style.text_shadows.clone(),
            });
        }
    }
}

/// Destination rect for one background image layer.
fn background_dest_rect(
    box_rect: Rect,
    size: BackgroundSizeMode,
    position: (f32, f32),
    image: &DecodedImage,
) -> Rect {
    let iw = image.width.max(1) as f32;
    let ih = image.height.max(1) as f32;
    let (w, h) = match size {
        BackgroundSizeMode::Explicit(w, h) => (
            if w > 0.0 { w } else { box_rect.w },
            if h > 0.0 { h } else { box_rect.w * ih / iw },
        ),
        BackgroundSizeMode::Cover => {
            let scale = (box_rect.w / iw).max(box_rect.h / ih);
            (iw * scale, ih * scale)
        }
        BackgroundSizeMode::Contain => {
            let scale = (box_rect.w / iw).min(box_rect.h / ih);
            (iw * scale, ih * scale)
        }
        BackgroundSizeMode::Auto => (iw, ih),
    };
    let x = box_rect.x + (box_rect.w - w) * position.0;
    let y = box_rect.y + (box_rect.h - h) * position.1;
    Rect { x, y, w, h }
}

/// Number of commands (used by benchmarks).
pub fn command_count(list: &DisplayList) -> usize {
    list.commands.len()
}

/// Largest sub-rect of `rect` with the given aspect ratio, centered
/// (the `object-fit: contain` behaviour real browsers apply to video).
fn fit_rect_aspect(rect: Rect, w: f32, h: f32) -> Rect {
    let aspect = w / h.max(1.0);
    let box_aspect = rect.w / rect.h.max(1.0);
    if aspect <= 0.0 || box_aspect <= 0.0 || (aspect - box_aspect).abs() < 0.001 {
        return rect;
    }
    if aspect > box_aspect {
        let height = rect.w / aspect;
        Rect {
            x: rect.x,
            y: rect.y + (rect.h - height) / 2.0,
            w: rect.w,
            h: height,
        }
    } else {
        let width = rect.h * aspect;
        Rect {
            x: rect.x + (rect.w - width) / 2.0,
            y: rect.y,
            w: width,
            h: rect.h,
        }
    }
}

/// Draws the native media control bar: translucent strip, progress fill,
/// play/pause and mute glyphs (blocky painter geometry — deliberately part
/// of the display list so page rendering stays single-pass).
fn draw_media_controls(list: &mut DisplayList, rect: Rect, overlay: Option<&MediaOverlay>) {
    let bar_h = (rect.h * 0.16).clamp(26.0, 40.0);
    let bar = Rect {
        x: rect.x,
        y: rect.y + rect.h - bar_h,
        w: rect.w,
        h: bar_h,
    };
    list.commands.push(DrawCmd::Rect {
        rect: bar,
        color: rowser_parsing::cascade::Rgba::new(12, 12, 12, 178),
        radius: BorderRadius::default(),
    });
    let (time, duration, paused, muted) = match overlay {
        Some(overlay) => (
            overlay.time,
            overlay.duration,
            overlay.paused,
            overlay.muted,
        ),
        None => (0.0, 0.0, true, false),
    };
    let cy = bar.y + bar_h / 2.0;
    let white = rowser_parsing::cascade::Rgba::new(240, 240, 240, 230);
    // Play / pause glyph at the left.
    if paused {
        // Blocky play triangle: three shrinking bars.
        for (i, width) in [10.0_f32, 7.0, 4.0].iter().enumerate() {
            let i = i as f32;
            list.commands.push(DrawCmd::Rect {
                rect: Rect {
                    x: bar.x + 18.0 + i * 4.0,
                    y: cy - 9.0 + i * 2.5,
                    w: *width,
                    h: 18.0 - i * 5.0,
                },
                color: white,
                radius: BorderRadius::default(),
            });
        }
    } else {
        for i in 0..2u8 {
            list.commands.push(DrawCmd::Rect {
                rect: Rect {
                    x: bar.x + 19.0 + f32::from(i) * 7.0,
                    y: cy - 8.0,
                    w: 5.0,
                    h: 16.0,
                },
                color: white,
                radius: BorderRadius::default(),
            });
        }
    }
    // Progress track + fill.
    let track_x = bar.x + 44.0;
    let track_w = (bar.w - 88.0).max(1.0);
    list.commands.push(DrawCmd::Rect {
        rect: Rect {
            x: track_x,
            y: cy - 2.0,
            w: track_w,
            h: 4.0,
        },
        color: rowser_parsing::cascade::Rgba::new(255, 255, 255, 96),
        radius: BorderRadius::default(),
    });
    if duration > 0.0 && duration.is_finite() {
        let frac = (time / duration).clamp(0.0, 1.0) as f32;
        let fill_w = (track_w * frac).max(2.0);
        list.commands.push(DrawCmd::Rect {
            rect: Rect {
                x: track_x,
                y: cy - 3.0,
                w: fill_w,
                h: 6.0,
            },
            color: rowser_parsing::cascade::Rgba::new(235, 235, 235, 235),
            radius: BorderRadius::default(),
        });
    }
    // Mute glyph at the right: a speaker square; muted = hollow center.
    let mx = bar.x + bar.w - 30.0;
    list.commands.push(DrawCmd::Rect {
        rect: Rect {
            x: mx,
            y: cy - 6.0,
            w: 12.0,
            h: 12.0,
        },
        color: white,
        radius: BorderRadius::default(),
    });
    if muted {
        list.commands.push(DrawCmd::Rect {
            rect: Rect {
                x: mx + 3.0,
                y: cy - 3.0,
                w: 6.0,
                h: 6.0,
            },
            color: rowser_parsing::cascade::Rgba::new(12, 12, 12, 220),
            radius: BorderRadius::default(),
        });
    }
}
