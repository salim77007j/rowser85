// Offline: render the @font-face test page through the full engine path
// (parse → register fonts → style → layout → display list → paint), minus
// the UI/networking stack. Output PNG compared against Chrome ground truth.
use rowser_layout::Viewport;
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;
use rowser_rendering::display_list::build_display_list;
use rowser_rendering::painter::{Painter, RenderOptions};

fn main() {
    let html = std::fs::read_to_string("/home/z/my-project/fonttest/test.html").unwrap();
    // Extract the <style> block (single sheet page).
    let css = html
        .split("<style>")
        .nth(1)
        .and_then(|s| s.split("</style>").next())
        .expect("style block")
        .to_owned();
    let media = MediaContext {
        width: 1360.0,
        height: 860.0,
        dark_mode: false,
    };
    let sheets = vec![parse_stylesheet(&css, &media)];
    let doc = parse_html(html.as_bytes());
    let mut engine = rowser_layout::LayoutEngine::new();
    // Register the three web fonts exactly as the live engine would.
    for (family, path) in [
        ("WebTestA", "/home/z/my-project/fonttest/wtest.woff2"),
        ("WebTestB", "/home/z/my-project/fonttest/wtest.woff"),
        ("WebTestC", "/home/z/my-project/fonttest/wtest.ttf"),
    ] {
        let bytes = std::fs::read(path).unwrap();
        let ok =
            rowser_engine::font_face::register_font_bytes(&mut engine.font_system, family, &bytes);
        println!("registered {family}: {ok}");
    }
    let (styles, layout) = engine.layout_document(
        &doc.dom,
        &sheets,
        &media,
        Viewport {
            width: 1360.0,
            height: 860.0,
        },
        &Default::default(),
    );

    for (node, rect) in layout.rects.iter() {
        let name = doc
            .dom
            .element(*node)
            .map(|e| e.name.local.to_string())
            .unwrap_or_default();
        println!(
            "  rect node={} <{}>: x={:.1} y={:.1} w={:.1} h={:.1}",
            node, name, rect.x, rect.y, rect.w, rect.h
        );
    }

    // Computed style dump for body/h1/first-row.
    for probe in [15u32, 17u32, 20u32] {
        if let Some(cs) = styles.get(probe) {
            let m = &cs.margins;
            let fmt = |v: &rowser_parsing::cascade::LengthOrAuto| match v {
                rowser_parsing::cascade::LengthOrAuto::Length(l) => format!("{:?}", l),
                rowser_parsing::cascade::LengthOrAuto::Auto => "auto".into(),
            };
            println!(
                "style node={probe}: font_size={:.1} line_h={:.1} stack={:?} margins=({},{},{},{})",
                cs.font_size,
                cs.line_height,
                cs.font_stack,
                fmt(&m.top),
                fmt(&m.right),
                fmt(&m.bottom),
                fmt(&m.left)
            );
        } else {
            println!("style node={probe}: NOT FOUND");
        }
    }
    let list = build_display_list(
        &doc.dom,
        &styles,
        &layout,
        &Default::default(),
        &Default::default(),
        &Default::default(),
    );
    println!(
        "display list: {} commands, {} text runs",
        list.commands.len(),
        layout.text.len()
    );
    for run in layout.text.iter().take(10) {
        let first = run.glyphs.first();
        println!(
            "  text run node={} glyphs={} first=({:.0},{:.0})",
            run.node,
            run.glyphs.len(),
            first.map(|g| g.x as f32).unwrap_or(-1.0),
            first.map(|g| g.y as f32).unwrap_or(-1.0)
        );
    }

    for cmd in list.commands.iter() {
        if let rowser_rendering::DrawCmd::Text { run } = cmd {
            let mut line_y: Option<i32> = None;
            let mut x0: i32 = i32::MAX;
            let mut x1: i32 = i32::MIN;
            let mut n: usize = 0;
            let mut flush = |line_y: Option<i32>, x0: i32, x1: i32, n: usize| {
                if let Some(y) = line_y {
                    println!(
                        "  run node={} line@y={} x {}..{} ({} glyphs)",
                        run.node, y, x0, x1, n
                    );
                }
            };
            for g in run.glyphs.iter() {
                if line_y.map(|ly: i32| (g.y - ly).abs() > 2).unwrap_or(true) {
                    flush(line_y, x0, x1, n);
                    line_y = Some(g.y);
                    x0 = i32::MAX;
                    x1 = i32::MIN;
                    n = 0;
                }
                x0 = x0.min(g.x);
                x1 = x1.max(g.x);
                n += 1;
            }
            flush(line_y, x0, x1, n);
        }
    }
    let mut painter = Painter::new();
    let frame = painter
        .render(
            &list,
            RenderOptions {
                viewport_width: 1360,
                viewport_height: 860,
                scroll_y: 0.0,
                background: rowser_parsing::cascade::Rgba::new_opaque(255, 255, 255),
                ..Default::default()
            },
            &mut engine.font_system,
        )
        .unwrap();
    frame.save_png("/tmp/fonttest-offline.png").unwrap();
    println!("saved /tmp/fonttest-offline.png");
}
