//! The browser application: state, the frame loop, event handling, input
//! routing and keyboard shortcuts.

use std::path::PathBuf;
use std::sync::mpsc::Receiver as StdReceiver;
use std::time::Duration;

use egui::{Context, Pos2, Rect};
use rowser_api::{EngineEvent, TabId};
use rowser_shell::{resolve_input, Suggestion, ThemeMode, Waker};

use crate::chrome::Chrome;
use crate::pages::InternalPage;
use crate::theme::Theme;

/// Per-tab UI state (display order lives here, never in the engine).
#[derive(Clone)]
pub struct TabUi {
    /// Engine tab id.
    pub id: TabId,
    /// Pinned tabs stay left at fixed width.
    pub pinned: bool,
    /// Muted (engine has no media pipeline in v1; the flag is stored and
    /// consumed when audio lands).
    pub muted: bool,
    /// Tab group, when assigned.
    pub group: Option<TabGroup>,
    /// Page zoom factor.
    pub zoom: f32,
    /// Scroll offset the UI believes the tab is at.
    pub scroll_y: f32,
    /// Cached title.
    pub title: String,
    /// Cached URL (display).
    pub url: String,
    /// Loading spinner state.
    pub loading: bool,
    /// Load progress 0..1.
    pub progress: f32,
    /// History availability.
    pub can_back: bool,
    /// Forward availability.
    pub can_fwd: bool,
    /// Suspended (frozen) — dim the tab.
    pub suspended: bool,
    /// The page texture (engine frames blitted here).
    pub texture: Option<egui::TextureHandle>,
    /// Engine frame id backing the texture.
    pub frame_id: u64,
    /// CSS-pixel viewport the UI last set.
    pub viewport: (f32, f32),
    /// Blocked request count for this tab.
    pub blocked: u64,
}

impl TabUi {
    pub fn new(id: TabId, url: String, title: String) -> TabUi {
        TabUi {
            id,
            pinned: false,
            muted: false,
            group: None,
            zoom: 1.0,
            scroll_y: 0.0,
            title,
            url,
            loading: false,
            progress: 0.0,
            can_back: false,
            can_fwd: false,
            suspended: false,
            texture: None,
            frame_id: 0,
            viewport: (0.0, 0.0),
            blocked: 0,
        }
    }

    /// First letter for the favicon avatar.
    pub fn letter(&self) -> char {
        let source: &str = if self.title.is_empty() {
            host_of(&self.url)
        } else {
            &self.title
        };
        source
            .chars()
            .find(|c| c.is_alphanumeric())
            .unwrap_or('?')
            .to_ascii_uppercase()
    }

    /// The internal page this tab shows, if any.
    pub fn internal_page(&self) -> Option<InternalPage> {
        InternalPage::from_url(&self.url)
    }

    /// True when the tab shows the new-tab page.
    pub fn is_newtab(&self) -> bool {
        self.url.is_empty() || self.url == "rowser://newtab"
    }
}

/// A tab group.
#[derive(Debug, Clone)]
pub struct TabGroup {
    /// Group label.
    pub name: String,
    /// Group color.
    pub color: egui::Color32,
}

/// The print dialog state.
#[derive(Clone)]
pub struct PrintDialog {
    /// Tab being printed.
    pub tab: TabId,
    /// Destination path.
    pub path: String,
    /// True while rendering/writing.
    pub busy: bool,
    /// Error text.
    pub error: Option<String>,
    /// First-page preview (rendered in the background).
    pub preview: Option<egui::ColorImage>,
    /// Page count, when the preview is ready.
    pub pages: Option<usize>,
    /// Restores the viewport after print.
    pub restore: (f32, f32),
}

/// In-app file picker.
#[derive(Clone)]
pub struct FileDialog {
    /// What the dialog is for.
    pub purpose: FilePurpose,
    /// Current directory.
    pub dir: PathBuf,
    /// Filename being typed.
    pub filename: String,
    /// Directory listing (name, is_dir), sorted.
    pub entries: Vec<(String, bool)>,
    /// Error text.
    pub error: Option<String>,
}

/// File dialog purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePurpose {
    /// Saving a page as HTML.
    SavePage,
    /// Saving a PDF.
    SavePdf,
    /// Importing bookmarks HTML.
    ImportBookmarks,
    /// Exporting bookmarks HTML.
    ExportBookmarks,
    /// Choosing the downloads directory.
    DownloadsDir,
}

