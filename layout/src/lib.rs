//! Rrowser layout: taffy box layout (block/flex/grid) with cosmic-text
//! shaping.
//!
//! The layout engine converts a styled DOM into a taffy tree:
//!
//! * `display: block/flex/grid` elements become taffy boxes with styles
//!   mapped from [`ComputedStyle`].
//! * Inline content (text nodes and `display:inline` elements) is flattened
//!   into rich text runs with per-span attributes, shaped by cosmic-text
//!   (rustybuzz + fontdb) and measured through taffy's measure callback.
//! * Output is a set of absolute rectangles per element plus positioned,
//!   rasterization-ready glyph runs.

pub mod text;

use std::collections::HashMap;

use rowser_dom::{Dom, NodeId};
use rowser_parsing::cascade::{compute_styles, ComputedStyle, DisplayMode, StyleMap};
use rowser_parsing::css::{MediaContext, ParsedStylesheet};
use taffy::geometry::{Rect as TaffyRect, Size as TaffySize};
use taffy::style::{
    AlignContent as TaffyAlignContent, AlignItems as TaffyAlignItems, AlignSelf, AvailableSpace,
    Dimension, Display as TaffyDisplay, FlexDirection as TaffyFlexDirection,
    FlexWrap as TaffyFlexWrap, JustifyContent as TaffyJustify, LengthPercentage,
    LengthPercentageAuto, Position as TaffyPosition, Style,
};
use taffy::style_helpers::TaffyAuto;
use taffy::tree::{CollapsibleMarginSet, LayoutInput, LayoutOutput, TaffyTree};
use taffy::{Baselines, NodeId as TaffyNode};

pub use text::{PlacedGlyph, SpanStyle, TextRun};

/// Page viewport.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    /// Viewport width in px.
    pub width: f32,
    /// Viewport height in px.
    pub height: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            width: 1280.0,
            height: 800.0,
        }
    }
}

/// An element rectangle in document coordinates.
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

/// Result of laying out a document.
#[derive(Debug, Default, Clone)]
pub struct LayoutResult {
    /// Element border-box rectangles in document coordinates.
    pub rects: HashMap<NodeId, Rect>,
    /// Text runs in paint order, positioned in document coordinates.
    pub text: Vec<TextRun>,
    /// Full document content size (for scrolling).
    pub content_size: (f32, f32),
}

/// One text leaf's data, owned by the taffy tree.
#[derive(Debug, Clone)]
pub struct TextLeaf {
    /// Owning (block) element.
    pub node: NodeId,
    /// Flattened text.
    pub text: String,
    /// Style spans over byte ranges.
    pub spans: Vec<(std::ops::Range<usize>, SpanStyle)>,
    /// Default (inherited) span style for the whole run.
    pub defaults: SpanStyle,
    /// Shaping cache: (width, shaped lines) from the last shape.
    pub cache: Option<(f32, std::sync::Arc<Vec<cosmic_text::LayoutLine>>)>,
}

/// The layout engine. Owns the shared font system (system fonts loaded
/// once per engine instance).
pub struct LayoutEngine {
    /// Shared cosmic-text font system.
    pub font_system: cosmic_text::FontSystem,
}

impl Default for LayoutEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutEngine {
    /// Creates the engine, loading system fonts.
    pub fn new() -> Self {
        LayoutEngine {
            font_system: cosmic_text::FontSystem::new(),
        }
    }

    /// Convenience: parse + style + layout in one call.
    pub fn layout_document(
        &mut self,
        dom: &Dom,
        author: &[ParsedStylesheet],
        media: &MediaContext,
        viewport: Viewport,
    ) -> (StyleMap, LayoutResult) {
        let styles = compute_styles(dom, author, media);
        let layout = self.compute(dom, &styles, viewport);
        (styles, layout)
    }

