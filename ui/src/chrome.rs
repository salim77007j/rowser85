//! Chrome: tab strip, toolbar + omnibox, bookmarks bar, status bar,
//! downloads shelf, devtools, find bar and all overlays.

use egui::{
    Align2, CentralPanel, Color32, Context, CornerRadius, Id, Order, Pos2, Rect, Sense, Shape,
    Stroke, TopBottomPanel, Ui, Vec2,
};
use rowser_shell::{SuggestionKind, TopSite};

use crate::app::{BrowserApp, FilePurpose, TabUi};
use crate::icons::Icon;
use crate::pages;
use crate::theme::Theme;

/// Chrome drawing state (focus flags, layout scratch, page-local UI state).
#[derive(Default)]
pub struct Chrome {
    /// Focus the omnibox next frame.
    pub omnibox_take_focus: bool,
    /// Focus the find input next frame.
    pub find_take_focus: bool,
    /// Fullscreen state.
    pub is_fullscreen: bool,
    /// The devtools section shown.
    pub devtools_section: String,
    /// Tab rects laid out this frame (for drag-reorder).
    tab_rects: Vec<(usize, Rect)>,
    /// NTP search field text.
    pub search_text: String,
    /// Settings section shown.
    pub settings_section: String,
    /// Settings scratch: bookmarks bar toggle.
    pub settings_bookmarks_bar: bool,
    /// Clear-browsing-data modal open.
    pub confirm_clear_data: bool,
    /// Permission add-site field.
    pub perm_site: String,
    /// History search field.
    pub history_search: String,
    /// Bookmarks manager search field.
    pub bookmarks_search: String,
    /// Bookmarks manager selected folder ("" all, "\0bar" bar).
    pub bookmarks_folder: String,
    /// Downloads page "new download" URL field.
    pub download_url: String,
}

impl Chrome {
    /// Draws the whole chrome + content for this frame.
    pub fn draw(&mut self, app: &mut BrowserApp, ctx: &Context) {
        self.tab_strip(app, ctx);
        self.toolbar(app, ctx);
        if app.shell.store().settings.show_bookmarks_bar
            && app
                .tabs
                .get(app.active)
                .map(|t| t.internal_page().is_none())
                .unwrap_or(true)
        {
            self.bookmarks_bar(app, ctx);
        }
        if app.devtools_open {
            self.devtools(app, ctx);
        }
        self.download_shelf(app, ctx);
        self.status_bar(app, ctx);
        self.content(app, ctx);
        self.find_bar(app, ctx);
        self.suggestions(app, ctx);
        self.tab_context_menu(app, ctx);
        self.dialogs(app, ctx);
        self.toasts(app, ctx);
        app.sync_viewport();
    }

    // -----------------------------------------------------------------------
    // Tab strip
    // -----------------------------------------------------------------------

    fn tab_strip(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let height = 38.0;
        let pal = app.theme.clone();
        self.tab_rects.clear();
        TopBottomPanel::top("tab_strip")
            .exact_height(height)
            .frame(egui::Frame::NONE.fill(pal.chrome_bg))
            .show(ctx, |ui| {
                let strip = ui.available_rect_before_wrap();
                let painter = ui.painter_at(strip);
                let top = strip.top();

                let pinned_count = app.tabs.iter().filter(|t| t.pinned).count();
                let normal_count = app.tabs.len().saturating_sub(pinned_count);
                let reserved = 52.0;
                let tab_w = ((strip.width() - reserved - pinned_count as f32 * 44.0)
                    / (normal_count.max(1) as f32))
                    .clamp(56.0, 240.0);

                let mut cursor = strip.left() + 6.0;
                // Pinned first, then the rest, each preserving app.tabs order.
                let mut order: Vec<usize> = (0..app.tabs.len()).collect();
                order.sort_by_key(|&i| app.tabs[i].pinned);

                for &index in &order {
                    let width = if app.tabs[index].pinned { 44.0 } else { tab_w };
                    let rect = Rect::from_min_size(
                        Pos2::new(cursor, top + 5.0),
                        Vec2::new(width, height - 5.0),
                    );
                    cursor += width;
                    if cursor > strip.right() {
                        break;
                    }
                    self.tab_rects.push((index, rect));
                    self.draw_tab(app, ui, &pal, index, rect);
                }

                // New-tab button.
                let plus = Rect::from_center_size(
                    Pos2::new(cursor + 24.0, strip.center().y),
                    Vec2::splat(28.0),
                );
                let response = ui.interact(plus, Id::new("new-tab"), Sense::click());
                if response.hovered() {
                    painter.rect_filled(plus, CornerRadius::same(14), pal.hover);
                }
                Icon::Plus.paint(&painter, plus, pal.text_dim);
                if response.clicked() {
                    app.new_tab(None);
                }

                // Double-click empty space → new tab.
                if cursor + 60.0 < strip.right() {
                    let empty = Rect::from_min_max(
                        Pos2::new(cursor + 50.0, strip.top()),
                        strip.right_bottom(),
                    );
                    let empty_response = ui.interact(empty, Id::new("strip-empty"), Sense::click());
                    if empty_response.double_clicked() {
                        app.new_tab(None);
                    }
                }

                // Drag reorder: bubble the dragged tab toward the pointer.
                if let (Some(drag), Some(pointer)) = (app.drag, ui.input(|i| i.pointer.hover_pos()))
                {
                    if ui.input(|i| i.pointer.primary_down()) {
                        let mut target = drag;
                        for &(index, rect) in &self.tab_rects {
                            if index != drag && rect.contains(pointer) {
                                target = index;
                            }
                        }
                        if target != drag
                            && app.tabs.get(drag).map(|t| t.pinned)
                                == app.tabs.get(target).map(|t| t.pinned)
                        {
                            app.tabs.swap(drag, target);
                            app.active = app
                                .tabs
                                .iter()
                                .position(|t| t.id == app.tabs[app.active].id)
                                .unwrap_or(app.active);
                            app.drag = Some(target);
                        }
                    } else {
                        app.drag = None;
                    }
                }

                painter.line_segment(
                    [strip.left_bottom(), strip.right_bottom()],
                    Stroke::new(1.0_f32, pal.border),
                );
            });
    }

