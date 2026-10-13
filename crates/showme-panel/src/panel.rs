//! The search panel: a field, the results grouped by kind with the weaker ones folded, the preview, and a footer that
//! says what Enter does. It stays in memory, hidden, and shows in a moment (`toggle`).
//!
//! Each keystroke asks the daemon twice: word matches (at once, the last word as a prefix) and a full search (meaning
//! too). Word matches show as they come; the full results replace them once typing pauses for 120 ms. Replies to older
//! keystrokes are dropped by their `seq`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use showme_core::model::{FileKind, code_query};
use showme_core::proto::{Request, Response, Status};
use showme_core::settings::Config;

use crate::client::{Daemon, Event, Line};
use crate::open;
use crate::pictures::{self, Key};
use crate::preview::{Preview, marked};
use crate::results::{Group, ResultRow, ResultSet};

const WIDTH: i32 = 1120;
const HEIGHT: i32 = 620;
/// The field and the kinds under it, the panel's height with no results.
const FIELD: i32 = 100;
/// The kinds that can be checked under the field, Alt+1 to Alt+6 (Alt+0 unchecks them all).
const KINDS: [(FileKind, &str); 6] = [
    (FileKind::Screenshot, "Screenshots"),
    (FileKind::Image, "Images"),
    (FileKind::Pdf, "PDFs"),
    (FileKind::Doc, "Documents"),
    (FileKind::Video, "Video"),
    (FileKind::Audio, "Audio"),
];
/// How long typing must pause before the full results replace the word matches.
const SETTLE: Duration = Duration::from_millis(120);

/// What a line of the list is.
#[derive(Clone)]
enum Item {
    Header,
    Result(Box<ResultRow>),
    Fold,
}

pub struct Panel {
    pub window: gtk::ApplicationWindow,
    entry: gtk::Entry,
    badge: gtk::Label,
    /// The kinds under the field: none checked searches them all.
    kinds: Vec<(FileKind, gtk::CheckButton)>,
    list: gtk::ListBox,
    body: gtk::Box,
    message: gtk::Label,
    footer: gtk::Label,
    preview: Rc<Preview>,
    daemon: Daemon,
    config: RefCell<Config>,
    items: RefCell<Vec<Item>>,
    results: RefCell<ResultSet>,
    /// Whether the full results (meaning too) of the current keystroke are shown, or only its word matches.
    shown_full: Cell<bool>,
    unfolded: Cell<bool>,
    seq: Cell<u64>,
    typed_at: Cell<Instant>,
    /// The user moved the selection: Space previews instead of typing a space.
    arrowed: Cell<bool>,
    /// The `segment_text` requests on the way, oldest first.
    texts: RefCell<std::collections::VecDeque<i64>>,
    status: RefCell<Option<Status>>,
}

impl Panel {
    pub fn new(app: &gtk::Application) -> Rc<Panel> {
        let config = Config::load().unwrap_or_default();
        let window = gtk::ApplicationWindow::builder().application(app).title("ShowMe").decorated(false).build();
        window.add_css_class("showme");
        // Broadway (GTK in a browser, for screenshots) doesn't grow a window once it's shown: start it full size there.
        let full = std::env::var_os("SHOWME_PANEL_FULL").is_some();
        window.set_default_size(WIDTH, if full { HEIGHT } else { FIELD });
        place(&window);

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("showme-frame");
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let icon = gtk::Image::from_icon_name("system-search-symbolic");
        icon.add_css_class("showme-search-icon");
        let entry = gtk::Entry::builder().hexpand(true).placeholder_text("Describe what you're looking for").build();
        entry.add_css_class("showme-query");
        let badge = gtk::Label::new(None);
        badge.add_css_class("showme-badge");
        header.append(&icon);
        header.append(&entry);
        header.append(&badge);
        frame.append(&header);

        let kind_row = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        kind_row.add_css_class("showme-kinds");
        let mut kinds = Vec::new();
        for (number, (kind, title)) in KINDS.into_iter().enumerate() {
            let check = gtk::CheckButton::with_label(title);
            check.set_tooltip_text(Some(&format!("Alt+{}, or start with {}:", number + 1, kind.as_str())));
            kind_row.append(&check);
            kinds.push((kind, check));
        }
        frame.append(&kind_row);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let columns = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        columns.set_vexpand(true);
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .min_content_width(560)
            .child(&list)
            .build();
        let message = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).vexpand(true).build();
        message.add_css_class("showme-message");
        let left = gtk::Stack::new();
        left.add_named(&scroller, Some("list"));
        left.add_named(&message, Some("message"));
        left.set_hexpand(true);
        columns.append(&left);
        columns.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        let preview = Preview::new(config.open.clone());
        columns.append(&preview.widget);
        body.append(&columns);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let footer = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
        footer.add_css_class("showme-footer");
        body.append(&footer);
        body.set_visible(false);
        // The panel's size comes from what it holds: a field alone, or the field over the results.
        body.set_size_request(-1, HEIGHT - FIELD);
        frame.set_size_request(WIDTH, -1);
        frame.append(&body);
        window.set_child(Some(&frame));

