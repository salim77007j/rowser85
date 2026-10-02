fn main() {
    // Test 1: ONLY the failing rule.
    let html =
        br#"<html><body><div class="mw-body"><div id="bodyContent">x</div></div></body></html>"#;
    let doc = rowser_parsing::html::parse_html(html);
    let css1 = ".mw-body #bodyContent { color: #123456; }";
    let media = rowser_parsing::css::MediaContext::default();
    let sheet1 = rowser_parsing::css::parse_stylesheet(css1, &media);
    let styles = rowser_parsing::cascade::compute_styles(&doc.dom, &[sheet1], &media);
    for node in doc.dom.subtree_elements(doc.dom.document()) {
        if let Some(el) = doc.dom.element(node) {
            if &*el.name.local == "div" {
                println!(
                    "test1 div#{} color={:?}",
                    el.id.clone().unwrap_or_default(),
                    styles.get(node).map(|cs| cs.color)
                );
            }
        }
    }

    // Test 2: the full grid-area rule from Wikipedia.
    let css2 = ".mw-body #bodyContent{grid-area:content}";
    let sheet2 = rowser_parsing::css::parse_stylesheet(css2, &media);
    let doc2 = rowser_parsing::html::parse_html(html);
    let styles2 = rowser_parsing::cascade::compute_styles(&doc2.dom, &[sheet2], &media);
    for node in doc2.dom.subtree_elements(doc2.dom.document()) {
        if let Some(el) = doc2.dom.element(node) {
            if &*el.name.local == "div" {
                println!(
                    "test2 div#{} grid_area={:?}",
                    el.id.clone().unwrap_or_default(),
                    styles2.get(node).and_then(|cs| cs.grid_area.clone())
                );
            }
        }
    }
}
