// shape_probe.rs — isolates cosmic-text shaping: shape "Home" at 16px with
// the resolved font and dump advances, to hunt the 2x-wide glyph bug.
use rowser_layout::text::{shape, ShapeCache, SpanStyle};
use rowser_layout::{LayoutEngine, TextLeaf};
use rowser_parsing::cascade::{FontStyleMode, Rgba, TextAlignMode};

fn main() {
    let mut engine = LayoutEngine::new();
    let mut cache = ShapeCache::new();
    for family in ["Liberation Sans", "DejaVu Sans", "sans-serif"] {
        let leaf = TextLeaf {
            node: 1,
            text: "Home Products".to_owned(),
            spans: Vec::new(),
            defaults: SpanStyle {
                family: family.to_owned(),
                family_stack: vec![family.to_owned()],
                font_size: 16.0,
                weight: 400.0,
                style: FontStyleMode::Normal,
                color: Rgba::new(0, 0, 0, 255),
                line_height: 20.0,
                text_align: TextAlignMode::Left,
            },
        };
        let lines = shape(&leaf, &mut engine.font_system, Some(400.0), &mut cache);
        println!("== {family} ==");
        for line in lines.iter() {
            println!("line w={:.2} glyphs={}", line.w, line.glyphs.len());
            let xs: Vec<String> = line
                .glyphs
                .iter()
                .map(|g| format!("x={:.2}", g.x))
                .collect();
            println!("  {}", xs.join(" "));
            let ws: Vec<String> = line
                .glyphs
                .iter()
                .map(|g| format!("fs={:.1}", g.font_size))
                .collect();
            println!("  {}", ws.join(" "));
        }
    }
}
