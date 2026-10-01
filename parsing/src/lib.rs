//! Rrowser parsing: html5ever HTML parsing, lightningcss CSS parsing and
//! the cascade engine that turns them into computed styles.
//!
//! Pipeline:
//!
//! 1. [`html::parse_html`] — bytes → [`Document`] (arena DOM) with charset
//!    sniffing (BOM + `<meta charset>`), powered by html5ever.
//! 2. [`css::parse_stylesheet`] — CSS text → rule list, powered by
//!    lightningcss, with selectors re-parsed through the `selectors` engine.
//! 3. [`cascade::compute_styles`] — DOM + rules → [`ComputedStyle`] per
//!    element, with UA stylesheet, author sheets, inline `style=""`
//!    overrides, specificity ordering and inheritance.

pub mod cascade;
pub mod css;
pub mod html;
pub mod selector_bucket;
pub mod ua;

pub use cascade::{compute_styles, ComputedStyle, DisplayMode, StyleMap};
pub use css::{parse_stylesheet, ParsedStylesheet, StyleRuleEntry};
pub use html::{parse_html, Document, ScriptInfo};

/// Shared value types used across the style pipeline.
pub mod values {
    pub use crate::cascade::{
        AlignItemsMode, BorderInfo, FlexDirectionMode, FlexWrapMode, FontStyleMode,
        JustifyContentMode, Length, LengthOrAuto, LineStyleMode, PositionMode, Rgba, TextAlignMode,
    };
}
