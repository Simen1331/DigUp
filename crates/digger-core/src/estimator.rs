//! How long the first pass over some folders takes, from what the crawl found and a quick look at each file: PDF page
//! counts (pdfium reads only the page tree) and media durations (ffprobe). Nothing is decoded or embedded. With many
//! PDFs or recordings, a sample is looked at and the rest estimated from their sizes.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::model::{FileKind, IndexOptions};
use crate::store::Candidate;
use crate::{media, pdf};

/// Seconds of indexing per unit, measured on an M4 Pro with llama.cpp (Q8_0) while files are read alongside the
/// model (DigUp's numbers, 2026-10-07). A desktop GPU through Vulkan is about as fast; on the CPU, expect several
/// times longer.
#[derive(Debug, Clone, Copy)]
pub struct Costs {
    pub image: f64,
    /// Its OCR runs alongside and sets the pace.
    pub screenshot: f64,
    pub pdf_page: f64,
    pub doc_chunk: f64,
    /// A 30 s window every 25 s.
    pub audio_second: f64,
    /// A keyframe per shot (at most one per 3 s) and the soundtrack.
    pub video_second: f64,
    pub code_stretch: f64,
}

impl Default for Costs {
    fn default() -> Self {
        Costs {
            image: 0.12,
            screenshot: 0.32,
            pdf_page: 0.09,
            doc_chunk: 0.095,
            audio_second: 0.0089,
            video_second: 0.019,
            code_stretch: 0.037,
        }
    }
}

/// How big the code index gets: its words (~2.4 bytes of index a character of code) and a vector row a stretch.
const CODE_BYTES_PER_CHARACTER: f64 = 2.4;
const CODE_BYTES_PER_STRETCH: f64 = 1600.0;
/// Characters a stretch holds on average.
const CHARACTERS_PER_STRETCH: i64 = 1400;

/// PDFs and recordings looked at one by one; past this many, the rest are estimated from their sizes.
const SAMPLE: usize = 150;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Estimate {
    pub files: BTreeMap<String, usize>,
    /// Pages the first pass reads, and all of them.
    pub pdf_pages: usize,
    pub pdf_pages_total: usize,
    pub audio_seconds: f64,
    pub video_seconds: f64,
    /// Chunks the first pass reads.
    pub doc_chunks: usize,
    /// The first pass.
    pub seconds: f64,
    /// Long PDFs and documents get their first pages in the first pass, and the rest once everything else is in.
    pub long_files: usize,
    pub later_seconds: f64,
    pub code_stretches: usize,
    /// The code index's size on disk.
    pub bytes: f64,
}

impl std::ops::AddAssign for Estimate {
    fn add_assign(&mut self, other: Estimate) {
        for (kind, count) in other.files {
            *self.files.entry(kind).or_default() += count;
        }
        self.pdf_pages += other.pdf_pages;
        self.pdf_pages_total += other.pdf_pages_total;
        self.audio_seconds += other.audio_seconds;
        self.video_seconds += other.video_seconds;
        self.doc_chunks += other.doc_chunks;
        self.seconds += other.seconds;
        self.long_files += other.long_files;
        self.later_seconds += other.later_seconds;
        self.code_stretches += other.code_stretches;
        self.bytes += other.bytes;
    }
}

/// A measure of each file that takes a look to know (pages, seconds), sampled when there are many: the files looked
/// at give their own, and the rest get their size times what the sampled ones had per byte.
fn measured(files: &[&Candidate], look: impl Fn(&Path) -> Option<f64>) -> Vec<f64> {
    let step = files.len().div_ceil(SAMPLE).max(1);
    let mut known: Vec<Option<f64>> = vec![None; files.len()];
    let (mut amount, mut bytes) = (0.0, 0.0);
    for index in (0..files.len()).step_by(step) {
        if let Some(value) = look(Path::new(&files[index].path)) {
            known[index] = Some(value);
            amount += value;
            bytes += files[index].size.max(1) as f64;
        }
    }
    let per_byte = if bytes > 0.0 { amount / bytes } else { 0.0 };
    files.iter().zip(known).map(|(file, value)| value.unwrap_or(file.size.max(1) as f64 * per_byte)).collect()
}

fn pages(path: &Path) -> Option<f64> {
    let document = pdf::pdfium()?.load_pdf_from_file(path, None).ok()?;
    Some(document.pages().len() as f64)
}

fn duration(path: &Path) -> Option<f64> {
    media::probe(path).ok().map(|probe| probe.duration).filter(|seconds| *seconds > 0.0)
}