    fn draw_tab(
        &mut self,
        app: &mut BrowserApp,
        ui: &mut Ui,
        pal: &Theme,
        index: usize,
        rect: Rect,
    ) {
        let id = Id::new(("tab", app.tabs[index].id));
        let response = ui.interact(rect, id, Sense::click_and_drag());
        let is_active = app.active == index;
        let suspended = app.tabs[index].suspended;
        let loading = app.tabs[index].loading;
        let pinned = app.tabs[index].pinned;
        let muted = app.tabs[index].muted;
        let group_color = app.tabs[index].group.as_ref().map(|g| g.color);

        let painter = ui.painter_at(rect);
        let bg = if is_active {
            pal.surface
        } else if response.hovered() {
            lighten(pal.chrome_bg, 0.06)
        } else {
            lighten(pal.chrome_bg, 0.03)
        };
        painter.rect_filled(
            rect,
            CornerRadius {
                nw: 9,
                ne: 9,
                sw: 0,
                se: 0,
            },
            bg,
        );

        if let Some(color) = group_color {
            painter.rect_filled(
                Rect::from_min_size(
                    Pos2::new(rect.left() + 4.0, rect.bottom() - 3.0),
                    Vec2::new(rect.width() - 8.0, 3.0),
                ),
                CornerRadius::same(2),
                color,
            );
        }

        // Favicon / spinner.
        let letter = app.tabs[index].letter();
        let icon_rect = Rect::from_center_size(
            Pos2::new(rect.left() + 17.0, rect.center().y),
            Vec2::splat(17.0),
        );
        if loading {
            let angle = ui.input(|i| i.time as f32 * 4.0);
            painter.add(Shape::Path(egui::epaint::PathShape::line(
                arc_points_helper(icon_rect.center(), 6.0, angle, angle + 4.4, 14),
                Stroke::new(2.0_f32, pal.accent),
            )));
        } else {
            painter.circle_filled(icon_rect.center(), 8.5, pal.accent_soft);
            painter.text(
                icon_rect.center(),
                Align2::CENTER_CENTER,
                letter.to_string(),
                egui::FontId::proportional(10.5),
                if suspended {
                    Color32::from_gray(150)
                } else {
                    pal.accent
                },
            );
        }

        // Title.
        let show_close = (is_active || response.hovered()) && !pinned;
        let text_max = rect.width() - 34.0 - if show_close { 24.0 } else { 0.0 };
        if !pinned {
            let title = app.tabs[index].title.clone();
            let text = if title.is_empty() {
                "New tab".to_owned()
            } else {
                ellipsize(ui, &title, text_max)
            };
            let right = if show_close { 40.0 } else { 14.0 };
            painter.text(
                Pos2::new(rect.left() + 30.0, rect.center().y),
                Align2::LEFT_CENTER,
                text,
                egui::FontId::proportional(13.0),
                if is_active && !muted {
                    pal.text
                } else {
                    pal.text_dim
                },
            );
            if muted {
                let mute_rect = Rect::from_center_size(
                    Pos2::new(rect.right() - right + 8.0, rect.center().y),
                    Vec2::splat(13.0),
                );
                Icon::Mute.paint(&painter, mute_rect, pal.text_dim);
            }
        } else if muted {
            Icon::Mute.paint(
                &painter,
                Rect::from_center_size(
                    Pos2::new(rect.right() - 12.0, rect.center().y),
                    Vec2::splat(13.0),
                ),
                pal.text_dim,
            );
        }

        // Close button.
        if show_close {
            let close_rect = Rect::from_center_size(
                Pos2::new(rect.right() - 13.0, rect.center().y),
                Vec2::splat(20.0),
            );
            let close_response = ui.interact(
                close_rect,
                Id::new(("tab-close", app.tabs[index].id)),
                Sense::click(),
            );
            if close_response.hovered() {
                painter.rect_filled(close_rect, CornerRadius::same(10), pal.hover);
            }
            Icon::Close.paint(&painter, close_rect, pal.text_dim);
            if close_response.clicked() {
                app.close_tab(index);
            }
        }

        // Interactions.
        if response.clicked() {
            app.set_active(index);
        }
        let response = if let Some(group) = app.tabs[index].group.clone() {
            response.on_hover_text(format!("Group: {}", group.name))
        } else {
            response
        };
        if response.clicked_by(egui::PointerButton::Middle) {
            app.close_tab(index);
        }
        if response.dragged() && app.drag.is_none() {
            app.drag = Some(index);
        }
        if response.secondary_clicked() {
            app.ctx_menu = Some((
                index,
                ui.input(|i| i.pointer.hover_pos().unwrap_or_default()),
            ));
        }
    }

    // -----------------------------------------------------------------------
    // Toolbar
    // -----------------------------------------------------------------------

    fn toolbar(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let height = 46.0;
        let pal = app.theme.clone();
        let loading = app.tabs.get(app.active).map(|t| t.loading).unwrap_or(false);
        let progress = app.tabs.get(app.active).map(|t| t.progress).unwrap_or(0.0);
        let can_back = app
            .tabs
            .get(app.active)
            .map(|t| t.can_back)
            .unwrap_or(false);
        let can_fwd = app.tabs.get(app.active).map(|t| t.can_fwd).unwrap_or(false);
        let url = app
            .tabs
            .get(app.active)
            .map(|t| t.url.clone())
            .unwrap_or_default();

        let mut nav_action: Vec<String> = Vec::new();
        TopBottomPanel::top("toolbar")
            .exact_height(height)
            .frame(egui::Frame::NONE.fill(pal.toolbar_bg))
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(4.0);
                    let nav = |ui: &mut Ui,
                               icon: Icon,
                               enabled: bool,
                               id: &str,
                               action: &mut Vec<String>| {
                        let (rect, response) =
                            ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
                        let painter = ui.painter_at(rect);
                        let color = if enabled {
                            pal.text
                        } else {
                            pal.text_dim.gamma_multiply(0.45)
                        };
                        if enabled && response.hovered() {
                            painter.rect_filled(rect, CornerRadius::same(16), pal.hover);
                        }
                        icon.paint(&painter, rect, color);
                        if enabled && response.clicked() {
                            action.push(id.to_owned());
                        }
                    };
                    nav(ui, Icon::Back, can_back, "back", &mut nav_action);
                    nav(ui, Icon::Forward, can_fwd, "fwd", &mut nav_action);
                    if loading {
                        nav(ui, Icon::Stop, true, "stop", &mut nav_action);
                    } else {
                        nav(ui, Icon::Reload, true, "reload", &mut nav_action);
                    }
                    nav(ui, Icon::Home, true, "home", &mut nav_action);

                    // Omnibox pill.
                    let width = ui.available_width() - 190.0;
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::new(width.max(120.0), 34.0), Sense::click());
                    app.omnibox_rect = rect;
                    let painter = ui.painter_at(rect);
                    let focused = app.omnibox_focused;
                    painter.rect_filled(
                        rect,
                        CornerRadius::same(17),
                        if focused {
                            pal.field_focus_bg
                        } else {
                            pal.field_bg
                        },
                    );
                    if focused {
                        painter.rect_stroke(
                            rect,
                            CornerRadius::same(17),
                            Stroke::new(1.6_f32, pal.accent),
                            egui::epaint::StrokeKind::Inside,
                        );
                    }

                    // Security / search indicator.
                    let editing = app.omnibox_focused;
                    let is_search = editing
                        && rowser_shell::normalize_url(&app.omnibox).is_none()
                        && !app.omnibox.trim().is_empty();
                    let (icon, icon_color) = if is_search {
                        (Icon::Search, pal.text_dim)
                    } else if url.starts_with("https://") {
                        (Icon::Lock, pal.success)
                    } else if url.starts_with("http://") {
                        (Icon::Warning, pal.warning)
                    } else if url.starts_with("rowser:") {
                        (Icon::Info, pal.text_dim)
                    } else {
                        (Icon::Search, pal.text_dim)
                    };
                    icon.paint(
                        &painter,
                        Rect::from_center_size(
                            Pos2::new(rect.left() + 18.0, rect.center().y),
                            Vec2::splat(15.0),
                        ),
                        icon_color,
                    );

                    // The text field inside the pill.
                    let enter_pressed;
                    let field_clicked;
                    {
                        let field_response = ui.put(
                            Rect::from_min_max(
                                Pos2::new(rect.left() + 36.0, rect.top() + 1.0),
                                Pos2::new(rect.right() - 36.0, rect.bottom() - 1.0),
                            ),
                            egui::TextEdit::singleline(&mut app.omnibox)
                                .id(Id::new("omnibox-field"))
                                .frame(false)
                                .hint_text("Search or enter address")
                                .font(egui::FontId::proportional(14.5))
                                .margin(Vec2::new(0.0, 4.0)),
                        );
                        field_clicked = field_response.clicked();
                        if field_response.changed() {
                            app.omnibox_focused = true;
                            app.sugg_index = 0;
                        }
                        if field_response.has_focus() {
                            app.omnibox_focused = true;
                        }
                        enter_pressed = field_response.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if std::env::var("ROWSER_UI_DEBUG").is_ok() {
                            eprintln!(
                                "[ui] enter check: lost={} key={} focused={} text={:?}",
                                field_response.lost_focus(),
                                ui.input(|i| i.key_pressed(egui::Key::Enter)),
                                app.omnibox_focused,
                                app.omnibox
                            );
                        }
                        if enter_pressed {
                            if let Some(selected) = app.suggestions.get(app.sugg_index) {
                                app.navigate_url(selected.url.clone());
                            } else {
                                let input = app.omnibox.clone();
                                app.navigate_input(&input);
                            }
                            field_response.surrender_focus();
                        }
                    }
                    // Focus on request (Ctrl+L), selecting everything.
                    if self.omnibox_take_focus {
                        self.omnibox_take_focus = false;
                        ui.memory_mut(|m| m.request_focus(Id::new("omnibox-field")));
                    }
                    if field_clicked {
                        // Clicking the omnibox starts a fresh edit: clear
                        // the field (the placeholder takes over) so typed
                        // input can never append to stale text.
                        if !app.omnibox_focused {
                            app.omnibox_focused = true;
                        }
                        app.omnibox.clear();
                        ui.memory_mut(|m| {
                            m.surrender_focus(Id::new("omnibox-field"));
                            m.request_focus(Id::new("omnibox-field"));
                        });
                    } else if response.clicked() && !app.omnibox_focused {
                        app.omnibox_focused = true;
                        app.omnibox.clear();
                    }

