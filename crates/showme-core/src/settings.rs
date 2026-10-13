//! ShowMe's settings: `~/.config/showme/config.toml`. The daemon reads it at start and again when told to
//! (`showme reload`), and `showme add` / `showme remove` edit it.
//!
//! ```toml
//! folders = ["~/Documents", "~/Pictures", "~/Downloads"]
//! skipped = ["~/Downloads/Torrents"]
//! code_folders = ["~/code"]
//! excluded_extensions = ["iso"]
//! index_on_battery = true
//!
//! [[collections]]
//! name = "business"
//! folders = ["~/Books/Business", "~/Books/Leadership"]
//! ```

use std::path::{Path, PathBuf};

use crate::crawler::standardized;
use crate::folder_selection::FolderSelection;
use crate::model::{FileKind, IndexOptions};
use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The folders to search.
    pub folders: Vec<String>,
    /// Folders inside them to leave out (a folder chosen inside a skipped one is still searched).
    pub skipped: Vec<String>,
    /// The folders code search reads (`code:` searches). Empty: code search is off.
    pub code_folders: Vec<String>,
    pub code_skipped: Vec<String>,
    /// File types never to read ("iso", "heic").
    pub excluded_extensions: Vec<String>,
    /// Also read loose source code and data files as documents.
    pub include_code: bool,
    /// Don't skip folders that look like datasets.
    pub include_datasets: bool,
    /// The big first pass also runs on battery (new files always index right away).
    pub index_on_battery: bool,
    /// The big first pass reads this long at a time, then pauses until told to go on (minutes); 0: all at once.
    pub step_minutes: f64,
    /// The code index catches up once the code folders have been quiet this long (seconds).
    pub code_quiet_seconds: f64,
    /// The query encoder (~0.25 GB) is let go after this long without a search (seconds).
    pub encoder_idle_seconds: f64,
    /// The indexing model (~1 GB) is let go after this long with nothing to index (seconds).
    pub linger_seconds: f64,
    /// The index folder; empty: ~/.local/share/showme/index.
    pub index: String,
    /// The big first pass waits in power-saver mode.
    pub pause_in_power_saver: bool,
    /// The languages Tesseract reads screenshots in ("eng", "nor"); empty: the installed ones ShowMe knows.
    pub ocr_languages: Vec<String>,
    /// Named sets of folders to search alone: "business: negotiating under pressure".
    pub collections: Vec<Collection>,
    /// How results open.
    pub open: OpenSettings,
    /// How the panel looks.
    pub panel: PanelSettings,
}

/// Folders searched alone by name: a search starting "business:" looks only in the business collection's folders
/// (and what's inside them). They are found in the main index; nothing is read again for them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Collection {
    pub name: String,
    pub folders: Vec<String>,
}

impl Collection {
    /// Its folders as full paths.
    pub fn roots(&self) -> Vec<String> {
        self.folders.iter().map(|folder| expand(folder).to_string_lossy().into_owned()).collect()
    }
}

/// A name a collection can have: a word or two people type before a colon ("business", "psych-notes"), not "code".
pub fn collection_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        && !name.eq_ignore_ascii_case(crate::model::code_query::PREFIX.trim_end_matches(':'))
        && FileKind::named(name).is_none()
}

/// What a search names before what it looks for: a collection and kinds, in any order ("business: pdf: pricing" →
/// the business collection's PDFs, "pricing").
#[derive(Debug, Clone, PartialEq)]
pub struct Scope<'c, 'q> {
    pub collection: Option<&'c Collection>,
    pub kinds: Vec<FileKind>,
    pub rest: &'q str,
}

/// The programs results open in: "auto" finds one; otherwise a command with {file}, {line}, {page}, {seconds} and
/// {terminal} in it, or an editor's name for code ("nvim", "zed", "code"…).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenSettings {
    pub code: String,
    pub video: String,
    pub audio: String,
    pub pdf: String,
}

