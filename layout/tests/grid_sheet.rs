#[cfg(test)]
mod grid_sheet_tests {
    use rowser_layout::{LayoutEngine, Viewport};
    use rowser_parsing::css::{parse_stylesheet, MediaContext};
    use rowser_parsing::html::parse_html;

    /// The sheet path (class selectors in a `<style>` block) must apply grid
    /// placements identically to inline styles.
    #[test]
    fn grid_placement_via_stylesheet() {
        let css = r#"
            .grid { display: grid; grid-template-columns: 120px 120px 120px 120px; grid-template-rows: 60px 60px; gap: 4px; width: 496px; }
            .item { background: #4a90d9; }
            .a { grid-column: 1 / 3; grid-row: 1; }
            .b { grid-column: 4; grid-row: 1; }
        "#;
        let html = br#"<html><body style="margin:0"><div class="grid">
        <div class="item a" id="a">A</div>
        <div class="item b" id="b">B</div>
        </div></body></html>"#;
        let doc = parse_html(html);
        let sheet = parse_stylesheet(css, &MediaContext::default());
        let styles = rowser_parsing::cascade::compute_styles(
            &doc.dom,
            &[sheet.clone()],
            &MediaContext::default(),
        );
        for node in doc.dom.subtree_elements(doc.dom.document()) {
            if let Some(id) = doc.dom.get_attr(node, "id") {
                if let Some(cs) = styles.get(node) {
                    println!(
                        "GRIDTEST {id} col={:?} row={:?}",
                        cs.grid_column, cs.grid_row
                    );
                }
            }
        }
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
        let mut by_id = std::collections::HashMap::new();
        for node in doc.dom.subtree_elements(doc.dom.document()) {
            if let Some(id) = dom_id(&doc.dom, node) {
                if let Some(r) = layout.rects.get(&node) {
                    by_id.insert(id, *r);
                }
            }
        }
        let a = by_id["a"];
        let b = by_id["b"];
        println!("GRIDTEST a={a:?} b={b:?}");
        assert!(
            (a.w - 244.0).abs() < 3.0,
            "a spans 2 cols (120+4+120): {a:?}"
        );
        assert!((b.x - 372.0).abs() < 3.0, "b at col 4 (x=372): {b:?}");
    }

    /// The exact fixture page the browser is screenshot-tested against
    /// (includes display:flex items, grid-area line form, spans, auto rows).
    /// Chrome ground truth (padding 4, gap 4):
    /// * a: cols 1-3 → x=4,   w=244, row 1 → y=4,   h=60
    /// * b: col 4, rows 1-3 → x=376, w=120, h=124
    /// * d: grid-area 3/2/5/4 → x=128, y=132, w=244, h=104
    #[test]
    fn grid_placement_fixture_page() {
        let html = std::fs::read_to_string("../tests/fixtures/grid-float.html").expect("fixture");
        let doc = parse_html(html.as_bytes());
        let dom = &doc.dom;
        let mut sheets = Vec::new();
        let media = MediaContext::default();
        for node in dom.subtree_elements(dom.document()) {
            if dom.element(node).is_some_and(|e| &*e.name.local == "style") {
                sheets.push(parse_stylesheet(&dom.text_content(node), &media));
            }
        }
        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            dom,
            &sheets,
            &media,
            Viewport {
                width: 1360.0,
                height: 860.0,
            },
            &Default::default(),
        );
        let mut by_id = std::collections::HashMap::new();
        for node in dom.subtree_elements(dom.document()) {
            if let Some(id) = dom.get_attr(node, "id") {
                if let Some(r) = layout.rects.get(&node) {
                    by_id.insert(id.to_string(), *r);
                }
            }
        }
        for (id, r) in &by_id {
            println!("FIXTURE {id} = {r:?}");
        }
        let a = by_id.get("a").expect("a");
        assert!(
            (a.x - 4.0).abs() < 3.0 && (a.w - 244.0).abs() < 3.0 && (a.y - 4.0).abs() < 3.0,
            "a: {a:?}"
        );
        let b = by_id.get("b").expect("b");
        assert!(
            (b.x - 376.0).abs() < 3.0 && (b.h - 124.0).abs() < 3.0,
            "b: {b:?}"
        );
        let d = by_id.get("d").expect("d");
        assert!(
            (d.x - 128.0).abs() < 3.0 && (d.y - 132.0).abs() < 3.0,
            "d: {d:?}"
        );
        assert!(
            (d.w - 244.0).abs() < 3.0 && (d.h - 104.0).abs() < 4.0,
            "d size: {d:?}"
        );
    }

    fn dom_id(dom: &rowser_dom::Dom, node: rowser_dom::NodeId) -> Option<String> {
        dom.get_attr(node, "id").map(|s| s.to_string())
    }
}
