//! What gets indexed. The defaults come from a real developer's machine: screenshot tools drop screenshots into code
//! projects, and one repo can hold a 55k-clip dataset.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::model::{FileKind, IndexOptions};

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Classification {
    Index(FileKind),
    /// A reason worth reporting ("code or data file").
    Skip(&'static str),
    /// A type we don't handle at all.
    Ignore,
}

/// Folders never worth walking into. Hidden folders are skipped by the crawler itself.
const SKIPPED_FOLDER_NAMES: &[&str] =
    &["node_modules", "bower_components", "venv", "site-packages", "__pycache__", "DerivedData", "Pods", "Carthage"];

/// Any of these in a folder makes it a code project, where only screenshots are indexed.
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "package.json",
    "Package.swift",
    "pyproject.toml",
    "setup.py",
    "requirements.txt",
    "Cargo.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "composer.json",
    "CMakeLists.txt",
    "meson.build",
];
const PROJECT_BUNDLE_EXTENSIONS: &[&str] = &["xcodeproj", "xcworkspace"];

const DOC_EXTENSIONS: &[&str] = &["txt", "text", "md", "markdown", "rtf", "docx", "doc", "odt", "html", "htm"];

/// Source code and data files: off by default (your editor and ripgrep search them better, and they flood an index).
const CODE_EXTENSIONS: &[&str] = &[
    "py", "ipynb", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "m", "mm", "swift", "tsx", "cts", "js", "jsx", "mjs",
    "cjs", "go", "rs", "java", "kt", "kts", "scala", "rb", "php", "cs", "fs", "sh", "bash", "zsh", "fish", "ps1",
    "lua", "pl", "r", "jl", "dart", "sql", "vue", "svelte", "css", "scss", "sass", "less", "json", "jsonl", "yaml",
    "yml", "toml", "xml", "plist", "gradle", "cmake", "mk", "lock", "log", "csv", "tsv", "ini", "cfg", "conf", "proto",
    "graphql", "tf", "nix", "zig", "hs", "ex", "exs", "erl", "clj", "elm", "ml", "vim", "el", "asm", "metal", "glsl",
    "hlsl", "wgsl", "cu", "pyi", "pyc",
];

/// What the file manager would call a picture, a PDF, a recording or a video (macOS asks its type system; here the
/// extension decides).
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "png", "heic", "heif", "webp", "gif", "bmp", "tif", "tiff", "avif", "jxl", "dng", "cr2",
    "cr3", "nef", "arw", "orf", "rw2", "raf", "srw", "pef",
];
const AUDIO_EXTENSIONS: &[&str] =
    &["mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "wma", "aiff", "aif", "amr", "caf"];
const VIDEO_EXTENSIONS: &[&str] =
    &["mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "3gp", "ogv", "vob"];

/// Screenshot tools' names: macOS's, CleanShot's, GNOME's, KDE Spectacle's and hyprshot's (Omarchy).
const SCREENSHOT_PREFIXES: &[&str] = &["Screenshot", "Screen Shot", "CleanShot", "SCR-", "screenshot", "Skjermbilde"];
const SCREENSHOT_MARKS: &[&str] = &["_hyprshot", "-hyprshot", "Screenshot_"];

/// Source files code search reads.
pub(crate) const SOURCE_EXTENSIONS: &[&str] = &[
    "py", "pyi", "pyx", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "m", "mm", "swift", "ts", "tsx", "mts",
    "cts", "js", "jsx", "mjs", "cjs", "go", "rs", "java", "kt", "kts", "scala", "groovy", "rb", "php", "cs", "fs",
    "sh", "bash", "zsh", "fish", "ps1", "lua", "pl", "pm", "r", "jl", "dart", "sql", "vue", "svelte", "astro", "html",
    "htm", "css", "scss", "sass", "less", "gradle", "cmake", "mk", "proto", "graphql", "gql", "tf", "hcl", "nix",
    "zig", "hs", "ex", "exs", "erl", "clj", "cljs", "elm", "ml", "mli", "vim", "el", "lisp", "asm", "s", "metal",
    "glsl", "hlsl", "wgsl", "cu", "cuh", "ino", "v", "sv", "tex",
];
/// What goes with code: a repo's docs and its settings.
const CODE_TEXT_EXTENSIONS: &[&str] = &["md", "markdown", "rst", "adoc", "org", "txt", "yaml", "yml", "toml"];
/// Code known by its name alone.
pub(crate) const CODE_FILE_NAMES: &[&str] = &[
    "Makefile",
    "makefile",
    "GNUmakefile",
    "Dockerfile",
    "Containerfile",
    "Justfile",
    "justfile",
    "Rakefile",
    "Gemfile",
    "Podfile",
    "Brewfile",
    "Procfile",
    "Vagrantfile",
    "Jenkinsfile",
    "PKGBUILD",
];
/// Folders code search doesn't go into, besides what git ignores: other people's code kept in a repo, and what
/// builds and runs leave.
const CODE_SKIPPED_FOLDER_NAMES: &[&str] = &[
    "vendor",
    "Vendor",
    "vendored",
    "third_party",
    "third-party",
    "thirdparty",
    "3rdparty",
    "external",
    "extern",
    "deps",
    "build",
    "Build",
    "dist",
    "out",
    "target",
    "obj",
    "coverage",
    "htmlcov",
    "generated",
    "__generated__",
    "wandb",
    "mlruns",
    "lightning_logs",
];

pub(crate) fn extension(path: &Path) -> String {
    path.extension().and_then(|ext| ext.to_str()).unwrap_or("").to_lowercase()
}

fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|name| name.to_str()).unwrap_or("")
}

