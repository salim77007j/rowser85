//! The cascade: DOM + rules → computed style per element.
//!
//! Implements the CSS cascade steps that matter for our layout and painting:
//! origin ordering (UA → author → inline), specificity, source order,
//! `!important` overrides, inheritance, and unit resolution
//! (`em`/`rem`/`%` and font-size keywords).

use std::collections::HashMap;

use rowser_dom::selector::{CachesWrap, ElementRef};
use rowser_dom::{Dom, NodeId};

use crate::css::{parse_style_attribute, MediaContext, ParsedStylesheet, StyleRuleEntry};
use crate::selector_bucket::RuleIndex;
use crate::ua;

/// A resolved sRGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
    /// Alpha channel.
    pub a: u8,
}

impl Rgba {
    /// Constructs a color.
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Rgba { r, g, b, a }
    }

    /// Fully opaque color.
    pub const fn new_opaque(r: u8, g: u8, b: u8) -> Self {
        Rgba { r, g, b, a: 255 }
    }

    /// Fully transparent color.
    pub const TRANSPARENT: Rgba = Rgba::new(0, 0, 0, 0);

    /// The `currentColor` sentinel used during flattening.
    pub const CURRENT_COLOR: Rgba = Rgba::new(1, 2, 3, 255);

    /// Premultiplied RGBA bytes (for tiny-skia).
    pub fn premultiplied(self) -> [u8; 4] {
        let a = self.a as f32 / 255.0;
        let f = |c: u8| ((c as f32) * a).round().clamp(0.0, 255.0) as u8;
        [f(self.r), f(self.g), f(self.b), self.a]
    }
}

/// A CSS length, pre-cascade.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    /// Absolute pixels.
    Px(f32),
    /// Relative font size (own, or parent for `font-size`).
    Em(f32),
    /// Relative to the root font size.
    Rem(f32),
    /// Percentage of the containing block (resolved by the layout engine).
    Percent(f32),
}

/// Root (html) font size in pixels.
pub const ROOT_FONT_SIZE: f32 = 16.0;

impl Length {
    /// Resolves to pixels against a reference font size.
    pub fn resolve(self, font_size: f32) -> f32 {
        match self {
            Length::Px(n) => n,
            Length::Em(n) => n * font_size,
            Length::Rem(n) => n * ROOT_FONT_SIZE,
            Length::Percent(_) => 0.0,
        }
    }
}

/// A length or `auto`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LengthOrAuto {
    /// Automatic sizing.
    Auto,
    /// A concrete length.
    Length(Length),
}

impl LengthOrAuto {
    /// Resolves to `Some(px)` or `None` for `auto`.
    pub fn resolve(self, font_size: f32) -> Option<f32> {
        match self {
            LengthOrAuto::Auto => None,
            LengthOrAuto::Length(l) => Some(l.resolve(font_size)),
        }
    }
}

/// The `display` property, resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// Block-level flow.
    Block,
    /// Inline content (flattened into the parent block's text run).
    Inline,
    /// Flexbox container.
    Flex,
    /// CSS grid container.
    Grid,
    /// Not rendered.
    None,
}

/// The `position` property, resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionMode {
    /// In-flow.
    Static,
    /// In-flow with offset support.
    Relative,
    /// Out-of-flow, positioned against the containing block.
    Absolute,
}

/// Flex direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDirectionMode {
    /// Horizontal, main start left.
    Row,
    /// Horizontal, main start right.
    RowReverse,
    /// Vertical.
    Column,
    /// Vertical, bottom-up.
    ColumnReverse,
}

/// Flex wrap mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexWrapMode {
    /// Single line.
    NoWrap,
    /// Wrap onto new lines.
    Wrap,
    /// Wrap in reverse.
    WrapReverse,
}

/// Main-axis distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JustifyContentMode {
    /// Pack to main start.
    Start,
    /// Center on the main axis.
    Center,
    /// Pack to main end.
    End,
    /// Even spacing with no outer gaps.
    SpaceBetween,
    /// Even spacing with half outer gaps.
    SpaceAround,
    /// Even spacing with full outer gaps.
    SpaceEvenly,
}

/// Cross-axis alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignItemsMode {
    /// Pack to cross start.
    Start,
    /// Center on the cross axis.
    Center,
    /// Pack to cross end.
    End,
    /// Stretch to fill.
    Stretch,
    /// `space-between` (align-content only).
    SpaceBetween,
    /// `space-around` (align-content only).
    SpaceAround,
    /// `space-evenly` (align-content only).
    SpaceEvenly,
}

