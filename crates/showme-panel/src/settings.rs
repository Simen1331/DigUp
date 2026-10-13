//! The settings window (`showme-panel --settings`): folders, code, how results open, what Tesseract reads, power,
//! looks, and what the daemon is doing. Changes are written to `~/.config/showme/config.toml` as they're made, and the
//! daemon is told to read it again.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{glib, pango};
use showme_core::estimator;
use showme_core::proto::{Request, Status};
use showme_core::settings::Config;

use crate::client::{Daemon, Event, Line};
use crate::folders::FolderPicker;

/// Languages Tesseract has, as it names them ("eng", "nor"), without its page-layout data.
pub fn ocr_languages() -> Vec<String> {
    let Ok(output) = Command::new("tesseract").arg("--list-langs").stderr(Stdio::null()).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1)
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty() && line != "osd" && !line.contains(' '))
        .collect()
}

type OnSaved = Box<dyn Fn(Result<(), String>)>;

/// Edits the settings, and writes them a moment after the last change.
pub struct Editing {
    pub config: RefCell<Config>,
    daemon: Daemon,
    pending: RefCell<Option<glib::SourceId>>,
    on_saved: RefCell<Option<OnSaved>>,
}

impl Editing {
    pub fn new(daemon: Daemon) -> Rc<Editing> {
        Rc::new(Editing {
            config: RefCell::new(Config::load().unwrap_or_default()),
            daemon,
            pending: RefCell::new(None),
            on_saved: RefCell::new(None),
        })
    }

    /// Changes the settings; they're saved soon (folders start indexing then).
    pub fn change(self: &Rc<Self>, edit: impl FnOnce(&mut Config)) {
        let before = self.config.borrow().clone();
        edit(&mut self.config.borrow_mut());
        if *self.config.borrow() == before {
            return;
        }
        if let Some(source) = self.pending.borrow_mut().take() {
            source.remove();
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(Duration::from_millis(800), move || {
            if let Some(editing) = weak.upgrade() {
                editing.pending.borrow_mut().take();
                editing.save();
            }
        });
        *self.pending.borrow_mut() = Some(source);
    }

    /// Writes the settings now (when a change is waiting) and has the daemon read them.
    pub fn save(&self) {
        if let Some(source) = self.pending.borrow_mut().take() {
            source.remove();
        }
        let result = self.config.borrow().save().map_err(|error| format!("{error:#}"));
        if result.is_ok() {
            self.daemon.send(Line::Other, Request::Reload);
        }
        if let Some(callback) = self.on_saved.borrow().as_ref() {
            callback(result);
        }
    }

    pub fn flush(&self) {
        if self.pending.borrow().is_some() {
            self.save();
        }
    }

    pub fn connect_saved(&self, callback: impl Fn(Result<(), String>) + 'static) {
        *self.on_saved.borrow_mut() = Some(Box::new(callback));
    }
}

// Building blocks, shared with setup.

/// Keeps `value` for as long as `widget` is there.
pub fn keep<T: 'static>(widget: &impl IsA<gtk::Widget>, value: T) {
    let value = RefCell::new(Some(value));
    widget.connect_destroy(move |_| drop(value.borrow_mut().take()));
}

/// Puts `text` on the clipboard.
pub fn copy(text: &str) {
    if let Some(display) = gtk::gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

/// A page's column, with room around it.
pub fn page() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 14);
    page.set_margin_top(20);
    page.set_margin_bottom(20);
    page.set_margin_start(24);
    page.set_margin_end(24);
    page
}

pub fn heading(text: &str) -> gtk::Label {
    let label = gtk::Label::builder().label(text).xalign(0.0).build();
    label.add_css_class("showme-heading");
    label
}

pub fn note(text: &str) -> gtk::Label {
    let label = gtk::Label::builder().label(text).xalign(0.0).wrap(true).wrap_mode(pango::WrapMode::WordChar).build();
    label.add_css_class("showme-sub");
    label
}

