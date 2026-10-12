//! The big search window (Ctrl+O from the panel): the same search with room to look, pictures in a grid, filters by
//! kind. A normal window, so it stays while you work with what it found.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{glib, pango};
use showme_core::proto::{Request, Response};
use showme_core::settings::Config;

use crate::client::{Daemon, Event, Line};
use crate::open;
use crate::pictures::{self, Key};
use crate::preview::Preview;
use crate::results::{Group, ResultRow, ResultSet};

struct Window {
    window: gtk::ApplicationWindow,
    entry: gtk::SearchEntry,
    grid: gtk::FlowBox,
    count: gtk::Label,
    preview: Rc<Preview>,
    daemon: Daemon,
    config: Config,
    rows: RefCell<Vec<ResultRow>>,
    filter: std::cell::Cell<Option<Group>>,
    seq: std::cell::Cell<u64>,
    texts: RefCell<std::collections::VecDeque<i64>>,
}

/// Opens a window searching for `query`.
pub fn open(app: &gtk::Application, query: &str) {
    let config = Config::load().unwrap_or_default();
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("ShowMe")
        .default_width(1280)
        .default_height(820)
        .build();
    window.add_css_class("showme-window");
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("showme-frame");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    top.set_margin_top(8);
    top.set_margin_start(8);
    top.set_margin_end(8);
    let entry = gtk::SearchEntry::builder().hexpand(true).text(query).build();
    let count = gtk::Label::new(None);
    count.add_css_class("showme-sub");
    top.append(&entry);
    top.append(&count);
    frame.append(&top);
    let filters = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    filters.set_margin_start(8);
    let all = gtk::ToggleButton::with_label("All");
    all.add_css_class("showme-filter");
    all.set_active(true);
    filters.append(&all);
    let mut buttons = vec![(None, all.clone())];
    for group in Group::ALL {
        let button = gtk::ToggleButton::with_label(group.title());
        button.set_group(Some(&all));
        button.add_css_class("showme-filter");
        filters.append(&button);
        buttons.push((Some(group), button));
    }
    frame.append(&filters);
    frame.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let columns = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    columns.set_vexpand(true);
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(8)
        .valign(gtk::Align::Start)
        .build();
    let scroller = gtk::ScrolledWindow::builder().hexpand(true).child(&grid).build();
    columns.append(&scroller);
    columns.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    let preview = Preview::new(config.open.clone());
    columns.append(&preview.widget);
    frame.append(&columns);
    window.set_child(Some(&frame));

    let (events, incoming) = async_channel::unbounded();
    let daemon = Daemon::start(events);
    let state = Rc::new(Window {
        window: window.clone(),
        entry: entry.clone(),
        grid: grid.clone(),
        count,
        preview,
        daemon,
        config,
        rows: RefCell::new(Vec::new()),
        filter: std::cell::Cell::new(None),
        seq: std::cell::Cell::new(0),
        texts: RefCell::new(Default::default()),
    });
    let weak = Rc::downgrade(&state);
    entry.connect_search_changed(move |_| {
        if let Some(state) = weak.upgrade() {
            state.search();
        }
    });
    for (group, button) in buttons {
        let weak = Rc::downgrade(&state);
        button.connect_toggled(move |button| {
            if button.is_active()
                && let Some(state) = weak.upgrade()
            {
                state.filter.set(group);
                state.search();
            }
        });
    }
    let weak = Rc::downgrade(&state);
    grid.connect_selected_children_changed(move |grid| {
        if let Some(state) = weak.upgrade() {
            let index = grid.selected_children().first().map(|child| child.index());
            state.select(index);
        }
    });
    let weak = Rc::downgrade(&state);
    grid.connect_child_activated(move |_, child| {
        if let Some(state) = weak.upgrade()
            && let Some(row) = state.rows.borrow().get(child.index() as usize)
        {
            open::open(row, &state.config.open);
        }
    });
    let weak = Rc::downgrade(&state);
    glib::spawn_future_local(async move {
        while let Ok(event) = incoming.recv().await {
            let Some(state) = weak.upgrade() else { break };
            match event {
                Event::Reply(Line::Full, response) => state.show(*response),
                Event::Reply(Line::Other, response) => {
                    if let Some(segment) = state.texts.borrow_mut().pop_front()
                        && let Some(text) = &response.text
                    {
                        state.preview.set_text(segment, text);
                    }
                }
                _ => {}
            }
        }
    });
    window.present();
    state.search();
    // Kept alive by its handlers until the window closes.
    window.connect_close_request(move |_| {
        let _ = &state;
        pictures::clear();
        glib::Propagation::Proceed
    });
}

impl Window {
    fn search(&self) {
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        let query = self.entry.text().to_string();
        if query.trim().is_empty() {
            self.show(Response { seq: Some(seq), hits: Some(Vec::new()), ..Response::ok() });
            return;
        }
        let kinds = self.filter.get().map(|group| group.kinds()).unwrap_or_default();
        let request =
            Request::Search { query, limit: 200, kinds, prefix_last: false, words_only: false, seq: Some(seq) };
        self.daemon.send(Line::Full, request);
    }

    fn show(&self, response: Response) {
        if response.seq != Some(self.seq.get()) {
            return;
        }
        while let Some(child) = self.grid.first_child() {
            self.grid.remove(&child);
        }
        let set = ResultSet::new(&response.hits.unwrap_or_default(), &response.terms.unwrap_or_default(), usize::MAX);
        let rows: Vec<ResultRow> = set.ranked.iter().chain(&set.weaker).cloned().collect();
        self.count.set_text(&format!("{} results", rows.len()));
        for row in &rows {
            self.grid.append(&tile(row));
        }
        *self.rows.borrow_mut() = rows;
        if let Some(child) = self.grid.child_at_index(0) {
            self.grid.select_child(&child);
        }
        let _ = &self.window;
    }

    fn select(&self, index: Option<i32>) {
        let row = index.and_then(|index| self.rows.borrow().get(index as usize).cloned());
        self.preview.show(row.as_ref(), &self.config.open);
        if let Some((segment, code)) = self.preview.wants_text() {
            self.texts.borrow_mut().push_back(segment);
            self.daemon.send(Line::Other, Request::SegmentText { segment_id: segment, code });
        }
    }
}

/// A result in the grid: its picture (or an icon), its name and where it matched.
fn tile(row: &ResultRow) -> gtk::Box {
    let tile = gtk::Box::new(gtk::Orientation::Vertical, 4);
    tile.add_css_class("showme-tile");
    let slot = gtk::Stack::new();
    slot.set_size_request(180, 130);
    let icon = gtk::Image::from_icon_name(Group::of(row.kind).icon());
    icon.set_pixel_size(48);
    slot.add_named(&icon, Some("icon"));
    let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).build();
    picture.add_css_class("showme-thumb");
    slot.add_named(&picture, Some("picture"));
    let key = Key::new(row, 360, false);
    if key.drawable() {
        let shown = slot.clone();
        pictures::load(key, move |texture| {
            if let Some(texture) = texture {
                picture.set_paintable(Some(&texture));
                shown.set_visible_child_name("picture");
            }
        });
    }
    tile.append(&slot);
    let name =
        gtk::Label::builder().label(row.title()).ellipsize(pango::EllipsizeMode::Middle).max_width_chars(22).build();
    name.add_css_class("showme-name");
    tile.append(&name);
    let place = gtk::Label::new(Some(&row.place().unwrap_or_else(|| row.folder.clone())));
    place.set_ellipsize(pango::EllipsizeMode::Start);
    place.set_max_width_chars(24);
    place.add_css_class("showme-sub");
    tile.append(&place);
    tile
}