impl Default for OpenSettings {
    fn default() -> Self {
        OpenSettings { code: "auto".into(), video: "auto".into(), audio: "auto".into(), pdf: "auto".into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PanelSettings {
    /// "omarchy": the colors of the current Omarchy theme; "gtk": the GTK theme's.
    pub theme: String,
    /// Empty: the same font as Walker (the system's monospace).
    pub font: String,
}

impl Default for PanelSettings {
    fn default() -> Self {
        PanelSettings { theme: "omarchy".into(), font: String::new() }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            folders: Vec::new(),
            skipped: Vec::new(),
            code_folders: Vec::new(),
            code_skipped: Vec::new(),
            excluded_extensions: Vec::new(),
            include_code: false,
            include_datasets: false,
            index_on_battery: true,
            step_minutes: 0.0,
            code_quiet_seconds: 60.0,
            encoder_idle_seconds: 600.0,
            linger_seconds: 60.0,
            index: String::new(),
            pause_in_power_saver: true,
            ocr_languages: Vec::new(),
            collections: Vec::new(),
            open: OpenSettings::default(),
            panel: PanelSettings::default(),
        }
    }
}

/// "~/Documents" → "/home/you/Documents"; relative paths are taken from the current folder.
pub fn expand(path: &str) -> PathBuf {
    let path = match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None if path == "~" => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()),
        None => PathBuf::from(path),
    };
    let path = if path.is_absolute() { path } else { std::env::current_dir().unwrap_or_default().join(path) };
    PathBuf::from(standardized(&path))
}

/// "/home/you/Documents" → "~/Documents", for the file people read and edit.
pub fn shorten(path: &Path) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let text = path.to_string_lossy();
    match text.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => text.into_owned(),
    }
}

impl Config {
    pub fn file() -> PathBuf {
        paths::config_folder().join("config.toml")
    }

