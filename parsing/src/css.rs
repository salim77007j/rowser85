//! CSS parsing: stylesheet text → rule list.
//!
//! lightningcss parses the stylesheet (transpiler-grade correctness and
//! speed); each style rule's selectors are serialized and re-parsed with the
//! `selectors` engine bound to our DOM, and its declarations are flattened
//! into [`StyleProps`] — a copy-cheap intermediate that the cascade resolves.

use lightningcss::declaration::DeclarationBlock;
use lightningcss::properties::align::{AlignContent, AlignItems, GapValue, JustifyContent};
use lightningcss::properties::border::{BorderSideWidth, LineStyle};
use lightningcss::properties::display::{Display, DisplayInside, DisplayKeyword, DisplayOutside};
use lightningcss::properties::flex::{FlexDirection as LcFlexDirection, FlexWrap as LcFlexWrap};
use lightningcss::properties::font::{
    AbsoluteFontWeight, FontFamily as LcFontFamily, FontSize as LcFontSize,
    FontStyle as LcFontStyle, FontWeight as LcFontWeight, GenericFontFamily,
};
use lightningcss::properties::position::Position;
use lightningcss::properties::size::{MaxSize, Size};
use lightningcss::properties::text::TextAlign as LcTextAlign;
use lightningcss::properties::Property;
use lightningcss::rules::style::StyleRule;
use lightningcss::rules::CssRule;
use lightningcss::stylesheet::{ParserOptions, PrinterOptions, StyleSheet};
use lightningcss::traits::ToCss;
use lightningcss::values::color::{CssColor, LABColor, RGBA};
use lightningcss::values::length::{
    Length as LcLength, LengthPercentage, LengthPercentageOrAuto, LengthValue,
};
use rowser_dom::{parse_selector_list, SelectorList};

use crate::cascade::{
    AlignItemsMode, BorderEdgeRaw, DisplayMode, FlexDirectionMode, FlexWrapMode, FontSizeRaw,
    FontStyleMode, FontWeightRaw, JustifyContentMode, Length, LengthOrAuto, LineHeightRaw,
    LineStyleMode, PositionMode, Rgba, StyleProps, TextAlignMode,
};

/// One style rule ready for cascade.
#[derive(Debug, Clone)]
pub struct StyleRuleEntry {
    /// Selector source text (kept for the bucket index and debugging).
    pub selector_text: String,
    /// Parsed selector list.
    pub selectors: SelectorList,
    /// Highest (a,b,c)-as-u32 specificity in the list.
    pub specificity: u32,
    /// Flattened declarations (normal origin).
    pub props: StyleProps,
    /// Flattened `!important` declarations (override normal origin).
    pub important: StyleProps,
    /// Source order for stable sorting.
    pub order: u32,
}

/// A parsed stylesheet.
#[derive(Debug, Clone, Default)]
pub struct ParsedStylesheet {
    /// Style rules in source order.
    pub rules: Vec<StyleRuleEntry>,
}

/// Viewport description used to evaluate `@media` rules.
#[derive(Debug, Clone, Copy)]
pub struct MediaContext {
    /// Viewport width in px.
    pub width: f32,
    /// Viewport height in px.
    pub height: f32,
    /// `prefers-color-scheme` value.
    pub dark_mode: bool,
}

impl Default for MediaContext {
    fn default() -> Self {
        MediaContext {
            width: 1280.0,
            height: 800.0,
            dark_mode: false,
        }
    }
}

/// Parses a stylesheet, evaluating media queries against `media`.
pub fn parse_stylesheet(css: &str, media: &MediaContext) -> ParsedStylesheet {
    let mut out = ParsedStylesheet::default();
    let Ok(sheet) = StyleSheet::parse(css, ParserOptions::default()) else {
        return out;
    };
    let mut order = 0u32;
    collect_rules(&sheet.rules.0, media, &mut out, &mut order);
    out
}

