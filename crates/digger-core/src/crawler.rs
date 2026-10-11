//! Walks the chosen folders and applies `rules`. Reads metadata only, never file contents (except the 600-byte sniff
//! that tells a real MPEG-TS video from a TypeScript file).
//!
//! For the code index (`IndexOptions::code`) it walks the code folders instead: code files only
//! (`rules::classify_code`), in a git repo only those git keeps (`GitRepos`), and none in vendored or built folders.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::folder_selection::{self, is_inside, real_path};
use crate::model::{FileKind, IndexOptions};
use crate::rules::{self, Classification, DatasetFile};
use crate::store::Candidate;

#[derive(Debug, Default, Clone)]
pub struct CrawlReport {
    pub candidates: Vec<Candidate>,
    /// Reason → files.
    pub skipped: BTreeMap<String, usize>,
    /// Reason → folders not walked into.
    pub skipped_folders: BTreeMap<String, usize>,
    /// Folder → files skipped as a dataset.
    pub datasets: BTreeMap<String, usize>,
}

/// The path as the walk reports it, without "." and ".." and a trailing slash.
pub fn standardized(path: &Path) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.to_str().unwrap_or("").split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

pub fn crawl(roots: &[PathBuf], options: &IndexOptions) -> CrawlReport {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut report = CrawlReport::default();
    let mut project_roots: HashSet<String> = HashSet::new();
    let mut repos = GitRepos::default();
    let mut found: Vec<Candidate> = Vec::new();
    let mut found_paths: HashSet<String> = HashSet::new(); // a folder inside another root would be walked twice
    let roots: Vec<String> = roots.iter().map(|root| standardized(root)).collect();
    let root_paths: HashSet<String> = roots.iter().cloned().collect();

    for root in &roots {
        if options.code {
            repos.enter_root(root);
        } else if rules::is_project_root(Path::new(root)) {
            project_roots.insert(root.clone());
        }
        // A skip applies inside the chosen folders that contain it: a folder chosen inside a skipped one is walked
        // whole (`FolderSelection`).
        let real = real_path(root);
        let skips: Vec<String> = options
            .excluded_folders
            .iter()
            .filter(|skip| is_inside(skip, root) || is_inside(skip, &real))
            .flat_map(|skip| [skip.clone(), real_path(skip)])
            .collect();
        let mut stack = vec![root.clone()];
        while let Some(folder) = stack.pop() {
            let Ok(entries) = fs::read_dir(&folder) else { continue };
            let mut entries: Vec<(String, fs::Metadata)> = entries
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_str()?.to_string();
                    let meta = entry.metadata().ok()?; // not following links
                    (!name.starts_with('.')).then(|| (format!("{}/{name}", folder.trim_end_matches('/')), meta))
                })
                .collect();
            entries.sort_by(|a, b| b.0.cmp(&a.0)); // popped in name order
            for (path, meta) in entries {
                if meta.is_dir() {
                    let reason = rules::skip_reason_for_folder(&path, &home, &skips)
                        .or_else(|| if options.code { rules::code_skip_reason(Path::new(&path)) } else { None });
                    if let Some(reason) = reason {
                        *report.skipped_folders.entry(reason.into()).or_default() += 1;
                        continue;
                    }
                    if options.code {
                        repos.enter(&path);
                    } else if rules::is_project_root(Path::new(&path)) {
                        project_roots.insert(path.clone());
                    }
                    stack.push(path);
                    continue;
                }
                let classification = if options.code {
                    rules::classify_code(Path::new(&path), options)
                } else {
                    rules::classify(Path::new(&path), options)
                };
                let kind = match classification {
                    Classification::Ignore => {
                        *report.skipped.entry("other type".into()).or_default() += 1;
                        continue;
                    }
                    Classification::Skip(reason) => {
                        *report.skipped.entry(reason.into()).or_default() += 1;
                        continue;
                    }
                    Classification::Index(kind) => kind,
                };
                if !meta.is_file() {
                    *report.skipped.entry("link or special file".into()).or_default() += 1;
                    continue;
                }
                if options.code {
                    // Too big to be written by hand; a notebook's file holds its outputs too.
                    let limit = if rules::extension(Path::new(&path)) == "ipynb" {
                        options.max_notebook_bytes
                    } else {
                        options.max_code_bytes
                    };
                    if meta.len() > limit {
                        *report.skipped.entry("too big to be written by hand".into()).or_default() += 1;
                        continue;
                    }
                    if repos.ignores(&path) {
                        *report.skipped.entry("ignored by git".into()).or_default() += 1;
                        continue;
                    }
                }
                if !found_paths.insert(path.clone()) {
                    continue;
                }
                let mtime = meta.mtime() as f64 + meta.mtime_nsec() as f64 / 1e9;
                found.push(Candidate {
                    path,
                    kind,
                    size: meta.len() as i64,
                    mtime,
                    inode: meta.ino(),
                    device: meta.dev(),
                });
            }
        }
    }

    // Inside a code project only screenshots count (assets, fixtures and datasets don't).
    let mut in_project: HashMap<String, bool> = HashMap::new();
    fn is_in_project(folder: &str, roots: &HashSet<String>, known: &mut HashMap<String, bool>) -> bool {
        if let Some(answer) = known.get(folder) {
            return *answer;
        }
        let parent = folder_selection::parent(folder);
        let answer =
            roots.contains(folder) || (parent != folder && parent.len() > 1 && is_in_project(parent, roots, known));
        known.insert(folder.to_string(), answer);
        answer
    }
    let mut kept = Vec::new();
    for file in found {
        let folder = folder_selection::parent(&file.path);
        if !options.code && file.kind != FileKind::Screenshot && is_in_project(folder, &project_roots, &mut in_project)
        {
            *report.skipped.entry("inside a code project".into()).or_default() += 1;
        } else {
            kept.push(file);
        }
    }

    if !options.include_datasets && !options.code {
        let stems: Vec<(String, String)> = kept
            .iter()
            .map(|file| {
                let path = Path::new(&file.path);
                let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("").to_string();
                (folder_selection::parent(&file.path).to_string(), stem)
            })
            .collect();
        let files: Vec<DatasetFile> = kept
            .iter()
            .zip(&stems)
            .map(|(file, (folder, stem))| DatasetFile { folder, stem, kind: file.kind })
            .collect();
        let datasets = rules::dataset_folders(&files, &root_paths, 300);
        if !datasets.is_empty() {
            kept.retain(|file| {
                let skip = datasets.contains_key(folder_selection::parent(&file.path))
                    && matches!(file.kind, FileKind::Image | FileKind::Audio | FileKind::Video);
                if skip {
                    *report.skipped.entry("dataset".into()).or_default() += 1;
                }
                !skip
            });
            report.datasets = datasets.into_iter().collect();
        }
    }
    report.candidates = kept;
    report
}

