//! Internal browser pages (`rowser://…`), rendered natively in egui and
//! fully wired to the engine + shell.

use egui::{
    Align2, Color32, CornerRadius, Id, Rect, RichText, ScrollArea, Sense, Stroke, Ui, Vec2,
};

use crate::app::BrowserApp;
use crate::icons::Icon;
use crate::theme::Theme;
use rowser_shell::{
    DownloadPhase, PermissionState, SearchEngine, StartupMode, ThemeMode, PERMISSION_KINDS,
};

/// The internal page a URL maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalPage {
    /// Start page / speed dial.
    NewTab,
    /// Settings.
    Settings,
    /// History manager.
    History,
    /// Bookmarks manager.
    Bookmarks,
    /// Downloads manager.
    Downloads,
    /// Privacy dashboard.
    Privacy,
    /// Extensions.
    Extensions,
}

impl InternalPage {
    /// Maps a `rowser://` URL to a page.
    pub fn from_url(url: &str) -> Option<InternalPage> {
        let path = url
            .strip_prefix("rowser://")
            .map(|p| p.trim_end_matches('/').to_owned())?;
        Some(match path.as_str() {
            "newtab" | "new-tab" | "start" => InternalPage::NewTab,
            "settings" => InternalPage::Settings,
            "history" => InternalPage::History,
            "bookmarks" => InternalPage::Bookmarks,
            "downloads" => InternalPage::Downloads,
            "privacy" => InternalPage::Privacy,
            "extensions" => InternalPage::Extensions,
            _ => return None,
        })
    }
}

/// Renders the active tab's internal page (or the new-tab page).
pub fn render(app: &mut BrowserApp, ui: &mut Ui) {
    let page = app
        .tabs
        .get(app.active)
        .and_then(|t| t.internal_page())
        .unwrap_or(InternalPage::NewTab);
    match page {
        InternalPage::NewTab => new_tab_page(app, ui),
        InternalPage::Settings => settings_page(app, ui),
        InternalPage::History => history_page(app, ui),
        InternalPage::Bookmarks => bookmarks_page(app, ui),
        InternalPage::Downloads => downloads_page(app, ui),
        InternalPage::Privacy => privacy_page(app, ui),
        InternalPage::Extensions => extensions_page(app, ui),
    }
}

/// Loading / unreachable page inside the content area.
pub fn blank_or_error(app: &mut BrowserApp, ui: &mut Ui, url: &str, loading: bool) {
    let pal = app.pal().clone();
    let rect = ui.available_rect_before_wrap();
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(0), pal.surface);
    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            if loading {
                ui.spinner();
                ui.add_space(8.0);
                ui.label(RichText::new("Loading…").color(pal.text_dim));
                ui.label(
                    RichText::new(url.to_string())
                        .monospace()
                        .small()
                        .color(pal.text_dim),
                );
            } else {
                let icon_rect = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover()).0;
                let p = ui.painter_at(icon_rect);
                Icon::Warning.paint(&p, icon_rect, pal.warning);
                ui.add_space(10.0);
                ui.label(
                    RichText::new("This site can't be reached")
                        .strong()
                        .size(19.0),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(ellipsize_url(url, 64))
                        .monospace()
                        .small()
                        .color(pal.text_dim),
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.button("Retry").clicked() {
                        app.reload();
                    }
                    let query = url
                        .trim_start_matches("https://")
                        .trim_start_matches("http://");
                    if ui.button("Search instead").clicked() {
                        app.navigate_input(query);
                    }
                });
            }
        });
    });
}

// ---------------------------------------------------------------------------
// New tab page
// ---------------------------------------------------------------------------

