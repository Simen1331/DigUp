//! The preview beside the results (it stands in for Quick Look): the picture, the page with your words marked, the
//! video or recording at its moment (and playing from there), the passage or the code that matched. Under it the name,
//! the folder, the details and why it matched, with Open and Show in Files.

use digger_core::model::FileKind;
use digger_core::settings::OpenSettings;
use gtk::prelude::*;
use gtk::{gio, glib, pango};
use sourceview5::prelude::*;

use crate::open;
use crate::pictures::{self, Key};
use crate::results::{ResultRow, clock};

pub struct Preview {
    pub widget: gtk::Box,
    media: gtk::Box,
    title: gtk::Label,
    folder: gtk::Button,
    details: gtk::Label,
    reason: gtk::Label,
    excerpt: gtk::Label,
    open_button: gtk::Button,
    show_button: gtk::Button,
    /// The row shown now, and the stream playing (stopped when the row changes).
    row: std::cell::RefCell<Option<ResultRow>>,
    stream: std::cell::RefCell<Option<gtk::MediaFile>>,
}

fn label(class: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .max_width_chars(50)
        .selectable(false)
        .build();
    label.add_css_class(class);
    label
}

/// Text with the words marked « » as Pango markup: the marks bold, the rest escaped.
pub fn marked(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('«') {
        out.push_str(&glib::markup_escape_text(&rest[..open]));
        let after = &rest[open + '«'.len_utf8()..];
        match after.find('»') {
            Some(close) => {
                out.push_str(&format!("<b>{}</b>", glib::markup_escape_text(&after[..close])));
                rest = &after[close + '»'.len_utf8()..];
            }
            None => {
                rest = after;
            }
        }
    }
    out.push_str(&glib::markup_escape_text(rest));
    out
}

