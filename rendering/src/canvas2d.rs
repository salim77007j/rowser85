//! Canvas 2D — the HTML `CanvasRenderingContext2D` state machine on
//! tiny-skia (Group D).
//!
//! Design:
//!
//! * One [`Canvas2D`] per `<canvas>` element, owned by the page-thread
//!   canvas registry ([`CanvasRegistryShared`]) and shared into the JS
//!   bridge (drawing calls) and the engine paint path (snapshot into the
//!   display-list image map).
//! * Coordinates are *user space*; the CTM is applied at draw time via
//!   tiny-skia's path/pixmap transform parameter. Gradient and pattern
//!   shaders are defined in user space too, so they follow the CTM exactly
//!   like the geometry (tiny-skia evaluates shaders in pre-transform
//!   space).
//! * The backing [`tiny_skia::Pixmap`] stores premultiplied RGBA; ImageData
//!   round-trips convert straight <-> premultiplied like Chrome.
//! * Composite operations map onto [`tiny_skia::BlendMode`] (Porter-Duff +
//!   separable blend modes — the set Chrome supports).
//! * Shadows render as: shape silhouette in shadow color -> 3-pass box
//!   blur (Gaussian approximation, the same kernel as the painter's
//!   box-shadow) -> offset composite, then the real shape on top.
//! * Dashing/stroking uses tiny-skia's native `Path::dash` + stroke
//!   conversion. `lineDashOffset` is applied by rotating the dash array.
//! * Text shapes via cosmic-text into a transparent temp pixmap that is
//!   composited through the CTM — full affine text (rotate/scale/skew).
//!   Gradient/pattern text fills sample the shader per glyph (documented
//!   approximation). `strokeText` approximates with an 8-direction
//!   outline pass.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tiny_skia::{
    BlendMode, FillRule, FilterQuality, LineCap, LineJoin, Mask, Paint, Path, PathBuilder, Pixmap,
    PixmapPaint, Shader, SpreadMode, Stroke, StrokeDash, Transform,
};

use crate::DecodedImage;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Style};

// ---------------------------------------------------------------------------
// Shared registry
// ---------------------------------------------------------------------------

/// Shared canvas registry: element handle -> live canvas surface. The
/// registry lives on the page thread (like the DOM `Rc`), so `Rc` is
/// correct and avoids `Arc<RefCell>` clippy::arc_with_non_send_sync.
pub type CanvasRegistryShared = Rc<RefCell<HashMap<u32, Rc<RefCell<Canvas2D>>>>>;

/// Creates an empty registry.
pub fn new_registry() -> CanvasRegistryShared {
    Rc::new(RefCell::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Style values
// ---------------------------------------------------------------------------

/// A parsed CSS color (`#rgb`, `rgb()`, `rgba()`, named, `transparent`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const BLACK: Color = Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    pub const TRANSPARENT: Color = Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };

    fn skia(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba(self.r, self.g, self.b, self.a)
            .unwrap_or(tiny_skia::Color::BLACK)
    }
}

/// Parses a CSS color string the canvas way.
pub fn parse_color(s: &str) -> Option<Color> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let named = |hex: u32| -> Option<Color> {
        Some(Color {
            r: ((hex >> 16) & 0xff) as f32 / 255.0,
            g: ((hex >> 8) & 0xff) as f32 / 255.0,
            b: (hex & 0xff) as f32 / 255.0,
            a: 1.0,
        })
    };
    if let Some(rest) = lower.strip_prefix('#') {
        let chan =
            |i: usize, n: usize| -> Option<u8> { u8::from_str_radix(rest.get(i..i + n)?, 16).ok() };
        let (r, g, b, a) = match rest.len() {
            3 | 4 => {
                let r = chan(0, 1)?.checked_mul(17)?;
                let g = chan(1, 1)?.checked_mul(17)?;
                let b = chan(2, 1)?.checked_mul(17)?;
                let a = if rest.len() == 4 {
                    chan(3, 1)?.checked_mul(17)?
                } else {
                    255
                };
                (r, g, b, a)
            }
            6 | 8 => {
                let r = chan(0, 2)?;
                let g = chan(2, 2)?;
                let b = chan(4, 2)?;
                let a = if rest.len() == 8 { chan(6, 2)? } else { 255 };
                (r, g, b, a)
            }
            _ => return None,
        };
        return Some(Color {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: a as f32 / 255.0,
        });
    }
    let num = |v: &str| -> Option<f32> {
        let v = v.trim();
        if let Some(p) = v.strip_suffix('%') {
            p.parse::<f32>().ok().map(|n| (n / 100.0).clamp(0.0, 1.0))
        } else {
            v.parse::<f32>().ok().map(|n| (n / 255.0).clamp(0.0, 1.0))
        }
    };
    if let Some(body) = lower.strip_prefix("rgb(").and_then(|b| b.strip_suffix(')')) {
        let parts: Vec<&str> = body.split(',').collect();
        if parts.len() < 3 {
            return None;
        }
        return Some(Color {
            r: num(parts[0])?,
            g: num(parts[1])?,
            b: num(parts[2])?,
            a: 1.0,
        });
    }
    if let Some(body) = lower
        .strip_prefix("rgba(")
        .and_then(|b| b.strip_suffix(')'))
    {
        let parts: Vec<&str> = body.split(',').collect();
        if parts.len() < 4 {
            return None;
        }
        let a = parts[3]
            .trim()
            .strip_suffix('%')
            .and_then(|p| p.parse::<f32>().ok().map(|n| n / 100.0))
            .or_else(|| parts[3].trim().parse::<f32>().ok())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        return Some(Color {
            r: num(parts[0])?,
            g: num(parts[1])?,
            b: num(parts[2])?,
            a,
        });
    }
    match lower.as_str() {
        "transparent" => Some(Color::TRANSPARENT),
        "black" => Some(Color::BLACK),
        "white" => named(0xffffff),
        "red" => named(0xff0000),
        "lime" | "green" => named(0x00ff00),
        "blue" => named(0x0000ff),
        "yellow" => named(0xffff00),
        "cyan" | "aqua" => named(0x00ffff),
        "magenta" | "fuchsia" => named(0xff00ff),
        "gray" | "grey" => named(0x808080),
        "silver" => named(0xc0c0c0),
        "maroon" => named(0x800000),
        "olive" => named(0x808000),
        "navy" => named(0x000080),
        "teal" => named(0x008080),
        "purple" => named(0x800080),
        "orange" => named(0xffa500),
        "pink" => named(0xffc0cb),
        "brown" => named(0xa52a2a),
        "gold" => named(0xffd700),
        _ => None,
    }
}

/// Gradient definition (linear / radial).
#[derive(Debug, Clone, Copy)]
pub enum GradientDef {
    Linear {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
    },
    Radial {
        x0: f32,
        y0: f32,
        r0: f32,
        x1: f32,
        y1: f32,
        r1: f32,
    },
}

/// One gradient color stop.
#[derive(Debug, Clone, Copy)]
pub struct Stop {
    pub offset: f32,
    pub color: Color,
}

/// Fill/stroke source.
#[derive(Clone)]
pub enum PaintSource {
    Color(Color),
    Gradient {
        def: GradientDef,
        stops: Vec<Stop>,
    },
    Pattern {
        pixmap: Rc<Pixmap>,
        repeat: PatternRepeat,
    },
}

/// Pattern repetition mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternRepeat {
    Repeat,
    RepeatX,
    RepeatY,
    NoRepeat,
}

/// Composite operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeOp {
    SourceOver,
    SourceIn,
    SourceOut,
    SourceAtop,
    DestinationOver,
    DestinationIn,
    DestinationOut,
    DestinationAtop,
    Copy,
    Xor,
    Lighter,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl CompositeOp {
    /// Parses the `globalCompositeOperation` string.
    pub fn parse(s: &str) -> Option<CompositeOp> {
        use CompositeOp::*;
        Some(match s {
            "source-over" => SourceOver,
            "source-in" => SourceIn,
            "source-out" => SourceOut,
            "source-atop" => SourceAtop,
            "destination-over" => DestinationOver,
            "destination-in" => DestinationIn,
            "destination-out" => DestinationOut,
            "destination-atop" => DestinationAtop,
            "copy" => Copy,
            "xor" => Xor,
            "lighter" => Lighter,
            "multiply" => Multiply,
            "screen" => Screen,
            "overlay" => Overlay,
            "darken" => Darken,
            "lighten" => Lighten,
            "color-dodge" => ColorDodge,
            "color-burn" => ColorBurn,
            "hard-light" => HardLight,
            "soft-light" => SoftLight,
            "difference" => Difference,
            "exclusion" => Exclusion,
            "hue" => Hue,
            "saturation" => Saturation,
            "color" => Color,
            "luminosity" => Luminosity,
            _ => return None,
        })
    }

    fn skia(self) -> BlendMode {
        use CompositeOp::*;
        match self {
            SourceOver => BlendMode::SourceOver,
            SourceIn => BlendMode::SourceIn,
            SourceOut => BlendMode::SourceOut,
            SourceAtop => BlendMode::SourceAtop,
            DestinationOver => BlendMode::DestinationOver,
            DestinationIn => BlendMode::DestinationIn,
            DestinationOut => BlendMode::DestinationOut,
            DestinationAtop => BlendMode::DestinationAtop,
            Copy => BlendMode::Source,
            Xor => BlendMode::Xor,
            Lighter => BlendMode::Plus,
            Multiply => BlendMode::Multiply,
            Screen => BlendMode::Screen,
            Overlay => BlendMode::Overlay,
            Darken => BlendMode::Darken,
            Lighten => BlendMode::Lighten,
            ColorDodge => BlendMode::ColorDodge,
            ColorBurn => BlendMode::ColorBurn,
            HardLight => BlendMode::HardLight,
            SoftLight => BlendMode::SoftLight,
            Difference => BlendMode::Difference,
            Exclusion => BlendMode::Exclusion,
            Hue => BlendMode::Hue,
            Saturation => BlendMode::Saturation,
            Color => BlendMode::Color,
            Luminosity => BlendMode::Luminosity,
        }
    }
}

/// Line cap style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Butt,
    Round,
    Square,
}

/// Line join style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    Bevel,
    Miter,
    Round,
}

/// `textAlign`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Start,
    End,
    Left,
    Center,
    Right,
}

/// `textBaseline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Baseline {
    Top,
    Hanging,
    Middle,
    Alphabetic,
    Ideographic,
    Bottom,
}

/// Parsed `font` shorthand.
#[derive(Debug, Clone)]
pub struct FontSpec {
    pub size: f32,
    pub family: String,
    pub weight: u16,
    pub italic: bool,
}

