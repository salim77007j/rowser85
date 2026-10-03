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
use taffy::geometry::Line;
use taffy::geometry::{Rect as TaffyRect, Size as TaffySize};
use taffy::style::{
    AlignContent as TaffyAlignContent, AlignItems as TaffyAlignItems, AlignSelf, AvailableSpace,
    Dimension, Display as TaffyDisplay, FlexDirection as TaffyFlexDirection,
    FlexWrap as TaffyFlexWrap, JustifyContent as TaffyJustify, LengthPercentage,
    LengthPercentageAuto, Overflow as TaffyOverflow, Position as TaffyPosition, Style,
};
use taffy::style::{
    Clear as TaffyClear, Float as TaffyFloat, GridPlacement, GridTemplateArea,
    GridTemplateAreas, GridTemplateComponent, MaxTrackSizingFunction, MinTrackSizingFunction,
};
use taffy::style_helpers::TaffyAuto;
use taffy::style_helpers::{
    auto as track_auto, fr, length as track_length, max_content, min_content, minmax,
    percent as track_percent, TaffyGridLine,
};
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
        intrinsic: &HashMap<NodeId, (f32, f32)>,
    ) -> (StyleMap, LayoutResult) {
        let styles = compute_styles(dom, author, media);
        let layout = self.compute(dom, &styles, viewport, intrinsic);
        (styles, layout)
    }

    /// Lays out a styled document. `intrinsic` carries replaced-element
    /// natural sizes (video dimensions once decoded) for aspect-correct
    /// sizing when CSS leaves width/height auto.
    pub fn compute(
        &mut self,
        dom: &Dom,
        styles: &StyleMap,
        viewport: Viewport,
        intrinsic: &HashMap<NodeId, (f32, f32)>,
    ) -> LayoutResult {
        let root = find_layout_root(dom);
        let mut tree: TaffyTree<TextLeaf> = TaffyTree::new();
        let mut dom_to_taffy: HashMap<NodeId, TaffyNode> = HashMap::new();
        let mut taffy_to_dom: HashMap<TaffyNode, NodeId> = HashMap::new();

        let Some(root) = root else {
            return LayoutResult::default();
        };

        // Table grid structure (colspan/rowspan placements) for every
        // table in the document — computed once, consumed by build_box as
        // cells and tables get their taffy grid styles.
        let tables = TableGrids::collect(dom, root);

        let taffy_root = build_box(
            dom,
            styles,
            &mut tree,
            root,
            &mut dom_to_taffy,
            &mut taffy_to_dom,
            intrinsic,
            &[],
            &tables,
        );
        let Some(taffy_root) = taffy_root else {
            return LayoutResult::default();
        };

        // Root margins (resolved): the extract walk seeds at the margin edge
        // (taffy positions the root border box at the layout origin, not at
        // its margin edge), and the root WIDTH is set definitively below.
        let root_margin = styles
            .get(root)
            .map(|cs| {
                let m = &cs.margins;
                let px = |v: &rowser_parsing::cascade::LengthOrAuto| match v {
                    rowser_parsing::cascade::LengthOrAuto::Length(l) => {
                        l.resolve(cs.font_size)
                    }
                    rowser_parsing::cascade::LengthOrAuto::Auto => 0.0,
                };
                (px(&m.left), px(&m.top), px(&m.right), px(&m.bottom))
            })
            .unwrap_or((0.0, 0.0, 0.0, 0.0));

        // Root sizing: a DEFINITE width (viewport minus the root's own
        // margins) so that percent-width children resolve against it —
        // with an auto width, taffy's intrinsic sizing pass measures the
        // subtree under MaxContent, where percent widths degenerate to
        // content size (a `width:100%` div rendered 8px wide). The old
        // code forced viewport.width and ignored body margins entirely
        // (content at x=0); this keeps the margin semantics while staying
        // definite. An explicit author width (px/em) wins.
        let mut root_style = taffy_style(styles.get(root).unwrap_or(&ComputedStyle::default()));
        let root_has_author_width = styles.get(root).is_some_and(|cs| {
            !matches!(cs.width, rowser_parsing::cascade::LengthOrAuto::Auto)
        });
        if !root_has_author_width {
            root_style.size = TaffySize {
                width: Dimension::length(
                    (viewport.width - root_margin.0 - root_margin.2).max(0.0),
                ),
                height: Dimension::AUTO,
            };
        }
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

        // Extract results. The root (body) border box sits at its margins:
        // taffy reports root-relative locations, so seed the walk with the
        // margin offset (mirrors CSS: the body content box is inset by its
        // margins within the html canvas).
        let mut result = LayoutResult::default();
        let abs = (root_margin.0, root_margin.1);
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

/// One cell's placement in a table grid, computed by the CSS 2.1 §17.4.1
/// occupancy algorithm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TableCellPlacement {
    /// Zero-based row of the cell's top-left corner.
    pub row: u16,
    /// Zero-based column of the cell's top-left corner.
    pub col: u16,
    /// Row span (>= 1).
    pub rowspan: u16,
    /// Column span (>= 1).
    pub colspan: u16,
}

/// Precomputed grid structure for every `<table>` under the layout root:
/// cell placements (with colspan/rowspan) and per-table column counts.
/// The UA stylesheet maps `table` to `display: grid` and rows/sections to
/// `display: contents`, so these placements drive the grid engine's
/// track-spanning item placement — that is how colspan/rowspan render.
#[derive(Debug, Default)]
pub struct TableGrids {
    cells: HashMap<NodeId, TableCellPlacement>,
    cols: HashMap<NodeId, usize>,
}

impl TableGrids {
    /// Walks every `<table>` under `root` (light tree) and computes the
    /// occupancy grid per table.
    pub fn collect(dom: &Dom, root: NodeId) -> Self {
        let mut grids = TableGrids::default();
        for node in dom.descendants(root) {
            if dom
                .element(node)
                .is_some_and(|e| &*e.name.local == "table")
            {
                let (cells, cols) = table_grid(dom, node);
                grids.cells.extend(cells);
                grids.cols.insert(node, cols);
            }
        }
        grids
    }
}