fn new_tab_page(app: &mut BrowserApp, ui: &mut Ui) {
    let pal = app.pal().clone();
    let blocked = app.shell.stats().total_blocked();
    ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(60.0);
        ui.vertical_centered(|ui| {
            // Logo.
            let logo = ui.allocate_exact_size(Vec2::splat(74.0), Sense::hover()).0;
            let painter = ui.painter_at(logo);
            painter.rect_filled(logo, CornerRadius::same(22), pal.accent);
            painter.text(
                logo.center(),
                Align2::CENTER_CENTER,
                "R",
                egui::FontId::proportional(44.0),
                Color32::WHITE,
            );
            ui.add_space(14.0);
            ui.heading("Rrowser85");
            ui.label(
                RichText::new("Fast. Private. Yours.")
                    .color(pal.text_dim)
                    .size(15.0),
            );
            if blocked > 0 {
                ui.add_space(6.0);
                let chip = egui::Frame::default()
                    .fill(pal.accent_soft)
                    .corner_radius(12)
                    .inner_margin(egui::Margin::symmetric(12, 4));
                chip.show(ui, |ui| {
                    ui.label(
                        RichText::new(format!("🛡 {} trackers & ads blocked so far", blocked))
                            .small()
                            .color(pal.text),
                    );
                });
            }
        });
        ui.add_space(26.0);

        // Search field.
        let mut search = app.chrome.search_text.clone();
        let width = (ui.available_width() * 0.56).clamp(320.0, 620.0);
        ui.vertical_centered(|ui| {
            let frame = egui::Frame::default()
                .fill(pal.field_bg)
                .corner_radius(22)
                .stroke(Stroke::new(1.0_f32, pal.border))
                .inner_margin(egui::Margin::symmetric(14, 7));
            frame.show(ui, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    let icon_rect = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover()).0;
                    let painter = ui.painter_at(icon_rect);
                    Icon::Search.paint(&painter, icon_rect, pal.text_dim);
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut search)
                            .frame(false)
                            .hint_text("Search the web")
                            .desired_width(width - 80.0)
                            .font(egui::FontId::proportional(15.5)),
                    );
                    let enter =
                        response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if enter && !search.trim().is_empty() {
                        app.navigate_input(&search.clone());
                    }
                });
            });
        });
        app.chrome.search_text = search;
        ui.add_space(34.0);

        // Speed dial.
        let top: Vec<(String, String)> = {
            let history = &app.shell.store().history;
            let sites = rowser_shell::top_sites(history, 8);
            if sites.is_empty() {
                default_tiles()
            } else {
                sites.into_iter().map(|s| (s.label, s.url)).collect()
            }
        };
        let tile_w = 108.0;
        let cols = ((ui.available_width() / (tile_w + 14.0)).floor() as usize).clamp(4, 8);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Speed dial").strong().size(15.0));
            ui.add_space(10.0);
            let mut navigate: Option<String> = None;
            for row in top.chunks(cols) {
                ui.horizontal(|ui| {
                    for (label, url) in row {
                        let (tile, response) =
                            ui.allocate_exact_size(Vec2::new(tile_w, 96.0), Sense::click());
                        let painter = ui.painter_at(tile);
                        let hovered = response.hovered();
                        painter.rect_filled(
                            tile,
                            CornerRadius::same(10),
                            if hovered {
                                pal.field_focus_bg
                            } else {
                                pal.field_bg
                            },
                        );
                        if hovered {
                            painter.rect_stroke(
                                tile,
                                CornerRadius::same(10),
                                Stroke::new(1.2_f32, pal.border),
                                egui::epaint::StrokeKind::Inside,
                            );
                        }
                        let avatar = Rect::from_center_size(
                            egui::Pos2::new(tile.center().x, tile.top() + 32.0),
                            Vec2::splat(38.0),
                        );
                        painter.circle_filled(avatar.center(), 19.0, pal.accent_soft);
                        painter.text(
                            avatar.center(),
                            Align2::CENTER_CENTER,
                            label
                                .chars()
                                .find(|c| c.is_alphanumeric())
                                .unwrap_or('?')
                                .to_uppercase()
                                .to_string(),
                            egui::FontId::proportional(17.0),
                            pal.accent,
                        );
                        painter.text(
                            egui::Pos2::new(tile.center().x, tile.bottom() - 20.0),
                            Align2::CENTER_CENTER,
                            crate::chrome::ellipsize_public(ui, label, tile_w - 16.0),
                            egui::FontId::proportional(12.0),
                            pal.text,
                        );
                        if response.clicked() {
                            navigate = Some(url.clone());
                        }
                        if response.clicked_by(egui::PointerButton::Middle) {
                            app.new_tab(Some(url.clone()));
                        }
                    }
                });
            }
            if let Some(url) = navigate {
                app.navigate_url(url);
            }
        });
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("Rrowser85 · engine v0.1 · privacy enforced in the engine core")
                    .small()
                    .color(pal.text_dim),
            );
        });
    });
}

fn default_tiles() -> Vec<(String, String)> {
    vec![
        ("Wikipedia".into(), "https://en.wikipedia.org/".into()),
        ("GitHub".into(), "https://github.com/".into()),
        ("Hacker News".into(), "https://news.ycombinator.com/".into()),
        ("example.com".into(), "https://example.com/".into()),
        ("MDN".into(), "https://developer.mozilla.org/".into()),
        ("DuckDuckGo".into(), "https://duckduckgo.com/".into()),
        ("Rust".into(), "https://www.rust-lang.org/".into()),
        ("W3C".into(), "https://www.w3.org/".into()),
    ]
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

fn settings_page(app: &mut BrowserApp, ui: &mut Ui) {
    let pal = app.pal().clone();
    if app.chrome.settings_section.is_empty() {
        app.chrome.settings_section = "appearance".into();
    }
    let section = app.chrome.settings_section.clone();
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.heading("Settings");
    });
    ui.separator();
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ScrollArea::vertical()
            .id_salt("settings-nav")
            .max_width(190.0)
            .show(ui, |ui| {
                ui.set_min_width(180.0);
                for (id, label) in [
                    ("appearance", "Appearance"),
                    ("search", "Search engine"),
                    ("startup", "On startup"),
                    ("privacy", "Privacy & security"),
                    ("permissions", "Site permissions"),
                    ("downloads", "Downloads"),
                    ("advanced", "Advanced"),
                    ("about", "About"),
                ] {
                    let selected = section == id;
                    let response = ui.selectable_label(selected, label);
                    if response.clicked() {
                        app.chrome.settings_section = id.into();
                    }
                }
            });
        ui.separator();
        ScrollArea::vertical()
            .id_salt("settings-body")
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width() - 40.0);
                ui.add_space(10.0);
                match section.as_str() {
                    "appearance" => settings_appearance(app, ui, &pal),
                    "search" => settings_search(app, ui, &pal),
                    "startup" => settings_startup(app, ui, &pal),
                    "privacy" => settings_privacy(app, ui, &pal),
                    "permissions" => settings_permissions(app, ui, &pal),
                    "downloads" => settings_downloads(app, ui, &pal),
                    "advanced" => settings_advanced(app, ui, &pal),
                    _ => settings_about(app, ui, &pal),
                }
            });
    });
}