/// A setting's line: what it is, what it means beneath, and its control on the right.
pub fn row(title: &str, meaning: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    let words = gtk::Box::new(gtk::Orientation::Vertical, 2);
    words.set_hexpand(true);
    words.append(&gtk::Label::builder().label(title).xalign(0.0).build());
    if !meaning.is_empty() {
        words.append(&note(meaning));
    }
    row.append(&words);
    control.set_valign(gtk::Align::Center);
    row.append(control);
    row
}

fn switch(editing: &Rc<Editing>, on: bool, set: impl Fn(&mut Config, bool) + 'static) -> gtk::Switch {
    let switch = gtk::Switch::builder().active(on).build();
    let editing = editing.clone();
    switch.connect_active_notify(move |switch| {
        let on = switch.is_active();
        editing.change(|config| set(config, on));
    });
    switch
}

fn entry(editing: &Rc<Editing>, text: &str, hint: &str, set: impl Fn(&mut Config, &str) + 'static) -> gtk::Entry {
    let entry = gtk::Entry::builder().text(text).placeholder_text(hint).width_chars(28).build();
    let editing = editing.clone();
    entry.connect_changed(move |entry| {
        let text = entry.text();
        editing.change(|config| set(config, text.trim()));
    });
    entry
}

/// Seconds, edited in minutes.
fn minutes(editing: &Rc<Editing>, seconds: f64, set: impl Fn(&mut Config, f64) + 'static) -> gtk::SpinButton {
    let spin = gtk::SpinButton::with_range(1.0, 240.0, 1.0);
    spin.set_value((seconds / 60.0).round().max(1.0));
    let editing = editing.clone();
    spin.connect_value_changed(move |spin| {
        let seconds = spin.value() * 60.0;
        editing.change(|config| set(config, seconds));
    });
    spin
}

/// A choice from a few (shown, stored) pairs, with what's stored now selected (added when it's none of them).
fn choice(
    editing: &Rc<Editing>,
    options: &[(String, String)],
    now: &str,
    set: impl Fn(&mut Config, &str) + 'static,
) -> gtk::DropDown {
    let mut options = options.to_vec();
    if !options.iter().any(|(_, stored)| stored == now) {
        options.push((now.to_string(), now.to_string()));
    }
    let shown: Vec<&str> = options.iter().map(|(shown, _)| shown.as_str()).collect();
    let dropdown = gtk::DropDown::from_strings(&shown);
    dropdown.set_selected(options.iter().position(|(_, stored)| stored == now).unwrap_or(0) as u32);
    let editing = editing.clone();
    dropdown.connect_selected_notify(move |dropdown| {
        if let Some((_, stored)) = options.get(dropdown.selected() as usize) {
            editing.change(|config| set(config, stored));
        }
    });
    dropdown
}

/// The folders' page: the picker, and what's read in them.
/// With `suggest` (setup, nothing chosen yet), the usual folders start checked.
pub fn folders_page(editing: &Rc<Editing>, suggest: bool) -> (gtk::Box, Rc<FolderPicker>) {
    let page = page();
    page.append(&heading("Folders to search"));
    page.append(&note(
        "ShowMe reads what's in these folders and keeps up as files come and go. Open a folder to leave parts of it out.",
    ));
    let mut selection = editing.config.borrow().folder_selection(false);
    if suggest && selection.chosen().is_empty() {
        for folder in crate::folders::suggestions(false) {
            selection.set_folder(&folder, true);
        }
        editing.change(|config| config.set_folder_selection(false, &selection));
    }
    let picker = FolderPicker::new(selection, false);
    let changes = editing.clone();
    picker.connect_changed(move |selection| changes.change(|config| config.set_folder_selection(false, selection)));
    page.append(&picker.widget);
    keep(&page, picker.clone());
    (page, picker)
}

