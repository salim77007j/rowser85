// Deterministic probe: register the three test fonts, then shape text whose
// CSS stack names the web families and report the resolved family + metrics.
use rowser_layout::text::shape;
use rowser_layout::TextLeaf;
use rowser_layout::text::SpanStyle;

fn leaf_with_stack(text: &str, stack: &[&str]) -> TextLeaf {
    TextLeaf {
        node: 1,
        text: text.to_string(),
        spans: Vec::new(),
        defaults: SpanStyle {
            family: stack[0].into(),
            family_stack: stack.iter().map(|s| s.to_string()).collect(),
            font_size: 22.0,
            weight: 400.0,
            style: FontStyleMode::Normal,
            color: Rgba::new(0, 0, 0, 255),
            line_height: 28.0,
            text_align: TextAlignMode::Left,
        },
        cache: None,
    }
}

fn main() {
    let mut engine = rowser_layout::LayoutEngine::new();
    let faces = [
        ("WebTestA", "/home/z/my-project/fonttest/wtest.woff2"),
        ("WebTestB", "/home/z/my-project/fonttest/wtest.woff"),
        ("WebTestC", "/home/z/my-project/fonttest/wtest.ttf"),
    ];
    for (family, path) in faces {
        let bytes = std::fs::read(path).unwrap();
        let ok = rowser_engine::font_face::register_font_bytes(
            &mut engine.font_system,
            family,
            &bytes,
        );
        println!("register {family} ({} bytes): {ok}", bytes.len());
    }
    // Shape with each web family before the stack fallback.
    for (family, label) in [
        ("WebTestA", "woff2"),
        ("WebTestB", "woff"),
        ("WebTestC", "ttf"),
    ] {
        let mut leaf = leaf_with_stack("The quick brown fox", &[family, "monospace"]);
        let lines = shape(&mut leaf, &mut engine.font_system, Some(1200.0));
        let n_glyphs: usize = lines.iter().map(|l| l.glyphs.len()).sum();
        let width: f32 = lines.iter().map(|l| l.w).sum();
        println!("{label}: lines={} glyphs={} width={:.1}", lines.len(), n_glyphs, width);
    }
    // Reference: the same text shaped with plain monospace fallback.
    let mut leaf = leaf_with_stack("The quick brown fox", &["NotLoadedFace", "monospace"]);
    let lines = shape(&mut leaf, &mut engine.font_system, Some(1200.0));
    let width: f32 = lines.iter().map(|l| l.w).sum();
    println!("fallback: lines={} width={:.1}", lines.len(), width);
    // And plain DejaVu Serif direct (the real family inside the files).
    let mut leaf = leaf_with_stack("The quick brown fox", &["DejaVu Serif"]);
    let lines = shape(&mut leaf, &mut engine.font_system, Some(1200.0));
    let width: f32 = lines.iter().map(|l| l.w).sum();
    println!("dejavu-serif direct: lines={} width={:.1}", lines.len(), width);
    // Generic sans-serif: which face does cosmic-text pick, and how wide?
    for family in ["sans-serif", "DejaVu Sans", "Liberation Sans", "Noto Sans"] {
        let mut leaf = leaf_with_stack("The quick brown fox", &[family]);
        leaf.defaults.font_size = 26.0;
        leaf.defaults.line_height = 32.0;
        let lines = shape(&mut leaf, &mut engine.font_system, Some(1200.0));
        let width: f32 = lines.iter().map(|l| l.w).sum();
        println!("26px {family}: width={:.1}", width);
    }
}