impl Default for FontSpec {
    fn default() -> Self {
        FontSpec {
            size: 10.0,
            family: "sans-serif".into(),
            weight: 400,
            italic: false,
        }
    }
}

/// Parses a CSS font shorthand: `[style] [weight] <size>[/<line-height>] <family>`.
pub fn parse_font(s: &str) -> Option<FontSpec> {
    let mut size = 10.0f32;
    let mut weight = 400u16;
    let mut italic = false;
    let mut seen_size = false;
    let mut family = String::new();
    let tokens: Vec<String> = s.split_whitespace().map(str::to_owned).collect();
    let mut i = 0;
    'outer: while i < tokens.len() {
        let t = tokens[i].to_ascii_lowercase();
        match t.as_str() {
            "italic" | "oblique" => {
                italic = true;
                i += 1;
            }
            "normal" | "small-caps" => {
                i += 1;
            }
            "bold" => {
                weight = 700;
                i += 1;
            }
            "bolder" => {
                weight = 900;
                i += 1;
            }
            "lighter" => {
                weight = 300;
                i += 1;
            }
            "system-ui" | "sans-serif" | "serif" | "monospace" | "cursive" | "fantasy"
            | "ui-sans-serif" | "ui-monospace" => {
                family.push_str(&tokens[i]);
                i += 1;
                for f in &tokens[i..] {
                    family.push(' ');
                    family.push_str(f);
                }
                break 'outer;
            }
            _ => {
                // Size may carry a line-height: "16px/1.5".
                let size_part = t.split('/').next().unwrap_or(&t).to_owned();
                let is_size = size_part.ends_with("px")
                    || size_part.ends_with("pt")
                    || size_part.ends_with("rem")
                    || size_part.ends_with("em");
                if is_size {
                    let n: f32 = size_part
                        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
                        .parse()
                        .ok()?;
                    size = match size_part.strip_suffix("pt") {
                        Some(_) => n * 4.0 / 3.0,
                        None => n,
                    };
                    seen_size = true;
                    i += 1;
                    // Family = the rest.
                    for f in &tokens[i..] {
                        if !family.is_empty() {
                            family.push(' ');
                        }
                        family.push_str(f);
                    }
                    break 'outer;
                }
                if let Ok(n) = t.parse::<u16>() {
                    if (100..=900).contains(&n) {
                        weight = n;
                        i += 1;
                        continue;
                    }
                }
                // Unknown token: start of the family.
                family.push_str(&tokens[i]);
                i += 1;
                for f in &tokens[i..] {
                    family.push(' ');
                    family.push_str(f);
                }
                break 'outer;
            }
        }
    }
    if !seen_size || family.is_empty() {
        return None;
    }
    // Strip quote characters from the family.
    let family: String = family.chars().filter(|c| *c != '"' && *c != '\'').collect();
    let family = family.trim().to_owned();
    if family.is_empty() {
        return None;
    }
    Some(FontSpec {
        size,
        family,
        weight,
        italic,
    })
}

// ---------------------------------------------------------------------------
// Canvas state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CtxState {
    ctm: Transform,
    fill: PaintSource,
    stroke: PaintSource,
    line_width: f32,
    line_cap: Cap,
    line_join: Join,
    miter_limit: f32,
    dash: Vec<f32>,
    dash_offset: f32,
    global_alpha: f32,
    composite: CompositeOp,
    shadow_color: Color,
    shadow_blur: f32,
    shadow_offset_x: f32,
    shadow_offset_y: f32,
    font: FontSpec,
    text_align: TextAlign,
    text_baseline: Baseline,
    image_smoothing: bool,
    clip: Option<Mask>,
}

impl Default for CtxState {
    fn default() -> Self {
        CtxState {
            ctm: Transform::identity(),
            fill: PaintSource::Color(Color::BLACK),
            stroke: PaintSource::Color(Color::BLACK),
            line_width: 1.0,
            line_cap: Cap::Butt,
            line_join: Join::Miter,
            miter_limit: 10.0,
            dash: Vec::new(),
            dash_offset: 0.0,
            global_alpha: 1.0,
            composite: CompositeOp::SourceOver,
            shadow_color: Color::TRANSPARENT,
            shadow_blur: 0.0,
            shadow_offset_x: 0.0,
            shadow_offset_y: 0.0,
            font: FontSpec::default(),
            text_align: TextAlign::Start,
            text_baseline: Baseline::Alphabetic,
            image_smoothing: true,
            clip: None,
        }
    }
}

/// A live 2D canvas surface.
pub struct Canvas2D {
    pixmap: Pixmap,
    state: CtxState,
    stack: Vec<CtxState>,
    builder: PathBuilder,
    path: Option<Path>,
    gradients: HashMap<u32, (GradientDef, Vec<Stop>)>,
    patterns: HashMap<u32, Rc<Pixmap>>,
    pattern_modes: HashMap<u32, PatternRepeat>,
    next_obj: u32,
}

impl Canvas2D {
    /// New canvas with a transparent backing bitmap `w` x `h`.
    pub fn new(w: u32, h: u32) -> Canvas2D {
        let pixmap = Pixmap::new(w.clamp(1, 8192), h.clamp(1, 8192))
            .unwrap_or_else(|| Pixmap::new(1, 1).expect("1x1 pixmap"));
        Canvas2D {
            pixmap,
            state: CtxState::default(),
            stack: Vec::new(),
            builder: PathBuilder::new(),
            path: None,
            gradients: HashMap::new(),
            patterns: HashMap::new(),
            pattern_modes: HashMap::new(),
            next_obj: 1,
        }
    }

    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    /// Resizes the backing bitmap (contents reset — spec behavior).
    pub fn resize(&mut self, w: u32, h: u32) {
        self.pixmap = Pixmap::new(w.clamp(1, 8192), h.clamp(1, 8192))
            .unwrap_or_else(|| Pixmap::new(1, 1).expect("1x1 pixmap"));
        self.stack.clear();
        self.state = CtxState::default();
    }

    /// Straight-RGBA snapshot for the display-list image map.
    pub fn snapshot(&self) -> DecodedImage {
        DecodedImage {
            width: self.pixmap.width(),
            height: self.pixmap.height(),
            rgba: std::sync::Arc::new(pixmap_to_straight(self.pixmap.data())),
        }
    }

    /// PNG-encoded snapshot (straight alpha), for `toDataURL('image/png')`.
    pub fn to_png_bytes(&self) -> Option<Vec<u8>> {
        let straight = pixmap_to_straight(self.pixmap.data());
        let img = image::RgbaImage::from_raw(self.pixmap.width(), self.pixmap.height(), straight)?;
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .ok()?;
        Some(png)
    }

