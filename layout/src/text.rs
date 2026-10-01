//! Text shaping: flattening inline content into rich spans and shaping them
//! with cosmic-text (rustybuzz + fontdb + swash).

use std::sync::Arc;

use cosmic_text::{
    Attrs, AttrsList, BufferLine, Family, FontSystem, Hinting, LayoutLine, LineEnding, Metrics,
    Shaping, Style, Weight, Wrap,
};
use rowser_dom::NodeId;
use rowser_parsing::cascade::{ComputedStyle, FontStyleMode, Rgba, TextAlignMode};

use crate::TextLeaf;

/// Style of one span of inline text.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanStyle {
    /// First font family name (or `sans-serif`/`serif`/`monospace`).
    pub family: String,
    /// Font size in px.
    pub font_size: f32,
    /// Font weight (100-900).
    pub weight: f32,
    /// Italic or normal.
    pub style: FontStyleMode,
    /// Text color.
    pub color: Rgba,
    /// Line height in px.
    pub line_height: f32,
    /// Text alignment (from the containing block).
    pub text_align: TextAlignMode,
}

impl SpanStyle {
    /// Builds the span style from a computed style.
    pub fn from_style(cs: &ComputedStyle) -> SpanStyle {
        SpanStyle {
            family: cs.font_family.clone(),
            font_size: cs.font_size,
            weight: cs.font_weight,
            style: cs.font_style,
            color: cs.color,
            line_height: cs.line_height,
            text_align: cs.text_align,
        }
    }

    /// Merges an inline element's style into the ambient span style
    /// (innermost inline wins).
    pub fn merge_from(&mut self, cs: &ComputedStyle) {
        self.family = cs.font_family.clone();
        self.font_size = cs.font_size;
        self.weight = cs.font_weight;
        self.style = cs.font_style;
        self.color = cs.color;
        self.line_height = cs.line_height;
    }

    fn family_ref(&self) -> Family<'_> {
        match self.family.as_str() {
            "serif" => Family::Serif,
            "monospace" => Family::Monospace,
            "cursive" => Family::Cursive,
            "fantasy" => Family::Fantasy,
            other => Family::Name(other),
        }
    }

    fn attrs(&self) -> Attrs<'_> {
        let weight = Weight(self.weight.round().clamp(100.0, 900.0) as u16);
        let style = match self.style {
            FontStyleMode::Normal => Style::Normal,
            FontStyleMode::Italic => Style::Italic,
        };
        let color =
            cosmic_text::Color::rgba(self.color.r, self.color.g, self.color.b, self.color.a);
        Attrs::new()
            .family(self.family_ref())
            .weight(weight)
            .style(style)
            .color(color)
            .metrics(Metrics::new(self.font_size, self.line_height))
    }
}

/// A glyph placed in document coordinates, ready for rasterization.
#[derive(Debug, Clone)]
pub struct PlacedGlyph {
    /// Swash cache key (includes subpixel position).
    pub cache_key: cosmic_text::CacheKey,
    /// X position of the glyph origin.
    pub x: i32,
    /// Y position of the glyph baseline.
    pub y: i32,
    /// Glyph color (from the span).
    pub color: Rgba,
}

/// A shaped text run, positioned in document coordinates.
#[derive(Debug, Clone)]
pub struct TextRun {
    /// The element that owns the text (for hit-testing and events).
    pub node: NodeId,
    /// Glyphs in document coordinates.
    pub glyphs: Vec<PlacedGlyph>,
}

/// Shapes `leaf` at `width` and returns the shaped lines (cached).
pub fn shape(
    leaf: &mut TextLeaf,
    font_system: &mut FontSystem,
    width: Option<f32>,
) -> Arc<Vec<LayoutLine>> {
    let key = width.map(|w| w.round()).unwrap_or(-1.0);
    if let Some((cached_key, lines)) = &leaf.cache {
        if *cached_key == key {
            return Arc::clone(lines);
        }
    }
    let lines = Arc::new(shape_lines(leaf, font_system, width));
    leaf.cache = Some((key, Arc::clone(&lines)));
    lines
}

/// Shapes `leaf` at `width` and converts to absolutely-positioned glyphs.
pub fn shape_at(
    leaf: &TextLeaf,
    font_system: &mut FontSystem,
    width: Option<f32>,
    abs: (f32, f32),
) -> Vec<PlacedGlyph> {
    let key = width.map(|w| w.round()).unwrap_or(-1.0);
    let lines = match &leaf.cache {
        Some((cached_key, lines)) if *cached_key == key => Arc::clone(lines),
        _ => Arc::new(shape_lines(leaf, font_system, width)),
    };
    let mut glyphs = Vec::new();
    let defaults = &leaf.defaults;
    let default_line_h = defaults.line_height;
    let mut y = abs.1;
    for line in lines.iter() {
        let line_h = line.line_height_opt.unwrap_or(default_line_h).max(1.0);
        let baseline = y + line.max_ascent;
        // Per-line alignment offset.
        let avail = width.unwrap_or(line.w).max(0.0);
        let x_offset = match defaults.text_align {
            TextAlignMode::Center => ((avail - line.w) / 2.0).max(0.0),
            TextAlignMode::Right | TextAlignMode::Justify => (avail - line.w).max(0.0),
            _ => 0.0,
        };
        for glyph in &line.glyphs {
            let color = glyph
                .color_opt
                .map(|c| {
                    Rgba::new(
                        ((c.0 >> 16) & 0xff) as u8,
                        ((c.0 >> 8) & 0xff) as u8,
                        (c.0 & 0xff) as u8,
                        (c.0 >> 24) as u8,
                    )
                })
                .unwrap_or(defaults.color);
            let physical = glyph.physical((glyph.x + x_offset, baseline), 1.0);
            glyphs.push(PlacedGlyph {
                cache_key: physical.cache_key,
                x: physical.x + abs.0.round() as i32,
                y: physical.y,
                color,
            });
        }
        y += line_h;
    }
    glyphs
}

fn shape_lines(
    leaf: &TextLeaf,
    font_system: &mut FontSystem,
    width: Option<f32>,
) -> Vec<LayoutLine> {
    if leaf.text.is_empty() {
        return Vec::new();
    }
    let defaults = &leaf.defaults;
    let attrs = defaults.attrs();
    let mut attrs_list = AttrsList::new(&attrs);
    for (range, span) in &leaf.spans {
        if span != defaults {
            let span_attrs = span.attrs();
            attrs_list.add_span(range.clone(), &span_attrs);
        }
    }
    let mut buffer_line = BufferLine::new(
        leaf.text.clone(),
        LineEnding::None,
        attrs_list,
        Shaping::Advanced,
    );
    let layout = buffer_line.layout(
        font_system,
        defaults.font_size,
        width,
        Wrap::Word,
        None,
        4,
        Hinting::Disabled,
    );
    layout.to_vec()
}
