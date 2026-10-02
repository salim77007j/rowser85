use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;

fn main() {
    // Exact Wikipedia pattern: text + <a> + text, links glued to words.
    let html = br#"<html><body>
<p>A web browser is an application for accessing websites. When a user requests a web page from a particular <a href="/wiki/Web_server">server</a>/<a href="/wiki/Web_browser">parser</a> and then displays it.</p>
<p>The most widely used browsers are <a href="/wiki/Google_Chrome">Google Chrome</a> (~69% market share).</p>
</body></html>"#;
    let doc = parse_html(html);
    let media = MediaContext::default();
    let sheets = vec![parse_stylesheet("", &media)];
    let mut engine = LayoutEngine::new();
    let (_, layout) = engine.layout_document(
        &doc.dom,
        &sheets,
        &media,
        Viewport {
            width: 1000.0,
            height: 800.0,
        },
        &Default::default(),
    );
    for run in &layout.text {
        // Reconstruct text from glyph positions? We don't have the chars in
        // TextRun. Instead check leaf text via the shapes... dump glyph x's.
        println!("run node={} glyphs={}", run.node, run.glyphs.len());
    }
    // Directly check the flattened text: simulate what build_box collects.
    // Find the p leafs through the layout result: print first-run glyph xs.
    if let Some(run) = layout.text.first() {
        let xs: Vec<i32> = run.glyphs.iter().map(|g| g.x).collect();
        println!("first run glyph xs: {:?}", &xs[..xs.len().min(20)]);
    }
}
