//! Search results the way the panel and the window show them: rows that say where the match is and why, grouped by
//! kind, with the weaker ones folded away. No GTK here, so it's tested on its own.

use std::collections::BTreeMap;

use showme_core::code;
use showme_core::model::FileKind;
use showme_core::proto::Hit;

/// One result: a file, the part of it that matched, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultRow {
    pub path: String,
    pub name: String,
    /// "~/Downloads/Screenshots", or for code its place in the repo ("billing/payments").
    pub folder: String,
    pub kind: FileKind,
    /// page, page_image, chunk, image, ocr, frame, audio, lines, cells, name
    pub segment: String,
    pub segment_id: i64,
    /// A PDF page, seconds into a recording, a chunk, or a stretch of code's first line.
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    /// Code: the line to open at.
    pub focus: Option<f64>,
    /// The words that matched are marked « ».
    pub excerpt: Option<String>,
    pub matched_meaning: bool,
    pub matched_words: bool,
    pub modified: f64,
    pub size: i64,
    pub info: BTreeMap<String, String>,
    /// The query's words: what previews mark on a page or in a passage.
    pub terms: Vec<String>,
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// "/home/you/Documents" → "~/Documents".
pub fn tilde(path: &str) -> String {
    let home = home();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// "0:06", "1:02:03".
pub fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    if total >= 3600 {
        format!("{}:{:02}:{:02}", total / 3600, total / 60 % 60, total % 60)
    } else {
        format!("{}:{:02}", total / 60, total % 60)
    }
}

/// "2.1 MB", "640 KB".
pub fn bytes(size: i64) -> String {
    let size = size.max(0) as f64;
    match size {
        size if size >= 1e9 => format!("{:.1} GB", size / 1e9),
        size if size >= 1e6 => format!("{:.1} MB", size / 1e6),
        size if size >= 1e3 => format!("{:.0} KB", size / 1e3),
        size => format!("{size:.0} bytes"),
    }
}

/// "Oct 1, 2026".
pub fn date(seconds: f64) -> String {
    chrono::DateTime::from_timestamp(seconds as i64, 0)
        .map(|date| date.with_timezone(&chrono::Local).format("%b %-d, %Y").to_string())
        .unwrap_or_default()
}

pub fn kind_title(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Screenshot => "Screenshot",
        FileKind::Image => "Image",
        FileKind::Pdf => "PDF",
        FileKind::Doc => "Document",
        FileKind::Audio => "Audio",
        FileKind::Video => "Video",
        FileKind::Code => "Code",
    }
}

impl ResultRow {
    pub fn new(hit: &Hit, terms: &[String]) -> ResultRow {
        let kind: FileKind = hit.kind.parse().unwrap_or(FileKind::Doc);
        let (parent, name) = hit.path.rsplit_once('/').unwrap_or(("", &hit.path));
        // Code says where it is in its repo: "billing/payments".
        let folder = match hit.info.get("repo") {
            Some(repo) if kind == FileKind::Code && hit.path.starts_with(&format!("{repo}/")) => {
                let repo_parent = repo.rsplit_once('/').map_or("", |(parent, _)| parent);
                parent.get(repo_parent.len() + 1..).unwrap_or(parent).to_string()
            }
            _ => tilde(parent),
        };
        ResultRow {
            path: hit.path.clone(),
            name: name.to_string(),
            folder,
            kind,
            segment: hit.segment.clone(),
            segment_id: hit.segment_id,
            loc: hit.loc,
            loc_end: hit.loc_end,
            focus: hit.focus,
            excerpt: hit.excerpt.clone().filter(|excerpt| !excerpt.is_empty()),
            matched_meaning: hit.z.unwrap_or(0.0) >= 2.0,
            matched_words: hit.keyword_rank.is_some(),
            modified: hit.modified,
            size: hit.size,
            info: hit.info.clone(),
            terms: terms.to_vec(),
        }
    }

    /// The name to show: left to right as file managers show it, also when it starts in Arabic.
    pub fn title(&self) -> String {
        format!("\u{200E}{}", self.name)
    }