fn settings_appearance(app: &mut BrowserApp, ui: &mut Ui, pal: &Theme) {
    section(ui, "Theme");
    ui.horizontal(|ui| {
        for (mode, label) in [
            (ThemeMode::Light, "☀ Light"),
            (ThemeMode::Dark, "🌙 Dark"),
            (ThemeMode::System, "🖥 System"),
        ] {
            let selected = app.shell.store().settings.theme == mode;
            if ui.selectable_label(selected, label).clicked() {
                app.shell.store_mut().settings.theme = mode;
                persist_settings(app);
            }
        }
    });
    section(ui, "Accent color");
    ui.horizontal(|ui| {
        for hex in [
            "1A73E8", "8430CE", "1E8E3E", "D93025", "E8710A", "00897B", "5F6368", "C5221F",
        ] {
            let color = crate::theme::parse_hex(hex).unwrap();
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
            let painter = ui.painter_at(rect);
            painter.circle_filled(rect.center(), 12.0, color);
            let selected = app.shell.store().settings.accent.eq_ignore_ascii_case(hex);
            if selected {
                painter.circle_stroke(rect.center(), 15.0, Stroke::new(2.0_f32, pal.text));
            }
            if response.clicked() {
                app.shell.store_mut().settings.accent = hex.to_owned();
                persist_settings(app);
            }
        }
        ui.label("Custom");
        let mut hex = app.shell.store().settings.accent.clone();
        let response = ui.add(
            egui::TextEdit::singleline(&mut hex)
                .desired_width(90.0)
                .hint_text("RRGGBB"),
        );
        if response.changed() {
            app.shell.store_mut().settings.accent = hex;
            persist_settings(app);
        }
    });
    section(ui, "Layout");
    ui.checkbox(&mut app.chrome.settings_bookmarks_bar, "Show bookmarks bar");
    if app.chrome.settings_bookmarks_bar != app.shell.store().settings.show_bookmarks_bar {
        app.shell.store_mut().settings.show_bookmarks_bar = app.chrome.settings_bookmarks_bar;
        persist_settings(app);
    }
    let mut zoom = app.shell.store().settings.default_zoom;
    ui.horizontal(|ui| {
        ui.label("Default page zoom");
        ui.add(egui::Slider::new(&mut zoom, 0.5..=2.0));
    });
    if (zoom - app.shell.store().settings.default_zoom).abs() > 0.001 {
        app.shell.store_mut().settings.default_zoom = zoom;
        persist_settings(app);
    }
}

fn settings_search(app: &mut BrowserApp, ui: &mut Ui, _pal: &Theme) {
    section(ui, "Search engine used in the address bar");
    let engines = SearchEngine::builtins();
    let current = app.shell.store().settings.search_engine.name.clone();
    for engine in engines {
        let selected = engine.name == current;
        let name = engine.name.clone();
        let url_template = engine.url.replace("{q}", "…");
        ui.horizontal(|ui| {
            if ui.radio(selected, "").clicked() {
                app.shell.store_mut().settings.search_engine = engine;
                persist_settings(app);
            }
            ui.strong(name);
            ui.label(
                RichText::new(url_template)
                    .small()
                    .color(app.pal().text_dim),
            );
        });
    }
}

fn settings_startup(app: &mut BrowserApp, ui: &mut Ui, _pal: &Theme) {
    section(ui, "On startup");
    let modes = [
        (StartupMode::NewTab, "Open the new tab page"),
        (StartupMode::Homepage, "Open my home page"),
        (StartupMode::PreviousSession, "Continue where I left off"),
    ];
    let current = app.shell.store().settings.startup.clone();
    for (mode, label) in modes {
        let selected = current == mode;
        if ui.radio(selected, label).clicked() {
            app.shell.store_mut().settings.startup = mode;
            persist_settings(app);
        }
    }
    section(ui, "Home page");
    let mut home = app.shell.store().settings.homepage.clone();
    let response = ui.add(
        egui::TextEdit::singleline(&mut home)
            .desired_width(400.0)
            .hint_text("rowser://newtab"),
    );
    if response.changed() {
        app.shell.store_mut().settings.homepage = home;
        persist_settings(app);
    }
    section(ui, "Session");
    let mut restore = app.shell.store().settings.restore_session;
    ui.checkbox(&mut restore, "Restore tabs after closing");
    if restore != app.shell.store().settings.restore_session {
        app.shell.store_mut().settings.restore_session = restore;
        persist_settings(app);
    }
}

fn save_permissions(app: &mut BrowserApp) {
    let dir = app.shell.store().dir.clone();
    app.shell.store().permissions.save(&dir);
}

