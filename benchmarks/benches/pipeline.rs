//! Rrowser pipeline micro-benchmarks (criterion).
//!
//! Stages: HTML parse → CSS parse+cascade → layout → display list → paint,
//! plus a QuickJS-ng execution benchmark. Sizes are calibrated to be
//! representative of real pages (a few hundred nodes / tens of KB of CSS)
//! while keeping CI runtime sane.
//!
//! Run with: `cargo bench -p rowser-benchmarks`

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

use rowser_layout::{LayoutEngine, Viewport};
use rowser_parsing::css::{parse_stylesheet, MediaContext, ParsedStylesheet};
use rowser_parsing::parse_html;
use rowser_rendering::{build_display_list, Painter, RenderOptions};

/// Generates a synthetic but realistic page: nested sections, list items,
/// class/id attributes, inline styles, stylesheet links.
fn page_html(items: usize) -> Vec<u8> {
    let mut html = String::from("<!DOCTYPE html><html><head><title>bench</title></head><body>");
    for section in 0..(items / 20).max(1) {
        html.push_str(&format!(
            "<div class=\"section\" id=\"sec-{section}\"><h2>Section {section}</h2>"
        ));
        for i in 0..20 {
            html.push_str(&format!(
                "<p class=\"item item-{i}\">Item number {i} in section {section} with some text \
                 content to shape and layout.</p>"
            ));
        }
        html.push_str("</div>");
    }
    html.push_str("</body></html>");
    html.into_bytes()
}

/// Representative author CSS: tag, class, id and descendant selectors.
fn page_css(n: usize) -> String {
    let mut css = String::from(
        "body { font-family: sans-serif; margin: 8px; color: #222; }\n\
         h2 { color: #3355aa; font-size: 22px; margin: 12px 0; }\n\
         .section { padding: 8px; border-color: #dddddd; }\n\
         .item { line-height: 1.4; margin: 2px 0; color: #333333; }\n",
    );
    for i in 0..n {
        css.push_str(&format!(
            ".item-{i} {{ padding-left: {i}px; background-color: #{i:06x}; }}\n"
        ));
    }
    css
}

fn bench_parse_html(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse-html");
    for (label, size) in [("small", 200), ("medium", 2_000), ("large", 20_000)] {
        let html = page_html(size);
        group.throughput(Throughput::Bytes(html.len() as u64));
        group.bench_function(label, |b| b.iter(|| parse_html(&html)));
    }
    group.finish();
}

fn bench_parse_css(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse-css");
    let media = MediaContext::default();
    for (label, rules) in [("small", 20), ("medium", 400), ("large", 4_000)] {
        let css = page_css(rules);
        group.throughput(Throughput::Bytes(css.len() as u64));
        group.bench_function(label, |b| b.iter(|| parse_stylesheet(&css, &media)));
    }
    group.finish();
}

fn bench_cascade_and_layout(c: &mut Criterion) {
    // Warm-engine layout (the realistic browser case: the font system and
    // shaping caches stay alive across pages).
    let mut group = c.benchmark_group("cascade+layout");
    for (label, items) in [("small", 200), ("medium", 2_000), ("large", 20_000)] {
        let html = page_html(items);
        let css = page_css(items / 4);
        let media = MediaContext::default();
        let doc = parse_html(&html);
        let sheet: ParsedStylesheet = parse_stylesheet(&css, &media);
        let mut engine = LayoutEngine::new();
        // Warm the shaping cache once.
        let _ = engine.layout_document(
            &doc.dom,
            &[sheet.clone()],
            &media,
            Viewport { width: 1280.0, height: 800.0 },
        );
        group.throughput(Throughput::Elements(items as u64));
        group.bench_function(label, |b| {
            b.iter(|| {
                engine.layout_document(
                    &doc.dom,
                    &[sheet.clone()],
                    &media,
                    Viewport { width: 1280.0, height: 800.0 },
                )
            })
        });
    }
    group.finish();
    // Cold engine creation (font system init) as its own cost line.
    c.bench_function("engine-cold-init", |b| {
        b.iter(|| LayoutEngine::new())
    });
}

fn bench_display_list(c: &mut Criterion) {
    let html = page_html(2_000);
    let css = page_css(500);
    let media = MediaContext::default();
    let doc = parse_html(&html);
    let sheet = parse_stylesheet(&css, &media);
    let mut engine = LayoutEngine::new();
    let (styles, layout) =
        engine.layout_document(&doc.dom, &[sheet], &media, Viewport { width: 1280.0, height: 800.0 });
    c.bench_function("display-list/2k-nodes", |b| {
        b.iter(|| build_display_list(&doc.dom, &styles, &layout, &Default::default()))
    });
}

fn bench_paint(c: &mut Criterion) {
    let html = page_html(2_000);
    let css = page_css(500);
    let media = MediaContext::default();
    let doc = parse_html(&html);
    let sheet = parse_stylesheet(&css, &media);
    let mut engine = LayoutEngine::new();
    let (styles, layout) =
        engine.layout_document(&doc.dom, &[sheet], &media, Viewport { width: 1280.0, height: 800.0 });
    let list = build_display_list(&doc.dom, &styles, &layout, &Default::default());
    // Steady-state paint: warm painter (glyph mask caches alive, the
    // scrolling case) against a warm font system.
    let mut painter = Painter::new();
    let mut engine = LayoutEngine::new();
    let mut group = c.benchmark_group("paint");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.bench_function("1280x800/2k-nodes", |b| {
        b.iter(|| {
            painter
                .render(
                    &list,
                    RenderOptions {
                        viewport_width: 1280,
                        viewport_height: 800,
                        ..RenderOptions::default()
                    },
                    &mut engine.font_system,
                )
                .expect("paint")
        })
    });
    group.finish();
}

fn bench_full_pipeline(c: &mut Criterion) {
    // parse → style → layout → display list → paint (no network / no JS).
    let html = page_html(2_000);
    let css = page_css(500);
    let media = MediaContext::default();
    let mut group = c.benchmark_group("pipeline");
    group.throughput(Throughput::Elements(2_000));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(15));
    group.bench_function("html-to-pixels/2k-nodes", |b| {
        b.iter_batched(
            || (Painter::new(), LayoutEngine::new()),
            |(mut painter, mut engine)| {
                let doc = parse_html(&html);
                let sheet = parse_stylesheet(&css, &media);
                let (styles, layout) = engine.layout_document(
                    &doc.dom,
                    &[sheet],
                    &media,
                    Viewport { width: 1280.0, height: 800.0 },
                );
                let list = build_display_list(&doc.dom, &styles, &layout, &Default::default());
                painter
                    .render(&list, RenderOptions::default(), &mut engine.font_system)
                    .expect("paint")
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_parse_html,
    bench_parse_css,
    bench_cascade_and_layout,
    bench_display_list,
    bench_paint,
    bench_full_pipeline
);
criterion_main!(benches);
