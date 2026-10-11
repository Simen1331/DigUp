//! Opening a result where it matched: a video or recording at its moment, a PDF at its page, code at its line, and
//! the rest in its default app. Plus showing it in the file manager and copying it.
//!
//! Programs are started through `uwsm app` when it's there (Omarchy starts apps that way), so they live on their own
//! and not as the panel's children.

use std::path::Path;
use std::process::{Command, Stdio};

use digger_core::model::FileKind;
use digger_core::settings::OpenSettings;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use crate::results::{ResultRow, clock};

/// Whether a program is on the PATH.
pub fn have(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|folder| folder.join(program).is_file()))
}

/// Starts a program on its own.
fn launch(args: &[String]) {
    let Some((program, rest)) = args.split_first() else { return };
    let mut command = if have("uwsm") {
        let mut command = Command::new("uwsm");
        command.args(["app", "--"]).arg(program).args(rest);
        command
    } else {
        let mut command = Command::new(program);
        command.args(rest);
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command
    };
    let _ = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// The terminal Omarchy opens programs in.
fn terminal() -> Vec<String> {
    if have("xdg-terminal-exec") {
        return vec!["xdg-terminal-exec".into()];
    }
    let chosen = std::env::var("TERMINAL").ok().filter(|terminal| !terminal.is_empty());
    let terminal =
        chosen.or_else(|| ["alacritty", "ghostty", "kitty", "foot"].iter().find(|t| have(t)).map(|t| t.to_string()));
    match terminal {
        Some(terminal) => vec![terminal, "-e".into()],
        None => vec!["xterm".into(), "-e".into()],
    }
}

/// A template from the settings ("mpv --start={seconds} {file}") with the result's place filled in.
fn template(template: &str, path: &str, line: u32, page: u32, seconds: f64) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in template.split_whitespace() {
        if word == "{terminal}" {
            words.extend(terminal());
            continue;
        }
        words.push(
            word.replace("{file}", path)
                .replace("{line}", &line.to_string())
                .replace("{page}", &page.to_string())
                .replace("{seconds}", &format!("{seconds:.1}")),
        );
    }
    words
}

/// The program a file opens in by default, by its executable's name ("mpv", "evince").
fn default_app(path: &str) -> Option<String> {
    let (content_type, _) = gio::content_type_guess(Some(path), None);
    let app = gio::AppInfo::default_for_type(&content_type, false)?;
    let executable = app.executable();
    executable.file_name().map(|name| name.to_string_lossy().into_owned())
}

/// An editor for code: (name shown, how to open a file at a line). `$EDITOR` first, then what's installed.
fn editor(chosen: &str) -> Option<(String, String)> {
    let known = |name: &str| -> Option<(&'static str, &'static str)> {
        Some(match name {
            "nvim" => ("Neovim", "{terminal} nvim +{line} {file}"),
            "vim" => ("Vim", "{terminal} vim +{line} {file}"),
            "hx" | "helix" => ("Helix", "{terminal} hx {file}:{line}"),
            "zed" => ("Zed", "zed {file}:{line}"),
            "zeditor" => ("Zed", "zeditor {file}:{line}"),
            "code" => ("VS Code", "code --goto {file}:{line}:1"),
            "cursor" => ("Cursor", "cursor --goto {file}:{line}:1"),
            "codium" => ("VSCodium", "codium --goto {file}:{line}:1"),
            "subl" => ("Sublime Text", "subl {file}:{line}"),
            "emacs" | "emacsclient" => ("Emacs", "emacsclient -n +{line} {file}"),
            "kate" => ("Kate", "kate -l {line} {file}"),
            "idea" => ("IntelliJ IDEA", "idea --line {line} {file}"),
            "rustrover" => ("RustRover", "rustrover --line {line} {file}"),
            "pycharm" => ("PyCharm", "pycharm --line {line} {file}"),
            _ => return None,
        })
    };
    if chosen.contains('{') {
        return Some(("your editor".into(), chosen.to_string()));
    }
    let wanted = if chosen.is_empty() || chosen == "auto" {
        std::env::var("EDITOR").ok().map(|editor| editor.rsplit('/').next().unwrap_or(&editor).to_string())
    } else {
        Some(chosen.to_string())
    };
    let candidates =
        wanted.into_iter().chain(["nvim", "zeditor", "zed", "code", "cursor", "hx", "subl"].map(String::from));
    candidates
        .filter(|name| have(name))
        .find_map(|name| known(&name).map(|(title, command)| (title.into(), command.into())))
}