fn settings_privacy(app: &mut BrowserApp, ui: &mut Ui, pal: &Theme) {
    section(ui, "Privacy protections (enforced in the engine core)");
    let mut privacy = app.shell.store().settings.privacy;
    let mut changed = false;
    let apply = |ui: &mut Ui, label: &str, help: &str, value: &mut bool| {
        ui.checkbox(value, label);
        ui.label(RichText::new(help).small().color(pal.text_dim));
        ui.add_space(2.0);
        let _ = ui;
    };
    let toggles: Vec<(&str, &str)> = vec![
        (
            "Block ads & trackers",
            "Network-layer blocking (Brave-derived rules).",
        ),
        (
            "HTTPS upgrade",
            "http:// requests upgrade to https:// where possible.",
        ),
        (
            "Block third-party cookies",
            "Unpartitioned cross-site cookies rejected (CHIPS stays).",
        ),
        (
            "Anti-fingerprinting",
            "Per-tab consistent canvas/audio/navigator spoofing.",
        ),
        (
            "WebRTC protection",
            "WebRTC candidate filtering (IP leak protection).",
        ),
        (
            "Safe browsing",
            "Local hash-prefix checks; no URLs leave the device.",
        ),
        (
            "Telemetry opt-in",
            "The engine ships zero telemetry; this flag exists for opt-in builds.",
        ),
    ];
    for (label, help) in toggles {
        let mut value = flag_of(privacy, label);
        let before = value;
        apply(ui, label, help, &mut value);
        if value != before {
            changed = true;
        }
        set_flag(&mut privacy, label, value);
    }
    if changed {
        app.shell.set_privacy(privacy.to_engine());
    }
    section(ui, "Danger zone");
    if ui.button("Clear all browsing data…").clicked() {
        app.chrome.confirm_clear_data = true;
    }
}

fn flag_of(privacy: rowser_shell::PrivacyMirror, label: &str) -> bool {
    match label {
        "Block ads & trackers" => privacy.block_ads,
        "HTTPS upgrade" => privacy.https_upgrade,
        "Block third-party cookies" => privacy.block_third_party_cookies,
        "Anti-fingerprinting" => privacy.anti_fingerprinting,
        "WebRTC protection" => privacy.webrtc_protection,
        "Safe browsing" => privacy.safe_browsing,
        "Telemetry opt-in" => privacy.telemetry_opt_in,
        _ => false,
    }
}

fn set_flag(privacy: &mut rowser_shell::PrivacyMirror, label: &str, value: bool) {
    match label {
        "Block ads & trackers" => privacy.block_ads = value,
        "HTTPS upgrade" => privacy.https_upgrade = value,
        "Block third-party cookies" => privacy.block_third_party_cookies = value,
        "Anti-fingerprinting" => privacy.anti_fingerprinting = value,
        "WebRTC protection" => privacy.webrtc_protection = value,
        "Safe browsing" => privacy.safe_browsing = value,
        "Telemetry opt-in" => privacy.telemetry_opt_in = value,
        _ => {}
    }
}

fn settings_permissions(app: &mut BrowserApp, ui: &mut Ui, pal: &Theme) {
    section(ui, "Site permissions");
    ui.label(
        RichText::new("Defaults are Ask. Sites you grant or deny are listed below.")
            .small()
            .color(pal.text_dim),
    );
    let sites: Vec<String> = app.shell.store().permissions.configured_sites();
    if sites.is_empty() {
        ui.label(RichText::new("No site-specific permissions yet.").color(pal.text_dim));
    }
    let mut reset: Option<String> = None;
    for site in sites {
        permission_site_row(ui, site, app, &mut reset, pal);
    }
    if let Some(site) = reset {
        app.shell.store_mut().permissions.reset_site(&site);
        save_permissions(app);
    }
    section(ui, "Add a site");
    ui.horizontal(|ui| {
        let mut site = app.chrome.perm_site.clone();
        let response = ui.add(
            egui::TextEdit::singleline(&mut site)
                .desired_width(240.0)
                .hint_text("example.com"),
        );
        if response.changed() {
            app.chrome.perm_site = site;
        }
        if ui.button("Add").clicked() && !app.chrome.perm_site.trim().is_empty() {
            app.shell.store_mut().permissions.set(
                app.chrome.perm_site.trim(),
                "location",
                PermissionState::Ask,
            );
            save_permissions(app);
            app.chrome.perm_site.clear();
        }
    });
}

fn permission_site_row(
    ui: &mut Ui,
    site: String,
    app: &mut BrowserApp,
    reset: &mut Option<String>,
    pal: &Theme,
) {
    let _ = pal;
    egui::CollapsingHeader::new(&site)
        .default_open(false)
        .show(ui, |ui| {
            egui::Grid::new(Id::new(("perm", site.clone()))).show(ui, |ui| {
                for kind in PERMISSION_KINDS {
                    ui.label(*kind);
                    let current = app.shell.store().permissions.get(&site, kind);
                    let next = permission_cycle(current);
                    if ui
                        .button(format!("{current:?}  →  {next:?}"))
                        .on_hover_text("Click to cycle Ask → Allow → Block")
                        .clicked()
                    {
                        let dir = app.shell.store().dir.clone();
                        app.shell.store_mut().permissions.set(&site, kind, next);
                        app.shell.store().permissions.save(&dir);
                    }
                    ui.end_row();
                }
            });
            if ui.button("Reset site").clicked() {
                *reset = Some(site.clone());
            }
        });
}

