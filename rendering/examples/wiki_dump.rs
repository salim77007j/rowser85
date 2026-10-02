use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;
use rowser_rendering::display_list::build_display_list;
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
        width: 1280.0,
        height: 800.0,
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
    let _list = build_display_list(
        &doc.dom,
        &styles,
        &layout,
        &Default::default(),
        &Default::default(),
        &Default::default(),
    );
    // Dump <p> elements and their rects
    let mut count = 0;
    for node in doc.dom.subtree_elements(doc.dom.document()) {
        if let Some(el) = doc.dom.element(node) {
            if &*el.name.local == "p" {
                if let Some(r) = layout.rects.get(&node) {
                    let txt: String = doc.dom.text_content(node).chars().take(40).collect();
                    println!(
                        "p rect=({:.0},{:.0} {:.0}x{:.0}) '{txt}'",
                        r.x, r.y, r.w, r.h
                    );
                    count += 1;
                    if count >= 12 {
                        break;
                    }
                }
            }
        }
    }
}
