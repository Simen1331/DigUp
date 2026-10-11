//! Which folders Digger searches: the folders you chose, minus the folders you skipped inside them. A skip applies
//! inside the chosen folders that contain it, and a chosen folder is searched whole even inside a skipped one (skip
//! Downloads/Media, still choose Downloads/Media/Courses). The crawler, the search filter and the folder lists all
//! follow this rule.

use std::path::Path;

/// A folder's checkbox: all of it, some of its subfolders, or none.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Mark {
    On,
    Mixed,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderSelection {
    chosen: Vec<String>,
    skipped: Vec<String>,
    /// Folders already searched (when Settings adds more): whatever is clicked, they stay.
    kept: Vec<String>,
}

impl FolderSelection {
    pub fn new(chosen: &[&str], skipped: &[&str], kept: &[&str]) -> FolderSelection {
        let own = |paths: &[&str]| unique(paths.iter().map(|path| path.to_string()).collect());
        FolderSelection { chosen: own(chosen), skipped: own(skipped), kept: own(kept) }
    }

    pub fn chosen(&self) -> &[String] {
        &self.chosen
    }

    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    fn kept_and_chosen(&self) -> Vec<String> {
        self.kept.iter().chain(&self.chosen).cloned().collect()
    }

    /// Whether `path` (a folder or a file) is searched.
    pub fn is_searched(&self, path: &str) -> bool {
        Self::is_searched_in(path, &self.kept_and_chosen(), &self.skipped)
    }

    pub fn is_searched_in(path: &str, folders: &[String], skipped: &[String]) -> bool {
        folders.iter().any(|folder| {
            (path == folder || is_inside(path, folder))
                && !skipped.iter().any(|skip| is_inside(skip, folder) && (path == skip || is_inside(path, skip)))
        })
    }

    /// The closest chosen (or kept) folder that searches `path`.
    pub fn searched_as(&self, path: &str) -> Option<String> {
        self.kept_and_chosen()
            .into_iter()
            .filter(|folder| {
                (path == folder || is_inside(path, folder))
                    && Self::is_searched_in(path, std::slice::from_ref(folder), &self.skipped)
            })
            .max_by_key(|folder| folder.len())
    }

    /// What the indexer gets: kept and chosen folders, without those another one already searches whole.
    pub fn folders(&self) -> Vec<String> {
        let all = unique(self.kept_and_chosen());
        all.iter()
            .filter(|folder| {
                !all.iter().any(|other| {
                    other != *folder
                        && is_inside(folder, other)
                        && Self::is_searched_in(folder, std::slice::from_ref(other), &self.skipped)
                })
            })
            .cloned()
            .collect()
    }

    /// The skips that leave something out (inside a folder that's searched).
    pub fn effective_skips(&self) -> Vec<String> {
        let folders = self.folders();
        self.skipped.iter().filter(|skip| folders.iter().any(|folder| is_inside(skip, folder))).cloned().collect()
    }

    /// A folder's checkbox: mixed when some of its subfolders are skipped (it's searched) or chosen (it isn't).
    pub fn mark(&self, folder: &str) -> Mark {
        if self.is_searched(folder) {
            let partly = self.skipped.iter().any(|skip| parent(skip) == folder && !self.is_searched(skip));
            return if partly { Mark::Mixed } else { Mark::On };
        }
        if self.kept_and_chosen().iter().any(|path| parent(path) == folder) { Mark::Mixed } else { Mark::Off }
    }

    /// Checks or unchecks a folder and its subfolders: all of it, or none of it.
    pub fn set_folder(&mut self, folder: &str, on: bool) {
        self.skipped.retain(|path| parent(path) != folder);
        self.chosen.retain(|path| parent(path) != folder);
        self.set_subfolder(folder, on);
    }

    /// Checks or unchecks one folder, leaving the choices inside it as they are.
    pub fn set_subfolder(&mut self, folder: &str, on: bool) {
        if on {
            self.skipped.retain(|path| path != folder);
            if !self.is_searched(folder) {
                self.chosen.push(folder.to_string());
            }
        } else {
            self.chosen.retain(|path| path != folder);
            if self.is_searched(folder) {
                self.skipped.push(folder.to_string());
            }
        }
    }

    /// Stops skipping `folder` (without choosing it).
    pub fn unskip(&mut self, folder: &str) {
        self.skipped.retain(|path| path != folder);
    }
}

/// Whether `path` is inside `folder`, comparing whole names ("/a/bc" isn't inside "/a/b").
pub fn is_inside(path: &str, folder: &str) -> bool {
    if folder.ends_with('/') {
        path.starts_with(folder)
    } else {
        path.len() > folder.len() && path.starts_with(folder) && path.as_bytes()[folder.len()] == b'/'
    }
}

pub fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(at) => &path[..at],
        None => "",
    }
}