fn permission_cycle(state: PermissionState) -> PermissionState {
    match state {
        PermissionState::Ask => PermissionState::Allow,
        PermissionState::Allow => PermissionState::Block,
        PermissionState::Block => PermissionState::Ask,
    }
}

fn settings_downloads(app: &mut BrowserApp, ui: &mut Ui, pal: &Theme) {
    section(ui, "Downloads");
    ui.horizontal(|ui| {
        ui.label("Location");
        ui.label(
            RichText::new(
                app.shell
                    .store()
                    .settings
                    .download_dir
                    .display()
                    .to_string(),
            )
            .monospace()
            .small()
            .color(pal.text_dim),
        );
        if ui.button("Change…").clicked() {
            let mut dialog = crate::app::FileDialog {
                purpose: crate::app::FilePurpose::DownloadsDir,
                dir: app.shell.store().settings.download_dir.clone(),
                filename: String::new(),
                entries: Vec::new(),
                error: None,
            };
            dialog.refresh();
            app.file = Some(dialog);
        }
    });
    let mut ask = app.shell.store().settings.ask_download_path;
    ui.checkbox(&mut ask, "Ask where to save each file before downloading");
    if ask != app.shell.store().settings.ask_download_path {
        app.shell.store_mut().settings.ask_download_path = ask;
        persist_settings(app);
    }
}

fn settings_advanced(app: &mut BrowserApp, ui: &mut Ui, pal: &Theme) {
    section(ui, "Engine");
    ui.label(format!("Engine boot: {} ms", app.shell.boot_ms()));
    ui.label(format!("Browser RSS: {} MB", app.shell.rss_kb() / 1024));
    ui.label(format!("Open tabs: {}", app.tabs.len()));
    section(ui, "Suspended-tab idle timeout");
    ui.label(
        RichText::new("Background tabs freeze after the engine's idle timeout (300 s default); frozen tabs free their frames and JS timers.")
            .small()
            .color(pal.text_dim),
    );
    section(ui, "Storage");
    ui.label(
        RichText::new(format!("Profile: {}", app.shell.store().dir.display()))
            .monospace()
            .small()
            .color(pal.text_dim),
    );
}

fn settings_about(_app: &mut BrowserApp, ui: &mut Ui, pal: &Theme) {
    section(ui, "Rrowser85");
    ui.label("Version 0.1.0 (engine v0.1, UI v1).");
    ui.label("A lean, private, fast browser: Rust engine + QuickJS-ng + egui UI.");
    ui.label(
        RichText::new("MIT OR Apache-2.0 licensed. No telemetry, ever.")
            .small()
            .color(pal.text_dim),
    );
}

fn persist_settings(app: &mut BrowserApp) {
    let dir = app.shell.store().dir.clone();
    app.shell.store_mut().settings.save(&dir);
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).strong().size(15.5));
    ui.add_space(4.0);
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

