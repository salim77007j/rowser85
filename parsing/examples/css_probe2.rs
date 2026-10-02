// css_probe.rs — quick cascade checks for descendant/child selectors,
// inset, z-index, font stacks.
use rowser_parsing::cascade::compute_styles;
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;

fn main() {
    let html = br#"<html><body>
<div class="nav" id="n1"><div class="item">Home</div><div class="item">About</div></div>
<div class="panel">Overlay</div>
</body></html>"#;
    let css = r#"
.nav { display: flex; background: #2244aa; border: 2px solid #000000; }
.nav .item { background: #dde6ff; padding: 4px 10px; }
div.item { margin-top: 2px; }
.nav > .item { color: #112233; }
.item { color: #445566; }
#n1 { gap: 20px; }
.panel { position: absolute; top: 40px; left: 120px; z-index: 10; font-family: "Segoe UI", Arial, sans-serif; }
body { font-family: -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; }
p { font-family: "Courier New", monospace; }
"#;
    let doc = parse_html(html);
    let sheet = parse_stylesheet(css, &MediaContext::default());
    let styles = compute_styles(&doc.dom, &[sheet], &MediaContext::default());

    let mut items = Vec::new();
    for node in doc.dom.subtree_elements(doc.dom.document()) {
        if let Some(el) = doc.dom.element(node) {
            let tag = el.name.local.to_string();
            if let Some(cs) = styles.get(node) {
                items.push(format!(
                    "{tag}{}: display={:?} bg={:?} pad_l={:?} color={:?} pos={:?} top={:?} left={:?} z={:?} stack={:?}",
                    el
                        .classes
                        .iter()
                        .map(|c| format!(".{c}"))
                        .collect::<String>(),
                    cs.display,
                    cs.background_color,
                    cs.paddings.left,
                    cs.color,
                    cs.position,
                    cs.top,
                    cs.left,
                    cs.z_index,
                    cs.font_stack,
                ));
            }
        }
    }
    for line in items {
        println!("{line}");
    }
}
