//! Digger's settings: `~/.config/digger/config.toml`. The daemon reads it at start and again when told to
//! (`digger reload`), and `digger add` / `digger remove` edit it.
//!
//! ```toml
//! folders = ["~/Documents", "~/Pictures", "~/Downloads"]
//! skipped = ["~/Downloads/Torrents"]
//! code_folders = ["~/code"]
//! excluded_extensions = ["iso"]
//! index_on_battery = true
//! ```

use std::path::{Path, PathBuf};

use crate::crawler::standardized;
use crate::folder_selection::FolderSelection;
use crate::model::IndexOptions;
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
    /// The code index catches up once the code folders have been quiet this long (seconds).
    pub code_quiet_seconds: f64,
    /// The query encoder (~0.25 GB) is let go after this long without a search (seconds).
    pub encoder_idle_seconds: f64,
    /// The indexing model (~1 GB) is let go after this long with nothing to index (seconds).
    pub linger_seconds: f64,
    /// The index folder; empty: ~/.local/share/digger/index.
    pub index: String,
    /// The big first pass waits in power-saver mode.
    pub pause_in_power_saver: bool,
    /// The languages Tesseract reads screenshots in ("eng", "nor"); empty: the installed ones Digger knows.
    pub ocr_languages: Vec<String>,
    /// How results open.
    pub open: OpenSettings,
    /// How the panel looks.
    pub panel: PanelSettings,
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
            code_quiet_seconds: 60.0,
            encoder_idle_seconds: 600.0,
            linger_seconds: 60.0,
            index: String::new(),
            pause_in_power_saver: true,
            ocr_languages: Vec::new(),
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

    fn selection(folders: &[String], skipped: &[String]) -> FolderSelection {
        let folders: Vec<String> = folders.iter().map(|path| expand(path).to_string_lossy().into_owned()).collect();
        let skipped: Vec<String> = skipped.iter().map(|path| expand(path).to_string_lossy().into_owned()).collect();
        let folders: Vec<&str> = folders.iter().map(String::as_str).collect();
        let skipped: Vec<&str> = skipped.iter().map(String::as_str).collect();
        FolderSelection::new(&folders, &skipped, &[])
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
    fn home_is_written_as_a_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(shorten(&expand("~/Documents")), "~/Documents");
        assert_eq!(expand("~/a/../b"), PathBuf::from(format!("{home}/b")));
    }
}