fn history_page(app: &mut BrowserApp, ui: &mut Ui) {
    let pal = app.pal().clone();
    let mut search = app.chrome.history_search.clone();
    let mut clear_data = app.chrome.confirm_clear_data;
    let mut remove: Option<u64> = None;
    let mut navigate: Option<String> = None;
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.heading("History");
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut search)
                .hint_text("Search history")
                .desired_width(260.0),
        );
        if ui.button("Clear browsing data…").clicked() {
            clear_data = true;
        }
    });
    ui.separator();
    ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(6.0);
        let entries: Vec<(u64, String, String, u64)> = {
            let store = app.shell.store();
            store
                .history
                .search(&search, 500)
                .into_iter()
                .map(|e| (e.visited, e.url.clone(), e.title.clone(), e.visited))
                .collect()
        };
        // NOTE: history has no stable ids; dedupe by (url) for display rows.
        let mut last_day = String::new();
        for (visited, url, title, _) in entries.clone() {
            let day = day_label(visited);
            if day != last_day {
                last_day = day.clone();
                ui.add_space(8.0);
                ui.label(RichText::new(day).strong().color(pal.text_dim));
                ui.separator();
            }
            let row_response = ui
                .horizontal(|ui| {
                    ui.label(
                        RichText::new(time_label(visited))
                            .small()
                            .color(pal.text_dim),
                    );
                    let label = if title.is_empty() {
                        url.clone()
                    } else {
                        title.clone()
                    };
                    let response = ui
                        .add(egui::Button::new(
                            RichText::new(crate::chrome::ellipsize_public(ui, &label, 420.0))
                                .color(pal.text),
                        ))
                        .on_hover_text(&url);
                    if response.clicked() {
                        navigate = Some(url.clone());
                    }
                    if response.clicked_by(egui::PointerButton::Middle) {
                        app.new_tab(Some(url.clone()));
                    }
                    ui.label(
                        RichText::new(crate::chrome::ellipsize_public(ui, &url, 360.0))
                            .monospace()
                            .small()
                            .color(pal.text_dim),
                    );
                })
                .response;
            let _ = row_response;
            // Remove button on hover: place at row end.
            if ui
                .add_enabled(true, egui::Button::new("✕").small())
                .on_hover_text("Remove")
                .clicked()
            {
                remove = Some(visited);
            }
        }
        if entries.is_empty() {
            ui.label(RichText::new("No history yet.").color(pal.text_dim));
        }
    });
    app.chrome.history_search = search;
    app.chrome.confirm_clear_data = clear_data;
    if let Some(visited) = remove {
        let dir = app.shell.store().dir.clone();
        app.shell
            .store_mut()
            .history
            .entries
            .retain(|e| e.visited != visited);
        app.shell.store().history.save(&dir);
    }
    if let Some(url) = navigate {
        app.navigate_url(url);
    }
    // Clear-data modal.
    if app.chrome.confirm_clear_data {
        let mut close = false;
        let mut do_clear: Option<Option<u64>> = None;
        egui::Window::new("Clear browsing data")
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .resizable(false)
            .collapsible(false)
            .frame(crate::chrome::window_frame_public(&pal))
            .show(ui.ctx(), |ui| {
                ui.label("Clear history from the last:");
                for (hours, label) in [
                    (Some(1u64), "hour"),
                    (Some(24u64), "24 hours"),
                    (Some(24u64 * 7), "7 days"),
                    (Some(24u64 * 30), "30 days"),
                    (None, "all time"),
                ] {
                    if ui.button(label).clicked() {
                        do_clear = Some(hours);
                    }
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        if let Some(hours) = do_clear {
            let dir = app.shell.store().dir.clone();
            app.shell.store_mut().history.clear_recent(hours);
            app.shell.store().history.save(&dir);
            close = true;
        }
        if close {
            app.chrome.confirm_clear_data = false;
        }
    }
}

fn day_label(visited_ms: u64) -> String {
    let days = (visited_ms / 86_400_000) as i64;
    let today = (now_ms() / 86_400_000) as i64;
    match today - days {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        n => format!("{n} days ago"),
    }
}

fn time_label(visited_ms: u64) -> String {
    let secs_of_day = (visited_ms / 1000) % 86_400;
    format!("{:02}:{:02}", secs_of_day / 3600, (secs_of_day % 3600) / 60)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Bookmarks manager
// ---------------------------------------------------------------------------

fn bookmarks_page(app: &mut BrowserApp, ui: &mut Ui) {
    let pal = app.pal().clone();
    let mut search = app.chrome.bookmarks_search.clone();
    let mut navigate: Option<String> = None;
    let mut edit: Option<(u64, String, String, String)> = None;
    let mut remove: Option<u64> = None;
    let mut import_export: Option<&str> = None;
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.heading("Bookmarks");
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut search)
                .hint_text("Search bookmarks")
                .desired_width(240.0),
        );
        if ui.button("Import…").clicked() {
            import_export = Some("import");
        }
        if ui.button("Export…").clicked() {
            import_export = Some("export");
        }
    });
    ui.separator();
    ui.horizontal(|ui| {
        // Folder sidebar.
        let folders = app.shell.store().bookmarks.folders();
        ScrollArea::vertical()
            .id_salt("bm-folders")
            .max_width(170.0)
            .show(ui, |ui| {
                ui.set_min_width(160.0);
                if ui
                    .selectable_label(app.chrome.bookmarks_folder.is_empty(), "⭐ All")
                    .clicked()
                {
                    app.chrome.bookmarks_folder = String::new();
                }
                if ui
                    .selectable_label(app.chrome.bookmarks_folder == "\0bar", "📋 Bookmarks bar")
                    .clicked()
                {
                    app.chrome.bookmarks_folder = "\0bar".into();
                }
                for folder in folders {
                    if ui
                        .selectable_label(
                            app.chrome.bookmarks_folder == folder,
                            format!("📁 {folder}"),
                        )
                        .clicked()
                    {
                        app.chrome.bookmarks_folder = folder;
                    }
                }
            });
        ui.separator();
        ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(4.0);
            let store = app.shell.store();
            let rows: Vec<(u64, String, String, String)> = store
                .bookmarks
                .items
                .iter()
                .filter(|b| {
                    let folder_match = match app.chrome.bookmarks_folder.as_str() {
                        "" => true,
                        "\0bar" => b.folder.is_empty(),
                        folder => b.folder == folder,
                    };
                    folder_match
                        && (search.is_empty()
                            || b.title.to_lowercase().contains(&search.to_lowercase())
                            || b.url.to_lowercase().contains(&search.to_lowercase()))
                })
                .map(|b| {
                    (
                        b.id,
                        b.url.clone(),
                        b.display_title().to_owned(),
                        b.folder.clone(),
                    )
                })
                .collect();
            for (id, url, title, folder) in rows {
                let url = url.clone();
                let title = title.clone();
                let folder = folder.clone();
                ui.horizontal(|ui| {
                    let response = ui
                        .add(egui::Button::new(
                            RichText::new(crate::chrome::ellipsize_public(ui, &title, 300.0))
                                .color(pal.text),
                        ))
                        .on_hover_text(&url);
                    if response.clicked() {
                        navigate = Some(url.clone());
                    }
                    if response.clicked_by(egui::PointerButton::Middle) {
                        app.new_tab(Some(url.clone()));
                    }
                    if response.secondary_clicked() {
                        edit = Some((id, url.clone(), title.clone(), folder.clone()));
                    }
                    if !folder.is_empty() {
                        ui.label(RichText::new(folder.clone()).small().color(pal.text_dim));
                    }
                    ui.label(
                        RichText::new(crate::chrome::ellipsize_public(ui, &url, 320.0))
                            .monospace()
                            .small()
                            .color(pal.text_dim),
                    );
                    if ui.button("✏").on_hover_text("Edit").clicked() {
                        edit = Some((id, url.clone(), title.clone(), folder.clone()));
                    }
                    if ui.button("✕").on_hover_text("Delete").clicked() {
                        remove = Some(id);
                    }
                });
            }
            if app.shell.store().bookmarks.items.is_empty() {
                ui.label(
                    RichText::new(
                        "No bookmarks yet — press Ctrl+D on a page, or the star in the toolbar.",
                    )
                    .color(pal.text_dim),
                );
            }
        });
    });
    app.chrome.bookmarks_search = search;
    if let Some(url) = navigate {
        app.navigate_url(url);
    }
    if let Some((id, url, title, folder)) = edit {
        app.bookmark_edit = Some(crate::app::BookmarkEdit {
            id,
            url,
            title,
            folder,
        });
    }
    if let Some(id) = remove {
        let dir = app.shell.store().dir.clone();
        app.shell.store_mut().bookmarks.remove(id);
        app.shell.store().bookmarks.save(&dir);
    }
    if let Some(action) = import_export {
        let dir = app.shell.store().settings.download_dir.clone();
        match action {
            "import" => {
                let mut dialog = crate::app::FileDialog {
                    purpose: crate::app::FilePurpose::ImportBookmarks,
                    dir,
                    filename: "bookmarks.html".into(),
                    entries: Vec::new(),
                    error: None,
                };
                dialog.refresh();
                app.file = Some(dialog);
            }
            _ => {
                let mut dialog = crate::app::FileDialog {
                    purpose: crate::app::FilePurpose::ExportBookmarks,
                    dir,
                    filename: "bookmarks.html".into(),
                    entries: Vec::new(),
                    error: None,
                };
                dialog.refresh();
                app.file = Some(dialog);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Downloads manager
// ---------------------------------------------------------------------------

fn downloads_page(app: &mut BrowserApp, ui: &mut Ui) {
    let pal = app.pal().clone();
    let items = app.shell.downloads().snapshot();
    let mut actions: Vec<(u64, u8)> = Vec::new();
    let mut start_download: Option<String> = None;
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.heading("Downloads");
    });
    ui.separator();
    // Download-from-URL (the engine's networking stack).
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label("URL:");
        let response = ui.add(
            egui::TextEdit::singleline(&mut app.chrome.download_url)
                .desired_width(520.0)
                .hint_text("https://… (downloads through the engine's network stack)"),
        );
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("Download").clicked() || (enter && !app.chrome.download_url.is_empty()) {
            start_download = Some(app.chrome.download_url.clone());
        }
    });
    ui.separator();
    ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(6.0);
        if items.is_empty() {
            ui.label(RichText::new("No downloads yet.").color(pal.text_dim));
        }
        for (id, filename, phase, progress, status) in items {
            ui.horizontal(|ui| {
                let icon = match phase {
                    DownloadPhase::Done => Icon::Check,
                    DownloadPhase::Failed => Icon::Warning,
                    _ => Icon::Download,
                };
                let icon_color = match phase {
                    DownloadPhase::Done => pal.success,
                    DownloadPhase::Failed => pal.danger,
                    _ => pal.text,
                };
                let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
                let painter = ui.painter_at(icon_rect);
                icon.paint(&painter, icon_rect, icon_color);
                ui.label(
                    RichText::new(crate::chrome::ellipsize_public(ui, &filename, 260.0)).strong(),
                );
                ui.label(
                    RichText::new(crate::chrome::ellipsize_public(ui, &status, 320.0))
                        .small()
                        .color(pal.text_dim),
                );
                match phase {
                    DownloadPhase::Writing => {
                        if ui.button("⏸ Pause").clicked() {
                            actions.push((id, 0));
                        }
                    }
                    DownloadPhase::Paused => {
                        if ui.button("▶ Resume").clicked() {
                            actions.push((id, 1));
                        }
                    }
                    DownloadPhase::Failed | DownloadPhase::Cancelled => {
                        if ui.button("↻ Retry").clicked() {
                            actions.push((id, 3));
                        }
                    }
                    DownloadPhase::Done => {
                        if ui.button("Open folder").clicked() {
                            actions.push((id, 4));
                        }
                    }
                    DownloadPhase::Fetching => {
                        if ui.button("✕ Cancel").clicked() {
                            actions.push((id, 2));
                        }
                    }
                }
                if phase != DownloadPhase::Fetching && ui.button("Remove").clicked() {
                    actions.push((id, 5));
                }
            });
            // Progress bar row.
            if matches!(phase, DownloadPhase::Writing | DownloadPhase::Paused) {
                let (rect, _) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width() - 60.0, 8.0),
                    Sense::hover(),
                );
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, CornerRadius::same(4), pal.border);
                if let Some(p) = progress {
                    painter.rect_filled(
                        Rect::from_min_size(rect.min, Vec2::new(rect.width() * p, rect.height())),
                        CornerRadius::same(4),
                        pal.accent,
                    );
                }
                ui.add_space(4.0);
            }
            ui.separator();
        }
    });
    for (id, action) in actions {
        match action {
            0 => app.shell.downloads().pause(id),
            1 => app.shell.downloads().resume(id),
            2 => app.shell.downloads().cancel(id),
            3 => app.shell.downloads().restart(id),
            4 => open_containing_folder(&app.shell.downloads().items(), id),
            _ => app.shell.downloads().remove(id),
        }
    }
    if let Some(url) = start_download {
        let dir = app.shell.store().settings.download_dir.clone();
        app.shell.downloads().start(url, dir);
        app.chrome.download_url.clear();
    }
}