fn reading_page(editing: &Rc<Editing>) -> gtk::Box {
    let page = page();
    let config = editing.config.borrow().clone();
    page.append(&heading("What ShowMe reads"));
    page.append(&row(
        "Source code and data files as documents",
        "Code outside the code folders, read as text. Code search reads code better.",
        &switch(editing, config.include_code, |config, on| config.include_code = on),
    ));
    page.append(&row(
        "Folders that look like datasets",
        "Thousands of numbered images or files of one kind are left out unless this is on.",
        &switch(editing, config.include_datasets, |config, on| config.include_datasets = on),
    ));
    page.append(&row(
        "File types to leave out",
        "Extensions, with commas between: iso, heic",
        &entry(editing, &config.excluded_extensions.join(", "), "none", |config, text| {
            config.excluded_extensions = text
                .split([',', ' '])
                .map(|word| word.trim_start_matches('.').to_string())
                .filter(|w| !w.is_empty())
                .collect()
        }),
    ));
    page.append(&heading("Text in screenshots"));
    let languages = ocr_languages();
    if languages.is_empty() {
        page.append(&note(
            "Tesseract isn't installed, so text in screenshots isn't read: sudo pacman -S tesseract tesseract-data-eng",
        ));
        return page;
    }
    page.append(&note(
        "The languages Tesseract reads in. Each one more makes reading slower. None checked: the ones ShowMe knows.",
    ));
    let flow = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(6).build();
    for language in languages {
        let check = gtk::CheckButton::with_label(&language);
        check.set_active(config.ocr_languages.contains(&language));
        let editing = editing.clone();
        check.connect_toggled(move |check| {
            let on = check.is_active();
            let language = language.clone();
            editing.change(move |config| {
                config.ocr_languages.retain(|have| *have != language);
                if on {
                    config.ocr_languages.push(language);
                }
            });
        });
        flow.append(&check);
    }
    page.append(&flow);
    page
}

/// The editors to offer: "auto", and those installed.
fn editors() -> Vec<(String, String)> {
    let mut options = vec![("Automatic ($EDITOR, or what's installed)".to_string(), "auto".to_string())];
    let known = [
        ("Neovim", "nvim"),
        ("Helix", "hx"),
        ("Zed", "zeditor"),
        ("Zed", "zed"),
        ("VS Code", "code"),
        ("Cursor", "cursor"),
        ("VSCodium", "codium"),
        ("Sublime Text", "subl"),
        ("Emacs", "emacsclient"),
        ("Kate", "kate"),
        ("Vim", "vim"),
    ];
    for (name, program) in known {
        if crate::open::have(program) && !options.iter().any(|(shown, _)| shown == name) {
            options.push((name.to_string(), program.to_string()));
        }
    }
    options
}

pub fn code_page(editing: &Rc<Editing>) -> gtk::Box {
    let page = page();
    let config = editing.config.borrow().clone();
    page.append(&heading("Code search"));
    page.append(&note(
        "Search your projects by what the code does: start a search with code: in the panel. Git's ignore rules are followed.",
    ));
    let picker = FolderPicker::new(config.folder_selection(true), true);
    let on = gtk::Switch::builder().active(!config.code_folders.is_empty()).build();
    picker.widget.set_sensitive(on.is_active());
    {
        let (editing, picker) = (editing.clone(), picker.clone());
        on.connect_active_notify(move |on| {
            picker.widget.set_sensitive(on.is_active());
            let selection = if on.is_active() { picker.selection() } else { Default::default() };
            editing.change(|config| config.set_folder_selection(true, &selection));
        });
    }
    {
        let (editing, on) = (editing.clone(), on.clone());
        picker.connect_changed(move |selection| {
            if on.is_active() {
                editing.change(|config| config.set_folder_selection(true, selection));
            }
        });
    }
    page.append(&row("Search code", "", &on));
    page.append(&picker.widget);
    keep(&page, picker);
    page.append(&row(
        "Open code in",
        "Results open at the line that matched.",
        &choice(editing, &editors(), &config.open.code, |config, editor| config.open.code = editor.to_string()),
    ));
    page.append(&row(
        "Read changes after (minutes)",
        "Code is read again once the folders have been quiet this long, so saves while you type don't each cost a pass.",
        &minutes(editing, config.code_quiet_seconds, |config, seconds| config.code_quiet_seconds = seconds),
    ));
    page
}

