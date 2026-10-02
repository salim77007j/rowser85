use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;
use rowser_rendering::display_list::build_display_list;
use rowser_rendering::painter::{Painter, RenderOptions};

fn main() {
    let html = br#"<html><body>
<p>A web browser is an application for accessing websites. When a user requests a web page from a particular <a href="/wiki/Web_server">server</a>/<a href="/wiki/Web_browser">parser</a> and then displays it.</p>
<p>The most widely used browsers are <a href="/wiki/Google_Chrome">Google Chrome</a> (~69% market share).</p>
</body></html>"#;
    let doc = parse_html(html);
    let media = MediaContext::default();
    let sheets = vec![parse_stylesheet("", &media)];
    let mut engine = LayoutEngine::new();
    let (styles, layout) = engine.layout_document(
        &doc.dom,
        &sheets,
        &media,
        Viewport {
            width: 1000.0,
            height: 200.0,
        },
        &Default::default(),
    );
    let list = build_display_list(
        &doc.dom,
        &styles,
        &layout,
        &Default::default(),
        &Default::default(),
        &Default::default(),
    );
    let mut painter = Painter::new();
    let frame = painter
        .render(
            &list,
            RenderOptions {
                viewport_width: 1000,
                viewport_height: 200,
                ..Default::default()
            },
            &mut engine.font_system,
        )
        .unwrap();
    frame.save_png("/tmp/ws-render.png").unwrap();
    println!("saved");
}