/// The git repos a code crawl walks through, and the files each keeps: a file git ignores (a build, an environment,
/// data, a secret listed in .gitignore) isn't code search's to read. A folder that isn't in a repo, or a machine
/// without git, has no list: everything there counts.
#[derive(Default)]
struct GitRepos {
    /// Repo folder → the files git keeps in it (paths relative to it), or None when git couldn't say.
    files: HashMap<String, Option<HashSet<String>>>,
    /// Folder → the repo it's in (None: none), as the walk found them.
    repo_of: HashMap<String, Option<String>>,
}

impl GitRepos {
    /// A chosen folder: a repo itself, or inside one (a repo's subfolder chosen on its own).
    fn enter_root(&mut self, path: &str) {
        let mut folder = path.to_string();
        loop {
            if Path::new(&folder).join(".git").exists() {
                self.files.insert(folder.clone(), git_files(&folder));
                self.repo_of.insert(path.to_string(), Some(folder));
                return;
            }
            let parent = folder_selection::parent(&folder).to_string();
            if parent == folder || parent.len() <= 1 {
                break;
            }
            folder = parent;
        }
        self.repo_of.insert(path.to_string(), None);
    }

    /// A folder the walk goes into: a repo of its own when it has a .git folder.
    fn enter(&mut self, path: &str) {
        if Path::new(path).join(".git").is_dir() {
            self.files.insert(path.to_string(), git_files(path));
            self.repo_of.insert(path.to_string(), Some(path.to_string()));
        }
    }

    /// The repo `folder` is in, from the closest folder the walk has seen.
    fn repo(&mut self, folder: &str) -> Option<String> {
        if let Some(known) = self.repo_of.get(folder) {
            return known.clone();
        }
        let parent = folder_selection::parent(folder);
        let answer = if parent != folder && parent.len() > 1 { self.repo(parent) } else { None };
        self.repo_of.insert(folder.to_string(), answer.clone());
        answer
    }

