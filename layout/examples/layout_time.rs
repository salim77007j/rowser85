//! Reproduces the github layout hang: parse HTML + all dumped stylesheets,
//! then run compute_styles + layout with a timeout watchdog thread.
use std::time::Instant;

fn main() {
    let html_path = std::env::args()
        .nth(1)
        .expect("usage: layout_time <html> [css-dir]");
    let css_dir = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/tmp/css-dump".into());

    let document = rowser_parsing::parse_html(&std::fs::read(&html_path).expect("read html"));
    let dom = std::rc::Rc::new(std::cell::RefCell::new(document.dom));
    let media = rowser_parsing::css::MediaContext {
        width: 1360.0,
        height: 860.0,
        dark_mode: false,
    };

    let mut sheets = Vec::new();
    for entry in std::fs::read_dir(&css_dir).expect("css dir") {
        let path = entry.expect("entry").path();
        if path.extension().map(|e| e == "css").unwrap_or(false) {
            let css = std::fs::read_to_string(&path).expect("css read");
            let t0 = Instant::now();
            let sheet = rowser_parsing::parse_stylesheet(&css, &media);
            println!(
                "{}: {} bytes -> {} rules ({:?})",
                path.file_name().unwrap().to_string_lossy(),
                css.len(),
                sheet.rules.len(),
                t0.elapsed()
            );
            sheets.push(sheet);
        }
    }

    // compute_styles phase
    let t0 = Instant::now();
    let styles = rowser_parsing::cascade::compute_styles(&dom.borrow(), &sheets, &media);
    println!("compute_styles: {}ms", t0.elapsed().as_millis());

    // layout phase
    let mut engine = rowser_layout::LayoutEngine::new();
    let viewport = rowser_layout::Viewport::default();
    let t1 = Instant::now();
    let layout = engine.layout_document(
        &dom.borrow(),
        &sheets,
        &media,
        viewport,
        &Default::default(),
    );
    println!(
        "layout_document (styles+layout): {}ms",
        t1.elapsed().as_millis()
    );
    let _ = (styles, layout);
    println!("done");
}