/// What code search reads in the code folders: source files, a repo's docs and settings, and notebooks. Never data,
/// minified bundles, licenses, or anything that looks like a key or a password file.
pub fn classify_code(path: &Path, options: &IndexOptions) -> Classification {
    let name = file_name(path);
    let ext = extension(path);
    if options.excluded_extensions.contains(&ext) {
        return Classification::Skip("excluded type");
    }
    if is_secret(name) {
        return Classification::Skip("key or password file");
    }
    let lower = name.to_lowercase();
    if lower.ends_with(".min.js") || lower.ends_with(".min.mjs") || lower.ends_with(".min.css") {
        return Classification::Skip("minified");
    }
    if ["license", "licence", "copying", "notice"].iter().any(|prefix| lower.starts_with(prefix)) {
        return Classification::Skip("license");
    }
    if (ext == "ts" || ext == "mts") && is_mpeg_transport_stream(path) {
        return Classification::Ignore; // a video
    }
    if SOURCE_EXTENSIONS.contains(&ext.as_str())
        || CODE_TEXT_EXTENSIONS.contains(&ext.as_str())
        || ext == "ipynb"
        || CODE_FILE_NAMES.contains(&name)
        || name.starts_with("Dockerfile")
    {
        return Classification::Index(FileKind::Code);
    }
    if CODE_EXTENSIONS.contains(&ext.as_str()) {
        return Classification::Skip("data file");
    }
    Classification::Ignore
}

/// Keys, certificates, password and credential files: never read, wherever they are.
pub fn is_secret(name: &str) -> bool {
    let lower = name.to_lowercase();
    const SECRET_TYPES: &[&str] = &[
        "pem",
        "key",
        "p12",
        "pfx",
        "keystore",
        "jks",
        "ppk",
        "kdbx",
        "gpg",
        "asc",
        "crt",
        "cer",
        "der",
        "p8",
        "mobileprovision",
    ];
    if SECRET_TYPES.contains(&extension(Path::new(&lower)).as_str()) {
        return true;
    }
    const SECRET_NAMES: &[&str] = &[
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        ".env",
        "secret",
        "credential",
        ".netrc",
        ".npmrc",
        ".pypirc",
        "htpasswd",
        ".htpasswd",
    ];
    SECRET_NAMES.iter().any(|prefix| lower.starts_with(prefix))
}

/// Why code search doesn't go into a folder, or None.
pub(crate) fn code_skip_reason(folder: &Path) -> Option<&'static str> {
    if CODE_SKIPPED_FOLDER_NAMES.contains(&file_name(folder)) {
        return Some("vendored or built code");
    }
    // A submodule or a worktree (".git" is a file there): someone else's code, or a copy of a repo's own.
    if folder.join(".git").is_file() {
        return Some("submodule or worktree");
    }
    None
}