/// Font style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontStyleMode {
    /// Upright.
    Normal,
    /// Italic/oblique.
    Italic,
}

/// Text alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlignMode {
    /// Left edge.
    Left,
    /// Right edge.
    Right,
    /// Centered.
    Center,
    /// Justified.
    Justify,
}

/// Border line style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyleMode {
    /// No border.
    None,
    /// Solid line.
    Solid,
    /// Dotted/dashed line.
    Dashed,
}

/// A resolved border edge.
#[derive(Debug, Clone, Copy)]
pub struct BorderInfo {
    /// Border width in px.
    pub width: f32,
    /// Border color.
    pub color: Rgba,
    /// Border line style.
    pub style: LineStyleMode,
}

/// One box edge (top/right/bottom/left).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Edges<T> {
    /// Top edge.
    pub top: T,
    /// Right edge.
    pub right: T,
    /// Bottom edge.
    pub bottom: T,
    /// Left edge.
    pub left: T,
}

/// A pre-cascade font size value.
#[derive(Debug, Clone, Copy)]
pub enum FontSizeRaw {
    /// Absolute px.
    Px(f32),
    /// `em` relative to the parent font size.
    Em(f32),
    /// `rem` relative to the root font size.
    Rem(f32),
    /// Percentage of the parent font size.
    Percent(f32),
    /// Multiplication factor (`larger`/`smaller`).
    Factor(f32),
}

/// A pre-cascade font weight value.
#[derive(Debug, Clone, Copy)]
pub enum FontWeightRaw {
    /// Absolute weight 100-900.
    Weight(f32),
    /// `bolder`.
    Bolder,
    /// `lighter`.
    Lighter,
}

/// A pre-cascade line-height value.
#[derive(Debug, Clone, Copy)]
pub enum LineHeightRaw {
    /// `normal`.
    Normal,
    /// Unitless multiplier.
    Number(f32),
    /// Absolute px.
    Px(f32),
    /// `em` relative to own font size.
    Em(f32),
    /// `rem`.
    Rem(f32),
    /// Percentage of own font size.
    Percent(f32),
}

/// A pre-cascade border edge.
#[derive(Debug, Clone, Copy)]
pub struct BorderEdgeRaw {
    /// Border width.
    pub width: Length,
    /// Border color, when specified.
    pub color: Option<Rgba>,
    /// Border line style.
    pub style: LineStyleMode,
}

impl BorderEdgeRaw {
    /// Width used when only color/style was specified.
    pub const DEFAULT_WIDTH: Length = Length::Px(3.0);
}

/// One bound (min or max side) of a grid track sizing function.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackBoundRaw {
    /// `auto`
    Auto,
    /// `min-content`
    MinContent,
    /// `max-content`
    MaxContent,
    /// Fixed length in px.
    Px(f32),
    /// Percentage of the grid container.
    Percent(f32),
    /// Fraction of the remaining space.
    Fr(f32),
}

/// One grid track: a (min, max) sizing pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackRaw {
    /// Minimum track size.
    pub min: TrackBoundRaw,
    /// Maximum track size.
    pub max: TrackBoundRaw,
}

/// One named grid area: name → rectangle in grid cells
/// (0-based, half-open [start, end)).
#[derive(Debug, Clone, PartialEq)]
pub struct NamedAreaRaw {
    /// Area name.
    pub name: String,
    /// First grid row (0-based).
    pub row_start: u16,
    /// One past the last grid row.
    pub row_end: u16,
    /// First grid column (0-based).
    pub col_start: u16,
    /// One past the last grid column.
    pub col_end: u16,
}