/// Computes one table's occupancy grid (CSS 2.1 §17.4.1, simplified):
/// cells are placed left-to-right in each row into the first slot not
/// occupied by a previous (possibly row-spanning) cell; `colspan`/
/// `rowspan` attributes drive the span. A `<caption>` occupies row 1
/// spanning every column (Chrome renders it above the table body).
fn table_grid(dom: &Dom, table: NodeId) -> (HashMap<NodeId, TableCellPlacement>, usize) {
    let mut cells: HashMap<NodeId, TableCellPlacement> = HashMap::new();
    let mut occupied: std::collections::HashSet<(u16, u16)> = std::collections::HashSet::new();
    let mut max_col: usize = 0;

    // Rows: direct <tr> children plus <tr> children of section groups
    // (tbody/thead/tfoot). Nested tables live inside cells, never at row
    // depth, so the walk does not recurse.
    let mut rows: Vec<NodeId> = Vec::new();
    let mut caption: Option<NodeId> = None;
    for child in dom.flat_children(table) {
        if let Some(el) = dom.element(child) {
            match &*el.name.local {
                "tr" => rows.push(child),
                "tbody" | "thead" | "tfoot" => {
                    for sub in dom.flat_children(child) {
                        if dom
                            .element(sub)
                            .is_some_and(|e| &*e.name.local == "tr")
                        {
                            rows.push(sub);
                        }
                    }
                }
                "caption" => caption = Some(child),
                _ => {}
            }
        }
    }
    let row_offset: u16 = if caption.is_some() { 1 } else { 0 };

    let span_attr = |node: NodeId, name: &str| -> u16 {
        dom.get_attr(node, name)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .map(|v| v.clamp(1, 100) as u16)
            .unwrap_or(1)
    };

    for (row_index, tr) in rows.iter().enumerate() {
        let row: u16 = row_index as u16;
        let mut col: u16 = 0;
        for cell in dom.flat_children(*tr) {
            let is_cell = dom
                .element(cell)
                .is_some_and(|e| matches!(&*e.name.local, "td" | "th"));
            if !is_cell {
                continue;
            }
            let colspan = span_attr(cell, "colspan");
            let rowspan = span_attr(cell, "rowspan");
            // First slot (from the row cursor) where the whole span fits.
            while col < 10_000 {
                let fits = (row..row + rowspan).all(|r| {
                    (col..col + colspan).all(|c| !occupied.contains(&(r, c)))
                });
                if fits {
                    break;
                }
                col += 1;
            }
            if col >= 10_000 {
                break;
            }
            cells.insert(
                cell,
                TableCellPlacement {
                    row: row + row_offset,
                    col,
                    rowspan,
                    colspan,
                },
            );
            for r in row..row + rowspan {
                for c in col..col + colspan {
                    occupied.insert((r, c));
                }
            }
            max_col = max_col.max((col + colspan) as usize);
            col += colspan;
        }
    }

    if let Some(caption) = caption {
        cells.insert(
            caption,
            TableCellPlacement {
                row: 0,
                col: 0,
                rowspan: 1,
                colspan: max_col.max(1) as u16,
            },
        );
    }
    (cells, max_col)
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
            LengthOrAuto::Length(Length::Percent(n)) => LengthPercentageAuto::percent(n),
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
        // Contents elements never reach taffy (no box); defensive fallback.
        DisplayMode::Contents => TaffyDisplay::Block,
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
        // Floats (CSS 2.1 §9.5): taffy's `float_layout` block algorithm
        // places floated boxes against the containing block edges and
        // shortens the space available to BFC-establishing content.
        float: match cs.float {
            rowser_parsing::cascade::FloatMode::Left => TaffyFloat::Left,
            rowser_parsing::cascade::FloatMode::Right => TaffyFloat::Right,
            rowser_parsing::cascade::FloatMode::None => TaffyFloat::None,
        },
        clear: match cs.clear {
            rowser_parsing::cascade::ClearMode::Left => TaffyClear::Left,
            rowser_parsing::cascade::ClearMode::Right => TaffyClear::Right,
            rowser_parsing::cascade::ClearMode::Both => TaffyClear::Both,
            rowser_parsing::cascade::ClearMode::None => TaffyClear::None,
        },
        // overflow: layout side effects only (BFC root for hidden/scroll —
        // floats stop propagating; flex/grid auto-min-size becomes 0).
        // Painting-side clipping is handled by the display-list clip stack.
        overflow: taffy::geometry::Point {
            x: if cs.overflow_x.clips() {
                TaffyOverflow::Hidden
            } else {
                TaffyOverflow::Visible
            },
            y: if cs.overflow_y.clips() {
                TaffyOverflow::Hidden
            } else {
                TaffyOverflow::Visible
            },
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
        // Inset (top/right/bottom/left): anchors for position:absolute,
        // offsets for position:relative. Previously dropped — absolutely
        // positioned navigation stacked at the containing block origin.
        inset: TaffyRect {
            top: lp(cs.top),
            right: lp(cs.right),
            bottom: lp(cs.bottom),
            left: lp(cs.left),
        },
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
        // align-content — ONLY on flex/grid containers. css-align-3 §5.1.1:
        // a non-`normal` align-content makes a BLOCK container establish an
        // independent formatting context. Emitting Some(..) on every block
        // made every block its own BFC: float contexts stopped propagating
        // into nested blocks (no line narrowing beside floats) and margin
        // collapsing broke (divergent spacing vs Chrome). Flex/grid are
        // formatting-context roots anyway, so mapping there is safe.
        align_content: match cs.display {
            DisplayMode::Flex | DisplayMode::Grid => Some(match cs.align_content {
                rowser_parsing::cascade::AlignItemsMode::Start => TaffyAlignContent::FLEX_START,
                rowser_parsing::cascade::AlignItemsMode::Center => TaffyAlignContent::CENTER,
                rowser_parsing::cascade::AlignItemsMode::End => TaffyAlignContent::FLEX_END,
                rowser_parsing::cascade::AlignItemsMode::Stretch => TaffyAlignContent::STRETCH,
                rowser_parsing::cascade::AlignItemsMode::SpaceBetween => {
                    TaffyAlignContent::SPACE_BETWEEN
                }
                rowser_parsing::cascade::AlignItemsMode::SpaceAround => {
                    TaffyAlignContent::SPACE_AROUND
                }
                rowser_parsing::cascade::AlignItemsMode::SpaceEvenly => {
                    TaffyAlignContent::SPACE_EVENLY
                }
            }),
            _ => None,
        },
        gap: TaffySize {
            width: LengthPercentage::length(cs.gap_column),
            height: LengthPercentage::length(cs.gap_row),
        },
        // CSS grid templates: track sizing (px, %, fr, min/max-content).
        // CSS: a grid with NO explicit column template stacks items in ONE
        // auto column (each child in its own row) — that's what Chrome does
        // for `dl{display:grid}` sidebars. Taffy instead spreads children
        // across implicit columns in a single row, colliding them (MDN's
        // dt/dl terms overlapped at the same y). Force a single auto track
        // when the author specified none.
        grid_template_columns: {
            let mut tracks: Vec<GridTemplateComponent<String>> = cs
                .grid_template_columns
                .iter()
                .map(track_sizing_fn)
                .collect();
            if tracks.is_empty() {
                tracks.push(GridTemplateComponent::Single(minmax(
                    track_auto(),
                    track_auto(),
                )));
            }
            tracks
        },
        grid_template_rows: cs.grid_template_rows.iter().map(track_sizing_fn).collect(),
        // Line/span item placement (`grid-column: 1 / 3`, `grid-row: span 2`)
        // and implicit track sizing.
        grid_row: grid_line_pair(cs.grid_row),
        grid_column: grid_line_pair(cs.grid_column),
        grid_auto_rows: cs.grid_auto_rows.iter().map(track_auto_sizing_fn).collect(),
        grid_auto_columns: cs.grid_auto_columns.iter().map(track_auto_sizing_fn).collect(),
        ..Style::default()
    }
}