    /// The words keyword search found in the excerpt (marked « »), as they're written there.
    pub fn found_words(&self) -> Vec<String> {
        let mut words: Vec<String> = Vec::new();
        let Some(excerpt) = &self.excerpt else { return words };
        let mut rest = excerpt.as_str();
        while let Some(open) = rest.find('«') {
            let after = &rest[open + '«'.len_utf8()..];
            let Some(close) = after.find('»') else { break };
            let word = &after[..close];
            if !word.is_empty() && !words.iter().any(|known| known == word) {
                words.push(word.to_string());
            }
            rest = &after[close + '»'.len_utf8()..];
        }
        words
    }

    /// The PDF page that matched (from 1).
    pub fn page(&self) -> Option<u32> {
        (self.kind == FileKind::Pdf && matches!(self.segment.as_str(), "page" | "page_image"))
            .then_some(self.loc? as u32)
    }

    /// Seconds into a recording where the match starts.
    pub fn moment(&self) -> Option<f64> {
        (matches!(self.kind, FileKind::Video | FileKind::Audio) && matches!(self.segment.as_str(), "frame" | "audio"))
            .then_some(self.loc?)
    }

    /// The line a code result opens at: where the name is defined or the query's words are, else where its stretch
    /// starts (a notebook's at its top: editors can't be told a cell).
    pub fn code_line(&self) -> u32 {
        if self.segment == "lines" { self.focus.or(self.loc).unwrap_or(1.0).max(1.0) as u32 } else { 1 }
    }

    /// The page or moment that matched, as a badge: "p. 3", "0:06", "♪ 0:25" (a video's sound), "L12–40" (code).
    pub fn place(&self) -> Option<String> {
        let loc = self.loc?;
        let end = self.loc_end.unwrap_or(loc);
        match self.segment.as_str() {
            "page" | "page_image" => Some(format!("p. {}", loc as i64)),
            "frame" => Some(clock(loc)),
            "audio" if self.kind == FileKind::Video => Some(format!("♪ {}", clock(loc))),
            "audio" => Some(clock(loc)),
            "lines" if loc as i64 == end as i64 => Some(format!("L{}", loc as i64)),
            "lines" => Some(format!("L{}–{}", loc as i64, end as i64)),
            "cells" => self.span(),
            _ => None,
        }
    }

    /// The lines (or a notebook's cells) of code that matched: "lines 12–40", "cell 3".
    pub fn span(&self) -> Option<String> {
        let (first, last) = (self.loc? as i64, self.loc_end.unwrap_or(self.loc?) as i64);
        let (one, many) = match self.segment.as_str() {
            "lines" => ("line", "lines"),
            "cells" => ("cell", "cells"),
            _ => return None,
        };
        Some(if first == last { format!("{one} {first}") } else { format!("{many} {first}–{last}") })
    }

    pub fn duration(&self) -> Option<f64> {
        self.info.get("duration").and_then(|value| value.parse().ok())
    }

    /// Why it's here, in a few words: "Matches the picture", "Matches the frame at 0:06".
    pub fn reason(&self) -> String {
        if !self.matched_meaning {
            return if self.matched_words { "Has your words".into() } else { "Matches its name".into() };
        }
        let place = self.loc.map(clock).unwrap_or_default();
        let page = self.loc.map(|loc| format!("page {}", loc as i64)).unwrap_or_else(|| "a page".into());
        match self.segment.as_str() {
            "image" if self.kind == FileKind::Screenshot => "Matches what's on screen".into(),
            "image" => "Matches the picture".into(),
            "ocr" => "Matches the text in it".into(),
            "frame" => format!("Matches the frame at {place}"),
            "audio" if self.kind == FileKind::Video => format!("Matches the sound at {place}"),
            "audio" => format!("Matches what's heard at {place}"),
            "page" => format!("Matches the text on {page}"),
            "page_image" => format!("Matches {page}"),
            "chunk" => "Matches its text".into(),
            "lines" | "cells" => format!("Matches {}", self.span().unwrap_or_else(|| "the code".into())),
            _ => "Matches its name".into(),
        }
    }