impl Preview {
    pub fn new(settings: OpenSettings) -> std::rc::Rc<Preview> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 8);
        widget.add_css_class("digger-preview");
        widget.set_size_request(480, -1);
        widget.set_hexpand(false);
        let media = gtk::Box::new(gtk::Orientation::Vertical, 0);
        media.set_vexpand(true);
        let title = label("digger-title");
        title.set_selectable(true);
        let folder_label =
            gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::Start).max_width_chars(50).build();
        let folder = gtk::Button::builder().has_frame(false).halign(gtk::Align::Fill).child(&folder_label).build();
        folder.add_css_class("digger-folder");
        folder.set_tooltip_text(Some("Show in Files"));
        let details = label("digger-details");
        let reason = label("digger-reason");
        let excerpt = label("digger-sub");
        excerpt.set_lines(4);
        excerpt.set_ellipsize(pango::EllipsizeMode::End);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let open_button = gtk::Button::with_label("Open");
        let show_button = gtk::Button::with_label("Show in Files");
        buttons.append(&open_button);
        buttons.append(&show_button);
        for child in [media.upcast_ref::<gtk::Widget>(), title.upcast_ref(), folder.upcast_ref(), details.upcast_ref()]
        {
            widget.append(child);
        }
        widget.append(&reason);
        widget.append(&excerpt);
        widget.append(&buttons);
        let preview = std::rc::Rc::new(Preview {
            widget,
            media,
            title,
            folder,
            details,
            reason,
            excerpt,
            open_button,
            show_button,
            row: Default::default(),
            stream: Default::default(),
        });
        {
            let weak = std::rc::Rc::downgrade(&preview);
            let settings = settings.clone();
            preview.open_button.connect_clicked(move |_| {
                if let Some(row) = weak.upgrade().and_then(|preview| preview.row.borrow().clone()) {
                    open::open(&row, &settings);
                }
            });
        }
        for button in [&preview.show_button, &preview.folder] {
            let weak = std::rc::Rc::downgrade(&preview);
            button.connect_clicked(move |_| {
                if let Some(row) = weak.upgrade().and_then(|preview| preview.row.borrow().clone()) {
                    open::show_in_files(&row.path);
                }
            });
        }
        preview.widget.set_visible(false);
        preview
    }

    /// Stops what's playing (the row changed, or the panel hid).
    pub fn stop(&self) {
        if let Some(stream) = self.stream.borrow_mut().take() {
            stream.pause();
            stream.clear();
        }
    }

    /// Plays the video or recording from its moment, or pauses it (Space).
    pub fn toggle_play(&self) -> bool {
        let Some(stream) = self.stream.borrow().clone() else { return false };
        if stream.is_playing() {
            stream.pause()
        } else {
            stream.play()
        }
        true
    }

    pub fn show(&self, row: Option<&ResultRow>, settings: &OpenSettings) {
        if self.row.borrow().as_ref() == row {
            return;
        }
        self.stop();
        while let Some(child) = self.media.first_child() {
            self.media.remove(&child);
        }
        *self.row.borrow_mut() = row.cloned();
        let Some(row) = row else {
            self.widget.set_visible(false);
            return;
        };
        self.widget.set_visible(true);
        self.title.set_text(&row.title());
        if let Some(label) = self.folder.child().and_downcast::<gtk::Label>() {
            label.set_text(&row.folder);
        }
        self.details.set_text(&row.details());
        self.reason.set_text(&format!("✦ {}", row.reason()));
        match &row.excerpt {
            Some(excerpt) if !matches!(row.kind, FileKind::Code | FileKind::Doc) => {
                self.excerpt.set_markup(&marked(excerpt));
                self.excerpt.set_visible(true);
            }
            _ => self.excerpt.set_visible(false),
        }
        self.open_button.set_label(&open::open_title(row, settings));

        match row.kind {
            FileKind::Image | FileKind::Screenshot | FileKind::Pdf => self.show_picture(row),
            FileKind::Video => self.show_video(row),
            FileKind::Audio => self.show_audio(row),
            FileKind::Code | FileKind::Doc => self.show_text_placeholder(row),
        }
    }

    fn show_picture(&self, row: &ResultRow) {
        let picture =
            gtk::Picture::builder().content_fit(gtk::ContentFit::Contain).can_shrink(true).vexpand(true).build();
        picture.set_height_request(300);
        self.media.append(&picture);
        let key = Key::new(row, 900, row.kind == FileKind::Pdf);
        pictures::load(key, move |texture| {
            if let Some(texture) = texture {
                picture.set_paintable(Some(&texture));
            }
        });
    }

    /// The video at its moment, ready to play from there. GStreamer plays it inside the panel.
    fn show_video(&self, row: &ResultRow) {
        let file = gtk::MediaFile::for_file(&gio::File::for_path(&row.path));
        file.set_muted(false);
        let video = gtk::Video::builder().autoplay(false).vexpand(true).build();
        video.set_height_request(300);
        video.set_media_stream(Some(&file));
        let at = row.loc.unwrap_or(0.0);
        // Seek once the stream knows its length; it shows the frame there until played.
        file.connect_prepared_notify(move |file| {
            if file.is_prepared() && at > 0.5 {
                file.seek((at * 1_000_000.0) as i64);
            }
        });
        self.media.append(&video);
        let moment = gtk::Label::new(Some(&match (row.place(), row.duration()) {
            (Some(place), Some(duration)) => format!("{place} of {}", clock(duration)),
            (Some(place), None) => place,
            _ => String::new(),
        }));
        moment.add_css_class("digger-sub");
        self.media.append(&moment);
        *self.stream.borrow_mut() = Some(file);
    }

    fn show_audio(&self, row: &ResultRow) {
        let icon = gtk::Image::from_icon_name("audio-x-generic-symbolic");
        icon.set_pixel_size(96);
        icon.set_vexpand(true);
        self.media.append(&icon);
        let file = gtk::MediaFile::for_file(&gio::File::for_path(&row.path));
        let at = row.loc.unwrap_or(0.0);
        file.connect_prepared_notify(move |file| {
            if file.is_prepared() && at > 0.5 {
                file.seek((at * 1_000_000.0) as i64);
            }
        });
        let controls = gtk::MediaControls::new(Some(&file));
        self.media.append(&controls);
        let span = match (row.loc, row.loc_end, row.duration()) {
            (Some(start), Some(end), Some(duration)) => {
                format!("{} – {} of {}", clock(start), clock(end), clock(duration))
            }
            (Some(start), _, _) => clock(start),
            _ => String::new(),
        };
        let moment = gtk::Label::new(Some(&span));
        moment.add_css_class("digger-sub");
        self.media.append(&moment);
        *self.stream.borrow_mut() = Some(file);
    }

    /// Code and passages need their text from the daemon (`set_text` fills them in); until then, the excerpt.
    fn show_text_placeholder(&self, row: &ResultRow) {
        let waiting = label("digger-sub");
        if let Some(excerpt) = &row.excerpt {
            waiting.set_markup(&marked(excerpt));
        }
        waiting.set_vexpand(true);
        waiting.set_valign(gtk::Align::Start);
        self.media.append(&waiting);
    }

    /// The segment the preview needs text for: (segment id, from the code index).
    pub fn wants_text(&self) -> Option<(i64, bool)> {
        let row = self.row.borrow();
        let row = row.as_ref()?;
        matches!(row.kind, FileKind::Code | FileKind::Doc).then_some((row.segment_id, row.kind == FileKind::Code))
    }

    /// The text of the code stretch or passage the preview shows.
    pub fn set_text(&self, segment_id: i64, text: &str) {
        let row = self.row.borrow().clone();
        let Some(row) = row.filter(|row| row.segment_id == segment_id) else { return };
        while let Some(child) = self.media.first_child() {
            self.media.remove(&child);
        }
        let scroller = gtk::ScrolledWindow::builder().vexpand(true).min_content_height(300).build();
        if row.kind == FileKind::Code {
            scroller.set_child(Some(&code_view(&row, text)));
            scroller.add_css_class("digger-code");
        } else {
            scroller.set_child(Some(&passage_view(&row, text)));
            scroller.add_css_class("digger-passage");
        }
        self.media.append(&scroller);
    }
}

