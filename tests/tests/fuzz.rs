//! Robustness / fuzz tests: arbitrary inputs must never panic or hang the
//! pipeline stages (parser, css, layout, display list, painter).
//!
//! These are property tests via proptest with an adversarial corpus of
//! pathological inputs plus randomized documents.

use proptest::prelude::*;

use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::parse_html;
use rowser_rendering::{build_display_list, Painter, RenderOptions};

fn media() -> MediaContext {
    MediaContext {
        width: 1280.0,
        height: 800.0,
        dark_mode: false,
    }
}

// Any byte input must parse without panic (and reasonably fast).
proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn html_fuzz_arbitrary_bytes(bytes in any::<Vec<u8>>()) {
        let doc = parse_html(&bytes);
        prop_assert!(doc.dom.node_count() < 100_000);
    }

    #[test]
    fn css_fuzz_arbitrary_text(css in ".*") {
        let sheet = parse_stylesheet(&css, &media());
        prop_assert!(sheet.rules.len() < 100_000);
    }

    #[test]
    fn css_fuzz_structured(seed in any::<u64>()) {
        // Random-ish selectors/declarations composed from nasty fragments.
        let fragments = [
            "div", ".a", "#b", "*", "a:hover", "p:nth-child(2n+1)", "[x=y]",
            "a > b + c ~ d", ":is(a, b)", "!", "{", "}", ";", ":",
            "color:", "0", "-1", "1e999", "calc(", "calc(1px + 2%)",
            "rgb(300, -5, 0.5)", "#zzz", "url(", "\\ ", "\u{0}",
            "important", "!important", "999999999999999999px", "50vh",
            "color: red; background: blue", "width: calc(100% - var(--x))",
        ];
        let mut rng = seed;
        let mut css = String::new();
        for _ in 0..40 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let idx = ((rng >> 33) as usize) % fragments.len();
            css.push_str(fragments[idx]);
            css.push(' ');
        }
        let _ = parse_stylesheet(&css, &media());
    }

    #[test]
    fn pipeline_fuzz_random_documents(nodes in 1usize..200, classes in 0usize..30) {
        let html = synthetic_page(nodes, classes);
        let css = synthetic_css(classes);
        let doc = parse_html(html.as_bytes());
        let sheet = parse_stylesheet(&css, &media());
        let mut engine = LayoutEngine::new();
        let (styles, layout) =
            engine.layout_document(&doc.dom, &[sheet], &media(), Viewport { width: 1280.0, height: 800.0 }, &Default::default());
        let list = build_display_list(&doc.dom, &styles, &layout, &Default::default(), &Default::default(), &Default::default());
        let mut painter = Painter::new();
        let frame = painter.render(&list, RenderOptions::default(), &mut engine.font_system);
        prop_assert!(frame.is_some());
    }

    #[test]
    fn html_fuzz_malformed_structures(seed in any::<u64>()) {
        // Pathological markup: deep nesting, unterminated tags, attribute soup.
        let snippets = [
            "<div", "</div>", "<p a='", "\"", "<script>", "</script>",
            "<!--", "-->", "<!DOCTYPE", "<?php", "text", "&amp;", "&", "&#x",
            "<a b=c d=\"e\" f='g' h>", "<svg><g><g><g>", "<table><td>",
            "\u{feff}", "<style>", "<title>", "<img src=x onerror=y>",
            "<base href='javascript:'>", "<iframe>", "<template>",
        ];
        let mut rng = seed;
        let mut html = String::from("<!DOCTYPE html><html><body>");
        for _ in 0..60 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let idx = ((rng >> 33) as usize) % snippets.len();
            html.push_str(snippets[idx]);
        }
        html.push_str("</body></html>");
        let doc = parse_html(html.as_bytes());
        let sheet = parse_stylesheet("body { color: red }", &media());
        let mut engine = LayoutEngine::new();
        let _ = engine.layout_document(&doc.dom, &[sheet], &media(), Viewport::default(), &Default::default());
    }
}

fn synthetic_page(nodes: usize, classes: usize) -> String {
    let mut html = String::from("<!DOCTYPE html><html><head><title>f</title></head><body>");
    for i in 0..nodes {
        let class = if classes > 0 {
            format!(" c{}", i % classes)
        } else {
            String::new()
        };
        html.push_str(&format!(
            "<div class=\"n{i}{class}\" id=\"i{i}\"><p>Node {i} text content for shaping</p></div>"
        ));
    }
    html.push_str("</body></html>");
    html
}

fn synthetic_css(classes: usize) -> String {
    let mut css = String::from("body { margin: 4px; color: #111 } div { display: block }");
    for c in 0..classes {
        css.push_str(&format!(
            ".c{c} {{ padding: {c}px; color: rgb({c}, 0, 0) }}"
        ));
    }
    css
}
