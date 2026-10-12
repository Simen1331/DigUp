//! `showme-panel`: ShowMe's search panel. It stays in memory with its window hidden; `showme-panel --toggle` (bound to
//! a key in Hyprland) shows or hides it in a moment, by handing the word to the running instance over D-Bus.
//!
//!   showme-panel            start (hidden, until --toggle), or show it if it's running
//!   showme-panel --toggle   show or hide the panel
//!   showme-panel --window   the big search window
//!   showme-panel --settings the settings
//!   showme-panel --setup    setup (it opens by itself the first time)

mod client;
mod folders;
mod integrate;
mod open;
mod panel;
mod pictures;
mod player;
mod preview;
mod results;
mod settings;
mod setup;
mod theme;
mod window;

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};

/// The panel, and the theme that keeps it styled.
type Started = (Rc<panel::Panel>, theme::Theme);

/// A hit as `showme search --json` prints it, as a row to open. That form leaves out what opening doesn't need.
fn hit_from_json(text: &str, query: &str) -> Result<results::ResultRow, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(text)?;
    if let Some(fields) = value.as_object_mut() {
        for (name, default) in [("strong", serde_json::json!(true)), ("modified", 0.into()), ("size", 0.into())] {
            fields.entry(name).or_insert(default);
        }
    }
    let hit: showme_core::proto::Hit = serde_json::from_value(value)?;
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    Ok(results::ResultRow::new(&hit, &terms))
}

fn main() -> glib::ExitCode {
    if std::env::args().any(|argument| argument == "--help" || argument == "-h") {
        println!(
            "showme-panel: ShowMe's search panel\n  --toggle   show or hide it\n  --window   the big search window\n  --settings the settings\n  --setup    set ShowMe up\n  --hidden   start without showing it\n  --open HIT [QUERY]  open a hit from `showme search --json` where it matched (the bar's search)\n  --window [QUERY]    the big search window, searching for QUERY"
        );
        return glib::ExitCode::SUCCESS;
    }
    let app = gtk::Application::builder()
        .application_id("dev.showme.Panel")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let state: Rc<RefCell<Option<Started>>> = Rc::new(RefCell::new(None));
    {
        let state = state.clone();
        app.connect_startup(move |app| {
            let settings = showme_core::settings::Config::load().unwrap_or_default();
            let theme = theme::load(&settings.panel);
            let panel = panel::Panel::new(app);
            let show = gio::SimpleAction::new("show-panel", None);
            let shown = Rc::downgrade(&panel);
            show.connect_activate(move |_, _| {
                if let Some(panel) = shown.upgrade() {
                    panel.present();
                }
            });
            app.add_action(&show);
            *state.borrow_mut() = Some((panel, theme));
            // It stays in memory with the window hidden: that's what makes it show in a moment.
            std::mem::forget(app.hold());
        });
    }
    app.connect_command_line(move |app, command_line| {
        let arguments: Vec<String> =
            command_line.arguments().iter().skip(1).map(|argument| argument.to_string_lossy().into_owned()).collect();
        let Some(panel) = state.borrow().as_ref().map(|(panel, _)| panel.clone()) else {
            return glib::ExitCode::FAILURE;
        };
        // The first time (no settings yet), setup comes first, however ShowMe was started.
        if !showme_core::settings::Config::file().exists()
            && arguments.first().is_none_or(|first| first != "--settings")
        {
            setup::open(app);
            return glib::ExitCode::SUCCESS;
        }
        match arguments.first().map(String::as_str) {
            Some("--toggle") => panel.toggle(),
            Some("--window") => window::open(app, arguments.get(1).map_or("", String::as_str)),
            Some("--open") => {
                let query = arguments.get(2).map_or("", String::as_str);
                match arguments.get(1).map(|hit| hit_from_json(hit, query)) {
                    Some(Ok(row)) => open::open(&row, &showme_core::settings::Config::load().unwrap_or_default().open),
                    Some(Err(error)) => {
                        eprintln!("showme-panel --open: {error}");
                        return glib::ExitCode::FAILURE;
                    }
                    None => return glib::ExitCode::FAILURE,
                }
            }
            Some("--settings") => settings::open(app),
            Some("--setup") => setup::open(app),
            Some("--hidden") => {}
            _ => panel.present(),
        }
        glib::ExitCode::SUCCESS
    });
    app.run()
}
