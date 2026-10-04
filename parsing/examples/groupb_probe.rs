//! Group B feature-fire probe: parse a real page's HTML+CSS, compute
//! styles, and count computed Group B features.
use rowser_parsing::cascade::{ComputedStyle, DisplayMode};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;

fn count(styles: &ComputedStyle) -> (bool, bool, bool, bool, bool) {
    (
        !styles.border_radius.is_zero(),
        !styles.background_layers.is_empty(),
        !styles.box_shadows.is_empty(),
        !styles.transform.is_empty(),
        !styles.text_shadows.is_empty(),
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let html = std::fs::read(&args[1]).expect("html file");
    let mut css = String::new();
    for path in &args[2..] {
        css.push_str(&std::fs::read_to_string(path).expect("css file"));
        css.push('\n');
    }
    let doc = parse_html(&html);
    let inline = doc.style_blocks().join("\n");
    let mut all = inline;
    all.push_str(&css);
    let sheet = parse_stylesheet(
        &all,
        &MediaContext {
            width: 1360.0,
            height: 745.0,
            dark_mode: false,
        },
    );
    println!(
        "rules={} keyframes={} pseudo-rules={}",
        sheet.rules.len(),
        sheet.keyframes.len(),
        sheet.rules.iter().filter(|r| r.pseudo.is_some()).count()
    );
    let media = MediaContext {
        width: 1360.0,
        height: 745.0,
        dark_mode: false,
    };
    let styles = rowser_parsing::cascade::compute_styles(&doc.dom, &[sheet], &media);
    let mut radius = 0usize;
    let mut layers = 0usize;
    let mut shadow = 0usize;
    let mut transform = 0usize;
    let mut tshadow = 0usize;
    let mut visible = 0usize;
    for style in styles.styles.values() {
        if style.display == DisplayMode::None {
            continue;
        }
        visible += 1;
        let (r, l, s, t, ts) = count(style);
        radius += r as usize;
        layers += l as usize;
        shadow += s as usize;
        transform += t as usize;
        tshadow += ts as usize;
    }
    let mut pseudo = 0usize;
    pseudo += styles.pseudo_before.len();
    pseudo += styles.pseudo_after.len();
    println!(
        "elements={visible} radius={radius} bg-layers={layers} box-shadow={shadow} transform={transform} text-shadow={tshadow} pseudo-boxes={pseudo}"
    );
    // Sample a few radii.
    for (node, style) in styles.styles.iter().take(200000) {
        if !style.border_radius.is_zero() {
            println!(
                "  sample node {node}: radius tl={:?}",
                style.border_radius.top_left
            );
            break;
        }
    }
}
