// overlap_check.rs — offline diagnostic: build layout + display list for a
// local HTML+CSS pair, then report text-run bounding boxes that overlap
// vertically (stacked/jumbled text) or run off the viewport horizontally.
use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext};
use rowser_parsing::html::parse_html;
use rowser_rendering::display_list::build_display_list;
use rowser_rendering::DrawCmd;
use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let html_path = args.next().expect("html");
    let vw: f32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1360.0);
    let css_paths: Vec<String> = std::env::args().skip(3).collect();
    let mut html = String::new();
    std::fs::File::open(&html_path)
        .unwrap()
        .read_to_string(&mut html)
        .unwrap();
    let media = MediaContext {
        width: vw,
        height: 860.0,
        dark_mode: false,
    };
    let mut sheets = Vec::new();
    for p in css_paths {
        let mut css = String::new();
        std::fs::File::open(&p)
            .unwrap()
            .read_to_string(&mut css)
            .unwrap();
        sheets.push(parse_stylesheet(&css, &media));
    }
    let doc = parse_html(html.as_bytes());
    let mut engine = LayoutEngine::new();
    let (styles, layout) = engine.layout_document(
        &doc.dom,
        &sheets,
        &media,
        Viewport {
            width: vw,
            height: 860.0,
        },
        &Default::default(),
    );
    let list = build_display_list(
        &doc.dom,
        &styles,
        &layout,
        &rowser_rendering::display_list::PaintInputs::default(),
    );

    // Collect text-run bounding boxes: (x0, y0, x1, y1, sample_text, node)
    struct Box_ {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        sample: String,
        _node: rowser_dom::NodeId,
        owner: String,
    }
    let mut boxes: Vec<Box_> = Vec::new();
    // Reconstruct sample text from glyphs? We don't keep chars; use run count only.
    // Instead: dump per-run first/last glyph positions.
    for c in &list.commands {
        if let DrawCmd::Text { run, .. } = c {
            if run.glyphs.is_empty() {
                continue;
            }
            let mut minx = f32::MAX;
            let mut maxx = f32::MIN;
            let mut miny = f32::MAX;
            let mut maxy = f32::MIN;
            for g in &run.glyphs {
                minx = minx.min(g.x as f32);
                maxx = maxx.max(g.x as f32);
                miny = miny.min(g.y as f32);
                maxy = maxy.max(g.y as f32);
            }
            let owner = {
                let el = doc.dom.element(run.node);
                match el {
                    Some(e) => format!("<{} .{}>", e.name.local, e.classes.join(".")),
                    None => format!("#text(node={})", run.node),
                }
            };
            boxes.push(Box_ {
                x0: minx,
                y0: miny,
                x1: maxx,
                y1: maxy,
                sample: format!("{}g @({:.0},{:.0})", run.glyphs.len(), minx, miny),
                _node: run.node,
                owner,
            });
        }
    }
    eprintln!("text runs: {}", boxes.len());

    // Overlap detection: two runs overlap if their y-ranges intersect by more
    // than 60% of the smaller height AND their x-ranges intersect.
    let mut overlaps = 0;
    for i in 0..boxes.len() {
        for j in (i + 1)..boxes.len() {
            let a = &boxes[i];
            let b = &boxes[j];
            let ix = (a.x0.min(b.x0))..(a.x1.max(b.x1));
            let x_overlap = (a.x1.min(b.x1) - a.x0.max(b.x0)).max(0.0);
            let y_overlap = (a.y1.min(b.y1) - a.y0.max(b.y0)).max(0.0);
            let h_min = (a.y1 - a.y0).min(b.y1 - b.y0).max(1.0);
            let _ = ix;
            if x_overlap > 2.0 && y_overlap > h_min * 0.6 {
                overlaps += 1;
                if overlaps <= 25 {
                    eprintln!(
                        "OVERLAP: run{} {} {} vs run{} {} {}  [xov={:.0} yov={:.0} hmin={:.0}]",
                        i, a.sample, a.owner, j, b.sample, b.owner, x_overlap, y_overlap, h_min
                    );
                }
            }
        }
    }
    eprintln!("overlapping text-run pairs: {}", overlaps);

    // Off-viewport text (horizontal overflow).
    let off = boxes.iter().filter(|b| b.x1 > vw + 8.0).count();
    eprintln!("runs past right edge: {}", off);

    // Vertical gap histogram for sibling-ish runs in the left column (x<400):
    let mut left: Vec<&Box_> = boxes
        .iter()
        .filter(|b| b.x0 < 400.0 && b.x1 - b.x0 > 50.0)
        .collect();
    left.sort_by_key(|b| b.y0 as i32);
    eprintln!("left-column runs: {}", left.len());
    let mut tiny_gaps = 0;
    for w in left.windows(2) {
        let gap = w[1].y0 - w[0].y0;
        if (0.0..8.0).contains(&gap) {
            tiny_gaps += 1;
        }
    }
    eprintln!(
        "left-column consecutive gaps < 8px (suspect stacking): {}",
        tiny_gaps
    );
}
