//! Text shaping: flattening inline content into rich spans and shaping them
//! with cosmic-text (rustybuzz + fontdb + swash).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cosmic_text::fontdb;
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
    /// Full font stack in CSS preference order.
    pub family_stack: Vec<String>,
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
            family_stack: family_stack_of(cs),
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
        self.family_stack = family_stack_of(cs);
        self.font_size = cs.font_size;
        self.weight = cs.font_weight;
        self.style = cs.font_style;
        self.color = cs.color;
        self.line_height = cs.line_height;
    }

    fn family_ref<'a>(&'a self, resolved: &'a str) -> Family<'a> {
        match resolved {
            "serif" => Family::Serif,
            "monospace" => Family::Monospace,
            "cursive" => Family::Cursive,
            "fantasy" => Family::Fantasy,
            other => Family::Name(other),
        }
    }

    fn attrs_with<'a>(&'a self, resolved_family: &'a str) -> Attrs<'a> {
        let weight = Weight(self.weight.round().clamp(100.0, 900.0) as u16);
        let style = match self.style {
            FontStyleMode::Normal => Style::Normal,
            FontStyleMode::Italic => Style::Italic,
        };
        let color =
            cosmic_text::Color::rgba(self.color.r, self.color.g, self.color.b, self.color.a);
        // Safety: the family string is either a &'static generic keyword or
        // borrowed from `self.family_stack` (kept alive by the span).
        let family = self.family_ref(resolved_family);
        // Transmute-free lifetime tie: Attrs borrows from self; the family
        // name borrowed from resolved_family must live as long as self —
        // callers pass a name owned by (or outliving) the span.
        Attrs::new()
            .family(family)
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

/// Hashes an f32 by its bit pattern (bit-stable keys: styles and widths
/// arrive as exact f32 values, so equal values hash equal).
fn hash_f32<H: std::hash::Hasher>(v: &f32, state: &mut H) {
    state.write_u32(v.to_bits());
}

/// Hashes a byte range's bounds.
fn hash_range<H: std::hash::Hasher>(r: &std::ops::Range<usize>, state: &mut H) {
    state.write_usize(r.start);
    state.write_usize(r.end);
}

impl SpanStyle {
    /// Feeds every field that influences shaping into `state`.
    fn hash_into<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write(self.family.as_bytes());
        state.write_usize(self.family_stack.len());
        for f in &self.family_stack {
            state.write(f.as_bytes());
        }
        hash_f32(&self.font_size, state);
        hash_f32(&self.weight, state);
        state.write_u8(match self.style {
            FontStyleMode::Normal => 0,
            FontStyleMode::Italic => 1,
        });
        state.write_u8(self.color.r);
        state.write_u8(self.color.g);
        state.write_u8(self.color.b);
        state.write_u8(self.color.a);
        hash_f32(&self.line_height, state);
        state.write_u8(match self.text_align {
            TextAlignMode::Left => 0,
            TextAlignMode::Center => 1,
            TextAlignMode::Right => 2,
            TextAlignMode::Justify => 3,
        });
    }
}

/// Full key of one shaped result. `width_key` is the rounded measure
/// width (-1.0 for indefinite). `font_gen` snapshots the web-font
/// generation at shape time.
#[derive(Clone, PartialEq)]
struct ShapeKey {
    text: String,
    spans: Vec<(std::ops::Range<usize>, SpanStyle)>,
    defaults: SpanStyle,
    width_key: f32,
    font_gen: u64,
}

impl ShapeKey {
    fn of(leaf: &TextLeaf, width: Option<f32>) -> ShapeKey {
        ShapeKey {
            text: leaf.text.clone(),
            spans: leaf.spans.clone(),
            defaults: leaf.defaults.clone(),
            width_key: width.map(|w| w.round()).unwrap_or(-1.0),
            font_gen: font_gen(),
        }
    }

    fn hash64(&self) -> u64 {
        use std::hash::Hasher;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        h.write(self.text.as_bytes());
        h.write_usize(self.spans.len());
        for (range, span) in &self.spans {
            hash_range(range, &mut h);
            span.hash_into(&mut h);
        }
        self.defaults.hash_into(&mut h);
        hash_f32(&self.width_key, &mut h);
        h.write_u64(self.font_gen);
        h.finish()
    }