                    // Right cluster.
                    let mut right_actions: Vec<String> = Vec::new();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Menu button (dropdown).
                        ui.menu_button("⋮", |ui| {
                            self.menu(app, ui);
                        });

                        // Devtools.
                        let (rect, response) =
                            ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                        let painter = ui.painter_at(rect);
                        if response.hovered() {
                            painter.rect_filled(rect, CornerRadius::same(15), pal.hover);
                        }
                        Icon::Code.paint(
                            &painter,
                            rect,
                            if app.devtools_open {
                                pal.accent
                            } else {
                                pal.text
                            },
                        );
                        if response.clicked() {
                            right_actions.push("devtools".to_owned());
                        }

                        // Privacy shield.
                        let stats = app.shell.stats().total_blocked();
                        let (rect, response) =
                            ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                        let painter = ui.painter_at(rect);
                        if response.hovered() {
                            painter.rect_filled(rect, CornerRadius::same(15), pal.hover);
                        }
                        Icon::Shield.paint(&painter, rect, pal.text);
                        if stats > 0 {
                            badge(&painter, rect, compact_count(stats), &pal);
                        }
                        if response.clicked() {
                            right_actions.push("privacy".to_owned());
                        }

                        // Downloads.
                        let active_downloads = app
                            .shell
                            .downloads()
                            .snapshot()
                            .into_iter()
                            .filter(|d| {
                                matches!(
                                    d.2,
                                    rowser_shell::DownloadPhase::Fetching
                                        | rowser_shell::DownloadPhase::Writing
                                        | rowser_shell::DownloadPhase::Paused
                                )
                            })
                            .count();
                        let (rect, response) =
                            ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                        let painter = ui.painter_at(rect);
                        if response.hovered() {
                            painter.rect_filled(rect, CornerRadius::same(15), pal.hover);
                        }
                        Icon::Download.paint(
                            &painter,
                            rect,
                            if active_downloads > 0 {
                                pal.accent
                            } else {
                                pal.text
                            },
                        );
                        if active_downloads > 0 {
                            badge(&painter, rect, compact_count(active_downloads as u64), &pal);
                        }
                        if response.clicked() {
                            right_actions.push("downloads".to_owned());
                        }