    /// Whether git ignores the file at `path` (in a repo whose files git listed).
    fn ignores(&mut self, path: &str) -> bool {
        let Some(repo) = self.repo(folder_selection::parent(path)) else {
            return false;
        };
        let Some(Some(kept)) = self.files.get(&repo) else {
            return false;
        };
        !kept.contains(&path[repo.len() + 1..])
    }
}

/// The files git keeps in a repo: tracked ones, and new ones its .gitignore doesn't leave out. None when git can't
/// say (no git, not a repo, an error).
fn git_files(repo: &str) -> Option<HashSet<String>> {
    let output = Command::new("git")
        .args(["-C", repo, "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        // Reading only: never wait for, or take, the repo's lock (an editor's git may be busy in it).
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .filter_map(|name| std::str::from_utf8(name).ok().map(String::from))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }

    fn names(report: &CrawlReport, root: &Path) -> Vec<String> {
        let prefix = format!("{}/", standardized(root));
        let mut names: Vec<String> =
            report.candidates.iter().map(|file| file.path.trim_start_matches(&prefix).to_string()).collect();
        names.sort();
        names
    }

    #[test]
    fn nested_roots_are_crawled_once() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "inner/trip.txt", "notes about the trip");
        let report = crawl(&[dir.path().to_path_buf(), dir.path().join("inner")], &IndexOptions::default());
        assert_eq!(names(&report, dir.path()), ["inner/trip.txt"]);
    }

    #[test]
    fn crawls_leave_skips_out_but_walk_chosen_folders_inside_them() {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in [
            ("D/top.txt", "top"),
            ("D/Media/film.txt", "film"),
            ("D/Media/Courses/week1.txt", "week one"),
            ("D/Media/Courses/Old/old.txt", "old"),
            ("D/.hidden/secret.txt", "no"),
        ] {
            write(dir.path(), path, text);
        }
        let downloads = dir.path().join("D");
        let courses = downloads.join("Media/Courses");
        let options = IndexOptions {
            excluded_folders: vec![standardized(&downloads.join("Media")), standardized(&courses.join("Old"))],
            ..Default::default()
        };
        let report = crawl(&[downloads, courses], &options);
        let mut found: Vec<String> =
            report.candidates.iter().map(|file| file.path.rsplit('/').next().unwrap().to_string()).collect();
        found.sort();
        assert_eq!(found, ["top.txt", "week1.txt"]);
    }

    #[test]
    fn code_crawl_keeps_what_git_keeps() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "app/main.py", "print(1)\n");
        write(root, "app/README.md", "# App\n");
        write(root, "app/.gitignore", "build/\n*.log\nscratch.py\n");
        write(root, "app/scratch.py", "x = 1\n"); // ignored by git
        write(root, "app/notes/todo.md", "- ship\n"); // new, not ignored: kept
        write(root, "app/build/out.py", "x = 2\n"); // ignored (and a built folder)
        write(root, "app/vendor/lib/core.py", "x = 3\n"); // vendored
        write(root, "app/node_modules/pad/index.js", "x\n"); // a dependency
        write(root, "app/data.json", "{}\n"); // data
        write(root, "app/big.py", &"x = 1\n".repeat(200_000)); // 1.2 MB: made by a program
        write(root, "app/sub/.git", "gitdir: ../.git/modules/sub\n"); // a submodule
        write(root, "app/sub/lib.py", "x = 4\n");
        write(root, "loose/backup.sh", "rsync\n"); // not in a repo: everything code counts
        Command::new("git").args(["-C", root.to_str().unwrap(), "init", "-q", "app"]).output().unwrap();
        let code = IndexOptions { code: true, ..Default::default() };
        let report = crawl(&[root.to_path_buf()], &code);
        assert_eq!(names(&report, root), ["app/README.md", "app/main.py", "app/notes/todo.md", "loose/backup.sh"]);
        assert_eq!(report.skipped.get("ignored by git"), Some(&1));
        assert_eq!(report.skipped_folders.get("vendored or built code"), Some(&2));
        assert_eq!(report.skipped_folders.get("submodule or worktree"), Some(&1));
        // A repo's subfolder chosen on its own still follows the repo's .gitignore.
        let sub = crawl(&[root.join("app")], &code);
        assert!(!sub.candidates.iter().any(|file| file.path.ends_with("scratch.py")));
        // The main index never takes code from a code project (only its screenshots).
        assert!(crawl(&[root.to_path_buf()], &IndexOptions::default()).candidates.is_empty());
    }
}
