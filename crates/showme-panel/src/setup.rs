//! Setup (`showme-panel --setup`, and by itself the first time): the model, the folders, code, the programs ShowMe
//! reads with, a key and the bar (Omarchy's, or Waybar). Each step can be skipped, and Settings changes all of it later.

use std::cell::Cell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use showme_core::proto::{Request, Status};

use crate::client::{Daemon, Event, Line};
use crate::integrate;
use crate::settings::{self, Editing, copy, heading, keep, note, page};

const STEPS: usize = 6;

/// The first step: what ShowMe is, the daemon, and the model's download.
struct Welcome {
    page: gtk::Box,
    state: gtk::Label,
    start: gtk::Button,
    download: gtk::Button,
    progress: gtk::ProgressBar,
}

impl Welcome {
    fn new(daemon: &Daemon) -> Rc<Welcome> {
        let page = page();
        page.append(&heading("Welcome to ShowMe"));
        page.append(&note(
            "ShowMe finds your files by what's in them: a photo by what's in it, a PDF by what it says, a recording by what was said. Everything is read on this computer, and nothing leaves it.\n\nIt reads with EmbeddingGemma, a model of about 865 MB that it downloads once.",
        ));
        let state = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let progress = gtk::ProgressBar::builder().show_text(true).visible(false).build();
        let start = gtk::Button::with_label("Start ShowMe in the background");
        let download = gtk::Button::with_label("Download the model");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.append(&start);
        buttons.append(&download);
        page.append(&state);
        page.append(&progress);
        page.append(&buttons);
        let welcome = Rc::new(Welcome { page, state, start, download, progress });
        let weak = Rc::downgrade(&welcome);
        welcome.start.connect_clicked(move |button| {
            button.set_sensitive(false);
            let weak = weak.clone();
            glib::spawn_future_local(async move {
                let result =
                    gtk::gio::spawn_blocking(|| integrate::enable_services().map_err(|e| format!("{e:#}"))).await;
                if let (Some(welcome), Ok(Err(error))) = (weak.upgrade(), result) {
                    welcome.state.set_text(&error);
                    welcome.start.set_sensitive(true);
                }
            });
        });
        let sender = daemon.clone();
        welcome.download.connect_clicked(move |button| {
            button.set_sensitive(false);
            sender.send(Line::Other, Request::Download);
        });
        welcome.update(None);
        welcome
    }

    fn update(&self, status: Option<&Status>) {
        let Some(status) = status else {
            self.state.set_text("ShowMe's daemon isn't running yet: it reads your files in the background.");
            self.start.set_visible(true);
            self.download.set_visible(false);
            self.progress.set_visible(false);
            return;
        };
        self.start.set_visible(false);
        let model = &status.model;
        self.download.set_visible(model.state == "missing" || model.state == "failed");
        self.download.set_sensitive(true);
        self.progress.set_visible(model.state == "downloading");
        self.progress.set_fraction(model.fraction.clamp(0.0, 1.0));
        self.progress.set_text(model.text.as_deref());
        self.state.set_text(&match model.state.as_str() {
            "ready" => "✓ ShowMe is running, and the model is here.".to_string(),
            "downloading" => "Downloading the model. You can go on meanwhile.".to_string(),
            "failed" => format!("The download stopped: {}", model.text.as_deref().unwrap_or("try again")),
            _ => "ShowMe is running. It needs the model before it can read your files.".to_string(),
        });
        if model.state == "downloading" {
            self.download.set_visible(false);
        }
    }
}

/// The programs ShowMe reads with: which are there, and the line that installs the rest.
fn programs_page() -> gtk::Box {
    let page = page();
    page.append(&heading("Programs ShowMe reads with"));
    page.append(&note(
        "ShowMe works without them, but reads less: these add screenshots' text, recordings and videos.",
    ));
    let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page.append(&list);
    let command = gtk::Label::builder().xalign(0.0).selectable(true).wrap(true).build();
    command.add_css_class("showme-code");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let copy_button = gtk::Button::with_label("Copy");
    let again = gtk::Button::with_label("Check again");
    buttons.append(&copy_button);
    buttons.append(&again);
    page.append(&command);
    page.append(&buttons);
    let fill = {
        let (list, command, copy_button) = (list.clone(), command.clone(), copy_button.clone());
        move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let needs = integrate::needs();
            for need in &needs {
                let mark = if need.there { "✓" } else { "✗" };
                let line = settings::row(&format!("{mark}  {}", need.name), need.why, &gtk::Label::new(None));
                list.append(&line);
            }
            match integrate::install_command(&needs) {
                Some(line) => {
                    command.set_text(&line);
                    copy_button.set_visible(true);
                }
                None => {
                    command.set_text("All there.");
                    copy_button.set_visible(false);
                }
            }
        }
    };
    fill();
    let shown = command.clone();
    copy_button.connect_clicked(move |_| copy(&shown.text()));
    again.connect_clicked(move |_| fill());
    page
}

