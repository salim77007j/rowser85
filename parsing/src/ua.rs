//! The user-agent (default) stylesheet.
//!
//! A minimal but faithful subset of the WHATWG HTML rendering section:
//! block structure, headings, list indentation, code font, link colors,
//! form control basics and `hidden` handling.

use std::sync::OnceLock;

use crate::css::{parse_stylesheet, MediaContext, ParsedStylesheet, StyleRuleEntry};

/// The UA stylesheet text.
pub const UA_STYLESHEET: &str = r#"
html { display: block; }
head, template, script, style, link, meta, title, noscript { display: none; }
body { display: block; margin: 8px; }
div, p, section, article, header, footer, main, aside, nav,
figure, figcaption, blockquote, fieldset, legend, details, summary,
form, hr, address, dialog, hgroup { display: block; }
ul, ol, dir { display: block; padding-left: 40px; }
li { display: list-item; }
dd { display: block; margin-left: 40px; }
dl, dt { display: block; }
pre { display: block; font-family: monospace; white-space: pre; margin: 1em 0; }
h1 { display: block; font-size: 2em; font-weight: bold; margin: 0.67em 0; }
h2 { display: block; font-size: 1.5em; font-weight: bold; margin: 0.83em 0; }
h3 { display: block; font-size: 1.17em; font-weight: bold; margin: 1em 0; }
h4 { display: block; font-weight: bold; margin: 1.33em 0; }
h5 { display: block; font-weight: bold; margin: 1.67em 0; }
h6 { display: block; font-weight: bold; margin: 2.33em 0; }
p { display: block; margin: 1em 0; }
blockquote { display: block; margin: 1em 40px; }
b, strong { font-weight: bold; }
i, em, cite, var, dfn { font-style: italic; }
u, ins { text-decoration: underline; }
s, strike, del { text-decoration: line-through; }
code, kbd, samp, tt { font-family: monospace; }
small { font-size: 0.83em; }
big { font-size: 1.17em; }
a { color: #0000ee; text-decoration: underline; }
sup { font-size: 0.83em; vertical-align: super; }
sub { font-size: 0.83em; vertical-align: sub; }
/* Tables map onto the grid engine (CSS 2.1 §17 structure): the table is a
 * grid with one track per column, rows/cells are placed explicitly by the
 * §17.4.1 occupancy algorithm (layout/src/lib.rs), which is what carries
 * colspan/rowspan. Section groups and rows are display:contents so the
 * cells participate directly in the table grid; the caption occupies the
 * first grid row spanning all columns. */
table { display: grid; }
/* Legacy layout wrappers: HN and countless classic pages nest their entire
 * layout inside <center> (or <font>). Inline-flattening such wrappers drops
 * their block descendants from the box tree entirely. */
center { display: block; text-align: center; }
font { display: inline; }
tbody, thead, tfoot { display: contents; }
tr { display: contents; }
td, th { display: block; }
col, colgroup { display: none; }
caption { display: block; text-align: center; }
img, video, canvas, svg, iframe, embed, object { display: block; }
input, textarea, select, button { display: block; }
br { display: block; }
[hidden] { display: none; }
"#;

static UA_RULES: OnceLock<Vec<StyleRuleEntry>> = OnceLock::new();

/// Returns the parsed UA rules (parsed once, cloned per call).
pub fn ua_rules(media: &MediaContext) -> Vec<StyleRuleEntry> {
    UA_RULES
        .get_or_init(|| {
            let sheet: ParsedStylesheet = parse_stylesheet(UA_STYLESHEET, media);
            sheet.rules
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ua_stylesheet_parses() {
        let rules = ua_rules(&MediaContext::default());
        assert!(
            rules.len() >= 20,
            "UA stylesheet produced {} rules",
            rules.len()
        );
    }
}