/// The path with symbolic links resolved, as a walk of the folder reports it.
pub fn real_path(path: &str) -> String {
    std::fs::canonicalize(Path::new(path))
        .ok()
        .and_then(|real| real.to_str().map(String::from))
        .unwrap_or_else(|| path.to_string())
}

fn unique(paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    paths.into_iter().filter(|path| seen.insert(path.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchecking_a_subfolder_skips_it() {
        let mut selection = FolderSelection::new(&["/D"], &[], &[]);
        selection.set_subfolder("/D/Courses", false);
        assert_eq!(selection.skipped(), ["/D/Courses"]);
        assert_eq!(selection.mark("/D"), Mark::Mixed);
        assert!(!selection.is_searched("/D/Courses/week1.mp4"));
        assert!(selection.is_searched("/D/Books/a.pdf"));
        selection.set_folder("/D", true); // a click on a mixed checkbox: all of it
        assert!(selection.skipped().is_empty());
        assert_eq!(selection.mark("/D"), Mark::On);
    }

    #[test]
    fn checking_a_subfolder_chooses_it_alone() {
        let mut selection = FolderSelection::default();
        selection.set_subfolder("/D/Receipts", true);
        assert_eq!(selection.chosen(), ["/D/Receipts"]);
        assert_eq!(selection.mark("/D"), Mark::Mixed);
        assert!(selection.is_searched("/D/Receipts/may.pdf"));
        assert!(!selection.is_searched("/D/other.pdf"));
        assert_eq!(selection.folders(), ["/D/Receipts"]);
        selection.set_folder("/D", true);
        assert_eq!(selection.chosen(), ["/D"]); // the subfolder is covered now
        selection.set_folder("/D", false);
        assert!(selection.chosen().is_empty());
        assert_eq!(selection.mark("/D"), Mark::Off);
    }

    #[test]
    fn unchecking_a_folder_drops_its_subfolder_choices_but_not_deeper_skips() {
        let mut selection = FolderSelection::new(&["/D"], &["/D/A", "/D/B/old"], &[]);
        selection.set_folder("/D", false);
        assert!(selection.chosen().is_empty());
        assert_eq!(selection.skipped(), ["/D/B/old"]);
        assert!(selection.effective_skips().is_empty()); // nothing around it is searched
    }

    #[test]
    fn a_chosen_folder_inside_a_skipped_one_is_searched_whole() {
        let mut selection = FolderSelection::new(&["/D"], &["/D/Media"], &[]);
        selection.set_subfolder("/D/Media/Courses", true);
        assert_eq!(selection.chosen(), ["/D", "/D/Media/Courses"]);
        assert!(selection.is_searched("/D/Media/Courses/week1/a.mp4"));
        assert!(!selection.is_searched("/D/Media/film.mp4"));
        assert_eq!(selection.folders(), ["/D", "/D/Media/Courses"]); // not covered by /D: Media is skipped
        selection.set_subfolder("/D/Media/Courses/old", false);
        assert!(!selection.is_searched("/D/Media/Courses/old/b.mp4"));
    }

    #[test]
    fn folders_inside_another_chosen_one_are_covered() {
        let selection = FolderSelection::new(&["/D/A", "/D", "/E"], &[], &[]);
        assert_eq!(selection.folders(), ["/D", "/E"]);
        assert_eq!(selection.searched_as("/D/A/x.png").as_deref(), Some("/D/A")); // the closest
    }

    #[test]
    fn kept_folders_stay_whatever_is_clicked() {
        let mut selection = FolderSelection::new(&[], &[], &["/D/Receipts"]);
        assert_eq!(selection.mark("/D"), Mark::Mixed);
        selection.set_folder("/D", true);
        assert_eq!(selection.folders(), ["/D"]);
        selection.set_folder("/D", false);
        assert_eq!(selection.folders(), ["/D/Receipts"]);
        assert_eq!(selection.mark("/D"), Mark::Mixed);
    }

    #[test]
    fn checking_a_skipped_folder_takes_the_skip_back() {
        let mut selection = FolderSelection::new(&[], &["/H/Movies"], &["/H"]);
        assert!(!selection.is_searched("/H/Movies"));
        selection.set_folder("/H/Movies", true);
        assert!(selection.skipped().is_empty());
        assert!(selection.chosen().is_empty()); // searched as part of /H again
        selection.set_folder("/H/Movies", false);
        assert_eq!(selection.skipped(), ["/H/Movies"]);
    }

    #[test]
    fn paths_are_compared_by_whole_names() {
        assert!(!is_inside("/a/bc", "/a/b"));
        assert!(is_inside("/a/b/c", "/a/b"));
        assert!(!FolderSelection::new(&["/a/b"], &[], &[]).is_searched("/a/bc/x"));
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/a"), "/");
    }
}
