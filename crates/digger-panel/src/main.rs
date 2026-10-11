//! `digger-panel`: Digger's search panel. It stays in memory with its window hidden; `digger-panel --toggle` (bound to
//! a key in Hyprland) shows or hides it in a moment, by handing the word to the running instance over D-Bus.
//!
//!   digger-panel            start (hidden, until --toggle), or show it if it's running
//!   digger-panel --toggle   show or hide the panel
//!   digger-panel --window   the big search window

mod client;
mod open;
mod panel;
mod pictures;
mod preview;
mod results;
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
            "digger-panel: Digger's search panel\n  --toggle   show or hide it\n  --window   the big search window\n  --hidden   start without showing it"
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
        match arguments.first().map(String::as_str) {
            Some("--toggle") => panel.toggle(),
            Some("--window") => window::open(app, ""),
            Some("--hidden") => {}
            _ => panel.present(),
        }
        glib::ExitCode::SUCCESS
    });
    app.run()
}
