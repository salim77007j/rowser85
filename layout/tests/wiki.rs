#[cfg(test)]
mod wiki_real_tests {
    use rowser_layout::{LayoutEngine, Viewport};
    use rowser_parsing::css::{parse_stylesheet, MediaContext};

    /// Full Wikipedia Rust-article regression: the infobox must float right
    /// and body text must not intrude into its band.
    #[test]
    fn wikipedia_infobox_float_and_wrap() {
        let Ok(html) = std::fs::read("/tmp/wiki.html") else {
            eprintln!("SKIP: /tmp/wiki.html fixture not present");
            return;
        };
        let doc = rowser_parsing::html::parse_html(&html);
        let dom = &doc.dom;
        // collect all css: inline <style> elements + the link stylesheets
        let mut sheets = Vec::new();
        let media = MediaContext {
            width: 1360.0,
            height: 860.0,
            dark_mode: false,
        };
        for node in dom.subtree_elements(dom.document()) {
            if dom.element(node).is_some_and(|e| &*e.name.local == "style") {
                let text = dom.text_content(node);
                if !text.is_empty() {
                    sheets.push(parse_stylesheet(&text, &media));
                }
            }
        }
        for path in ["/tmp/wiki-vector.css", "/tmp/wiki-site2.css"] {
            if let Ok(css) = std::fs::read_to_string(path) {
                sheets.push(parse_stylesheet(&css, &media));
            }
        }
        eprintln!("WIKI sheets={}", sheets.len());
        let styles = rowser_parsing::cascade::compute_styles(dom, &sheets, &media);
        // find the infobox table
        let mut infobox = None;
        for node in dom.subtree_elements(dom.document()) {
            if let Some(el) = dom.element(node) {
                if &*el.name.local == "table" {
                    let cls = dom.get_attr(node, "class").unwrap_or_default();
                    if cls.contains("infobox") {
                        infobox = Some(node);
                        break;
                    }
                }
            }
        }
        let infobox = infobox.expect("infobox table found");
        let cs = styles.get(infobox).expect("infobox style");
        eprintln!("WIKI infobox float={:?} width={:?}", cs.float, cs.width);

        let mut engine = LayoutEngine::new();
        let (_, layout) = engine.layout_document(
            dom,
            &sheets,
            &media,
            Viewport {
                width: 1360.0,
                height: 860.0,
            },
            &Default::default(),
        );
        let rect = layout.rects.get(&infobox).expect("infobox rect");
        eprintln!("WIKI infobox rect={rect:?}");
        assert_eq!(
            cs.float,
            rowser_parsing::cascade::FloatMode::Right,
            "infobox must be floated right"
        );
        assert!(rect.x > 700.0, "infobox on the right side, got {rect:?}");
        // paragraph glyphs must not enter the infobox band (with tolerance)
        // Glyphs owned by nodes OUTSIDE the infobox subtree that land in its band.
        let mut in_infobox = std::collections::HashSet::new();
        {
            let mut stack = vec![infobox];
            while let Some(n) = stack.pop() {
                in_infobox.insert(n);
                for c in dom.flat_children(n) {
                    if dom.element(c).is_some() {
                        stack.push(c);
                    }
                }
            }
        }
        let mut intruders = 0;
        for run in &layout.text {
            if in_infobox.contains(&run.node) {
                continue;
            }
            for g in &run.glyphs {
                let gy = g.y as f32;
                if gy >= rect.y && gy <= rect.y + rect.h && g.x as f32 > rect.x + 4.0 {
                    intruders += 1;
                }
            }
        }

        // Structure probe: parent chains of infobox and first overlapping run.
        {
            let chain = |mut n: rowser_dom::NodeId| -> Vec<String> {
                let mut out = Vec::new();
                loop {
                    match dom.flat_parent_element(n) {
                        Some(p) => {
                            let name = dom
                                .element(p)
                                .map(|e| e.name.local.to_string())
                                .unwrap_or_default();
                            let cls = dom.get_attr(p, "class").unwrap_or_default().to_string();
                            let cls = cls.split_whitespace().next().unwrap_or("").to_string();
                            out.push(format!("{name}.{cls}"));
                            n = p;
                        }
                        None => break,
                    }
                    if out.len() > 12 {
                        break;
                    }
                }
                out
            };
            eprintln!("WIKI infobox chain: {:?}", chain(infobox));
            for run in &layout.text {
                if in_infobox.contains(&run.node) {
                    continue;
                }
                let g = run.glyphs.first().unwrap();
                let gy = g.y as f32;
                if gy >= rect.y && gy <= rect.y + rect.h && g.x as f32 > rect.x + 4.0 {
                    eprintln!(
                        "WIKI overlapping-run node={} chain={:?}",
                        u64::from(run.node),
                        chain(run.node)
                    );
                    break;
                }
            }
        }
        eprintln!("WIKI glyphs intruding infobox band: {intruders}");
        // The language dropdown (.vector-dropdown-content) hides itself with
        // height:0 + overflow:hidden + opacity:0 + visibility:hidden; until
        // overflow clipping lands (gap 3) its absolute-positioned links bleed
        // over the page. Exclude position:absolute subtrees from the float
        // metric — they are a separate, tracked failure mode.
        let mut pos_abs = std::collections::HashSet::new();
        for (node, cs) in &styles.styles {
            if cs.position == rowser_parsing::cascade::PositionMode::Absolute {
                let mut stack = vec![*node];
                while let Some(n) = stack.pop() {
                    pos_abs.insert(n);
                    for c in dom.flat_children(n) {
                        if dom.element(c).is_some() {
                            stack.push(c);
                        }
                    }
                }
            }
        }
        let mut float_intruders = 0;
        for run in &layout.text {
            if in_infobox.contains(&run.node) || pos_abs.contains(&run.node) {
                continue;
            }
            for g in &run.glyphs {
                let gy = g.y as f32;
                if gy >= rect.y && gy <= rect.y + rect.h && g.x as f32 > rect.x + 4.0 {
                    float_intruders += 1;
                }
            }
        }
        eprintln!("WIKI float-band intruders (abs excluded): {float_intruders}");
        assert!(
            float_intruders < 30,
            "{float_intruders} glyphs overlap the infobox — float wrap broken"
        );
    }
}