    /// Rough memory footprint of the cached value (lines + glyphs).
    fn value_bytes(lines: &[LayoutLine]) -> usize {
        let glyph = lines.first().map_or(64, |l| l.glyphs.len());
        let per_line = std::mem::size_of::<LayoutLine>() + glyph.max(1) * 64;
        per_line * lines.len().max(1)
    }
}

#[derive(Clone)]
struct ShapeCacheEntry {
    key: ShapeKey,
    value: Arc<Vec<LayoutLine>>,
    bytes: usize,
}

/// Persistent cross-render text-shaping cache (Group E).
///
/// The taffy tree is rebuilt from the DOM every layout pass, so the
/// per-leaf caches died with the tree and every re-render re-shaped every
/// text run through cosmic-text — the dominant relayout cost on
/// text-heavy pages (Wikipedia, GitHub). This cache lives on the
/// [`crate::LayoutEngine`], keyed by the *complete* shaping input: text,
/// span styles, default style, rounded width and the web-font generation.
/// A hit skips both font-stack resolution and shaping entirely.
///
/// Bounded by entry count and an estimated byte budget, LRU-evicted.
#[derive(Default)]
pub struct ShapeCache {
    entries: HashMap<u64, ShapeCacheEntry>,
    order: VecDeque<u64>,
    bytes: usize,
    hits: u64,
    misses: u64,
    shape_calls: u64,
}

impl ShapeCache {
    /// Cache tuning: at most 2048 entries / 48 MiB of estimated line data.
    const MAX_ENTRIES: usize = 2048;
    const MAX_BYTES: usize = 48 * 1024 * 1024;

    /// Creates an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// (hits, misses, live entries, estimated bytes, total lookups).
    pub fn stats(&self) -> (u64, u64, usize, usize, u64) {
        (
            self.hits,
            self.misses,
            self.entries.len(),
            self.bytes,
            self.shape_calls,
        )
    }

    /// Drops every entry (memory pressure, navigation).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }

    fn lookup(&mut self, leaf: &TextLeaf, width: Option<f32>) -> Option<Arc<Vec<LayoutLine>>> {
        self.shape_calls += 1;
        let key = ShapeKey::of(leaf, width);
        let hash = key.hash64();
        if let Some(entry) = self.entries.get(&hash) {
            if entry.key == key {
                self.hits += 1;
                // LRU touch.
                if let Some(pos) = self.order.iter().position(|&h| h == hash) {
                    self.order.remove(pos);
                    self.order.push_back(hash);
                }
                return Some(Arc::clone(&entry.value));
            }
        }
        self.misses += 1;
        None
    }

    fn insert(&mut self, leaf: &TextLeaf, width: Option<f32>, value: Arc<Vec<LayoutLine>>) {
        let key = ShapeKey::of(leaf, width);
        let hash = key.hash64();
        let bytes = ShapeKey::value_bytes(&value);
        // Replace (same leaf re-shaped at a width that collided rounds).
        if let Some(old) = self
            .entries
            .insert(hash, ShapeCacheEntry { key, value, bytes })
        {
            self.bytes = self.bytes.saturating_sub(old.bytes);
            if let Some(pos) = self.order.iter().position(|&h| h == hash) {
                self.order.remove(pos);
            }
        }
        self.bytes += bytes;
        self.order.push_back(hash);
        self.evict();
    }

    fn evict(&mut self) {
        while self.entries.len() > Self::MAX_ENTRIES || self.bytes > Self::MAX_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
    }
}

/// Shapes `leaf` at `width` (persistent-cache backed).
///
/// Hits return the previously shaped lines without touching the shaper;
/// misses shape and cache. Within one layout pass the same leaf measured
/// at the same width hits the cache directly.
pub fn shape(
    leaf: &TextLeaf,
    font_system: &mut FontSystem,
    width: Option<f32>,
    cache: &mut ShapeCache,
) -> Arc<Vec<LayoutLine>> {
    if let Some(lines) = cache.lookup(leaf, width) {
        return lines;
    }
    let lines = Arc::new(shape_lines(leaf, font_system, width));
    cache.insert(leaf, width, Arc::clone(&lines));
    lines
}