    /// "Screenshot · Oct 1, 2026 · 1440 × 900 · 2.1 MB", "PDF · … · 463 pages, 120 read so far · 23 MB",
    /// "Python · 63 lines · Oct 1, 2026 · 2 KB".
    pub fn details(&self) -> String {
        let number = |key: &str| self.info.get(key).and_then(|value| value.parse::<u64>().ok());
        let mut parts: Vec<String> = Vec::new();
        if self.kind == FileKind::Code {
            parts.push(code::language(&self.path));
            if let Some(cells) = number("cells") {
                parts.push(if cells == 1 { "1 cell".into() } else { format!("{cells} cells") });
            } else if let Some(lines) = number("lines") {
                parts.push(if lines == 1 { "1 line".into() } else { format!("{lines} lines") });
            }
            parts.push(date(self.modified));
        } else {
            parts.push(kind_title(self.kind).into());
            parts.push(date(self.modified));
            if let (Some(width), Some(height)) = (self.info.get("width"), self.info.get("height")) {
                parts.push(format!("{width} × {height}"));
            }
            // A long file still being read: what isn't read yet can't match.
            let read_to = number("read_to");
            if let Some(pages) = number("pages") {
                let pages = if pages == 1 { "1 page".to_string() } else { format!("{pages} pages") };
                parts.push(pages + &read_to.map(|read| format!(", {read} read so far")).unwrap_or_default());
            } else if read_to.is_some() {
                parts.push("partly read so far".into());
            }
            if let Some(duration) = self.duration() {
                parts.push(clock(duration));
            }
        }
        if self.size > 0 {
            parts.push(bytes(self.size));
        }
        parts.join(" · ")
    }
}

/// How results are grouped in the panel, and the kind filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    Screenshots,
    Images,
    Documents,
    Video,
    Audio,
    Code,
}

impl Group {
    /// The kind filters, Alt+2 to Alt+7 (Alt+1 is all).
    pub const ALL: [Group; 6] =
        [Group::Screenshots, Group::Images, Group::Documents, Group::Video, Group::Audio, Group::Code];

    pub fn of(kind: FileKind) -> Group {
        match kind {
            FileKind::Screenshot => Group::Screenshots,
            FileKind::Image => Group::Images,
            FileKind::Pdf | FileKind::Doc => Group::Documents,
            FileKind::Video => Group::Video,
            FileKind::Audio => Group::Audio,
            FileKind::Code => Group::Code,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Group::Screenshots => "Screenshots",
            Group::Images => "Images",
            Group::Documents => "Documents",
            Group::Video => "Video",
            Group::Audio => "Audio",
            Group::Code => "Code",
        }
    }

    /// The kinds a filter asks the daemon for.
    pub fn kinds(self) -> Vec<String> {
        let kinds: &[FileKind] = match self {
            Group::Screenshots => &[FileKind::Screenshot],
            Group::Images => &[FileKind::Image],
            Group::Documents => &[FileKind::Pdf, FileKind::Doc],
            Group::Video => &[FileKind::Video],
            Group::Audio => &[FileKind::Audio],
            Group::Code => &[FileKind::Code],
        };
        kinds.iter().map(|kind| kind.as_str().to_string()).collect()
    }

