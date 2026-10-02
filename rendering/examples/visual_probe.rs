//! visual_probe.rs — renders synthetic pages exercising the rendering
//! overhaul (leaf positioning, inset, stacking, font stacks, backgrounds)
//! to PNGs for visual inspection. Fast inner loop, no browser needed.
//!
//! Run: cargo run -p rowser-rendering --example visual_probe -- <outdir>

use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;
use rowser_rendering::display_list::build_display_list;
use rowser_rendering::painter::{Painter, RenderOptions};

const PAGES: &[(&str, &str, &str)] = &[
    // 0. CSS grid with template columns: the modern layout backbone
    // (Wikipedia Vector 2022, countless sites). Three tracks: fixed
    // sidebar, fluid content, fixed rail — plus a repeat(2, 1fr) card row.
    (
        "grid",
        r#"<html><body>
<div class="layout">
  <div class="side">Sidebar link 1</div>
  <div class="main">Main article content in the middle column</div>
  <div class="rail">Right rail box</div>
</div>
<div class="cards"><div>Card A</div><div>Card B</div><div>Card C</div><div>Card D</div></div>
</body></html>"#,
        r#"
.layout { display: grid; grid-template-columns: 160px 1fr 200px; gap: 12px; }
.side { background: #dde7f0; padding: 8px; }
.main { background: #ffffff; padding: 8px; border: 1px solid #999999; }
.rail { background: #f0e6dd; padding: 8px; }
.cards { display: grid; grid-template-columns: repeat(2, 1fr); gap: 10px; margin-top: 20px; }
.cards div { background: #cfe8cf; padding: 10px; }
"#,
    ),
    // 1. Flex row text positioning: three items must render side by side,
    //    NOT stacked at the container origin (the old leaf-origin bug).
    (
        "flex-row",
        r#"<html><body>
<div class="nav"><div>Home</div><div>Products</div><div>Company</div><div>Contact</div></div>
</body></html>"#,
        r#"
.nav { display: flex; flex-direction: row; gap: 20px; background: #2244aa; padding: 8px; }
.nav div { background: #dde6ff; padding: 4px 10px; }
"#,
    ),
    // 2. CSS font stack: first family absent — must resolve down the stack.
    (
        "font-stack",
        r#"<html><body>
<p class="a">Arial-class sans text 0123456789</p>
<p class="b">Serif-class text 0123456789</p>
<p class="c">Mono-class text 0123456789</p>
</body></html>"#,
        r#"
.a { font-family: -apple-system, "Segoe UI", Arial, sans-serif; font-size: 20px; }
.b { font-family: "Times New Roman", Georgia, serif; font-size: 20px; }
.c { font-family: "Courier New", monospace; font-size: 20px; }
"#,
    ),
    // 3. Absolute positioning + inset + stacking: the overlay must sit at
    //    top-left 120,40 and paint ABOVE the late in-flow red block.
    (
        "positioned",
        r#"<html><body>
<div class="panel">Positioned overlay on top</div>
<p>Paragraph before</p><p>Paragraph two</p>
<div class="late">Late in-flow block</div>
</body></html>"#,
        r#"
.panel { position: absolute; top: 40px; left: 120px; width: 300px; height: 120px;
         background: #0055cc; color: #ffffff; padding: 10px; z-index: 10; }
.late { background: #ff2222; height: 60px; width: 600px; }
"#,
    ),
    // 4. Table layout (HN-style): rows must be side-by-side cells.
    (
        "table",
        r#"<html><body>
<table>
<tr class="hdr"><td>1.</td><td>Title of the story goes here</td><td>example.com</td></tr>
<tr><td>2.</td><td>Another headline entirely</td><td>news.ycombinator.com</td></tr>
<tr><td>3.</td><td>Third story with a longer title text</td><td>rust-lang.org</td></tr>
</table>
</body></html>"#,
        "td { padding: 6px 10px; } .hdr { background: #ff6600; }",
    ),
    // 5. Padded container + text: text must respect padding, and per-item
    //    text in nested boxes must not overlap.
    (
        "padding",
        r#"<html><body>
<div class="box"><p>First line inside padding</p><p>Second line below</p></div>
</body></html>"#,
        r#"
.box { padding: 40px 60px; background: #f0f4c3; }
.box p { margin: 0 0 12px 0; background: #ffffff; }
"#,
    ),
];

fn main() {
    let outdir = std::env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    std::fs::create_dir_all(&outdir).expect("create outdir");
    for (name, html, css) in PAGES {
        let doc = parse_html(html.as_bytes());
        let mut media = MediaContext::default();
        media.width = 1000.0;
        media.height = 700.0;
        let sheets = vec![parse_stylesheet(css, &media)];
        let mut engine = LayoutEngine::new();
        let (styles, layout) = engine.layout_document(
            &doc.dom,
            &sheets,
            &media,
            Viewport {
                width: 1000.0,
                height: 700.0,
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
                    viewport_height: 700,
                    ..Default::default()
                },
                &mut engine.font_system,
            )
            .expect("frame");
        let path = format!("{outdir}/{name}.png");
        frame.save_png(&path).expect("png");
        eprintln!(
            "wrote {} ({} cmds, {} text runs)",
            path,
            list.commands.len(),
            layout.text.len()
        );
    }
}