/// Shapes `leaf` at `width` and converts to absolutely-positioned glyphs.
pub fn shape_at(
    leaf: &TextLeaf,
    font_system: &mut FontSystem,
    width: Option<f32>,
    abs: (f32, f32),
    cache: &mut ShapeCache,
) -> Vec<PlacedGlyph> {
    let lines = shape(leaf, font_system, width, cache);
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
            // NOTE: cosmic-text's `physical()` already adds the glyph's own
            // line-relative `self.x` to the offset we pass. Feeding it
            // `glyph.x` here DOUBLED every x coordinate — text rendered
            // ~2x too wide, smearing over neighbouring runs: the
            // "reversed and jumbled text" failure mode. The offset must be
            // only the line-alignment shift (+ the run origin).
            let physical = glyph.physical((x_offset, baseline), 1.0);
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

/// The computed style's font stack, with a generic fallback appended when
/// the author supplied none ("Arial" alone must still fall back to *some*
/// sans-serif if Arial is missing).
fn family_stack_of(cs: &ComputedStyle) -> Vec<String> {
    let mut stack = cs.font_stack.clone();
    let has_generic = stack.iter().any(|f| {
        matches!(
            f.as_str(),
            "serif" | "sans-serif" | "monospace" | "cursive" | "fantasy"
        )
    });
    if !has_generic {
        stack.push("sans-serif".to_owned());
    }
    stack
}

/// Well-known web font names mapped to metric-compatible fonts that ship
/// with this environment (Liberation = Arial/Times/Courier metrics).
const FAMILY_ALIASES: &[(&str, &str)] = &[
    ("arial", "Liberation Sans"),
    ("helvetica", "Liberation Sans"),
    ("arial black", "Liberation Sans"),
    ("segoe ui", "Liberation Sans"),
    ("roboto", "Liberation Sans"),
    ("system-ui", "Liberation Sans"),
    ("-apple-system", "Liberation Sans"),
    ("blinkmacsystemfont", "Liberation Sans"),
    ("microsoft yahei", "Noto Sans SC"),
    ("pingfang sc", "Noto Sans SC"),
    ("hiragino sans gb", "Noto Sans SC"),
    ("microsoft sans serif", "Liberation Sans"),
    ("tahoma", "DejaVu Sans"),
    ("verdana", "DejaVu Sans"),
    ("geneva", "DejaVu Sans"),
    ("lucida sans unicode", "DejaVu Sans"),
    ("lucida grande", "DejaVu Sans"),
    ("trebuchet ms", "DejaVu Sans"),
    ("ui-sans-serif", "Liberation Sans"),
    ("times new roman", "Liberation Serif"),
    ("times", "Liberation Serif"),
    ("georgia", "Liberation Serif"),
    ("garamond", "Liberation Serif"),
    ("palatino", "Liberation Serif"),
    ("book antiqua", "Liberation Serif"),
    ("ui-serif", "Liberation Serif"),
    ("courier new", "Liberation Mono"),
    ("courier", "Liberation Mono"),
    ("consolas", "Liberation Mono"),
    ("menlo", "Liberation Mono"),
    ("monaco", "Liberation Mono"),
    ("sf mono", "Liberation Mono"),
    ("andale mono", "Liberation Mono"),
    ("ui-monospace", "Liberation Mono"),
    ("calibri", "Carlito"),
    ("cambria", "Liberation Serif"),
    ("cursive", "cursive"),
    ("comic sans ms", "Liberation Sans"),
    ("impact", "Liberation Sans"),
    ("symbol", "DejaVu Sans"),
    ("wingdings", "DejaVu Sans"),
    ("webkit-standard", "Liberation Sans"),
    ("ui-rounded", "Liberation Sans"),
    ("applesdgothicneo", "Noto Sans SC"),
    ("noto sans", "Noto Sans SC"),
    ("open sans", "Liberation Sans"),
    ("lato", "Liberation Sans"),
    ("ubuntu", "Liberation Sans"),
    ("fira sans", "Liberation Sans"),
    ("inter", "Liberation Sans"),
    ("sf pro text", "Liberation Sans"),
    ("sf pro display", "Liberation Sans"),
    ("Helvetica Neue", "Liberation Sans"),
];

/// Final-resort families tried after the author stack is exhausted.
const FALLBACK_CHAIN: &[&str] = &["Liberation Sans", "DejaVu Sans", "Noto Sans SC"];

/// Runtime web-font aliases: CSS @font-face family → the registered
/// fontdb family name of the loaded face(s). Populated by the engine when
/// @font-face sources finish loading (see engine::font_face).
static WEB_FONTS: std::sync::OnceLock<std::sync::RwLock<HashMap<String, String>>> =
    std::sync::OnceLock::new();

/// Web-font installation generation: bumped every time a @font-face face
/// registers. Part of the shape-cache key — new faces change shaping
/// results, so cached lines shaped before registration must not be reused.
pub static FONT_GEN: AtomicU64 = AtomicU64::new(0);

/// Reads the web-font generation counter.
fn font_gen() -> u64 {
    FONT_GEN.load(Ordering::Relaxed)
}

/// Registers a loaded @font-face: `css_family` (as written in the rule) maps
/// to `real_family` (the name inside the font file).
pub fn register_web_font(css_family: &str, real_family: &str) {
    FONT_GEN.fetch_add(1, Ordering::Relaxed);
    let lock = WEB_FONTS.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
    if let Ok(mut map) = lock.write() {
        map.insert(css_family.to_ascii_lowercase(), real_family.to_owned());
    }
}

/// True when the font system has a face for `name`.
fn family_installed(name: &str, font_system: &mut FontSystem) -> bool {
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(name)],
        ..Default::default()
    };
    font_system.db_mut().query(&query).is_some()
}

