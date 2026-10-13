//! The collections' page in the settings: named sets of folders, each searched alone by typing its name and a colon
//! ("business: negotiating under pressure"). They're found in the main index, so nothing is read again for them.

use std::path::Path;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};
use showme_core::settings::{self as config, Collection, Config};

use crate::settings::{Editing, heading, keep, note, page};

pub fn collections_page(editing: &Rc<Editing>) -> gtk::Box {
    let page = page();
    page.append(&heading("Collections"));
    page.append(&note(
        "A collection is some of your folders, searched alone: type its name and a colon before what you look for, \
         like business: negotiating under pressure. Its files are the ones ShowMe already reads; nothing is read again.",
    ));
    let list = gtk::Box::new(gtk::Orientation::Vertical, 18);
    page.append(&list);
    let new = gtk::Button::with_label("New Collection");
    new.set_halign(gtk::Align::Start);
    page.append(&new);

    let page_list = Rc::new(List { list, editing: editing.clone() });
    page_list.rebuild();
    let weak = Rc::downgrade(&page_list);
    new.connect_clicked(move |_| {
        let Some(page_list) = weak.upgrade() else { return };
        page_list.editing.change(|config| config.collections.push(Collection::default()));
        page_list.rebuild();
        // The new one's name, ready to type.
        if let Some(entry) =
            page_list.list.last_child().and_then(|card| card.first_child()).and_then(|top| top.first_child())
        {
            entry.grab_focus();
        }
    });
    keep(&page, page_list);
    page
}

struct List {
    list: gtk::Box,
    editing: Rc<Editing>,
}

impl List {
    /// Builds each collection's card again from the settings (after one is made or removed, or a folder changes).
    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let config = self.editing.config.borrow().clone();
        if config.collections.is_empty() {
            self.list.append(&note("No collections yet."));
        }
        for (index, collection) in config.collections.iter().enumerate() {
            self.list.append(&self.card(index, collection, &config));
        }
    }

    /// A collection: its name, its folders (each with ×), and Add Folder….
    fn card(self: &Rc<Self>, index: usize, collection: &Collection, config: &Config) -> gtk::Box {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let name = gtk::Entry::builder()
            .text(&collection.name)
            .placeholder_text("a name, like business")
            .width_chars(24)
            .build();
        let problem = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        problem.add_css_class("showme-sub");
        let remove = gtk::Button::with_label("Remove");
        top.append(&name);
        top.append(&problem);
        top.append(&remove);
        card.append(&top);
        let say = {
            let problem = problem.clone();
            move |name: &gtk::Entry, text: &str| {
                problem.set_text(text);
                if text.is_empty() { name.remove_css_class("error") } else { name.add_css_class("error") }
            }
        };
        say(&name, &name_problem(config, index, &collection.name));

        let editing = self.editing.clone();
        name.connect_changed(move |name| {
            let text = name.text().trim().to_string();
            let problem = name_problem(&editing.config.borrow(), index, &text);
            say(name, &problem);
            if problem.is_empty() {
                editing.change(|config| config.collections[index].name = text);
            }
        });
        let weak = Rc::downgrade(self);
        remove.connect_clicked(move |_| {
            let Some(list) = weak.upgrade() else { return };
            list.editing.change(|config| {
                config.collections.remove(index);
            });
            list.rebuild();
        });

        let searched: Vec<String> =
            config.main().0.iter().map(|folder| folder.to_string_lossy().into_owned()).collect();
        for folder in &collection.folders {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_start(12);
            let path = config::expand(folder);
            let label = gtk::Label::builder().label(folder).xalign(0.0).build();
            row.append(&label);
            // Only what ShowMe reads can be found: say so for a folder outside its folders.
            if !searched.iter().any(|root| path.starts_with(root)) {
                let outside = gtk::Label::new(Some("not in the folders ShowMe reads"));
                outside.add_css_class("showme-sub");
                row.append(&outside);
            }
            let gone = gtk::Button::from_icon_name("window-close-symbolic");
            gone.add_css_class("flat");
            gone.set_tooltip_text(Some("Take it out of the collection"));
            row.append(&gone);
            let (weak, folder) = (Rc::downgrade(self), folder.clone());
            gone.connect_clicked(move |_| {
                let Some(list) = weak.upgrade() else { return };
                list.editing.change(|config| config.collections[index].folders.retain(|kept| *kept != folder));
                list.rebuild();
            });
            card.append(&row);
        }
        let add = gtk::Button::with_label("Add Folder…");
        add.set_halign(gtk::Align::Start);
        add.set_margin_start(12);
        let weak = Rc::downgrade(self);
        let start = collection.folders.first().or(config.folders.first()).map(|folder| config::expand(folder));
        add.connect_clicked(move |button| {
            let dialog = gtk::FileDialog::builder().title("Add a folder to the collection").modal(true).build();
            if let Some(start) = &start {
                dialog.set_initial_folder(Some(&gio::File::for_path(start)));
            }
            let window = button.root().and_downcast::<gtk::Window>();
            let weak = weak.clone();
            dialog.select_folder(window.as_ref(), gio::Cancellable::NONE, move |result| {
                let Some(list) = weak.upgrade() else { return };
                if let Ok(folder) = result
                    && let Some(path) = folder.path()
                {
                    let folder = config::shorten(Path::new(&path));
                    list.editing.change(|config| {
                        let folders = &mut config.collections[index].folders;
                        if !folders.contains(&folder) {
                            folders.push(folder);
                        }
                    });
                    // After the dialog has let go of the window.
                    glib::idle_add_local_once(move || list.rebuild());
                }
            });
        });
        card.append(&add);
        card
    }
}

/// What's wrong with a name for the collection at `index` ("" when nothing is).
fn name_problem(config: &Config, index: usize, name: &str) -> String {
    if name.is_empty() {
        "Give it a name to search it by.".into()
    } else if !config::collection_name_ok(name) {
        "One word: letters, digits, - and _ (and not \"code\").".into()
    } else if config
        .collections
        .iter()
        .enumerate()
        .any(|(other, collection)| other != index && collection.name.eq_ignore_ascii_case(name))
    {
        "Another collection has this name.".into()
    } else {
        String::new()
    }
}