fn collect_rules(
    rules: &[CssRule<'_>],
    media: &MediaContext,
    out: &mut ParsedStylesheet,
    order: &mut u32,
) {
    for rule in rules {
        match rule {
            CssRule::Style(style) => collect_style_rule(style, out, order),
            CssRule::Media(media_rule) => {
                if media_matches(&media_rule.query, media) {
                    collect_rules(&media_rule.rules.0, media, out, order);
                }
            }
            CssRule::Supports(supports_rule) => {
                if supports_rule
                    .condition
                    .to_css_string(PrinterOptions::default())
                    .is_ok()
                {
                    collect_rules(&supports_rule.rules.0, media, out, order);
                }
            }
            CssRule::Nesting(nesting) => {
                collect_rules(
                    std::slice::from_ref(&CssRule::Style(nesting.style.clone())),
                    media,
                    out,
                    order,
                );
            }
            CssRule::LayerBlock(layer) => {
                collect_rules(&layer.rules.0, media, out, order);
            }
            _ => {}
        }
    }
}

fn collect_style_rule(rule: &StyleRule<'_>, out: &mut ParsedStylesheet, order: &mut u32) {
    let Ok(selector_text) = rule.selectors.to_css_string(PrinterOptions::default()) else {
        return;
    };
    let Some(selectors) = parse_selector_list(&selector_text) else {
        return;
    };
    let specificity = selectors
        .slice()
        .iter()
        .map(|s| s.specificity())
        .max()
        .unwrap_or(0);
    let mut props = StyleProps::default();
    let mut important = StyleProps::default();
    for decl in &rule.declarations.declarations {
        apply_property(&mut props, decl);
    }
    for decl in &rule.declarations.important_declarations {
        apply_property(&mut important, decl);
    }
    out.rules.push(StyleRuleEntry {
        selector_text,
        selectors,
        specificity,
        props,
        important,
        order: *order,
    });
    *order += 1;
}

/// Inline `style="..."` attribute parsing.
pub fn parse_style_attribute(css: &str) -> StyleProps {
    let mut props = StyleProps::default();
    if let Ok(block) = DeclarationBlock::parse_string(css, ParserOptions::default()) {
        for decl in &block.declarations {
            apply_property(&mut props, decl);
        }
        // `!important` in inline styles behaves identically for our cascade.
        for decl in &block.important_declarations {
            apply_property(&mut props, decl);
        }
    }
    props
}

// ---------------------------------------------------------------------------
// value conversions
// ---------------------------------------------------------------------------

fn convert_length(lp: &LengthPercentage) -> Length {
    match lp {
        LengthPercentage::Dimension(value) => convert_length_value(value),
        LengthPercentage::Percentage(p) => Length::Percent(p.0),
        LengthPercentage::Calc(_) => Length::Px(0.0),
    }
}

fn convert_length_value(value: &LengthValue) -> Length {
    match value {
        LengthValue::Px(n) => Length::Px(*n),
        LengthValue::Em(n) => Length::Em(*n),
        LengthValue::Rem(n) => Length::Rem(*n),
        LengthValue::Cm(n) => Length::Px(n * 96.0 / 2.54),
        LengthValue::Mm(n) => Length::Px(n * 96.0 / 25.4),
        LengthValue::Q(n) => Length::Px(n * 96.0 / 101.6),
        LengthValue::In(n) => Length::Px(n * 96.0),
        LengthValue::Pt(n) => Length::Px(n * 96.0 / 72.0),
        LengthValue::Pc(n) => Length::Px(n * 16.0),
        _ => Length::Px(0.0),
    }
}

fn convert_lpa(value: &LengthPercentageOrAuto) -> LengthOrAuto {
    match value {
        LengthPercentageOrAuto::Auto => LengthOrAuto::Auto,
        LengthPercentageOrAuto::LengthPercentage(lp) => LengthOrAuto::Length(convert_length(lp)),
    }
}

fn convert_size(value: &Size) -> LengthOrAuto {
    match value {
        Size::LengthPercentage(lp) => LengthOrAuto::Length(convert_length(lp)),
        _ => LengthOrAuto::Auto,
    }
}

fn convert_max_size(value: &MaxSize) -> LengthOrAuto {
    match value {
        MaxSize::LengthPercentage(lp) => LengthOrAuto::Length(convert_length(lp)),
        _ => LengthOrAuto::Auto,
    }
}

/// Converts any lightningcss color into an sRGB [`Rgba`].
///
/// OKLab/OKLCH are converted exactly (they are the modern defaults);
/// legacy CIE Lab/LCH and wide-gamut predefined profiles are approximated
/// (documented v1 limitation).
pub fn convert_color(color: &CssColor) -> Rgba {
    match color {
        CssColor::RGBA(rgba) => rgba_to_ours(rgba),
        CssColor::CurrentColor => Rgba::CURRENT_COLOR,
        CssColor::LAB(boxed) => match &**boxed {
            LABColor::LAB(lab) => oklab_to_rgba(lab_to_oklab(lab.l, lab.a, lab.b), lab.alpha),
            LABColor::LCH(lch) => {
                let (l, a, b) = lch_to_lab(lch.l, lch.c, lch.h);
                oklab_to_rgba(lab_to_oklab(l, a, b), lch.alpha)
            }
            LABColor::OKLAB(lab) => oklab_to_rgba((lab.l, lab.a, lab.b), lab.alpha),
            LABColor::OKLCH(lch) => {
                let (l, a, b) = lch_to_lab(lch.l, lch.c, lch.h);
                oklab_to_rgba((l, a, b), lch.alpha)
            }
        },
        CssColor::Predefined(_) | CssColor::Float(_) => Rgba::new_opaque(128, 128, 128),
        CssColor::LightDark(_light, dark) => convert_color(dark),
        CssColor::System(_) => Rgba::new_opaque(0, 0, 0),
    }
}

fn rgba_to_ours(rgba: &RGBA) -> Rgba {
    Rgba::new(rgba.red, rgba.green, rgba.blue, rgba.alpha)
}

fn lab_to_oklab(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    // D50 Lab → approximate OKLab (rough but visually plausible).
    let l_ok = (l / 100.0).powf(0.625);
    (l_ok, a / 100.0, b / 100.0)
}

fn lch_to_lab(l: f32, c: f32, h: f32) -> (f32, f32, f32) {
    let hue = h.to_radians();
    (l, c * hue.cos(), c * hue.sin())
}

fn oklab_to_rgba((l, a, b): (f32, f32, f32), alpha: f32) -> Rgba {
    // OKLab → linear sRGB (Björn Ottosson's matrices).
    let l_ = l + 0.396_337_8 * a + 0.215_803_8 * b;
    let m_ = l - 0.105_561_4 * a - 0.063_854_2 * b;
    let s_ = l - 0.089_484_2 * a - 1.291_485_5 * b;
    let l3 = l_ * l_ * l_;
    let m3 = m_ * m_ * m_;
    let s3 = s_ * s_ * s_;
    let lin_r = 4.076_742 * l3 - 3.307_712 * m3 + 0.230_970 * s3;
    let lin_g = -1.268_438 * l3 + 2.609_757 * m3 - 0.341_319_4 * s3;
    let lin_b = -0.004_196_1 * l3 - 0.703_418 * m3 + 1.707_615 * s3;
    let gamma = |c: f32| {
        if c <= 0.003_130_8 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgba::new(
        byte(gamma(lin_r)),
        byte(gamma(lin_g)),
        byte(gamma(lin_b)),
        (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

fn convert_font_size(value: &LcFontSize) -> FontSizeRaw {
    use lightningcss::properties::font::{AbsoluteFontSize, RelativeFontSize};
    match value {
        LcFontSize::Length(lp) => match convert_length(lp) {
            Length::Px(n) => FontSizeRaw::Px(n),
            Length::Em(n) => FontSizeRaw::Em(n),
            Length::Rem(n) => FontSizeRaw::Rem(n),
            Length::Percent(n) => FontSizeRaw::Percent(n),
        },
        LcFontSize::Absolute(absolute) => {
            let px = match absolute {
                AbsoluteFontSize::XXSmall => 9.0,
                AbsoluteFontSize::XSmall => 10.0,
                AbsoluteFontSize::Small => 13.0,
                AbsoluteFontSize::Medium => 16.0,
                AbsoluteFontSize::Large => 18.0,
                AbsoluteFontSize::XLarge => 24.0,
                AbsoluteFontSize::XXLarge => 32.0,
                AbsoluteFontSize::XXXLarge => 48.0,
            };
            FontSizeRaw::Px(px)
        }
        LcFontSize::Relative(relative) => match relative {
            RelativeFontSize::Larger => FontSizeRaw::Factor(1.2),
            RelativeFontSize::Smaller => FontSizeRaw::Factor(0.833),
        },
    }
}

fn convert_font_weight(value: &LcFontWeight) -> FontWeightRaw {
    match value {
        LcFontWeight::Absolute(absolute) => match absolute {
            AbsoluteFontWeight::Weight(n) => FontWeightRaw::Weight(*n),
            AbsoluteFontWeight::Normal => FontWeightRaw::Weight(400.0),
            AbsoluteFontWeight::Bold => FontWeightRaw::Weight(700.0),
        },
        LcFontWeight::Bolder => FontWeightRaw::Bolder,
        LcFontWeight::Lighter => FontWeightRaw::Lighter,
    }
}

fn convert_font_family(list: &[LcFontFamily<'_>]) -> String {
    match list.first() {
        Some(LcFontFamily::Generic(generic)) => match generic {
            GenericFontFamily::Serif | GenericFontFamily::UISerif => "serif".to_owned(),
            GenericFontFamily::Monospace | GenericFontFamily::UIMonospace => "monospace".to_owned(),
            GenericFontFamily::Cursive => "cursive".to_owned(),
            GenericFontFamily::Fantasy => "fantasy".to_owned(),
            _ => "sans-serif".to_owned(),
        },
        Some(LcFontFamily::FamilyName(name)) => {
            let quoted = name
                .to_css_string(PrinterOptions::default())
                .unwrap_or_else(|_| "sans-serif".to_owned());
            let trimmed = quoted.trim_matches('"');
            if trimmed.is_empty() {
                "sans-serif".to_owned()
            } else {
                trimmed.to_owned()
            }
        }
        None => "sans-serif".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// property application
// ---------------------------------------------------------------------------

fn apply_property(props: &mut StyleProps, property: &Property<'_>) {
    use lightningcss::properties::Property as P;
    match property {
        P::BackgroundColor(value) => props.background_color = Some(convert_color(value)),
        P::Color(value) => props.color = Some(convert_color(value)),
        P::Display(value) => props.display = Some(convert_display(value)),
        P::Width(value) => props.width = Some(convert_size(value)),
        P::Height(value) => props.height = Some(convert_size(value)),
        P::MinWidth(value) => props.min_width = Some(convert_size(value)),
        P::MinHeight(value) => props.min_height = Some(convert_size(value)),
        P::MaxWidth(value) => props.max_width = Some(convert_max_size(value)),
        P::MaxHeight(value) => props.max_height = Some(convert_max_size(value)),
        P::Margin(value) => {
            props.margin_top = Some(convert_lpa(&value.top));
            props.margin_right = Some(convert_lpa(&value.right));
            props.margin_bottom = Some(convert_lpa(&value.bottom));
            props.margin_left = Some(convert_lpa(&value.left));
        }
        P::MarginTop(value) => props.margin_top = Some(convert_lpa(value)),
        P::MarginRight(value) => props.margin_right = Some(convert_lpa(value)),
        P::MarginBottom(value) => props.margin_bottom = Some(convert_lpa(value)),
        P::MarginLeft(value) => props.margin_left = Some(convert_lpa(value)),
        P::Padding(value) => {
            props.padding_top = Some(convert_lpa(&value.top));
            props.padding_right = Some(convert_lpa(&value.right));
            props.padding_bottom = Some(convert_lpa(&value.bottom));
            props.padding_left = Some(convert_lpa(&value.left));
        }
        P::PaddingTop(value) => props.padding_top = Some(convert_lpa(value)),
        P::PaddingRight(value) => props.padding_right = Some(convert_lpa(value)),
        P::PaddingBottom(value) => props.padding_bottom = Some(convert_lpa(value)),
        P::PaddingLeft(value) => props.padding_left = Some(convert_lpa(value)),
        P::BorderTopWidth(value) => props.border_top = Some(border_from_width(value)),
        P::BorderRightWidth(value) => props.border_right = Some(border_from_width(value)),
        P::BorderBottomWidth(value) => props.border_bottom = Some(border_from_width(value)),
        P::BorderLeftWidth(value) => props.border_left = Some(border_from_width(value)),
        P::BorderTopColor(value) => set_border_color(props, Side::Top, convert_color(value)),
        P::BorderRightColor(value) => set_border_color(props, Side::Right, convert_color(value)),
        P::BorderBottomColor(value) => set_border_color(props, Side::Bottom, convert_color(value)),
        P::BorderLeftColor(value) => set_border_color(props, Side::Left, convert_color(value)),
        P::BorderTopStyle(value) => set_border_style(props, Side::Top, convert_line_style(value)),
        P::BorderRightStyle(value) => {
            set_border_style(props, Side::Right, convert_line_style(value))
        }
        P::BorderBottomStyle(value) => {
            set_border_style(props, Side::Bottom, convert_line_style(value))
        }
        P::BorderLeftStyle(value) => set_border_style(props, Side::Left, convert_line_style(value)),
        P::FontFamily(value) => props.font_family = Some(convert_font_family(value)),
        P::FontSize(value) => props.font_size = Some(convert_font_size(value)),
        P::FontWeight(value) => props.font_weight = Some(convert_font_weight(value)),
        P::FontStyle(value) => props.font_style = Some(convert_font_style(value)),
        P::LineHeight(value) => props.line_height = Some(convert_line_height(value)),
        P::TextAlign(value) => props.text_align = Some(convert_text_align(value)),
        P::Flex(value, _) => {
            props.flex_grow = Some(value.grow);
            props.flex_shrink = Some(value.shrink);
            props.flex_basis = Some(convert_lpa(&value.basis));
        }
        P::FlexDirection(value, _) => props.flex_direction = Some(convert_flex_direction(value)),
        P::FlexWrap(value, _) => props.flex_wrap = Some(convert_flex_wrap(value)),
        P::FlexGrow(value, _) => props.flex_grow = Some(*value),
        P::FlexShrink(value, _) => props.flex_shrink = Some(*value),
        P::FlexBasis(value, _) => props.flex_basis = Some(convert_lpa(value)),
        P::JustifyContent(value, _) => props.justify_content = Some(convert_justify(value)),
        P::AlignItems(value, _) => props.align_items = Some(convert_align_items(value)),
        P::AlignContent(value, _) => props.align_content = Some(convert_align_content(value)),
        P::Gap(value) => {
            props.row_gap = Some(convert_gap(&value.row));
            props.column_gap = Some(convert_gap(&value.column));
        }
        P::RowGap(value) => props.row_gap = Some(convert_gap(value)),
        P::ColumnGap(value) => props.column_gap = Some(convert_gap(value)),
        P::Position(value) => props.position = Some(convert_position(value)),
        _ => {}
    }
}

enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

fn border_from_width(value: &BorderSideWidth) -> BorderEdgeRaw {
    let width = match value {
        BorderSideWidth::Length(LcLength::Value(v)) => convert_length_value(v),
        BorderSideWidth::Length(_) => Length::Px(0.0),
        BorderSideWidth::Thin => Length::Px(1.0),
        BorderSideWidth::Medium => Length::Px(3.0),
        BorderSideWidth::Thick => Length::Px(5.0),
    };
    BorderEdgeRaw {
        width,
        color: None,
        style: LineStyleMode::Solid,
    }
}

fn set_border_color(props: &mut StyleProps, side: Side, color: Rgba) {
    let slot = match side {
        Side::Top => &mut props.border_top,
        Side::Right => &mut props.border_right,
        Side::Bottom => &mut props.border_bottom,
        Side::Left => &mut props.border_left,
    };
    match slot {
        Some(info) => info.color = Some(color),
        None => {
            *slot = Some(BorderEdgeRaw {
                width: BorderEdgeRaw::DEFAULT_WIDTH,
                color: Some(color),
                style: LineStyleMode::Solid,
            })
        }
    }
}

fn set_border_style(props: &mut StyleProps, side: Side, style: LineStyleMode) {
    let slot = match side {
        Side::Top => &mut props.border_top,
        Side::Right => &mut props.border_right,
        Side::Bottom => &mut props.border_bottom,
        Side::Left => &mut props.border_left,
    };
    match slot {
        Some(info) => info.style = style,
        None => {
            *slot = Some(BorderEdgeRaw {
                width: BorderEdgeRaw::DEFAULT_WIDTH,
                color: None,
                style,
            })
        }
    }
}

fn convert_line_style(value: &LineStyle) -> LineStyleMode {
    match value {
        LineStyle::None | LineStyle::Hidden => LineStyleMode::None,
        LineStyle::Dotted | LineStyle::Dashed => LineStyleMode::Dashed,
        _ => LineStyleMode::Solid,
    }
}

fn convert_display(value: &Display) -> DisplayMode {
    match value {
        Display::Keyword(keyword) => match keyword {
            DisplayKeyword::None => DisplayMode::None,
            DisplayKeyword::Contents => DisplayMode::Inline,
            _ => DisplayMode::Block,
        },
        Display::Pair(pair) => match pair.inside {
            DisplayInside::Flex(_) => DisplayMode::Flex,
            DisplayInside::Grid => DisplayMode::Grid,
            DisplayInside::Box(_) => DisplayMode::Flex,
            _ => match pair.outside {
                DisplayOutside::Inline => DisplayMode::Inline,
                _ => DisplayMode::Block,
            },
        },
    }
}

fn convert_font_style(value: &LcFontStyle) -> FontStyleMode {
    match value {
        LcFontStyle::Normal => FontStyleMode::Normal,
        LcFontStyle::Italic => FontStyleMode::Italic,
        LcFontStyle::Oblique(_) => FontStyleMode::Italic,
    }
}

fn convert_line_height(value: &lightningcss::properties::font::LineHeight) -> LineHeightRaw {
    use lightningcss::properties::font::LineHeight as L;
    match value {
        L::Normal => LineHeightRaw::Normal,
        L::Number(n) => LineHeightRaw::Number(*n),
        L::Length(lp) => match convert_length(lp) {
            Length::Px(n) => LineHeightRaw::Px(n),
            Length::Em(n) => LineHeightRaw::Em(n),
            Length::Rem(n) => LineHeightRaw::Rem(n),
            Length::Percent(n) => LineHeightRaw::Percent(n),
        },
    }
}

fn convert_text_align(value: &LcTextAlign) -> TextAlignMode {
    match value {
        LcTextAlign::Left | LcTextAlign::Start => TextAlignMode::Left,
        LcTextAlign::Right | LcTextAlign::End => TextAlignMode::Right,
        LcTextAlign::Center => TextAlignMode::Center,
        LcTextAlign::Justify => TextAlignMode::Justify,
        _ => TextAlignMode::Left,
    }
}

fn convert_flex_direction(value: &LcFlexDirection) -> FlexDirectionMode {
    match value {
        LcFlexDirection::Row => FlexDirectionMode::Row,
        LcFlexDirection::RowReverse => FlexDirectionMode::RowReverse,
        LcFlexDirection::Column => FlexDirectionMode::Column,
        LcFlexDirection::ColumnReverse => FlexDirectionMode::ColumnReverse,
    }
}

fn convert_flex_wrap(value: &LcFlexWrap) -> FlexWrapMode {
    match value {
        LcFlexWrap::Wrap => FlexWrapMode::Wrap,
        LcFlexWrap::WrapReverse => FlexWrapMode::WrapReverse,
        LcFlexWrap::NoWrap => FlexWrapMode::NoWrap,
    }
}

fn convert_justify(value: &JustifyContent) -> JustifyContentMode {
    use lightningcss::properties::align::{ContentDistribution, ContentPosition};
    match value {
        JustifyContent::ContentDistribution(dist) => match dist {
            ContentDistribution::SpaceBetween => JustifyContentMode::SpaceBetween,
            ContentDistribution::SpaceAround => JustifyContentMode::SpaceAround,
            ContentDistribution::SpaceEvenly => JustifyContentMode::SpaceEvenly,
            ContentDistribution::Stretch => JustifyContentMode::Start,
        },
        JustifyContent::ContentPosition { value: pos, .. } => match pos {
            ContentPosition::Center => JustifyContentMode::Center,
            ContentPosition::End | ContentPosition::FlexEnd => JustifyContentMode::End,
            _ => JustifyContentMode::Start,
        },
        JustifyContent::Left { .. } => JustifyContentMode::Start,
        JustifyContent::Right { .. } => JustifyContentMode::End,
        _ => JustifyContentMode::Start,
    }
}

fn convert_align_items(value: &AlignItems) -> AlignItemsMode {
    use lightningcss::properties::align::SelfPosition;
    match value {
        AlignItems::Stretch | AlignItems::Normal => AlignItemsMode::Stretch,
        AlignItems::SelfPosition { value: pos, .. } => match pos {
            SelfPosition::Center => AlignItemsMode::Center,
            SelfPosition::End | SelfPosition::SelfEnd | SelfPosition::FlexEnd => {
                AlignItemsMode::End
            }
            _ => AlignItemsMode::Start,
        },
        _ => AlignItemsMode::Stretch,
    }
}

fn convert_align_content(value: &AlignContent) -> AlignItemsMode {
    use lightningcss::properties::align::{ContentDistribution, ContentPosition};
    match value {
        AlignContent::ContentDistribution(dist) => match dist {
            ContentDistribution::SpaceBetween => AlignItemsMode::SpaceBetween,
            ContentDistribution::SpaceAround => AlignItemsMode::SpaceAround,
            ContentDistribution::SpaceEvenly => AlignItemsMode::SpaceEvenly,
            ContentDistribution::Stretch => AlignItemsMode::Stretch,
        },
        AlignContent::ContentPosition { value: pos, .. } => match pos {
            ContentPosition::Center => AlignItemsMode::Center,
            ContentPosition::End | ContentPosition::FlexEnd => AlignItemsMode::End,
            _ => AlignItemsMode::Start,
        },
        _ => AlignItemsMode::Stretch,
    }
}

fn convert_gap(value: &GapValue) -> Length {
    match value {
        GapValue::Normal => Length::Px(0.0),
        GapValue::LengthPercentage(lp) => convert_length(lp),
    }
}

fn convert_position(value: &Position) -> PositionMode {
    match value {
        Position::Static => PositionMode::Static,
        Position::Relative => PositionMode::Relative,
        Position::Absolute | Position::Fixed => PositionMode::Absolute,
        Position::Sticky(_) => PositionMode::Relative,
    }
}

/// Structured `@media` evaluation against the viewport.
///
/// lightningcss normalizes BOTH syntaxes to one AST: `min-width: 300px`
/// becomes `Range { Width, >=, 300px }` and `(width >= 300px)` parses to the
/// same. The previous string-based matcher silently passed the range syntax
/// (serialized as `width >= 300px` — no `min-width`/`max-width` keyword, no
/// colon), so EVERY media query evaluated true and desktop pages received
/// mobile stylesheet blocks (`td { height: inherit }`, `#hnmain { width: 100% }`
/// — the full-viewport HN explosion).
fn media_matches(query: &lightningcss::media_query::MediaList<'_>, ctx: &MediaContext) -> bool {
    if query.media_queries.is_empty() {
        return true;
    }
    query
        .media_queries
        .iter()
        .any(|q| media_query_matches(q, ctx))
}

fn media_query_matches(q: &lightningcss::media_query::MediaQuery<'_>, ctx: &MediaContext) -> bool {
    use lightningcss::media_query::{MediaType, Qualifier};
    let type_ok = match &q.media_type {
        MediaType::All | MediaType::Screen => true,
        // Print, speech, custom types: this is a screen browser.
        _ => false,
    };
    let qualifier_not = matches!(q.qualifier, Some(Qualifier::Not));
    let base = match &q.condition {
        None => type_ok,
        Some(condition) => type_ok && condition_matches(condition, ctx),
    };
    base != qualifier_not
}

fn condition_matches(
    condition: &lightningcss::media_query::MediaCondition<'_>,
    ctx: &MediaContext,
) -> bool {
    use lightningcss::media_query::{MediaCondition, Operator};
    match condition {
        MediaCondition::Feature(feature) => feature_matches(feature, ctx),
        MediaCondition::Not(inner) => !condition_matches(inner, ctx),
        MediaCondition::Operation {
            operator,
            conditions,
        } => match operator {
            Operator::And => conditions.iter().all(|c| condition_matches(c, ctx)),
            Operator::Or => conditions.iter().any(|c| condition_matches(c, ctx)),
        },
        MediaCondition::Unknown(_) => false,
    }
}

/// The viewport value a length-valued feature compares against, if known.
fn feature_viewport_value(
    name: &lightningcss::media_query::MediaFeatureName<
        '_,
        lightningcss::media_query::MediaFeatureId,
    >,
    ctx: &MediaContext,
) -> Option<f32> {
    use lightningcss::media_query::{MediaFeatureId, MediaFeatureName};
    match name {
        MediaFeatureName::Standard(MediaFeatureId::Width)
        | MediaFeatureName::Standard(MediaFeatureId::DeviceWidth) => Some(ctx.width),
        MediaFeatureName::Standard(MediaFeatureId::Height)
        | MediaFeatureName::Standard(MediaFeatureId::DeviceHeight) => Some(ctx.height),
        _ => None,
    }
}

fn media_length(value: &lightningcss::media_query::MediaFeatureValue<'_>) -> Option<f32> {
    use lightningcss::media_query::MediaFeatureValue;
    match value {
        MediaFeatureValue::Length(lightningcss::values::length::Length::Value(v)) => {
            match convert_length_value(v) {
                crate::css::Length::Px(px) => Some(px),
                _ => None,
            }
        }
        _ => None,
    }
}

fn feature_matches(
    feature: &lightningcss::media_query::MediaFeature<'_>,
    ctx: &MediaContext,
) -> bool {
    use lightningcss::media_query::{
        MediaFeature, MediaFeatureComparison, MediaFeatureId, MediaFeatureName, MediaFeatureValue,
    };
    match feature {
        MediaFeature::Plain { name, value } => match name {
            MediaFeatureName::Standard(MediaFeatureId::Orientation) => {
                let landscape = ctx.width >= ctx.height;
                match value {
                    MediaFeatureValue::Ident(id) => {
                        (id.eq_ignore_ascii_case("landscape") && landscape)
                            || (id.eq_ignore_ascii_case("portrait") && !landscape)
                    }
                    _ => false,
                }
            }
            MediaFeatureName::Standard(MediaFeatureId::PrefersColorScheme) => match value {
                MediaFeatureValue::Ident(id) => id.eq_ignore_ascii_case("dark") == ctx.dark_mode,
                _ => false,
            },
            _ => false,
        },
        MediaFeature::Boolean { name } => match name {
            // Desktop-class environment: hover available.
            MediaFeatureName::Standard(MediaFeatureId::Hover) => true,
            _ => false,
        },
        MediaFeature::Range {
            name,
            operator,
            value,
        } => {
            let Some(actual) = feature_viewport_value(name, ctx) else {
                return false;
            };
            let Some(v) = media_length(value) else {
                return false;
            };
            match operator {
                MediaFeatureComparison::Equal => actual == v,
                MediaFeatureComparison::GreaterThan => actual > v,
                MediaFeatureComparison::GreaterThanEqual => actual >= v,
                MediaFeatureComparison::LessThan => actual < v,
                MediaFeatureComparison::LessThanEqual => actual <= v,
            }
        }
        MediaFeature::Interval {
            name,
            start,
            start_operator,
            end,
            end_operator,
        } => {
            let Some(actual) = feature_viewport_value(name, ctx) else {
                return false;
            };
            let Some(s) = media_length(start) else {
                return false;
            };
            let Some(e) = media_length(end) else {
                return false;
            };
            let start_ok = match start_operator {
                MediaFeatureComparison::Equal => actual == s,
                MediaFeatureComparison::GreaterThan => actual > s,
                MediaFeatureComparison::GreaterThanEqual => actual >= s,
                MediaFeatureComparison::LessThan => actual < s,
                MediaFeatureComparison::LessThanEqual => actual <= s,
            };
            let end_ok = match end_operator {
                MediaFeatureComparison::Equal => actual == e,
                MediaFeatureComparison::GreaterThan => actual > e,
                MediaFeatureComparison::GreaterThanEqual => actual >= e,
                MediaFeatureComparison::LessThan => actual < e,
                MediaFeatureComparison::LessThanEqual => actual <= e,
            };
            start_ok && end_ok
        }
    }
}
