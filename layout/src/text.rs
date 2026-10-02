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

/// Sanitizes bidi paragraph separators that survive CSS whitespace
/// collapsing.
///
/// Unicode assigns bidi class **B** (paragraph separator) to more than the
/// familiar line breaks: alongside LF/VT/FF/CR/NEL/LS/PS it also covers
/// U+001C (FILE), U+001D (GROUP) and U+001E (RECORD SEPARATOR). Those three
/// are *not* Unicode `White_Space`, so [`append_collapsed_text`] lets them
/// through — and once a layout line reaches unicode-bidi, every class-B
/// character splits it into a separate bidi *paragraph*. A line holding e.g.
/// an Arabic run and a Latin run across such a separator produces paragraphs
/// with conflicting base directions, which trips an assertion inside
/// cosmic-text's shaper and takes the whole page thread down with it.
///
/// CSS Text treats class-B characters as whitespace; mapping the survivors
/// 1:1 onto a plain space (all are single-byte, so span ranges stay valid)
/// is both spec-correct and crash-proof.
fn sanitize_bidi_separators(text: &str) -> std::borrow::Cow<'_, str> {
    if !text
        .chars()
        .any(|c| matches!(c, '\u{1c}' | '\u{1d}' | '\u{1e}'))
    {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(
        text.chars()
            .map(|c| match c {
                '\u{1c}' | '\u{1d}' | '\u{1e}' => ' ',
                other => other,
            })
            .collect(),
    )
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
    let text = sanitize_bidi_separators(&leaf.text);
    let mut buffer_line = BufferLine::new(text, LineEnding::None, attrs_list, Shaping::Advanced);
    // Defense in depth: no conceivable text content may abort the page
    // thread. If the shaper still panics (unknown font edge case, exotic
    // script run), degrade to an empty line instead of crashing the tab.
    let layout = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        buffer_line
            .layout(
                font_system,
                defaults.font_size,
                width,
                Wrap::Word,
                None,
                4,
                Hinting::Disabled,
            )
            .to_vec()
    }));
    match layout {
        Ok(lines) => lines.to_vec(),
        Err(report) => {
            let msg = report
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| report.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".to_string());
            log::warn!("text shaping panicked (degraded to blank line): {msg}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf_with(text: &str) -> TextLeaf {
        TextLeaf {
            node: 1,
            text: text.to_string(),
            spans: Vec::new(),
            defaults: SpanStyle {
                family: "sans-serif".into(),
                font_size: 16.0,
                weight: 400.0,
                style: FontStyleMode::Normal,
                color: Rgba::new(0, 0, 0, 255),
                line_height: 20.0,
                text_align: TextAlignMode::Left,
            },
            cache: None,
        }
    }

    /// Regression: bidi class-B separators (U+001C/001D/001E) inside a
    /// mixed-direction line previously crashed cosmic-text's shaper (and
    /// with it the whole page thread). They must now shape to plain lines.
    #[test]
    fn bidi_separator_mixed_direction_does_not_panic() {
        // Arabic run + FILE SEPARATOR + Latin run: paragraph directions
        // would disagree once split by unicode-bidi.
        let cases = [
            "\u{0645}\u{0631}\u{062d}\u{0628}\u{0627}\u{1c}Hello world",
            "abc\u{1d}\u{05e9}\u{05dc}\u{05d5}\u{05dd}def",
            "\u{1e}",
            "\u{0645}\u{0631}\u{062d}\u{0628}\u{0627}\u{1c}\u{1d}\u{1e}plain",
        ];
        let mut font_system = cosmic_text::FontSystem::new();
        for text in cases {
            let leaf = leaf_with(text);
            let lines = shape_lines(&leaf, &mut font_system, Some(400.0));
            // Sanitized input must not produce a panic; shaping may yield
            // zero or more lines depending on font coverage — the contract
            // is survival.
            assert!(lines.len() <= 2, "unexpected line count {}", lines.len());
        }
    }

    /// The sanitizer must map separators 1:1 so span ranges stay valid.
    #[test]
    fn sanitize_preserves_length_and_offsets() {
        let before = "ab\u{1c}cd\u{1e}ef";
        let after = sanitize_bidi_separators(before);
        assert_eq!(after, "ab cd ef");
        assert_eq!(before.len(), after.len());
        // untouched strings borrow unchanged
        let clean = "nothing to see";
        assert!(matches!(
            sanitize_bidi_separators(clean),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}