impl FileDialog {
    /// Refreshes the listing.
    pub fn refresh(&mut self) {
        self.entries.clear();
        self.error = None;
        match std::fs::read_dir(&self.dir) {
            Ok(iter) => {
                for entry in iter.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    if name.starts_with('.') {
                        continue;
                    }
                    self.entries.push((name, is_dir));
                }
                self.entries.sort_by(|a, b| {
                    b.1.cmp(&a.1).then(a.0.to_lowercase().cmp(&b.0.to_lowercase()))
                });
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    /// Navigates up one directory.
    pub fn up(&mut self) {
        if let Some(parent) = self.dir.parent() {
            self.dir = parent.to_path_buf();
            self.refresh();
        }
    }

    /// Enters a directory.
    pub fn enter(&mut self, name: &str) {
        let next = self.dir.join(name);
        if next.is_dir() {
            self.dir = next;
            self.refresh();
        }
    }

    /// The full selected path.
    pub fn full_path(&self) -> PathBuf {
        if self.filename.is_empty() {
            self.dir.clone()
        } else {
            self.dir.join(&self.filename)
        }
    }
}

/// Bookmark editor dialog.
#[derive(Clone)]
pub struct BookmarkEdit {
    /// Bookmark id (0 = creating).
    pub id: u64,
    /// URL.
    pub url: String,
    /// Title.
    pub title: String,
    /// Folder.
    pub folder: String,
}

/// A toast notification.
pub struct Toast {
    /// Text.
    pub text: String,
    /// Seconds remaining.
    pub ttl: f32,
}

/// The browser app.
pub struct BrowserApp {
    /// The integration shell (engine + state).
    pub shell: rowser_shell::Shell,
    /// The active theme.
    pub theme: Theme,
    /// Cached theme mode (to detect changes).
    pub theme_mode: ThemeMode,
    /// Cached accent (to detect changes).
    pub accent_hex: String,
    /// Tabs in display order.
    pub tabs: Vec<TabUi>,
    /// Index of the active tab.
    pub active: usize,
    /// Omnibox text while editing.
    pub omnibox: String,
    /// Whether the omnibox is editing (suggestions live).
    pub omnibox_focused: bool,
    /// Current suggestions.
    pub suggestions: Vec<Suggestion>,
    /// Highlighted suggestion index.
    pub sugg_index: usize,
    /// The omnibox rect (for the dropdown anchor).
    pub omnibox_rect: Rect,
    /// The toolbar rect (for the find-bar anchor).
    pub toolbar_rect: Rect,
    /// Find bar open.
    pub find_open: bool,
    /// Find query.
    pub find_query: String,
    /// (total matches, active index) from the engine.
    pub find_state: (usize, Option<usize>),
    /// Devtools panel open.
    pub devtools_open: bool,
    /// Devtools JS eval input.
    pub devtools_input: String,
    /// Devtools console filter.
    pub devtools_filter: String,
    /// Privacy-gated request log (for devtools).
    pub network_log: Vec<(TabId, String, String)>,
    /// Downloads shelf visible.
    pub downloads_shelf: bool,
    /// Link under the pointer (status bar).
    pub hover_link: Option<String>,
    /// Pointer position of the last hit-test.
    pub hit_test_at: Pos2,
    /// Toasts.
    pub toasts: Vec<Toast>,
    /// Print dialog, when open.
    pub print: Option<PrintDialog>,
    /// File dialog, when open.
    pub file: Option<FileDialog>,
    /// Bookmark editor, when open.
    pub bookmark_edit: Option<BookmarkEdit>,
    /// Tab context menu (tab index + screen pos).
    pub ctx_menu: Option<(usize, Pos2)>,
    /// Tab being dragged (index).
    pub drag: Option<usize>,
    /// Content rect of the last frame.
    pub content_rect: Rect,
    /// Print worker results.
    print_rx: StdReceiver<PrintOutcome>,
    /// Print worker sender (moved into worker threads).
    pub print_tx: std::sync::mpsc::Sender<PrintOutcome>,
    /// Cross-thread waker for app-owned background work.
    pub waker: std::sync::Arc<Waker>,
    /// Chrome painting state (intermediate layout info).
    pub chrome: Chrome,
    /// Scratch: bookmark removed this frame.
    removed_bookmark: bool,
    /// Scratch: bookmark added this frame.
    added_bookmark: bool,
}

/// Background print outcomes (worker threads → UI).
pub enum PrintOutcome {
    /// The preview rendered.
    Preview {
        /// Tab id.
        tab: TabId,
        /// Page count.
        pages: usize,
        /// First-page pixels, when available.
        image: Option<egui::ColorImage>,
    },
    /// The PDF was written.
    Done {
        /// Tab id.
        tab: TabId,
        /// Destination path.
        path: String,
        /// Pages written.
        pages: usize,
    },
    /// Printing failed.
    Failed {
        /// Tab id.
        tab: TabId,
        /// Error text.
        error: String,
    },
}

impl BrowserApp {
    /// Builds the app on top of a booted shell.
    pub fn new(shell: rowser_shell::Shell, cc: &eframe::CreationContext<'_>) -> BrowserApp {
        let settings = shell.store().settings.clone();
        let theme = Theme::new(settings.theme, &settings.accent);
        theme.apply(&cc.egui_ctx);
        install_cjk_font(&cc.egui_ctx);

        // Initial tabs: engine-side tabs (session restore) in id order.
        let mut tabs: Vec<TabUi> = Vec::new();
        let mut ids = shell.browser().tabs();
        ids.sort_unstable();
        for id in ids {
            let snapshot = shell.browser().snapshot(id);
            tabs.push(TabUi::new(
                id,
                snapshot.as_ref().map(|s| s.url.clone()).unwrap_or_default(),
                snapshot.as_ref().map(|s| s.title.clone()).unwrap_or_default(),
            ));
        }

        let (print_tx, print_rx) = std::sync::mpsc::channel::<PrintOutcome>();
        let waker = std::sync::Arc::new(Waker::default());
        {
            let ctx = cc.egui_ctx.clone();
            let waker = std::sync::Arc::clone(&waker);
            waker.set(Box::new(move || ctx.request_repaint()));
        }
        let app = BrowserApp {
            shell,
            theme,
            theme_mode: settings.theme,
            accent_hex: settings.accent.clone(),
            tabs,
            active: 0,
            omnibox: String::new(),
            omnibox_focused: false,
            suggestions: Vec::new(),
            sugg_index: 0,
            omnibox_rect: Rect::ZERO,
            toolbar_rect: Rect::ZERO,
            find_open: false,
            find_query: String::new(),
            find_state: (0, None),
            devtools_open: false,
            devtools_input: String::new(),
            devtools_filter: String::new(),
            network_log: Vec::new(),
            downloads_shelf: false,
            hover_link: None,
            hit_test_at: Pos2::ZERO,
            toasts: Vec::new(),
            print: None,
            file: None,
            bookmark_edit: None,
            ctx_menu: None,
            drag: None,
            content_rect: Rect::ZERO,
            print_rx,
            print_tx,
            waker,
            chrome: Chrome::default(),
            removed_bookmark: false,
            added_bookmark: false,
        };
        app
    }

    /// The active tab id.
    pub fn active_id(&self) -> Option<TabId> {
        self.tabs.get(self.active).map(|t| t.id)
    }

    /// Index of a tab id.
    pub fn tab_index(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// The theme palette (shortcut).
    pub fn pal(&self) -> &Theme {
        &self.theme
    }

    /// Pushes a toast.
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toasts.push(Toast {
            text: text.into(),
            ttl: 4.5,
        });
    }

    /// Navigates the active tab from omnibox-style input.
    pub fn navigate_input(&mut self, input: &str) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let url = resolve_input(input, &self.shell.store().settings.search_engine);
        self.shell.browser().navigate(tab.id, url);
        self.omnibox_focused = false;
        self.suggestions.clear();
    }

    /// Opens a URL in the active tab.
    pub fn navigate_url(&mut self, url: impl Into<String>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        self.shell.browser().navigate(tab.id, url.into());
        self.omnibox_focused = false;
        self.suggestions.clear();
    }

    /// Creates a new tab (optionally navigating).
    pub fn new_tab(&mut self, url: Option<String>) {
        let display_url = url.clone().unwrap_or_default();
        let id = self.shell.browser().new_tab(url);
        // The engine emits TabCreated; reconcile next poll. Focus it now.
        let index = self.tabs.len();
        self.tabs.push(TabUi::new(id, display_url, String::new()));
        self.set_active(index);
    }

    /// Closes a tab by index (UI + engine).
    pub fn close_tab(&mut self, index: usize) {
        if self.tabs.len() <= 1 {
            // Last tab: replace with a fresh NTP instead of quitting.
            let id = self.tabs[0].id;
            self.shell.browser().navigate(id, "rowser://newtab");
            self.tabs[0].url = "rowser://newtab".into();
            self.tabs[0].title.clear();
            return;
        }
        let tab = self.tabs.remove(index);
        self.shell.browser().close_tab(tab.id);
        self.active = self.active.min(self.tabs.len() - 1);
        self.set_active(self.active);
    }

    /// Focuses a tab by index.
    pub fn set_active(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        let id = self.tabs[index].id;
        self.shell.browser().focus(id);
        self.shell.set_active_tab(Some(id));
        self.omnibox_focused = false;
        self.suggestions.clear();
        self.omnibox = self.tabs[index].url.clone();
    }

    /// Reloads the active tab.
    pub fn reload(&self) {
        if let Some(tab) = self.tabs.get(self.active) {
            self.shell.browser().reload(tab.id);
        }
    }

    /// Applies theme changes from settings.
    fn sync_theme(&mut self, ctx: &Context) {
        let settings = &self.shell.store().settings;
        if settings.theme != self.theme_mode || settings.accent != self.accent_hex {
            self.theme_mode = settings.theme;
            self.accent_hex = settings.accent.clone();
            self.theme = Theme::new(settings.theme, &settings.accent);
            self.theme.apply(ctx);
        }
    }

    /// Handles engine events (frame textures, titles, find, console...).
    fn handle_events(&mut self, ctx: &Context) {
        let events = self.shell.poll_events();
        for event in events {
            match event {
                EngineEvent::TabCreated(id) => {
                    if self.tab_index(id).is_none() {
                        let snapshot = self.shell.browser().snapshot(id);
                        self.tabs.push(TabUi::new(
                            id,
                            snapshot
                                .as_ref()
                                .map(|s| s.url.clone())
                                .unwrap_or_default(),
                            snapshot
                                .as_ref()
                                .map(|s| s.title.clone())
                                .unwrap_or_default(),
                        ));
                    }
                }
                EngineEvent::TabClosed(id) => {
                    if let Some(index) = self.tab_index(id) {
                        self.tabs.remove(index);
                        if self.tabs.is_empty() {
                            self.new_tab(None);
                        } else {
                            self.active = self.active.min(self.tabs.len() - 1);
                        }
                    }
                }
                EngineEvent::PageLoaded { tab, url, title } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        t.title = title;
                        t.url = url;
                        t.loading = false;
                        t.progress = 1.0;
                        t.suspended = false;
                        refresh_frame(ctx, self, tab);
                    }
                    if Some(tab) == self.active_id() && !self.omnibox_focused {
                        self.omnibox = self.tabs[self.active].url.clone();
                    }
                }
                EngineEvent::LoadProgress { tab, progress } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        t.loading = progress < 0.999;
                        t.progress = progress;
                    }
                }
                EngineEvent::TitleChanged { tab, title } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        t.title = title;
                    }
                }
                EngineEvent::FrameReady { tab, .. } => {
                    refresh_frame(ctx, self, tab);
                }
                EngineEvent::NavigationStarted { tab, url } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        t.url = url.clone();
                        t.loading = true;
                        t.progress = 0.05;
                        t.texture = None;
                        t.frame_id = 0;
                        t.scroll_y = 0.0;
                    }
                    if Some(tab) == self.active_id() && !self.omnibox_focused {
                        self.omnibox = url;
                    }
                }
                EngineEvent::TabSuspended(tab) => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        t.suspended = true;
                        t.texture = None;
                    }
                }
                EngineEvent::TabResumed(tab) => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        t.suspended = false;
                        t.loading = true;
                    }
                }
                EngineEvent::BlockedRequest { tab, url, reason } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        t.blocked += 1;
                    }
                    self.network_log.push((tab, url, reason));
                    if self.network_log.len() > 200 {
                        let overflow = self.network_log.len() - 200;
                        self.network_log.drain(0..overflow);
                    }
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        t.blocked = self
                            .shell
                            .stats()
                            .per_tab
                            .get(&tab)
                            .copied()
                            .unwrap_or(t.blocked);
                    }
                }
                EngineEvent::FindResult {
                    tab,
                    matches,
                    active,
                } => {
                    if Some(tab) == self.active_id() {
                        self.find_state = (matches, active);
                    }
                }
                EngineEvent::JsResult { tab, ok, result } => {
                    let level = if ok { "result" } else { "error" };
                    self.shell.push_console(tab, level, result);
                }
                EngineEvent::ConsoleMessage { tab, level, text } => {
                    self.shell.push_console(tab, &level, text);
                }
                EngineEvent::HitTestResult { tab, href, .. } => {
                    if Some(tab) == self.active_id() {
                        self.hover_link = href;
                    }
                }
                EngineEvent::PageSaved { tab: _, path } => {
                    self.toast(format!("Page saved to {path}"));
                }
                EngineEvent::MemoryPressure { .. } => {}
            }
        }
    }

    /// Polls print worker results into the dialog.
    fn poll_print(&mut self) {
        while let Ok(outcome) = self.print_rx.try_recv() {
            match outcome {
                PrintOutcome::Preview {
                    tab,
                    pages,
                    image,
                } => {
                    if let Some(dialog) = &mut self.print {
                        if dialog.tab == tab {
                            dialog.pages = Some(pages);
                            dialog.preview = image;
                            dialog.busy = false;
                        }
                    }
                }
                PrintOutcome::Done { tab, path, pages } => {
                    if self.print.as_ref().map(|d| d.tab) == Some(tab) {
                        self.print = None;
                    }
                    self.toast(format!("Saved {pages}-page PDF to {path}"));
                }
                PrintOutcome::Failed { tab, error } => {
                    if let Some(dialog) = &mut self.print {
                        if dialog.tab == tab {
                            dialog.busy = false;
                            dialog.error = Some(error);
                        }
                    }
                }
            }
        }
    }

    /// Keyboard shortcuts + content key routing.
    fn handle_shortcuts(&mut self, ctx: &Context) {
        let text_focus = ctx.wants_keyboard_input();
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());

        for event in &events {
            if let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            {
                use egui::Key::*;
                let ctrl = modifiers.ctrl || modifiers.command;
                let shift = modifiers.shift;
                match (key, ctrl, shift) {
                    // Work even while typing (browser-level).
                    (T, true, false) => self.new_tab(None),
                    (T, true, true) => {
                        if let Some(id) = self.shell.reopen_closed_tab() {
                            self.tabs.push(TabUi::new(
                                id,
                                self.shell
                                    .browser()
                                    .snapshot(id)
                                    .map(|s| s.url.clone())
                                    .unwrap_or_default(),
                                String::new(),
                            ));
                            self.set_active(self.tabs.len() - 1);
                        }
                    }
                    (W, true, false) => {
                        let index = self.active;
                        self.close_tab(index);
                    }
                    (L, true, false) => {
                        self.omnibox_focused = true;
                        self.omnibox = self.tabs.get(self.active).map(|t| t.url.clone()).unwrap_or_default();
                        self.chrome.omnibox_take_focus = true;
                    }
                    (R, true, false) => self.reload(),
                    (D, true, false) => self.toggle_bookmark(),
                    (F, true, false) => {
                        self.find_open = true;
                        self.chrome.find_take_focus = true;
                    }
                    (P, true, false) => self.open_print_dialog(),
                    (S, true, false) => self.open_save_dialog(),
                    (J, true, false) => {
                        self.navigate_url("rowser://downloads");
                    }
                    (H, true, false) => {
                        self.navigate_url("rowser://history");
                    }
                    (O, true, true) => {
                        self.navigate_url("rowser://bookmarks");
                    }
                    (Tab, true, false) => {
                        if !self.tabs.is_empty() {
                            let next = (self.active + 1) % self.tabs.len();
                            self.set_active(next);
                        }
                    }
                    (Tab, true, true) => {
                        if !self.tabs.is_empty() {
                            let prev = (self.active + self.tabs.len() - 1) % self.tabs.len();
                            self.set_active(prev);
                        }
                    }
                    (Equals, true, _) | (Plus, true, _) => self.zoom_step(1),
                    (Minus, true, _) => self.zoom_step(-1),
                    (Num0, true, false) => self.zoom_reset(),
                    (F11, false, _) => {
                        let fullscreen = self.chrome.is_fullscreen;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
                        self.chrome.is_fullscreen = !fullscreen;
                    }
                    (F12, false, _) => self.devtools_open = !self.devtools_open,
                    (I, true, true) => self.devtools_open = !self.devtools_open,
                    (ArrowLeft, true, false) => self.go_back(),
                    (ArrowRight, true, false) => self.go_forward(),
                    (Escape, false, _) if self.find_open => {
                        self.find_clear();
                    }
                    // Only when no text field has focus:
                    (ArrowDown, false, _) if !text_focus => self.scroll_active(120.0),
                    (ArrowUp, false, _) if !text_focus => self.scroll_active(-120.0),
                    (PageDown, false, _) if !text_focus => self.scroll_active(600.0),
                    (PageUp, false, _) if !text_focus => self.scroll_active(-600.0),
                    (Space, false, _) if !text_focus => {
                        let amount = if shift { -600.0 } else { 600.0 };
                        self.scroll_active(amount);
                    }
                    (Home, false, false) => {
                        if let Some(tab) = self.tabs.get_mut(self.active) {
                            tab.scroll_y = 0.0;
                            self.shell.browser().scroll(tab.id, 0.0);
                        }
                    }
                    (Home, false, true) => {
                        // Shift+Home → home page (keyboard-only machines).
                        if let Some(tab) = self.tabs.get(self.active) {
                            let home = self.shell.store().settings.homepage.clone();
                            self.shell.browser().navigate(tab.id, home);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Goes back in the active tab's history.
    pub fn go_back(&self) {
        if let Some(tab) = self.tabs.get(self.active) {
            self.shell.browser().go_back(tab.id);
        }
    }

    /// Goes forward.
    pub fn go_forward(&self) {
        if let Some(tab) = self.tabs.get(self.active) {
            self.shell.browser().go_forward(tab.id);
        }
    }

    /// Stops the active load.
    pub fn stop_load(&self) {
        if let Some(tab) = self.tabs.get(self.active) {
            self.shell.browser().stop(tab.id);
        }
    }

    /// Steps the zoom of the active tab.
    pub fn zoom_step(&mut self, direction: i32) {
        let steps = [0.5, 0.67, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let current = steps
            .iter()
            .position(|&z| (z - tab.zoom).abs() < 0.02)
            .unwrap_or(4);
        let next = (current as i32 + direction).clamp(0, steps.len() as i32 - 1) as usize;
        tab.zoom = steps[next];
    }

    /// Resets zoom.
    pub fn zoom_reset(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.zoom = 1.0;
        }
    }

    /// Scrolls the active tab.
    pub fn scroll_active(&mut self, delta_y: f32) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let content_h = self
            .shell
            .browser()
            .snapshot(tab.id)
            .map(|s| s.content_size.1)
            .unwrap_or(tab.viewport.1);
        let viewport_h = tab.viewport.1.max(1.0);
        let max = (content_h - viewport_h).max(0.0);
        tab.scroll_y = (tab.scroll_y + delta_y).clamp(0.0, max);
        self.shell.browser().scroll(tab.id, tab.scroll_y);
    }

    /// Toggles the bookmark for the active page.
    pub fn toggle_bookmark(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let url = tab.url.clone();
        let title = tab.title.clone();
        if url.is_empty() || url.starts_with("rowser:") {
            return;
        }
        {
            let store = self.shell.store_mut();
            if let Some(existing) = store.bookmarks.find_url(&url) {
                let id = existing.id;
                store.bookmarks.remove(id);
                self.removed_bookmark = true;
            } else {
                store.bookmarks.add(url, title);
                self.added_bookmark = true;
            }
            let dir = store.dir.clone();
            store.bookmarks.save(&dir);
        }
        if self.removed_bookmark {
            self.removed_bookmark = false;
            self.toast("Bookmark removed");
        }
        if self.added_bookmark {
            self.added_bookmark = false;
            self.toast("Bookmark added");
        }
    }

    /// Opens the print dialog for the active tab.
    pub fn open_print_dialog(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let filename = print_filename(&tab.url);
        let dir = self.shell.store().settings.download_dir.clone();
        let dialog = PrintDialog {
            tab: tab.id,
            path: dir.join(filename).display().to_string(),
            busy: true,
            error: None,
            preview: None,
            pages: None,
            restore: (
                tab.viewport.0.max(640.0),
                tab.viewport.1.max(480.0),
            ),
        };
        self.print = Some(dialog);

        // Render the first page as a preview in the background.
        let browser = self.shell.browser().clone();
        let tab_id = tab.id;
        let tx = self.print_tx.clone();
        let waker = std::sync::Arc::clone(&self.waker);
        std::thread::Builder::new()
            .name("rowser-print-preview".into())
            .spawn(move || {
                let outcome = match rowser_shell::render_pages(&browser, tab_id, (1280.0, 800.0)) {
                    Ok(pages) => {
                        let count = pages.len();
                        let image = pages.first().map(|p| egui::ColorImage {
                            size: [p.width as usize, p.height as usize],
                            source_size: egui::Vec2::new(p.width as f32, p.height as f32),
                            pixels: rgb_to_egui(&p.rgb),
                        });
                        PrintOutcome::Preview {
                            tab: tab_id,
                            pages: count,
                            image,
                        }
                    }
                    Err(err) => PrintOutcome::Failed {
                        tab: tab_id,
                        error: format!("{err:#}"),
                    },
                };
                let _ = tx.send(outcome);
                waker.wake();
            })
            .ok();
    }

    /// Opens the save-page dialog.
    pub fn open_save_dialog(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let dir = self.shell.store().settings.download_dir.clone();
        let filename = format!(
            "{}.html",
            tab.title
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .take(40)
                .collect::<String>()
                .to_lowercase()
        );
        self.file = Some(FileDialog {
            purpose: FilePurpose::SavePage,
            dir,
            filename: if filename.is_empty() {
                "page.html".into()
            } else {
                filename
            },
            entries: Vec::new(),
            error: None,
        });
        if let Some(dialog) = &mut self.file {
            dialog.refresh();
        }
    }

    /// Clears find state.
    pub fn find_clear(&mut self) {
        if let Some(tab) = self.tabs.get(self.active) {
            self.shell.browser().find_in_page(tab.id, "");
        }
        self.find_open = false;
        self.find_query.clear();
        self.find_state = (0, None);
    }

    pub(crate) fn sync_viewport(&mut self) {
        let zoom = self.tabs.get(self.active).map(|t| t.zoom).unwrap_or(1.0);
        let size = self.content_rect.size();
        let css_w = (size.x / zoom).round();
        let css_h = (size.y / zoom).round();
        if css_w < 100.0 || css_h < 100.0 {
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        if (tab.viewport.0 - css_w).abs() > 0.5 || (tab.viewport.1 - css_h).abs() > 0.5 {
            tab.viewport = (css_w, css_h);
            self.shell.browser().set_viewport(tab.id, css_w, css_h);
        }
        // Engine-side zoom is viewport math: keep scroll in range.
        let viewport_h = css_h;
        let content_h = self
            .shell
            .browser()
            .snapshot(tab.id)
            .map(|s| s.content_size.1)
            .unwrap_or(viewport_h);
        let max = (content_h - viewport_h).max(0.0);
        if tab.scroll_y > max {
            tab.scroll_y = max;
            self.shell.browser().scroll(tab.id, max);
        }
    }

    /// Commits the open file dialog (returns true when it should close).
    pub fn confirm_file_dialog(&mut self) -> bool {
        let Some(dialog) = self.file.as_ref() else {
            return true;
        };
        let purpose = dialog.purpose;
        let path = dialog.full_path();
        match purpose {
            FilePurpose::SavePage => {
                let Some(tab) = self.tabs.get(self.active) else { return true };
                let path = path.with_extension("html");
                self.shell.browser().save_page(tab.id, path);
                true
            }
            FilePurpose::SavePdf => {
                if let Some(dialog) = self.print.as_mut() {
                    dialog.path = path.display().to_string();
                }
                true
            }
            FilePurpose::ImportBookmarks => {
                match std::fs::read_to_string(&path) {
                    Ok(html) => {
                        let dir = self.shell.store().dir.clone();
                        let count = self.shell.store_mut().bookmarks.import_html(&html);
                        self.shell.store().bookmarks.save(&dir);
                        self.toast(format!("Imported {count} bookmarks"));
                    }
                    Err(err) => {
                        self.toast(format!("Import failed: {err}"));
                    }
                }
                true
            }
            FilePurpose::ExportBookmarks => {
                let html = self.shell.store().bookmarks.export_html();
                match std::fs::write(&path, html) {
                    Ok(()) => self.toast(format!("Bookmarks exported to {}", path.display())),
                    Err(err) => self.toast(format!("Export failed: {err}")),
                }
                true
            }
            FilePurpose::DownloadsDir => {
                let dir = if path.is_dir() {
                    path
                } else {
                    path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
                };
                self.shell.store_mut().settings.download_dir = dir;
                persist_settings_path(self);
                true
            }
        }
    }

    /// Saves the bookmark editor dialog.
    pub fn save_bookmark_edit(&mut self) {
        let Some(edit) = self.bookmark_edit.clone() else { return };
        let dir = self.shell.store().dir.clone();
        if edit.id == 0 {
            self.shell.store_mut().bookmarks.add(edit.url, edit.title);
            self.shell.store_mut().bookmarks.set_folder_last(edit.folder);
        } else {
            let store = self.shell.store_mut();
            if let Some(b) = store.bookmarks.items.iter_mut().find(|b| b.id == edit.id) {
                b.url = edit.url;
                b.title = edit.title;
                b.folder = edit.folder;
            }
        }
        self.shell.store().bookmarks.save(&dir);
    }

    /// Deletes the bookmark being edited.
    pub fn delete_bookmark_edit(&mut self) {
        let Some(edit) = self.bookmark_edit.clone() else { return };
        let dir = self.shell.store().dir.clone();
        self.shell.store_mut().bookmarks.remove(edit.id);
        self.shell.store().bookmarks.save(&dir);
    }
}

fn persist_settings_path(app: &mut BrowserApp) {
    let dir = app.shell.store().dir.clone();
    app.shell.store_mut().settings.save(&dir);
}

fn rgb_to_egui(rgb: &[u8]) -> Vec<egui::Color32> {
    rgb.chunks_exact(3)
        .map(|c| egui::Color32::from_rgb(c[0], c[1], c[2]))
        .collect()
}

fn refresh_frame(ctx: &Context, app: &mut BrowserApp, tab: TabId) {
    let Some(frame) = app.shell.browser().frame(tab) else { return };
    let Some(t) = app.tabs.iter_mut().find(|t| t.id == tab) else { return };
    if frame.frame.id <= t.frame_id && t.texture.is_some() {
        return;
    }
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [frame.width as usize, frame.height as usize],
        &frame.straight_rgba(),
    );
    if let Some(texture) = t.texture.as_mut() {
        texture.set(image, egui::TextureOptions::default());
    } else {
        t.texture = Some(ctx.load_texture(
            format!("page-{tab}"),
            image,
            egui::TextureOptions::default(),
        ));
    }
    t.frame_id = frame.frame.id;
    t.suspended = false;
}

impl eframe::App for BrowserApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.handle_events(ctx);
        self.poll_print();
        self.sync_theme(ctx);
        self.handle_shortcuts(ctx);
        let mut chrome = std::mem::take(&mut self.chrome);
        chrome.draw(self, ctx);
        self.chrome = chrome;

        // Keep things alive: engine events wake us via the waker; otherwise
        // poll a few times a second (progress, downloads, suspension).
        let busy = self
            .tabs
            .get(self.active)
            .map(|t| t.loading)
            .unwrap_or(false);
        ctx.request_repaint_after(if busy {
            Duration::from_millis(32)
        } else {
            Duration::from_millis(250)
        });

        // Expire toasts.
        let dt = ctx.input(|i| i.unstable_dt).clamp(0.0, 0.5);
        for toast in &mut self.toasts {
            toast.ttl -= dt;
        }
        self.toasts.retain(|t| t.ttl > 0.0);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shell.save_session();
        self.shell.shutdown();
    }
}

fn install_cjk_font(ctx: &Context) {
    let candidates = [
        "/usr/share/fonts/truetype/chinese/NotoSansSC-Regular.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "assets/fonts/NotoSansSC-Regular.ttf",
    ];
    let path = candidates.iter().find_map(|p| {
        let path = PathBuf::from(p);
        std::fs::metadata(&path).is_ok().then_some(path)
    });
    let Some(path) = path else { return };
    let Ok(bytes) = std::fs::read(&path) else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "noto-sans-sc".into(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        if let Some(list) = fonts.families.get_mut(&family) {
            list.push("noto-sans-sc".to_owned());
        }
    }
    ctx.set_fonts(fonts);
}

fn host_of(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = after_scheme.split(['/', '?', '#']).next().unwrap_or(after_scheme);
    host.trim_start_matches("www.")
}

fn print_filename(url: &str) -> String {
    let host = host_of(url);
    if host.is_empty() {
        "page.pdf".into()
    } else {
        format!("{}.pdf", host)
    }
}
