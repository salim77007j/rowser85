fn main() {
    let css = r#"
.mw-body{display:grid;grid-template:min-content min-content min-content 1fr / minmax(0,59.25rem) min-content;grid-template-areas:'titlebar-cx .' 'titlebar columnEnd' 'toolbar columnEnd' 'content columnEnd'}
.mw-body .vector-page-titlebar{grid-area:titlebar}
.mw-body .vector-page-toolbar{grid-area:toolbar}
.mw-body #bodyContent{grid-area:content}
.mw-body .vector-column-end{grid-area:columnEnd}
"#;
    let html = r#"<html><body>
<div class="mw-body">
  <div class="vector-page-titlebar">Titlebar text</div>
  <div class="vector-page-toolbar">Toolbar text</div>
  <div id="bodyContent">Content text long enough to wrap around the column a bit more.</div>
  <div class="vector-column-end">ColEnd</div>
</div>
</body></html>"#;
    let doc = rowser_parsing::html::parse_html(html.as_bytes());
    let media = rowser_parsing::css::MediaContext::default();
    let sheets = vec![rowser_parsing::css::parse_stylesheet(css, &media)];
    let mut engine = rowser_layout::LayoutEngine::new();
    let (styles, layout) = engine.layout_document(
        &doc.dom,
        &sheets,
        &media,
        rowser_layout::Viewport {
            width: 1280.0,
            height: 800.0,
        },
        &Default::default(),
    );
    for node in doc.dom.subtree_elements(doc.dom.document()) {
        if let Some(el) = doc.dom.element(node) {
            if let Some(r) = layout.rects.get(&node) {
                if let Some(cs) = styles.get(node) {
                    println!(
                        "{} rect=({:.0},{:.0} {:.0}x{:.0}) rows={} area={:?}",
                        el.name.local,
                        r.x,
                        r.y,
                        r.w,
                        r.h,
                        cs.grid_template_rows.len(),
                        cs.grid_area
                    );
                }
            }
        }
    }
}
