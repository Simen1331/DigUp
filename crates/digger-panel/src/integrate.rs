//! Fitting Digger into the desktop, for setup: the background services, a key in Hyprland, the Waybar module, and the
//! programs Digger reads with. Every file it changes is copied aside first (`name.digger-backup-…`), and it only ever
//! adds lines.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::open::have;

const DAEMON_UNIT: &str = include_str!("../../../packaging/digger.service");
const PANEL_UNIT: &str = include_str!("../../../packaging/digger-panel.service");
const WAYBAR_STYLE: &str = include_str!("../../../packaging/waybar.css");

/// What runs the panel's key.
const TOGGLE: &str = "digger-panel --toggle";

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn config(path: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()).map(PathBuf::from);
    base.unwrap_or_else(|| home().join(".config")).join(path)
}

/// Copies `file` aside before it's changed; returns where the copy is.
fn backup(file: &Path) -> Result<PathBuf> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let name = format!("{}.digger-backup-{stamp}", file.file_name().unwrap_or_default().to_string_lossy());
    let copy = file.with_file_name(name);
    std::fs::copy(file, &copy).with_context(|| format!("can't copy {}", file.display()))?;
    Ok(copy)
}

fn run(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

// The services.

/// Writes the user services (when the package didn't) and starts them now and at each login.
pub fn enable_services() -> Result<()> {
    let folder = config("systemd/user");
    std::fs::create_dir_all(&folder)?;
    let installed = |name: &str| Path::new("/usr/lib/systemd/user").join(name).exists() || folder.join(name).exists();
    for (name, text) in [("digger.service", DAEMON_UNIT), ("digger-panel.service", PANEL_UNIT)] {
        if !installed(name) {
            // Installed with cargo: the programs are in ~/.cargo/bin, as the units say.
            std::fs::write(folder.join(name), text)?;
        }
    }
    run("systemctl", &["--user", "daemon-reload"]);
    if !run("systemctl", &["--user", "enable", "--now", "digger.service"]) {
        bail!("systemctl couldn't start digger.service (journalctl --user -u digger shows why)");
    }
    run("systemctl", &["--user", "enable", "digger-panel.service"]);
    Ok(())
}

// The key.

/// The file Omarchy keeps its keys in (or Hyprland's own).
pub fn hyprland_file() -> PathBuf {
    let bindings = config("hypr/bindings.conf");
    if bindings.exists() { bindings } else { config("hypr/hyprland.conf") }
}

/// The line that binds `key` ("SUPER, D") to the panel.
pub fn bind_line(key: &str) -> String {
    format!("bindd = {}, Digger, exec, {TOGGLE}", key.trim())
}

/// "SUPER SHIFT, F" → (the modifier mask Hyprland reports, the key), or None when it doesn't read as a key.
fn parse_key(key: &str) -> Option<(u32, String)> {
    let (mods, name) = key.split_once(',')?;
    let name = name.trim();
    if name.is_empty() || name.contains(',') {
        return None;
    }
    let mut mask = 0;
    for word in mods.split([' ', '_', '+']).filter(|word| !word.is_empty()) {
        mask |= match word.to_uppercase().as_str() {
            "SHIFT" => 1,
            "CTRL" | "CONTROL" => 4,
            "ALT" => 8,
            "SUPER" | "WIN" | "LOGO" | "MOD4" => 64,
            _ => return None,
        };
    }
    Some((mask, name.to_lowercase()))
}

/// Whether the key reads as one Hyprland takes.
pub fn valid_key(key: &str) -> bool {
    parse_key(key).is_some()
}

/// What `key` does now in Hyprland, if anything ("Launch browser"); None when it's free or Hyprland isn't running.
pub fn key_in_use(key: &str) -> Option<String> {
    let (mask, name) = parse_key(key)?;
    let output = Command::new("hyprctl").args(["binds", "-j"]).stderr(Stdio::null()).output().ok()?;
    let binds: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).ok()?;
    binds.iter().find_map(|bind| {
        let same = bind["modmask"].as_u64() == Some(mask as u64)
            && bind["key"].as_str().is_some_and(|key| key.eq_ignore_ascii_case(&name));
        if !same || bind["arg"].as_str().is_some_and(|arg| arg.contains(TOGGLE)) {
            return None;
        }
        let description = bind["description"].as_str().filter(|text| !text.is_empty());
        let action = format!("{} {}", bind["dispatcher"].as_str().unwrap_or(""), bind["arg"].as_str().unwrap_or(""));
        Some(description.map(String::from).unwrap_or_else(|| action.trim().to_string()))
    })
}

