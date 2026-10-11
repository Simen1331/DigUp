//! How the panel looks: like Walker, in the colors of the current Omarchy theme.
//!
//! Omarchy keeps the active theme at `~/.config/omarchy/current/theme`, and its `walker.css` defines the colors Walker
//! uses (`@define-color base …`, `text`, `border`, `selected-text`…). The panel loads that file as it is, under its own
//! stylesheet, which only names those colors. Without Omarchy it falls back to a dark palette of its own. The theme
//! folder is watched: switching themes restyles an open panel.

use std::path::PathBuf;
use std::process::Command;

use gtk::prelude::*;
use gtk::{gdk, gio};

/// The colors used when there's no Omarchy theme (Tokyo Night, Omarchy's first theme).
const DEFAULTS: &str = "
@define-color base #1a1b26;
@define-color text #cfc9c2;
@define-color border #33ccff;
@define-color selected-text #7dcfff;
@define-color background #1a1b26;
@define-color foreground #cfc9c2;
";

/// The panel's own stylesheet: shapes and spacing, in the theme's colors.
const STYLE: &str = r#"
window.digger, window.digger-window {
  background: transparent;
}
.digger-frame {
  background-color: @base;
  color: @text;
  border: 2px solid @border;
  border-radius: 0;
}
.digger-frame entry.digger-query {
  background: transparent;
  border: none;
  box-shadow: none;
  outline: none;
  color: @text;
  font-size: 18px;
  padding: 10px 6px;
  min-height: 32px;
}
.digger-frame .digger-search-icon {
  color: alpha(@text, 0.6);
  margin-left: 14px;
}
.digger-frame .digger-badge {
  color: alpha(@text, 0.55);
  font-size: 12px;
  margin-right: 14px;
}
.digger-frame separator {
  background-color: alpha(@border, 0.35);
  min-height: 1px;
  min-width: 1px;
}
.digger-frame list, .digger-frame list > row, .digger-frame scrolledwindow, .digger-frame viewport {
  background: transparent;
  color: @text;
}
.digger-frame list > row {
  padding: 4px 10px;
  border-radius: 0;
}
.digger-frame list > row:selected {
  background-color: alpha(@selected-text, 0.18);
}
.digger-frame list > row:selected .digger-name {
  color: @selected-text;
}
.digger-group {
  color: alpha(@text, 0.5);
  font-size: 11px;
  font-weight: bold;
  letter-spacing: 1px;
  padding: 8px 10px 2px 10px;
}
.digger-name {
  font-weight: bold;
}
.digger-place {
  color: @selected-text;
  font-size: 12px;
}
.digger-sub, .digger-footer, .digger-details, .digger-folder {
  color: alpha(@text, 0.6);
  font-size: 12px;
}
.digger-footer {
  padding: 6px 14px;
}
.digger-thumb {
  border-radius: 3px;
  background-color: alpha(@text, 0.06);
}
.digger-preview {
  padding: 12px;
}
.digger-preview .digger-title {
  font-weight: bold;
  font-size: 14px;
}
.digger-preview .digger-reason {
  color: @selected-text;
}
.digger-preview button {
  background: alpha(@text, 0.08);
  color: @text;
  border: 1px solid alpha(@border, 0.4);
  border-radius: 0;
  padding: 4px 12px;
}
.digger-preview button:hover {
  background: alpha(@selected-text, 0.2);
}
.digger-passage, .digger-code {
  background-color: alpha(@text, 0.05);
  color: @text;
  padding: 8px;
}
.digger-passage text, .digger-code text {
  background: transparent;
  color: @text;
}
.digger-message {
  color: alpha(@text, 0.65);
  padding: 24px;
}
.digger-filter {
  background: transparent;
  color: alpha(@text, 0.6);
  border: none;
  border-radius: 0;
  padding: 2px 8px;
  font-size: 12px;
}
.digger-filter:checked {
  color: @selected-text;
  box-shadow: inset 0 -2px @selected-text;
}
.digger-tile {
  padding: 6px;
}
.digger-frame flowbox > flowboxchild {
  border-radius: 0;
}
.digger-frame flowbox > flowboxchild:selected {
  background-color: alpha(@selected-text, 0.18);
  color: @text;
}
.digger-frame flowbox > flowboxchild:selected .digger-name {
  color: @selected-text;
}
.digger-frame entry, .digger-frame searchentry, .digger-settings entry {
  background-color: alpha(@text, 0.06);
  color: @text;
  border: 1px solid alpha(@border, 0.4);
  border-radius: 0;
  box-shadow: none;
}
.digger-frame entry.digger-query {
  background: transparent;
  border: none;
}
window.digger-settings {
  background-color: @base;
  color: @text;
}
.digger-settings stacksidebar, .digger-settings stacksidebar list {
  background-color: alpha(@text, 0.03);
}
.digger-settings stacksidebar row:selected {
  background-color: alpha(@selected-text, 0.18);
  color: @selected-text;
}
.digger-heading {
  font-weight: bold;
  font-size: 15px;
  margin-top: 6px;
}
.digger-code {
  font-family: monospace;
  background-color: alpha(@text, 0.06);
  padding: 8px;
}
.digger-frame searchentry image, .digger-frame entry image {
  color: alpha(@text, 0.6);
}
"#;

