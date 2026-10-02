// grid_place_probe.rs — minimal named-area grid placement test replicating
// Wikipedia's mw-body grid: 'titlebar columnEnd' 'toolbar columnEnd' etc.
use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;

fn main() {
    let html = r#"<html><body>
<div class="mw-body">
  <div class="titlebarcx">cx</div>
  <div class="titlebar">Titlebar text</div>
  <div class="toolbar">Toolbar text</div>
  <div class="content">Content text that is reasonably long so it wraps a bit in the column.</div>
  <div class="colend">ColEnd</div>
</div>
</body></html>"#;
    let css = r#"
.mw-body { display: grid; grid-template-columns: minmax(0,59.25rem) min-content; grid-template-rows: min-content min-content min-content 1fr;
           grid-template-areas: 'titlebar-cx .' 'titlebar columnEnd' 'toolbar columnEnd' 'content columnEnd'; }
.titlebarcx { grid-area: titlebar-cx; background: #eeeeee; }
.titlebar { grid-area: titlebar; background: #ffeeaa; }
.toolbar { grid-area: toolbar; background: #aaffee; }
.content { grid-area: content; background: #ffffff; }
.colend { grid-area: columnEnd; background: #ffaadd; }
"#;
    let doc = parse_html(html.as_bytes());
    let media = MediaContext::default();
    let sheets = vec![parse_stylesheet(css, &media)];
    let mut engine = LayoutEngine::new();
    let (_, layout) = engine.layout_document(
        &doc.dom,
        &sheets,
        &media,
        Viewport {
            width: 1280.0,
            height: 800.0,
        },
        &Default::default(),
    );
    for node in doc.dom.subtree_elements(doc.dom.document()) {
        if let Some(el) = doc.dom.element(node) {
            let tag = el.name.local.to_string();
            if let Some(r) = layout.rects.get(&node) {
                println!(
                    "{tag}.{} rect=({:.0},{:.0} {:.0}x{:.0})",
                    el.classes.iter().cloned().collect::<Vec<_>>().join("."),
                    r.x,
                    r.y,
                    r.w,
                    r.h
                );
            }
        }
    }
}
