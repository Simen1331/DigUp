//! Where ShowMe keeps things: the XDG folders.

use std::path::PathBuf;

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn xdg(variable: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(variable).filter(|value| !value.is_empty()) {
        Some(folder) => PathBuf::from(folder),
        None => home().join(fallback),
    }
}

/// `$XDG_DATA_HOME/showme` (~/.local/share/showme): the model, pdfium and the indexes.
pub fn data_folder() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("showme")
}

/// `$XDG_CONFIG_HOME/showme` (~/.config/showme): settings.
pub fn config_folder() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("showme")
}

/// Where scratch files go while indexing: `$XDG_RUNTIME_DIR` (memory, cleared at logout), else the temporary folder.
pub fn scratch_root() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|folder| folder.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}