/// The key that shows the panel, and ShowMe in the bar (Omarchy's, or Waybar). "Add for me" copies the file aside and adds the lines.
fn desktop_page() -> gtk::Box {
    let page = page();
    page.append(&heading("A key for ShowMe"));
    let bound = integrate::bound_key();
    let key = gtk::Entry::builder().text(bound.as_deref().unwrap_or("SUPER, D")).width_chars(18).build();
    let file = showme_core::settings::shorten(&integrate::hyprland_file());
    page.append(&note(&format!(
        "Hyprland shows the panel with this key. It's written as Hyprland writes keys (SUPER SHIFT, F), and goes in {file}."
    )));
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    line.append(&key);
    let add = gtk::Button::with_label("Add for me");
    let copy_key = gtk::Button::with_label("Copy the line");
    line.append(&add);
    line.append(&copy_key);
    page.append(&line);
    let said = note("");
    page.append(&said);
    let check = {
        let (key, add, said) = (key.clone(), add.clone(), said.clone());
        let bound = bound.clone();
        move || {
            let text = key.text();
            if let Some(bound) = &bound {
                said.set_text(&format!("✓ ShowMe has a key already: {bound}."));
                add.set_sensitive(false);
            } else if !integrate::valid_key(&text) {
                said.set_text("Write the key as SUPER, D or SUPER SHIFT, F.");
                add.set_sensitive(false);
            } else if let Some(used) = integrate::key_in_use(&text) {
                said.set_text(&format!("That key does something already: {used}. Pick another."));
                add.set_sensitive(false);
            } else {
                said.set_text("");
                add.set_sensitive(true);
            }
        }
    };
    check();
    let recheck = check.clone();
    key.connect_changed(move |_| recheck());
    {
        let (key, said) = (key.clone(), said.clone());
        add.connect_clicked(move |button| match integrate::add_key(&key.text()) {
            Ok(backup) => {
                let kept = backup
                    .map(|path| format!(" The file as it was is in {}.", showme_core::settings::shorten(&path)))
                    .unwrap_or_default();
                said.set_text(&format!("✓ Added: {} shows ShowMe now.{kept}", key.text()));
                button.set_sensitive(false);
            }
            Err(error) => said.set_text(&format!("{error:#}")),
        });
    }
    let shown = key.clone();
    copy_key.connect_clicked(move |_| copy(&integrate::bind_line(&shown.text())));

    if integrate::omarchy_bar() {
        omarchy_bar_section(&page);
    } else {
        waybar_section(&page);
    }
    page
}

/// ShowMe's widget in Omarchy's own bar (Omarchy 4).
fn omarchy_bar_section(page: &gtk::Box) {
    page.append(&heading("ShowMe in the bar"));
    page.append(&note(
        "While ShowMe reads files, the bar shows how far it has got; when all is read it hides. Click it to search, middle-click to pause. It goes in ~/.config/omarchy/plugins/showme, at the right of the bar.",
    ));
    let add = gtk::Button::with_label("Add for me");
    add.set_halign(gtk::Align::Start);
    page.append(&add);
    let said = note("");
    page.append(&said);
    if integrate::in_omarchy_bar() {
        said.set_text("✓ ShowMe is in the bar already.");
        add.set_sensitive(false);
    }
    add.connect_clicked(move |button| match integrate::add_to_omarchy_bar() {
        Ok(copies) => {
            let names: Vec<String> = copies.iter().map(|path| showme_core::settings::shorten(path)).collect();
            let kept = if names.is_empty() {
                String::new()
            } else {
                format!(" The bar's settings as they were: {}", names.join(", "))
            };
            said.set_text(&format!("✓ Added to the bar.{kept}"));
            button.set_sensitive(false);
        }
        Err(error) => said.set_text(&format!("{error:#}")),
    });
}

/// ShowMe's module in Waybar.
fn waybar_section(page: &gtk::Box) {
    page.append(&heading("ShowMe in Waybar"));
    page.append(&note(
        "While ShowMe reads files, Waybar shows how far it has got; when all is read it hides. Click it to search, middle-click to pause.",
    ));
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let add = gtk::Button::with_label("Add for me");
    let copy_module = gtk::Button::with_label("Copy the module");
    line.append(&add);
    line.append(&copy_module);
    page.append(&line);
    let said = note("");
    page.append(&said);
    if integrate::in_waybar() {
        said.set_text("✓ ShowMe is in Waybar already.");
        add.set_sensitive(false);
    } else if !integrate::waybar_file().exists() {
        said.set_text("Waybar's config isn't in ~/.config/waybar/config.jsonc: copy the module in by hand.");
        add.set_sensitive(false);
    }
    add.connect_clicked(move |button| match integrate::add_to_waybar() {
        Ok(copies) => {
            let names: Vec<String> = copies.iter().map(|path| showme_core::settings::shorten(path)).collect();
            said.set_text(&format!("✓ Added, and Waybar restarted. The files as they were: {}", names.join(", ")));
            button.set_sensitive(false);
        }
        Err(error) => said.set_text(&format!("{error:#}")),
    });
    copy_module.connect_clicked(|_| copy(integrate::WAYBAR_MODULE));
}