pub fn estimate(candidates: &[Candidate], options: &IndexOptions, costs: &Costs) -> Estimate {
    let mut estimate = Estimate::default();
    let of = |kind: FileKind| candidates.iter().filter(|file| file.kind == kind).collect::<Vec<_>>();
    for file in candidates {
        *estimate.files.entry(file.kind.as_str().to_string()).or_default() += 1;
        match file.kind {
            FileKind::Image => estimate.seconds += costs.image,
            FileKind::Screenshot => estimate.seconds += costs.screenshot,
            FileKind::Doc => {
                let all = (file.size / 2000).clamp(1, options.max_chunks.max(1) as i64) as usize;
                let first = all.min(options.doc_step);
                estimate.doc_chunks += first;
                estimate.seconds += first as f64 * costs.doc_chunk;
                if all > first {
                    estimate.long_files += 1;
                    estimate.later_seconds += (all - first) as f64 * costs.doc_chunk;
                }
            }
            FileKind::Code => {
                // A notebook's file is mostly its outputs, which aren't read: a guess at its cells' share.
                let characters = if file.path.to_lowercase().ends_with(".ipynb") { file.size / 8 } else { file.size };
                let stretches = (characters.max(1) + CHARACTERS_PER_STRETCH - 1) / CHARACTERS_PER_STRETCH;
                estimate.code_stretches += stretches as usize;
                estimate.seconds += stretches as f64 * costs.code_stretch;
                estimate.bytes +=
                    characters as f64 * CODE_BYTES_PER_CHARACTER + stretches as f64 * CODE_BYTES_PER_STRETCH;
            }
            FileKind::Pdf | FileKind::Audio | FileKind::Video => {}
        }
    }
    let pdfs = of(FileKind::Pdf);
    for total in measured(&pdfs, pages) {
        let total = (total.round() as usize).max(1);
        let all = total.min(options.max_pages);
        let first = if total > options.big_pdf_pages { options.pdf_step.min(all) } else { all };
        estimate.pdf_pages_total += total;
        estimate.pdf_pages += first;
        estimate.seconds += first as f64 * costs.pdf_page;
        if all > first {
            estimate.long_files += 1;
            estimate.later_seconds += (all - first) as f64 * costs.pdf_page;
        }
    }
    let audio: f64 = measured(&of(FileKind::Audio), duration).iter().sum();
    estimate.audio_seconds = audio;
    estimate.seconds += audio * costs.audio_second;
    let video: f64 = measured(&of(FileKind::Video), duration).iter().sum();
    estimate.video_seconds = video;
    estimate.seconds += video * costs.video_second;
    estimate
}

/// "≈ 8 s", "≈ 2 min", "≈ 1.5 h", or "< 1 s" for a few short files.
pub fn approximately(seconds: f64) -> String {
    if seconds < 0.5 { "< 1 s".into() } else { format!("≈ {}", rough_duration(seconds)) }
}

/// "8 s", "2 min", "1.5 h": the precision an estimate deserves.
pub fn rough_duration(seconds: f64) -> String {
    if seconds < 90.0 {
        format!("{} s", seconds.round() as i64)
    } else if seconds < 5400.0 {
        format!("{} min", (seconds / 60.0).round() as i64)
    } else {
        format!("{:.1} h", seconds / 3600.0)
    }
}

/// "40 MB", "1.2 GB".
pub fn rough_bytes(bytes: f64) -> String {
    if bytes < 1e6 {
        format!("{} kB", (bytes / 1e3).round().max(1.0) as i64)
    } else if bytes < 1e9 {
        format!("{} MB", (bytes / 1e6).round() as i64)
    } else {
        format!("{:.1} GB", bytes / 1e9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, kind: FileKind, size: i64) -> Candidate {
        Candidate { path: path.into(), kind, size, mtime: 0.0, inode: 0, device: 0 }
    }

    #[test]
    fn first_pass_and_the_rest_of_long_files() {
        let options = IndexOptions::default();
        let costs = Costs::default();
        let long = (options.doc_step as i64 + 10) * 2000;
        let files = [
            file("/a.jpg", FileKind::Image, 1),
            file("/b.png", FileKind::Screenshot, 1),
            file("/c.md", FileKind::Doc, 100),
            file("/d.txt", FileKind::Doc, long),
        ];
        let estimate = estimate(&files, &options, &costs);
        assert_eq!(estimate.files["doc"], 2);
        assert_eq!(estimate.doc_chunks, 1 + options.doc_step);
        assert_eq!(estimate.long_files, 1);
        assert!((estimate.later_seconds - 10.0 * costs.doc_chunk).abs() < 1e-9);
        let seconds = costs.image + costs.screenshot + (1 + options.doc_step) as f64 * costs.doc_chunk;
        assert!((estimate.seconds - seconds).abs() < 1e-9);
    }

    #[test]
    fn unseen_files_are_estimated_from_their_size() {
        let files: Vec<Candidate> = (0..400).map(|n| file(&format!("/{n}.wav"), FileKind::Audio, 1000)).collect();
        let refs: Vec<&Candidate> = files.iter().collect();
        let looked = std::sync::atomic::AtomicUsize::new(0);
        let seconds = measured(&refs, |_| {
            looked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(10.0)
        });
        assert!(looked.into_inner() <= SAMPLE);
        assert!(seconds.iter().all(|seconds| (*seconds - 10.0).abs() < 1e-9));
    }

    #[test]
    fn durations_read_as_people_say_them() {
        assert_eq!(approximately(0.2), "< 1 s");
        assert_eq!(approximately(42.0), "≈ 42 s");
        assert_eq!(rough_duration(600.0), "10 min");
        assert_eq!(rough_duration(7200.0), "2.0 h");
        assert_eq!(rough_bytes(40e6), "40 MB");
    }
}