/// Maps one cascade track (min, max) pair onto a taffy grid template
/// component. Every track is a minmax pair; lonely bounds pair with auto.
fn track_sizing_fn(track: &rowser_parsing::cascade::TrackRaw) -> GridTemplateComponent<String> {
    use rowser_parsing::cascade::TrackBoundRaw as B;
    let min_bound = |b: &B| -> MinTrackSizingFunction {
        match b {
            B::Auto => track_auto(),
            B::MinContent => min_content(),
            B::MaxContent => max_content(),
            B::Px(v) => track_length(*v),
            B::Percent(p) => track_percent(*p),
            // fr is invalid as a minimum in CSS: degrade to auto.
            B::Fr(_) => track_auto(),
        }
    };
    let max_bound = |b: &B| -> MaxTrackSizingFunction {
        match b {
            B::Auto => track_auto(),
            B::MinContent => min_content(),
            B::MaxContent => max_content(),
            B::Px(v) => track_length(*v),
            B::Percent(p) => track_percent(*p),
            B::Fr(f) => fr(*f),
        }
    };
    GridTemplateComponent::Single(minmax(min_bound(&track.min), max_bound(&track.max)))
}

/// Maps one raw grid placement side onto a taffy GridPlacement.
fn grid_line_placement(side: rowser_parsing::cascade::GridLineRaw) -> GridPlacement {
    use rowser_parsing::cascade::GridLineRaw;
    match side {
        GridLineRaw::Auto => GridPlacement::Auto,
        GridLineRaw::Line(n) => GridPlacement::from_line_index(n),
        GridLineRaw::Span(n) => GridPlacement::Span(n.max(1)),
    }
}

/// Maps a (start, end) placement pair onto a taffy Line<GridPlacement>.
fn grid_line_pair(
    placement: rowser_parsing::cascade::GridPlacementRaw,
) -> Line<GridPlacement> {
    Line {
        start: grid_line_placement(placement.start),
        end: grid_line_placement(placement.end),
    }
}

/// Maps one cascade track onto a taffy implicit-track sizing function
/// (grid-auto-rows/columns use TrackSizingFunction, not the template
/// component type).
fn track_auto_sizing_fn(
    track: &rowser_parsing::cascade::TrackRaw,
) -> taffy::style::TrackSizingFunction {
    use rowser_parsing::cascade::TrackBoundRaw as B;
    let min_bound = |b: &B| -> MinTrackSizingFunction {
        match b {
            B::Auto => track_auto(),
            B::MinContent => min_content(),
            B::MaxContent => max_content(),
            B::Px(v) => track_length(*v),
            B::Percent(p) => track_percent(*p),
            B::Fr(_) => track_auto(),
        }
    };
    let max_bound = |b: &B| -> MaxTrackSizingFunction {
        match b {
            B::Auto => track_auto(),
            B::MinContent => min_content(),
            B::MaxContent => max_content(),
            B::Px(v) => track_length(*v),
            B::Percent(p) => track_percent(*p),
            B::Fr(f) => fr(*f),
        }
    };
    minmax(min_bound(&track.min), max_bound(&track.max))
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
        LengthOrAuto::Length(Length::Percent(n)) => LengthPercentage::percent(n),
    }
}