fn open_containing_folder(items: &[std::sync::Arc<rowser_shell::DownloadItem>], id: u64) {
    let Some(item) = items.iter().find(|i| i.id == id) else {
        return;
    };
    let dir = item.path.parent().map(|p| p.to_path_buf());
    let Some(dir) = dir else { return };
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
    }
}

// ---------------------------------------------------------------------------
// Privacy dashboard
// ---------------------------------------------------------------------------

fn privacy_page(app: &mut BrowserApp, ui: &mut Ui) {
    let pal = app.pal().clone();
    let stats = app.shell.stats().clone();
    ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.heading("Privacy dashboard");
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            stat_card(
                ui,
                "Trackers blocked",
                &stats.trackers_blocked.to_string(),
                Icon::Shield,
                pal.accent,
                &pal,
            );
            stat_card(
                ui,
                "Ads blocked",
                &stats.ads_blocked.to_string(),
                Icon::Shield,
                pal.danger,
                &pal,
            );
            stat_card(
                ui,
                "CNAME trackers unmasked",
                &stats.cname_unmasked.to_string(),
                Icon::External,
                pal.warning,
                &pal,
            );
            stat_card(
                ui,
                "Fingerprint protections active",
                &stats
                    .fingerprint_protections(&app.shell.store().settings.privacy.to_engine())
                    .to_string(),
                Icon::Lock,
                pal.success,
                &pal,
            );
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(
                RichText::new(
                    "Every protection on this page is enforced in the engine core (network layer, JS runtime, storage) — not in UI filters a site can dodge.",
                )
                .small()
                .color(pal.text_dim),
            );
        });
        ui.add_space(8.0);
        // Per-tab breakdown.
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(RichText::new("Per-tab blocking").strong());
        });
        egui::Grid::new("per-tab").show(ui, |ui| {
            ui.strong("Tab");
            ui.strong("URL");
            ui.strong("Blocked");
            ui.end_row();
            for tab in &app.tabs {
                ui.label(format!("#{}", tab.id));
                ui.label(
                    RichText::new(crate::chrome::ellipsize_public(ui, &tab.url, 420.0))
                        .monospace()
                        .small(),
                );
                ui.label(stats.per_tab.get(&tab.id).copied().unwrap_or(0).to_string());
                ui.end_row();
            }
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            if ui.button("Open settings → Privacy").clicked() {
                app.navigate_url("rowser://settings");
            }
        });
    });
}