    /// Lays out a styled document.
    pub fn compute(&mut self, dom: &Dom, styles: &StyleMap, viewport: Viewport) -> LayoutResult {
        let root = find_layout_root(dom);
        let mut tree: TaffyTree<TextLeaf> = TaffyTree::new();
        let mut dom_to_taffy: HashMap<NodeId, TaffyNode> = HashMap::new();
        let mut taffy_to_dom: HashMap<TaffyNode, NodeId> = HashMap::new();

        let Some(root) = root else {
            return LayoutResult::default();
        };

        let taffy_root = build_box(
            dom,
            styles,
            &mut tree,
            root,
            &mut dom_to_taffy,
            &mut taffy_to_dom,
        );
        let Some(taffy_root) = taffy_root else {
            return LayoutResult::default();
        };

        // Root fills the viewport width.
        let mut root_style = taffy_style(styles.get(root).unwrap_or(&ComputedStyle::default()));
        root_style.size = TaffySize {
            width: Dimension::length(viewport.width),
            height: Dimension::AUTO,
        };
        tree.set_style(taffy_root, root_style).ok();

        let available = TaffySize {
            width: AvailableSpace::Definite(viewport.width),
            height: AvailableSpace::Definite(f32::MAX),
        };

        let font_system = &mut self.font_system;
        tree.compute_layout_with_measure(taffy_root, available, |input, _node, context, _style| {
            measure_leaf(input, context, font_system)
        })
        .ok();

        // Extract results.
        let mut result = LayoutResult::default();
        let abs = (0.0f32, 0.0f32);
        extract(
            dom,
            &tree,
            taffy_root,
            root,
            &taffy_to_dom,
            abs,
            &mut result,
            font_system,
        );
        result
    }
}

fn find_layout_root(dom: &Dom) -> Option<NodeId> {
    for node in dom.subtree_elements(dom.document()) {
        if let Some(el) = dom.element(node) {
            if &*el.name.local == "body" {
                return Some(node);
            }
        }
    }
    // No body: fall back to the first element child of the document.
    dom.subtree_elements(dom.document()).next()
}

