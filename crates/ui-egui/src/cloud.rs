//! The Cloud page: connect to a Nextcloud server, browse its folders, open a file in its Craft
//! app, and follow the files open from the cloud ("Open from cloud"), with the prompts that settle
//! a conflict.
//!
//! The page only asks (every change is a [`Command`], so automation can do the same) and only
//! shows what the backend reports: a file is called saved when the backend says so, and a file no
//! Craft app opens yet is shown but can't be opened.

use artcraft_launcher_core::cloud::{
    CLOSED_DETAIL, CloudAccount, CloudEntry, ConflictChoice, SEARCH_PATH, SHARED_PATH, SIGN_IN_EXPIRED, SyncStatus, app_for, normalize_server,
};
use artcraft_launcher_core::protocol::{Command, Event};
use egui::{Color32, CornerRadius, FontId, Id, Key, Modifiers, Pos2, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use crate::app::{Dialog, LauncherUi};
use crate::i18n::{fmt, msg};
use crate::theme::{self, Tokens};
use crate::widgets::{self, Kind};

/// Which page the window shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Page {
    #[default]
    Apps,
    Cloud,
}

/// What the Cloud page lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum View {
    /// One folder, with breadcrumbs.
    #[default]
    Files,
    Shared,
    Recent,
    Search,
}

/// A file open from the cloud, as the backend last reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenFile {
    pub path: String,
    pub status: SyncStatus,
    pub detail: Option<String>,
    /// The backend says Stop tracking loses nothing it could still upload (a read-only copy with
    /// changes, a copy that is missing): only then is it offered.
    pub forgettable: bool,
}

/// The last listing asked for: `key` is the folder path, [`SHARED_PATH`] or [`SEARCH_PATH`];
/// `entries` is `None` until the answer comes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Listing {
    pub key: String,
    pub entries: Option<Result<Vec<CloudEntry>, String>>,
}

/// The Cloud page's state, fed by the backend's `Event::Cloud*`.
#[derive(Debug, Default)]
pub(crate) struct CloudState {
    pub account: Option<CloudAccount>,
    /// The server address as typed.
    pub server: String,
    /// Why the typed address or the last sign-in didn't work.
    pub form_error: Option<String>,
    /// A sign-in has started and waits for the browser.
    pub waiting: bool,
    pub login_url: Option<String>,
    pub view: View,
    /// The folder the Files view shows (`/` at the top).
    pub folder: String,
    pub query: String,
    pub listing: Option<Listing>,
    pub recent: Option<Vec<CloudEntry>>,
    pub open: Vec<OpenFile>,
    /// Files closed while a copy of them was kept on this computer, and where (dismissible).
    pub closed: Vec<String>,
    /// The latest change, for the live region; the time it goes is set when it is first shown.
    pub announcement: Option<(String, Option<f64>)>,
    /// The next cloud dialog drawn puts focus on its default button.
    pub focus_dialog: bool,
    /// The next listing drawn puts focus on its first row (after the user moved to a folder).
    pub focus_first_row: bool,
}

/// How long a change stays in the live region.
const ANNOUNCE_FOR: f64 = 10.0;
/// Height of one row of the file list.
const ROW_H: f32 = 40.0;

/// The name of a server path.
fn file_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// `https://cloud.example.com/nc` → `cloud.example.com/nc`: the server as people say it.
fn host_of(server: &str) -> &str {
    server.split_once("://").map_or(server, |(_, rest)| rest)
}

/// What a status is called on screen.
pub(crate) fn status_label(status: SyncStatus) -> &'static str {
    match status {
        SyncStatus::Opening => tl!("Opening…"),
        SyncStatus::Saved => tl!("Saved to cloud"),
        SyncStatus::Uploading => tl!("Uploading…"),
        SyncStatus::WaitingToUpload => tl!("Waiting to upload"),
        SyncStatus::ReconnectNeeded => tl!("Reconnect needed"),
        SyncStatus::Conflict => tl!("Conflict"),
        SyncStatus::ReadOnly => tl!("Read-only"),
        SyncStatus::CloseInAppFirst => tl!("Close it in the app first"),
    }
}

/// The folders from the top down to `folder`: (name, path).
fn crumbs(folder: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut path = String::new();
    for seg in folder.split('/').filter(|s| !s.is_empty()) {
        path.push('/');
        path.push_str(seg);
        out.push((seg.to_string(), path.clone()));
    }
    out
}

/// What kind of file an icon shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileKind {
    Folder,
    Document,
    Sheet,
    Slides,
    Pdf,
    Other,
}

fn kind_of(entry: &CloudEntry) -> FileKind {
    if entry.is_dir {
        return FileKind::Folder;
    }
    match app_for(&entry.name) {
        Some("wordcraft") => FileKind::Document,
        Some("gridcraft") => FileKind::Sheet,
        Some("deckcraft") => FileKind::Slides,
        Some("pdfcraft") => FileKind::Pdf,
        _ => FileKind::Other,
    }
}

/// Draws a file-type icon (our own shapes): a folder, or a page with a folded corner and a mark
/// for its kind.
fn paint_file_icon(p: &egui::Painter, rect: Rect, kind: FileKind, color: Color32) {
    let stroke = Stroke::new(1.25, color);
    let c = rect.center();
    if kind == FileKind::Folder {
        let body = Rect::from_center_size(c + vec2(0.0, 1.5), vec2(18.0, 12.0));
        let tab = [
            pos2(body.left(), body.top()),
            pos2(body.left(), body.top() - 3.0),
            pos2(body.left() + 7.0, body.top() - 3.0),
            pos2(body.left() + 9.0, body.top()),
        ];
        p.add(egui::Shape::line(tab.to_vec(), stroke));
        p.rect_stroke(body, CornerRadius::ZERO, stroke, egui::StrokeKind::Middle);
        return;
    }
    let page = Rect::from_center_size(c, vec2(14.0, 18.0));
    let fold = 4.0;
    let outline =
        vec![page.left_top(), pos2(page.right() - fold, page.top()), pos2(page.right(), page.top() + fold), page.right_bottom(), page.left_bottom()];
    p.add(egui::Shape::closed_line(outline, stroke));
    let inner = page.shrink2(vec2(3.0, 5.0));
    match kind {
        FileKind::Document => {
            for i in 0..3 {
                let y = inner.top() + 1.0 + i as f32 * 3.5;
                p.hline(inner.left()..=inner.right(), y, stroke);
            }
        }
        FileKind::Sheet => {
            p.rect_stroke(inner, CornerRadius::ZERO, stroke, egui::StrokeKind::Middle);
            p.hline(inner.x_range(), inner.center().y, stroke);
            p.vline(inner.center().x, inner.y_range(), stroke);
        }
        FileKind::Slides => {
            let screen = Rect::from_min_size(inner.left_top(), vec2(inner.width(), inner.height() * 0.6));
            p.rect_filled(screen, CornerRadius::ZERO, color);
        }
        FileKind::Pdf => {
            let band = Rect::from_min_size(pos2(page.left(), inner.center().y - 1.5), vec2(page.width(), 4.0));
            p.rect_filled(band, CornerRadius::ZERO, color);
        }
        FileKind::Folder | FileKind::Other => {}
    }
}