/// Builds a taffy box for `node` (recursively). Returns the taffy node.
/// `parent_areas` carries the parent grid's named areas for
/// `grid-area: name` placement of this node.
#[allow(clippy::too_many_arguments)]
fn build_box(
    dom: &Dom,
    styles: &StyleMap,
    tree: &mut TaffyTree<TextLeaf>,
    node: NodeId,
    dom_to_taffy: &mut HashMap<NodeId, TaffyNode>,
    taffy_to_dom: &mut HashMap<TaffyNode, NodeId>,
    intrinsic: &HashMap<NodeId, (f32, f32)>,
    parent_areas: &[rowser_parsing::cascade::NamedAreaRaw],
    tables: &TableGrids,
) -> Option<TaffyNode> {
    let style = styles.get(node)?;
    if style.display == DisplayMode::None {
        return None;
    }
    // This node's named grid areas — children placed with `grid-area:`.
    let own_areas = style.grid_template_areas.clone();

    let mut children: Vec<TaffyNode> = Vec::new();
    let mut text = String::new();
    let mut spans: Vec<(std::ops::Range<usize>, SpanStyle)> = Vec::new();
    let defaults = SpanStyle::from_style(style);
    let ctx = defaults.clone();

    collect_children(
        dom,
        styles,
        tree,
        node,
        dom_to_taffy,
        taffy_to_dom,
        intrinsic,
        &own_areas,
        tables,
        &mut children,
        &mut text,
        &mut spans,
        &ctx,
    );

    // Text leaf: a taffy leaf carrying the flattened text. Every element
    // gets one (empty text for childless boxes): a childless taffy node
    // without context is measured as HIDDEN (0x0) — the leaf is what
    // keeps the box in the block layout path.
    let force_leaf = children.is_empty();
    flush_text_leaf(
        tree,
        node,
        &mut text,
        &mut spans,
        &defaults,
        &mut children,
        force_leaf,
    );

    let mut style = taffy_style(styles.get(node)?);
    // Table grid mapping (CSS 2.1 §17): the UA stylesheet turns <table>
    // into display:grid and rows/sections into display:contents, so the
    // precomputed cell placements (colspan/rowspan included) become
    // explicit line placements on each cell. The table gets one track
    // per column — auto tracks (content-sized, like auto table layout)
    // or minmax(auto, 1fr) when the width is definite so extra space
    // distributes across columns like Chrome's auto algorithm.
    if let Some(&placement) = tables.cells.get(&node) {
        style.grid_row = Line {
            start: GridPlacement::from_line_index(placement.row as i16 + 1),
            end: GridPlacement::from_line_index(
                placement.row as i16 + 1 + placement.rowspan as i16,
            ),
        };
        style.grid_column = Line {
            start: GridPlacement::from_line_index(placement.col as i16 + 1),
            end: GridPlacement::from_line_index(
                placement.col as i16 + 1 + placement.colspan as i16,
            ),
        };
    }
    if dom
        .element(node)
        .is_some_and(|e| &*e.name.local == "table")
        && style.display == TaffyDisplay::Grid
    {
        if let Some(&cols) = tables.cols.get(&node) {
            let definite_width = styles.get(node).is_some_and(|cs| {
                !matches!(
                    cs.width,
                    rowser_parsing::cascade::LengthOrAuto::Auto
                )
            });
            style.grid_template_columns = (0..cols)
                .map(|_| {
                    if definite_width {
                        GridTemplateComponent::Single(minmax(
                            track_auto(),
                            fr(1.0),
                        ))
                    } else {
                        GridTemplateComponent::Single(minmax(
                            track_auto(),
                            track_auto(),
                        ))
                    }
                })
                .collect();
            // Rows stay implicit (auto tracks): the count follows the
            // placed cells, including rowspan continuation rows.
            style.grid_template_rows = Vec::new();
            style.grid_auto_rows = vec![minmax(track_auto(), track_auto())];
        }
    }
    // Named grid-area placement: `grid-area: name` on this node resolves
    // against the PARENT's template areas into explicit line placements
    // (taffy lines are 1-based; our areas are 0-based half-open).
    if let Some(name) = styles.get(node).and_then(|cs| cs.grid_area.as_ref()) {
        if let Some(area) = parent_areas.iter().find(|a| &a.name == name) {
            style.grid_row = Line {
                start: GridPlacement::from_line_index(area.row_start as i16 + 1),
                end: GridPlacement::from_line_index(area.row_end as i16 + 1),
            };
            style.grid_column = Line {
                start: GridPlacement::from_line_index(area.col_start as i16 + 1),
                end: GridPlacement::from_line_index(area.col_end as i16 + 1),
            };
        }
    }
    // Register the container's named areas (needed when taffy resolves
    // placements spanning multiple tracks).
    if !own_areas.is_empty() {
        let row_count = own_areas.iter().map(|a| a.row_end).max().unwrap_or(0);
        let column_count = own_areas.iter().map(|a| a.col_end).max().unwrap_or(0);
        style.grid_template_areas = Some(GridTemplateAreas {
            areas: own_areas
                .iter()
                .map(|a| GridTemplateArea {
                    name: a.name.clone(),
                    row_start: a.row_start,
                    row_end: a.row_end,
                    column_start: a.col_start,
                    column_end: a.col_end,
                })
                .collect(),
            row_count,
            column_count,
        });
    }
    // Replaced-element sizing: video/audio default to 300x150; video
    // adopts the decoded aspect ratio when the height is auto and the
    // width is not a percentage (taffy 0.14 collapses percent-width +
    // aspect-ratio to the content size — a real replaced-element measure
    // function is the follow-up).
    if dom
        .element(node)
        .is_some_and(|el| matches!(&*el.name.local, "video" | "audio"))
    {
        if let Some(cs) = styles.get(node) {
            use rowser_parsing::cascade::{Length, LengthOrAuto};
            let auto_w = matches!(cs.width, LengthOrAuto::Auto);
            let auto_h = matches!(cs.height, LengthOrAuto::Auto);
            let width_is_percent = matches!(cs.width, LengthOrAuto::Length(Length::Percent(_)));
            if let Some(&(w, h)) = intrinsic.get(&node) {
                if dom
                    .element(node)
                    .is_some_and(|el| &*el.name.local == "video")
                    && !width_is_percent
                {
                    style.aspect_ratio = Some(w / h.max(1.0));
                    if auto_w && auto_h {
                        style.size.width = Dimension::length(w);
                        style.size.height = Dimension::length(h);
                    }
                }
            } else if auto_w && auto_h {
                style.size.width = Dimension::length(300.0);
                style.size.height = Dimension::length(150.0);
            }
        }
    }
    let taffy_node = tree
        .new_with_children(style, &children)
        .expect("taffy node allocation");
    dom_to_taffy.insert(node, taffy_node);
    taffy_to_dom.insert(taffy_node, node);
    Some(taffy_node)
}

/// Closes the accumulated inline text into an anonymous text-leaf node
/// and appends it to `children`. Called at the end of a box's collection
/// AND whenever a block-level child interrupts the inline run (see
/// `collect_children`): without the mid-run flush, all text fragments of a
/// mixed inline/block container glued into ONE trailing leaf placed after
/// the block boxes — `<td>Nested table:<table>…</table></td>` rendered the
/// label BELOW the nested table, and `Rail<br>spans<br>three<br>rows`
/// concatenated into a single line.
fn flush_text_leaf(
    tree: &mut TaffyTree<TextLeaf>,
    owner: NodeId,
    text: &mut String,
    spans: &mut Vec<(std::ops::Range<usize>, SpanStyle)>,
    defaults: &SpanStyle,
    children: &mut Vec<TaffyNode>,
    force: bool,
) {
    if text.is_empty() && !force {
        return;
    }
    close_inline_text(text, spans);
    let leaf = TextLeaf {
        node: owner,
        text: std::mem::take(text),
        spans: std::mem::take(spans),
        defaults: defaults.clone(),
        cache: None,
    };
    // The leaf is an ANONYMOUS block-level box for the element's inline
    // content — NOT a second copy of the owning element's box. Carrying
    // the owner's margins/paddings/insets/sizes on the leaf DOUBLE-applied
    // them (paragraph margins doubled; percent widths squared). The owner
    // box above already contributes all of those; the leaf starts from a
    // clean style instead.
    //
    // `overflow: hidden` (a taffy-internal layout hint, never painted)
    // makes the leaf an independent formatting context: taffy's block
    // algorithm then places it through the float-aware BFC slot
    // machinery, so inline content measurably narrows against floats
    // (line-box shortening at leaf granularity) — CSS 2.1 §9.5's
    // wrapping behaviour.
    let leaf_style = Style {
        display: TaffyDisplay::Block,
        overflow: taffy::geometry::Point {
            x: TaffyOverflow::Hidden,
            y: TaffyOverflow::Hidden,
        },
        align_self: Some(AlignSelf::START),
        ..Style::default()
    };
    if let Ok(leaf_node) = tree.new_leaf_with_context(leaf_style, leaf) {
        children.push(leaf_node);
    }
}