fn taffy_style(cs: &ComputedStyle) -> Style {
    let lp = |l: rowser_parsing::cascade::LengthOrAuto| -> LengthPercentageAuto {
        use rowser_parsing::cascade::{Length, LengthOrAuto};
        match l {
            LengthOrAuto::Auto => LengthPercentageAuto::AUTO,
            LengthOrAuto::Length(Length::Px(n)) => LengthPercentageAuto::length(n),
            LengthOrAuto::Length(Length::Em(n)) => LengthPercentageAuto::length(n * cs.font_size),
            LengthOrAuto::Length(Length::Rem(n)) => {
                LengthPercentageAuto::length(n * rowser_parsing::cascade::ROOT_FONT_SIZE)
            }
            LengthOrAuto::Length(Length::Percent(n)) => LengthPercentageAuto::percent(n / 100.0),
        }
    };
    let dim = |l: rowser_parsing::cascade::LengthOrAuto| -> Dimension {
        match lp(l) {
            LengthPercentageAuto::AUTO => Dimension::AUTO,
            value => Dimension::from(value),
        }
    };
    let display = match cs.display {
        DisplayMode::Block => TaffyDisplay::Block,
        DisplayMode::Flex => TaffyDisplay::Flex,
        DisplayMode::Grid => TaffyDisplay::Grid,
        DisplayMode::Inline => TaffyDisplay::Block,
        DisplayMode::None => TaffyDisplay::None,
    };
    let border = TaffyRect {
        top: LengthPercentage::length(cs.borders.top.width),
        right: LengthPercentage::length(cs.borders.right.width),
        bottom: LengthPercentage::length(cs.borders.bottom.width),
        left: LengthPercentage::length(cs.borders.left.width),
    };
    Style {
        display,
        position: match cs.position {
            rowser_parsing::cascade::PositionMode::Absolute => TaffyPosition::Absolute,
            _ => TaffyPosition::Relative,
        },
        size: TaffySize {
            width: dim(cs.width),
            height: dim(cs.height),
        },
        min_size: TaffySize {
            width: lp(cs.min_width),
            height: lp(cs.min_height),
        },
        max_size: TaffySize {
            width: lp(cs.max_width),
            height: lp(cs.max_height),
        },
        margin: TaffyRect {
            top: lp(cs.margins.top),
            right: lp(cs.margins.right),
            bottom: lp(cs.margins.bottom),
            left: lp(cs.margins.left),
        },
        padding: TaffyRect {
            top: length_pct(cs.paddings.top, cs.font_size),
            right: length_pct(cs.paddings.right, cs.font_size),
            bottom: length_pct(cs.paddings.bottom, cs.font_size),
            left: length_pct(cs.paddings.left, cs.font_size),
        },
        border,
        flex_direction: match cs.flex_direction {
            rowser_parsing::cascade::FlexDirectionMode::Row => TaffyFlexDirection::Row,
            rowser_parsing::cascade::FlexDirectionMode::RowReverse => {
                TaffyFlexDirection::RowReverse
            }
            rowser_parsing::cascade::FlexDirectionMode::Column => TaffyFlexDirection::Column,
            rowser_parsing::cascade::FlexDirectionMode::ColumnReverse => {
                TaffyFlexDirection::ColumnReverse
            }
        },
        flex_wrap: match cs.flex_wrap {
            rowser_parsing::cascade::FlexWrapMode::NoWrap => TaffyFlexWrap::NoWrap,
            rowser_parsing::cascade::FlexWrapMode::Wrap => TaffyFlexWrap::Wrap,
            rowser_parsing::cascade::FlexWrapMode::WrapReverse => TaffyFlexWrap::WrapReverse,
        },
        flex_grow: cs.flex_grow,
        flex_shrink: cs.flex_shrink,
        flex_basis: dim(cs.flex_basis),
        justify_content: Some(match cs.justify_content {
            rowser_parsing::cascade::JustifyContentMode::Start => TaffyJustify::FLEX_START,
            rowser_parsing::cascade::JustifyContentMode::Center => TaffyJustify::CENTER,
            rowser_parsing::cascade::JustifyContentMode::End => TaffyJustify::FLEX_END,
            rowser_parsing::cascade::JustifyContentMode::SpaceBetween => {
                TaffyJustify::SPACE_BETWEEN
            }
            rowser_parsing::cascade::JustifyContentMode::SpaceAround => TaffyJustify::SPACE_AROUND,
            rowser_parsing::cascade::JustifyContentMode::SpaceEvenly => TaffyJustify::SPACE_EVENLY,
        }),
        align_items: Some(match cs.align_items {
            rowser_parsing::cascade::AlignItemsMode::Start => TaffyAlignItems::FLEX_START,
            rowser_parsing::cascade::AlignItemsMode::Center => TaffyAlignItems::CENTER,
            rowser_parsing::cascade::AlignItemsMode::End => TaffyAlignItems::FLEX_END,
            _ => TaffyAlignItems::STRETCH,
        }),
        align_content: Some(match cs.align_content {
            rowser_parsing::cascade::AlignItemsMode::Start => TaffyAlignContent::FLEX_START,
            rowser_parsing::cascade::AlignItemsMode::Center => TaffyAlignContent::CENTER,
            rowser_parsing::cascade::AlignItemsMode::End => TaffyAlignContent::FLEX_END,
            rowser_parsing::cascade::AlignItemsMode::Stretch => TaffyAlignContent::STRETCH,
            rowser_parsing::cascade::AlignItemsMode::SpaceBetween => {
                TaffyAlignContent::SPACE_BETWEEN
            }
            rowser_parsing::cascade::AlignItemsMode::SpaceAround => TaffyAlignContent::SPACE_AROUND,
            rowser_parsing::cascade::AlignItemsMode::SpaceEvenly => TaffyAlignContent::SPACE_EVENLY,
        }),
        gap: TaffySize {
            width: LengthPercentage::length(cs.gap_column),
            height: LengthPercentage::length(cs.gap_row),
        },
        ..Style::default()
    }
}