pub fn classify(path: &Path, options: &IndexOptions) -> Classification {
    let ext = extension(path);
    if options.excluded_extensions.contains(&ext) {
        return Classification::Skip("excluded type");
    }
    let code = if options.include_code {
        Classification::Index(FileKind::Doc)
    } else {
        Classification::Skip("code or data file")
    };
    // .ts / .mts are usually TypeScript. Only a real transport stream is video.
    if ext == "ts" || ext == "mts" || ext == "m2ts" {
        return if is_mpeg_transport_stream(path) { Classification::Index(FileKind::Video) } else { code };
    }
    let ext = ext.as_str();
    if DOC_EXTENSIONS.contains(&ext) {
        return Classification::Index(FileKind::Doc);
    }
    if CODE_EXTENSIONS.contains(&ext) || SOURCE_EXTENSIONS.contains(&ext) {
        return code;
    }
    if ext == "pdf" {
        return Classification::Index(FileKind::Pdf);
    }
    if IMAGE_EXTENSIONS.contains(&ext) {
        return Classification::Index(if is_screenshot(path) { FileKind::Screenshot } else { FileKind::Image });
    }
    if VIDEO_EXTENSIONS.contains(&ext) {
        return Classification::Index(FileKind::Video);
    }
    if AUDIO_EXTENSIONS.contains(&ext) {
        return Classification::Index(FileKind::Audio);
    }
    Classification::Ignore
}

/// MPEG-TS packets start with 0x47 every 188 bytes (or every 192 with a 4-byte timecode, as in .m2ts / AVCHD .mts).
pub fn is_mpeg_transport_stream(path: &Path) -> bool {
    let mut bytes = Vec::with_capacity(600);
    let Ok(file) = fs::File::open(path) else { return false };
    if file.take(600).read_to_end(&mut bytes).is_err() || bytes.len() < 400 {
        return false;
    }
    (bytes[0] == 0x47 && bytes[188] == 0x47 && bytes[376] == 0x47)
        || (bytes[4] == 0x47 && bytes[196] == 0x47 && bytes[388] == 0x47)
}

/// Screenshots are known by the names screenshot tools give them.
pub fn is_screenshot(path: &Path) -> bool {
    let name = file_name(path);
    SCREENSHOT_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || SCREENSHOT_MARKS.iter().any(|mark| name.contains(mark))
}

pub(crate) fn skip_reason_for_folder(path: &str, home: &str, excluded: &[String]) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or("");
    if SKIPPED_FOLDER_NAMES.contains(&name) {
        return Some("developer folder");
    }
    if name.ends_with(".noindex") {
        return Some(".noindex folder");
    }
    // Snap's per-app homes and Flatpak's: apps' own files, as ~/Library is on a Mac.
    if path == format!("{home}/snap") {
        return Some("~/snap");
    }
    if excluded.iter().any(|skip| path == skip || path.starts_with(&format!("{skip}/"))) {
        return Some("excluded folder");
    }
    None
}

pub(crate) fn is_project_root(folder: &Path) -> bool {
    let Ok(entries) = fs::read_dir(folder) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        PROJECT_MARKERS.contains(&name.as_ref())
            || PROJECT_BUNDLE_EXTENSIONS.contains(&extension(Path::new(name.as_ref())).as_str())
    })
}

const CAMERA_PREFIXES: &[&str] =
    &["img", "dsc", "pxl", "mvimg", "vid", "gopr", "dji", "photo", "whatsapp", "screenshot"];

/// Machine-made names like "clip_000123", "frame-0042" or "3f9a1c0e-…", but not camera names like "IMG_1234".
pub fn looks_machine_named(stem: &str) -> bool {
    static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{8,}([-_][0-9a-f]{4,})*$").unwrap());
    static NUMBERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z]{0,16}[-_. ]?\d{3,}([-_.]\d+)*$").unwrap());
    let name = stem.to_lowercase();
    if CAMERA_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
        return false;
    }
    HEX.is_match(&name) || NUMBERED.is_match(&name)
}

/// One file the dataset check looks at.
pub struct DatasetFile<'a> {
    pub folder: &'a str,
    pub stem: &'a str,
    pub kind: FileKind,
}