        let (events, incoming) = async_channel::unbounded();
        let daemon = Daemon::start(events);
        let panel = Rc::new(Panel {
            window,
            entry,
            badge,
            kinds,
            list,
            body,
            message,
            footer,
            preview,
            daemon,
            config: RefCell::new(config),
            items: RefCell::new(Vec::new()),
            results: RefCell::new(ResultSet::default()),
            shown_full: Cell::new(false),
            unfolded: Cell::new(false),
            seq: Cell::new(0),
            typed_at: Cell::new(Instant::now()),
            arrowed: Cell::new(false),
            texts: RefCell::new(Default::default()),
            status: RefCell::new(None),
        });
        panel.connect(left);
        let weak = Rc::downgrade(&panel);
        glib::spawn_future_local(async move {
            while let Ok(event) = incoming.recv().await {
                let Some(panel) = weak.upgrade() else { break };
                panel.handle(event);
            }
        });
        panel
    }

    fn connect(self: &Rc<Self>, left: gtk::Stack) {
        let weak = Rc::downgrade(self);
        self.entry.connect_changed(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.arrowed.set(false);
                panel.search();
            }
        });
        for (_, check) in &self.kinds {
            let weak = Rc::downgrade(self);
            check.connect_toggled(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.search();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.list.connect_selected_rows_changed(move |list| {
            if let Some(panel) = weak.upgrade() {
                panel.selection_changed(list.selected_row().map(|row| row.index()));
            }
        });
        let weak = Rc::downgrade(self);
        self.list.connect_row_activated(move |_, row| {
            if let Some(panel) = weak.upgrade() {
                panel.activate(row.index());
            }
        });
        // The message replaces the list when there's nothing to list.
        let weak = Rc::downgrade(self);
        self.message.connect_label_notify(move |message| {
            if weak.upgrade().is_some() {
                left.set_visible_child_name(if message.label().is_empty() { "list" } else { "message" });
            }
        });

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, modifiers| match weak.upgrade() {
            Some(panel) => panel.key(key, modifiers),
            None => glib::Propagation::Proceed,
        });
        self.window.add_controller(keys);

        // A panel that loses focus goes away, as a launcher does.
        let weak = Rc::downgrade(self);
        self.window.connect_is_active_notify(move |window| {
            if !window.is_active()
                && window.is_visible()
                && let Some(panel) = weak.upgrade()
            {
                glib::timeout_add_local_once(Duration::from_millis(150), move || {
                    if !panel.window.is_active() {
                        panel.hide();
                    }
                });
            }
        });
    }

    pub fn toggle(&self) {
        if self.window.is_visible() { self.hide() } else { self.present() }
    }

    pub fn present(&self) {
        self.daemon.send(Line::Other, Request::Prewarm);
        *self.config.borrow_mut() = Config::load().unwrap_or_default();
        self.window.present();
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
        self.update_message();
    }

    pub fn hide(&self) {
        self.preview.stop();
        pictures::clear();
        self.window.set_visible(false);
    }

    // Searching

    fn search(&self) {
        let text = self.entry.text().to_string();
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        self.typed_at.set(Instant::now());
        self.shown_full.set(false);
        if text.trim().is_empty() {
            self.show(ResultSet::default(), false);
            return;
        }
        let kinds = self.checked().iter().map(|kind| kind.as_str().to_string()).collect::<Vec<_>>();
        let request = |words_only: bool| Request::Search {
            query: text.clone(),
            limit: 60,
            kinds: kinds.clone(),
            prefix_last: words_only,
            words_only,
            seq: Some(seq),
        };
        self.daemon.send(Line::Words, request(true));
        self.daemon.send(Line::Full, request(false));
    }

    /// The kinds checked under the field.
    fn checked(&self) -> Vec<FileKind> {
        self.kinds.iter().filter(|(_, check)| check.is_active()).map(|(kind, _)| *kind).collect()
    }

    /// What the search is narrowed to, "PDFs in business", from what's typed before it or else what's checked.
    fn narrowed(&self, query: &str) -> Option<String> {
        let config = self.config.borrow();
        let scope = config.scope(query);
        let kinds =
            if scope.kinds.is_empty() { FileKind::titles(&self.checked()) } else { FileKind::titles(&scope.kinds) };
        match (kinds, scope.collection) {
            (Some(kinds), Some(collection)) => Some(format!("{kinds} in {}", collection.name)),
            (Some(kinds), None) => Some(kinds),
            (None, Some(collection)) => Some(collection.name.clone()),
            (None, None) => None,
        }
    }

    fn handle(self: &Rc<Self>, event: Event) {
        match event {
            Event::Status(status) => {
                *self.status.borrow_mut() = status.map(|status| *status);
                self.update_badge();
                self.update_message();
            }
            Event::Reply(Line::Other, response) => {
                if let Some(segment) = self.texts.borrow_mut().pop_front()
                    && let Some(text) = &response.text
                {
                    self.preview.set_text(segment, text);
                }
            }
            Event::Reply(line, response) => self.search_reply(line, *response),
        }
    }

    fn search_reply(self: &Rc<Self>, line: Line, response: Response) {
        if std::env::var_os("SHOWME_PANEL_DEBUG").is_some() {
            let hits = response.hits.as_ref().map_or(0, Vec::len);
            eprintln!(
                "reply {line:?} seq {:?} (now {}) ok {} hits {hits} {:?}",
                response.seq,
                self.seq.get(),
                response.ok,
                response.error
            );
        }
        if response.seq != Some(self.seq.get()) {
            return; // an older keystroke's
        }
        if !response.ok {
            let error = response.error.unwrap_or_default();
            if error != "superseded" {
                self.message.set_label(&error);
                self.body.set_visible(true);
            }
            return;
        }
        let terms = response.terms.clone().unwrap_or_default();
        let code = response.code.unwrap_or(false);
        let set = ResultSet::new(&response.hits.unwrap_or_default(), &terms, if code { 18 } else { 6 });
        match line {
            Line::Words if !self.shown_full.get() => self.show(set, false),
            Line::Full => {
                let waited = self.typed_at.get().elapsed();
                if waited >= SETTLE {
                    self.shown_full.set(true);
                    self.show(set, response.meaning.unwrap_or(false));
                } else {
                    let panel = self.clone();
                    let seq = self.seq.get();
                    let meaning = response.meaning.unwrap_or(false);
                    glib::timeout_add_local_once(SETTLE - waited, move || {
                        if panel.seq.get() == seq {
                            panel.shown_full.set(true);
                            panel.show(set, meaning);
                        }
                    });
                }
            }
            _ => {}
        }
    }

    // Showing results

    fn show(&self, set: ResultSet, meaning: bool) {
        let query = self.entry.text().to_string();
        let selected = self.selected_row().map(|row| (row.path, row.loc));
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut items = Vec::new();
        for group in &set.groups {
            let title = if group.total > group.rows.len() {
                format!("{}  ·  {} more", group.group.title().to_uppercase(), group.total - group.rows.len())
            } else {
                group.group.title().to_uppercase()
            };
            self.list.append(&header_row(&title));
            items.push(Item::Header);
            for row in &group.rows {
                self.list.append(&result_row(row));
                items.push(Item::Result(Box::new(row.clone())));
            }
        }
        if !set.weaker.is_empty() {
            let unfolded = self.unfolded.get();
            let arrow = if unfolded { "▾" } else { "▸" };
            let fold =
                gtk::Label::builder().xalign(0.0).label(format!("{arrow} {} weaker matches", set.weaker.len())).build();
            fold.add_css_class("showme-sub");
            self.list.append(&fold);
            items.push(Item::Fold);
            if unfolded {
                for row in &set.weaker {
                    self.list.append(&result_row(row));
                    items.push(Item::Result(Box::new(row.clone())));
                }
            }
        }
        for (index, item) in items.iter().enumerate() {
            if matches!(item, Item::Header)
                && let Some(row) = self.list.row_at_index(index as i32)
            {
                row.set_selectable(false);
                row.set_activatable(false);
            }
        }
        let strong = set.ranked.len();
        let weaker = set.weaker.len();
        *self.items.borrow_mut() = items;
        *self.results.borrow_mut() = set;

        let has_query = !query.trim().is_empty();
        self.body.set_visible(has_query);
        // Keep the selection on the same result, else the first.
        let keep = selected.and_then(|(path, loc)| {
            self.items
                .borrow()
                .iter()
                .position(|item| matches!(item, Item::Result(row) if row.path == path && row.loc == loc))
        });
        let first = self.items.borrow().iter().position(|item| matches!(item, Item::Result(_)));
        match keep.or(first).and_then(|index| self.list.row_at_index(index as i32)) {
            Some(row) => self.list.select_row(Some(&row)),
            None => self.preview.show(None, &self.config.borrow().open),
        }
        let mut footer = summary(strong, weaker);
        if let Some(narrowed) = self.narrowed(&query) {
            footer += &format!("  ·  {narrowed} only");
        }
        if !meaning && strong + weaker > 0 {
            footer += "  ·  words only so far";
        }
        self.set_footer(&footer);
        self.update_message();
    }

    /// What the list says instead of results: nothing found, no folders yet, the daemon isn't there.
    fn update_message(&self) {
        let query = self.entry.text().to_string();
        let status = self.status.borrow();
        let config = self.config.borrow();
        let scope = config.scope(&query);
        let narrowed = self.narrowed(&query);
        let text = if query.trim().is_empty() {
            String::new()
        } else if status.is_none() {
            "ShowMe's daemon isn't running.\nStart it with: systemctl --user start showme".into()
        } else if code_query::text(&query).is_some_and(|code| code.is_empty()) {
            "Search your code by what it does, or by a name in it:\n\ncode: retry a failed call with backoff\ncode: parse the config file\ncode: refreshAccessToken"
                .into()
        } else if code_query::text(&query).is_some() && !status.as_ref().is_some_and(|status| status.has_code) {
            "Code search is off.\nAdd code folders with: showme add --code ~/code".into()
        } else if scope.rest.is_empty() {
            let narrowed = narrowed.unwrap_or_default();
            match scope.collection {
                Some(collection) => {
                    format!("Search {narrowed} alone, by what's in it:\n\n{}", collection.folders.join("\n"))
                }
                None => format!("Search {narrowed} alone, by what's in them."),
            }
        } else if status.as_ref().is_some_and(|status| status.files == 0) {
            "No folders chosen yet.\nAdd some with: showme add ~/Documents ~/Pictures".into()
        } else if self.results.borrow().is_empty() && self.shown_full.get() {
            match narrowed {
                Some(narrowed) => format!("No good matches ({narrowed} only).\nTry other words, or search everything."),
                None => "No good matches.\nTry other words, or describe what's in it.".into(),
            }
        } else {
            String::new()
        };
        if self.message.label() != text {
            self.message.set_label(&text);
        }
    }

    fn update_badge(&self) {
        let status = self.status.borrow();
        let text = match status.as_ref() {
            None => "daemon not running".to_string(),
            Some(status) if status.model.state == "downloading" => {
                format!("Downloading the model {}", status.model.text.clone().unwrap_or_default())
            }
            Some(status) if matches!(status.state.as_str(), "indexing" | "syncing") && status.pending > 0 => {
                let percent = (status.searchable * 100).checked_div(status.files).unwrap_or(100);
                format!("Indexing {percent}%")
            }
            Some(status) if status.state == "paused" => "Indexing paused".into(),
            _ => String::new(),
        };
        self.badge.set_text(&text);
    }

    fn set_footer(&self, summary: &str) {
        let keys = match self.selected_row() {
            Some(row) => {
                let open = open::open_title(&row, &self.config.borrow().open);
                format!("↩ {open}   Ctrl+↩ Show in Files   Space Preview   Ctrl+C Copy   Ctrl+O Window")
            }
            None => "Alt+1–7 Filter   Esc Close".into(),
        };
        let text = if summary.is_empty() { keys } else { format!("{summary}        {keys}") };
        self.footer.set_text(&text);
    }

    fn selected_row(&self) -> Option<ResultRow> {
        let index = self.list.selected_row()?.index();
        match self.items.borrow().get(index as usize)? {
            Item::Result(row) => Some((**row).clone()),
            _ => None,
        }
    }

    fn selection_changed(&self, index: Option<i32>) {
        let item = index.and_then(|index| self.items.borrow().get(index as usize).cloned());
        let row = match item {
            Some(Item::Result(row)) => Some(*row),
            _ => None,
        };
        self.preview.show(row.as_ref(), &self.config.borrow().open);
        if let Some((segment, code)) = self.preview.wants_text() {
            self.texts.borrow_mut().push_back(segment);
            self.daemon.send(Line::Other, Request::SegmentText { segment_id: segment, code });
        }
        let results = self.results.borrow();
        let text = summary(results.ranked.len(), results.weaker.len());
        drop(results);
        self.set_footer(&text);
    }

    fn activate(&self, index: i32) {
        let item = self.items.borrow().get(index as usize).cloned();
        match item {
            Some(Item::Result(row)) => {
                open::open(&row, &self.config.borrow().open);
                self.hide();
            }
            Some(Item::Fold) => {
                self.unfolded.set(!self.unfolded.get());
                let set = self.results.borrow().clone();
                self.show(set, self.shown_full.get());
                if let Some(row) = self.list.row_at_index(index) {
                    self.list.select_row(Some(&row));
                }
            }
            _ => {}
        }
    }

    // Keys

    fn move_selection(&self, step: i32) {
        let items = self.items.borrow();
        let count = items.len() as i32;
        if count == 0 {
            return;
        }
        let mut index = self.list.selected_row().map(|row| row.index()).unwrap_or(-1);
        loop {
            index += step;
            if index < 0 || index >= count {
                return;
            }
            if !matches!(items[index as usize], Item::Header) {
                break;
            }
        }
        drop(items);
        if let Some(row) = self.list.row_at_index(index) {
            self.list.select_row(Some(&row));
            row.grab_focus();
            self.entry.grab_focus_without_selecting();
            self.arrowed.set(true);
        }
    }

    fn key(self: &Rc<Self>, key: gdk::Key, modifiers: gdk::ModifierType) -> glib::Propagation {
        let control = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        let alt = modifiers.contains(gdk::ModifierType::ALT_MASK);
        let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
        let handled = match key {
            gdk::Key::Escape => {
                self.hide();
                true
            }
            gdk::Key::Down => {
                self.move_selection(1);
                true
            }
            gdk::Key::Up => {
                self.move_selection(-1);
                true
            }
            gdk::Key::j | gdk::Key::n if control => {
                self.move_selection(1);
                true
            }
            gdk::Key::k | gdk::Key::p if control => {
                self.move_selection(-1);
                true
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                match self.selected_row() {
                    Some(row) if control => {
                        open::show_in_files(&row.path);
                        self.hide();
                    }
                    _ => {
                        if let Some(row) = self.list.selected_row() {
                            self.activate(row.index());
                        }
                    }
                }
                true
            }
            gdk::Key::space if self.arrowed.get() => self.preview.toggle_play(),
            gdk::Key::c | gdk::Key::C if control && !self.entry.selection_bounds().is_some_and(|(a, b)| a != b) => {
                match self.selected_row() {
                    Some(row) if shift => open::copy_path(&row.path),
                    Some(row) => open::copy_file(&row.path),
                    None => return glib::Propagation::Proceed,
                }
                self.set_footer("Copied");
                true
            }
            gdk::Key::comma if control => {
                crate::settings::open(&self.window.application().expect("no application"));
                self.hide();
                true
            }
            gdk::Key::o if control => {
                crate::window::open(&self.window.application().expect("no application"), &self.entry.text());
                self.hide();
                true
            }
            _ if alt => {
                let number = key.to_unicode().and_then(|c| c.to_digit(10)).map(|number| number as usize);
                match number {
                    Some(0) => {
                        self.kinds.iter().for_each(|(_, check)| check.set_active(false));
                        true
                    }
                    Some(number) if number <= self.kinds.len() => {
                        let check = &self.kinds[number - 1].1;
                        check.set_active(!check.is_active());
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        if handled { glib::Propagation::Stop } else { glib::Propagation::Proceed }
    }
}

/// The panel as an overlay on the focused screen (layer shell), else as a plain window Hyprland can float.
fn place(window: &gtk::ApplicationWindow) {
    #[cfg(feature = "layer-shell")]
    {
        use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
        let wayland = gdk::Display::default().is_some_and(|display| display.type_().name() == "GdkWaylandDisplay");
        if wayland && gtk4_layer_shell::is_supported() {
            window.init_layer_shell();
            window.set_namespace(Some("showme"));
            window.set_layer(Layer::Overlay);
            window.set_keyboard_mode(KeyboardMode::Exclusive);
            window.set_anchor(Edge::Top, true);
            // A little above the middle, as launchers sit.
            let top = gdk::Display::default()
                .and_then(|display| display.monitors().item(0))
                .and_then(|monitor| monitor.downcast::<gdk::Monitor>().ok())
                .map(|monitor| monitor.geometry().height() * 16 / 100)
                .unwrap_or(160);
            window.set_margin(Edge::Top, top);
            return;
        }
    }
    window.set_resizable(false);
}

/// "1 result", "12 results · 4 weaker".
fn summary(strong: usize, weaker: usize) -> String {
    let results = if strong == 1 { "1 result".to_string() } else { format!("{strong} results") };
    match (strong + weaker, weaker) {
        (0, _) => String::new(),
        (_, 0) => results,
        _ => format!("{results} · {weaker} weaker"),
    }
}

fn header_row(title: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder().xalign(0.0).label(title).build();
    label.add_css_class("showme-group");
    gtk::ListBoxRow::builder().child(&label).selectable(false).activatable(false).build()
}

fn result_row(row: &ResultRow) -> gtk::ListBoxRow {
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let thumb = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).build();
    thumb.set_size_request(40, 40);
    thumb.add_css_class("showme-thumb");
    let icon = gtk::Image::from_icon_name(Group::of(row.kind).icon());
    icon.set_pixel_size(22);
    icon.set_size_request(40, 40);
    let picture_slot = gtk::Stack::new();
    picture_slot.add_named(&icon, Some("icon"));
    picture_slot.add_named(&thumb, Some("thumb"));
    picture_slot.set_visible_child_name("icon");
    line.append(&picture_slot);
    let key = Key::new(row, 120, false);
    if key.drawable() && row.kind != FileKind::Pdf {
        let slot = picture_slot.clone();
        pictures::load(key, move |texture| {
            if let Some(texture) = texture {
                thumb.set_paintable(Some(&texture));
                slot.set_visible_child_name("thumb");
            }
        });
    }
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let name = gtk::Label::builder()
        .xalign(0.0)
        .label(row.title())
        .ellipsize(pango::EllipsizeMode::Middle)
        .max_width_chars(48)
        .hexpand(true)
        .build();
    name.add_css_class("showme-name");
    top.append(&name);
    if let Some(place) = row.place() {
        let badge = gtk::Label::new(Some(&place));
        badge.add_css_class("showme-place");
        top.append(&badge);
    }
    text.append(&top);
    let sub = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).max_width_chars(60).build();
    sub.add_css_class("showme-sub");
    match &row.excerpt {
        Some(excerpt) if excerpt.contains('«') => sub.set_markup(&marked(excerpt)),
        _ => sub.set_text(&row.folder),
    }
    text.append(&sub);
    line.append(&text);
    let list_row = gtk::ListBoxRow::builder().child(&line).build();
    // Drag a result out to copy the file (never to move it).
    let drag = gtk::DragSource::new();
    drag.set_actions(gdk::DragAction::COPY);
    let path = row.path.clone();
    drag.connect_prepare(move |_, _, _| {
        let file = gtk::gio::File::for_path(&path);
        Some(gdk::ContentProvider::for_value(&gdk::FileList::from_array(&[file]).to_value()))
    });
    list_row.add_controller(drag);
    list_row
}