    /// The settings, or the defaults when there's no file yet.
    pub fn load() -> Result<Config> {
        let file = Self::file();
        match std::fs::read_to_string(&file) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("can't read {}", file.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(error).with_context(|| format!("can't read {}", file.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let file = Self::file();
        std::fs::create_dir_all(file.parent().unwrap())?;
        let part = file.with_extension("toml.part");
        std::fs::write(&part, toml::to_string_pretty(self)?)?;
        std::fs::rename(&part, &file)?;
        Ok(())
    }

    pub fn index_folder(&self) -> PathBuf {
        if self.index.is_empty() { paths::data_folder().join("index") } else { expand(&self.index) }
    }

    /// The folders as the folder picker edits them: the main index's, or the code index's.
    pub fn folder_selection(&self, code: bool) -> FolderSelection {
        if code {
            Self::selection(&self.code_folders, &self.code_skipped)
        } else {
            Self::selection(&self.folders, &self.skipped)
        }
    }

    /// Writes what the folder picker chose back, in the short form ("~/Documents").
    pub fn set_folder_selection(&mut self, code: bool, selection: &FolderSelection) {
        let short = |paths: &[String]| paths.iter().map(|path| shorten(Path::new(path))).collect::<Vec<_>>();
        let (folders, skipped) = (short(selection.chosen()), short(selection.skipped()));
        if code {
            (self.code_folders, self.code_skipped) = (folders, skipped);
        } else {
            (self.folders, self.skipped) = (folders, skipped);
        }
    }

    fn selection(folders: &[String], skipped: &[String]) -> FolderSelection {
        let folders: Vec<String> = folders.iter().map(|path| expand(path).to_string_lossy().into_owned()).collect();
        let skipped: Vec<String> = skipped.iter().map(|path| expand(path).to_string_lossy().into_owned()).collect();
        let folders: Vec<&str> = folders.iter().map(String::as_str).collect();
        let skipped: Vec<&str> = skipped.iter().map(String::as_str).collect();
        FolderSelection::new(&folders, &skipped, &[])
    }

    /// The collection and kinds a search names, and what's searched for in them.
    pub fn scope<'q>(&self, query: &'q str) -> Scope<'_, 'q> {
        let mut scope = Scope { collection: None, kinds: Vec::new(), rest: query.trim() };
        while let Some((head, rest)) = scope.rest.split_once(':') {
            let head = head.trim();
            if let Some(kinds) = FileKind::named(head) {
                for kind in kinds {
                    if !scope.kinds.contains(kind) {
                        scope.kinds.push(*kind);
                    }
                }
            } else if scope.collection.is_none()
                && let Some(collection) = self
                    .collections
                    .iter()
                    .find(|collection| !collection.name.is_empty() && collection.name.eq_ignore_ascii_case(head))
            {
                scope.collection = Some(collection);
            } else {
                break;
            }
            scope.rest = rest.trim();
        }
        scope
    }

    /// The main index's folders (without those another one already searches whole), and its options.
    pub fn main(&self) -> (Vec<PathBuf>, IndexOptions) {
        let selection = Self::selection(&self.folders, &self.skipped);
        let options = IndexOptions {
            include_code: self.include_code,
            include_datasets: self.include_datasets,
            excluded_folders: selection.effective_skips(),
            excluded_extensions: IndexOptions::extensions(&self.excluded_extensions.join(",")),
            ..Default::default()
        };
        (selection.folders().into_iter().map(PathBuf::from).collect(), options)
    }

    /// The code index's folders and options; no folders when code search is off.
    pub fn code(&self) -> (Vec<PathBuf>, IndexOptions) {
        let selection = Self::selection(&self.code_folders, &self.code_skipped);
        let options = IndexOptions {
            code: true,
            excluded_folders: selection.effective_skips(),
            excluded_extensions: IndexOptions::extensions(&self.excluded_extensions.join(",")),
            ..Default::default()
        };
        (selection.folders().into_iter().map(PathBuf::from).collect(), options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_skips_apply_inside_folders() {
        let text = "folders = [\"/d\", \"/d/a\", \"/e\"]\nskipped = [\"/d/old\", \"/x/y\"]\ncode_folders = [\"/c\"]\nindex_on_battery = false\n";
        let config: Config = toml::from_str(text).unwrap();
        assert!(!config.index_on_battery);
        assert_eq!(config.linger_seconds, 60.0);
        let (folders, options) = config.main();
        assert_eq!(folders, [PathBuf::from("/d"), PathBuf::from("/e")]);
        assert_eq!(options.excluded_folders, ["/d/old"]);
        let (code, options) = config.code();
        assert_eq!(code, [PathBuf::from("/c")]);
        assert!(options.code);
        let back: Config = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(back, config);
        assert!(toml::from_str::<Config>("folder = []").is_err()); // a typo is an error, not silence
    }

    #[test]
    fn a_search_can_name_a_collection() {
        let text = "folders = [\"/b\"]\n\n[[collections]]\nname = \"business\"\nfolders = [\"/b/Business\"]\n";
        let config: Config = toml::from_str(text).unwrap();
        let scope = config.scope("  Business: pricing power");
        assert_eq!((scope.collection.unwrap().name.as_str(), scope.rest), ("business", "pricing power"));
        assert_eq!(scope.collection.unwrap().roots(), ["/b/Business"]);
        assert!(config.scope("psychology: habits").collection.is_none());
        assert_eq!(config.scope("business plan").rest, "business plan");
        // Kinds too, before or after the collection.
        let scope = config.scope("pdf: business: Video: negotiating");
        assert_eq!(scope.collection.unwrap().name, "business");
        assert_eq!((scope.kinds.as_slice(), scope.rest), ([FileKind::Pdf, FileKind::Video].as_slice(), "negotiating"));
        assert_eq!(FileKind::titles(&scope.kinds).as_deref(), Some("PDFs and videos"));
        assert_eq!(config.scope("code: retry").rest, "code: retry"); // the code index's, not a kind
        assert_eq!(config.scope("note: call mum").rest, "note: call mum");
        let back: Config = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(back, config);
        assert!(
            collection_name_ok("psych-notes")
                && !collection_name_ok("code")
                && !collection_name_ok("two words")
                && !collection_name_ok("PDF")
        );
    }

    #[test]
    fn home_is_written_as_a_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(shorten(&expand("~/Documents")), "~/Documents");
        assert_eq!(expand("~/a/../b"), PathBuf::from(format!("{home}/b")));
    }
}