/// The key already bound to the panel in the file, if one is ("SUPER, D").
pub fn bound_key() -> Option<String> {
    let text = std::fs::read_to_string(hyprland_file()).ok()?;
    bound_key_in(&text)
}

fn bound_key_in(text: &str) -> Option<String> {
    text.lines().map(str::trim).filter(|line| !line.starts_with('#') && line.contains(TOGGLE)).find_map(|line| {
        let (_, rest) = line.split_once('=')?;
        let mut parts = rest.split(',').map(str::trim);
        Some(format!("{}, {}", parts.next()?, parts.next()?))
    })
}

/// Adds the key to Hyprland's config (Hyprland reads it again by itself). Returns the backup.
pub fn add_key(key: &str) -> Result<Option<PathBuf>> {
    if !valid_key(key) {
        bail!("“{key}” isn't a key Hyprland knows; write it as “SUPER, D”");
    }
    let file = hyprland_file();
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    if bound_key_in(&text).is_some() {
        bail!("{} already has a key for Digger", tilde(&file));
    }
    let copy = if file.exists() { Some(backup(&file)?) } else { None };
    let mut text = text;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("\n# Digger: search your files\n{}\n", bind_line(key)));
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(&file, text)?;
    Ok(copy)
}

// Waybar.

/// The module, as it goes in Waybar's config.
pub const WAYBAR_MODULE: &str = r#""custom/digger": {
    "exec": "digger status --waybar --follow",
    "return-type": "json",
    "restart-interval": 5,
    "on-click": "digger-panel --toggle",
    "on-click-middle": "digger pause --toggle",
    "tooltip": true
  }"#;

pub fn waybar_file() -> PathBuf {
    config("waybar/config.jsonc")
}

pub fn in_waybar() -> bool {
    std::fs::read_to_string(waybar_file()).is_ok_and(|text| text.contains("\"custom/digger\""))
}

/// Waybar's config with Digger in it: first in "modules-right", and the module's block first in the bar. None when the
/// config isn't one this can change safely (or Digger is there already).
fn waybar_with_digger(text: &str) -> Option<String> {
    if text.contains("\"custom/digger\"") {
        return None;
    }
    let list = text.find("\"modules-right\"")?;
    let open = list + text[list..].find('[')?;
    let bar = text.find('{').filter(|bar| *bar < list)?;
    let mut changed = String::with_capacity(text.len() + WAYBAR_MODULE.len() + 40);
    changed.push_str(&text[..=bar]);
    changed.push_str("\n  ");
    changed.push_str(WAYBAR_MODULE);
    changed.push(',');
    changed.push_str(&text[bar + 1..=open]);
    // An empty list takes no comma after the name.
    let empty = text[open + 1..].trim_start().starts_with(']');
    changed.push_str(if empty { "\"custom/digger\"" } else { "\"custom/digger\", " });
    changed.push_str(&text[open + 1..]);
    Some(changed)
}