fn opening_page(editing: &Rc<Editing>) -> gtk::Box {
    let page = page();
    let config = editing.config.borrow().clone();
    page.append(&heading("Opening results"));
    page.append(&note(
        "“auto” uses the file's default app, at the moment or page that matched when it's one ShowMe knows (mpv, VLC, Celluloid, Papers, Evince, Zathura, Okular). Or write a command with {file}, {page}, {seconds} or {line} in it.",
    ));
    page.append(&row(
        "Videos",
        "",
        &entry(editing, &config.open.video, "auto", |config, text| config.open.video = or_auto(text)),
    ));
    page.append(&row(
        "Recordings",
        "",
        &entry(editing, &config.open.audio, "auto", |config, text| config.open.audio = or_auto(text)),
    ));
    page.append(&row(
        "PDFs",
        "",
        &entry(editing, &config.open.pdf, "auto", |config, text| config.open.pdf = or_auto(text)),
    ));
    page.append(&heading("Looks"));
    let themes =
        [("Omarchy's theme".to_string(), "omarchy".to_string()), ("GTK's theme".to_string(), "gtk".to_string())];
    page.append(&row(
        "Colors",
        "Omarchy's follow the theme you pick in Omarchy, as Walker's do.",
        &choice(editing, &themes, &config.panel.theme, |config, theme| config.panel.theme = theme.to_string()),
    ));
    page.append(&row(
        "Font",
        "Empty: the same as Walker. Takes effect when the panel starts again.",
        &entry(editing, &config.panel.font, "as Walker", |config, text| config.panel.font = text.to_string()),
    ));
    page
}

fn or_auto(text: &str) -> String {
    if text.is_empty() { "auto".into() } else { text.into() }
}

fn power_page(editing: &Rc<Editing>) -> gtk::Box {
    let page = page();
    let config = editing.config.borrow().clone();
    page.append(&heading("Power and memory"));
    page.append(&row(
        "Read on battery",
        "The first big pass also runs on battery. New files are always read right away.",
        &switch(editing, config.index_on_battery, |config, on| config.index_on_battery = on),
    ));
    page.append(&row(
        "Wait in power saver",
        "The first big pass waits while the power profile is power saver.",
        &switch(editing, config.pause_in_power_saver, |config, on| config.pause_in_power_saver = on),
    ));
    page.append(&row(
        "Keep the reading model (minutes)",
        "The model that reads files (about 1 GB) is let go after this long with nothing to read.",
        &minutes(editing, config.linger_seconds, |config, seconds| config.linger_seconds = seconds),
    ));
    page.append(&row(
        "Keep the search model (minutes)",
        "The model that reads what you search for (about 0.25 GB) is let go after this long without a search.",
        &minutes(editing, config.encoder_idle_seconds, |config, seconds| config.encoder_idle_seconds = seconds),
    ));
    page
}

/// The first pass over the folders, at the top of their page: how far it has come and how long is left, with pause
/// and the steps it's read in.
struct Progress {
    widget: gtk::Box,
    label: gtk::Label,
    bar: gtk::ProgressBar,
    /// The file being read now (and how far into it, for a long one).
    current: gtk::Label,
    pause: gtk::Button,
    paused: Cell<bool>,
    /// (when, files read) over the last two minutes: the pace the time left is reckoned from.
    samples: RefCell<VecDeque<(Instant, usize)>>,
}