fn done_page() -> gtk::Box {
    let page = page();
    page.append(&heading("That's it"));
    let key = integrate::bound_key().map(|key| key.replace(", ", "+")).unwrap_or_else(|| "your key".into());
    page.append(&note(&format!(
        "ShowMe reads your folders now, newest files first, and keeps up as they change. Press {key} to search; what's read already is searchable straight away.\n\nIn the panel: ↑↓ to move, Enter opens, Space plays, Ctrl+C copies the file, Ctrl+O opens the big window, Alt+1–7 picks a kind, Ctrl+, opens the settings. Start with code: to search code.\n\nSettings: showme-panel --settings"
    )));
    let search = gtk::Button::with_label("Search now");
    search.set_halign(gtk::Align::Start);
    search.connect_clicked(|button| {
        if let Some(window) = button.root().and_downcast::<gtk::Window>() {
            if let Some(app) = window.application() {
                app.activate_action("show-panel", None);
            }
            window.close();
        }
    });
    page.append(&search);
    page
}

/// Opens setup (or brings it forward).
pub fn open(app: &gtk::Application) {
    if let Some(window) = app.windows().into_iter().find(|window| window.widget_name() == "showme-setup") {
        window.present();
        return;
    }
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Set up ShowMe")
        .default_width(760)
        .default_height(620)
        .build();
    window.set_widget_name("showme-setup");
    window.add_css_class("showme-settings");
    let (events, incoming) = async_channel::unbounded();
    let daemon = Daemon::start(events);
    let editing = Editing::new(daemon.clone());
    let welcome = Welcome::new(&daemon);
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeftRight).vexpand(true).build();
    let scrolled = |child: &gtk::Box| {
        gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(child).build()
    };
    let code = settings::code_page(&editing);
    code.prepend(&note("Optional: skip this when you don't write code."));
    let pages = [
        scrolled(&welcome.page),
        scrolled(&settings::folders_page(&editing, true).0),
        scrolled(&code),
        scrolled(&programs_page()),
        scrolled(&desktop_page()),
        scrolled(&done_page()),
    ];
    for (index, page) in pages.iter().enumerate() {
        stack.add_named(page, Some(&index.to_string()));
    }
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bottom.set_margin_top(10);
    bottom.set_margin_bottom(10);
    bottom.set_margin_start(16);
    bottom.set_margin_end(16);
    let back = gtk::Button::with_label("Back");
    let step = gtk::Label::builder().hexpand(true).build();
    step.add_css_class("showme-sub");
    let next = gtk::Button::with_label("Next");
    next.add_css_class("suggested-action");
    bottom.append(&back);
    bottom.append(&step);
    bottom.append(&next);
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.append(&stack);
    frame.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    frame.append(&bottom);
    window.set_child(Some(&frame));

    let at = Rc::new(Cell::new(0usize));
    let go = {
        let (stack, back, next, step, at) = (stack.clone(), back.clone(), next.clone(), step.clone(), at.clone());
        let editing = editing.clone();
        move |to: usize| {
            // Leaving the folders or code: save, so reading starts while setup goes on.
            editing.flush();
            at.set(to);
            stack.set_visible_child_name(&to.to_string());
            back.set_sensitive(to > 0);
            next.set_label(if to + 1 == STEPS { "Done" } else { "Next" });
            step.set_text(&format!("{} of {STEPS}", to + 1));
        }
    };
    go(std::env::var("SHOWME_SETUP_STEP").ok().and_then(|step| step.parse().ok()).unwrap_or(0).min(STEPS - 1));
    {
        let (go, at) = (go.clone(), at.clone());
        back.connect_clicked(move |_| go(at.get().saturating_sub(1)));
    }
    {
        let (window, editing) = (window.clone(), editing.clone());
        next.connect_clicked(move |_| {
            if at.get() + 1 < STEPS {
                go(at.get() + 1);
            } else {
                // The file is there from now on, so setup doesn't open by itself again.
                editing.save();
                window.close();
            }
        });
    }
    let weak = Rc::downgrade(&welcome);
    glib::spawn_future_local(async move {
        while let Ok(event) = incoming.recv().await {
            let Some(welcome) = weak.upgrade() else { break };
            if let Event::Status(status) = event {
                welcome.update(status.as_deref());
            }
        }
    });
    keep(&window, welcome);
    window.connect_close_request(move |_| {
        editing.flush();
        glib::Propagation::Proceed
    });
    window.present();
}
