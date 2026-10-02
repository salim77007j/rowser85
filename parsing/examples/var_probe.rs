// var_probe.rs — check what DeclarationBlock serialization emits for
// custom properties and var() references.
use lightningcss::declaration::DeclarationBlock;
use lightningcss::stylesheet::{ParserOptions, PrinterOptions};
use lightningcss::traits::ToCss;

fn main() {
    let css = ":root { --brand: #ff6600; } p { color: var(--brand); background-color: var(--ink, #abcdef); margin: 0 auto; }";
    let Ok(sheet) = lightningcss::stylesheet::StyleSheet::parse(
        css,
        ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        },
    ) else {
        println!("sheet parse failed");
        return;
    };
    for rule in &sheet.rules.0 {
        if let lightningcss::rules::CssRule::Style(style) = rule {
            let sel = style
                .selectors
                .to_css_string(PrinterOptions::default())
                .unwrap();
            let block = style
                .declarations
                .to_css_string(PrinterOptions::default())
                .unwrap_or_default();
            println!("RULE {sel} => BLOCK: [{block}]");
            for d in &style.declarations.declarations {
                println!("   typed: {:?}", std::mem::discriminant(d));
            }
        }
    }
    // Also check parse_string path (used for style attributes + substitution).
    let block = DeclarationBlock::parse_string(
        "color: #ff6600; margin: 0 auto",
        ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        },
    )
    .unwrap();
    println!(
        "attr block: [{}]",
        block
            .to_css_string(PrinterOptions::default())
            .unwrap_or_default()
    );
    let sub = DeclarationBlock::parse_string(
        "color: #112233",
        ParserOptions {
            error_recovery: true,
            ..ParserOptions::default()
        },
    )
    .unwrap();
    println!(
        "sub block: [{}]",
        sub.to_css_string(PrinterOptions::default())
            .unwrap_or_default()
    );
}
