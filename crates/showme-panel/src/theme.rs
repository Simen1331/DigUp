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
window.showme, window.showme-window {
  background: transparent;
}
.showme-frame {
  background-color: @base;
  color: @text;
  border: 2px solid @border;
  border-radius: 0;
}
.showme-frame entry.showme-query {
  background: transparent;
  border: none;
  box-shadow: none;
  outline: none;
  color: @text;
  font-size: 18px;
  padding: 10px 6px;
  min-height: 32px;
}
.showme-frame .showme-search-icon {
  color: alpha(@text, 0.6);
  margin-left: 14px;
}
.showme-frame .showme-badge {
  color: alpha(@text, 0.55);
  font-size: 12px;
  margin-right: 14px;
}
.showme-frame separator {
  background-color: alpha(@border, 0.35);
  min-height: 1px;
  min-width: 1px;
}
.showme-frame list, .showme-frame list > row, .showme-frame scrolledwindow, .showme-frame viewport {
  background: transparent;
  color: @text;
}
.showme-frame list > row {
  padding: 4px 10px;
  border-radius: 0;
}
.showme-frame list > row:selected {
  background-color: alpha(@selected-text, 0.18);
}
.showme-frame list > row:selected .showme-name {
  color: @selected-text;
}
.showme-group {
  color: alpha(@text, 0.5);
  font-size: 11px;
  font-weight: bold;
  letter-spacing: 1px;
  padding: 8px 10px 2px 10px;
}
.showme-name {
  font-weight: bold;
}
.showme-place {
  color: @selected-text;
  font-size: 12px;
}
.showme-sub, .showme-footer, .showme-details, .showme-folder {
  color: alpha(@text, 0.6);
  font-size: 12px;
}
.showme-footer {
  padding: 6px 14px;
}
.showme-thumb {
  border-radius: 3px;
  background-color: alpha(@text, 0.06);
}
.showme-preview {
  padding: 12px;
}
.showme-preview .showme-title {
  font-weight: bold;
  font-size: 14px;
}
.showme-preview .showme-reason {
  color: @selected-text;
}
.showme-preview button {
  background: alpha(@text, 0.08);
  color: @text;
  border: 1px solid alpha(@border, 0.4);
  border-radius: 0;
  padding: 4px 12px;
}
.showme-preview button:hover {
  background: alpha(@selected-text, 0.2);
}
.showme-passage, .showme-code {
  background-color: alpha(@text, 0.05);
  color: @text;
  padding: 8px;
}
.showme-passage text, .showme-code text {
  background: transparent;
  color: @text;
}
.showme-message {
  color: alpha(@text, 0.65);
  padding: 24px;
}
.showme-filter {
  background: transparent;
  color: alpha(@text, 0.6);
  border: none;
  border-radius: 0;
  padding: 2px 8px;
  font-size: 12px;
}
.showme-filter:checked {
  color: @selected-text;
  box-shadow: inset 0 -2px @selected-text;
}
.showme-tile {
  padding: 6px;
}
.showme-frame flowbox > flowboxchild {
  border-radius: 0;
}
.showme-frame flowbox > flowboxchild:selected {
  background-color: alpha(@selected-text, 0.18);
  color: @text;
}
.showme-frame flowbox > flowboxchild:selected .showme-name {
  color: @selected-text;
}
.showme-frame entry, .showme-frame searchentry, .showme-settings entry {
  background-color: alpha(@text, 0.06);
  color: @text;
  border: 1px solid alpha(@border, 0.4);
  border-radius: 0;
  box-shadow: none;
}
.showme-frame entry.showme-query {
  background: transparent;
  border: none;
}
window.showme-settings {
  background-color: @base;
  color: @text;
}
.showme-settings stacksidebar, .showme-settings stacksidebar list {
  background-color: alpha(@text, 0.03);
}
.showme-settings stacksidebar row:selected {
  background-color: alpha(@selected-text, 0.18);
  color: @selected-text;
}
.showme-heading {
  font-weight: bold;
  font-size: 15px;
  margin-top: 6px;
}
.showme-code {
  font-family: monospace;
  background-color: alpha(@text, 0.06);
  padding: 8px;
}
.showme-frame searchentry image, .showme-frame entry image {
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
pub fn load(settings: &showme_core::settings::PanelSettings) -> Theme {
    let display = gdk::Display::default().expect("no display");
    let defaults = gtk::CssProvider::new();
    defaults.load_from_string(DEFAULTS);
    gtk::style_context_add_provider_for_display(&display, &defaults, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION - 2);
    let colors = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(&display, &colors, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION - 1);
    let style = gtk::CssProvider::new();
    let family = font(&settings.font).replace('"', "");
    style.load_from_string(&format!(
        "{STYLE}\n.showme-frame, .showme-window-frame, window.showme-settings {{ font-family: \"{family}\"; }}"
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
