// wiki_probe.rs — runs the REAL Wikipedia HTML + CSS through our layout
// pipeline and dumps the grid container's computed style + layout rects.
// Usage: cargo run -p rowser-layout --example wiki_probe -- <html> <css...>
use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;
use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let html_path = args.next().expect("html path");
    let css_paths: Vec<String> = args.collect();
    let mut html = String::new();
    std::fs::File::open(&html_path)
        .unwrap()
        .read_to_string(&mut html)
        .unwrap();
    let mut sheets = Vec::new();
    let media = MediaContext {
        width: 1280.0,
        height: 800.0,
        dark_mode: false,
    };
    for css_path in css_paths {
        let mut css = String::new();
        std::fs::File::open(&css_path)
            .unwrap()
            .read_to_string(&mut css)
            .unwrap();
        sheets.push(parse_stylesheet(&css, &media));
    }
    let doc = parse_html(html.as_bytes());
    let mut engine = LayoutEngine::new();
    let (styles, layout) = engine.layout_document(
        &doc.dom,
        &sheets,
        &media,
        Viewport {
            width: 1280.0,
            height: 800.0,
        },
        &Default::default(),
    );

    // Find the interesting elements.
    for node in doc.dom.subtree_elements(doc.dom.document()) {
        if let Some(el) = doc.dom.element(node) {
            let tag = el.name.local.to_string();
            let class = el.classes.to_vec().join(".");
            let has_area = styles
                .get(node)
                .and_then(|cs| cs.grid_area.as_ref())
                .is_some();
            let interesting = has_area
                || el.id.is_some()
                || el
                    .classes
                    .iter()
                    .any(|c| c.contains("vector-page-container") || c.contains("mw-body"))
                || tag == "body"
                || tag == "html";
            if !interesting {
                continue;
            }
            if let Some(cs) = styles.get(node) {
                let rect = layout
                    .rects
                    .get(&node)
                    .map(|r| format!("({:.0},{:.0} {:.0}x{:.0})", r.x, r.y, r.w, r.h))
                    .unwrap_or_else(|| "none".into());
                println!(
                    "{tag}#{} .{class}: display={:?} grid_cols={} area={:?} rect={}",
                    el.id.clone().unwrap_or_default(),
                    cs.display,
                    cs.grid_template_columns.len() + cs.grid_template_areas.len(),
                    cs.grid_area,
                    rect
                );
            }
        }
    }
}