/// Paints `text` from `left` (vertically centred on `y`), cut with "…" to `width`.
fn paint_cut(ui: &Ui, left: f32, y: f32, width: f32, text: &str, font: FontId, color: Color32) {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(0.0));
    let galley = ui.painter().layout_job(job);
    let at = Pos2::new(left, y - galley.size().y / 2.0);
    ui.painter().galley(at, galley, color);
}

/// Marks a widget's accessibility node as a polite live region named `text`.
fn live_region(ui: &Ui, id: Id, text: &str) {
    let text = text.to_string();
    ui.ctx().accesskit_node_builder(id, |node| {
        node.set_role(egui::accesskit::Role::Status);
        node.set_live(egui::accesskit::Live::Polite);
        node.set_label(text);
    });
}

impl LauncherUi {
    /// Whether the Cloud page can be offered: it needs the desktop launcher.
    pub(crate) fn cloud_available(&self) -> bool {
        self.caps.manage_apps
    }

    /// Puts `text` in the live region, replacing what was there.
    pub(crate) fn announce(&mut self, text: impl Into<String>) {
        self.cloud.announcement = Some((text.into(), None));
    }

    /// The name of the Craft app `id` (catalog id) as shown.
    fn app_name(&self, id: &str) -> String {
        self.entry(id).map_or_else(|| id.to_string(), |e| e.app.name.to_string())
    }

    /// The folder the Files view starts in: the last one shown.
    fn start_folder(&self) -> String {
        let f = self.settings.cloud_folder.trim();
        if f.starts_with('/') { f.to_string() } else { "/".to_string() }
    }

    /// Opens a cloud dialog, unless another kind of dialog is on screen (the panel still offers it).
    fn show_cloud_dialog(&mut self, dialog: Dialog) {
        if self.dialog.as_ref().is_none_or(Dialog::is_cloud) {
            self.dialog = Some(dialog);
            self.cloud.focus_dialog = true;
        }
    }

    /// Handles one `Event::Cloud*`. Listings and file statuses that arrive while no account is
    /// connected are answers to an earlier connection, and are dropped.
    pub(crate) fn on_cloud(&mut self, event: Event) {
        let connected = self.cloud.account.is_some();
        match event {
            Event::CloudLoginUrl(url) => {
                self.cloud.waiting = true;
                self.cloud.login_url = Some(url);
                self.announce(tl!("Approve the sign-in in your browser to finish connecting."));
            }
            Event::CloudConnected(account) => {
                let text = fmt(tl!("Connected to {server} as {name}"), &[("server", host_of(&account.server)), ("name", &account.display)]);
                self.cloud.account = Some(account);
                self.cloud.waiting = false;
                self.cloud.login_url = None;
                self.cloud.form_error = None;
                self.cloud.view = View::Files;
                self.cloud.folder = self.start_folder();
                self.cloud.listing = Some(Listing { key: self.cloud.folder.clone(), entries: None });
                self.send(Command::CloudList { path: self.cloud.folder.clone() });
                self.announce(text);
            }
            Event::CloudDisconnected => {
                let server = std::mem::take(&mut self.cloud.server);
                self.cloud = crate::cloud::CloudState { server, ..Default::default() };
                self.settings.cloud_folder.clear();
                if self.dialog.as_ref().is_some_and(Dialog::is_cloud) {
                    self.dialog = None;
                }
                self.announce(tl!("Disconnected from your cloud."));
            }
            Event::CloudListed { path, entries } => {
                if !connected {
                    return;
                }
                if let Some(listing) = self.cloud.listing.as_mut().filter(|l| l.key == path) {
                    if let Err(e) = &entries {
                        let text = msg(e);
                        self.cloud.announcement = Some((text, None));
                    }
                    listing.entries = Some(entries);
                }
            }
            Event::CloudFileStatus { path, status, detail, forgettable } => {
                if connected {
                    self.on_file_status(path, status, detail, forgettable);
                }
            }
            Event::CloudLockedBy { path, by, app } => {
                if !connected {
                    return;
                }
                let name = file_name(&path).to_string();
                let text = if app {
                    fmt(tl!("{name} is locked by an app in Nextcloud."), &[("name", &name)])
                } else if by.is_empty() {
                    fmt(tl!("Someone else is editing {name}."), &[("name", &name)])
                } else {
                    fmt(tl!("{by} is editing {name}."), &[("by", &by), ("name", &name)])
                };
                self.announce(text);
                self.show_cloud_dialog(Dialog::CloudLocked { path, by, app });
            }
            Event::CloudError(message) => {
                // A sign-in ends with CloudConnected or an error: either way it no longer waits.
                self.cloud.waiting = false;
                self.cloud.login_url = None;
                if !connected {
                    self.cloud.form_error = Some(message.clone());
                }
                self.toast(message, true);
            }
            Event::CloudRecentListed(list) if connected => self.cloud.recent = Some(list),
            _ => {}
        }
    }

