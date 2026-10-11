//! The folder picker in setup and settings: each folder with a checkbox, its subfolders one level down (all of a
//! folder, some of it, or none), what's in it, and Add Folder…. It edits a `FolderSelection`, the rule the crawler and
//! the search follow.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use digger_core::crawler;
use digger_core::folder_selection::{FolderSelection, Mark};
use digger_core::model::{FileKind, IndexOptions};
use gtk::prelude::*;
use gtk::{gio, glib, pango};

use crate::results::tilde;

type OnChange = Box<dyn Fn(&FolderSelection)>;

pub struct FolderPicker {
    pub widget: gtk::Box,
    list: gtk::Box,
    selection: RefCell<FolderSelection>,
    /// The folders listed at the top level (chosen ones and suggestions).
    roots: RefCell<Vec<String>>,
    checks: RefCell<Vec<(String, gtk::CheckButton)>>,
    /// The checkboxes are being set from the selection: their toggles aren't clicks.
    refreshing: std::cell::Cell<bool>,
    code: bool,
    changed: RefCell<Option<OnChange>>,
}

/// The folders worth suggesting: the XDG user folders that exist (Documents, Pictures…), or for code the usual places
/// for projects.
pub fn suggestions(code: bool) -> Vec<String> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let names: Vec<PathBuf> = if code {
        ["code", "Code", "src", "projects", "Projects", "dev", "Work", "work", "repos", "git"]
            .iter()
            .map(|name| home.join(name))
            .collect()
    } else {
        let xdg = [
            glib::UserDirectory::Documents,
            glib::UserDirectory::Pictures,
            glib::UserDirectory::Downloads,
            glib::UserDirectory::Desktop,
            glib::UserDirectory::Music,
            glib::UserDirectory::Videos,
        ];
        let mut folders: Vec<PathBuf> = xdg.iter().filter_map(|directory| glib::user_special_dir(*directory)).collect();
        // Without user-dirs.dirs: the usual names.
        let usual = ["Documents", "Pictures", "Downloads", "Desktop", "Music", "Videos", "Screenshots"];
        folders.extend(usual.iter().map(|name| home.join(name)));
        folders
    };
    let mut seen = Vec::new();
    for folder in names {
        let path = folder.to_string_lossy().into_owned();
        if folder.is_dir() && folder != home && !seen.contains(&path) {
            seen.push(path);
        }
    }
    seen
}

/// The folders in `folder`, by name, without hidden ones.
fn subfolders(folder: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(folder) else { return Vec::new() };
    let mut folders: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().to_str().map(String::from))
        .filter(|name| !name.starts_with('.'))
        .map(|name| format!("{}/{name}", folder.trim_end_matches('/')))
        .collect();
    folders.sort_by_key(|path| path.to_lowercase());
    folders
}

/// "136 pictures · 25 PDFs · 3 videos" from a quick crawl (names and sizes only).
fn contents(folder: &str, code: bool) -> String {
    let options = IndexOptions { code, ..Default::default() };
    let report = crawler::crawl(&[PathBuf::from(folder)], &options);
    let mut kinds: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for candidate in &report.candidates {
        let name = match candidate.kind {
            FileKind::Screenshot => ("screenshot", "screenshots"),
            FileKind::Image => ("picture", "pictures"),
            FileKind::Pdf => ("PDF", "PDFs"),
            FileKind::Doc => ("document", "documents"),
            FileKind::Audio => ("recording", "recordings"),
            FileKind::Video => ("video", "videos"),
            FileKind::Code => ("code file", "code files"),
        };
        *kinds.entry(name).or_default() += 1;
    }
    if kinds.is_empty() {
        return "nothing to read".into();
    }
    let mut parts: Vec<((&str, &str), usize)> = kinds.into_iter().collect();
    parts.sort_by_key(|part| std::cmp::Reverse(part.1));
    let part =
        |((one, many), count): &((&str, &str), usize)| format!("{count} {}", if *count == 1 { one } else { many });
    parts.iter().map(part).collect::<Vec<_>>().join(" · ")
}