/// How a recording opens at its moment: by the settings' template, or by the default app when it's one that can be
/// told a time. None: it opens at the start.
fn moment_command(row: &ResultRow, settings: &OpenSettings) -> Option<Vec<String>> {
    let seconds = row.moment().filter(|seconds| *seconds >= 1.0)?;
    let chosen = if row.kind == FileKind::Audio { &settings.audio } else { &settings.video };
    if chosen.contains('{') {
        return Some(template(chosen, &row.path, 1, 1, seconds));
    }
    let app = if chosen == "auto" { default_app(&row.path)? } else { chosen.clone() };
    let start = format!("{seconds:.1}");
    Some(match app.as_str() {
        "mpv" if row.kind == FileKind::Audio => {
            vec!["mpv".into(), "--force-window=yes".into(), format!("--start={start}"), row.path.clone()]
        }
        "mpv" => vec!["mpv".into(), format!("--start={start}"), row.path.clone()],
        "vlc" => vec!["vlc".into(), format!("--start-time={start}"), row.path.clone()],
        "celluloid" => vec!["celluloid".into(), format!("--mpv-start={start}"), row.path.clone()],
        _ => return None,
    })
}

/// How a PDF opens at its page, with a word from it to search for where the viewer can take one.
fn page_command(row: &ResultRow, settings: &OpenSettings) -> Option<Vec<String>> {
    let page = row.page().filter(|page| *page > 1)?;
    if settings.pdf.contains('{') {
        return Some(template(&settings.pdf, &row.path, 1, page, 0.0));
    }
    let app = if settings.pdf == "auto" { default_app(&row.path)? } else { settings.pdf.clone() };
    let word = row.found_words().into_iter().next();
    Some(match app.as_str() {
        "evince" | "papers" => {
            let mut command = vec![app.clone(), "-i".into(), page.to_string()];
            if let Some(word) = word {
                command.push(format!("--find={word}"));
            }
            command.push(row.path.clone());
            command
        }
        "zathura" => vec!["zathura".into(), "-P".into(), page.to_string(), row.path.clone()],
        "okular" => vec!["okular".into(), "-p".into(), page.to_string(), row.path.clone()],
        "atril" => vec!["atril".into(), "-i".into(), page.to_string(), row.path.clone()],
        "xreader" => vec!["xreader".into(), "-i".into(), page.to_string(), row.path.clone()],
        _ => return None,
    })
}

/// What Enter does, for the footer and the preview's button: "Open at 0:06", "Open at p. 3", "Open in Neovim".
pub fn open_title(row: &ResultRow, settings: &OpenSettings) -> String {
    if row.kind == FileKind::Code {
        return match editor(&settings.code) {
            Some((name, _)) => format!("Open in {name}"),
            None => "Open".into(),
        };
    }
    if moment_command(row, settings).is_some() {
        return format!("Open at {}", clock(row.moment().unwrap_or(0.0)));
    }
    if page_command(row, settings).is_some() {
        return format!("Open at p. {}", row.page().unwrap_or(1));
    }
    "Open".into()
}

/// Opens a result where it matched.
pub fn open(row: &ResultRow, settings: &OpenSettings) {
    let command = if row.kind == FileKind::Code {
        editor(&settings.code).map(|(_, command)| template(&command, &row.path, row.code_line(), 1, 0.0))
    } else {
        moment_command(row, settings).or_else(|| page_command(row, settings))
    };
    match command {
        Some(command) => launch(&command),
        None => {
            let uri = gio::File::for_path(&row.path).uri();
            if gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>).is_err() {
                launch(&["xdg-open".into(), row.path.clone()]);
            }
        }
    }
}

/// Shows the file selected in the file manager (Nautilus, Dolphin, Thunar: org.freedesktop.FileManager1), else opens
/// its folder.
pub fn show_in_files(path: &str) {
    let uri = gio::File::for_path(path).uri().to_string();
    let shown = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok().is_some_and(|bus| {
        bus.call_sync(
            Some("org.freedesktop.FileManager1"),
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1",
            "ShowItems",
            Some(&(vec![uri], String::new()).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
        )
        .is_ok()
    });
    if !shown && let Some(folder) = Path::new(path).parent() {
        launch(&["xdg-open".into(), folder.to_string_lossy().into_owned()]);
    }
}

/// Puts the file on the clipboard, so pasting it in a file manager or a chat copies the file, and pasting it in a
/// text field gives its path.
pub fn copy_file(path: &str) {
    let Some(display) = gdk::Display::default() else { return };
    let uri = gio::File::for_path(path).uri().to_string();
    let providers = [
        gdk::ContentProvider::for_bytes("text/uri-list", &glib::Bytes::from_owned(format!("{uri}\r\n"))),
        gdk::ContentProvider::for_bytes(
            "x-special/gnome-copied-files",
            &glib::Bytes::from_owned(format!("copy\n{uri}")),
        ),
        gdk::ContentProvider::for_value(&path.to_value()),
    ];
    display.clipboard().set_content(Some(&gdk::ContentProvider::new_union(&providers))).ok();
}

pub fn copy_path(path: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_fill_in_the_place() {
        let words = template("mpv --start={seconds} {file}", "/v/a b.mp4", 1, 1, 6.0);
        assert_eq!(words, ["mpv", "--start=6.0", "/v/a b.mp4"]);
        let words = template("zathura -P {page} {file}", "/d/x.pdf", 1, 3, 0.0);
        assert_eq!(words, ["zathura", "-P", "3", "/d/x.pdf"]);
    }
}
