//! Documents usvg 0.44 sizing behaviors the SVG pipeline depends on:
//! 1. a root without `xmlns` fails to parse (html5ever strips it from
//!    inline SVG — the engine re-injects it);
//! 2. with only one size attribute, usvg keeps the raw viewBox extent for
//!    the missing axis (Chrome uses the aspect ratio — hence our own
//!    `svg_natural_size`);
//! 3. root width/height + viewBox maps content per `preserveAspectRatio`.
fn main() {
    let cases = [
        r##"<svg viewBox="0 0 2 1"><rect x="0" y="0" width="1" height="1" fill="black"/></svg>"##,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 2 1"><rect x="0" y="0" width="1" height="1" fill="black"/></svg>"##,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" viewBox="0 0 2 1"/>"##,
    ];
    for (i, svg) in cases.iter().enumerate() {
        let opts = resvg::usvg::Options::default();
        match resvg::usvg::Tree::from_str(svg, &opts) {
            Ok(t) => println!("case {i}: OK size={:?}", t.size()),
            Err(e) => println!("case {i}: ERR {e:?}"),
        }
    }
}