    /// JPEG-encoded snapshot, for `toDataURL('image/jpeg', quality)`.
    pub fn to_jpeg_bytes(&self, quality: u8) -> Option<Vec<u8>> {
        let straight = pixmap_to_straight(self.pixmap.data());
        let img = image::RgbaImage::from_raw(self.pixmap.width(), self.pixmap.height(), straight)?;
        let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
        let mut jpg = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut jpg);
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality);
        image::DynamicImage::ImageRgb8(rgb)
            .write_with_encoder(enc)
            .ok()?;
        Some(jpg)
    }

    // -- state ----------------------------------------------------------------

    pub fn save(&mut self) {
        self.stack.push(self.state.clone());
    }

    pub fn restore(&mut self) {
        if let Some(s) = self.stack.pop() {
            self.state = s;
        }
    }

    pub fn global_alpha(&self) -> f32 {
        self.state.global_alpha
    }

    pub fn set_global_alpha(&mut self, a: f32) {
        self.state.global_alpha = a.clamp(0.0, 1.0);
    }

    pub fn composite(&self) -> CompositeOp {
        self.state.composite
    }

    pub fn set_composite(&mut self, op: CompositeOp) {
        self.state.composite = op;
    }

    pub fn set_transform_row(&mut self, a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) {
        if all_finite(&[a, b, c, d, e, f]) {
            self.state.ctm = Transform::from_row(a, b, c, d, e, f);
        }
    }

    pub fn transform(&mut self, a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) {
        if all_finite(&[a, b, c, d, e, f]) {
            self.state.ctm = self
                .state
                .ctm
                .pre_concat(Transform::from_row(a, b, c, d, e, f));
        }
    }

    pub fn translate(&mut self, tx: f32, ty: f32) {
        self.state.ctm = self.state.ctm.pre_concat(Transform::from_translate(tx, ty));
    }

    pub fn rotate(&mut self, radians: f32) {
        self.state.ctm = self
            .state
            .ctm
            .pre_concat(Transform::from_rotate(radians.to_degrees()));
    }

    pub fn scale(&mut self, sx: f32, sy: f32) {
        self.state.ctm = self.state.ctm.pre_concat(Transform::from_scale(sx, sy));
    }

    pub fn set_line_width(&mut self, w: f32) {
        self.state.line_width = w.max(0.0);
    }

    pub fn set_line_cap(&mut self, cap: Cap) {
        self.state.line_cap = cap;
    }

    pub fn set_line_join(&mut self, join: Join) {
        self.state.line_join = join;
    }

    pub fn set_miter_limit(&mut self, l: f32) {
        self.state.miter_limit = l.max(1.0);
    }

    pub fn set_line_dash(&mut self, dash: Vec<f32>) {
        self.state.dash = dash.iter().map(|d| d.max(0.0)).collect();
    }

    pub fn set_line_dash_offset(&mut self, o: f32) {
        self.state.dash_offset = o;
    }

    pub fn set_fill_color(&mut self, c: Color) {
        self.state.fill = PaintSource::Color(c);
    }

    pub fn set_stroke_color(&mut self, c: Color) {
        self.state.stroke = PaintSource::Color(c);
    }

    pub fn set_fill_gradient(&mut self, id: u32) {
        if let Some((def, stops)) = self.gradients.get(&id).cloned() {
            self.state.fill = PaintSource::Gradient { def, stops };
        }
    }

    pub fn set_stroke_gradient(&mut self, id: u32) {
        if let Some((def, stops)) = self.gradients.get(&id).cloned() {
            self.state.stroke = PaintSource::Gradient { def, stops };
        }
    }

    pub fn set_fill_pattern(&mut self, id: u32) {
        if let Some(pixmap) = self.patterns.get(&id).cloned() {
            let repeat = self
                .pattern_modes
                .get(&id)
                .copied()
                .unwrap_or(PatternRepeat::Repeat);
            self.state.fill = PaintSource::Pattern { pixmap, repeat };
        }
    }

    pub fn set_stroke_pattern(&mut self, id: u32) {
        if let Some(pixmap) = self.patterns.get(&id).cloned() {
            let repeat = self
                .pattern_modes
                .get(&id)
                .copied()
                .unwrap_or(PatternRepeat::Repeat);
            self.state.stroke = PaintSource::Pattern { pixmap, repeat };
        }
    }

    pub fn set_shadow_color(&mut self, c: Color) {
        self.state.shadow_color = c;
    }

    pub fn set_shadow_blur(&mut self, b: f32) {
        self.state.shadow_blur = b.max(0.0);
    }

    pub fn set_shadow_offset(&mut self, x: f32, y: f32) {
        self.state.shadow_offset_x = x;
        self.state.shadow_offset_y = y;
    }

    /// Current `shadowOffsetX`.
    pub fn shadow_offset_x(&self) -> f32 {
        self.state.shadow_offset_x
    }

    /// Current `shadowOffsetY`.
    pub fn shadow_offset_y(&self) -> f32 {
        self.state.shadow_offset_y
    }

    pub fn set_font(&mut self, spec: FontSpec) {
        self.state.font = spec;
    }

    /// Current font spec.
    pub fn font(&self) -> &FontSpec {
        &self.state.font
    }

    pub fn set_text_align(&mut self, a: TextAlign) {
        self.state.text_align = a;
    }

    pub fn set_text_baseline(&mut self, b: Baseline) {
        self.state.text_baseline = b;
    }

    pub fn set_image_smoothing(&mut self, on: bool) {
        self.state.image_smoothing = on;
    }

    /// Current `imageSmoothingEnabled`.
    pub fn image_smoothing(&self) -> bool {
        self.state.image_smoothing
    }

    // -- gradients and patterns ----------------------------------------------

    pub fn create_gradient(&mut self, def: GradientDef) -> u32 {
        let id = self.next_obj;
        self.next_obj += 1;
        self.gradients.insert(id, (def, Vec::new()));
        id
    }

    pub fn gradient_add_stop(&mut self, id: u32, offset: f32, color: Color) {
        if let Some((_, stops)) = self.gradients.get_mut(&id) {
            stops.push(Stop {
                offset: offset.clamp(0.0, 1.0),
                color,
            });
        }
    }

    /// Creates a pattern from an image (straight RGBA) with a repeat mode.
    pub fn create_pattern(&mut self, image: &DecodedImage, repeat: PatternRepeat) -> u32 {
        let id = self.next_obj;
        self.next_obj += 1;
        if let Some(pixmap) = straight_to_pixmap(&image.rgba, image.width, image.height) {
            self.patterns.insert(id, Rc::new(pixmap));
            self.pattern_modes.insert(id, repeat);
        }
        id
    }

    // -- path building ---------------------------------------------------------

    pub fn begin_path(&mut self) {
        self.builder = PathBuilder::new();
        self.path = None;
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(x, y);
        self.path = None;
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(x, y);
        self.path = None;
    }

    pub fn quadratic_curve_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.builder.quad_to(cx, cy, x, y);
        self.path = None;
    }

    pub fn bezier_curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.builder.cubic_to(c1x, c1y, c2x, c2y, x, y);
        self.path = None;
    }

    pub fn close_path(&mut self) {
        self.builder.close();
        self.path = None;
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if let Some(r) = rect_from(x, y, w, h) {
            self.builder.push_rect(r);
            self.path = None;
        }
    }

    /// `arc(cx, cy, r, start, end, anticlockwise)`.
    pub fn arc(&mut self, cx: f32, cy: f32, r: f32, start: f32, end: f32, anticlockwise: bool) {
        self.emit_arc(cx, cy, r, r, 0.0, start, end, anticlockwise);
    }

    /// `ellipse(cx, cy, rx, ry, rotation, start, end, anticlockwise)`.
    #[allow(clippy::too_many_arguments)]
    pub fn ellipse(
        &mut self,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        rotation: f32,
        start: f32,
        end: f32,
        anticlockwise: bool,
    ) {
        self.emit_arc(cx, cy, rx, ry, rotation, start, end, anticlockwise);
    }

    /// Emits an arc as <= 90-degree cubic segments (kappa approximation).
    /// For rotated ellipses the control points are mapped through
    /// (translate * rotate) applied around the center.
    #[allow(clippy::too_many_arguments)]
    fn emit_arc(
        &mut self,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        rotation: f32,
        start: f32,
        end: f32,
        anticlockwise: bool,
    ) {
        if rx <= 0.0 || ry <= 0.0 {
            return;
        }
        let tau = 2.0 * std::f32::consts::PI;
        let a0 = start.rem_euclid(tau);
        let mut a1 = end.rem_euclid(tau);
        if (a1 - a0).abs() < 1e-9 {
            // Equal angles: full circle when the caller passed different
            // raw values (e.g. 0 and 2pi -> rem_euclid made them equal).
            if (end - start).abs() > 1e-9 {
                a1 = a0 + if anticlockwise { -tau } else { tau };
            }
        }
        let sweep = if anticlockwise {
            let mut s = a0 - a1;
            if s <= 0.0 {
                s += tau;
            }
            -s
        } else {
            let mut s = a1 - a0;
            if s < 0.0 {
                s += tau;
            }
            s
        };
        let segs =
            ((sweep.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize).clamp(1, 64);
        let step = sweep / segs as f32;
        let kappa = 0.552_284_7;
        let map = |x: f32, y: f32| -> (f32, f32) {
            // Local ellipse point (cos*a*rx - sin*b*ry, ...) around center.
            if rotation == 0.0 {
                (cx + x, cy + y)
            } else {
                let (sn, cs) = rotation.sin_cos();
                (cx + x * cs - y * sn, cy + x * sn + y * cs)
            }
        };
        let mut angle = a0;
        let p = |a: f32| map(a.cos() * rx, a.sin() * ry);
        let mut p0 = p(angle);
        self.builder.move_to(p0.0, p0.1);
        for _ in 0..segs {
            let a1 = angle + step;
            let p1 = p(a1);
            let h = (step.abs() / 2.0).tan() * kappa;
            // Tangent control offsets in local space.
            let t0x = -angle.sin() * rx * h;
            let t0y = angle.cos() * ry * h;
            let t1x = -a1.sin() * rx * h;
            let t1y = a1.cos() * ry * h;
            let c0 = map(p0.0 - cx + t0x, p0.1 - cy + t0y);
            let c1 = map(p1.0 - cx - t1x, p1.1 - cy - t1y);
            self.builder.cubic_to(c0.0, c0.1, c1.0, c1.1, p1.0, p1.1);
            p0 = p1;
            angle = a1;
        }
        self.path = None;
    }

    /// `arcTo(x0, y0, x1, y1, r)` — `current` is the path's current point.
    pub fn arc_to(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, current: (f32, f32)) {
        let l1 = ((x0 - current.0).hypot(y0 - current.1)).max(1e-9);
        let l2 = ((x1 - x0).hypot(y1 - y0)).max(1e-9);
        if r <= 0.0 || l1 <= 1e-9 || l2 <= 1e-9 {
            self.line_to(x0, y0);
            return;
        }
        // Chrome clamps the radius to what fits the corner legs.
        let rr = r.min(l1.min(l2));
        let u1 = ((x0 - current.0) / l1, (y0 - current.1) / l1);
        let u2 = ((x1 - x0) / l2, (y1 - y0) / l2);
        let dot = (u1.0 * u2.0 + u1.1 * u2.1).clamp(-1.0, 1.0);
        let theta = dot.acos();
        if theta.abs() < 1e-4 {
            // Legs collinear: no arc, just the line to the corner.
            self.line_to(x0, y0);
            return;
        }
        let tan_half = (theta / 2.0).tan();
        let dist = rr / tan_half.max(1e-9);
        let t1 = (x0 - u1.0 * dist, y0 - u1.1 * dist);
        let t2 = (x0 + u2.0 * dist, y0 + u2.1 * dist);
        // Center = corner + bisector * rr / sin(theta/2).
        let bis = (u1.0 + u2.0, u1.1 + u2.1);
        let bl = (bis.0 * bis.0 + bis.1 * bis.1).sqrt().max(1e-9);
        let hyp = (rr / (theta / 2.0).sin().max(1e-9)) * (bl / 2.0).max(1e-9);
        let _ = hyp;
        let center = (
            x0 + bis.0 / bl * (rr / (theta / 2.0).sin().max(1e-9)),
            y0 + bis.1 / bl * (rr / (theta / 2.0).sin().max(1e-9)),
        );
        let a0 = (t1.0 - center.0).atan2(t1.1 - center.1);
        let a1 = (t2.0 - center.0).atan2(t2.1 - center.1);
        let mut sweep = a1 - a0;
        while sweep > std::f32::consts::PI {
            sweep -= 2.0 * std::f32::consts::PI;
        }
        while sweep < -std::f32::consts::PI {
            sweep += 2.0 * std::f32::consts::PI;
        }
        self.line_to(t1.0, t1.1);
        self.emit_arc(center.0, center.1, rr, rr, 0.0, a0, a0 + sweep, sweep < 0.0);
    }

    /// The current path (built from the builder on demand).
    pub fn current_path(&mut self) -> Option<&Path> {
        if self.path.is_none() {
            self.path = self.builder.clone().finish();
        }
        self.path.as_ref()
    }

    /// Last path point (for arcTo chaining).
    pub fn last_path_point(&self) -> (f32, f32) {
        self.builder
            .last_point()
            .map(|p| (p.x, p.y))
            .unwrap_or((0.0, 0.0))
    }

    // -- drawing ---------------------------------------------------------------

    pub fn clear_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let Some(r) = rect_from(x, y, w, h) else {
            return;
        };
        let ctm = self.state.ctm;
        let paint = Paint {
            shader: Shader::SolidColor(tiny_skia::Color::from_rgba(0.0, 0.0, 0.0, 1.0).unwrap()),
            blend_mode: BlendMode::DestinationOut,
            anti_alias: true,
            ..Default::default()
        };
        let clip = self.state.clip.clone();
        match clip.as_ref() {
            Some(m) => self.pixmap.as_mut().fill_rect(r, &paint, ctm, Some(m)),
            None => self.pixmap.as_mut().fill_rect(r, &paint, ctm, None),
        }
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let Some(r) = rect_from(x, y, w, h) else {
            return;
        };
        let path = PathBuilder::from_rect(r);
        self.fill_path_impl(&path, FillRule::Winding);
    }

    pub fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let Some(r) = rect_from(x, y, w, h) else {
            return;
        };
        let path = PathBuilder::from_rect(r);
        self.stroke_path_impl(&path);
    }

    pub fn fill(&mut self, evenodd: bool) {
        let path = self.current_path().cloned();
        if let Some(path) = path {
            self.fill_path_impl(
                &path,
                if evenodd {
                    FillRule::EvenOdd
                } else {
                    FillRule::Winding
                },
            );
        }
    }

    pub fn stroke(&mut self) {
        let path = self.current_path().cloned();
        if let Some(path) = path {
            self.stroke_path_impl(&path);
        }
    }

    pub fn clip(&mut self, evenodd: bool) {
        let path = self.current_path().cloned();
        let Some(path) = path else { return };
        let ctm = self.state.ctm;
        let rule = if evenodd {
            FillRule::EvenOdd
        } else {
            FillRule::Winding
        };
        let (w, h) = (self.pixmap.width(), self.pixmap.height());
        match &mut self.state.clip {
            Some(mask) => mask.intersect_path(&path, rule, true, ctm),
            None => {
                if let Some(mut mask) = Mask::new(w, h) {
                    mask.fill_path(&path, rule, true, ctm);
                    self.state.clip = Some(mask);
                }
            }
        }
    }

    pub fn is_point_in_path(&mut self, x: f32, y: f32, evenodd: bool) -> bool {
        let ctm = self.state.ctm;
        let path = self.current_path().cloned();
        let Some(path) = path else { return false };
        // Transform the probe point into user space and rasterize the path
        // into a 1x1 probe: translate by (-(x-0.5), -(y-0.5)) in device
        // space, applied after the CTM.
        let device = path.clone().transform(ctm).unwrap_or_else(|| path.clone());
        let Some(shifted) = device.transform(Transform::from_translate(-x + 0.5, -y + 0.5)) else {
            return false;
        };
        let Some(mut probe) = Pixmap::new(1, 1) else {
            return false;
        };
        let paint = Paint {
            shader: Shader::SolidColor(tiny_skia::Color::WHITE),
            blend_mode: BlendMode::SourceOver,
            anti_alias: false,
            ..Default::default()
        };
        probe.as_mut().fill_path(
            &shifted,
            &paint,
            if evenodd {
                FillRule::EvenOdd
            } else {
                FillRule::Winding
            },
            Transform::identity(),
            None,
        );
        probe.pixel(0, 0).map(|p| p.alpha() > 127).unwrap_or(false)
    }

    // -- image ops ---------------------------------------------------------------

    /// `drawImage` 3-arg form.
    pub fn draw_image(&mut self, img: &DecodedImage, dx: f32, dy: f32) {
        self.draw_image_9(
            img,
            0.0,
            0.0,
            img.width as f32,
            img.height as f32,
            dx,
            dy,
            img.width as f32,
            img.height as f32,
        );
    }

    /// `drawImage` 5-arg form.
    pub fn draw_image_scaled(&mut self, img: &DecodedImage, dx: f32, dy: f32, dw: f32, dh: f32) {
        self.draw_image_9(
            img,
            0.0,
            0.0,
            img.width as f32,
            img.height as f32,
            dx,
            dy,
            dw,
            dh,
        );
    }

    /// `drawImage` 9-arg form: source rect (image pixels) -> dest rect (user space).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_image_9(
        &mut self,
        img: &DecodedImage,
        sx: f32,
        sy: f32,
        sw: f32,
        sh: f32,
        dx: f32,
        dy: f32,
        dw: f32,
        dh: f32,
    ) {
        if sw <= 0.0 || sh <= 0.0 || dw <= 0.0 || dh <= 0.0 {
            return;
        }
        // Clip the source rect to the image bounds.
        let sx = sx.clamp(0.0, img.width as f32);
        let sy = sy.clamp(0.0, img.height as f32);
        let sw = sw.min(img.width as f32 - sx);
        let sh = sh.min(img.height as f32 - sy);
        if sw <= 0.0 || sh <= 0.0 {
            return;
        }
        let Some(src) = crop_straight(&img.rgba, img.width, img.height, sx, sy, sw, sh) else {
            return;
        };
        // Shadow pass.
        if self.shadow_active() {
            if let Some(r) = rect_from(dx, dy, dw, dh) {
                self.shadow_rect(r);
            }
        }
        let ctm = self.state.ctm;
        // Source pixel (u, v) -> user (dx + u*dw/sw, dy + v*dh/sh) -> device.
        // tiny-skia post_concat(other) = other * self, so building
        // S.post(T).post(CTM) yields CTM * T * S — the point order we need.
        let full = Transform::from_scale(dw / sw, dh / sh)
            .post_concat(Transform::from_translate(dx, dy))
            .post_concat(ctm);
        let pixmap_paint = PixmapPaint {
            opacity: self.state.global_alpha,
            blend_mode: self.state.composite.skia(),
            quality: if self.state.image_smoothing {
                FilterQuality::Bilinear
            } else {
                FilterQuality::Nearest
            },
        };
        let clip = self.state.clip.clone();
        match clip.as_ref() {
            Some(m) => {
                self.pixmap
                    .as_mut()
                    .draw_pixmap(0, 0, src.as_ref(), &pixmap_paint, full, Some(m))
            }
            None => self
                .pixmap
                .as_mut()
                .draw_pixmap(0, 0, src.as_ref(), &pixmap_paint, full, None),
        }
    }

    // -- pixel data ---------------------------------------------------------------

    /// `getImageData` (straight RGBA).
    pub fn get_image_data(&self, x: i32, y: i32, w: u32, h: u32) -> Option<(Vec<u8>, u32, u32)> {
        let w = w.min(self.pixmap.width().saturating_sub(x.max(0) as u32).max(1));
        let h = h.min(self.pixmap.height().saturating_sub(y.max(0) as u32).max(1));
        let (w, h) = (w.clamp(1, 8192), h.clamp(1, 8192));
        let x = x.max(0) as u32;
        let y = y.max(0) as u32;
        if x >= self.pixmap.width() || y >= self.pixmap.height() {
            return Some((vec![0; (w * h * 4) as usize], w, h));
        }
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        let stride = (self.pixmap.width() * 4) as usize;
        let data = self.pixmap.data();
        for row in 0..h {
            let y = y + row;
            if y >= self.pixmap.height() {
                out.extend(std::iter::repeat_n(0, (w * 4) as usize));
                continue;
            }
            let start = y as usize * stride + (x * 4) as usize;
            let end = (start + (w * 4) as usize).min(data.len());
            let mut row_bytes = data[start..end].to_vec();
            if row_bytes.len() < (w * 4) as usize {
                row_bytes.resize((w * 4) as usize, 0);
            }
            out.extend(premul_bytes_to_straight(&row_bytes));
        }
        Some((out, w, h))
    }

    /// `putImageData` (straight RGBA at device pixels, ignoring the CTM).
    pub fn put_image_data(&mut self, data: &[u8], w: u32, h: u32, dx: i32, dy: i32) {
        if data.len() < (w * h * 4) as usize || w == 0 || h == 0 {
            return;
        }
        for row in 0..h {
            let py = dy + row as i32;
            if py < 0 || py >= self.pixmap.height() as i32 {
                continue;
            }
            for col in 0..w {
                let px = dx + col as i32;
                if px < 0 || px >= self.pixmap.width() as i32 {
                    continue;
                }
                let i = ((row * w + col) * 4) as usize;
                let a = data[i + 3];
                let (r, g, b) = if a == 0 {
                    (0u8, 0u8, 0u8)
                } else {
                    let pr = ((data[i] as u32 * a as u32 + 127) / 255).min(255) as u8;
                    let pg = ((data[i + 1] as u32 * a as u32 + 127) / 255).min(255) as u8;
                    let pb = ((data[i + 2] as u32 * a as u32 + 127) / 255).min(255) as u8;
                    (pr, pg, pb)
                };
                let off = (py as u32 as usize) * (self.pixmap.width() as usize) * 4
                    + (px as u32 as usize) * 4;
                let buf = self.pixmap.data_mut();
                if off + 3 < buf.len() {
                    buf[off] = r;
                    buf[off + 1] = g;
                    buf[off + 2] = b;
                    buf[off + 3] = a;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal drawing core
// ---------------------------------------------------------------------------

impl Canvas2D {
    fn shadow_active(&self) -> bool {
        self.state.shadow_color.a > 0.01
            && (self.state.shadow_blur > 0.0
                || self.state.shadow_offset_x != 0.0
                || self.state.shadow_offset_y != 0.0)
    }

    /// Builds a tiny-skia shader for a paint source, in user space.
    fn build_shader(
        src: &PaintSource,
        alpha: f32,
        repeat: Option<PatternRepeat>,
    ) -> Option<Shader<'static>> {
        match src {
            PaintSource::Color(c) => {
                let mut sh = Shader::SolidColor(c.skia());
                if alpha < 1.0 {
                    sh.apply_opacity(alpha);
                }
                Some(sh)
            }
            PaintSource::Gradient { def, stops } => {
                let mut sk_stops = Vec::with_capacity(stops.len());
                let mut sorted = stops.clone();
                sorted.sort_by_key(|s| (s.offset * 1000.0) as i32);
                for st in sorted {
                    sk_stops.push(tiny_skia::GradientStop::new(
                        st.offset.clamp(0.0, 1.0),
                        st.color.skia(),
                    ));
                }
                if sk_stops.is_empty() {
                    return None;
                }
                let mut sh = match def {
                    GradientDef::Linear { x0, y0, x1, y1 } => tiny_skia::LinearGradient::new(
                        tiny_skia::Point::from_xy(*x0, *y0),
                        tiny_skia::Point::from_xy(*x1, *y1),
                        sk_stops,
                        SpreadMode::Pad,
                        Transform::identity(),
                    )?,
                    GradientDef::Radial {
                        x0,
                        y0,
                        r0,
                        x1,
                        y1,
                        r1,
                    } => tiny_skia::RadialGradient::new(
                        tiny_skia::Point::from_xy(*x0, *y0),
                        r0.max(0.0),
                        tiny_skia::Point::from_xy(*x1, *y1),
                        r1.max(0.0),
                        sk_stops,
                        SpreadMode::Pad,
                        Transform::identity(),
                    )?,
                };
                if alpha < 1.0 {
                    sh.apply_opacity(alpha);
                }
                Some(sh)
            }
            PaintSource::Pattern { pixmap, .. } => {
                // The pattern shader borrows the pixmap; we rebuild it at
                // call time with the live repeat mode. (Handled by the
                // caller for borrow-splitting; this arm is unreachable
                // through the generic path.)
                let _ = pixmap;
                let _ = repeat;
                None
            }
        }
    }

    /// Stroke parameters (cap/join/miter/dash).
    fn stroke_spec(&self) -> Stroke {
        let cap = match self.state.line_cap {
            Cap::Butt => LineCap::Butt,
            Cap::Round => LineCap::Round,
            Cap::Square => LineCap::Square,
        };
        let join = match self.state.line_join {
            Join::Bevel => LineJoin::Bevel,
            Join::Miter => LineJoin::Miter,
            Join::Round => LineJoin::Round,
        };
        let dash = if self.state.dash.iter().any(|d| *d > 0.0) {
            StrokeDash::new(self.state.dash.clone(), self.state.dash_offset)
        } else {
            None
        };
        Stroke {
            width: self.state.line_width.max(0.0),
            miter_limit: self.state.miter_limit,
            line_cap: cap,
            line_join: join,
            dash,
        }
    }

    /// Dashes the path first (tiny-skia Stroke.dash is applied by the
    /// stroker via `Path::dash`), producing a stroke outline.
    fn stroke_outline(&self, path: &Path) -> Option<Path> {
        let st = self.stroke_spec();
        let res = tiny_skia::PathStroker::compute_resolution_scale(&self.state.ctm);
        if let Some(dash) = &st.dash {
            let dashed = path.dash(dash, res)?;
            dashed.stroke(&st, res)
        } else {
            path.stroke(&st, res)
        }
    }

    fn fill_path_impl(&mut self, path: &Path, rule: FillRule) {
        let source = self.state.fill.clone();
        self.paint_path_impl(path, rule, source);
    }

    /// Core path painter: draws `path` with the given source (fill or
    /// stroke), handling shadow, pattern, gradient and clip state.
    fn paint_path_impl(&mut self, path: &Path, rule: FillRule, source: PaintSource) {
        // Split borrows: pixmap (mut), state (read), patterns (read).
        let state = &self.state;
        let pixmap = &mut self.pixmap;
        let ctm = state.ctm;
        let alpha = state.global_alpha;
        let blend = state.composite.skia();

        // Shadow pass.
        if state.shadow_color.a > 0.01
            && (state.shadow_blur > 0.0
                || state.shadow_offset_x != 0.0
                || state.shadow_offset_y != 0.0)
        {
            if let Some(mut sil) = Pixmap::new(pixmap.width(), pixmap.height()) {
                let sh_paint = Paint {
                    shader: Shader::SolidColor(state.shadow_color.skia()),
                    blend_mode: BlendMode::SourceOver,
                    anti_alias: true,
                    ..Default::default()
                };
                sil.as_mut().fill_path(path, &sh_paint, rule, ctm, None);
                blur_shadow(&mut sil, state.shadow_blur);
                let off = shadow_offset_device(state);
                let p = PixmapPaint {
                    opacity: 1.0,
                    blend_mode: BlendMode::SourceOver,
                    quality: FilterQuality::Nearest,
                };
                pixmap.as_mut().draw_pixmap(
                    off.0,
                    off.1,
                    sil.as_ref(),
                    &p,
                    Transform::identity(),
                    None,
                );
            }
        }

        // Pattern fill: borrow the pattern pixmap for the shader.
        if let PaintSource::Pattern {
            pixmap: pat,
            repeat,
        } = &source
        {
            draw_pattern_fill(
                pixmap,
                path,
                rule,
                ctm,
                pat,
                *repeat,
                alpha,
                blend,
                state.clip.as_ref(),
            );
            return;
        }

        if let Some(shader) = Self::build_shader(&source, alpha, None) {
            let paint = Paint {
                shader,
                blend_mode: blend,
                anti_alias: true,
                ..Default::default()
            };
            match state.clip.as_ref() {
                Some(m) => pixmap.as_mut().fill_path(path, &paint, rule, ctm, Some(m)),
                None => pixmap.as_mut().fill_path(path, &paint, rule, ctm, None),
            }
        }
    }

    fn stroke_path_impl(&mut self, path: &Path) {
        let Some(outline) = self.stroke_outline(path) else {
            return;
        };
        // The stroked outline is FILLED with the stroke source (was the
        // fill source — strokes painted in fillStyle color).
        let source = self.state.stroke.clone();
        self.paint_path_impl(&outline, FillRule::Winding, source);
    }

    /// Shadow for a rect (drawImage case).
    fn shadow_rect(&mut self, r: tiny_skia::Rect) {
        let state = &self.state;
        let pixmap = &mut self.pixmap;
        let ctm = state.ctm;
        let mut sil = match Pixmap::new(pixmap.width(), pixmap.height()) {
            Some(p) => p,
            None => return,
        };
        let sh_paint = Paint {
            shader: Shader::SolidColor(state.shadow_color.skia()),
            blend_mode: BlendMode::SourceOver,
            anti_alias: true,
            ..Default::default()
        };
        sil.as_mut().fill_rect(r, &sh_paint, ctm, None);
        blur_shadow(&mut sil, state.shadow_blur);
        let off = shadow_offset_device(state);
        let p = PixmapPaint {
            opacity: 1.0,
            blend_mode: BlendMode::SourceOver,
            quality: FilterQuality::Nearest,
        };
        pixmap
            .as_mut()
            .draw_pixmap(off.0, off.1, sil.as_ref(), &p, Transform::identity(), None);
    }
}

/// Fills `path` with a pattern shader (borrow-friendly: the pattern pixmap
/// is borrowed from the caller's map).
#[allow(clippy::too_many_arguments)]
fn draw_pattern_fill(
    pixmap: &mut Pixmap,
    path: &Path,
    rule: FillRule,
    ctm: Transform,
    pat: &Rc<Pixmap>,
    repeat: PatternRepeat,
    alpha: f32,
    blend: BlendMode,
    clip: Option<&Mask>,
) {
    // Tiling: Repeat is native (SpreadMode::Repeat); the others pre-tile
    // into a bigger pixmap (Pad) covering the fill area. The tiled pixmap
    // must outlive the shader borrow, so it is bound in this scope.
    let mut tiled: Option<Pixmap> = None;
    if repeat != PatternRepeat::Repeat {
        // Pre-tile to cover the device-space bounds of the path.
        let bounds = {
            let device = path.clone().transform(ctm).unwrap_or_else(|| path.clone());
            device.bounds()
        };
        let need_w = (bounds.right() - bounds.left())
            .ceil()
            .max(pat.width() as f32) as u32;
        let need_h = (bounds.bottom() - bounds.top())
            .ceil()
            .max(pat.height() as f32) as u32;
        let tile_w = if repeat == PatternRepeat::RepeatY || repeat == PatternRepeat::NoRepeat {
            pat.width()
        } else {
            need_w.clamp(1, 8192)
        };
        let tile_h = if repeat == PatternRepeat::RepeatX || repeat == PatternRepeat::NoRepeat {
            pat.height()
        } else {
            need_h.clamp(1, 8192)
        };
        if let Some(mut p) = Pixmap::new(tile_w.max(1), tile_h.max(1)) {
            let paint = PixmapPaint {
                opacity: 1.0,
                blend_mode: BlendMode::SourceOver,
                quality: FilterQuality::Nearest,
            };
            let mut x = 0;
            while x < tile_w {
                let mut y = 0;
                while y < tile_h {
                    p.as_mut().draw_pixmap(
                        x as i32,
                        y as i32,
                        Pixmap::as_ref(pat),
                        &paint,
                        Transform::identity(),
                        None,
                    );
                    y += pat.height();
                }
                x += pat.width();
            }
            tiled = Some(p);
        }
    }
    let shader = match (repeat, tiled.as_ref()) {
        (PatternRepeat::Repeat, _) => tiny_skia::Pattern::new(
            Pixmap::as_ref(pat),
            SpreadMode::Repeat,
            FilterQuality::Bilinear,
            alpha,
            Transform::identity(),
        ),
        (_, Some(t)) => tiny_skia::Pattern::new(
            t.as_ref(),
            SpreadMode::Pad,
            FilterQuality::Bilinear,
            alpha,
            Transform::identity(),
        ),
        // Tiling allocation failed: fall back to a single repeat.
        (_, None) => tiny_skia::Pattern::new(
            Pixmap::as_ref(pat),
            SpreadMode::Repeat,
            FilterQuality::Bilinear,
            alpha,
            Transform::identity(),
        ),
    };
    let paint = Paint {
        shader,
        blend_mode: blend,
        anti_alias: true,
        ..Default::default()
    };
    match clip {
        Some(m) => pixmap.as_mut().fill_path(path, &paint, rule, ctm, Some(m)),
        None => pixmap.as_mut().fill_path(path, &paint, rule, ctm, None),
    }
}

/// Shadow offset in device space (offset transformed by the CTM's linear
/// part).
fn shadow_offset_device(state: &CtxState) -> (i32, i32) {
    let mut p = tiny_skia::Point::from_xy(state.shadow_offset_x, state.shadow_offset_y);
    state.ctm.map_point(&mut p);
    (p.x.round() as i32, p.y.round() as i32)
}

/// 3-pass box blur (Gaussian approximation), radius from the canvas
/// `shadowBlur` (Chrome uses blur/2 as the Gaussian sigma).
fn blur_shadow(pixmap: &mut Pixmap, blur: f32) {
    let radius = (blur / 2.0).round().max(0.0) as usize;
    crate::painter::blur_pixmap(pixmap, radius);
}

// ---------------------------------------------------------------------------
// Pixel conversion helpers
// ---------------------------------------------------------------------------

/// Converts a full premultiplied RGBA buffer to straight RGBA.
pub fn pixmap_to_straight(premul: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(premul.len());
    for px in premul.chunks_exact(4) {
        let a = px[3] as u32;
        let (r, g, b) = if a == 0 {
            (0, 0, 0)
        } else if a == 255 {
            (px[0] as u32, px[1] as u32, px[2] as u32)
        } else {
            (
                ((px[0] as u32 * 255 + a / 2) / a).min(255),
                ((px[1] as u32 * 255 + a / 2) / a).min(255),
                ((px[2] as u32 * 255 + a / 2) / a).min(255),
            )
        };
        out.extend_from_slice(&[r as u8, g as u8, b as u8, a as u8]);
    }
    out
}

fn premul_bytes_to_straight(bytes: &[u8]) -> Vec<u8> {
    pixmap_to_straight(bytes)
}

/// Builds a premultiplied Pixmap from straight RGBA bytes.
pub fn straight_to_pixmap(rgba: &[u8], w: u32, h: u32) -> Option<Pixmap> {
    if rgba.len() != (w as usize) * (h as usize) * 4 || w == 0 || h == 0 {
        return None;
    }
    let mut premul = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        let a = px[3] as u32;
        let (r, g, b) = if a == 255 || a == 0 {
            (px[0] as u32, px[1] as u32, px[2] as u32)
        } else {
            (
                (px[0] as u32 * a + 127) / 255,
                (px[1] as u32 * a + 127) / 255,
                (px[2] as u32 * a + 127) / 255,
            )
        };
        premul.extend_from_slice(&[r as u8, g as u8, b as u8, a as u8]);
    }
    Pixmap::from_vec(premul, tiny_skia::IntSize::from_wh(w, h)?)
}

/// Crops a sub-rect (image-pixel coordinates) from straight RGBA and
/// returns it as a premultiplied Pixmap.
fn crop_straight(rgba: &[u8], iw: u32, ih: u32, x: f32, y: f32, w: f32, h: f32) -> Option<Pixmap> {
    let x0 = x.floor().max(0.0) as u32;
    let y0 = y.floor().max(0.0) as u32;
    let x1 = (x + w).ceil().min(iw as f32) as u32;
    let y1 = (y + h).ceil().min(ih as f32) as u32;
    let cw = x1.saturating_sub(x0);
    let ch = y1.saturating_sub(y0);
    if cw == 0 || ch == 0 {
        return Pixmap::new(1, 1);
    }
    let mut premul = Vec::with_capacity((cw * ch * 4) as usize);
    let stride = (iw * 4) as usize;
    for row in y0..y1 {
        let start = row as usize * stride + (x0 * 4) as usize;
        let end = (start + (cw * 4) as usize).min(rgba.len());
        let mut line = rgba[start..end].to_vec();
        if line.len() < (cw * 4) as usize {
            line.resize((cw * 4) as usize, 0);
        }
        for px in line.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a != 255 && a != 0 {
                px[0] = ((px[0] as u32 * a + 127) / 255).min(255) as u8;
                px[1] = ((px[1] as u32 * a + 127) / 255).min(255) as u8;
                px[2] = ((px[2] as u32 * a + 127) / 255).min(255) as u8;
            }
        }
        premul.extend_from_slice(&line);
    }
    Pixmap::from_vec(premul, tiny_skia::IntSize::from_wh(cw, ch)?)
}

fn rect_from(x: f32, y: f32, w: f32, h: f32) -> Option<tiny_skia::Rect> {
    if !w.is_finite() || !h.is_finite() || !x.is_finite() || !y.is_finite() || w == 0.0 || h == 0.0
    {
        return None;
    }
    let right = x + w;
    let bottom = y + h;
    tiny_skia::Rect::from_ltrb(x.min(right), y.min(bottom), x.max(right), y.max(bottom))
}

fn all_finite(v: &[f32]) -> bool {
    v.iter().all(|f| f.is_finite())
}

// ---------------------------------------------------------------------------
// Text (cosmic-text)
// ---------------------------------------------------------------------------

thread_local! {
    static FONT_SYSTEM: RefCell<FontSystem> = RefCell::new(FontSystem::new());
}

/// One shaped glyph: device-ish position within the text block (origin at
/// the baseline start) + its cache key.
struct ShapedGlyph {
    x: f32,
    y: f32,
    cache_key: cosmic_text::CacheKey,
}

struct ShapedText {
    glyphs: Vec<ShapedGlyph>,
    width: f32,
    ascent: f32,
    descent: f32,
}

/// Shapes a single line at the spec's size. Positions are relative to
/// (0, baseline) at scale 1.
fn shape_text(text: &str, font: &FontSpec) -> ShapedText {
    FONT_SYSTEM.with(|fs| {
        let mut fs = fs.borrow_mut();
        let metrics = Metrics::new(font.size, font.size * 1.3);
        let mut buffer = Buffer::new(&mut fs, metrics);
        let mut attrs = Attrs::new();
        attrs = match font.family.as_str() {
            "serif" => attrs.family(Family::Serif),
            "monospace" | "ui-monospace" => attrs.family(Family::Monospace),
            "cursive" => attrs.family(Family::Cursive),
            "fantasy" => attrs.family(Family::Fantasy),
            other => attrs.family(Family::Name(other)),
        };
        let weight = match font.weight {
            w if w < 450 => cosmic_text::Weight::NORMAL,
            w if w < 550 => cosmic_text::Weight::MEDIUM,
            w if w < 650 => cosmic_text::Weight::SEMIBOLD,
            w if w < 800 => cosmic_text::Weight::BOLD,
            _ => cosmic_text::Weight::BLACK,
        };
        attrs = attrs.weight(weight);
        if font.italic {
            attrs = attrs.style(Style::Italic);
        }
        buffer.set_text(&mut fs, text, &attrs, cosmic_text::Shaping::Advanced, None);
        buffer.set_wrap(&mut fs, cosmic_text::Wrap::None);
        buffer.shape_until_scroll(&mut fs, false);
        let mut glyphs = Vec::new();
        let mut width = 0.0f32;
        let mut ascent = 0.0f32;
        let mut descent = 0.0f32;
        for run in buffer.layout_runs() {
            ascent = ascent.max(run.line_y);
            descent = descent.max((run.line_height - run.line_y).max(0.0));
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((0.0, 0.0), 1.0);
                glyphs.push(ShapedGlyph {
                    x: physical.x as f32,
                    y: physical.y as f32,
                    cache_key: physical.cache_key,
                });
                width = width.max(glyph.x + glyph.w);
            }
        }
        ShapedText {
            glyphs,
            width,
            ascent,
            descent,
        }
    })
}

