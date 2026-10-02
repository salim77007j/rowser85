//! Display list construction: styled DOM + layout → paint commands in
//! document order.

use std::sync::Arc;

use rowser_dom::{Dom, NodeId};
use rowser_layout::LayoutResult;
use rowser_parsing::cascade::{ComputedStyle, DisplayMode, PositionMode, StyleMap};

use crate::{DecodedImage, Rect};

/// One paint operation.
#[derive(Debug, Clone)]
pub enum DrawCmd {
    /// Filled rectangle (background).
    Rect {
        /// Destination rectangle in document coordinates.
        rect: Rect,
        /// Fill color.
        color: rowser_parsing::cascade::Rgba,
    },
    /// A border edge.
    Border {
        /// The border rectangle (full border box).
        rect: Rect,
        /// Border widths: top, right, bottom, left.
        widths: [f32; 4],
        /// Border colors: top, right, bottom, left.
        colors: [rowser_parsing::cascade::Rgba; 4],
    },
    /// A text run (already positioned glyphs).
    Text {
        /// Glyphs.
        run: Arc<rowser_layout::text::TextRun>,
    },
    /// An image scaled into `rect`.
    Image {
        /// Destination rectangle.
        rect: Rect,
        /// Decoded image.
        image: Arc<DecodedImage>,
    },
}

/// An ordered list of paint commands.
#[derive(Debug, Default, Clone)]
pub struct DisplayList {
    /// Commands in paint order.
    pub commands: Vec<DrawCmd>,
}

/// Decoded images keyed by DOM node id, produced by the engine (network or
/// data URLs) before display list construction.
pub type ImageMap = std::collections::HashMap<NodeId, Arc<DecodedImage>>;

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
    images: &ImageMap,
    video_frames: &ImageMap,
    media: &MediaOverlays,
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
        images,
        video_frames,
        media,
        runs: &runs,
        order: 0,
        positioned: Vec::new(),
    };
    walk(dom, &mut ctx, root, &mut list);
    // Positioned layer: sorted by (z, DOM order); painted above in-flow.
    ctx.positioned.sort_by_key(|a| (a.z, a.order));
    for entry in ctx.positioned {
        list.commands.extend(entry.commands);
    }
    list
}

/// One collected positioned subtree.
struct PositionedEntry {
    z: i32,
    order: u32,
    commands: Vec<DrawCmd>,
}

/// Shared immutable inputs plus the positioned-subtree collector.
struct WalkCtx<'a> {
    styles: &'a StyleMap,
    layout: &'a LayoutResult,
    images: &'a ImageMap,
    video_frames: &'a ImageMap,
    media: &'a MediaOverlays,
    runs: &'a std::collections::HashMap<NodeId, Vec<Arc<rowser_layout::text::TextRun>>>,
    /// Monotonic DOM order counter (stacking tiebreak).
    order: u32,
    /// Positioned subtrees collected during the walk.
    positioned: Vec<PositionedEntry>,
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
/// layer: positioned elements, or anything carrying an explicit z-index.
fn is_positioned(style: &ComputedStyle) -> bool {
    style.position != PositionMode::Static || style.z_index.is_some()
}

fn walk(dom: &Dom, ctx: &mut WalkCtx<'_>, node: NodeId, list: &mut DisplayList) {
    let Some(style) = ctx.styles.get(node) else {
        return;
    };
    if style.display == DisplayMode::None {
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

    // Positioned subtrees paint into their own layer, not the in-flow list.
    if is_positioned(style) && ctx.order > 1 {
        let mut sub = DisplayList::default();
        paint_element(dom, ctx, node, style, rect, &mut sub);
        ctx.positioned.push(PositionedEntry {
            z: style.z_index.unwrap_or(0),
            order,
            commands: sub.commands,
        });
        return;
    }

    paint_element(dom, ctx, node, style, rect, list);
}

/// Paints one element (background, replaced content, border, text, then
/// children in DOM order) into `list`.
fn paint_element(
    dom: &Dom,
    ctx: &mut WalkCtx<'_>,
    node: NodeId,
    style: &ComputedStyle,
    rect: Rect,
    list: &mut DisplayList,
) {
    // Background.
    if style.background_color.a > 0 {
        list.commands.push(DrawCmd::Rect {
            rect,
            color: style.background_color,
        });
    }

    // Images (img elements with a decoded image).
    if let Some(image) = ctx.images.get(&node) {
        list.commands.push(DrawCmd::Image {
            rect,
            image: Arc::clone(image),
        });
    }

    // Media elements: blit the latest decoded video frame aspect-preserving
    // (letterboxed, like real browsers); before the first frame arrives,
    // paint the standard letterbox black.
    if let Some(element) = dom.element(node) {
        let tag = &*element.name.local;
        if tag == "video" || tag == "audio" {
            if let Some(image) = ctx.video_frames.get(&node) {
                let fitted =
                    fit_rect_aspect(rect, image.width.max(1) as f32, image.height.max(1) as f32);
                if fitted.w < rect.w || fitted.h < rect.h {
                    list.commands.push(DrawCmd::Rect {
                        rect,
                        color: rowser_parsing::cascade::Rgba::new_opaque(0, 0, 0),
                    });
                }
                list.commands.push(DrawCmd::Image {
                    rect: fitted,
                    image: Arc::clone(image),
                });
            } else if tag == "video" && rect.w > 1.0 && rect.h > 1.0 {
                list.commands.push(DrawCmd::Rect {
                    rect,
                    color: rowser_parsing::cascade::Rgba::new_opaque(0, 0, 0),
                });
            }
            // Native controls bar (controls attribute).
            if dom.get_attr(node, "controls").is_some() && rect.w > 60.0 && rect.h > 40.0 {
                draw_media_controls(list, rect, ctx.media.get(&node));
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
        });
    }

    // The element's own text.
    if let Some(element_runs) = ctx.runs.get(&node) {
        for run in element_runs {
            list.commands.push(DrawCmd::Text {
                run: Arc::clone(run),
            });
        }
    }

    // Children (flat tree: shadow content composes in at its host).
    for child in dom.flat_children(node) {
        if dom.element(child).is_some() {
            walk(dom, ctx, child, list);
        }
    }
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
        });
    }
}