/// The words to mark: the ones keyword search found, then the query's.
fn words(row: &ResultRow) -> Vec<String> {
    row.found_words().into_iter().chain(row.terms.iter().cloned()).filter(|word| word.chars().count() >= 2).collect()
}

/// Marks each of `words` in `buffer` (whatever its case) with `tag`; returns where the first mark is.
fn mark(buffer: &gtk::TextBuffer, words: &[String], tag: &gtk::TextTag) -> Option<i32> {
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_lowercase();
    let mut first: Option<i32> = None;
    for word in words {
        let needle = word.to_lowercase();
        for (at, _) in text.match_indices(&needle) {
            let start_char = text[..at].chars().count() as i32;
            let end_char = start_char + needle.chars().count() as i32;
            let (start, end) = (buffer.iter_at_offset(start_char), buffer.iter_at_offset(end_char));
            buffer.apply_tag(tag, &start, &end);
            first = Some(first.map_or(start_char, |known| known.min(start_char)));
        }
    }
    first
}

fn passage_view(row: &ResultRow, text: &str) -> gtk::TextView {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(6)
        .right_margin(6)
        .build();
    let buffer = view.buffer();
    buffer.set_text(text);
    let tag = buffer.create_tag(Some("mark"), &[("background", &"#e5c07b"), ("foreground", &"#1a1b26")]);
    if let Some(tag) = tag
        && let Some(first) = mark(&buffer, &words(row), &tag)
    {
        let view = view.clone();
        glib::idle_add_local_once(move || {
            let mut at = view.buffer().iter_at_offset(first);
            view.scroll_to_iter(&mut at, 0.1, true, 0.0, 0.2);
        });
    }
    view
}

/// The code with its line numbers (from where the stretch starts), its language's colors, the line it opens at marked
/// and in view.
fn code_view(row: &ResultRow, text: &str) -> gtk::Box {
    let first = row.loc.unwrap_or(1.0) as usize;
    let lines = text.split('\n').count();
    let numbers = gtk::Label::builder().xalign(1.0).yalign(0.0).build();
    numbers.add_css_class("digger-sub");
    numbers.add_css_class("monospace");
    numbers.set_text(&(first..first + lines).map(|line| line.to_string()).collect::<Vec<_>>().join("\n"));
    let buffer = sourceview5::Buffer::new(None);
    let manager = sourceview5::LanguageManager::default();
    if let Some(language) = manager.guess_language(Some(&row.path), None) {
        buffer.set_language(Some(&language));
    }
    let schemes = sourceview5::StyleSchemeManager::default();
    if let Some(scheme) = schemes.scheme("Adwaita-dark").or_else(|| schemes.scheme("oblivion")) {
        buffer.set_style_scheme(Some(&scheme));
    }
    buffer.set_highlight_syntax(true);
    buffer.set_text(&text.replace('\t', "    "));
    let view = sourceview5::View::with_buffer(&buffer);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_hexpand(true);
    let gtk_buffer: gtk::TextBuffer = buffer.clone().upcast();
    if let Some(tag) = gtk_buffer.create_tag(Some("mark"), &[("background", &"rgba(229,192,123,0.35)")]) {
        mark(&gtk_buffer, &words(row), &tag);
    }
    // The line it opens at.
    if let Some(focus) = row.focus.map(|line| line as usize).filter(|line| *line >= first)
        && let Some(tag) = gtk_buffer.create_tag(Some("focus"), &[("paragraph-background", &"rgba(125,207,255,0.15)")])
        && let Some(start) = gtk_buffer.iter_at_line((focus - first) as i32)
    {
        let mut end = start;
        end.forward_to_line_end();
        gtk_buffer.apply_tag(&tag, &start, &end);
        let view = view.clone();
        glib::idle_add_local_once(move || {
            if let Some(mut at) = view.buffer().iter_at_line((focus - first).saturating_sub(2) as i32) {
                view.scroll_to_iter(&mut at, 0.0, true, 0.0, 0.0);
            }
        });
    }
    let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row_box.append(&numbers);
    row_box.append(&view);
    row_box
}