                        // Star (bookmark).
                        let starred = app.shell.store().bookmarks.find_url(&url).is_some();
                        let (rect, response) =
                            ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                        let painter = ui.painter_at(rect);
                        if response.hovered() {
                            painter.rect_filled(rect, CornerRadius::same(15), pal.hover);
                        }
                        Icon::Star.paint(
                            &painter,
                            rect,
                            if starred { pal.warning } else { pal.text_dim },
                        );
                        if response.clicked() {
                            right_actions.push("star".to_owned());
                        }
                    });
                    for action in right_actions {
                        match action.as_str() {
                            "devtools" => app.devtools_open = !app.devtools_open,
                            "privacy" => app.navigate_url("rowser://privacy"),
                            "downloads" => app.downloads_shelf = !app.downloads_shelf,
                            "star" => app.toggle_bookmark(),
                            _ => {}
                        }
                    }
                });

                // Loading progress line.
                if loading {
                    let rect = ui.max_rect();
                    let painter = ui.painter_at(rect);
                    let bar_w = rect.width() * progress.clamp(0.03, 1.0);
                    painter.rect_filled(
                        Rect::from_min_size(
                            Pos2::new(rect.left(), rect.bottom() - 2.0),
                            Vec2::new(bar_w, 2.5),
                        ),
                        CornerRadius::same(1),
                        pal.accent,
                    );
                }
            });
        for action in nav_action {
            match action.as_str() {
                "back" => app.go_back(),
                "fwd" => app.go_forward(),
                "reload" => app.reload(),
                "stop" => app.stop_load(),
                "home" => {
                    if let Some(tab) = app.tabs.get(app.active) {
                        let home = app.shell.store().settings.homepage.clone();
                        app.shell.browser().navigate(tab.id, home);
                    }
                }
                _ => {}
            }
        }
        app.toolbar_rect = ctx.screen_rect();
    }

    fn menu(&mut self, app: &mut BrowserApp, ui: &mut Ui) {
        let mut actions: Vec<String> = Vec::new();
        let pal = app.theme.clone();
        let fullscreen = self.is_fullscreen;
        {
            ui.set_min_width(262.0);
            let item = |ui: &mut Ui,
                        icon: Icon,
                        label: &str,
                        action: &str,
                        actions: &mut Vec<String>,
                        pal: &Theme| {
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(250.0, 26.0), Sense::click());
                let painter = ui.painter_at(rect);
                if response.hovered() {
                    painter.rect_filled(rect, CornerRadius::same(6), pal.hover);
                }
                icon.paint(
                    &painter,
                    Rect::from_center_size(
                        Pos2::new(rect.left() + 13.0, rect.center().y),
                        Vec2::splat(16.0),
                    ),
                    pal.text,
                );
                painter.text(
                    Pos2::new(rect.left() + 32.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    label,
                    egui::FontId::proportional(13.5),
                    pal.text,
                );
                if response.clicked() {
                    actions.push(action.to_owned());
                    ui.close();
                }
            };
            item(
                ui,
                Icon::Plus,
                "New tab  ·  Ctrl+T",
                "new-tab",
                &mut actions,
                &pal,
            );
            item(
                ui,
                Icon::Reload,
                "Reopen closed tab  ·  Ctrl+Shift+T",
                "reopen",
                &mut actions,
                &pal,
            );
            ui.separator();
            item(
                ui,
                Icon::Bookmarks,
                "Bookmarks  ·  Ctrl+Shift+O",
                "bookmarks",
                &mut actions,
                &pal,
            );
            item(
                ui,
                Icon::Clock,
                "History  ·  Ctrl+H",
                "history",
                &mut actions,
                &pal,
            );
            item(
                ui,
                Icon::Download,
                "Downloads  ·  Ctrl+J",
                "downloads",
                &mut actions,
                &pal,
            );
            item(
                ui,
                Icon::Shield,
                "Privacy dashboard",
                "privacy",
                &mut actions,
                &pal,
            );
            item(
                ui,
                Icon::Puzzle,
                "Extensions",
                "extensions",
                &mut actions,
                &pal,
            );
            ui.separator();
            item(
                ui,
                Icon::Find,
                "Find in page  ·  Ctrl+F",
                "find",
                &mut actions,
                &pal,
            );
            item(
                ui,
                Icon::ZoomIn,
                "Zoom in  ·  Ctrl+Plus",
                "zoom-in",
                &mut actions,
                &pal,
            );
            item(
                ui,
                Icon::ZoomOut,
                "Zoom out  ·  Ctrl+Minus",
                "zoom-out",
                &mut actions,
                &pal,
            );
            item(
                ui,
                Icon::Print,
                "Print…  ·  Ctrl+P",
                "print",
                &mut actions,
                &pal,
            );
            item(
                ui,
                Icon::Save,
                "Save page…  ·  Ctrl+S",
                "save",
                &mut actions,
                &pal,
            );
            item(
                ui,
                Icon::Fullscreen,
                "Fullscreen  ·  F11",
                "fullscreen",
                &mut actions,
                &pal,
            );
            ui.separator();
            item(ui, Icon::Gear, "Settings", "settings", &mut actions, &pal);
            let _ = fullscreen;
        }
        for action in actions {
            match action.as_str() {
                "new-tab" => app.new_tab(None),
                "reopen" => {
                    if let Some(id) = app.shell.reopen_closed_tab() {
                        app.tabs.push(TabUi::new(
                            id,
                            app.shell
                                .browser()
                                .snapshot(id)
                                .map(|s| s.url.clone())
                                .unwrap_or_default(),
                            String::new(),
                        ));
                        app.set_active(app.tabs.len() - 1);
                    }
                }
                "bookmarks" => app.navigate_url("rowser://bookmarks"),
                "history" => app.navigate_url("rowser://history"),
                "downloads" => app.navigate_url("rowser://downloads"),
                "privacy" => app.navigate_url("rowser://privacy"),
                "extensions" => app.navigate_url("rowser://extensions"),
                "find" => {
                    app.find_open = true;
                    self.find_take_focus = true;
                }
                "zoom-in" => app.zoom_step(1),
                "zoom-out" => app.zoom_step(-1),
                "print" => app.open_print_dialog(),
                "save" => app.open_save_dialog(),
                "fullscreen" => {
                    let fs = self.is_fullscreen;
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
                    self.is_fullscreen = !fs;
                }
                "settings" => app.navigate_url("rowser://settings"),
                _ => {}
            }
        }
    }

    // -----------------------------------------------------------------------
    // Bookmarks bar
    // -----------------------------------------------------------------------

    fn bookmarks_bar(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let pal = app.theme.clone();
        let entries: Vec<(u64, String, String)> = app
            .shell
            .store()
            .bookmarks
            .bar()
            .iter()
            .map(|b| (b.id, b.display_title().to_owned(), b.url.clone()))
            .collect();
        let folders = app.shell.store().bookmarks.folders();
        let mut actions: Vec<(u64, bool)> = Vec::new();
        let mut folder_opens: Vec<String> = Vec::new();
        TopBottomPanel::top("bookmarks_bar")
            .exact_height(30.0)
            .frame(egui::Frame::NONE.fill(pal.surface))
            .show(ctx, |ui| {
                let painter = ui.painter();
                let rect = ui.max_rect();
                painter.line_segment(
                    [rect.left_top(), rect.right_top()],
                    Stroke::new(1.0_f32, pal.border),
                );
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    for (id, title, url) in entries.iter().take(24) {
                        let label = ellipsize(ui, title, 130.0);
                        let response = ui
                            .add(egui::Button::new(egui::RichText::new(label).size(12.5)))
                            .on_hover_text(url);
                        if response.clicked() {
                            actions.push((*id, false));
                        }
                        if response.clicked_by(egui::PointerButton::Middle) {
                            actions.push((*id, true));
                        }
                        if response.secondary_clicked() {
                            app.bookmark_edit = Some(crate::app::BookmarkEdit {
                                id: *id,
                                url: url.clone(),
                                title: title.clone(),
                                folder: String::new(),
                            });
                        }
                    }
                    for folder in folders {
                        let response = ui.add(egui::Button::new(format!("📁 {}", folder)).small());
                        if response.clicked() {
                            folder_opens.push(folder.clone());
                        }
                    }
                });
            });
        for (id, new_tab) in actions {
            let url = app
                .shell
                .store()
                .bookmarks
                .items
                .iter()
                .find(|b| b.id == id)
                .map(|b| b.url.clone());
            if let Some(url) = url {
                if new_tab {
                    app.new_tab(Some(url));
                } else {
                    app.navigate_url(url);
                }
            }
        }
        for folder in folder_opens {
            let urls: Vec<String> = app
                .shell
                .store()
                .bookmarks
                .items
                .iter()
                .filter(|b| b.folder == folder)
                .map(|b| b.url.clone())
                .collect();
            for url in urls {
                app.new_tab(Some(url));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Status bar
    // -----------------------------------------------------------------------

    fn status_bar(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let Some(tab) = app.tabs.get(app.active) else {
            return;
        };
        let hover = app.hover_link.clone();
        let blocked = tab.blocked;
        let memory = app
            .shell
            .browser()
            .snapshot(tab.id)
            .map(|s| s.memory_bytes)
            .unwrap_or(0);
        let zoom = tab.zoom;
        let rss = app.shell.rss_kb();
        let pal = app.theme.clone();
        TopBottomPanel::bottom("status_bar")
            .exact_height(22.0)
            .frame(egui::Frame::NONE.fill(pal.surface))
            .show(ctx, |ui| {
                let painter = ui.painter();
                let rect = ui.max_rect();
                painter.line_segment(
                    [rect.left_top(), rect.right_top()],
                    Stroke::new(1.0_f32, pal.border),
                );
                ui.horizontal_centered(|ui| {
                    ui.add_space(8.0);
                    if let Some(link) = hover {
                        ui.label(
                            egui::RichText::new(ellipsize(ui, &link, ui.available_width() - 260.0))
                                .small()
                                .color(pal.text_dim),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(format!("{} MB RSS", rss / 1024))
                                .small()
                                .color(pal.text_dim),
                        );
                        if blocked > 0 {
                            ui.label(
                                egui::RichText::new(format!("{blocked} blocked"))
                                    .small()
                                    .color(pal.accent),
                            );
                        }
                        if memory > 0 {
                            ui.label(
                                egui::RichText::new(format!("{} MB page", memory / 1_048_576))
                                    .small()
                                    .color(pal.text_dim),
                            );
                        }
                        ui.label(
                            egui::RichText::new(format!("{}%", (zoom * 100.0).round() as i32))
                                .small()
                                .color(pal.text_dim),
                        );
                    });
                });
            });
    }

    // -----------------------------------------------------------------------
    // Downloads shelf
    // -----------------------------------------------------------------------

    fn download_shelf(&mut self, app: &mut BrowserApp, ctx: &Context) {
        if !app.downloads_shelf {
            return;
        }
        let items = app.shell.downloads().items();
        if items.is_empty() {
            app.downloads_shelf = false;
            return;
        }
        let pal = app.theme.clone();
        let mut actions: Vec<(u64, u8)> = Vec::new();
        let mut open_page = false;
        let mut close_shelf = false;
        let time = ctx.input(|i| i.time as f32);
        TopBottomPanel::bottom("download_shelf")
            .exact_height(86.0)
            .frame(egui::Frame::NONE.fill(pal.surface))
            .show(ctx, |ui| {
                let painter = ui.painter();
                let rect = ui.max_rect();
                painter.line_segment(
                    [rect.left_top(), rect.right_top()],
                    Stroke::new(1.0_f32, pal.border),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for item in items.iter().take(6) {
                        let phase = *item.phase.lock().unwrap();
                        let progress = item.progress();
                        let status = item.status_text();
                        let (response_rect, response) =
                            ui.allocate_exact_size(Vec2::new(252.0, 70.0), Sense::click());
                        let painter = ui.painter_at(response_rect);
                        painter.rect_filled(response_rect, CornerRadius::same(8), pal.field_bg);
                        painter.text(
                            Pos2::new(response_rect.left() + 10.0, response_rect.top() + 15.0),
                            Align2::LEFT_CENTER,
                            ellipsize(ui, &item.filename, 190.0),
                            egui::FontId::proportional(13.0),
                            pal.text,
                        );
                        painter.text(
                            Pos2::new(response_rect.left() + 10.0, response_rect.top() + 32.0),
                            Align2::LEFT_CENTER,
                            ellipsize(ui, &status, 200.0),
                            egui::FontId::proportional(11.0),
                            pal.text_dim,
                        );
                        let bar = Rect::from_min_size(
                            Pos2::new(response_rect.left() + 10.0, response_rect.top() + 46.0),
                            Vec2::new(186.0, 6.0),
                        );
                        painter.rect_filled(bar, CornerRadius::same(3), pal.border);
                        let fill_w = match progress {
                            Some(p) => bar.width() * p.clamp(0.0, 1.0),
                            None if phase == rowser_shell::DownloadPhase::Fetching => {
                                ((time * 1.6).fract() * bar.width()).abs()
                            }
                            None => 0.0,
                        };
                        painter.rect_filled(
                            Rect::from_min_size(bar.min, Vec2::new(fill_w, bar.height())),
                            CornerRadius::same(3),
                            pal.accent,
                        );

                        // Controls: pause / resume / cancel.
                        let control_rect = Rect::from_center_size(
                            Pos2::new(response_rect.right() - 18.0, response_rect.top() + 50.0),
                            Vec2::splat(26.0),
                        );
                        let control =
                            ui.interact(control_rect, Id::new(("dl-ctl", item.id)), Sense::click());
                        let control_painter = ui.painter_at(control_rect);
                        let control_icon = match phase {
                            rowser_shell::DownloadPhase::Writing => Icon::Clock,
                            rowser_shell::DownloadPhase::Paused => Icon::Reload,
                            _ => Icon::Close,
                        };
                        control_icon.paint(&control_painter, control_rect, pal.text_dim);
                        if control.clicked() {
                            match phase {
                                rowser_shell::DownloadPhase::Writing => actions.push((item.id, 0)),
                                rowser_shell::DownloadPhase::Paused => actions.push((item.id, 1)),
                                _ => actions.push((item.id, 2)),
                            }
                        }
                        if response.clicked() {
                            open_page = true;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        if ui.button("✕").clicked() {
                            close_shelf = true;
                        }
                    });
                });
            });
        for (id, action) in actions {
            match action {
                0 => app.shell.downloads().pause(id),
                1 => app.shell.downloads().resume(id),
                _ => app.shell.downloads().cancel(id),
            }
        }
        if open_page {
            app.navigate_url("rowser://downloads");
        }
        if close_shelf {
            app.downloads_shelf = false;
        }
    }

    // -----------------------------------------------------------------------
    // Devtools
    // -----------------------------------------------------------------------

    fn devtools(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let pal = app.theme.clone();
        if app.chrome.devtools_section.is_empty() {
            app.chrome.devtools_section = "console".into();
        }
        let section = app.chrome.devtools_section.clone();
        let mut do_clear = false;
        let mut do_close = false;
        let mut eval = false;
        TopBottomPanel::bottom("devtools")
            .default_height(280.0)
            .resizable(true)
            .frame(egui::Frame::NONE.fill(pal.elevated))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("DevTools").strong());
                    ui.separator();
                    ui.selectable_value(
                        &mut app.chrome.devtools_section,
                        "console".into(),
                        "Console",
                    );
                    ui.selectable_value(
                        &mut app.chrome.devtools_section,
                        "network".into(),
                        "Network",
                    );
                    let _ = &section;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Clear").clicked() {
                            do_clear = true;
                        }
                        if ui.button("✕").clicked() {
                            do_close = true;
                        }
                    });
                });
                ui.separator();
                if app.chrome.devtools_section == "network" {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if app.network_log.is_empty() {
                            ui.label(
                                egui::RichText::new(
                                    "Privacy-gated requests appear here. Nothing blocked yet.",
                                )
                                .color(pal.text_dim),
                            );
                        }
                        for (tab, url, reason) in app.network_log.iter().rev().take(120) {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("🛡 {reason}"))
                                        .small()
                                        .color(pal.danger),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        "[tab {tab}] {}",
                                        ellipsize(ui, url, 700.0)
                                    ))
                                    .small()
                                    .color(pal.text_dim),
                                );
                            });
                        }
                    });
                    return;
                }
                ui.horizontal(|ui| {
                    ui.label("Filter");
                    ui.add(
                        egui::TextEdit::singleline(&mut app.devtools_filter).desired_width(240.0),
                    );
                });
                ui.add_space(2.0);
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let filter = app.devtools_filter.to_lowercase();
                        let entries: Vec<_> = app
                            .shell
                            .console()
                            .iter()
                            .filter(|e| {
                                filter.is_empty()
                                    || e.text.to_lowercase().contains(&filter)
                                    || e.level.contains(&filter)
                            })
                            .collect();
                        if entries.is_empty() {
                            ui.label(
                                egui::RichText::new("Console is quiet. Evaluate JS below.")
                                    .color(pal.text_dim),
                            );
                        }
                        for entry in entries.iter().rev() {
                            let color = match entry.level.as_str() {
                                "error" => pal.danger,
                                "warn" => pal.warning,
                                "result" => pal.success,
                                "info" => pal.text,
                                _ => pal.text,
                            };
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("[{}]", entry.level))
                                        .monospace()
                                        .small()
                                        .color(color),
                                );
                                ui.label(
                                    egui::RichText::new(ellipsize(
                                        ui,
                                        &entry.text,
                                        ui.available_width() - 120.0,
                                    ))
                                    .monospace()
                                    .small()
                                    .color(pal.text),
                                );
                            });
                        }
                    });
                ui.separator();
                let enter_pressed = ui
                    .horizontal(|ui| {
                        ui.label(egui::RichText::new(">").monospace());
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut app.devtools_input)
                                .hint_text("evaluate JavaScript in the page (QuickJS-ng)")
                                .font(egui::FontId::monospace(13.0))
                                .desired_width(ui.available_width()),
                        );
                        response.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter))
                    })
                    .inner;
                if enter_pressed {
                    eval = true;
                }
            });
        if do_clear {
            app.shell.clear_console();
            app.network_log.clear();
        }
        if do_close {
            app.devtools_open = false;
        }
        if eval {
            if let Some(tab) = app.tabs.get(app.active) {
                let code = app.devtools_input.clone();
                if !code.trim().is_empty() {
                    app.shell.push_console(tab.id, "input", code.clone());
                    app.shell.browser().eval_js(tab.id, code);
                    app.devtools_input.clear();
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Find bar
    // -----------------------------------------------------------------------

    fn find_bar(&mut self, app: &mut BrowserApp, ctx: &Context) {
        if !app.find_open {
            return;
        }
        let pal = app.theme.clone();
        let anchor = Pos2::new(
            (app.content_rect.right() - 350.0).max(app.content_rect.left() + 10.0),
            app.content_rect.top() + 8.0,
        );
        let mut do_step = 0i32;
        let mut do_query = false;
        let mut do_close = false;
        egui::Area::new(Id::new("find-bar"))
            .order(Order::Foreground)
            .fixed_pos(anchor)
            .show(ctx, |ui| {
                let frame = egui::Frame::default()
                    .fill(pal.elevated)
                    .corner_radius(10)
                    .stroke(Stroke::new(1.0_f32, pal.border))
                    .inner_margin(egui::Margin::symmetric(8, 5))
                    .shadow(egui::Shadow {
                        offset: [0, 2],
                        blur: 12,
                        spread: 0,
                        color: Color32::from_black_alpha(40),
                    });
                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut app.find_query)
                                .hint_text("Find in page")
                                .desired_width(180.0)
                                .font(egui::FontId::proportional(13.5)),
                        );
                        if self.find_take_focus {
                            self.find_take_focus = false;
                            response.request_focus();
                        }
                        if response.changed() {
                            do_query = true;
                        }
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let shift = ui.input(|i| i.modifiers.shift);
                        if enter && response.has_focus() {
                            do_step = if shift { -1 } else { 1 };
                        }
                        let (total, active) = app.find_state;
                        let count = match (total, active) {
                            (0, _) if app.find_query.is_empty() => String::new(),
                            (0, _) => "0".to_owned(),
                            (total, Some(active)) => format!("{}/{}", active + 1, total),
                            (total, None) => total.to_string(),
                        };
                        ui.label(egui::RichText::new(count).small().color(pal.text_dim));
                        if ui.button("↑").clicked() {
                            do_step = -1;
                        }
                        if ui.button("↓").clicked() {
                            do_step = 1;
                        }
                        if ui.button("✕").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Escape))
                        {
                            do_close = true;
                        }
                    });
                });
            });
        if do_close {
            app.find_clear();
        }
        if let Some(tab) = app.tabs.get(app.active) {
            if do_query {
                let query = app.find_query.clone();
                app.shell.browser().find_in_page(tab.id, query);
            }
            if do_step != 0 {
                app.shell.browser().find_step(tab.id, do_step);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Omnibox suggestions
    // -----------------------------------------------------------------------

    fn suggestions(&mut self, app: &mut BrowserApp, ctx: &Context) {
        if !app.omnibox_focused {
            app.suggestions.clear();
            return;
        }
        {
            let history = &app.shell.store().history;
            let bookmarks = &app.shell.store().bookmarks;
            let engine = &app.shell.store().settings.search_engine;
            let top: Vec<(String, String)> = rowser_shell::top_sites(history, 6)
                .into_iter()
                .map(|t: TopSite| (t.label, t.url))
                .collect();
            app.suggestions =
                rowser_shell::suggest(&app.omnibox, engine, history, bookmarks, &top, 8);
        }
        if app.suggestions.is_empty() {
            return;
        }
        let anchor = Pos2::new(app.omnibox_rect.left(), app.omnibox_rect.bottom() + 4.0);
        let width = app.omnibox_rect.width();
        let pal = app.theme.clone();
        let suggestions = app.suggestions.clone();
        let mut pick: Option<String> = None;
        egui::Area::new(Id::new("suggestions"))
            .order(Order::Foreground)
            .fixed_pos(anchor)
            .show(ctx, |ui| {
                let frame = egui::Frame::default()
                    .fill(pal.elevated)
                    .corner_radius(12)
                    .stroke(Stroke::new(1.0_f32, pal.border))
                    .inner_margin(egui::Margin::symmetric(4, 4))
                    .shadow(egui::Shadow {
                        offset: [0, 3],
                        blur: 16,
                        spread: 0,
                        color: Color32::from_black_alpha(50),
                    });
                frame.show(ui, |ui| {
                    ui.set_width(width);
                    for (index, suggestion) in suggestions.iter().enumerate() {
                        let selected = index == app.sugg_index;
                        let (rect, response) =
                            ui.allocate_exact_size(Vec2::new(width - 8.0, 30.0), Sense::click());
                        let painter = ui.painter_at(rect);
                        let bg = if response.hovered() || selected {
                            pal.hover
                        } else {
                            Color32::TRANSPARENT
                        };
                        if bg != Color32::TRANSPARENT {
                            painter.rect_filled(rect, CornerRadius::same(8), bg);
                        }
                        if selected {
                            painter.rect_filled(
                                Rect::from_min_size(
                                    rect.left_top() + Vec2::new(0.0, 6.0),
                                    Vec2::new(3.0, 18.0),
                                ),
                                CornerRadius::same(2),
                                pal.accent,
                            );
                        }
                        let badge = match suggestion.kind {
                            SuggestionKind::Search => ("🔍", pal.text_dim),
                            SuggestionKind::Url => ("🌐", pal.text_dim),
                            SuggestionKind::History => ("🕘", pal.text_dim),
                            SuggestionKind::Bookmark => ("★", pal.warning),
                            SuggestionKind::TopSite => ("📌", pal.text_dim),
                        };
                        painter.text(
                            Pos2::new(rect.left() + 12.0, rect.center().y),
                            Align2::LEFT_CENTER,
                            badge.0,
                            egui::FontId::proportional(13.0),
                            badge.1,
                        );
                        painter.text(
                            Pos2::new(rect.left() + 34.0, rect.center().y),
                            Align2::LEFT_CENTER,
                            ellipsize(ui, &suggestion.title, width - 280.0),
                            egui::FontId::proportional(13.5),
                            pal.text,
                        );
                        painter.text(
                            Pos2::new(rect.right() - 12.0, rect.center().y),
                            Align2::RIGHT_CENTER,
                            ellipsize(ui, &suggestion.url, 210.0),
                            egui::FontId::proportional(11.5),
                            pal.text_dim,
                        );
                        if response.clicked() {
                            pick = Some(suggestion.url.clone());
                        }
                    }
                });
            });
        if let Some(url) = pick {
            app.navigate_url(url);
        }
        // Keyboard navigation.
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());
        for event in events {
            if let egui::Event::Key {
                key, pressed: true, ..
            } = event
            {
                match key {
                    egui::Key::ArrowDown if !app.suggestions.is_empty() => {
                        app.sugg_index = (app.sugg_index + 1) % app.suggestions.len();
                    }
                    egui::Key::ArrowUp if !app.suggestions.is_empty() => {
                        app.sugg_index =
                            (app.sugg_index + app.suggestions.len() - 1) % app.suggestions.len();
                    }
                    egui::Key::Escape if app.omnibox_focused => {
                        app.omnibox_focused = false;
                        app.suggestions.clear();
                        ctx.memory_mut(|m| m.surrender_focus(Id::new("omnibox-field")));
                    }
                    _ => {}
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Content
    // -----------------------------------------------------------------------

    fn content(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let pal = app.theme.clone();
        CentralPanel::default()
            .frame(egui::Frame::NONE.fill(pal.surface))
            .show(ctx, |ui| {
                let rect = ui.available_rect_before_wrap();
                app.content_rect = rect;

                // Snapshot refresh (cheap mutex reads).
                {
                    let ids: Vec<rowser_api::TabId> = app.tabs.iter().map(|t| t.id).collect();
                    for id in ids {
                        let Some(snapshot) = app.shell.browser().snapshot(id) else {
                            continue;
                        };
                        if let Some(t) = app.tabs.iter_mut().find(|t| t.id == id) {
                            t.title = snapshot.title.clone();
                            t.url = snapshot.url.clone();
                            t.loading = snapshot.loading;
                            t.can_back = snapshot.can_go_back;
                            t.can_fwd = snapshot.can_go_forward;
                            if snapshot.frame.is_none() && snapshot.loading {
                                t.texture = None;
                            }
                        }
                    }
                }

                let internal = app
                    .tabs
                    .get(app.active)
                    .map(|t| t.internal_page().is_some() || t.is_newtab())
                    .unwrap_or(true);
                if internal {
                    pages::render(app, ui);
                    return;
                }

                let texture = app.tabs.get(app.active).and_then(|t| t.texture.clone());
                let Some(texture) = texture else {
                    let loading = app.tabs.get(app.active).map(|t| t.loading).unwrap_or(false);
                    let url = app
                        .tabs
                        .get(app.active)
                        .map(|t| t.url.clone())
                        .unwrap_or_default();
                    pages::blank_or_error(app, ui, &url, loading);
                    return;
                };

                let zoom = app.tabs[app.active].zoom;
                let size = rect.size();
                let (_, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
                ui.put(
                    rect,
                    egui::Image::from_texture(&texture).fit_to_exact_size(size),
                );
                let scale = zoom;

                // Scroll wheel.
                let scroll = ui.input(|i| i.raw_scroll_delta);
                if response.hovered() && scroll.y.abs() > 0.0 {
                    let delta = -scroll.y * 2.4 * scale.max(1.0);
                    app.scroll_active(delta);
                }
                // Hover → hit-test (throttled).
                if let Some(pos) = response.hover_pos() {
                    if pos.distance(app.hit_test_at) > 6.0 {
                        app.hit_test_at = pos;
                        let doc_x = (pos.x - rect.left()) * scale;
                        let doc_y = (pos.y - rect.top()) * scale + app.tabs[app.active].scroll_y;
                        if let Some(id) = app.tabs.get(app.active).map(|t| t.id) {
                            app.shell.browser().hit_test(id, doc_x, doc_y);
                        }
                    }
                } else {
                    app.hover_link = None;
                }
                if app.hover_link.is_some() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                // Click → engine hit-tested click.
                if response.clicked() {
                    if let Some(pos) = response.interact_pointer_pos() {
                        let doc_x = (pos.x - rect.left()) * scale;
                        let doc_y = (pos.y - rect.top()) * scale + app.tabs[app.active].scroll_y;
                        if let Some(id) = app.tabs.get(app.active).map(|t| t.id) {
                            app.shell.browser().click_at(id, doc_x, doc_y);
                        }
                    }
                }
                // Middle click → open the hovered link in a new tab.
                if response.clicked_by(egui::PointerButton::Middle) {
                    if let Some(link) = app.hover_link.clone() {
                        app.new_tab(Some(link));
                    }
                }
            });
    }

    // -----------------------------------------------------------------------
    // Tab context menu
    // -----------------------------------------------------------------------

    fn tab_context_menu(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let Some((index, pos)) = app.ctx_menu else {
            return;
        };
        if index >= app.tabs.len() {
            app.ctx_menu = None;
            return;
        }
        let tab = app.tabs[index].clone();
        let pal = app.theme.clone();
        let mut actions: Vec<String> = Vec::new();
        egui::Window::new("tab-menu")
            .id(Id::new("tab-context-menu"))
            .fixed_pos(pos)
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .order(Order::Foreground)
            .frame(
                egui::Frame::default()
                    .fill(pal.elevated)
                    .corner_radius(10)
                    .inner_margin(egui::Margin::symmetric(6, 5))
                    .stroke(Stroke::new(1.0_f32, pal.border))
                    .shadow(egui::Shadow {
                        offset: [0, 3],
                        blur: 14,
                        spread: 0,
                        color: Color32::from_black_alpha(45),
                    }),
            )
            .show(ctx, |ui| {
                ui.set_min_width(215.0);
                let item = |ui: &mut Ui, label: &str, action: &str, actions: &mut Vec<String>| {
                    if ui.button(label).clicked() {
                        actions.push(action.to_owned());
                    }
                };
                item(ui, "New tab to the right", "new-tab-right", &mut actions);
                item(ui, "Reload", "reload", &mut actions);
                item(ui, "Duplicate", "duplicate", &mut actions);
                ui.separator();
                item(
                    ui,
                    if tab.pinned { "Unpin tab" } else { "Pin tab" },
                    "pin",
                    &mut actions,
                );
                item(
                    ui,
                    if tab.muted {
                        "Unmute site"
                    } else {
                        "Mute site"
                    },
                    "mute",
                    &mut actions,
                );
                item(ui, "Add to new group", "group-new", &mut actions);
                if tab.group.is_some() {
                    item(ui, "Remove from group", "group-remove", &mut actions);
                }
                ui.separator();
                if ui
                    .add_enabled(
                        !app.shell.closed_tabs().is_empty(),
                        egui::Button::new("Reopen closed tab"),
                    )
                    .clicked()
                {
                    actions.push("reopen".into());
                }
                item(ui, "Close", "close", &mut actions);
                item(ui, "Close other tabs", "close-others", &mut actions);
                item(ui, "Close tabs to the right", "close-right", &mut actions);
            });
        for action in actions {
            match action.as_str() {
                "new-tab-right" => app.new_tab(None),
                "reload" => app.shell.browser().reload(tab.id),
                "duplicate" => app.new_tab(Some(tab.url.clone())),
                "pin" => {
                    if let Some(t) = app.tabs.iter_mut().find(|t| t.id == tab.id) {
                        t.pinned = !t.pinned;
                    }
                }
                "mute" => {
                    if let Some(t) = app.tabs.iter_mut().find(|t| t.id == tab.id) {
                        t.muted = !t.muted;
                    }
                }
                "group-new" => {
                    let group = app.tabs.iter().filter_map(|t| t.group.as_ref()).count() + 1;
                    if let Some(t) = app.tabs.iter_mut().find(|t| t.id == tab.id) {
                        t.group = Some(crate::app::TabGroup {
                            name: format!("Group {group}"),
                            color: group_color(group as u32),
                        });
                    }
                }
                "group-remove" => {
                    if let Some(t) = app.tabs.iter_mut().find(|t| t.id == tab.id) {
                        t.group = None;
                    }
                }
                "reopen" => {
                    if let Some(id) = app.shell.reopen_closed_tab() {
                        app.tabs.push(TabUi::new(
                            id,
                            app.shell
                                .browser()
                                .snapshot(id)
                                .map(|s| s.url.clone())
                                .unwrap_or_default(),
                            String::new(),
                        ));
                        app.set_active(app.tabs.len() - 1);
                    }
                }
                "close" => app.close_tab(index),
                "close-others" => {
                    let keep = tab.id;
                    let ids: Vec<rowser_api::TabId> = app
                        .tabs
                        .iter()
                        .map(|t| t.id)
                        .filter(|&i| i != keep)
                        .collect();
                    for i in ids {
                        app.shell.browser().close_tab(i);
                    }
                    app.tabs.retain(|t| t.id == keep);
                    app.set_active(0);
                }
                "close-right" => {
                    let ids: Vec<rowser_api::TabId> =
                        app.tabs[index + 1..].iter().map(|t| t.id).collect();
                    for i in ids {
                        app.shell.browser().close_tab(i);
                    }
                    app.tabs.truncate(index + 1);
                    app.active = app.active.min(index);
                }
                _ => {}
            }
            app.ctx_menu = None;
        }
        // Dismiss when clicking outside the window.
        if ctx.input(|i| i.pointer.any_click()) {
            if let Some(pos) = ctx.pointer_hover_pos() {
                let inside = ctx
                    .memory(|m| m.area_rect(Id::new("tab-context-menu")))
                    .map(|r| r.contains(pos))
                    .unwrap_or(false);
                if !inside {
                    app.ctx_menu = None;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Dialogs
    // -----------------------------------------------------------------------

    fn dialogs(&mut self, app: &mut BrowserApp, ctx: &Context) {
        self.print_dialog(app, ctx);
        self.file_dialog(app, ctx);
        self.bookmark_dialog(app, ctx);
    }

    fn print_dialog(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let Some(dialog) = app.print.clone() else {
            return;
        };
        let pal = app.theme.clone();
        let mut close = false;
        let mut start = false;
        let mut path_edit = dialog.path.clone();
        // Modal Escape closes the print dialog (matches Chrome/Firefox
        // print preview behavior and unblocks keyboard shortcuts).
        close |= ctx.input(|i| i.key_pressed(egui::Key::Escape));
        egui::Window::new("Print")
            .resizable(false)
            .collapsible(false)
            .order(Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(window_frame(&pal))
            .show(ctx, |ui| {
                ui.set_min_width(470.0);
                ui.heading("Print");
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Destination");
                    ui.strong("Save as PDF (A4)");
                });
                ui.horizontal(|ui| {
                    ui.label("Save to");
                    ui.add(egui::TextEdit::singleline(&mut path_edit).desired_width(300.0));
                    if ui.button("Browse…").clicked() {
                        let dir = std::path::Path::new(&dialog.path)
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| app.shell.store().settings.download_dir.clone());
                        let mut picker = crate::app::FileDialog {
                            purpose: FilePurpose::SavePdf,
                            dir,
                            filename: std::path::Path::new(&dialog.path)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "page.pdf".into()),
                            entries: Vec::new(),
                            error: None,
                        };
                        picker.refresh();
                        app.file = Some(picker);
                    }
                });
                if let Some(image) = &dialog.preview {
                    let scale = 320.0 / image.size[0].max(1) as f32;
                    let size = Vec2::new(320.0, image.size[1] as f32 * scale);
                    let texture = ui.ctx().load_texture(
                        "print-preview",
                        image.clone(),
                        egui::TextureOptions::default(),
                    );
                    ui.add(egui::Image::from_texture(&texture).fit_to_exact_size(size));
                    if let Some(pages) = dialog.pages {
                        ui.label(
                            egui::RichText::new(format!("{pages} page(s)"))
                                .small()
                                .color(pal.text_dim),
                        );
                    }
                } else if dialog.busy {
                    ui.spinner();
                    ui.label("Rendering preview…");
                }
                if let Some(error) = &dialog.error {
                    ui.label(egui::RichText::new(error).color(pal.danger));
                }
                ui.separator();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!dialog.busy, egui::Button::new("Save PDF"))
                        .clicked()
                    {
                        start = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if path_edit != dialog.path {
            if let Some(d) = app.print.as_mut() {
                d.path = path_edit;
            }
        }
        if start {
            if let Some(dialog) = app.print.as_mut() {
                dialog.busy = true;
                let browser = app.shell.browser().clone();
                let tab = dialog.tab;
                let path = dialog.path.clone();
                let restore = dialog.restore;
                let tx = app.print_tx.clone();
                let waker = std::sync::Arc::clone(&app.waker);
                std::thread::Builder::new()
                    .name("rowser-print".into())
                    .spawn(move || {
                        let outcome = match rowser_shell::print_to_pdf(
                            &browser,
                            tab,
                            std::path::Path::new(&path),
                            restore,
                        ) {
                            Ok(pages) => crate::app::PrintOutcome::Done { tab, path, pages },
                            Err(err) => crate::app::PrintOutcome::Failed {
                                tab,
                                error: format!("{err:#}"),
                            },
                        };
                        let _ = tx.send(outcome);
                        waker.wake();
                    })
                    .ok();
            }
        }
        if close {
            app.print = None;
        }
    }

    fn file_dialog(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let Some(dialog) = app.file.clone() else {
            return;
        };
        let pal = app.theme.clone();
        let purpose = dialog.purpose;
        let title = match purpose {
            FilePurpose::SavePage => "Save page as…",
            FilePurpose::SavePdf => "Save PDF as…",
            FilePurpose::ImportBookmarks => "Import bookmarks…",
            FilePurpose::ExportBookmarks => "Export bookmarks to…",
            FilePurpose::DownloadsDir => "Choose downloads folder…",
        };
        let is_dir_pick = purpose == FilePurpose::DownloadsDir;
        let mut close = false;
        let mut action: Option<String> = None;
        let mut filename_edit = dialog.filename.clone();
        egui::Window::new(title)
            .resizable(false)
            .collapsible(false)
            .order(Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(window_frame(&pal))
            .show(ctx, |ui| {
                ui.set_min_width(540.0);
                ui.horizontal(|ui| {
                    if ui.button("⬆ Up").clicked() {
                        action = Some("up".into());
                    }
                    ui.label(
                        egui::RichText::new(ellipsize(
                            ui,
                            &dialog.dir.display().to_string(),
                            420.0,
                        ))
                        .monospace()
                        .small()
                        .color(pal.text_dim),
                    );
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .show(ui, |ui| {
                        for (name, is_dir) in dialog.entries.iter() {
                            let label = if *is_dir {
                                format!("📁 {name}")
                            } else {
                                name.clone()
                            };
                            if ui.button(label).clicked() {
                                if *is_dir {
                                    action = Some(format!("enter:{name}"));
                                } else if !is_dir_pick {
                                    action = Some(format!("pick:{name}"));
                                }
                            }
                        }
                    });
                if let Some(error) = &dialog.error {
                    ui.label(egui::RichText::new(error).color(pal.danger));
                }
                ui.separator();
                if !is_dir_pick {
                    ui.horizontal(|ui| {
                        ui.label("File name");
                        ui.add(egui::TextEdit::singleline(&mut filename_edit).desired_width(380.0));
                    });
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let confirm_label = match purpose {
                        FilePurpose::ImportBookmarks => "Import",
                        FilePurpose::DownloadsDir => "Choose",
                        _ => "Save",
                    };
                    if ui.button(confirm_label).clicked() {
                        action = Some("confirm".into());
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if filename_edit != dialog.filename {
            if let Some(d) = app.file.as_mut() {
                d.filename = filename_edit;
            }
        }
        if let Some(action) = action {
            if let Some(dir) = action.strip_prefix("enter:") {
                if let Some(d) = app.file.as_mut() {
                    d.enter(dir);
                }
            } else if let Some(name) = action.strip_prefix("pick:") {
                if let Some(d) = app.file.as_mut() {
                    d.filename = name.to_owned();
                }
            } else {
                match action.as_str() {
                    "up" => {
                        if let Some(d) = app.file.as_mut() {
                            d.up();
                        }
                    }
                    "confirm" => {
                        close = app.confirm_file_dialog();
                    }
                    _ => {}
                }
            }
        }
        if close {
            app.file = None;
        }
    }

    fn bookmark_dialog(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let Some(dialog) = app.bookmark_edit.clone() else {
            return;
        };
        let pal = app.theme.clone();
        let mut close = false;
        let mut save = false;
        let mut delete = false;
        let mut title_edit = dialog.title.clone();
        let mut url_edit = dialog.url.clone();
        let mut folder_edit = dialog.folder.clone();
        // Modal Escape closes the bookmark editor without saving.
        close |= ctx.input(|i| i.key_pressed(egui::Key::Escape));
        egui::Window::new("Edit bookmark")
            .resizable(false)
            .collapsible(false)
            .order(Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(window_frame(&pal))
            .show(ctx, |ui| {
                ui.set_min_width(430.0);
                ui.horizontal(|ui| {
                    ui.label("Title");
                    ui.add(egui::TextEdit::singleline(&mut title_edit).desired_width(330.0));
                });
                ui.horizontal(|ui| {
                    ui.label("URL");
                    ui.add(egui::TextEdit::singleline(&mut url_edit).desired_width(330.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Folder");
                    ui.add(
                        egui::TextEdit::singleline(&mut folder_edit)
                            .hint_text("Bookmarks bar")
                            .desired_width(330.0),
                    );
                });
                ui.separator();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if dialog.id != 0 && ui.button("Delete").clicked() {
                        delete = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        {
            let d = app.bookmark_edit.as_mut().unwrap();
            d.title = title_edit;
            d.url = url_edit;
            d.folder = folder_edit;
        }
        if save {
            app.save_bookmark_edit();
            close = true;
        }
        if delete {
            app.delete_bookmark_edit();
            close = true;
        }
        if close {
            app.bookmark_edit = None;
        }
    }

    // -----------------------------------------------------------------------
    // Toasts
    // -----------------------------------------------------------------------

    fn toasts(&mut self, app: &mut BrowserApp, ctx: &Context) {
        let toasts: Vec<(String, f32)> =
            app.toasts.iter().map(|t| (t.text.clone(), t.ttl)).collect();
        if toasts.is_empty() {
            return;
        }
        let pal = app.theme.clone();
        let screen = ctx.screen_rect();
        let anchor = Pos2::new(
            screen.right() - 310.0,
            screen.bottom() - 44.0 - (toasts.len() as f32) * 42.0,
        );
        egui::Area::new(Id::new("toasts"))
            .order(Order::Foreground)
            .fixed_pos(anchor)
            .show(ctx, |ui| {
                for (text, _) in toasts.iter().rev() {
                    let frame = egui::Frame::default()
                        .fill(pal.elevated)
                        .corner_radius(8)
                        .stroke(Stroke::new(1.0_f32, pal.border))
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .shadow(egui::Shadow {
                            offset: [0, 2],
                            blur: 10,
                            spread: 0,
                            color: Color32::from_black_alpha(40),
                        });
                    frame.show(ui, |ui| {
                        ui.set_width(292.0);
                        ui.horizontal(|ui| {
                            let icon_rect = Rect::from_center_size(
                                Pos2::new(ui.max_rect().left() + 14.0, ui.max_rect().center().y),
                                Vec2::splat(14.0),
                            );
                            let painter = ui.painter();
                            Icon::Check.paint(painter, icon_rect, pal.success);
                            ui.label(egui::RichText::new(ellipsize(ui, text, 240.0)).small());
                        });
                    });
                }
            });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Truncates text with an ellipsis to fit `max_width` (public helper for
/// the internal pages).
pub fn ellipsize_public(ui: &Ui, text: &str, max_width: f32) -> String {
    ellipsize(ui, text, max_width)
}

/// The dialog window frame (public helper for the internal pages).
pub fn window_frame_public(pal: &Theme) -> egui::Frame {
    window_frame(pal)
}

fn arc_points_helper(
    center: Pos2,
    radius: f32,
    start: f32,
    end: f32,
    segments: usize,
) -> Vec<Pos2> {
    let mut pts = Vec::with_capacity(segments + 1);
    for i in 0..=segments {
        let t = i as f32 / segments as f32;
        let a = start + (end - start) * t;
        pts.push(center + Vec2::new(a.cos() * radius, a.sin() * radius));
    }
    pts
}

fn badge(painter: &egui::Painter, rect: Rect, text: String, pal: &Theme) {
    let badge_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 5.0, rect.top() + 3.0),
        Vec2::new(26.0, 13.0),
    );
    painter.rect_filled(badge_rect, CornerRadius::same(6), pal.accent);
    painter.text(
        badge_rect.center(),
        Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(9.0),
        pal.on_accent,
    );
}

/// Lightens a color by `amount` (0..1).
fn lighten(color: Color32, amount: f32) -> Color32 {
    if amount == 0.0 {
        return color;
    }
    let r = color.r() as f32 + (255.0 - color.r() as f32) * amount;
    let g = color.g() as f32 + (255.0 - color.g() as f32) * amount;
    let b = color.b() as f32 + (255.0 - color.b() as f32) * amount;
    Color32::from_rgb(r as u8, g as u8, b as u8)
}

/// Truncates text with an ellipsis to fit `max_width`.
fn ellipsize(ui: &Ui, text: &str, max_width: f32) -> String {
    if max_width <= 0.0 || text.is_empty() {
        return text.chars().take(80).collect();
    }
    let font = egui::FontId::proportional(13.0);
    let mut out: String = text.chars().take(200).collect();
    loop {
        let width = ui
            .painter()
            .layout_no_wrap(out.clone(), font.clone(), Color32::WHITE)
            .size()
            .x;
        if width <= max_width {
            return out;
        }
        // Drop the ellipsis, one char, then re-append.
        let base: String = out.strip_suffix('…').unwrap_or(&out).to_owned();
        let mut shortened = base;
        shortened.pop();
        if shortened.is_empty() {
            return "…".to_owned();
        }
        shortened.push('…');
        out = shortened;
    }
}

fn compact_count(n: u64) -> String {
    if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

fn group_color(n: u32) -> Color32 {
    const COLORS: [Color32; 6] = [
        Color32::from_rgb(0x1A, 0x73, 0xE8),
        Color32::from_rgb(0xE8, 0x71, 0x0A),
        Color32::from_rgb(0x18, 0x8D, 0x18),
        Color32::from_rgb(0xD9, 0x30, 0x25),
        Color32::from_rgb(0x9C, 0x27, 0xB0),
        Color32::from_rgb(0x00, 0x89, 0x7B),
    ];
    COLORS[(n as usize) % COLORS.len()]
}

fn window_frame(pal: &Theme) -> egui::Frame {
    egui::Frame::default()
        .fill(pal.elevated)
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .stroke(Stroke::new(1.0_f32, pal.border))
        .shadow(egui::Shadow {
            offset: [0, 4],
            blur: 20,
            spread: 0,
            color: Color32::from_black_alpha(60),
        })
}
