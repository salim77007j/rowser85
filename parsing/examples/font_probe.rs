// Probe: does lightningcss surface @font-face rules with our parse path?
use rowser_parsing::css::parse_stylesheet;
use rowser_parsing::css::MediaContext;

fn main() {
    let css = r#"
    @font-face { font-family: "WebTestA"; src: url("wtest.woff2") format("woff2"); }
    @font-face { font-family: WebTestB; src: url(wtest.woff) format("woff"); }
    @font-face { font-family: 'WebTestC'; src: url(wtest.ttf); }
    p { color: red; }
    "#;
    let media = MediaContext {
        width: 1360.0,
        height: 860.0,
        dark_mode: false,
    };
    let sheet = parse_stylesheet(css, &media);
    println!("font_faces: {}", sheet.font_faces.len());
    for face in &sheet.font_faces {
        println!("  family={:?} sources={:?}", face.family, face.sources);
    }
    println!("style rules: {}", sheet.rules.len());
}
