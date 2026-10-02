fn main() {
    let css = "body { display: grid; grid-template-columns: 15.5rem minmax(0,1fr); }
.a { grid-template-columns: 160px 1fr 200px; }
.b { grid-template-columns: repeat(2, 1fr); }
.c { display: grid; grid-template-columns: minmax(0,1fr) min-content; }";
    let media = rowser_parsing::css::MediaContext::default();
    let sheet = rowser_parsing::css::parse_stylesheet(css, &media);
    for rule in &sheet.rules {
        println!("selector: {:?}", rule.selectors);
        if let Some(p) = &rule.props.grid_template_columns {
            println!("  cols: {} tracks: {:?}", p.len(), p);
        } else {
            println!("  cols: NONE");
        }
    }
}