/// Flattened declarations from one rule (or inline style attribute).
///
/// All values are pre-parsed; the cascade resolves them against the
/// inherited context.
#[derive(Debug, Clone, Default)]
pub struct StyleProps {
    /// `background-color`.
    pub background_color: Option<Rgba>,
    /// `color`.
    pub color: Option<Rgba>,
    /// `display`.
    pub display: Option<DisplayMode>,
    /// `position`.
    pub position: Option<PositionMode>,
    /// `width`.
    pub width: Option<LengthOrAuto>,
    /// `height`.
    pub height: Option<LengthOrAuto>,
    /// `min-width`.
    pub min_width: Option<LengthOrAuto>,
    /// `min-height`.
    pub min_height: Option<LengthOrAuto>,
    /// `max-width`.
    pub max_width: Option<LengthOrAuto>,
    /// `max-height`.
    pub max_height: Option<LengthOrAuto>,
    /// `margin-top`.
    pub margin_top: Option<LengthOrAuto>,
    /// `margin-right`.
    pub margin_right: Option<LengthOrAuto>,
    /// `margin-bottom`.
    pub margin_bottom: Option<LengthOrAuto>,
    /// `margin-left`.
    pub margin_left: Option<LengthOrAuto>,
    /// `padding-top`.
    pub padding_top: Option<LengthOrAuto>,
    /// `padding-right`.
    pub padding_right: Option<LengthOrAuto>,
    /// `padding-bottom`.
    pub padding_bottom: Option<LengthOrAuto>,
    /// `padding-left`.
    pub padding_left: Option<LengthOrAuto>,
    /// `border-top`.
    pub border_top: Option<BorderEdgeRaw>,
    /// `border-right`.
    pub border_right: Option<BorderEdgeRaw>,
    /// `border-bottom`.
    pub border_bottom: Option<BorderEdgeRaw>,
    /// `border-left`.
    pub border_left: Option<BorderEdgeRaw>,
    /// `font-family` (full stack, in preference order).
    pub font_family: Option<Vec<String>>,
    /// `top`.
    pub top: Option<LengthOrAuto>,
    /// `right`.
    pub right: Option<LengthOrAuto>,
    /// `bottom`.
    pub bottom: Option<LengthOrAuto>,
    /// `left`.
    pub left: Option<LengthOrAuto>,
    /// `z-index`.
    pub z_index: Option<i32>,
    /// `font-size`.
    pub font_size: Option<FontSizeRaw>,
    /// `font-weight`.
    pub font_weight: Option<FontWeightRaw>,
    /// `font-style`.
    pub font_style: Option<FontStyleMode>,
    /// `line-height`.
    pub line_height: Option<LineHeightRaw>,
    /// `text-align`.
    pub text_align: Option<TextAlignMode>,
    /// `flex-direction`.
    pub flex_direction: Option<FlexDirectionMode>,
    /// `flex-wrap`.
    pub flex_wrap: Option<FlexWrapMode>,
    /// `flex-grow`.
    pub flex_grow: Option<f32>,
    /// `flex-shrink`.
    pub flex_shrink: Option<f32>,
    /// `flex-basis`.
    pub flex_basis: Option<LengthOrAuto>,
    /// `justify-content`.
    pub justify_content: Option<JustifyContentMode>,
    /// `align-items`.
    pub align_items: Option<AlignItemsMode>,
    /// `align-content`.
    pub align_content: Option<AlignItemsMode>,
    /// `row-gap`.
    pub row_gap: Option<Length>,
    /// `column-gap`.
    pub column_gap: Option<Length>,
    /// `grid-template-columns` (flattened, repeats expanded).
    pub grid_template_columns: Option<Vec<TrackRaw>>,
    /// `grid-template-rows` (flattened, repeats expanded).
    pub grid_template_rows: Option<Vec<TrackRaw>>,
    /// `grid-template-areas` (named area rectangles).
    pub grid_template_areas: Option<Vec<NamedAreaRaw>>,
    /// `grid-area` (name form).
    pub grid_area: Option<String>,
}