impl Canvas2D {
    /// Measures text: (advance width, ascent, descent).
    pub fn measure_text(&self, text: &str) -> (f32, f32, f32) {
        let shaped = shape_text(text, &self.state.font);
        (shaped.width, shaped.ascent, shaped.descent)
    }

    /// `fillText` / `strokeText`.
    pub fn fill_text(&mut self, text: &str, x: f32, y: f32, stroke: bool) {
        if text.is_empty() {
            return;
        }
        let shaped = shape_text(text, &self.state.font);
        if shaped.glyphs.is_empty() {
            return;
        }
        // Baseline offset.
        let dy = match self.state.text_baseline {
            Baseline::Top => shaped.ascent,
            Baseline::Hanging => shaped.ascent * 0.8,
            Baseline::Middle => (shaped.ascent - shaped.descent) / 2.0,
            Baseline::Alphabetic | Baseline::Ideographic => 0.0,
            Baseline::Bottom => -shaped.descent,
        };
        // Alignment offset.
        let dx = match self.state.text_align {
            TextAlign::Start | TextAlign::Left => 0.0,
            TextAlign::Center => -shaped.width / 2.0,
            TextAlign::End | TextAlign::Right => -shaped.width,
        };
        // Clone the paint source out of state so the per-glyph color
        // closure never borrows `self` (blit_text needs &mut self).
        let source = if stroke {
            self.state.stroke.clone()
        } else {
            self.state.fill.clone()
        };
        let shadow = self.state.shadow_color;
        let alpha = self.state.global_alpha;
        // Shadow pass: rasterize the text block once in shadow color.
        if self.shadow_active() {
            self.blit_text(&shaped, x + dx, y + dy, |_, _| shadow.skia(), 1.0);
        }
        if stroke {
            // Outline approximation: 8 directions at lineWidth/2.
            let half = (self.state.line_width / 2.0).max(0.5);
            for (ox, oy) in [
                (half, 0.0),
                (-half, 0.0),
                (0.0, half),
                (0.0, -half),
                (half * 0.7, half * 0.7),
                (-half * 0.7, half * 0.7),
                (half * 0.7, -half * 0.7),
                (-half * 0.7, -half * 0.7),
            ] {
                let bx = x + dx + ox;
                let by = y + dy + oy;
                let src = &source;
                self.blit_text(&shaped, bx, by, |cx, cy| glyph_color(src, cx, cy), alpha);
            }
        } else {
            let src = &source;
            self.blit_text(
                &shaped,
                x + dx,
                y + dy,
                |cx, cy| glyph_color(src, cx, cy),
                alpha,
            );
        }
    }