    /// An icon from the icon theme (Adwaita's names, which Omarchy's icon themes have too).
    pub fn icon(self) -> &'static str {
        match self {
            Group::Screenshots => "camera-photo-symbolic",
            Group::Images => "image-x-generic-symbolic",
            Group::Documents => "x-office-document-symbolic",
            Group::Video => "video-x-generic-symbolic",
            Group::Audio => "audio-x-generic-symbolic",
            Group::Code => "text-x-script-symbolic",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResultGroup {
    pub group: Group,
    pub rows: Vec<ResultRow>,
    /// Strong results of this kind, shown or not (the panel shows a few).
    pub total: usize,
}

/// Search results the way they're shown: the strong ones grouped by kind (the best hit's group first, as many rows per
/// group as there's room for), and the weaker ones (below half the top score) folded underneath.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultSet {
    pub groups: Vec<ResultGroup>,
    /// The strong ones in score order (the window's grid; the panel shows `groups`).
    pub ranked: Vec<ResultRow>,
    pub weaker: Vec<ResultRow>,
}

impl ResultSet {
    pub fn new(hits: &[Hit], terms: &[String], rows_per_group: usize) -> ResultSet {
        let mut set = ResultSet::default();
        for hit in hits {
            let row = ResultRow::new(hit, terms);
            if hit.strong { set.ranked.push(row) } else { set.weaker.push(row) }
        }
        for row in &set.ranked {
            let group = Group::of(row.kind);
            match set.groups.iter_mut().find(|known| known.group == group) {
                Some(known) => {
                    known.total += 1;
                    if known.rows.len() < rows_per_group {
                        known.rows.push(row.clone());
                    }
                }
                None => set.groups.push(ResultGroup { group, rows: vec![row.clone()], total: 1 }),
            }
        }
        set
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.weaker.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, kind: &str, segment: &str, loc: Option<f64>, strong: bool) -> Hit {
        Hit {
            path: path.into(),
            kind: kind.into(),
            segment: segment.into(),
            segment_id: 1,
            score: 3.0,
            strong,
            z: Some(3.0),
            keyword_rank: None,
            loc,
            loc_end: loc.map(|loc| loc + 3.0),
            focus: None,
            excerpt: Some("the «zebra» crossing at «dawn»".into()),
            modified: 1_790_000_000.0,
            size: 2_100_000,
            info: BTreeMap::from([("duration".into(), "192".into())]),
        }
    }

    #[test]
    fn places_reasons_and_words() {
        let video = ResultRow::new(&hit("/v/safari.mp4", "video", "frame", Some(6.0), true), &[]);
        assert_eq!(video.place().as_deref(), Some("0:06"));
        assert_eq!(video.reason(), "Matches the frame at 0:06");
        assert_eq!(video.moment(), Some(6.0));
        let sound = ResultRow::new(&hit("/v/safari.mp4", "video", "audio", Some(25.0), true), &[]);
        assert_eq!(sound.place().as_deref(), Some("♪ 0:25"));
        let page = ResultRow::new(&hit("/d/lease.pdf", "pdf", "page", Some(3.0), true), &[]);
        assert_eq!((page.place().as_deref(), page.page()), (Some("p. 3"), Some(3)));
        assert_eq!(page.found_words(), ["zebra", "dawn"]);
        let code = ResultRow::new(&hit("/c/app/main.rs", "code", "lines", Some(12.0), true), &[]);
        assert_eq!(code.place().as_deref(), Some("L12–15"));
        assert_eq!(code.span().as_deref(), Some("lines 12–15"));
        assert_eq!(code.code_line(), 12);
        assert!(video.details().starts_with("Video · "));
        assert!(video.details().ends_with("3:12 · 2.1 MB"));
    }

    #[test]
    fn groups_follow_the_best_hit_and_weaker_ones_fold() {
        let hits = [
            hit("/v/a.mp4", "video", "frame", Some(0.0), true),
            hit("/p/b.jpg", "image", "image", None, true),
            hit("/v/c.mp4", "video", "frame", Some(3.0), true),
            hit("/v/d.mp4", "video", "frame", Some(3.0), true),
            hit("/d/e.pdf", "pdf", "page", Some(1.0), false),
        ];
        let set = ResultSet::new(&hits, &[], 2);
        assert_eq!(set.groups.iter().map(|group| group.group).collect::<Vec<_>>(), [Group::Video, Group::Images]);
        assert_eq!((set.groups[0].rows.len(), set.groups[0].total), (2, 3));
        assert_eq!(set.weaker.len(), 1);
        assert_eq!(set.ranked.len(), 4);
    }

    #[test]
    fn code_shows_its_place_in_the_repo() {
        let mut code = hit("/home/x/work/billing/payments/retry.py", "code", "lines", Some(1.0), true);
        code.info.insert("repo".into(), "/home/x/work/billing".into());
        assert_eq!(ResultRow::new(&code, &[]).folder, "billing/payments");
    }
}
