use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;
use rowser_rendering::display_list::build_display_list;
use rowser_rendering::painter::{Painter, RenderOptions};
use rowser_rendering::DrawCmd;
use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let html_path = args.next().expect("html");
    let css_paths: Vec<String> = args.collect();
    let mut html = String::new();
    std::fs::File::open(&html_path)
        .unwrap()
        .read_to_string(&mut html)
        .unwrap();
    let media = MediaContext {
        width: 1280.0,
        height: 800.0,
        dark_mode: false,
    };
    let mut sheets = Vec::new();
    for p in css_paths {
        let mut css = String::new();
        std::fs::File::open(&p)
            .unwrap()
            .read_to_string(&mut css)
            .unwrap();
        sheets.push(parse_stylesheet(&css, &media));
    }
    let doc = parse_html(html.as_bytes());
    let mut engine = LayoutEngine::new();
    let (styles, layout) = engine.layout_document(
        &doc.dom,
        &sheets,
        &media,
        Viewport {
            width: 1280.0,
            height: 800.0,
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
    let text_cmds = list
        .commands
        .iter()
        .filter(|c| matches!(c, DrawCmd::Text { .. }))
        .count();
    let rect_cmds = list
        .commands
        .iter()
        .filter(|c| matches!(c, DrawCmd::Rect { .. }))
        .count();
    println!(
        "commands: total={} rect={} text={}",
        list.commands.len(),
        rect_cmds,
        text_cmds
    );
    println!("text runs: {}", layout.text.len());
    let mut painter = Painter::new();
    let frame = painter
        .render(
            &list,
            RenderOptions {
                viewport_width: 1280,
                viewport_height: 800,
                scroll_y: 0.0,
                background: rowser_parsing::cascade::Rgba::new_opaque(255, 255, 255),
                ..Default::default()
            },
            &mut engine.font_system,
        )
        .unwrap();
    frame.save_png("/tmp/wiki-offline-top.png").unwrap();
    println!("saved /tmp/wiki-offline-top.png");
}