    /// Rasterizes shaped glyphs into a transparent block, then composites
    /// the block through the CTM (full affine text).
    fn blit_text(
        &mut self,
        shaped: &ShapedText,
        origin_x: f32,
        origin_y: f32,
        color_of: impl Fn(f32, f32) -> tiny_skia::Color,
        alpha: f32,
    ) {
        let Some(block) = Pixmap::new(
            (shaped.width.ceil() as u32).max(1).saturating_add(2),
            (shaped.ascent + shaped.descent).ceil().max(1.0) as u32,
        ) else {
            return;
        };
        let mut block = block;
        let baseline = shaped.ascent;
        FONT_SYSTEM.with(|fs| {
            let mut fs = fs.borrow_mut();
            let mut cache = cosmic_text::SwashCache::new();
            for glyph in &shaped.glyphs {
                let Some(image) = cache.get_image(&mut fs, glyph.cache_key) else {
                    continue;
                };
                let color = color_of(
                    (origin_x + glyph.x + 4.0).max(0.0),
                    (origin_y - baseline).max(0.0),
                );
                let mut color = color;
                if alpha < 1.0 {
                    color = tiny_skia::Color::from_rgba(
                        color.red() * alpha,
                        color.green() * alpha,
                        color.blue() * alpha,
                        color.alpha() * alpha,
                    )
                    .unwrap_or(color);
                }
                blit_glyph_mask(
                    &mut block,
                    image,
                    glyph.x as i32,
                    baseline as i32 + glyph.y as i32,
                    color,
                );
            }
        });
        // Composite: user (origin_x + u, origin_y - ascent + v) -> device.
        let ctm = self.state.ctm;
        let transform =
            Transform::from_translate(origin_x, origin_y - shaped.ascent).post_concat(ctm);
        let paint = PixmapPaint {
            opacity: 1.0,
            blend_mode: self.state.composite.skia(),
            quality: FilterQuality::Nearest,
        };
        let clip = self.state.clip.clone();
        match clip.as_ref() {
            Some(m) => {
                self.pixmap
                    .as_mut()
                    .draw_pixmap(0, 0, block.as_ref(), &paint, transform, Some(m))
            }
            None => self
                .pixmap
                .as_mut()
                .draw_pixmap(0, 0, block.as_ref(), &paint, transform, None),
        }
    }
}

