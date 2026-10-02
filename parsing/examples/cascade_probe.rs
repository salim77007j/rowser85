//! cascade_probe.rs — run the full cascade offline on a saved HTML + CSS.
use rowser_parsing::cascade::compute_styles;
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;

fn main() {
    let html_path = std::env::args().nth(1).expect("html path");
    let css_paths: Vec<String> = std::env::args().skip(2).collect();
    let html = std::fs::read(&html_path).expect("html");
    let document = parse_html(&html);
    let dom = document.dom;

    let media = MediaContext { width: 1360.0, height: 724.0, dark_mode: false };
    let mut sheets = Vec::new();
    for p in &css_paths {
        let css = std::fs::read_to_string(p).expect("css");
        let sheet = parse_stylesheet(&css, &media);
        println!("sheet {p}: {} rules", sheet.rules.len());
        sheets.push(sheet);
    }
    let t0 = std::time::Instant::now();
    let styles = compute_styles(&dom, &sheets, &media);
    println!("cascade: {}ms for {} nodes", t0.elapsed().as_millis(), dom.node_count());

    // body + a few probes
    for probe in ["body", "html", "div", "p", "h1", "td"] {
        let mut shown = 0;
        for node in dom.subtree_elements(dom.document()) {
            if let Some(el) = dom.element(node) {
                if &*el.name.local == probe {
                    if let Some(s) = styles.get(node) {
                        println!(
                            "{probe}#{}: display={:?} bg={:?} font={} {}px weight={} color={:?}",
                            node,
                            s.display,
                            s.background_color,
                            s.font_family,
                            s.font_size,
                            s.font_weight,
                            s.color
                        );
                        shown += 1;
                    }
                    if shown >= 2 { break; }
                }
            }
        }
    }
}