impl Progress {
    fn new(daemon: &Daemon, editing: &Rc<Editing>) -> Rc<Progress> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 6);
        widget.add_css_class("showme-progress");
        let label = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let bar = gtk::ProgressBar::new();
        let current = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::Middle).build();
        current.add_css_class("showme-sub");
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let pause = gtk::Button::with_label("Pause");
        let steps =
            [("All at once", "0"), ("15 minutes", "15"), ("30 minutes", "30"), ("1 hour", "60"), ("2 hours", "120")]
                .map(|(shown, stored)| (shown.to_string(), stored.to_string()));
        let now = format!("{}", editing.config.borrow().step_minutes.round() as i64);
        let step = choice(editing, &steps, &now, |config, stored| config.step_minutes = stored.parse().unwrap_or(0.0));
        let step_label = gtk::Label::new(Some("Read in steps of"));
        step_label.set_margin_start(12);
        step_label.add_css_class("showme-sub");
        controls.append(&pause);
        controls.append(&step_label);
        controls.append(&step);
        widget.append(&label);
        widget.append(&bar);
        widget.append(&current);
        widget.append(&controls);
        widget.append(&note(
            "A big folder can take hours the first time. With steps, ShowMe pauses after each one and tells you; \
             press Resume for the next.",
        ));
        let progress = Rc::new(Progress {
            widget,
            label,
            bar,
            current,
            pause,
            paused: Cell::new(false),
            samples: RefCell::new(VecDeque::new()),
        });
        let (weak, sender) = (Rc::downgrade(&progress), daemon.clone());
        progress.pause.connect_clicked(move |_| {
            if let Some(progress) = weak.upgrade() {
                sender.send(Line::Other, if progress.paused.get() { Request::Resume } else { Request::Pause });
            }
        });
        progress.widget.set_visible(false);
        progress
    }

    fn update(&self, status: Option<&Status>) {
        let Some(status) = status.filter(|status| status.files > 0) else {
            self.widget.set_visible(false);
            return;
        };
        self.widget.set_visible(true);
        let read = status.files.saturating_sub(status.pending);
        let paused = status.state == "paused";
        self.paused.set(paused);
        self.pause.set_label(if paused { "Resume" } else { "Pause" });
        self.pause.set_sensitive(status.pending > 0 || status.unfinished > 0);
        self.bar.set_fraction(read as f64 / status.files as f64);
        let share = 100 * read / status.files;
        let now = Instant::now();
        let mut samples = self.samples.borrow_mut();
        if paused || status.pending == 0 {
            samples.clear();
        } else {
            samples.push_back((now, read));
            while samples.front().is_some_and(|(when, _)| now.duration_since(*when) > Duration::from_secs(120)) {
                samples.pop_front();
            }
        }
        // The time left at the pace of the last minutes, once there's enough of it to go by.
        let left = match (samples.front(), samples.back()) {
            (Some((first, first_read)), Some((last, last_read)))
                if last.duration_since(*first) >= Duration::from_secs(15) && last_read > first_read =>
            {
                let pace = (last_read - first_read) as f64 / last.duration_since(*first).as_secs_f64();
                Some(estimator::rough_duration(status.pending as f64 / pace))
            }
            _ => None,
        };
        let text = if status.pending == 0 && status.unfinished > 0 {
            // The second pass: every file is in, by its start; long ones are read on to the end.
            let further = match status.state.as_str() {
                "paused" => "Reading further into long files is paused",
                "waiting" => "Reading further into long files is waiting",
                _ => "Reading further into long files",
            };
            let long =
                if status.unfinished == 1 { "1 file left".into() } else { format!("{} files left", status.unfinished) };
            format!(
                "✓ Every file is searchable: {} files, by name and how they begin.\n{further}: {long}.",
                status.files
            )
        } else if status.pending == 0 {
            format!("All {} files are read.", status.files)
        } else if paused && status.detail.as_deref() == Some("step") {
            format!("Paused after a step: {read} of {} files read ({share}%). Resume for the next step.", status.files)
        } else if paused {
            format!("Paused: {read} of {} files read ({share}%).", status.files)
        } else {
            let mut text = format!("Reading: {read} of {} files ({share}%)", status.files);
            match left {
                Some(left) => text.push_str(&format!(" · about {left} left")),
                None => text.push_str(" · working out the time left"),
            }
            text
        };
        self.label.set_text(&text);
        self.bar.set_visible(status.pending > 0);
        let reading = matches!(status.state.as_str(), "indexing" | "finishing");
        match status.current.as_deref().filter(|_| reading) {
            Some(path) => {
                let mut text = format!("Now: {}", short_path(path, status));
                if let Some(place) = &status.place {
                    let unit = if place.pages { "page" } else { "part" };
                    text.push_str(&format!(", {unit} {} of {}", place.at.min(place.of), place.of));
                }
                self.current.set_text(&text);
                self.current.set_visible(true);
            }
            None => self.current.set_visible(false),
        }
    }
}