    fn on_file_status(&mut self, path: String, status: SyncStatus, detail: Option<String>, forgettable: bool) {
        let name = file_name(&path).to_string();
        if detail.as_deref().is_some_and(|d| d.starts_with(CLOSED_DETAIL)) {
            self.cloud.open.retain(|f| f.path != path);
            if self.dialog.as_ref().is_some_and(|d| d.cloud_path() == Some(path.as_str())) {
                self.dialog = None;
            }
            let text = match detail.as_deref() {
                Some(d) if d != CLOSED_DETAIL => {
                    let note = format!("{name}: {}", msg(d));
                    self.cloud.closed.push(note.clone());
                    note
                }
                _ => fmt(tl!("{name} is saved to the cloud and closed."), &[("name", &name)]),
            };
            self.announce(text);
            return;
        }
        let i = match self.cloud.open.iter().position(|f| f.path == path) {
            Some(i) => i,
            None => {
                self.cloud.open.push(OpenFile { path: path.clone(), status, detail: None, forgettable });
                self.cloud.open.len() - 1
            }
        };
        let file = &mut self.cloud.open[i];
        file.forgettable = forgettable;
        file.status = status;
        file.detail.clone_from(&detail);
        let mut text = format!("{name}: {}", status_label(status));
        if let Some(d) = &detail {
            text.push_str(". ");
            text.push_str(&msg(d));
        }
        self.announce(text.clone());
        match status {
            SyncStatus::Conflict => self.show_cloud_dialog(Dialog::CloudConflict(path)),
            SyncStatus::CloseInAppFirst => self.show_cloud_dialog(Dialog::CloudCloseInApp(path)),
            _ => {
                // Settled elsewhere (another window of time, a restart): the prompt is out of date.
                if matches!(&self.dialog, Some(Dialog::CloudConflict(p) | Dialog::CloudUseMine(p) | Dialog::CloudCloseInApp(p)) if *p == path) {
                    self.dialog = None;
                }
                if status == SyncStatus::ReconnectNeeded && self.page != Page::Cloud {
                    self.toast(text, true);
                }
            }
        }
    }

    /// Asks for the folder `path` and shows it.
    fn go_to_folder(&mut self, path: &str) {
        self.cloud.view = View::Files;
        self.cloud.folder = path.to_string();
        self.settings.cloud_folder = path.to_string();
        self.cloud.listing = Some(Listing { key: path.to_string(), entries: None });
        self.cloud.focus_first_row = true;
        self.act_command(Command::CloudList { path: path.to_string() });
    }

    /// Shows `view`, asking the backend for what it lists.
    fn show_view(&mut self, view: View) {
        match view {
            View::Files => {
                let folder = if self.cloud.folder.is_empty() { "/".to_string() } else { self.cloud.folder.clone() };
                self.go_to_folder(&folder);
            }
            View::Shared => {
                self.cloud.view = View::Shared;
                self.cloud.listing = Some(Listing { key: SHARED_PATH.to_string(), entries: None });
                self.act_command(Command::CloudShared);
            }
            View::Recent => {
                self.cloud.view = View::Recent;
                self.cloud.recent = None;
                self.act_command(Command::CloudRecent);
            }
            View::Search => self.search(),
        }
    }

    fn search(&mut self) {
        let query = self.cloud.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.cloud.view = View::Search;
        self.cloud.listing = Some(Listing { key: SEARCH_PATH.to_string(), entries: None });
        self.act_command(Command::CloudSearch { query });
    }

    /// Opens the file `entry` in `app` (already known to be installed).
    fn open_entry(&mut self, entry: &CloudEntry, app: &str, read_only: bool) {
        self.act_command(Command::CloudOpen { path: entry.path.clone(), app: Some(app.to_string()), read_only });
    }

    /// The Cloud page.
    pub(crate) fn cloud_page(&mut self, ui: &mut Ui) {
        let t = Tokens::get(ui.ctx());
        self.cloud_live_line(ui, &t);
        ui.add_space(8.0);
        if self.cloud.account.is_none() {
            self.connect_form(ui, &t);
            return;
        }
        if self.cloud.waiting {
            // Reconnecting: the sign-in waits for the browser while the files stay listed.
            self.sign_in_waiting(ui, &t);
            ui.add_space(14.0);
        }
        self.account_bar(ui, &t);
        ui.add_space(14.0);
        self.view_bar(ui, &t);
        ui.add_space(10.0);
        let panel_rows = self.cloud.open.len() + self.cloud.closed.len();
        let panel_h = if panel_rows == 0 { 0.0 } else { (40.0 + panel_rows as f32 * 56.0).min(ui.available_height() * 0.45) };
        let list_h = (ui.available_height() - panel_h).max(ROW_H * 2.0);
        egui::ScrollArea::vertical().id_salt("cloud-list").max_height(list_h).auto_shrink([false, true]).show(ui, |ui| self.file_list(ui, &t));
        if panel_rows > 0 {
            self.open_panel(ui, &t);
        }
    }

