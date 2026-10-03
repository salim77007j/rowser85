// Dump every parsed rule (selector + declarations) of the font-test CSS.
use rowser_parsing::css::{parse_stylesheet, MediaContext};

fn main() {
    let css = std::fs::read_to_string("/home/z/my-project/fonttest/test.css").unwrap();
    let media = MediaContext { width: 1360.0, height: 860.0, dark_mode: false };
    let sheet = parse_stylesheet(&css, &media);
    println!("rules: {}", sheet.rules.len());
    for rule in &sheet.rules {
        println!("RULE {}", rule.selector_text);
        let p = &rule.props;
        println!("  font_size={:?} font_family={:?} line_height={:?}", p.font_size, p.font_family, p.line_height);
        println!("  margins=({:?},{:?},{:?},{:?})", p.margin_top, p.margin_right, p.margin_bottom, p.margin_left);
        println!("  bg={:?} color={:?}", p.background_color, p.color);
    }
}
