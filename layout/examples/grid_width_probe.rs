// Does a grid container with a percent width + fr/auto tracks stretch
// like a block, in taffy 0.14 as used through our style mapping?
use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;

fn main() {
    let cases = [
        ("block-100", "<html><body style='margin:0'><div style='width:100%;height:10px' id='x'></div></body></html>"),
        ("grid-100-fr", "<html><body style='margin:0'><div style='display:grid;width:100%;grid-template-columns:1fr 1fr' id='x'><div>a</div><div>b</div></div></body></html>"),
        ("grid-100-minmax", "<html><body style='margin:0'><div style='display:grid;width:100%' id='x'><div>a</div><div>b</div></body></html>"),
        ("grid-800-fr", "<html><body style='margin:0'><div style='display:grid;width:800px;grid-template-columns:1fr 1fr' id='x'><div>a</div><div>b</div></div></body></html>"),
        ("table-100", "<html><body style='margin:0'><table width='100%' id='x'><tr><td>a</td><td>b</td></tr></table></body></html>"),
        ("nested-100", "<html><body style='margin:0'><div><div style='width:100%;height:10px' id='x'></div></div></body></html>"),
        ("wrapper-800-100", "<html><body style='margin:0'><div style='width:800px'><div style='width:100%;height:10px' id='x'></div></div></body></html>"),
        ("body-margin-8-100", "<html><body><div style='width:100%;height:10px' id='x'></div></body></html>"),
        ("pct-50", "<html><body style='margin:0'><div style='width:50%;height:10px' id='x'></div></body></html>"),
        ("pct-100-text", "<html><body style='margin:0'><div style='width:100%' id='x'>hello text</div></body></html>"),
        ("pct-100-padding", "<html><body style='margin:0'><div style='width:100%;padding:4px;height:10px' id='x'></div></body></html>"),
    ];
    for (name, html) in cases {
        let doc = parse_html(html.as_bytes());
        let sheet = parse_stylesheet("", &MediaContext::default());
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            &doc.dom,
            &[sheet],
            &MediaContext::default(),
            Viewport {
                width: 800.0,
                height: 600.0,
            },
            &Default::default(),
        );
        for (node, r) in layout.rects.iter() {
            let el = doc
                .dom
                .element(*node)
                .map(|e| e.name.local.to_string())
                .unwrap_or_default();
            let id = doc.dom.get_attr(*node, "id").unwrap_or("");
            println!(
                "{name}: <{el} id={id}> x={:.1} y={:.1} w={:.1} h={:.1}",
                r.x, r.y, r.w, r.h
            );
        }
    }
}
