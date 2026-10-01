//! Renders the last input events + a text field, for keyboard-delivery tests.
use std::sync::Arc;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 500.0])
            .with_title("kbdprobe"),
        ..Default::default()
    };
    eframe::run_native(
        "kbdprobe",
        options,
        Box::new(|cc| {
            let ctx = cc.egui_ctx.clone();
            let _ = Arc::new(ctx);
            Ok(Box::new(Probe::default()))
        }),
    )
}

#[derive(Default)]
struct Probe {
    events: Vec<String>,
    text: String,
}

impl eframe::App for Probe {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let new: Vec<String> =
                ctx.input(|i| i.events.iter().map(|e| format!("{e:?}")).collect());
            for e in new {
                self.events.insert(0, e);
            }
            self.events.truncate(18);
            ui.label("field:");
            ui.add(egui::TextEdit::singleline(&mut self.text).desired_width(400.0));
            ui.label(format!("text: {:?}", self.text));
            ui.separator();
            ui.heading("Input events (newest first):");
            for e in &self.events {
                ui.monospace(e);
            }
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}