/// Fully resolved style for one element.
#[derive(Debug, Clone)]
pub struct ComputedStyle {
    /// `display`.
    pub display: DisplayMode,
    /// `position`.
    pub position: PositionMode,
    /// `top` inset.
    pub top: LengthOrAuto,
    /// `right` inset.
    pub right: LengthOrAuto,
    /// `bottom` inset.
    pub bottom: LengthOrAuto,
    /// `left` inset.
    pub left: LengthOrAuto,
    /// `z-index` (None = auto).
    pub z_index: Option<i32>,
    /// Foreground color.
    pub color: Rgba,
    /// Background color.
    pub background_color: Rgba,
    /// First font family (resolved stack head; see `font_stack`).
    pub font_family: String,
    /// Full font stack in CSS preference order — walked at shaping time
    /// until an installed font is found (CSS font matching).
    pub font_stack: Vec<String>,
    /// Font size in px.
    pub font_size: f32,
    /// Font weight (100-900).
    pub font_weight: f32,
    /// Font style.
    pub font_style: FontStyleMode,
    /// Line height in px.
    pub line_height: f32,
    /// Text alignment.
    pub text_align: TextAlignMode,
    /// Margins.
    pub margins: Edges<LengthOrAuto>,
    /// Paddings.
    pub paddings: Edges<LengthOrAuto>,
    /// Borders.
    pub borders: Edges<BorderInfo>,
    /// Preferred width.
    pub width: LengthOrAuto,
    /// Preferred height.
    pub height: LengthOrAuto,
    /// Minimum width.
    pub min_width: LengthOrAuto,
    /// Minimum height.
    pub min_height: LengthOrAuto,
    /// Maximum width.
    pub max_width: LengthOrAuto,
    /// Maximum height.
    pub max_height: LengthOrAuto,
    /// Flex direction.
    pub flex_direction: FlexDirectionMode,
    /// Flex wrap.
    pub flex_wrap: FlexWrapMode,
    /// Flex grow factor.
    pub flex_grow: f32,
    /// Flex shrink factor.
    pub flex_shrink: f32,
    /// Flex basis.
    pub flex_basis: LengthOrAuto,
    /// Main-axis distribution.
    pub justify_content: JustifyContentMode,
    /// Cross-axis alignment.
    pub align_items: AlignItemsMode,
    /// Wrap-line distribution.
    pub align_content: AlignItemsMode,
    /// Row gap in px.
    pub gap_row: f32,
    /// Column gap in px.
    pub gap_column: f32,
    /// Grid column track list (empty = auto).
    pub grid_template_columns: Vec<TrackRaw>,
    /// Grid row track list (empty = auto).
    pub grid_template_rows: Vec<TrackRaw>,
    /// Named grid areas (for `grid-area: name` placement).
    pub grid_template_areas: Vec<NamedAreaRaw>,
    /// `grid-area` name (placed against the parent's areas).
    pub grid_area: Option<String>,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        ComputedStyle {
            display: DisplayMode::Inline,
            position: PositionMode::Static,
            top: LengthOrAuto::Auto,
            right: LengthOrAuto::Auto,
            bottom: LengthOrAuto::Auto,
            left: LengthOrAuto::Auto,
            z_index: None,
            color: Rgba::new_opaque(0, 0, 0),
            background_color: Rgba::TRANSPARENT,
            font_family: "sans-serif".to_owned(),
            font_stack: vec!["sans-serif".to_owned()],
            font_size: 16.0,
            font_weight: 400.0,
            font_style: FontStyleMode::Normal,
            line_height: 20.0,
            text_align: TextAlignMode::Left,
            margins: Edges {
                top: LengthOrAuto::Length(Length::Px(0.0)),
                right: LengthOrAuto::Length(Length::Px(0.0)),
                bottom: LengthOrAuto::Length(Length::Px(0.0)),
                left: LengthOrAuto::Length(Length::Px(0.0)),
            },
            paddings: Edges {
                top: LengthOrAuto::Length(Length::Px(0.0)),
                right: LengthOrAuto::Length(Length::Px(0.0)),
                bottom: LengthOrAuto::Length(Length::Px(0.0)),
                left: LengthOrAuto::Length(Length::Px(0.0)),
            },
            borders: Edges {
                top: BorderInfo {
                    width: 0.0,
                    color: Rgba::TRANSPARENT,
                    style: LineStyleMode::None,
                },
                right: BorderInfo {
                    width: 0.0,
                    color: Rgba::TRANSPARENT,
                    style: LineStyleMode::None,
                },
                bottom: BorderInfo {
                    width: 0.0,
                    color: Rgba::TRANSPARENT,
                    style: LineStyleMode::None,
                },
                left: BorderInfo {
                    width: 0.0,
                    color: Rgba::TRANSPARENT,
                    style: LineStyleMode::None,
                },
            },
            width: LengthOrAuto::Auto,
            height: LengthOrAuto::Auto,
            min_width: LengthOrAuto::Auto,
            min_height: LengthOrAuto::Auto,
            max_width: LengthOrAuto::Auto,
            max_height: LengthOrAuto::Auto,
            flex_direction: FlexDirectionMode::Row,
            flex_wrap: FlexWrapMode::NoWrap,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: LengthOrAuto::Auto,
            justify_content: JustifyContentMode::Start,
            align_items: AlignItemsMode::Stretch,
            align_content: AlignItemsMode::Stretch,
            gap_row: 0.0,
            gap_column: 0.0,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_template_areas: Vec::new(),
            grid_area: None,
        }
    }
}