/// The active Omarchy theme's folder, when there is one.
fn omarchy_theme() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let folder = home.join(".config/omarchy/current/theme");
    folder.exists().then_some(folder)
}

/// The font Walker and the terminal use (`fc-match monospace`), unless the settings name one.
fn font(chosen: &str) -> String {
    if !chosen.is_empty() {
        return chosen.to_string();
    }
    Command::new("fc-match")
        .args(["-f", "%{family[0]}", "monospace"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|family| !family.is_empty())
        .unwrap_or_else(|| "monospace".into())
}

/// Keeps the theme folder watched.
pub struct Theme {
    _monitor: Option<gio::FileMonitor>,
}

/// Loads the stylesheets for the whole app; keeps them in step with the Omarchy theme.
pub fn load(settings: &digger_core::settings::PanelSettings) -> Theme {
    let display = gdk::Display::default().expect("no display");
    let defaults = gtk::CssProvider::new();
    defaults.load_from_string(DEFAULTS);
    gtk::style_context_add_provider_for_display(&display, &defaults, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION - 2);
    let colors = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(&display, &colors, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION - 1);
    let style = gtk::CssProvider::new();
    let family = font(&settings.font).replace('"', "");
    style.load_from_string(&format!(
        "{STYLE}\n.digger-frame, .digger-window-frame, window.digger-settings {{ font-family: \"{family}\"; }}"
    ));
    gtk::style_context_add_provider_for_display(&display, &style, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

    let use_omarchy = settings.theme != "gtk";
    let reload = {
        let colors = colors.clone();
        move || {
            let theme = omarchy_theme().filter(|_| use_omarchy);
            // Buttons, switches and lists follow the theme's mode (Omarchy marks light themes with `light.mode`).
            if let (Some(folder), Some(settings)) = (&theme, gtk::Settings::default()) {
                settings.set_gtk_application_prefer_dark_theme(!folder.join("light.mode").exists());
            }
            let walker = theme.map(|folder| folder.join("walker.css")).filter(|file| file.exists());
            match walker {
                Some(file) if use_omarchy => colors.load_from_path(file),
                _ => colors.load_from_string(""),
            }
        }
    };
    reload();
    // Omarchy switches themes by pointing `current/theme` elsewhere: watch `current`.
    let monitor = std::env::var_os("HOME")
        .map(|home| gio::File::for_path(PathBuf::from(home).join(".config/omarchy/current")))
        .and_then(|folder| folder.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE).ok());
    if let Some(monitor) = &monitor {
        monitor.connect_changed(move |_, _, _, _| reload());
    }
    Theme { _monitor: monitor }
}