/// Per-glyph color for text fills (gradient/pattern sources sample the
/// shader at the glyph position — a documented approximation).
fn glyph_color(source: &PaintSource, cx: f32, cy: f32) -> tiny_skia::Color {
    match source {
        PaintSource::Color(c) => c.skia(),
        PaintSource::Gradient { def, stops } => sample_gradient(def, stops, cx, cy),
        PaintSource::Pattern { .. } => tiny_skia::Color::from_rgba(0.5, 0.5, 0.5, 1.0).unwrap(),
    }
}

/// Blits one swash glyph (mask or color bitmap) onto a pixmap.
fn blit_glyph_mask(
    pixmap: &mut Pixmap,
    image: &cosmic_text::SwashImage,
    x: i32,
    y: i32,
    color: tiny_skia::Color,
) {
    use cosmic_text::SwashContent;
    let (cr, cg, cb, ca) = (
        (color.red() * 255.0).round().clamp(0.0, 255.0) as u8,
        (color.green() * 255.0).round().clamp(0.0, 255.0) as u8,
        (color.blue() * 255.0).round().clamp(0.0, 255.0) as u8,
        (color.alpha() * 255.0).round().clamp(0.0, 255.0) as u8,
    );
    let base_x = x + image.placement.left;
    let base_y = y - image.placement.top;
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    let stride = pixmap.width() as usize * 4;
    match image.content {
        SwashContent::SubpixelMask => {
            // Subpixel coverage: treat as a mask approximation.
            let mut i = 0usize;
            for off_y in 0..image.placement.height as i32 {
                let row_y = base_y + off_y;
                if row_y < 0 || row_y >= h {
                    i += image.placement.width as usize * 4;
                    continue;
                }
                for off_x in 0..image.placement.width as i32 {
                    let px = base_x + off_x;
                    // SubpixelMask is RGB subpixel coverage; approximate
                    // the alpha by the average.
                    if px >= 0 && px < w {
                        let r = image.data[i];
                        let g = image.data[i + 1];
                        let b = image.data[i + 2];
                        let a = image.data[i + 3];
                        let avg = ((r as u32 + g as u32 + b as u32) / 3).min(255) as u8;
                        let alpha = ((avg as u32 * ca as u32 + 127) / 255).min(255) as u8;
                        let _ = a;
                        if alpha > 0 {
                            blend_pixel(
                                pixmap,
                                row_y as usize * stride + px as usize * 4,
                                cr,
                                cg,
                                cb,
                                alpha,
                            );
                        }
                    }
                    i += 4;
                }
            }
        }
        SwashContent::Mask => {
            let mut i = 0usize;
            for off_y in 0..image.placement.height as i32 {
                let row_y = base_y + off_y;
                if row_y < 0 || row_y >= h {
                    i += image.placement.width as usize;
                    continue;
                }
                for off_x in 0..image.placement.width as i32 {
                    let a = image.data[i];
                    if a > 0 {
                        let px = base_x + off_x;
                        if px >= 0 && px < w {
                            let alpha = ((a as u32 * ca as u32 + 127) / 255).min(255) as u8;
                            blend_pixel(
                                pixmap,
                                row_y as usize * stride + px as usize * 4,
                                cr,
                                cg,
                                cb,
                                alpha,
                            );
                        }
                    }
                    i += 1;
                }
            }
        }
        SwashContent::Color => {
            let mut i = 0usize;
            for off_y in 0..image.placement.height as i32 {
                let row_y = base_y + off_y;
                if row_y < 0 || row_y >= h {
                    i += image.placement.width as usize * 4;
                    continue;
                }
                for off_x in 0..image.placement.width as i32 {
                    let px = base_x + off_x;
                    if px >= 0 && px < w {
                        let b = image.data[i];
                        let g = image.data[i + 1];
                        let r = image.data[i + 2];
                        let a = image.data[i + 3];
                        if a > 0 {
                            blend_pixel(
                                pixmap,
                                row_y as usize * stride + px as usize * 4,
                                r,
                                g,
                                b,
                                a,
                            );
                        }
                    }
                    i += 4;
                }
            }
        }
    }
}