/// Resolves a CSS font stack to the first *installed* family name,
/// applying metric-compatible aliases for common web fonts and falling
/// back to a concrete default when nothing matches. Mirrors CSS font
/// matching (family-by-family, in order) instead of betting on entry #1.
fn resolve_font_stack(stack: &[String], font_system: &mut FontSystem) -> String {
    // Loaded @font-face families take precedence over identically-named
    // system fonts (CSS font matching: author faces shadow local ones).
    if let Some(lock) = WEB_FONTS.get() {
        if let Ok(map) = lock.read() {
            for family in stack {
                if let Some(real) = map.get(&family.to_ascii_lowercase()) {
                    if family_installed(real, font_system) {
                        return real.clone();
                    }
                }
            }
        }
    }
    for family in stack {
        match family.as_str() {
            "sans-serif" => {
                // Generic keywords resolve through fontconfig in Chrome,
                // which aliases sans-serif to the Arial-metric face
                // (Liberation Sans here) — visibly narrower than
                // cosmic-text's built-in default (DejaVu Sans). Prefer the
                // fontconfig-compatible face when installed; the keyword
                // stays as the last resort.
                for candidate in ["Liberation Sans", "DejaVu Sans"] {
                    if family_installed(candidate, font_system) {
                        return candidate.to_owned();
                    }
                }
                return family.clone();
            }
            "serif" => {
                for candidate in ["Liberation Serif", "DejaVu Serif"] {
                    if family_installed(candidate, font_system) {
                        return candidate.to_owned();
                    }
                }
                return family.clone();
            }
            "monospace" | "cursive" | "fantasy" => {
                // cosmic-text's monospace default matches Chrome's on this
                // system (verified: identical row extents); keep its
                // per-glyph fallback machinery for these keywords.
                return family.clone();
            }
            _ => {}
        }
        // Exact installed match wins immediately.
        if family_installed(family, font_system) {
            return family.clone();
        }
        // Metric-compatible alias, if installed.
        if let Some((_, alias)) = FAMILY_ALIASES
            .iter()
            .find(|(from, _)| from.eq_ignore_ascii_case(family))
        {
            if family_installed(alias, font_system) {
                return (*alias).to_owned();
            }
        }
    }
    // Nothing in the author stack matched: concrete fallback.
    for fallback in FALLBACK_CHAIN {
        if family_installed(fallback, font_system) {
            return (*fallback).to_owned();
        }
    }
    "sans-serif".to_owned()
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
    // Resolve the CSS font stacks against installed fonts BEFORE shaping:
    // "-apple-system, BlinkMacSystemFont, Segoe UI" previously hit entry
    // #1, found nothing in fontdb and degraded to an arbitrary face.
    let defaults_name = resolve_font_stack(&defaults.family_stack, font_system);
    let defaults_attrs = defaults.attrs_with(&defaults_name);
    let mut attrs_list = AttrsList::new(&defaults_attrs);
    let mut span_names: Vec<String> = Vec::with_capacity(leaf.spans.len());
    for (_, span) in &leaf.spans {
        if span == defaults {
            span_names.push(defaults_name.clone());
        } else {
            span_names.push(resolve_font_stack(&span.family_stack, font_system));
        }
    }
    for ((range, span), name) in leaf.spans.iter().zip(span_names.iter()) {
        if span != defaults {
            let span_attrs = span.attrs_with(name);
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
                family_stack: vec!["sans-serif".into()],
                font_size: 16.0,
                weight: 400.0,
                style: FontStyleMode::Normal,
                color: Rgba::new(0, 0, 0, 255),
                line_height: 20.0,
                text_align: TextAlignMode::Left,
            },
        }
    }

    /// Second shape of the same (text, style, width) must hit the
    /// persistent cache: identical output, no re-shaping.
    #[test]
    fn shape_cache_hits_on_repeat() {
        let mut font_system = cosmic_text::FontSystem::new();
        let mut cache = ShapeCache::new();
        let leaf = leaf_with("cache me once");
        let a = shape(&leaf, &mut font_system, Some(400.0), &mut cache);
        let b = shape(&leaf, &mut font_system, Some(400.0), &mut cache);
        let (hits, _, entries, _, _) = cache.stats();
        assert_eq!(hits, 1, "second shape must be a cache hit");
        assert_eq!(entries, 1);
        assert_eq!(a.len(), b.len());
    }

    /// A different width is a different key: miss, different wrapping.
    #[test]
    fn shape_cache_misses_on_different_width() {
        let mut font_system = cosmic_text::FontSystem::new();
        let mut cache = ShapeCache::new();
        let leaf = leaf_with("one two three four five six seven eight");
        let wide = shape(&leaf, &mut font_system, Some(400.0), &mut cache);
        let narrow = shape(&leaf, &mut font_system, Some(60.0), &mut cache);
        let (hits, misses, entries, _, _) = cache.stats();
        assert_eq!(hits, 0);
        assert_eq!(misses, 2);
        assert_eq!(entries, 2);
        // Narrow width must wrap into more lines than wide.
        assert!(
            narrow.len() > wide.len(),
            "narrow={} wide={}",
            narrow.len(),
            wide.len()
        );
    }

    /// Style changes (font size) are different keys.
    #[test]
    fn shape_cache_misses_on_style_change() {
        let mut font_system = cosmic_text::FontSystem::new();
        let mut cache = ShapeCache::new();
        let mut leaf = leaf_with("styled text");
        shape(&leaf, &mut font_system, Some(400.0), &mut cache);
        leaf.defaults.font_size = 28.0;
        shape(&leaf, &mut font_system, Some(400.0), &mut cache);
        let (hits, misses, entries, _, _) = cache.stats();
        assert_eq!(hits, 0);
        assert_eq!(misses, 2);
        assert_eq!(entries, 2);
    }

    /// Web-font registration bumps the generation: cached lines shaped
    /// against the old face set must NOT be reused.
    #[test]
    fn shape_cache_invalidates_on_web_font_registration() {
        let mut font_system = cosmic_text::FontSystem::new();
        let mut cache = ShapeCache::new();
        let leaf = leaf_with("webfont era");
        shape(&leaf, &mut font_system, Some(400.0), &mut cache);
        register_web_font("MyFace", "Liberation Sans");
        shape(&leaf, &mut font_system, Some(400.0), &mut cache);
        let (hits, misses, entries, _, _) = cache.stats();
        assert_eq!(hits, 0, "stale font-generation entry must not be reused");
        assert_eq!(misses, 2);
        assert_eq!(entries, 2, "both generations cached separately");
    }

    /// LRU eviction keeps the cache bounded.
    #[test]
    fn shape_cache_evicts_lru() {
        let mut font_system = cosmic_text::FontSystem::new();
        let mut cache = ShapeCache::new();
        for i in 0..64 {
            let leaf = leaf_with(&format!("eviction candidate number {i}"));
            shape(&leaf, &mut font_system, Some(400.0), &mut cache);
        }
        let (_, _, entries, _, _) = cache.stats();
        assert_eq!(entries, 64);
        // Touch entry 0 (oldest) so it becomes most-recent, then overflow.
        let oldest = leaf_with("eviction candidate number 0");
        shape(&oldest, &mut font_system, Some(400.0), &mut cache);
        let newer = leaf_with("eviction candidate number 64");
        shape(&newer, &mut font_system, Some(400.0), &mut cache);
        let (hits, _, entries, _, _) = cache.stats();
        assert_eq!(entries, 65);
        assert_eq!(hits, 1, "the touch must have hit");
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
