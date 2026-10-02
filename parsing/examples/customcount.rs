fn main() {
    let media = rowser_parsing::css::MediaContext::default();
    let css = ":root { --brand: #ff6600; }\np.a { color: var(--brand); }";
    let sheet = rowser_parsing::css::parse_stylesheet(css, &media);
    for r in sheet.rules.iter() {
        println!(
            "rule [{}] custom={:?} var_props={:?} color={:?}",
            r.selector_text, r.props.custom, r.props.var_props, r.props.color
        );
    }
}