/// Folders with hundreds of same-kind, machine-named media files: training data, not your stuff. They're skipped
/// (and reported) instead of being indexed silently. Roots you pick yourself are never datasets.
pub fn dataset_folders(files: &[DatasetFile], roots: &HashSet<String>, minimum: usize) -> HashMap<String, usize> {
    let mut groups: HashMap<(&str, FileKind), Vec<&str>> = HashMap::new();
    for file in files {
        if matches!(file.kind, FileKind::Image | FileKind::Audio | FileKind::Video) && !roots.contains(file.folder) {
            groups.entry((file.folder, file.kind)).or_default().push(file.stem);
        }
    }
    let mut datasets: HashMap<String, usize> = HashMap::new();
    for ((folder, _), stems) in groups {
        if stems.len() < minimum {
            continue;
        }
        let machine = stems.iter().filter(|stem| looks_machine_named(stem)).count();
        if machine as f64 / stems.len() as f64 >= 0.7 {
            *datasets.entry(folder.to_string()).or_default() += stems.len();
        }
    }
    datasets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_is_not_video() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.ts");
        fs::write(&file, "export const x: number = 1;\n").unwrap();
        assert_eq!(classify(&file, &IndexOptions::default()), Classification::Skip("code or data file"));
    }

    #[test]
    fn real_transport_stream_is_video() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0xFFu8; 188 * 4];
        for packet in 0..4 {
            bytes[packet * 188] = 0x47;
        }
        let file = dir.path().join("clip.ts");
        fs::write(&file, bytes).unwrap();
        assert_eq!(classify(&file, &IndexOptions::default()), Classification::Index(FileKind::Video));
    }

    #[test]
    fn code_can_be_included() {
        let options = IndexOptions { include_code: true, ..Default::default() };
        assert_eq!(classify(Path::new("/x/main.py"), &options), Classification::Index(FileKind::Doc));
    }

    #[test]
    fn kinds_by_extension() {
        let options = IndexOptions::default();
        assert_eq!(classify(Path::new("/x/IMG_1.HEIC"), &options), Classification::Index(FileKind::Image));
        assert_eq!(
            classify(Path::new("/x/2026-10-11-123456_hyprshot.png"), &options),
            Classification::Index(FileKind::Screenshot)
        );
        assert_eq!(classify(Path::new("/x/talk.opus"), &options), Classification::Index(FileKind::Audio));
        assert_eq!(classify(Path::new("/x/trip.mkv"), &options), Classification::Index(FileKind::Video));
        assert_eq!(classify(Path::new("/x/lease.pdf"), &options), Classification::Index(FileKind::Pdf));
        assert_eq!(classify(Path::new("/x/a.zip"), &options), Classification::Ignore);
    }

    #[test]
    fn machine_names() {
        assert!(looks_machine_named("clip_000123"));
        assert!(looks_machine_named("frame-0042"));
        assert!(looks_machine_named("3f9a1c0e-12ab-4cde"));
        assert!(looks_machine_named("000123"));
        assert!(!looks_machine_named("IMG_1234"));
        assert!(!looks_machine_named("PXL_20260101_123456789"));
        assert!(!looks_machine_named("Screenshot 2026-10-01 at 9.41.07 PM"));
        assert!(!looks_machine_named("Tokyo itinerary"));
    }

    #[test]
    fn dataset_needs_many_machine_named_files() {
        let clips: Vec<String> = (0..400).map(|n| format!("clip_{n:06}")).collect();
        let photos: Vec<String> = (0..400).map(|n| format!("IMG_{}", 1000 + n)).collect();
        let few: Vec<String> = (0..50).map(|n| format!("clip_{n}")).collect();
        let mut files = Vec::new();
        files.extend(clips.iter().map(|stem| DatasetFile { folder: "/x/clips", stem, kind: FileKind::Audio }));
        files.extend(photos.iter().map(|stem| DatasetFile { folder: "/x/camera", stem, kind: FileKind::Image }));
        files.extend(few.iter().map(|stem| DatasetFile { folder: "/x/few", stem, kind: FileKind::Audio }));
        let datasets = dataset_folders(&files, &HashSet::new(), 300);
        assert_eq!(datasets, HashMap::from([("/x/clips".to_string(), 400)]));
    }

    #[test]
    fn secrets_are_never_read() {
        assert!(is_secret("id_ed25519"));
        assert!(is_secret(".env.local"));
        assert!(is_secret("server.PEM"));
        assert!(!is_secret("main.rs"));
        let options = IndexOptions { code: true, ..Default::default() };
        assert_eq!(classify_code(Path::new("/r/app.min.js"), &options), Classification::Skip("minified"));
        assert_eq!(classify_code(Path::new("/r/LICENSE"), &options), Classification::Skip("license"));
        assert_eq!(classify_code(Path::new("/r/data.json"), &options), Classification::Skip("data file"));
        assert_eq!(classify_code(Path::new("/r/PKGBUILD"), &options), Classification::Index(FileKind::Code));
    }
}
