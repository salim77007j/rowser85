fn main() {
    let html = br#"<html><body><p class="a">x</p></body></html>"#;
    let doc = rowser_parsing::html::parse_html(html);
    let list = rowser_dom::parse_selector_list(":root").expect(":root parse failed");
    let dom = &doc.dom;
    for node in dom.subtree_elements(dom.document()) {
        let er = rowser_dom::selector::ElementRef::new(dom, node).unwrap();
        let m = rowser_dom::selector::matches(&list, &er);
        println!("{}: matches={}", dom.element(node).unwrap().name.local, m);
    }
}
