//! Display list construction: styled DOM + layout → paint commands in
//! document order.

use std::sync::Arc;

use rowser_dom::{Dom, NodeId};
use rowser_layout::LayoutResult;
use rowser_parsing::cascade::{DisplayMode, StyleMap};

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

/// Builds the display list for a laid-out document.
///
/// Paint order per element (document order): background → border → the
/// element's own text runs → child elements.
pub fn build_display_list(
    dom: &Dom,
    styles: &StyleMap,
    layout: &LayoutResult,
    images: &ImageMap,
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
    walk(dom, styles, layout, images, root, &mut list, &runs);
    list
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

#[allow(clippy::too_many_arguments)]
fn walk(
    dom: &Dom,
    styles: &StyleMap,
    layout: &LayoutResult,
    images: &ImageMap,
    node: NodeId,
    list: &mut DisplayList,
    runs: &std::collections::HashMap<NodeId, Vec<Arc<rowser_layout::text::TextRun>>>,
) {
    let Some(style) = styles.get(node) else {
        return;
    };
    if style.display == DisplayMode::None {
        return;
    }
    let Some(rect) = layout.rects.get(&node) else {
        return;
    };
    let rect = Rect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    };

    // Background.
    if style.background_color.a > 0 {
        list.commands.push(DrawCmd::Rect {
            rect,
            color: style.background_color,
        });
    }

    // Images (img elements with a decoded image).
    if let Some(image) = images.get(&node) {
        list.commands.push(DrawCmd::Image {
            rect,
            image: Arc::clone(image),
        });
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
    if let Some(element_runs) = runs.get(&node) {
        for run in element_runs {
            list.commands.push(DrawCmd::Text {
                run: Arc::clone(run),
            });
        }
    }

    // Children.
    for child in dom.children(node) {
        if dom.element(child).is_some() {
            walk(dom, styles, layout, images, child, list, runs);
        }
    }
}

/// Number of commands (used by benchmarks).
pub fn command_count(list: &DisplayList) -> usize {
    list.commands.len()
}
