use lightningcss::rules::CssRule;
use lightningcss::stylesheet::StyleSheet;
use lightningcss::stylesheet::{ParserOptions, PrinterOptions};
use lightningcss::traits::ToCss;
use rowser_dom::parse_selector_list;
use rowser_dom::selector::matches;
use rowser_dom::ElementRef;
use rowser_parsing::html::parse_html;

fn main() {
    let css = ".mw-body #bodyContent { color: #123456; }";
    let sheet = StyleSheet::parse(css, ParserOptions::default()).unwrap();
    let mut serialized = String::new();
    for rule in sheet.rules.0.iter() {
        if let CssRule::Style(style) = rule {
            let text = style
                .selectors
                .to_css_string(PrinterOptions::default())
                .unwrap();
            println!("serialized: {text:?}");
            if let Some(list) = parse_selector_list(&text) {
                let doc = parse_html(br#"<html><body><div class="mw-body"><div id="bodyContent">x</div></div></body></html>"#);
                let nodes: Vec<_> = doc
                    .dom
                    .subtree_elements(doc.dom.document())
                    .filter(|n| doc.dom.element(*n).is_some())
                    .collect();
                for node in nodes {
                    let el = ElementRef::new(&doc.dom, node).unwrap();
                    let d = doc.dom.element(node).unwrap();
                    println!(
                        "  el {} #{} => {}",
                        d.name.local,
                        d.id.clone().unwrap_or_default(),
                        matches(&list, &el)
                    );
                }
            } else {
                println!("  REPARSE FAILED");
            }
        }
    }
}