/// Computed styles for every element in a document.
#[derive(Debug, Default, Clone)]
pub struct StyleMap {
    /// Node id → computed style.
    pub styles: HashMap<NodeId, ComputedStyle>,
}

impl StyleMap {
    /// Style of a node (elements only; text nodes inherit from the parent).
    pub fn get(&self, node: NodeId) -> Option<&ComputedStyle> {
        self.styles.get(&node)
    }
}

struct RuleSet {
    entries: Vec<StyleRuleEntry>,
    index: RuleIndex,
}

impl RuleSet {
    fn build(entries: Vec<StyleRuleEntry>) -> Self {
        let index = RuleIndex::build(&entries);
        RuleSet { entries, index }
    }
}

/// Computes styles for the whole document.
///
/// `author` sheets are applied in order after the UA stylesheet; inline
/// `style="..."` attributes win over both (matching CSS specificity rules).
pub fn compute_styles(dom: &Dom, author: &[ParsedStylesheet], media: &MediaContext) -> StyleMap {
    let mut all_entries: Vec<StyleRuleEntry> = Vec::with_capacity(128);
    all_entries.extend(ua::ua_rules(media));
    for sheet in author {
        all_entries.extend(sheet.rules.iter().cloned());
    }
    let rules = RuleSet::build(all_entries);

    let mut map = StyleMap::default();
    let mut caches = CachesWrap::default();
    let root = dom.document();
    // Flat-tree node set: the light document plus every shadow subtree
    // (WebComponents). Shadow content composes into the light tree at its
    // host; slotted light children keep their light-tree inheritance.
    let mut nodes: Vec<NodeId> = std::iter::once(root).chain(dom.descendants(root)).collect();
    for shadow_root in dom.all_shadow_roots() {
        nodes.extend(dom.descendants(shadow_root));
    }
    let trace = std::env::var("ROWSER_UI_TRACE").is_ok();
    let t0 = std::time::Instant::now();
    for (n, node) in nodes.iter().enumerate() {
        if trace && n % 200 == 0 {
            eprintln!(
                "[cascade] {n}/{} elements, {}ms elapsed",
                nodes.len(),
                t0.elapsed().as_millis()
            );
        }
        if dom.element(*node).is_none() {
            continue;
        }
        let parent_style = dom
            .flat_parent_element(*node)
            .and_then(|p| map.styles.get(&p))
            .cloned()
            .unwrap_or_default();
        // Inline `style=` PLUS legacy presentational attributes (`bgcolor`,
        // `width`, `height`) — the backbone of the table-era web (HN and
        // millions of older pages). Spec order: style= beats attributes, so
        // the attribute-derived declarations come last.
        let mut inline_css = dom.get_attr(*node, "style").unwrap_or_default().to_owned();
        {
            let tag = dom
                .element(*node)
                .map(|e| e.name.local.to_string())
                .unwrap_or_default();
            let mut extra: Vec<String> = Vec::new();
            if let Some(bg) = dom.get_attr(*node, "bgcolor") {
                extra.push(format!("background-color:{bg}"));
            }
            if matches!(
                tag.as_str(),
                "table" | "td" | "th" | "img" | "hr" | "iframe" | "input" | "canvas" | "video"
            ) {
                for (attr, prop) in [("width", "width"), ("height", "height")] {
                    if let Some(value) = dom.get_attr(*node, attr) {
                        let value = value.trim();
                        if value.is_empty() {
                            continue;
                        }
                        // Unitless numbers are pixels; "%" passes through.
                        let css_value = if value.parse::<f32>().is_ok() {
                            format!("{value}px")
                        } else {
                            value.to_owned()
                        };
                        extra.push(format!("{prop}:{css_value}"));
                    }
                }
            }
            if !extra.is_empty() {
                if !inline_css.is_empty() {
                    inline_css.push(';');
                }
                inline_css.push_str(&extra.join(";"));
            }
        }
        let inline = if inline_css.is_empty() {
            None
        } else {
            Some(parse_style_attribute(&inline_css))
        };
        let style = cascade_element(
            dom,
            *node,
            &rules,
            inline.as_ref(),
            &parent_style,
            &mut caches,
        );
        map.styles.insert(*node, style);
    }
    map
}

