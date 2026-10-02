// debug_layout.rs — dumps computed rects + glyph positions for the two
// failing probe pages to diagnose truncation and table collapse.
use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;

fn dump(name: &str, html: &str, css: &str) {
    let doc = parse_html(html.as_bytes());
    let media = MediaContext::default();
    let sheets = vec![parse_stylesheet(css, &media)];
    let mut engine = LayoutEngine::new();
    let (_, layout) = engine.layout_document(
        &doc.dom,
        &sheets,
        &media,
        Viewport {
            width: 1000.0,
            height: 700.0,
        },
        &Default::default(),
    );
    println!("== {name} ==");
    for node in doc.dom.subtree_elements(doc.dom.document()) {
        if let Some(el) = doc.dom.element(node) {
            let tag = el.name.local.to_string();
            if let Some(r) = layout.rects.get(&node) {
                println!(
                    "  {tag}{} rect=({:.0},{:.0} {:.0}x{:.0})",
                    el.classes
                        .iter()
                        .map(|c| format!(".{c}"))
                        .collect::<String>(),
                    r.x,
                    r.y,
                    r.w,
                    r.h
                );
            }
        }
    }
    for run in &layout.text {
        let xs: Vec<String> = run
            .glyphs
            .iter()
            .map(|g| format!("({},{})", g.x, g.y))
            .take(6)
            .collect();
        println!(
            "  text[{:?}] node={} glyphs={} first={:?}",
            "",
            run.node,
            run.glyphs.len(),
            xs
        );
    }
}

fn main() {
    dump(
        "flex-row",
        r#"<html><body>
<div class="nav"><div>Home</div><div>Products</div><div>Company</div><div>Contact</div></div>
</body></html>"#,
        ".nav { display: flex; flex-direction: row; gap: 20px; background: #2244aa; padding: 8px; }
.nav div { background: #dde6ff; padding: 4px 10px; }",
    );
    dump(
        "table",
        r#"<html><body>
<table><tbody>
<tr class="hdr"><td>1.</td><td>Title of the story goes here</td><td>example.com</td></tr>
<tr><td>2.</td><td>Another headline entirely</td><td>news.ycombinator.com</td></tr>
</tbody></table>
</body></html>"#,
        "td { padding: 6px 10px; } .hdr { background: #ff6600; }",
    );
}