/// Adds the module to Waybar and its look to Waybar's style, then restarts Waybar. Returns the backups.
pub fn add_to_waybar() -> Result<Vec<PathBuf>> {
    let file = waybar_file();
    let text = std::fs::read_to_string(&file).with_context(|| format!("can't read {}", tilde(&file)))?;
    let Some(changed) = waybar_with_digger(&text) else {
        bail!("{} has no \"modules-right\" list to add Digger to; copy the lines instead", tilde(&file));
    };
    let mut copies = vec![backup(&file)?];
    std::fs::write(&file, changed)?;
    let style = config("waybar/style.css");
    if let Ok(text) = std::fs::read_to_string(&style)
        && !text.contains("#custom-digger")
    {
        copies.push(backup(&style)?);
        std::fs::write(&style, format!("{}\n{WAYBAR_STYLE}", text.trim_end()))?;
    }
    if have("omarchy-restart-waybar") {
        run("omarchy-restart-waybar", &[]);
    } else {
        run("pkill", &["-SIGUSR2", "-x", "waybar"]);
    }
    Ok(copies)
}

fn tilde(path: &Path) -> String {
    digger_core::settings::shorten(path)
}

// What Digger reads with.

pub struct Need {
    pub name: &'static str,
    pub why: &'static str,
    pub there: bool,
    /// The Arch package.
    pub package: &'static str,
}

/// The programs Digger uses, and whether they're there.
pub fn needs() -> Vec<Need> {
    let tesseract = have("tesseract");
    let languages = if tesseract { crate::settings::ocr_languages() } else { Vec::new() };
    vec![
        Need {
            name: "ffmpeg",
            why: "listens to recordings and looks at videos",
            there: have("ffmpeg") && have("ffprobe"),
            package: "ffmpeg",
        },
        Need { name: "Tesseract", why: "reads the text in screenshots", there: tesseract, package: "tesseract" },
        Need {
            name: "English for Tesseract",
            why: "the language it reads",
            there: languages.iter().any(|language| language == "eng"),
            package: "tesseract-data-eng",
        },
        Need {
            name: "A video player",
            why: "opens recordings and videos where they matched",
            there: ["mpv", "vlc", "celluloid"].iter().any(|player| have(player)),
            package: "mpv",
        },
        Need { name: "wl-clipboard", why: "copies files and paths", there: have("wl-copy"), package: "wl-clipboard" },
    ]
}

/// The command that installs what's missing, or None when all is there.
pub fn install_command(needs: &[Need]) -> Option<String> {
    let missing: Vec<&str> = needs.iter().filter(|need| !need.there).map(|need| need.package).collect();
    if missing.is_empty() { None } else { Some(format!("sudo pacman -S --needed {}", missing.join(" "))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_read_as_hyprland_writes_them() {
        assert_eq!(parse_key("SUPER, D"), Some((64, "d".into())));
        assert_eq!(parse_key("SUPER SHIFT, F"), Some((65, "f".into())));
        assert_eq!(parse_key("CTRL ALT, space"), Some((12, "space".into())));
        assert_eq!(parse_key("SUPER D"), None);
        assert_eq!(parse_key("HYPER, D"), None);
        assert_eq!(bind_line(" SUPER, D "), "bindd = SUPER, D, Digger, exec, digger-panel --toggle");
        let text = "bindd = SUPER, F, Files, exec, nautilus\n# bind = SUPER, X, exec, digger-panel --toggle\nbindd = SUPER ALT, D, Digger, exec, digger-panel --toggle\n";
        assert_eq!(bound_key_in(text).as_deref(), Some("SUPER ALT, D"));
    }

    #[test]
    fn waybar_gets_the_module_and_its_place() {
        let config = "// Omarchy\n{\n  \"layer\": \"top\",\n  \"modules-right\": [\n    \"network\",\n    \"battery\"\n  ],\n  \"battery\": {}\n}\n";
        let changed = waybar_with_digger(config).unwrap();
        assert!(changed.starts_with("// Omarchy\n{\n  \"custom/digger\": {"));
        assert!(changed.contains("\"modules-right\": [\"custom/digger\", \n    \"network\""));
        assert!(changed.ends_with("\"battery\": {}\n}\n"));
        assert_eq!(waybar_with_digger(&changed), None);
        let empty = "{ \"modules-right\": [ ] }";
        assert!(waybar_with_digger(empty).unwrap().contains("[\"custom/digger\" ]"));
        assert_eq!(waybar_with_digger("{ \"modules-left\": [] }"), None);
    }
}