/// Applies one declaration set to the working style (non-deferred parts).
fn apply_props(style: &mut ComputedStyle, props: &StyleProps, parent: &ComputedStyle) {
    let resolve_ems = |lpa: LengthOrAuto| -> LengthOrAuto {
        match lpa {
            LengthOrAuto::Length(Length::Em(n)) => {
                LengthOrAuto::Length(Length::Px(n * parent.font_size))
            }
            other => other,
        }
    };
    if let Some(bg) = props.background_color {
        style.background_color = bg;
    }
    if let Some(color) = props.color {
        style.color = color;
    }
    if let Some(display) = props.display {
        style.display = display;
    }
    if let Some(position) = props.position {
        style.position = position;
    }
    if let Some(w) = props.width {
        style.width = w;
    }
    if let Some(h) = props.height {
        style.height = h;
    }
    if let Some(v) = props.min_width {
        style.min_width = v;
    }
    if let Some(v) = props.min_height {
        style.min_height = v;
    }
    if let Some(v) = props.max_width {
        style.max_width = v;
    }
    if let Some(v) = props.max_height {
        style.max_height = v;
    }
    if let Some(v) = props.margin_top {
        style.margins.top = resolve_ems(v);
    }
    if let Some(v) = props.margin_right {
        style.margins.right = resolve_ems(v);
    }
    if let Some(v) = props.margin_bottom {
        style.margins.bottom = resolve_ems(v);
    }
    if let Some(v) = props.margin_left {
        style.margins.left = resolve_ems(v);
    }
    if let Some(v) = props.padding_top {
        style.paddings.top = resolve_ems(v);
    }
    if let Some(v) = props.padding_right {
        style.paddings.right = resolve_ems(v);
    }
    if let Some(v) = props.padding_bottom {
        style.paddings.bottom = resolve_ems(v);
    }
    if let Some(v) = props.padding_left {
        style.paddings.left = resolve_ems(v);
    }
    if let Some(mode) = props.font_style {
        style.font_style = mode;
    }
    if let Some(mode) = props.text_align {
        style.text_align = mode;
    }
    if let Some(mode) = props.flex_direction {
        style.flex_direction = mode;
    }
    if let Some(mode) = props.flex_wrap {
        style.flex_wrap = mode;
    }
    if let Some(v) = props.flex_grow {
        style.flex_grow = v;
    }
    if let Some(v) = props.flex_shrink {
        style.flex_shrink = v;
    }
    if let Some(v) = props.flex_basis {
        style.flex_basis = resolve_ems(v);
    }
    if let Some(mode) = props.justify_content {
        style.justify_content = mode;
    }
    if let Some(mode) = props.align_items {
        style.align_items = mode;
    }
    if let Some(mode) = props.align_content {
        style.align_content = mode;
    }
    // Inset + stacking: non-inherited, applied directly.
    if let Some(v) = props.top {
        style.top = resolve_ems(v);
    }
    if let Some(v) = props.right {
        style.right = resolve_ems(v);
    }
    if let Some(v) = props.bottom {
        style.bottom = resolve_ems(v);
    }
    if let Some(v) = props.left {
        style.left = resolve_ems(v);
    }
    if let Some(z) = props.z_index {
        style.z_index = Some(z);
    }
    if let Some(tracks) = &props.grid_template_columns {
        style.grid_template_columns = tracks.clone();
    }
    if let Some(tracks) = &props.grid_template_rows {
        style.grid_template_rows = tracks.clone();
    }
    if let Some(areas) = &props.grid_template_areas {
        style.grid_template_areas = areas.clone();
    }
    if let Some(area) = &props.grid_area {
        style.grid_area = Some(area.clone());
    }
}

fn inherited_from(parent: &ComputedStyle) -> ComputedStyle {
    ComputedStyle {
        color: parent.color,
        font_family: parent.font_family.clone(),
        font_stack: parent.font_stack.clone(),
        font_size: parent.font_size,
        font_weight: parent.font_weight,
        font_style: parent.font_style,
        line_height: parent.line_height,
        text_align: parent.text_align,
        ..ComputedStyle::default()
    }
}

/// The list of (rule props, important props) sources in cascade order.
struct Sources<'a> {
    normal: Vec<&'a StyleProps>,
    important: Vec<&'a StyleProps>,
    inline: Option<&'a StyleProps>,
}