/// What the daemon is doing, kept current, with pause and sync.
struct StatusPage {
    page: gtk::Box,
    state: gtk::Label,
    numbers: gtk::Label,
    pause: gtk::Button,
    paused: Cell<bool>,
}

impl StatusPage {
    fn new(daemon: &Daemon) -> Rc<StatusPage> {
        let page = page();
        page.append(&heading("ShowMe"));
        let state = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let numbers = note("");
        page.append(&state);
        page.append(&numbers);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let pause = gtk::Button::with_label("Pause");
        let sync = gtk::Button::with_label("Look for changes now");
        let setup = gtk::Button::with_label("Run setup again");
        buttons.append(&pause);
        buttons.append(&sync);
        buttons.append(&setup);
        page.append(&buttons);
        page.append(&note(&format!(
            "Settings: {}\nShowMe {} · files are read with EmbeddingGemma on llama.cpp, on this computer only.",
            showme_core::settings::shorten(&Config::file()),
            env!("CARGO_PKG_VERSION")
        )));
        let status = Rc::new(StatusPage { page, state, numbers, pause, paused: Cell::new(false) });
        let (weak, sender) = (Rc::downgrade(&status), daemon.clone());
        status.pause.connect_clicked(move |_| {
            if let Some(status) = weak.upgrade() {
                sender.send(Line::Other, if status.paused.get() { Request::Resume } else { Request::Pause });
            }
        });
        let sender = daemon.clone();
        sync.connect_clicked(move |_| sender.send(Line::Other, Request::Sync));
        setup.connect_clicked(|button| {
            if let Some(app) = button.root().and_downcast::<gtk::Window>().and_then(|window| window.application()) {
                crate::setup::open(&app);
            }
        });
        status
    }

    fn update(&self, status: Option<&Status>) {
        let Some(status) = status else {
            self.state.set_text("ShowMe's daemon isn't running. Start it with: systemctl --user enable --now showme");
            self.numbers.set_text("");
            self.pause.set_sensitive(false);
            return;
        };
        self.pause.set_sensitive(true);
        self.paused.set(status.state == "paused");
        self.pause.set_label(if status.state == "paused" { "Resume" } else { "Pause" });
        self.state.set_text(&describe(status));
        let mut numbers = format!("{} files searchable · {} vectors", status.searchable, status.vectors);
        if status.has_code {
            numbers.push_str(&format!(" · {} code files", status.code_files));
        }
        if !status.missing.is_empty() {
            numbers.push_str(&format!(
                "\nNot there right now (their files stay searchable): {}",
                status.missing.join(", ")
            ));
        }
        if let Some(error) = &status.last_error {
            numbers.push_str(&format!("\nLast problem: {error}"));
        }
        self.numbers.set_text(&numbers);
    }
}

/// A file's path from inside its chosen folder ("Bøker/Dune.pdf"), or from home.
fn short_path(path: &str, status: &Status) -> String {
    let root = status
        .folders
        .iter()
        .map(|folder| folder.path.trim_end_matches('/'))
        .filter(|root| path.strip_prefix(root).is_some_and(|rest| rest.starts_with('/')))
        .max_by_key(|root| root.len());
    match root {
        Some(root) => {
            let name = root.rsplit('/').next().unwrap_or(root);
            format!("{name}{}", &path[root.len()..])
        }
        None => crate::results::tilde(path),
    }
}