impl FolderPicker {
    pub fn new(selection: FolderSelection, code: bool) -> Rc<FolderPicker> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .min_content_height(280)
            .child(&list)
            .build();
        widget.append(&scroller);
        let add = gtk::Button::with_label("Add Folder…");
        add.set_halign(gtk::Align::Start);
        widget.append(&add);
        let mut roots: Vec<String> = selection.chosen().to_vec();
        if roots.is_empty() || !code {
            for suggestion in suggestions(code) {
                if !roots
                    .iter()
                    .any(|root| *root == suggestion || digger_core::folder_selection::is_inside(&suggestion, root))
                {
                    roots.push(suggestion);
                }
            }
        }
        // Folders chosen inside others show under them, not at the top.
        let tops: Vec<String> = roots
            .iter()
            .filter(|root| {
                !roots.iter().any(|other| other != *root && digger_core::folder_selection::is_inside(root, other))
            })
            .cloned()
            .collect();
        let picker = Rc::new(FolderPicker {
            widget,
            list,
            selection: RefCell::new(selection),
            roots: RefCell::new(tops),
            checks: RefCell::new(Vec::new()),
            refreshing: std::cell::Cell::new(false),
            code,
            changed: RefCell::new(None),
        });
        picker.rebuild();
        let weak = Rc::downgrade(&picker);
        add.connect_clicked(move |button| {
            let Some(picker) = weak.upgrade() else { return };
            let dialog = gtk::FileDialog::builder().title("Add a folder to search").modal(true).build();
            let window = button.root().and_downcast::<gtk::Window>();
            dialog.select_folder(window.as_ref(), gio::Cancellable::NONE, move |result| {
                if let Ok(folder) = result
                    && let Some(path) = folder.path()
                {
                    let path = path.to_string_lossy().into_owned();
                    picker.selection.borrow_mut().set_folder(&path, true);
                    if !picker.roots.borrow().contains(&path) {
                        picker.roots.borrow_mut().push(path);
                    }
                    picker.rebuild();
                    picker.notify();
                }
            });
        });
        picker
    }

    pub fn connect_changed(&self, callback: impl Fn(&FolderSelection) + 'static) {
        *self.changed.borrow_mut() = Some(Box::new(callback));
    }

    pub fn selection(&self) -> FolderSelection {
        self.selection.borrow().clone()
    }

    fn notify(&self) {
        if let Some(callback) = self.changed.borrow().as_ref() {
            callback(&self.selection.borrow());
        }
    }

    /// Sets each checkbox from the selection: on, off, or mixed (some of its subfolders).
    fn refresh_marks(&self) {
        let checks = self.checks.borrow().clone();
        let marks: Vec<Mark> = checks.iter().map(|(path, _)| self.selection.borrow().mark(path)).collect();
        self.refreshing.set(true);
        for ((_, check), mark) in checks.iter().zip(marks) {
            check.set_inconsistent(mark == Mark::Mixed);
            check.set_active(mark != Mark::Off);
        }
        self.refreshing.set(false);
    }

    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.checks.borrow_mut().clear();
        for root in self.roots.borrow().clone() {
            let expander = gtk::Expander::new(None);
            let header = self.check_row(&root, true);
            expander.set_label_widget(Some(&header));
            let inner = gtk::Box::new(gtk::Orientation::Vertical, 0);
            inner.set_margin_start(28);
            expander.set_child(Some(&inner));
            // Subfolders are read when the folder is opened.
            let weak = Rc::downgrade(self);
            let filled = std::cell::Cell::new(false);
            expander.connect_expanded_notify(move |expander| {
                if !expander.is_expanded() || filled.replace(true) {
                    return;
                }
                let Some(picker) = weak.upgrade() else { return };
                for folder in subfolders(&root) {
                    inner.append(&picker.check_row(&folder, false));
                }
                picker.refresh_marks();
            });
            self.list.append(&expander);
        }
        self.refresh_marks();
    }

    /// A folder's line: its checkbox and name, and (for top folders) what's in it, counted in the background.
    fn check_row(self: &Rc<Self>, path: &str, top: bool) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let name = if top {
            tilde(path)
        } else {
            Path::new(path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
        };
        let check = gtk::CheckButton::with_label(&name);
        row.append(&check);
        if top {
            let counts = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
            counts.add_css_class("digger-sub");
            counts.set_text("counting…");
            row.append(&counts);
            let (folder, code) = (path.to_string(), self.code);
            glib::spawn_future_local(async move {
                let text = gio::spawn_blocking(move || contents(&folder, code)).await.unwrap_or_default();
                counts.set_text(&text);
            });
        }
        let weak = Rc::downgrade(self);
        let folder = path.to_string();
        check.connect_toggled(move |check| {
            let Some(picker) = weak.upgrade() else { return };
            if picker.refreshing.get() {
                return;
            }
            // A mixed folder clicked becomes all of it.
            let on = check.is_active() || check.is_inconsistent();
            if top {
                picker.selection.borrow_mut().set_folder(&folder, on);
            } else {
                picker.selection.borrow_mut().set_subfolder(&folder, on);
            }
            picker.refresh_marks();
            picker.notify();
        });
        self.checks.borrow_mut().push((path.to_string(), check));
        row
    }
}