fn stat_card(ui: &mut Ui, title: &str, value: &str, icon: Icon, color: Color32, pal: &Theme) {
    let frame = egui::Frame::default()
        .fill(pal.field_bg)
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .stroke(Stroke::new(1.0_f32, pal.border));
    frame.show(ui, |ui| {
        ui.set_min_width(150.0);
        ui.horizontal(|ui| {
            let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
            let painter = ui.painter_at(icon_rect);
            icon.paint(&painter, icon_rect, color);
            ui.label(RichText::new(value).strong().size(24.0).color(color));
        });
        ui.label(RichText::new(title).small().color(pal.text_dim));
    });
}

// ---------------------------------------------------------------------------
// Extensions
// ---------------------------------------------------------------------------

fn extensions_page(app: &mut BrowserApp, ui: &mut Ui) {
    let pal = app.pal().clone();
    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            let icon_rect = ui.allocate_exact_size(Vec2::splat(70.0), Sense::hover()).0;
            let painter = ui.painter_at(icon_rect);
            Icon::Puzzle.paint(&painter, icon_rect, pal.text_dim);
            ui.add_space(12.0);
            ui.label(RichText::new("Extensions").strong().size(20.0));
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "The engine core (v1) does not yet expose an extension host.\nThis panel will list installed extensions the moment the engine ships one —\nno placeholder switches, no fake toggles.",
                )
                .color(pal.text_dim),
            );
            ui.add_space(12.0);
            if ui.button("Read the engine docs").clicked() {
                app.navigate_url("https://github.com/salim77007j/rowser85");
            }
        });
    });
}

fn ellipsize_url(url: &str, max: usize) -> String {
    if url.chars().count() <= max {
        url.to_owned()
    } else {
        let prefix: String = url.chars().take(max / 2).collect();
        let suffix: String = url.chars().skip(url.chars().count() - max / 3).collect();
        format!("{prefix}…{suffix}")
    }
}