/// "Reading 120 of 3 400 files…", as a sentence.
pub fn describe(status: &Status) -> String {
    let detail = status.detail.as_deref().unwrap_or("");
    match status.state.as_str() {
        "finishing" if status.total > 0 => format!(
            "Every file is searchable. Reading further into {} long files: {} of {} pages and parts done.",
            status.unfinished, status.done, status.total
        ),
        "finishing" => format!("Every file is searchable. Reading further into {} long files.", status.unfinished),
        "indexing" if status.total > 0 => format!("Reading files: {} of {} done.", status.done, status.total),
        "indexing" => "Reading files.".into(),
        "syncing" => "Looking for changes.".into(),
        "starting" => "Starting.".into(),
        "paused" => "Paused.".into(),
        "waiting" if detail == "model" => "Waiting for the model to download.".into(),
        "waiting" => format!("Waiting ({detail})."),
        "error" => format!("Stopped: {detail}"),
        _ if status.pending > 0 => format!("{} files still to read.", status.pending),
        _ => "Up to date.".into(),
    }
}

/// Opens the settings (or brings the open window forward).
pub fn open(app: &gtk::Application) {
    if let Some(window) = app.windows().into_iter().find(|window| window.widget_name() == "showme-settings") {
        window.present();
        return;
    }
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("ShowMe Settings")
        .default_width(860)
        .default_height(640)
        .build();
    window.set_widget_name("showme-settings");
    window.add_css_class("showme-settings");
    let (events, incoming) = async_channel::unbounded();
    let daemon = Daemon::start(events);
    let editing = Editing::new(daemon.clone());
    let status = StatusPage::new(&daemon);
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).hexpand(true).build();
    let scrolled = |child: &gtk::Box| {
        gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(child).build()
    };
    let progress = Progress::new(&daemon, &editing);
    let (folders, picker) = folders_page(&editing, false);
    // Under the page's heading and note, above the folders.
    folders.insert_child_after(&progress.widget, picker.widget.prev_sibling().as_ref());
    stack.add_titled(&scrolled(&folders), Some("folders"), "Folders");
    stack.add_titled(&scrolled(&reading_page(&editing)), Some("reading"), "Reading");
    stack.add_titled(&scrolled(&code_page(&editing)), Some("code"), "Code");
    stack.add_titled(&scrolled(&crate::collections::collections_page(&editing)), Some("collections"), "Collections");
    stack.add_titled(&scrolled(&opening_page(&editing)), Some("opening"), "Opening & Looks");
    stack.add_titled(&scrolled(&power_page(&editing)), Some("power"), "Power");
    stack.add_titled(&scrolled(&status.page), Some("status"), "Status");
    let sidebar = gtk::StackSidebar::builder().stack(&stack).width_request(190).build();
    let columns = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    columns.append(&sidebar);
    columns.append(&stack);
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.append(&columns);
    columns.set_vexpand(true);
    let saved =
        gtk::Label::builder().xalign(0.0).margin_start(12).margin_end(12).margin_top(6).margin_bottom(6).build();
    saved.add_css_class("showme-sub");
    saved.set_text("Changes are saved as you make them.");
    frame.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    frame.append(&saved);
    window.set_child(Some(&frame));
    editing.connect_saved(move |result| match result {
        Ok(()) => saved.set_text("Saved. ShowMe is reading the settings again."),
        Err(error) => saved.set_text(&format!("Couldn't save: {error}")),
    });
    if let Ok(page) = std::env::var("SHOWME_SETTINGS_PAGE") {
        stack.set_visible_child_name(&page);
    }
    let (weak, weak_progress, weak_picker) = (Rc::downgrade(&status), Rc::downgrade(&progress), Rc::downgrade(&picker));
    glib::spawn_future_local(async move {
        while let Ok(event) = incoming.recv().await {
            let Some(status) = weak.upgrade() else { break };
            if let Event::Status(now) = event {
                status.update(now.as_deref());
                if let Some(progress) = weak_progress.upgrade() {
                    progress.update(now.as_deref());
                }
                if let (Some(picker), Some(now)) = (weak_picker.upgrade(), now.as_deref()) {
                    picker.set_progress(&now.folders);
                }
            }
        }
    });
    window.connect_close_request(move |_| {
        editing.flush();
        let _ = (&status, &progress);
        glib::Propagation::Proceed
    });
    window.present();
}