/// Source-over blend of one premultiplied pixel.
fn blend_pixel(pixmap: &mut Pixmap, offset: usize, r: u8, g: u8, b: u8, a: u8) {
    let data = pixmap.data_mut();
    if offset + 3 >= data.len() {
        return;
    }
    if a == 255 {
        data[offset] = r;
        data[offset + 1] = g;
        data[offset + 2] = b;
        data[offset + 3] = 255;
        return;
    }
    if a == 0 {
        return;
    }
    let dst_a = data[offset + 3] as u32;
    let out_a = a as u32 + (dst_a * (255 - a as u32) + 127) / 255;
    if out_a == 0 {
        return;
    }
    let inv = 255 - a as u32;
    data[offset] = blend_channel(r as u32, data[offset] as u32, inv);
    data[offset + 1] = blend_channel(g as u32, data[offset + 1] as u32, inv);
    data[offset + 2] = blend_channel(b as u32, data[offset + 2] as u32, inv);
    data[offset + 3] = out_a.min(255) as u8;
}

fn blend_channel(src: u32, dst: u32, inv: u32) -> u8 {
    // Premultiplied source-over: out = src + dst * (1 - a).
    ((src.saturating_mul(255) + dst.saturating_mul(inv)) / 255).min(255) as u8
}

/// Samples a gradient at a point (for gradient text fills).
fn sample_gradient(def: &GradientDef, stops: &[Stop], x: f32, y: f32) -> tiny_skia::Color {
    let t = match def {
        GradientDef::Linear { x0, y0, x1, y1 } => {
            let dx = x1 - x0;
            let dy = y1 - y0;
            let len2 = dx * dx + dy * dy;
            if len2 < 1e-9 {
                0.5
            } else {
                (((x - x0) * dx + (y - y0) * dy) / len2).clamp(0.0, 1.0)
            }
        }
        GradientDef::Radial {
            x0,
            y0,
            r0,
            x1,
            y1,
            r1,
        } => {
            let d = (x - x1).hypot(y - y1);
            let r1c = r1.max(1e-3);
            let t = (d / r1c).clamp(0.0, 1.0);
            let _ = (x0, y0, r0);
            t
        }
    };
    let mut sorted = stops.to_vec();
    sorted.sort_by_key(|s| (s.offset * 1000.0) as i32);
    if sorted.is_empty() {
        return tiny_skia::Color::BLACK;
    }
    if t <= sorted[0].offset {
        return sorted[0].color.skia();
    }
    if t >= sorted[sorted.len() - 1].offset {
        return sorted[sorted.len() - 1].color.skia();
    }
    for w in sorted.windows(2) {
        let (a, b) = (w[0], w[1]);
        if t >= a.offset && t <= b.offset {
            let span = (b.offset - a.offset).max(1e-6);
            let f = (t - a.offset) / span;
            let lerp = |p: f32, q: f32| p + (q - p) * f;
            return tiny_skia::Color::from_rgba(
                lerp(a.color.r, b.color.r).clamp(0.0, 1.0),
                lerp(a.color.g, b.color.g).clamp(0.0, 1.0),
                lerp(a.color.b, b.color.b).clamp(0.0, 1.0),
                lerp(a.color.a, b.color.a).clamp(0.0, 1.0),
            )
            .unwrap_or(tiny_skia::Color::BLACK);
        }
    }
    sorted[0].color.skia()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn px(c: &Canvas2D, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let p = c.pixmap.pixel(x, y).unwrap();
        // Demultiply for comparison.
        let a = p.alpha() as u32;
        if a == 0 {
            return (0, 0, 0, 0);
        }
        (
            ((p.red() as u32 * 255 + a / 2) / a).min(255) as u8,
            ((p.green() as u32 * 255 + a / 2) / a).min(255) as u8,
            ((p.blue() as u32 * 255 + a / 2) / a).min(255) as u8,
            a as u8,
        )
    }

    #[test]
    fn fill_rect_and_clear() {
        let mut c = Canvas2D::new(100, 80);
        c.set_fill_color(Color {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        });
        c.fill_rect(10.0, 10.0, 30.0, 20.0);
        assert_eq!(px(&c, 20, 20), (255, 0, 0, 255));
        assert_eq!(px(&c, 5, 5), (0, 0, 0, 0));
        c.clear_rect(15.0, 15.0, 5.0, 5.0);
        assert_eq!(px(&c, 17, 17), (0, 0, 0, 0));
        assert_eq!(px(&c, 12, 12), (255, 0, 0, 255));
    }

    #[test]
    fn global_alpha_fill() {
        let mut c = Canvas2D::new(50, 50);
        c.set_fill_color(Color {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        });
        c.set_global_alpha(0.5);
        c.fill_rect(0.0, 0.0, 50.0, 50.0);
        let p = c.pixmap.pixel(25, 25).unwrap();
        assert_eq!(p.alpha(), 128);
    }

    #[test]
    fn save_restore_transform() {
        let mut c = Canvas2D::new(100, 100);
        c.save();
        c.translate(50.0, 50.0);
        c.set_fill_color(Color {
            r: 0.0,
            g: 1.0,
            b: 0.0,
            a: 1.0,
        });
        c.fill_rect(-10.0, -10.0, 20.0, 20.0);
        c.restore();
        // Fill color and CTM restore: the post-restore fill is black at the
        // identity CTM (device 0..10), not translated.
        assert_eq!(px(&c, 50, 50), (0, 255, 0, 255));
        c.fill_rect(0.0, 0.0, 10.0, 10.0);
        assert_eq!(px(&c, 2, 2), (0, 0, 0, 255)); // restored black state fill
        assert_eq!(px(&c, 55, 5), (0, 0, 0, 0)); // untranslated
    }

    #[test]
    fn linear_gradient_fill() {
        let mut c = Canvas2D::new(100, 10);
        let g = c.create_gradient(GradientDef::Linear {
            x0: 0.0,
            y0: 0.0,
            x1: 100.0,
            y1: 0.0,
        });
        c.gradient_add_stop(
            g,
            0.0,
            Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
        );
        c.gradient_add_stop(
            g,
            1.0,
            Color {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
        );
        c.set_fill_gradient(g);
        c.fill_rect(0.0, 0.0, 100.0, 10.0);
        let left = c.pixmap.pixel(2, 5).unwrap().red();
        let right = c.pixmap.pixel(97, 5).unwrap().red();
        assert!(
            right > left + 30,
            "gradient must go dark -> light, {left} vs {right}"
        );
    }

    #[test]
    fn path_arc_and_fill() {
        let mut c = Canvas2D::new(80, 80);
        c.begin_path();
        c.arc(40.0, 40.0, 30.0, 0.0, std::f32::consts::TAU, false);
        c.set_fill_color(Color {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            a: 1.0,
        });
        c.fill(false);
        assert_eq!(px(&c, 40, 40), (0, 0, 255, 255));
        assert_eq!(px(&c, 3, 3), (0, 0, 0, 0));
        assert_eq!(px(&c, 40, 10), (0, 0, 255, 255));
    }

    #[test]
    fn stroke_rect_path() {
        let mut c = Canvas2D::new(60, 60);
        c.set_fill_color(Color {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        });
        // The STROKE source drives strokeRect (fill stays black: a previous
        // bug painted strokes in fillStyle color).
        c.set_stroke_color(Color {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        });
        c.set_line_width(3.0);
        c.stroke_rect(10.0, 10.0, 40.0, 40.0);
        // Top stroke band: y in [8.5, 11.5]; y=10 is its center (full alpha).
        assert_eq!(px(&c, 30, 10), (255, 0, 0, 255));
        assert_eq!(px(&c, 30, 20), (0, 0, 0, 0), "interior stays empty");
    }

    #[test]
    fn stroke_uses_stroke_style_not_fill() {
        let mut c = Canvas2D::new(60, 60);
        c.set_fill_color(Color {
            r: 0.0,
            g: 1.0,
            b: 0.0,
            a: 1.0,
        }); // green fill
        c.set_stroke_color(Color {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            a: 1.0,
        }); // blue stroke
        c.set_line_width(4.0);
        c.stroke_rect(10.0, 10.0, 40.0, 40.0);
        assert_eq!(
            px(&c, 30, 10),
            (0, 0, 255, 255),
            "stroke paints in strokeStyle"
        );
        assert_eq!(px(&c, 30, 30), (0, 0, 0, 0), "interior untouched by stroke");
    }

    #[test]
    fn clip_restricts_fill() {
        let mut c = Canvas2D::new(100, 100);
        c.begin_path();
        c.rect(25.0, 25.0, 50.0, 50.0);
        c.clip(false);
        c.set_fill_color(Color {
            r: 0.0,
            g: 1.0,
            b: 0.0,
            a: 1.0,
        });
        c.fill_rect(0.0, 0.0, 100.0, 100.0);
        assert_eq!(px(&c, 50, 50), (0, 255, 0, 255));
        assert_eq!(px(&c, 5, 5), (0, 0, 0, 0), "outside clip stays empty");
    }

    #[test]
    fn image_data_roundtrip() {
        let mut c = Canvas2D::new(20, 20);
        c.set_fill_color(Color {
            r: 0.2,
            g: 0.4,
            b: 0.6,
            a: 1.0,
        });
        c.fill_rect(0.0, 0.0, 20.0, 20.0);
        let (data, w, h) = c.get_image_data(0, 0, 20, 20).unwrap();
        assert_eq!((w, h), (20, 20));
        assert_eq!(&data[0..4], &[51, 102, 153, 255]);
        let mut data2 = data.clone();
        data2[0] = 255;
        data2[1] = 255;
        data2[2] = 255;
        data2[3] = 255;
        c.put_image_data(&data2, 20, 20, 0, 0);
        assert_eq!(px(&c, 0, 0), (255, 255, 255, 255));
        assert_eq!(px(&c, 10, 10), (51, 102, 153, 255));
    }

    #[test]
    fn draw_image_scales() {
        let mut c = Canvas2D::new(60, 60);
        // 2x2 image: red, green / blue, white.
        let img = DecodedImage {
            width: 2,
            height: 2,
            rgba: std::sync::Arc::new(vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ]),
        };
        c.draw_image_scaled(&img, 10.0, 10.0, 40.0, 40.0);
        assert_eq!(px(&c, 15, 15), (255, 0, 0, 255));
        assert_eq!(px(&c, 45, 15), (0, 255, 0, 255));
        assert_eq!(px(&c, 15, 45), (0, 0, 255, 255));
        assert_eq!(px(&c, 45, 45), (255, 255, 255, 255));
        // 9-arg crop: only the blue pixel, blown up.
        c.draw_image_9(&img, 0.0, 1.0, 1.0, 1.0, 10.0, 10.0, 20.0, 20.0);
        assert_eq!(px(&c, 15, 15), (0, 0, 255, 255));
    }

    #[test]
    fn composite_multiply_darkens() {
        let mut c = Canvas2D::new(30, 30);
        c.set_fill_color(Color {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        });
        c.fill_rect(0.0, 0.0, 30.0, 30.0);
        c.set_composite(CompositeOp::Multiply);
        c.set_fill_color(Color {
            r: 0.5,
            g: 0.5,
            b: 0.5,
            a: 1.0,
        });
        c.fill_rect(0.0, 0.0, 30.0, 30.0);
        let p = c.pixmap.pixel(15, 15).unwrap();
        assert_eq!(p.red(), 128);
    }

    #[test]
    fn to_png_bytes_magic() {
        let mut c = Canvas2D::new(8, 8);
        c.set_fill_color(Color {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        });
        c.fill_rect(0.0, 0.0, 8.0, 8.0);
        let png = c.to_png_bytes().unwrap();
        assert_eq!(&png[0..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn fill_text_paints_glyphs() {
        let mut c = Canvas2D::new(120, 40);
        c.set_font(parse_font("16px sans-serif").unwrap());
        c.set_fill_color(Color::BLACK);
        c.fill_text("Hello", 5.0, 25.0, false);
        // Some dark pixels in the text band.
        let dark = (0..120)
            .flat_map(|x| (0..40).map(move |y| (x, y)))
            .filter(|&(x, y)| {
                c.pixmap
                    .pixel(x, y)
                    .map(|p| p.alpha() > 40)
                    .unwrap_or(false)
            })
            .count();
        assert!(dark > 10, "text must paint something, got {dark} pixels");
        let (w, _, _) = c.measure_text("Hello");
        assert!(w > 20.0, "measureText width, got {w}");
    }

    #[test]
    fn font_parse() {
        let f = parse_font("bold 24px Arial").unwrap();
        assert_eq!((f.size, f.weight, f.family.as_str()), (24.0, 700, "Arial"));
        let f =
            parse_font("italic small-caps bold 16px/1.5 \"Helvetica Neue\", sans-serif").unwrap();
        assert!(f.italic);
        assert_eq!(f.size, 16.0);
        assert_eq!(f.family, "Helvetica Neue, sans-serif");
        let f = parse_font("10px monospace").unwrap();
        assert_eq!(f.family, "monospace");
        assert!(parse_font("bold").is_none());
    }

    #[test]
    fn dashed_stroke() {
        let mut c = Canvas2D::new(200, 10);
        c.set_line_width(2.0);
        c.set_line_dash(vec![10.0, 10.0]);
        c.begin_path();
        c.move_to(0.0, 5.0);
        c.line_to(200.0, 5.0);
        c.set_fill_color(Color::BLACK);
        c.stroke();
        // Gaps: pixel at x=15 (inside the second dash gap) should be empty.
        // Dash pattern: 0-10 on, 10-20 off, 20-30 on...
        let on = c
            .pixmap
            .pixel(5, 5)
            .map(|p| p.alpha() > 100)
            .unwrap_or(false);
        let off = c
            .pixmap
            .pixel(15, 5)
            .map(|p| p.alpha() > 100)
            .unwrap_or(false);
        assert!(on, "x=5 inside a dash");
        assert!(!off, "x=15 inside a gap");
    }

    #[test]
    fn rotate_transforms_draw() {
        let mut c = Canvas2D::new(40, 40);
        c.translate(20.0, 20.0);
        c.rotate(std::f32::consts::FRAC_PI_2);
        c.set_fill_color(Color {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        });
        c.fill_rect(-15.0, -15.0, 30.0, 30.0);
        // Square rotated 90° around the center — same coverage (5..35)².
        assert_eq!(px(&c, 20, 20), (255, 0, 0, 255));
        assert_eq!(px(&c, 20, 6), (255, 0, 0, 255));
        assert_eq!(px(&c, 20, 4), (0, 0, 0, 0), "above the square");
        assert_eq!(px(&c, 2, 2), (0, 0, 0, 0), "corner outside");
    }

    #[test]
    fn shadow_blur_paints_halo() {
        let mut c = Canvas2D::new(100, 100);
        c.set_shadow_color(Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.8,
        });
        c.set_shadow_blur(6.0);
        c.set_shadow_offset(8.0, 8.0);
        c.set_fill_color(Color {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        });
        c.fill_rect(30.0, 30.0, 20.0, 20.0);
        // Shadow region (offset +8): outside the shape, alpha > 0.
        let a = c.pixmap.pixel(40, 52).map(|p| p.alpha()).unwrap_or(0);
        assert!(a > 0, "shadow must bleed below-right");
        // Shape still solid.
        assert_eq!(px(&c, 40, 40), (255, 0, 0, 255));
    }

    #[test]
    fn is_point_in_path_hit() {
        let mut c = Canvas2D::new(50, 50);
        c.begin_path();
        c.arc(25.0, 25.0, 10.0, 0.0, std::f32::consts::TAU, false);
        assert!(c.is_point_in_path(25.0, 25.0, false));
        assert!(!c.is_point_in_path(2.0, 2.0, false));
    }

    #[test]
    fn resize_resets() {
        let mut c = Canvas2D::new(20, 20);
        c.set_fill_color(Color {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        });
        c.fill_rect(0.0, 0.0, 20.0, 20.0);
        c.resize(40, 30);
        assert_eq!((c.width(), c.height()), (40, 30));
        assert_eq!(c.pixmap.pixel(10, 10).map(|p| p.alpha()), Some(0));
    }
}

#[cfg(test)]
mod put_offset_tests {
    use super::*;

    #[test]
    fn put_at_offset_after_fill() {
        let mut c = Canvas2D::new(40, 40);
        c.set_fill_color(Color {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        });
        c.fill_rect(0.0, 0.0, 40.0, 40.0);
        // 4x4 green block placed at (10,10): covers (10..13)².
        let mut data = Vec::new();
        for _ in 0..16 {
            data.extend_from_slice(&[0, 255, 0, 255]);
        }
        c.put_image_data(&data, 4, 4, 10, 10);
        let p = c.pixmap.pixel(12, 12).unwrap();
        assert_eq!(
            (p.red(), p.green(), p.blue()),
            (0, 255, 0),
            "put at offset must write"
        );
        let p2 = c.pixmap.pixel(20, 20).unwrap();
        assert_eq!(
            (p2.red(), p2.green(), p2.blue()),
            (255, 0, 0),
            "outside stays red"
        );
    }
}
