//! `digger-panel`: Digger's search panel. It stays in memory with its window hidden; `digger-panel --toggle` (bound to
//! a key in Hyprland) shows or hides it in a moment, by handing the word to the running instance over D-Bus.
//!
//!   digger-panel            start (hidden, until --toggle), or show it if it's running
//!   digger-panel --toggle   show or hide the panel
//!   digger-panel --window   the big search window
//!   digger-panel --settings the settings
//!   digger-panel --setup    setup (it opens by itself the first time)

mod client;
mod folders;
mod integrate;
mod open;
mod panel;
mod pictures;
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

fn main() -> glib::ExitCode {
    if std::env::args().any(|argument| argument == "--help" || argument == "-h") {
        println!(
            "digger-panel: Digger's search panel\n  --toggle   show or hide it\n  --window   the big search window\n  --settings the settings\n  --setup    set Digger up\n  --hidden   start without showing it"
        );
        return glib::ExitCode::SUCCESS;
    }
    let app = gtk::Application::builder()
        .application_id("dev.digger.Panel")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let state: Rc<RefCell<Option<Started>>> = Rc::new(RefCell::new(None));
    {
        let state = state.clone();
        app.connect_startup(move |app| {
            let settings = digger_core::settings::Config::load().unwrap_or_default();
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
        // The first time (no settings yet), setup comes first, however Digger was started.
        if !digger_core::settings::Config::file().exists()
            && arguments.first().is_none_or(|first| first != "--settings")
        {
            setup::open(app);
            return glib::ExitCode::SUCCESS;
        }
        match arguments.first().map(String::as_str) {
            Some("--toggle") => panel.toggle(),
            Some("--window") => window::open(app, ""),
            Some("--settings") => settings::open(app),
            Some("--setup") => setup::open(app),
            Some("--hidden") => {}
            _ => panel.present(),
        }
        glib::ExitCode::SUCCESS
    });
    app.run()
}