/// Iterates `node`'s children, splicing boxes and inline text into the
/// parent's accumulation. `display: contents` children are transparent:
/// their OWN children are collected here as if direct children (inherited
/// span styles still flow through them). Block-level children FLUSH the
/// inline text accumulated so far into its own leaf first (CSS 2.1
/// anonymous block boxes around runs of inline content).
#[allow(clippy::too_many_arguments)]
fn collect_children(
    dom: &Dom,
    styles: &StyleMap,
    tree: &mut TaffyTree<TextLeaf>,
    node: NodeId,
    dom_to_taffy: &mut HashMap<NodeId, TaffyNode>,
    taffy_to_dom: &mut HashMap<TaffyNode, NodeId>,
    intrinsic: &HashMap<NodeId, (f32, f32)>,
    parent_areas: &[rowser_parsing::cascade::NamedAreaRaw],
    tables: &TableGrids,
    children: &mut Vec<TaffyNode>,
    text: &mut String,
    spans: &mut Vec<(std::ops::Range<usize>, SpanStyle)>,
    ctx: &SpanStyle,
) {
    for child in dom.flat_children(node) {
        match dom.kind(child) {
            rowser_dom::NodeKind::Text(t) => {
                append_collapsed_text(text, spans, t, ctx);
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
                            // Anonymous block box boundary: the inline run
                            // collected so far closes HERE (CSS 2.1
                            // §9.2.1), not after the block children.
                            if !text.is_empty() {
                                flush_text_leaf(
                                    tree, node, text, spans, &ctx.clone(), children, false,
                                );
                            }
                            if let Some(t) = build_box(
                                dom,
                                styles,
                                tree,
                                child,
                                dom_to_taffy,
                                taffy_to_dom,
                                intrinsic,
                                parent_areas,
                                tables,
                            ) {
                                children.push(t);
                            }
                        } else {
                            let mut inner = ctx.clone();
                            if let Some(cs) = child_style {
                                inner.merge_from(cs);
                            }
                            collect_inline(dom, styles, child, inner, text, spans);
                        }
                    }
                    DisplayMode::None => {}
                    DisplayMode::Contents => {
                        // No box for this element: its children participate
                        // HERE (grid/flex items of the grandparent, block
                        // siblings, or inline text). Inherited span styling
                        // flows through the contents element.
                        let mut inner = ctx.clone();
                        if let Some(cs) = child_style {
                            inner.merge_from(cs);
                        }
                        collect_children(
                            dom,
                            styles,
                            tree,
                            child,
                            dom_to_taffy,
                            taffy_to_dom,
                            intrinsic,
                            parent_areas,
                            tables,
                            children,
                            text,
                            spans,
                            &inner,
                        );
                    }
                    _ => {
                        // Block-level child: flush the inline run first
                        // (anonymous block boxes around inline content —
                        // CSS 2.1 §9.2.1). Without this, text before a
                        // block (e.g. "Nested table:" before a nested
                        // <table>, or lines around <br>) glued into one
                        // leaf appended AFTER the block boxes.
                        if !text.is_empty() {
                            flush_text_leaf(
                                tree, node, text, spans, &ctx.clone(), children, false,
                            );
                        }
                        if let Some(t) = build_box(
                            dom,
                            styles,
                            tree,
                            child,
                            dom_to_taffy,
                            taffy_to_dom,
                            intrinsic,
                            parent_areas,
                            tables,
                        ) {
                            children.push(t);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// True when the element subtree (excluding the element itself) contains
/// block-level content — used to promote block-in-inline wrappers to boxes.
/// `display: contents` elements are transparent: their children count.
fn has_block_descendant(dom: &Dom, styles: &StyleMap, node: NodeId) -> bool {
    for child in dom.flat_children(node) {
        if dom.element(child).is_some() {
            let display = styles
                .get(child)
                .map(|s| s.display)
                .unwrap_or(DisplayMode::Inline);
            match display {
                DisplayMode::Block | DisplayMode::Flex | DisplayMode::Grid => return true,
                DisplayMode::None => continue,
                DisplayMode::Inline | DisplayMode::Contents => {
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
    for child in dom.flat_children(node) {
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
///
/// Whitespace across inline boundaries: "is " + <a>application</a> + " for"
/// must render "is application for". The previous fold trimmed trailing
/// whitespace at every text node, gluing words across element boundaries
/// ("anapplicationfor") — the trailing space of one node is the separator
/// for the next. Trailing space is trimmed once, at leaf close, instead.
fn append_collapsed_text(
    text: &mut String,
    spans: &mut Vec<(std::ops::Range<usize>, SpanStyle)>,
    raw: &str,
    ctx: &SpanStyle,
) {
    if raw.is_empty() {
        return;
    }
    let prev_ws = text
        .chars()
        .last()
        .map(|c| c.is_whitespace())
        .unwrap_or(false);
    let prev_empty = text.is_empty();

    let mut collapsed = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_whitespace() {
            let acc_ok = collapsed
                .chars()
                .last()
                .map(|l| !l.is_whitespace())
                .unwrap_or(!prev_ws && !prev_empty);
            if acc_ok {
                collapsed.push(' ');
            }
        } else {
            collapsed.push(c);
        }
    }
    let body = collapsed.trim_end();
    if body.is_empty() {
        // Pure-whitespace node: a separator between inline elements (or
        // ignorable leading whitespace at the start of the leaf).
        if !prev_ws && !prev_empty {
            text.push(' ');
        }
        return;
    }
    let start = text.len();
    text.push_str(body);
    // One trailing space survives here (separator for the next node);
    // `close_inline_text` trims it at the end of the leaf.
    let ends_ws = body.len() < collapsed.len();
    if ends_ws {
        text.push(' ');
    }
    spans.push((start..text.len(), ctx.clone()));
}

/// Finalizes a leaf's flattened text: removes the single trailing separator
/// space and shortens the last span to match (CSS: trailing whitespace at
/// the end of an inline formatting context does not render).
fn close_inline_text(text: &mut String, spans: &mut Vec<(std::ops::Range<usize>, SpanStyle)>) {
    if text.ends_with(' ') {
        let new_len = text.len() - 1;
        text.truncate(new_len);
        if let Some((range, _)) = spans.last_mut() {
            if range.end > new_len {
                range.end = new_len;
                if range.start >= range.end {
                    spans.pop();
                }
            }
        }
    }
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
    // Taffy uses the measure output's size as the node's FINAL size: no
    // stretch pass runs afterwards for childless nodes. Returning the
    // content width (`total_w`) when a definite width was passed (the
    // stretched block width, or a float-narrowed BFC slot width) shrank
    // every text leaf to its max-content width — then the extract pass
    // re-shaped at that undersized width and the last word wrapped onto
    // a second line (a 699.4px line measured into a 699.0px box). Report
    // the definite width when there is one; content width only when the
    // available space is indefinite (intrinsic sizing passes).
    let out_w = width.filter(|w| *w > total_w).unwrap_or(total_w);
    LayoutOutput {
        size: TaffySize {
            width: out_w,
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
            // The leaf's own taffy layout location (its position inside the
            // parent's content box — flex row slots, paddings, margins) is
            // part of the glyph origin. Positioning text at the PARENT's
            // origin instead made every text leaf in a flex row overlap at
            // the container corner: nav bars, table rows (our table→flex
            // mapping) and grid tracks collapsed into one jumbled column.
            let child_layout = tree.layout(child);
            let width = child_layout
                .as_ref()
                .map(|l| l.size.width)
                .unwrap_or(0.0)
                .max(0.0);
            let origin = match child_layout.as_ref() {
                Ok(l) => (node_abs.0 + l.location.x, node_abs.1 + l.location.y),
                Err(_) => node_abs,
            };
            let glyphs = text::shape_at(leaf, font_system, Some(width), origin);
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


    /// Finds the first element with `tag` under the layout root.
    fn find_tag(dom: &Dom, tag: &str) -> Option<NodeId> {
        dom.subtree_elements(dom.document()).find(|n| {
            dom.element(*n)
                .map(|e| &*e.name.local == tag)
                .unwrap_or(false)
        })
    }

    /// CSS 2.1 §9.5: a left float sticks to the containing block's left
    /// content edge; subsequent line content flows beside it, not under it.
    #[test]
    fn float_left_places_box_and_narrows_content() {
        let html = br#"<html><body>
        <div><div style="float:left; width:200px; height:100px; background:#fa0"></div>
        <p>Content beside the float</p></div>
        </body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let divs: Vec<Rect> = doc
            .dom
            .subtree_elements(doc.dom.document())
            .filter(|n| doc.dom.element(*n).is_some_and(|e| &*e.name.local == "div"))
            .filter_map(|n| layout.rects.get(&n).copied())
            .collect();
        let float = divs[1]; // first inner div = the float
        // The float's containing block chain starts at the body content
        // edge (UA 8px margin), matching Chrome.
        assert!((float.x - 8.0).abs() < 1.0, "float at left edge: {float:?}");
        assert!((float.w - 200.0).abs() < 1.0, "float width: {float:?}");
        // The block box may span the full width (CSS: blocks overlap floats),
        // but the LINE CONTENT — the shaped glyphs — must clear the float
        // band horizontally (line boxes are shortened).
        let mut glyph_count = 0;
        for run in &layout.text {
            for g in &run.glyphs {
                glyph_count += 1;
                assert!(
                    g.x as f32 >= float.w - 1.0,
                    "glyph x={} inside float band (float w={})",
                    g.x,
                    float.w
                );
            }
        }
        assert!(glyph_count > 5, "no glyphs shaped");
    }

    /// A right float hugs the right content edge; line content stays left.
    #[test]
    fn float_right_hugs_right_edge() {
        let html = br#"<html><body style="margin:0">
        <div><div style="float:right; width:300px; height:80px; background:#fa0"></div>
        <p>The quick brown fox jumps over the lazy dog again and again to wrap beside the float zone</p></div>
        </body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let float = *layout
            .rects
            .iter()
            .find(|(_, r)| (r.w - 300.0).abs() < 1.0)
            .map(|(_, r)| r)
            .unwrap();
        assert!(
            (float.x + float.w - 800.0).abs() < 2.0,
            "float hugs right edge: {float:?}"
        );
        // Line content must stay LEFT of the right float's band.
        let mut glyph_count = 0;
        for run in &layout.text {
            for g in &run.glyphs {
                glyph_count += 1;
                assert!(
                    (g.x as f32) < float.x - 2.0,
                    "glyph x={} overlaps right float band (float x={})",
                    g.x,
                    float.x
                );
            }
        }
        assert!(glyph_count > 5, "no glyphs shaped");
    }

    /// `clear: both` pushes the following block below both floats.
    #[test]
    fn clear_both_pushes_below_floats() {
        let html = br#"<html><body style="margin:0">
        <div><div style="float:left; width:150px; height:120px"></div>
        <div style="float:right; width:150px; height:90px"></div>
        <div style="clear:both; height:20px; background:#333">Cleared</div></div>
        </body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let cleared = layout
            .rects
            .values()
            .find(|r| (r.h - 20.0).abs() < 2.0 && r.y > 100.0)
            .copied()
            .unwrap_or_else(|| panic!("cleared box below floats, rects: {:?}", layout.rects));
        assert!(cleared.y >= 118.0, "cleared box at y={cleared:?}");
    }

    /// Regression: the anonymous text leaf must NOT carry the owner's
    /// margins — paragraphs with margin:40px previously measured 100px tall
    /// (40+20+40 margins + 20 text) and gaps between them doubled.
    #[test]
    fn text_leaf_does_not_double_owner_margins() {
        let html = br#"<html><body>
        <p style="margin:40px 0; line-height:20px">AAA</p>
        <p style="margin:40px 0; line-height:20px">BBB</p>
        </body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let ps: Vec<Rect> = doc
            .dom
            .subtree_elements(doc.dom.document())
            .filter(|n| doc.dom.element(*n).is_some_and(|e| &*e.name.local == "p"))
            .filter_map(|n| layout.rects.get(&n).copied())
            .collect();
        assert_eq!(ps.len(), 2);
        assert!(
            (ps[0].h - 20.0).abs() < 2.0,
            "p height = one text line, got {:?}",
            ps[0]
        );
        // CSS margin collapsing: 40px gap between the two paragraphs.
        assert!(
            (ps[1].y - (ps[0].y + ps[0].h + 40.0)).abs() < 2.0,
            "collapsed 40px gap: p1={:?} p2={:?}",
            ps[0],
            ps[1]
        );
    }

    /// CSS grid line-based placement: `grid-column: 1 / 3` must span two
    /// tracks; item rects must land on the requested lines.
    #[test]
    fn grid_line_placement() {
        let html = br#"<html><body style="margin:0"><div style="display:grid; grid-template-columns: 100px 100px 100px; grid-template-rows: 60px 60px; width: 300px">
        <div style="grid-column: 1 / 3; grid-row: 1; background:#f00" id="a">A</div>
        <div style="grid-column: 3; grid-row: 1;" id="b">B</div>
        <div style="grid-column: span 2; grid-row: 2;" id="c">CC</div>
        </div></body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let mut by_id = HashMap::new();
        for node in doc.dom.subtree_elements(doc.dom.document()) {
            if let Some(id) = doc.dom.get_attr(node, "id") {
                if let Some(r) = layout.rects.get(&node) {
                    by_id.insert(id.to_string(), *r);
                }
            }
        }
        let a = by_id["a"];
        assert!((a.x - 0.0).abs() < 1.0 && (a.w - 200.0).abs() < 2.0, "A spans cols 1-3: {a:?}");
        let b = by_id["b"];
        assert!((b.x - 200.0).abs() < 1.0 && (b.w - 100.0).abs() < 2.0, "B in col 3: {b:?}");
        let c = by_id["c"];
        assert!((c.y - 60.0).abs() < 1.0 && (c.w - 200.0).abs() < 2.0, "C spans 2 cols in row 2: {c:?}");
    }

    /// `grid-area: r1 / c1 / r2 / c2` (4-line form) places the item.
    #[test]
    fn grid_area_line_form() {
        let html = br#"<html><body style="margin:0"><div style="display:grid; grid-template-columns: 80px 80px 80px; grid-template-rows: 50px 50px 50px; width: 240px">
        <div style="grid-area: 2 / 1 / 4 / 3; background:#f00" id="x">XX</div>
        </div></body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let mut rect = None;
        for node in doc.dom.subtree_elements(doc.dom.document()) {
            if doc.dom.get_attr(node, "id").is_some_and(|id| id == "x") {
                rect = layout.rects.get(&node).copied();
            }
        }
        let r = rect.expect("x rect");
        assert!((r.x - 0.0).abs() < 1.0, "x at col 1: {r:?}");
        assert!((r.y - 50.0).abs() < 1.0, "x at row 2: {r:?}");
        assert!((r.w - 160.0).abs() < 2.0, "x spans 2 cols (160px): {r:?}");
        assert!((r.h - 100.0).abs() < 2.0, "x spans 2 rows (100px): {r:?}");
    }

    /// table width="100%" (presentational) → definite grid width → the
    /// tracks flex to fill the containing block.
    #[test]
    fn table_width_percent_stretches_full_width() {
        let html = br#"<html><body style="margin:0"><table width="100%" id="full">
        <tr><td id="c1">a</td><td id="c2">b</td></tr>
        </table></body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let node_id = |id: &str| -> NodeId {
            for node in doc.dom.subtree_elements(doc.dom.document()) {
                if doc.dom.get_attr(node, "id") == Some(id) {
                    return node;
                }
            }
            panic!("no node for {id}")
        };
        let table = *layout.rects.get(&node_id("full")).expect("table rect");
        assert!(
            (table.w - 800.0).abs() < 8.0,
            "full-width table: {table:?}"
        );
        let c2 = *layout.rects.get(&node_id("c2")).expect("c2 rect");
        assert!(c2.x >= 399.0, "second column in the right half: {c2:?}");
    }

    /// `<br>` is a block-level boundary (UA): text around it splits into
    /// separate lines stacked vertically, and text BEFORE a block child
    /// (nested table) stays ABOVE it — previously all fragments glued into
    /// one trailing leaf placed after the block boxes.
    #[test]
    fn br_splits_lines_and_text_precedes_block_children() {
        let html = br#"<html><body><div style="width:200px">
        <div id="multi">A<br>B<br>C</div>
        <div id="mixed">Label:<table><tr><td id="inner">cell</td></tr></table></div>
        </div></body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let rect = |id: &str| -> Rect {
            for node in doc.dom.subtree_elements(doc.dom.document()) {
                if doc.dom.get_attr(node, "id") == Some(id) {
                    if let Some(r) = layout.rects.get(&node) {
                        return *r;
                    }
                }
            }
            panic!("no rect for {id}")
        };
        // Node ids for the containers.
        let node_id = |id: &str| -> NodeId {
            for node in doc.dom.subtree_elements(doc.dom.document()) {
                if doc.dom.get_attr(node, "id") == Some(id) {
                    return node;
                }
            }
            panic!("no node for {id}")
        };
        let (multi, mixed) = (node_id("multi"), node_id("mixed"));
        // The br container holds three separate single-line leaves with
        // increasing baselines.
        let ys: Vec<f32> = layout
            .text
            .iter()
            .filter(|run| run.node == multi)
            .map(|run| run.glyphs.first().map(|g| g.y as f32).unwrap_or(-1.0))
            .collect();
        assert_eq!(ys.len(), 3, "three br-separated lines: {ys:?}");
        assert!(ys[1] > ys[0] + 5.0 && ys[2] > ys[1] + 5.0, "br lines stack: {ys:?}");
        // Text before the block child stays ABOVE the nested table cell.
        let label_y = layout
            .text
            .iter()
            .filter(|run| run.node == mixed)
            .map(|run| run.glyphs.first().map(|g| g.y as f32).unwrap_or(-1.0))
            .next()
            .expect("label leaf");
        let inner = rect("inner");
        assert!(
            label_y < inner.y + inner.h,
            "label above nested table: label_y={label_y} inner={inner:?}"
        );
    }

    /// CSS 2.1 §17: colspan=2 spans two columns; the next cell lands in
    /// column 3, not overlapping the span.
    #[test]
    fn table_colspan_places_cells_in_columns() {
        let html = br#"<html><body><table>
        <tr><td colspan="2" id="wide">A</td><td id="b">B</td></tr>
        <tr><td id="x1">XX</td><td id="x2">YY</td><td id="b2">B</td></tr>
        </table></body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let rect = |id: &str| -> Rect {
            for node in doc.dom.subtree_elements(doc.dom.document()) {
                if doc.dom.get_attr(node, "id") == Some(id) {
                    if let Some(r) = layout.rects.get(&node) {
                        return *r;
                    }
                }
            }
            panic!("no rect for {id}");
        };
        let wide = rect("wide");
        let b = rect("b");
        let x1 = rect("x1");
        let x2 = rect("x2");
        assert!(b.x >= wide.x + wide.w - 1.0, "B starts after the colspan span: wide={wide:?} b={b:?}");
        // The span covers the sum of the two single-cell columns (which
        // carry their own content constraints from row 2).
        assert!(
            (wide.w - (x1.w + x2.w)).abs() < 3.0,
            "colspan width = col1 + col2: wide={wide:?} x1={x1:?} x2={x2:?}"
        );
        assert!(
            (x2.x - (x1.x + x1.w)).abs() < 2.0,
            "columns are adjacent: x1={x1:?} x2={x2:?}"
        );
    }

    /// rowspan=2: the spanning cell is exactly two rows tall; the cell
    /// below-left in the NEXT row starts below the span, not inside it.
    #[test]
    fn table_rowspan_spans_two_rows() {
        let html = br#"<html><body><table>
        <tr><td rowspan="2" id="tall">T</td><td id="r1">1</td></tr>
        <tr><td id="r2">2</td></tr>
        </table></body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let rect = |id: &str| -> Rect {
            for node in doc.dom.subtree_elements(doc.dom.document()) {
                if doc.dom.get_attr(node, "id") == Some(id) {
                    if let Some(r) = layout.rects.get(&node) {
                        return *r;
                    }
                }
            }
            panic!("no rect for {id}");
        };
        let tall = rect("tall");
        let r1 = rect("r1");
        let r2 = rect("r2");
        // The spanning cell is beside r1 (same row)...
        assert!((tall.y - r1.y).abs() < 1.0, "tall aligns with row 1: tall={tall:?} r1={r1:?}");
        // ...and covers row 2's band vertically.
        assert!(
            tall.y + tall.h >= r2.y + r2.h - 2.0,
            "tall spans both rows: tall={tall:?} r2={r2:?}"
        );
        assert!(
            tall.h > r1.h * 1.5,
            "rowspan cell taller than a single row: tall={tall:?} r1={r1:?}"
        );
    }

    /// A classic 3-column layout row: colspan across the header, rowspan
    /// down the left rail, per CSS 2.1 §17.4.1 the following cells flow
    /// around the occupied slots.
    #[test]
    fn table_span_matrix_matches_occupancy() {
        let html = br#"<html><body><table>
        <tr><td colspan="3" id="h">H</td></tr>
        <tr><td rowspan="2" id="l">L</td><td id="a">a</td><td id="b">b</td></tr>
        <tr><td id="c">c</td><td id="d">d</td></tr>
        </table></body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let rect = |id: &str| -> Rect {
            for node in doc.dom.subtree_elements(doc.dom.document()) {
                if doc.dom.get_attr(node, "id") == Some(id) {
                    if let Some(r) = layout.rects.get(&node) {
                        return *r;
                    }
                }
            }
            panic!("no rect for {id}");
        };
        let (h, l, a, b, c, d) = (
            rect("h"), rect("l"), rect("a"), rect("b"), rect("c"), rect("d"),
        );
        // Header spans the full three-column width.
        assert!(h.w >= a.w + b.w + 2.0 * (a.x - l.x) - 4.0 || h.w > 2.0 * a.w, "header spans: h={h:?}");
        // Row 2: rail + a + b side by side, a right of l.
        assert!(a.x > l.x + l.w - 2.0, "a right of rail: l={l:?} a={a:?}");
        assert!((b.x - (a.x + a.w)).abs() < 3.0, "b follows a: a={a:?} b={b:?}");
        // Row 3 (below the rowspan): c and d shifted into the rail's
        // column band only if the rail no longer occupies it.
        assert!((c.y - (l.y + l.h)).abs() < 40.0, "c below the rowspan: l={l:?} c={c:?}");
        assert!((d.x - (c.x + c.w)).abs() < 3.0, "d follows c: c={c:?} d={d:?}");
    }

    /// `grid-auto-rows` sizes implicit rows.
    #[test]
    fn grid_auto_rows_sizing() {
        let html = br#"<html><body style="margin:0"><div style="display:grid; grid-template-columns: 100px 100px; grid-auto-rows: 40px; width: 200px">
        <div id="r1">1</div><div id="r2">2</div><div id="r3">3</div><div id="r4">4</div>
        </div></body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport { width: 800.0, height: 600.0 },
            &Default::default(),
        );
        let mut by_id = HashMap::new();
        for node in doc.dom.subtree_elements(doc.dom.document()) {
            if let Some(id) = doc.dom.get_attr(node, "id") {
                if let Some(r) = layout.rects.get(&node) {
                    by_id.insert(id.to_string(), *r);
                }
            }
        }
        assert!((by_id["r1"].h - 40.0).abs() < 1.0, "auto row height 40: {:?}", by_id["r1"]);
        assert!((by_id["r3"].y - 40.0).abs() < 1.0, "row 2 at y=40: {:?}", by_id["r3"]);
        assert!((by_id["r2"].x - 100.0).abs() < 1.0, "r2 in col 2: {:?}", by_id["r2"]);
    }

    /// The built-in media viewer page: video must fill the width and get a
    /// real aspect-derived height. Diagnostic over CSS variants — the
    /// shipping viewer CSS must be the one that passes.
    #[test]
    fn media_viewer_layout_sizing() {
        let variants: &[(&str, &str)] = &[
            (
                "shipping: viewport px width (as the viewer page bakes it)",
                "body{margin:0;background:#000}video{width:1360px;height:auto;background:#000}",
            ),
            (
                "percent width (documented taffy 0.14 limitation)",
                "body{margin:0}video{width:100%;height:auto}",
            ),
            ("no css at all (intrinsic default)", ""),
        ];
        let template = br#"<!doctype html><html><head><style>__CSS__</style></head>
<body><video src="x.mp4" controls autoplay></video></body></html>"#;
        for (name, css) in variants {
            let html = String::from_utf8_lossy(template).replace("__CSS__", css);
            let doc = parse_html(html.as_bytes());
            let sheet = parse_stylesheet(css, &MediaContext::default());
            let video = {
                let dom = &doc.dom;
                dom.subtree_elements(dom.document())
                    .find(|n| {
                        dom.element(*n)
                            .map(|e| &*e.name.local == "video")
                            .unwrap_or(false)
                    })
                    .expect("video element")
            };
            let mut engine = LayoutEngine::new();
            let mut intrinsic = HashMap::new();
            intrinsic.insert(video, (320.0, 240.0));
            let (_, layout) = engine.layout_document(
                &doc.dom,
                &[sheet],
                &MediaContext::default(),
                Viewport {
                    width: 1360.0,
                    height: 724.0,
                },
                &intrinsic,
            );
            let rect = layout.rects.get(&video).copied();
            eprintln!("[viewer-variant {name}] video rect = {rect:?}");
        }
        // The shipping variant (index 0) must be full-width with an
        // aspect-derived height.
        let (name, css) = variants[0];
        let html = String::from_utf8_lossy(template).replace("__CSS__", css);
        let doc = parse_html(html.as_bytes());
        let sheet = parse_stylesheet(css, &MediaContext::default());
        let video = {
            let dom = &doc.dom;
            dom.subtree_elements(dom.document())
                .find(|n| {
                    dom.element(*n)
                        .map(|e| &*e.name.local == "video")
                        .unwrap_or(false)
                })
                .expect("video element")
        };
        let mut engine = LayoutEngine::new();
        let mut intrinsic = HashMap::new();
        intrinsic.insert(video, (320.0, 240.0));
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport {
                width: 1360.0,
                height: 724.0,
            },
            &intrinsic,
        );
        let rect = layout.rects.get(&video).copied().expect("video rect");
        assert!(rect.w > 1200.0, "{name}: video fills width, got {rect:?}");
        assert!(
            rect.h > 700.0,
            "{name}: aspect-derived height (1360/(320/240)=1020), got {rect:?}"
        );
    }

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
            &Default::default(),
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
        // Chrome semantics: body carries the UA 8px margin, so the border
        // box is inset 8px and spans viewport - 16.
        assert!(
            (body_rect.x - 8.0).abs() < 1.0,
            "body x {}",
            body_rect.x
        );
        assert!(
            (body_rect.w - 784.0).abs() < 1.0,
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
            &Default::default(),
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
