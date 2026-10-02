fn main() {
    let css = r#"p { color: red; }
@media only screen and (min-width: 300px) and (max-width: 750px) { span { color: blue; } }
@media screen { div { color: green; } }
@media screen and (min-width: 1000px) { b { color: purple; } }
@media print { i { color: gray; } }"#;
    for width in [1360.0, 400.0] {
        let media = rowser_parsing::css::MediaContext {
            width,
            height: 724.0,
            dark_mode: false,
        };
        let sheet = rowser_parsing::css::parse_stylesheet(css, &media);
        println!(
            "width={} -> rules: {:?}",
            width,
            sheet
                .rules
                .iter()
                .map(|r| r.selector_text.clone())
                .collect::<Vec<_>>()
        );
    }
}