fn length_pct(l: rowser_parsing::cascade::LengthOrAuto, font_size: f32) -> LengthPercentage {
    use rowser_parsing::cascade::{Length, LengthOrAuto};
    match l {
        LengthOrAuto::Auto => LengthPercentage::length(0.0),
        LengthOrAuto::Length(Length::Px(n)) => LengthPercentage::length(n),
        LengthOrAuto::Length(Length::Em(n)) => LengthPercentage::length(n * font_size),
        LengthOrAuto::Length(Length::Rem(n)) => {
            LengthPercentage::length(n * rowser_parsing::cascade::ROOT_FONT_SIZE)
        }
        LengthOrAuto::Length(Length::Percent(n)) => LengthPercentage::percent(n / 100.0),
    }
}

/// Builds a taffy box for `node` (recursively). Returns the taffy node.
#[allow(clippy::too_many_arguments)]
fn build_box(
    dom: &Dom,
    styles: &StyleMap,
    tree: &mut TaffyTree<TextLeaf>,
    node: NodeId,
    dom_to_taffy: &mut HashMap<NodeId, TaffyNode>,
    taffy_to_dom: &mut HashMap<TaffyNode, NodeId>,
) -> Option<TaffyNode> {
    let style = styles.get(node)?;
    if style.display == DisplayMode::None {
        return None;
    }

    let mut children: Vec<TaffyNode> = Vec::new();
    let mut text = String::new();
    let mut spans: Vec<(std::ops::Range<usize>, SpanStyle)> = Vec::new();
    let defaults = SpanStyle::from_style(style);
    let ctx = defaults.clone();

    for child in dom.children(node) {
        match dom.kind(child) {
            rowser_dom::NodeKind::Text(t) => {
                append_collapsed_text(&mut text, &mut spans, t, &ctx);
            }
            rowser_dom::NodeKind::Element(_) => {
                let child_style = styles.get(child);
                let display = child_style
                    .map(|s| s.display)
                    .unwrap_or(DisplayMode::Inline);
                match display {
                    DisplayMode::Inline => {
                        // Block-in-inline: an inline element whose subtree
                        // contains block content (e.g. <center><table>,
                        // <a><div>card</div></a>) must not be flattened into
                        // text — that would drop the block boxes entirely.
                        // Promote it to a box; recursion handles nesting.
                        if has_block_descendant(dom, styles, child) {
                            if let Some(t) =
                                build_box(dom, styles, tree, child, dom_to_taffy, taffy_to_dom)
                            {
                                children.push(t);
                            }
                        } else {
                            let mut inner = ctx.clone();
                            if let Some(cs) = child_style {
                                inner.merge_from(cs);
                            }
                            collect_inline(dom, styles, child, inner, &mut text, &mut spans);
                        }
                    }
                    DisplayMode::None => {}
                    _ => {
                        if let Some(t) =
                            build_box(dom, styles, tree, child, dom_to_taffy, taffy_to_dom)
                        {
                            children.push(t);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Text leaf: a taffy leaf carrying the flattened text.
    if !text.is_empty() || children.is_empty() {
        let leaf = TextLeaf {
            node,
            text,
            spans,
            defaults,
            cache: None,
        };
        // Text leaves measure themselves; align-self start keeps baseline
        // behavior out of the way.
        let mut leaf_style = taffy_style(style);
        leaf_style.align_self = Some(AlignSelf::START);
        if let Ok(leaf_node) = tree.new_leaf_with_context(leaf_style, leaf) {
            children.push(leaf_node);
        }
    }

    let style = taffy_style(styles.get(node)?);
    let taffy_node = tree
        .new_with_children(style, &children)
        .expect("taffy node allocation");
    dom_to_taffy.insert(node, taffy_node);
    taffy_to_dom.insert(taffy_node, node);
    Some(taffy_node)
}

/// True when the element subtree (excluding the element itself) contains
/// block-level content — used to promote block-in-inline wrappers to boxes.
fn has_block_descendant(dom: &Dom, styles: &StyleMap, node: NodeId) -> bool {
    for child in dom.children(node) {
        if dom.element(child).is_some() {
            let display = styles
                .get(child)
                .map(|s| s.display)
                .unwrap_or(DisplayMode::Inline);
            match display {
                DisplayMode::Block | DisplayMode::Flex | DisplayMode::Grid => return true,
                DisplayMode::None => continue,
                DisplayMode::Inline => {
                    if has_block_descendant(dom, styles, child) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Recursively flattens inline content into the parent's text buffer.
#[allow(clippy::too_many_arguments)]
fn collect_inline(
    dom: &Dom,
    styles: &StyleMap,
    node: NodeId,
    ctx: SpanStyle,
    text: &mut String,
    spans: &mut Vec<(std::ops::Range<usize>, SpanStyle)>,
) {
    for child in dom.children(node) {
        match dom.kind(child) {
            rowser_dom::NodeKind::Text(t) => {
                append_collapsed_text(text, spans, t, &ctx);
            }
            rowser_dom::NodeKind::Element(_) => {
                let child_style = styles.get(child);
                let display = child_style
                    .map(|s| s.display)
                    .unwrap_or(DisplayMode::Inline);
                match display {
                    DisplayMode::None => {}
                    _ => {
                        let mut inner = ctx.clone();
                        if let Some(cs) = child_style {
                            inner.merge_from(cs);
                        }
                        collect_inline(dom, styles, child, inner, text, spans);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Appends text with CSS whitespace collapsing; pushes a span when the
/// style differs from the run defaults.
fn append_collapsed_text(
    text: &mut String,
    spans: &mut Vec<(std::ops::Range<usize>, SpanStyle)>,
    raw: &str,
    ctx: &SpanStyle,
) {
    if raw.is_empty() {
        return;
    }
    let needs_space = text
        .chars()
        .last()
        .map(|c| c.is_whitespace())
        .unwrap_or(false);
    let collapsed: String = if needs_space {
        raw.trim_start().chars().fold(String::new(), |mut acc, c| {
            if c.is_whitespace() {
                if acc
                    .chars()
                    .last()
                    .map(|l| !l.is_whitespace())
                    .unwrap_or(false)
                {
                    acc.push(' ');
                }
            } else {
                acc.push(c);
            }
            acc
        })
    } else {
        raw.chars().fold(String::new(), |mut acc, c| {
            if c.is_whitespace() {
                if acc
                    .chars()
                    .last()
                    .map(|l| !l.is_whitespace())
                    .unwrap_or(false)
                {
                    acc.push(' ');
                }
            } else {
                acc.push(c);
            }
            acc
        })
    };
    let collapsed = collapsed.trim_end().to_string();
    if collapsed.is_empty() {
        text.push(' ');
        return;
    }
    let start = text.len();
    text.push_str(&collapsed);
    spans.push((start..text.len(), ctx.clone()));
}

/// Taffy measure callback: shapes the leaf's text at the available width.
fn measure_leaf(
    input: LayoutInput,
    context: Option<&mut TextLeaf>,
    font_system: &mut cosmic_text::FontSystem,
) -> LayoutOutput {
    let Some(leaf) = context else {
        return LayoutOutput::HIDDEN;
    };
    let width = match input.known_dimensions.width {
        Some(w) => Some(w.max(0.0)),
        None => match input.available_space.width {
            AvailableSpace::Definite(w) => Some(w.max(0.0)),
            _ => None,
        },
    };
    let lines = text::shape(leaf, font_system, width);
    let line_h = leaf.defaults.line_height;
    let total_h = lines
        .iter()
        .map(|l| l.line_height_opt.unwrap_or(line_h))
        .sum::<f32>()
        .max(0.0);
    let total_w = lines.iter().map(|l| l.w).fold(0.0f32, f32::max).max(0.0);
    LayoutOutput {
        size: TaffySize {
            width: total_w,
            height: total_h,
        },
        scrollable_overflow_rect: taffy::geometry::Rect::ZERO,
        baselines: Baselines::NONE,
        top_margin: CollapsibleMarginSet::ZERO,
        bottom_margin: CollapsibleMarginSet::ZERO,
        margins_can_collapse_through: false,
    }
}

/// Extracts rectangles and shaped glyph runs from the computed taffy tree.
#[allow(clippy::too_many_arguments)]
fn extract(
    dom: &Dom,
    tree: &TaffyTree<TextLeaf>,
    taffy_node: TaffyNode,
    dom_node: NodeId,
    taffy_to_dom: &HashMap<TaffyNode, NodeId>,
    abs: (f32, f32),
    out: &mut LayoutResult,
    font_system: &mut cosmic_text::FontSystem,
) {
    let _ = dom;
    let Ok(layout) = tree.layout(taffy_node) else {
        return;
    };
    let node_abs = (abs.0 + layout.location.x, abs.1 + layout.location.y);
    out.rects.insert(
        dom_node,
        Rect {
            x: node_abs.0,
            y: node_abs.1,
            w: layout.size.width,
            h: layout.size.height,
        },
    );
    out.content_size.0 = out.content_size.0.max(node_abs.0 + layout.size.width);
    out.content_size.1 = out.content_size.1.max(node_abs.1 + layout.size.height);

    let children = tree.children(taffy_node).unwrap_or_default();
    for child in children {
        if let Some(leaf) = tree.get_node_context(child) {
            // Final shaping at the settled width, with absolute offsets.
            let width = tree
                .layout(child)
                .map(|l| l.size.width)
                .unwrap_or(0.0)
                .max(0.0);
            let glyphs = text::shape_at(leaf, font_system, Some(width), node_abs);
            if !glyphs.is_empty() {
                out.text.push(TextRun {
                    node: leaf.node,
                    glyphs,
                });
            }
        } else if let Some(&child_dom) = taffy_to_dom.get(&child) {
            if child_dom != dom_node {
                extract(
                    dom,
                    tree,
                    child,
                    child_dom,
                    taffy_to_dom,
                    node_abs,
                    out,
                    font_system,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rowser_parsing::css::{parse_stylesheet, MediaContext};
    use rowser_parsing::html::parse_html;

    #[test]
    fn basic_document_layout() {
        let html = br#"<html><body><p>Hello <b>bold</b> world</p><div style="height: 50px; background-color: red;"></div></body></html>"#;
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
        );
        assert!(!layout.rects.is_empty());
        // body rect spans full width
        let body = doc
            .dom
            .subtree_elements(doc.dom.document())
            .find(|n| {
                doc.dom
                    .element(*n)
                    .map(|e| &*e.name.local == "body")
                    .unwrap_or(false)
            })
            .unwrap();
        let body_rect = layout.rects.get(&body).cloned().unwrap();
        assert!(
            (body_rect.w - 800.0).abs() < 1.0,
            "body width {}",
            body_rect.w
        );
        assert!(body_rect.h > 50.0);
        // text present with glyphs
        assert!(!layout.text.is_empty(), "no text runs");
        let glyphs: usize = layout.text.iter().map(|r| r.glyphs.len()).sum();
        assert!(glyphs > 10, "only {glyphs} glyphs");
        // the fixed-height div is laid out
        let div = doc
            .dom
            .subtree_elements(doc.dom.document())
            .find(|n| doc.dom.get_attr(*n, "style").is_some())
            .unwrap();
        let div_rect = layout.rects.get(&div).cloned().unwrap();
        assert!((div_rect.h - 50.0).abs() < 2.0, "div height {}", div_rect.h);
        let _ = styles;
    }

    #[test]
    fn flexbox_layout() {
        let html = br#"<html><body><div style="display: flex; width: 600px;"><div style="flex: 1; height: 40px;"></div><div style="flex: 1; height: 40px;"></div></div></body></html>"#;
        let doc = parse_html(html);
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[],
            &MediaContext::default(),
            Viewport {
                width: 800.0,
                height: 600.0,
            },
        );
        let rects: Vec<_> = doc
            .dom
            .subtree_elements(doc.dom.document())
            .filter_map(|n| layout.rects.get(&n).cloned())
            .collect();
        // two flex children each 300px wide
        let wide: Vec<&Rect> = rects.iter().filter(|r| (r.w - 300.0).abs() < 1.0).collect();
        assert_eq!(wide.len(), 2, "flex children not sized: {rects:?}");
    }
}
