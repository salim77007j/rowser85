// cascade_dump.rs — compute styles for a local DOM + CSS and dump what key
// elements actually receive (display, background, widths). Isolates
// selector-matching vs declaration failures.
use rowser_parsing::cascade::{compute_styles, DisplayMode};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;
use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let html_path = args.next().expect("html");
    let css_paths: Vec<String> = args.collect();
    let mut html = String::new();
    std::fs::File::open(&html_path)
        .unwrap()
        .read_to_string(&mut html)
        .unwrap();
    let media = MediaContext {
        width: 1360.0,
        height: 860.0,
        dark_mode: false,
    };
    let mut sheets = Vec::new();
    for p in css_paths {
        let mut css = String::new();
        std::fs::File::open(&p)
            .unwrap()
            .read_to_string(&mut css)
            .unwrap();
        sheets.push(parse_stylesheet(&css, &media));
    }
    let doc = parse_html(html.as_bytes());
    let dom = &doc.dom;
    let styles = compute_styles(dom, &sheets, &media);
    println!(
        "styles computed for {} elements; {} with non-default display",
        styles.styles.len(),
        styles
            .styles
            .values()
            .filter(|s| s.display != DisplayMode::Inline)
            .count()
    );
    let with_bg = styles
        .styles
        .values()
        .filter(|s| s.background_color.a > 0)
        .count();
    println!("elements with visible background: {with_bg}");
    for node in dom.subtree_elements(dom.document()) {
        let Some(el) = dom.element(node) else {
            continue;
        };
        let tag = el.name.local.to_string();
        let classes = el.classes.join(".");
        if let Some(s) = styles.styles.get(&node) {
            println!(
                "  <{tag} .{classes}> display={:?} color=({},{},{}) bg=({},{},{},{}) custom-n={} var-resolved-check",
                s.display, s.color.r, s.color.g, s.color.b,
                s.background_color.r, s.background_color.g, s.background_color.b, s.background_color.a, s.custom.len()
            );
        }
    }
}
