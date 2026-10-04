//! Display-list probe: does a real page's paint stream contain Group B
//! commands (gradients, rounded rects, shadows, transforms)?
use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;
use rowser_rendering::display_list::{build_display_list, DrawCmd, PaintInputs};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let html = std::fs::read(&args[1]).expect("html");
    let mut css = String::new();
    for path in &args[2..] {
        css.push_str(&std::fs::read_to_string(path).expect("css"));
        css.push('\n');
    }
    let doc = parse_html(&html);
    let inline = doc.style_blocks().join("\n");
    let mut all = inline;
    all.push_str(&css);
    let media = MediaContext {
        width: 1360.0,
        height: 745.0,
        dark_mode: false,
    };
    let sheet = parse_stylesheet(&all, &media);
    let mut engine = LayoutEngine::new();
    let (styles, layout) = engine.layout_document(
        &doc.dom,
        &[sheet],
        &media,
        Viewport {
            width: 1360.0,
            height: 745.0,
        },
        &Default::default(),
    );
    let list = build_display_list(&doc.dom, &styles, &layout, &PaintInputs::default());
    let mut gradients = 0usize;
    let mut rounded = 0usize;
    let mut shadows = 0usize;
    let mut transforms = 0usize;
    let mut rects = 0usize;
    for cmd in &list.commands {
        match cmd {
            DrawCmd::Gradient { .. } => gradients += 1,
            DrawCmd::BoxShadow { .. } => shadows += 1,
            DrawCmd::PushTransform { .. } | DrawCmd::PushFixed | DrawCmd::PushSticky { .. } => {
                transforms += 1
            }
            DrawCmd::Rect { radius, .. }
            | DrawCmd::Border { radius, .. }
            | DrawCmd::Image { radius, .. } => {
                if !radius.is_zero() {
                    rounded += 1;
                }
                rects += 1;
            }
            _ => {}
        }
    }
    println!(
        "cmds={} rects/borders={} rounded={} gradients={} shadows={} transform-groups={}",
        list.commands.len(),
        rects,
        rounded,
        gradients,
        shadows,
        transforms
    );
}