impl Sources<'_> {
    /// Visits every declaration source in CSS cascade order:
    /// normal (by specificity/order), then important, then inline.
    fn for_each<F: FnMut(&StyleProps)>(&self, mut f: F) {
        for props in &self.normal {
            f(props);
        }
        for props in &self.important {
            f(props);
        }
        if let Some(inline) = self.inline {
            f(inline);
        }
    }
}

fn cascade_element(
    dom: &Dom,
    node: NodeId,
    rules: &RuleSet,
    inline: Option<&StyleProps>,
    parent: &ComputedStyle,
    caches: &mut CachesWrap,
) -> ComputedStyle {
    let element = dom.element(node).expect("cascade on non-element");
    let tag = element.name.local.to_string();
    let indices = rules.index.lookup_indices(
        &tag,
        element.id.as_deref(),
        element.classes.iter().map(String::as_str),
    );

    let element_ref = ElementRef::new(dom, node).expect("element");
    let candidates: Vec<usize> = indices
        .into_iter()
        .filter(|&i| {
            let entry = &rules.entries[i];
            rowser_dom::selector::matches_with_caches(&entry.selectors, &element_ref, caches)
        })
        .collect();

    // Sort by (specificity, source order).
    let mut matched: Vec<&StyleRuleEntry> = candidates.iter().map(|&i| &rules.entries[i]).collect();
    matched.sort_by_key(|entry| (entry.specificity, entry.order));

    let sources = Sources {
        normal: matched.iter().map(|e| &e.props).collect(),
        important: matched.iter().map(|e| &e.important).collect(),
        inline,
    };

    // Working style: inherit from the parent, then apply all declarations.
    let mut style = inherited_from(parent);
    sources.for_each(|props| apply_props(&mut style, props, parent));

    // Deferred properties (resolved against the final font size).
    style.font_size = parent.font_size;
    let mut font_size: Option<FontSizeRaw> = None;
    let mut font_weight: Option<FontWeightRaw> = None;
    let mut line_height: Option<LineHeightRaw> = None;
    let mut font_family: Option<Vec<String>> = None;
    sources.for_each(|props| {
        if props.font_size.is_some() {
            font_size = props.font_size;
        }
        if props.font_weight.is_some() {
            font_weight = props.font_weight;
        }
        if props.line_height.is_some() {
            line_height = props.line_height;
        }
        if props.font_family.is_some() {
            font_family = props.font_family.clone();
        }
    });
    if let Some(raw) = font_size {
        style.font_size = resolve_font_size(raw, parent);
    }
    if let Some(raw) = font_weight {
        style.font_weight = resolve_font_weight(raw, parent);
    }
    if let Some(stack) = font_family {
        style.font_family = stack
            .first()
            .cloned()
            .unwrap_or_else(|| "sans-serif".to_owned());
        style.font_stack = stack;
    }
    style.line_height = match line_height {
        Some(raw) => resolve_line_height_raw(raw, style.font_size),
        None => (style.font_size * 1.25).max(1.0),
    };

    // Borders: two-phase (width, then color/style), em resolved against the
    // element's own font size.
    let fs = style.font_size;
    for (slot, side) in [
        (&mut style.borders.top, Side::Top),
        (&mut style.borders.right, Side::Right),
        (&mut style.borders.bottom, Side::Bottom),
        (&mut style.borders.left, Side::Left),
    ] {
        let mut raw: Option<BorderEdgeRaw> = None;
        sources.for_each(|props| {
            let value = match side {
                Side::Top => props.border_top,
                Side::Right => props.border_right,
                Side::Bottom => props.border_bottom,
                Side::Left => props.border_left,
            };
            if let Some(value) = value {
                raw = match (raw, value) {
                    (Some(prev), next) => Some(BorderEdgeRaw {
                        width: next.width,
                        color: next.color.or(prev.color),
                        style: next.style,
                    }),
                    (None, next) => Some(next),
                };
            }
        });
        if let Some(raw) = raw {
            let color = raw
                .color
                .map(|c| {
                    if c == Rgba::CURRENT_COLOR {
                        style.color
                    } else {
                        c
                    }
                })
                .unwrap_or(style.color);
            *slot = BorderInfo {
                width: raw.width.resolve(fs).max(0.0),
                color,
                style: raw.style,
            };
        }
    }

    // Gaps.
    let mut gap_row: Option<Length> = None;
    let mut gap_column: Option<Length> = None;
    sources.for_each(|props| {
        if props.row_gap.is_some() {
            gap_row = props.row_gap;
        }
        if props.column_gap.is_some() {
            gap_column = props.column_gap;
        }
    });
    style.gap_row = gap_row.map(|g| g.resolve(fs)).unwrap_or(0.0).max(0.0);
    style.gap_column = gap_column.map(|g| g.resolve(fs)).unwrap_or(0.0).max(0.0);

    // currentColor fixups.
    if style.background_color == Rgba::CURRENT_COLOR {
        style.background_color = style.color;
    }

    style
}

enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

fn resolve_font_size(raw: FontSizeRaw, parent: &ComputedStyle) -> f32 {
    match raw {
        FontSizeRaw::Px(n) => n,
        FontSizeRaw::Em(n) => n * parent.font_size,
        FontSizeRaw::Rem(n) => n * ROOT_FONT_SIZE,
        FontSizeRaw::Percent(n) => n * parent.font_size,
        FontSizeRaw::Factor(n) => n * parent.font_size,
    }
    .max(1.0)
}

fn resolve_font_weight(raw: FontWeightRaw, parent: &ComputedStyle) -> f32 {
    match raw {
        FontWeightRaw::Weight(n) => n,
        FontWeightRaw::Bolder => (parent.font_weight + 300.0).clamp(100.0, 900.0),
        FontWeightRaw::Lighter => (parent.font_weight - 300.0).clamp(100.0, 900.0),
    }
}

fn resolve_line_height_raw(raw: LineHeightRaw, font_size: f32) -> f32 {
    match raw {
        LineHeightRaw::Normal => font_size * 1.25,
        LineHeightRaw::Number(n) => n * font_size,
        LineHeightRaw::Px(n) => n,
        LineHeightRaw::Em(n) => n * font_size,
        LineHeightRaw::Rem(n) => n * ROOT_FONT_SIZE,
        LineHeightRaw::Percent(n) => n * font_size,
    }
    .max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css::parse_stylesheet;
    use crate::html::parse_html;

    #[test]
    fn cascade_basics() {
        let html = br#"<html><body><p style="color: rgb(255, 0, 0)">hi</p></body></html>"#;
        let doc = parse_html(html);
        let author = parse_stylesheet("p { background-color: #00ff00; }", &MediaContext::default());
        let map = compute_styles(&doc.dom, &[author], &MediaContext::default());
        let body = doc.dom.find_by_id(doc.dom.document(), "nonexistent");
        assert!(body.is_none());
        let p = doc
            .dom
            .subtree_elements(doc.dom.document())
            .find(|n| {
                doc.dom
                    .element(*n)
                    .map(|e| &*e.name.local == "p")
                    .unwrap_or(false)
            })
            .expect("p element");
        let style = map.get(p).expect("p style");
        assert_eq!(style.color, Rgba::new_opaque(255, 0, 0));
        assert_eq!(style.background_color, Rgba::new_opaque(0, 255, 0));
        assert_eq!(style.display, DisplayMode::Block);
    }

    #[test]
    fn inheritance_and_specificity() {
        let html = b"<html><body><div class='a'><em>hi</em></div></body></html>";
        let doc = parse_html(html);
        let author = parse_stylesheet(
            "div.a { color: #101010; } div { color: #ffffff; } body { color: #333333; }",
            &MediaContext::default(),
        );
        let map = compute_styles(&doc.dom, &[author], &MediaContext::default());
        let find = |tag: &str| {
            doc.dom
                .subtree_elements(doc.dom.document())
                .find(|n| {
                    doc.dom
                        .element(*n)
                        .map(|e| &*e.name.local == tag)
                        .unwrap_or(false)
                })
                .unwrap()
        };
        let div = find("div");
        let em = find("em");
        assert_eq!(
            map.get(div).unwrap().color,
            Rgba::new_opaque(0x10, 0x10, 0x10)
        );
        // em inherits the winning color from div.
        assert_eq!(
            map.get(em).unwrap().color,
            Rgba::new_opaque(0x10, 0x10, 0x10)
        );
        assert_eq!(
            map.get(find("body")).unwrap().color,
            Rgba::new_opaque(0x33, 0x33, 0x33)
        );
    }
}