    /// One line at the top of the page that says what just changed, also read out by screen
    /// readers (a polite live region). Its height is always kept, so nothing jumps.
    fn cloud_live_line(&mut self, ui: &mut Ui, t: &Tokens) {
        let now = ui.input(|i| i.time);
        if let Some((_, until)) = &mut self.cloud.announcement {
            let until = *until.get_or_insert(now + ANNOUNCE_FOR);
            if now > until {
                self.cloud.announcement = None;
            } else {
                ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
            }
        }
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
        if let Some((text, _)) = &self.cloud.announcement {
            let text = text.clone();
            let line = egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center));
            let resp = ui.scope_builder(line, |ui| ui.add(egui::Label::new(RichText::new(&text).size(12.0).color(t.muted)).truncate())).inner;
            live_region(ui, resp.id, &text);
        }
    }

    /// Connect to cloud: the server address and the sign-in in the browser.
    fn connect_form(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.add_space(16.0);
        ui.label(widgets::heading(tl!("Connect to cloud"), theme::SHEET_TITLE, t.ink_strong));
        ui.add_space(6.0);
        ui.label(RichText::new(tl!("Open your Nextcloud files in the Craft apps. Saves go back to your server, and a newer version saved by someone else is never replaced.")).color(t.muted));
        ui.add_space(18.0);
        if self.cloud.waiting {
            self.sign_in_waiting(ui, t);
            return;
        }
        widgets::hud(ui, tl!("Server address"), t.faint);
        ui.add_space(4.0);
        let mut submit = false;
        ui.horizontal(|ui| {
            let field = ui
                .add(
                    egui::TextEdit::singleline(&mut self.cloud.server)
                        .hint_text("cloud.example.com")
                        .desired_width(360.0)
                        .min_size(vec2(0.0, theme::CONTROL_H))
                        .align(egui::Align2::LEFT_CENTER)
                        .margin(vec2(10.0, 0.0)),
                )
                .accessible_name(tl!("Server address"));
            submit |= field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            submit |= widgets::button(ui, tl!("Connect"), Kind::Primary, 0.0).on_hover_text(tl!("Signs in through your browser")).clicked();
        });
        if submit {
            match normalize_server(&self.cloud.server) {
                Ok(server) => {
                    self.cloud.form_error = None;
                    self.cloud.waiting = true;
                    self.act_command(Command::CloudConnect { server });
                }
                Err(why) => {
                    self.announce(msg(&why));
                    self.cloud.form_error = Some(why);
                }
            }
        }
        if let Some(why) = &self.cloud.form_error {
            ui.add_space(8.0);
            ui.add(egui::Label::new(RichText::new(msg(why)).color(t.bad)).wrap());
        }
        ui.add_space(12.0);
        ui.label(RichText::new(tl!("The launcher keeps a sign-in you can revoke in Nextcloud, never your password.")).size(12.0).color(t.faint));
    }

    /// A sign-in waiting for the browser: its address, and Cancel.
    fn sign_in_waiting(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.label(RichText::new(tl!("Waiting for you to approve the sign-in in your browser…")).color(t.ink));
        if let Some(url) = self.cloud.login_url.clone() {
            ui.add_space(6.0);
            ui.label(RichText::new(tl!("If the browser didn't open, go to this address:")).color(t.muted));
            ui.add(egui::Label::new(RichText::new(url).family(theme::mono()).color(t.ink)).selectable(true));
        }
        ui.add_space(12.0);
        if widgets::button(ui, tl!("Cancel"), Kind::Secondary, 0.0).clicked() {
            self.act_command(Command::CloudCancelConnect);
            self.cloud.waiting = false;
            self.cloud.login_url = None;
        }
    }

    /// Signs in again to the connected server (after the sign-in was revoked or ran out).
    fn reconnect(&mut self) {
        if let Some(server) = self.cloud.account.as_ref().map(|a| a.server.clone()) {
            self.cloud.waiting = true;
            self.act_command(Command::CloudConnect { server });
        }
    }

    /// Who is connected, Reconnect and Disconnect.
    fn account_bar(&mut self, ui: &mut Ui, t: &Tokens) {
        let Some(account) = self.cloud.account.clone() else { return };
        ui.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter().rect_filled(dot, CornerRadius::ZERO, t.good);
            ui.add_space(8.0);
            let text = fmt(tl!("Connected to {server} as {name}"), &[("server", host_of(&account.server)), ("name", &account.display)]);
            ui.label(RichText::new(text).color(t.ink));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::button(ui, tl!("Disconnect"), Kind::Secondary, 0.0).clicked() {
                    self.show_cloud_dialog(Dialog::CloudDisconnect);
                }
                if !self.cloud.waiting
                    && widgets::button(ui, tl!("Reconnect"), Kind::Secondary, 0.0).on_hover_text(tl!("Sign in to this server again")).clicked()
                {
                    self.reconnect();
                }
            });
        });
    }

    /// Files | Shared with me | Recent, the breadcrumbs, the search field and Refresh.
    fn view_bar(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.horizontal(|ui| {
            let mut chosen = self.cloud.view;
            let options = [
                (View::Files, widgets::Cell { label: tl!("Files"), tip: tl!("Your folders"), count: None }),
                (View::Shared, widgets::Cell { label: tl!("Shared with me"), tip: tl!("Files others shared with you"), count: None }),
                (View::Recent, widgets::Cell { label: tl!("Recent"), tip: tl!("Files you opened from the launcher"), count: None }),
            ];
            if widgets::cells(ui, &mut chosen, &options) {
                self.show_view(chosen);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let loading = self.cloud.listing.as_ref().is_some_and(|l| l.entries.is_none());
                if crate::bar::refresh_button(ui, tl!("Refresh the list"), loading).clicked() {
                    self.show_view(self.cloud.view);
                }
                ui.add_space(4.0);
                if self.cloud.view == View::Search && widgets::glyph_button(ui, widgets::Glyph::Close, tl!("Clear search")).clicked() {
                    self.cloud.query.clear();
                    self.show_view(View::Files);
                }
                let field = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.cloud.query)
                            .hint_text(tl!("Search files"))
                            .desired_width(220.0)
                            .min_size(vec2(0.0, theme::CONTROL_H))
                            .align(egui::Align2::LEFT_CENTER)
                            .margin(vec2(10.0, 0.0)),
                    )
                    .accessible_name(tl!("Search files"));
                if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    self.search();
                }
            });
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| match self.cloud.view {
            View::Files => self.breadcrumbs(ui, t),
            View::Shared => {
                widgets::hud(ui, tl!("Shared with me"), t.muted);
            }
            View::Recent => {
                widgets::hud(ui, tl!("Opened from the launcher, newest first"), t.muted);
            }
            View::Search => {
                let text = fmt(tl!("Results for “{query}”"), &[("query", self.cloud.query.trim())]);
                widgets::hud(ui, &text, t.muted);
            }
        });
    }

    fn breadcrumbs(&mut self, ui: &mut Ui, t: &Tokens) {
        let folder = self.cloud.folder.clone();
        let trail = crumbs(&folder);
        let mut go = None;
        if crumb(ui, t, tl!("Top folder"), !trail.is_empty()).clicked() {
            go = Some("/".to_string());
        }
        for (i, (name, path)) in trail.iter().enumerate() {
            widgets::hud(ui, " › ", t.faint);
            if crumb(ui, t, name, i + 1 < trail.len()).clicked() {
                go = Some(path.clone());
            }
        }
        if let Some(path) = go {
            self.go_to_folder(&path);
        }
    }

    /// The rows of the current view, or why there are none.
    fn file_list(&mut self, ui: &mut Ui, t: &Tokens) {
        let (entries, recent) = if self.cloud.view == View::Recent {
            (self.cloud.recent.clone().map(Ok), true)
        } else {
            (self.cloud.listing.as_ref().and_then(|l| l.entries.clone()), false)
        };
        let entries = match entries {
            None => {
                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0).color(t.muted));
                    ui.label(RichText::new(tl!("Loading…")).color(t.muted));
                });
                return;
            }
            Some(Err(why)) => {
                ui.add_space(20.0);
                ui.add(egui::Label::new(RichText::new(msg(&why)).color(t.bad)).wrap());
                ui.add_space(10.0);
                if why == SIGN_IN_EXPIRED {
                    if !self.cloud.waiting && widgets::button(ui, tl!("Reconnect"), Kind::Primary, 0.0).clicked() {
                        self.reconnect();
                    }
                } else if widgets::button(ui, tl!("Try again"), Kind::Secondary, 0.0).clicked() {
                    self.show_view(self.cloud.view);
                }
                return;
            }
            Some(Ok(entries)) => entries,
        };
        if entries.is_empty() {
            ui.add_space(20.0);
            let empty = match self.cloud.view {
                View::Files => tl!("This folder is empty."),
                View::Shared => tl!("Nobody has shared anything with you yet."),
                View::Recent => tl!("Files you open from the cloud show here."),
                View::Search => tl!("No files match."),
            };
            widgets::hud(ui, empty, t.muted);
            return;
        }
        let mut rows = Vec::with_capacity(entries.len());
        for entry in &entries {
            if let Some(id) = self.entry_row(ui, t, entry, recent) {
                rows.push(id);
            }
        }
        if std::mem::take(&mut self.cloud.focus_first_row)
            && let Some(first) = rows.first()
        {
            ui.memory_mut(|m| m.request_focus(*first));
        }
        // Up and Down move between rows, as in a list.
        if let Some(i) = rows.iter().position(|id| ui.memory(|m| m.has_focus(*id))) {
            let (down, up) = ui.input_mut(|inp| (inp.consume_key(Modifiers::NONE, Key::ArrowDown), inp.consume_key(Modifiers::NONE, Key::ArrowUp)));
            let next = if down {
                rows.get(i + 1)
            } else if up {
                i.checked_sub(1).and_then(|j| rows.get(j))
            } else {
                None
            };
            if let Some(next) = next {
                ui.memory_mut(|m| {
                    m.request_focus(*next);
                    m.move_focus(egui::FocusDirection::None);
                });
            }
        }
    }

    /// One file or folder. Returns its id when it can be opened (and so takes focus).
    fn entry_row(&mut self, ui: &mut Ui, t: &Tokens, entry: &CloudEntry, recent: bool) -> Option<Id> {
        let app = (!entry.is_dir).then(|| app_for(&entry.name)).flatten();
        let installed = app.is_some_and(|a| self.is_installed(a));
        let openable = entry.is_dir || installed;
        let app_name = app.map(|a| self.app_name(a));
        let kind_text = if entry.is_dir {
            tl!("Folder").to_string()
        } else {
            match &app_name {
                Some(name) => name.clone(),
                None => tl!("Not supported yet").to_string(),
            }
        };
        let when = if entry.modified > 0 {
            let date = crate::views::date_text(entry.modified);
            if recent { fmt(tl!("Opened {date}"), &[("date", &date)]) } else { date }
        } else {
            String::new()
        };
        let size = if entry.is_dir || recent { String::new() } else { widgets::human_bytes(entry.size) };
        let mut marks = Vec::new();
        if entry.shared {
            marks.push(tl!("Shared").to_string());
        }
        if let Some(by) = &entry.locked_by {
            marks.push(if entry.lock_by_app {
                tl!("Locked by an app").to_string()
            } else if by.is_empty() {
                tl!("Locked").to_string()
            } else {
                fmt(tl!("Locked by {name}"), &[("name", by)])
            });
        }
        let mut facts: Vec<String> = vec![kind_text.clone()];
        facts.extend([size.clone(), when.clone()].into_iter().filter(|s| !s.is_empty()));
        facts.extend(marks.iter().cloned());
        let why_not = match (entry.is_dir, app, installed) {
            (true, _, _) | (false, Some(_), true) => None,
            (false, None, _) => Some(tl!("Opening from the cloud isn't supported for this app yet.").to_string()),
            (false, Some(_), false) => {
                Some(fmt(tl!("{app} isn't installed. Install it to open this file."), &[("app", app_name.as_deref().unwrap_or_default())]))
            }
        };
        let description = match &why_not {
            Some(why) => format!("{}. {why}", facts.join(", ")),
            None => facts.join(", "),
        };

        let width = ui.available_width();
        let (rect, resp) = ui.add_enabled_ui(openable, |ui| ui.allocate_exact_size(vec2(width, ROW_H), Sense::click())).inner;
        let focused = resp.has_focus();
        let p = ui.painter();
        if openable && (resp.hovered() || focused) {
            p.rect_filled(rect, CornerRadius::ZERO, t.sunken);
        }
        if focused {
            p.rect_stroke(rect.shrink(1.0), CornerRadius::ZERO, Stroke::new(1.5, t.accent_ink), egui::StrokeKind::Inside);
        }
        p.hline(rect.x_range(), rect.bottom() - 0.5, t.hairline());
        let ink = |c: Color32| if openable { c } else { c.gamma_multiply(0.6) };
        let icon_color = app.and_then(|a| self.entry(a)).map_or(t.muted, |e| t.app_ink(e.app));
        let y = rect.center().y;
        paint_file_icon(p, Rect::from_center_size(pos2(rect.left() + 20.0, y), vec2(20.0, 20.0)), kind_of(entry), ink(icon_color));
        // Columns from the right: marks, date, size, app; the name takes what is left.
        let marks_w = 110.0;
        let when_w = 150.0;
        let size_w = 80.0;
        let kind_w = 170.0;
        let right = rect.right() - 12.0;
        let marks_x = right - marks_w;
        let when_x = marks_x - when_w;
        let size_x = when_x - size_w;
        let kind_x = size_x - kind_w;
        let name_x = rect.left() + 40.0;
        let small = FontId::new(12.0, theme::medium());
        paint_cut(ui, name_x, y, kind_x - name_x - 12.0, &entry.name, FontId::new(13.5, theme::medium()), ink(t.ink_strong));
        let show_install = app.is_some() && !installed && self.caps.manage_apps;
        if !show_install {
            paint_cut(ui, kind_x, y, kind_w - 8.0, &kind_text, small.clone(), ink(t.muted));
        }
        paint_cut(ui, size_x, y, size_w - 8.0, &size, small.clone(), ink(t.muted));
        paint_cut(ui, when_x, y, when_w - 8.0, &when, small.clone(), ink(t.muted));
        paint_cut(ui, marks_x, y, marks_w, &marks.join(" · "), small, ink(t.muted));

        let name = entry.name.clone();
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, openable, &name));
        ui.ctx().accesskit_node_builder(resp.id, |node| node.set_description(description.clone()));
        let resp = match &why_not {
            Some(why) => resp.on_disabled_hover_text(why.as_str()),
            None => resp.on_hover_text(description.as_str()),
        };
        if show_install && let Some(app) = app {
            let label = fmt(tl!("Install {app}"), &[("app", app_name.as_deref().unwrap_or(app))]);
            let w = widgets::button_width(ui, &label, 0.0).min(kind_w);
            let at = Rect::from_center_size(pos2(kind_x + w / 2.0, y), vec2(w, theme::CONTROL_H));
            if ui.put(at, |ui: &mut Ui| widgets::button(ui, &label, Kind::Secondary, 0.0)).clicked() {
                self.install_app(app);
            }
        }
        if !openable {
            return None;
        }
        if resp.clicked() {
            if entry.is_dir {
                self.go_to_folder(&entry.path);
            } else if let Some(app) = app {
                self.open_entry(entry, app, false);
            }
        }
        Some(resp.id)
    }

    /// Open from cloud: each open file, its status in words, and what can be done with it.
    fn open_panel(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.add_space(8.0);
        ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top(), t.hairline());
        ui.add_space(10.0);
        widgets::hud(ui, tl!("Open from cloud"), t.faint);
        ui.add_space(6.0);
        egui::ScrollArea::vertical().id_salt("cloud-open").auto_shrink([false, true]).show(ui, |ui| {
            for file in self.cloud.open.clone() {
                self.open_row(ui, t, &file);
                ui.add_space(6.0);
            }
            let mut dismiss = None;
            for (i, note) in self.cloud.closed.iter().enumerate() {
                ui.horizontal(|ui| {
                    if widgets::inline_glyph_button(ui, widgets::Glyph::Close, tl!("Dismiss")).clicked() {
                        dismiss = Some(i);
                    }
                    ui.add(egui::Label::new(RichText::new(note).size(12.0).color(t.muted)).wrap());
                });
            }
            if let Some(i) = dismiss {
                self.cloud.closed.remove(i);
            }
        });
    }

    fn open_row(&mut self, ui: &mut Ui, t: &Tokens, file: &OpenFile) {
        let name = file_name(&file.path).to_string();
        let app = app_for(&name);
        ui.horizontal(|ui| {
            let entry = CloudEntry {
                path: file.path.clone(),
                name: name.clone(),
                is_dir: false,
                size: 0,
                modified: 0,
                etag: String::new(),
                shared: false,
                locked_by: None,
                lock_by_app: false,
            };
            let (icon, _) = ui.allocate_exact_size(vec2(24.0, theme::CONTROL_H), Sense::hover());
            let color = app.and_then(|a| self.entry(a)).map_or(t.muted, |e| t.app_ink(e.app));
            paint_file_icon(ui.painter(), icon, kind_of(&entry), color);
            ui.add_space(6.0);
            ui.label(RichText::new(&name).family(theme::medium()).color(t.ink_strong));
            if let Some(app) = app {
                ui.add_space(10.0);
                ui.label(RichText::new(self.app_name(app)).size(12.0).color(t.muted));
            }
            ui.add_space(14.0);
            let tone = match file.status {
                SyncStatus::Saved => t.good,
                SyncStatus::Conflict | SyncStatus::ReconnectNeeded => t.bad,
                SyncStatus::WaitingToUpload | SyncStatus::CloseInAppFirst | SyncStatus::ReadOnly => t.warn,
                SyncStatus::Opening | SyncStatus::Uploading => t.accent_ink,
            };
            let (mark, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter().rect_filled(mark, CornerRadius::ZERO, tone);
            ui.add_space(6.0);
            ui.label(RichText::new(status_label(file.status)).color(t.ink));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.open_actions(ui, file, &name));
        });
        if let Some(detail) = &file.detail {
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                ui.add(egui::Label::new(RichText::new(msg(detail)).size(12.0).color(t.muted)).wrap());
            });
        }
    }

    /// The buttons of one open file, right to left.
    fn open_actions(&mut self, ui: &mut Ui, file: &OpenFile, name: &str) {
        let named = |label: &str| format!("{label}: {name}");
        match file.status {
            SyncStatus::Conflict => {
                let label = tl!("Choose a version");
                if widgets::labelled_button(ui, label, &named(label), Kind::Primary, 0.0).clicked() {
                    self.show_cloud_dialog(Dialog::CloudConflict(file.path.clone()));
                }
            }
            SyncStatus::CloseInAppFirst => {
                let label = tl!("I've closed it");
                if widgets::labelled_button(ui, label, &named(label), Kind::Primary, 0.0)
                    .on_hover_text(tl!("Their version replaces yours, which is kept as a backup"))
                    .clicked()
                {
                    self.act_command(Command::CloudResolve { path: file.path.clone(), choice: ConflictChoice::TheirsClosed });
                }
            }
            SyncStatus::ReconnectNeeded => {
                let label = tl!("Reconnect");
                if !self.cloud.waiting && widgets::labelled_button(ui, label, &named(label), Kind::Primary, 0.0).clicked() {
                    self.reconnect();
                }
            }
            SyncStatus::Opening | SyncStatus::Uploading => {}
            SyncStatus::Saved | SyncStatus::WaitingToUpload | SyncStatus::ReadOnly => {
                let label = tl!("Done");
                if widgets::labelled_button(ui, label, &named(label), Kind::Secondary, 0.0)
                    .on_hover_text(tl!("Uploads what is left, then closes the file here"))
                    .clicked()
                {
                    self.act_command(Command::CloudDone { path: file.path.clone() });
                }
                if file.forgettable {
                    let label = tl!("Stop tracking (keep a local copy)");
                    if widgets::labelled_button(ui, label, &named(label), Kind::Secondary, 0.0)
                        .on_hover_text(tl!("Stops saving this file to the cloud; your copy moves to the kept folder on this computer"))
                        .clicked()
                    {
                        self.show_cloud_dialog(Dialog::CloudForget(file.path.clone()));
                    }
                }
            }
        }
    }

    /// The width a small cloud prompt takes.
    fn prompt_width(ui: &mut Ui) {
        ui.set_width(520.0f32.min(ui.ctx().content_rect().width() - 80.0));
    }

    fn prompt_title(ui: &mut Ui, t: &Tokens, title: &str) {
        ui.label(widgets::heading(title, theme::DIALOG_TITLE, t.ink_strong));
        ui.add_space(10.0);
    }

    /// Focuses `resp` if the dialog has just opened.
    fn focus_default(&mut self, resp: &egui::Response) {
        if std::mem::take(&mut self.cloud.focus_dialog) {
            resp.request_focus();
        }
    }

    fn open_detail(&self, path: &str) -> Option<String> {
        self.cloud.open.iter().find(|f| f.path == path).and_then(|f| f.detail.clone())
    }

    /// A file changed both here and in the cloud: which version to keep. Keep both is the default.
    pub(crate) fn cloud_conflict_dialog(&mut self, ui: &mut Ui, path: &str) -> bool {
        let t = Tokens::get(ui.ctx());
        Self::prompt_width(ui);
        let name = file_name(path);
        Self::prompt_title(ui, &t, &fmt(tl!("{name} was changed in the cloud too"), &[("name", name)]));
        let why = self.open_detail(path).map_or_else(
            || tl!("Someone changed this file in the cloud while you were editing it. Choose which version to keep.").to_string(),
            |d| msg(&d),
        );
        ui.add(egui::Label::new(RichText::new(why).color(t.muted)).wrap());
        ui.add_space(10.0);
        for line in [
            tl!("Keep both: your version goes up as a copy beside theirs. Nothing is lost."),
            tl!("Use mine: your version replaces theirs in the cloud."),
            tl!("Use theirs: their version opens here; yours is kept as a backup on this computer."),
        ] {
            ui.add(egui::Label::new(RichText::new(line).size(12.5).color(t.ink)).wrap());
        }
        ui.add_space(16.0);
        let mut close = false;
        ui.horizontal(|ui| {
            let keep = widgets::button(ui, tl!("Keep both"), Kind::Primary, 0.0);
            self.focus_default(&keep);
            if keep.clicked() {
                self.act_command(Command::CloudResolve { path: path.to_string(), choice: ConflictChoice::KeepBoth });
                close = true;
            }
            if widgets::button(ui, tl!("Use mine…"), Kind::Secondary, 0.0).clicked() {
                self.dialog = Some(Dialog::CloudUseMine(path.to_string()));
                self.cloud.focus_dialog = true;
            }
            if widgets::button(ui, tl!("Use theirs"), Kind::Secondary, 0.0).clicked() {
                self.act_command(Command::CloudResolve { path: path.to_string(), choice: ConflictChoice::UseTheirs });
                close = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close |= widgets::button(ui, tl!("Decide later"), Kind::Secondary, 0.0).clicked();
            });
        });
        close
    }

    /// "Use mine" replaces someone else's version: it is only sent once confirmed here.
    pub(crate) fn cloud_use_mine_dialog(&mut self, ui: &mut Ui, path: &str) -> bool {
        let t = Tokens::get(ui.ctx());
        Self::prompt_width(ui);
        Self::prompt_title(ui, &t, tl!("Overwrite the version in Nextcloud?"));
        let body = fmt(
            tl!("Your version of {name} replaces the one in the cloud, and the other person's changes there are lost."),
            &[("name", file_name(path))],
        );
        ui.add(egui::Label::new(RichText::new(body).color(t.muted)).wrap());
        ui.add_space(16.0);
        let mut close = false;
        ui.horizontal(|ui| {
            let cancel = widgets::button(ui, tl!("Cancel"), Kind::Secondary, 0.0);
            self.focus_default(&cancel);
            if cancel.clicked() {
                self.dialog = Some(Dialog::CloudConflict(path.to_string()));
                self.cloud.focus_dialog = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::button(ui, tl!("Overwrite"), Kind::Danger, 0.0).clicked() {
                    self.act_command(Command::CloudResolve { path: path.to_string(), choice: ConflictChoice::UseMine });
                    close = true;
                }
            });
        });
        close
    }

    /// After "Use theirs": the app still holds the user's version, so it must close it first.
    pub(crate) fn cloud_close_in_app_dialog(&mut self, ui: &mut Ui, path: &str) -> bool {
        let t = Tokens::get(ui.ctx());
        Self::prompt_width(ui);
        let name = file_name(path);
        let app = app_for(name).map_or_else(|| tl!("its app").to_string(), |a| self.app_name(a));
        Self::prompt_title(ui, &t, &fmt(tl!("Close {name} in {app}"), &[("name", name), ("app", &app)]));
        let body = self.open_detail(path).map_or_else(
            || {
                tl!("Close the document in its app, then confirm. Your version is kept as a backup on this computer, and their version opens.")
                    .to_string()
            },
            |d| msg(&d),
        );
        ui.add(egui::Label::new(RichText::new(body).color(t.muted)).wrap());
        ui.add_space(16.0);
        let mut close = false;
        ui.horizontal(|ui| {
            let closed = widgets::button(ui, tl!("I've closed it"), Kind::Primary, 0.0);
            self.focus_default(&closed);
            if closed.clicked() {
                self.act_command(Command::CloudResolve { path: path.to_string(), choice: ConflictChoice::TheirsClosed });
                close = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close |= widgets::button(ui, tl!("Not yet"), Kind::Secondary, 0.0).clicked();
            });
        });
        close
    }

    /// Someone else holds the file's lock: open it read-only, or not at all.
    pub(crate) fn cloud_locked_dialog(&mut self, ui: &mut Ui, path: &str, by: &str, app: bool) -> bool {
        let t = Tokens::get(ui.ctx());
        Self::prompt_width(ui);
        let title = if app {
            tl!("This file is locked by an app in Nextcloud").to_string()
        } else if by.is_empty() {
            tl!("Someone else is editing this file").to_string()
        } else {
            fmt(tl!("{name} is editing this file"), &[("name", by)])
        };
        Self::prompt_title(ui, &t, &title);
        let body = fmt(tl!("You can open {file} read-only. Changes you make to it aren't saved to the cloud."), &[("file", file_name(path))]);
        ui.add(egui::Label::new(RichText::new(body).color(t.muted)).wrap());
        ui.add_space(16.0);
        let mut close = false;
        let app = app_for(file_name(path));
        ui.horizontal(|ui| {
            if let Some(app) = app {
                let read_only = widgets::button(ui, tl!("Open read-only"), Kind::Primary, 0.0);
                self.focus_default(&read_only);
                if read_only.clicked() {
                    self.act_command(Command::CloudOpen { path: path.to_string(), app: Some(app.to_string()), read_only: true });
                    close = true;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let cancel = widgets::button(ui, tl!("Cancel"), Kind::Secondary, 0.0);
                if app.is_none() {
                    self.focus_default(&cancel);
                }
                close |= cancel.clicked();
            });
        });
        close
    }

    /// Stop tracking only after a confirmation that says what isn't in the cloud. Cancel has focus,
    /// so Enter keeps the file tracked.
    pub(crate) fn cloud_forget_dialog(&mut self, ui: &mut Ui, path: &str) -> bool {
        let t = Tokens::get(ui.ctx());
        Self::prompt_width(ui);
        let name = file_name(path);
        Self::prompt_title(ui, &t, &fmt(tl!("Stop tracking {name}?"), &[("name", name)]));
        let warning = tl!(
            "Your changes aren't in Nextcloud. They'll only be kept in the launcher's kept folder on this computer; the launcher says where once it's done."
        );
        ui.add(egui::Label::new(RichText::new(warning).color(t.ink)).wrap());
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(RichText::new(tl!("Nothing is uploaded and nothing is deleted. The file in the cloud stays as it is.")).color(t.muted))
                .wrap(),
        );
        ui.add_space(16.0);
        let mut close = false;
        ui.horizontal(|ui| {
            let cancel = widgets::button(ui, tl!("Cancel"), Kind::Secondary, 0.0);
            self.focus_default(&cancel);
            close |= cancel.clicked();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::button(ui, tl!("Stop tracking"), Kind::Danger, 0.0).clicked() {
                    self.act_command(Command::CloudForget { path: path.to_string() });
                    close = true;
                }
            });
        });
        close
    }

    /// Disconnect, after listing the files still open (which must be finished first).
    pub(crate) fn cloud_disconnect_dialog(&mut self, ui: &mut Ui) -> bool {
        let t = Tokens::get(ui.ctx());
        Self::prompt_width(ui);
        let server = self.cloud.account.as_ref().map(|a| host_of(&a.server).to_string()).unwrap_or_default();
        Self::prompt_title(ui, &t, &fmt(tl!("Disconnect from {server}?"), &[("server", &server)]));
        let open = self.cloud.open.clone();
        if open.is_empty() {
            ui.add(
                egui::Label::new(
                    RichText::new(tl!("The launcher forgets this sign-in and clears its downloaded copies. Anything left there is moved to the kept folder on this computer first; nothing in the cloud changes."))
                        .color(t.muted),
                )
                .wrap(),
            );
        } else {
            ui.add(
                egui::Label::new(
                    RichText::new(tl!("These files are still open from the cloud. Choose Done on each one (or Stop tracking) first:")).color(t.muted),
                )
                .wrap(),
            );
            ui.add_space(8.0);
            for f in &open {
                let line = format!("{}: {}", file_name(&f.path), status_label(f.status));
                let color = if f.status == SyncStatus::Saved { t.muted } else { t.ink };
                ui.label(RichText::new(line).color(color));
            }
        }
        ui.add_space(16.0);
        let mut close = false;
        ui.horizontal(|ui| {
            let cancel = widgets::button(ui, tl!("Cancel"), Kind::Secondary, 0.0);
            self.focus_default(&cancel);
            close |= cancel.clicked();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let allowed = open.is_empty();
                let resp = ui.add_enabled_ui(allowed, |ui| widgets::button(ui, tl!("Disconnect"), Kind::Danger, 0.0)).inner;
                let resp = resp.on_disabled_hover_text(tl!("Finish or close your open cloud files first."));
                if resp.clicked() {
                    self.act_command(Command::CloudDisconnect);
                    close = true;
                }
            });
        });
        close
    }
}

/// One folder in the breadcrumbs: a link-like button, plain text for the folder shown.
fn crumb(ui: &mut Ui, t: &Tokens, text: &str, clickable: bool) -> egui::Response {
    let galley = widgets::hud_galley(ui, text, t.ink);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(4.0, 8.0), if clickable { Sense::click() } else { Sense::hover() });
    let color = if !clickable || resp.hovered() || resp.has_focus() { t.ink_strong } else { t.accent_ink };
    ui.painter().galley_with_override_text_color(rect.left_center() + vec2(2.0, -galley.size().y / 2.0), galley, color);
    if resp.has_focus() {
        ui.painter().rect_stroke(rect, CornerRadius::ZERO, Stroke::new(1.0, t.accent_ink), egui::StrokeKind::Inside);
    }
    if clickable {
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Link, true, text));
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, text));
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crumbs_walk_down_from_the_top() {
        let c = crumbs("/Projects/2026");
        assert_eq!(c.len(), 2);
        assert!(c[1].1.starts_with(&c[0].1), "each crumb is inside the one before");
        assert!(crumbs("/").is_empty());
    }
}
