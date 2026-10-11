//! Keeps the index in step with the chosen folders, then extracts and embeds whatever is pending.
//!
//! Model work goes to an `Embedder` (made on first use, freed by `close_worker`). Extraction runs on a second thread
//! one job ahead, so reading files and embedding them overlap (see `process`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

use crate::code::{self, CodeFile};
use crate::crawler::{self, CrawlReport};
use crate::embedder::{Embedder, WorkerError};
use crate::extract::{self, ExtractError};
use crate::model::{FileKind, IndexOptions, Modality, SegmentKind, prompts};
use crate::search_text::{excerpt, has_words};
use crate::store::{FileRecord, IndexStore, Info};

#[derive(Debug)]
pub enum IndexError {
    /// The index was built with another model or other settings.
    FingerprintMismatch {
        index: String,
        now: String,
    },
    Worker(WorkerError),
    Database(rusqlite::Error),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::FingerprintMismatch { index, now } => write!(
                f,
                "this index was built with different settings or model:\n  index: {index}\n  now:   {now}\n\
                 re-run with --reindex (vectors from different settings can't be compared)"
            ),
            IndexError::Worker(error) => error.fmt(f),
            IndexError::Database(error) => write!(f, "index: {error}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<rusqlite::Error> for IndexError {
    fn from(error: rusqlite::Error) -> Self {
        IndexError::Database(error)
    }
}

impl From<WorkerError> for IndexError {
    fn from(error: WorkerError) -> Self {
        IndexError::Worker(error)
    }
}

pub type Result<T> = std::result::Result<T, IndexError>;

#[derive(Debug, Default)]
pub struct SyncSummary {
    pub added: usize,
    pub changed: usize,
    pub moved: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub crawl: CrawlReport,
    /// Files this sync queued for embedding (new or changed), so a watcher can index exactly those first.
    pub queued: Vec<i64>,
}

/// One file of the first pass, done.
#[derive(Debug, Clone)]
pub struct Event {
    pub done: usize,
    pub total: usize,
    pub path: String,
    pub kind: FileKind,
    pub state: String,
    pub seconds: f64,
    pub note: Option<String>,
}

/// One step of the later pass: how far it has got (`done` of the `total` pages and chunks it set out to read, in
/// `files` files), and the file it just read more of.
#[derive(Debug, Clone)]
pub struct Step {
    pub done: usize,
    pub total: usize,
    /// Long files not finished yet, this one included until its last step.
    pub files: usize,
    pub path: String,
    pub kind: FileKind,
    /// "partial" until the file's last step, then "done".
    pub state: String,
    pub seconds: f64,
    pub note: Option<String>,
}

pub type MakeEmbedder = Box<dyn FnMut() -> std::result::Result<Box<dyn Embedder>, WorkerError>>;
/// Hears that the index started over: (what it was built with, what builds it now).
pub type OnReset = Box<dyn FnMut(&str, &str)>;
type Committed<'c> = &'c mut dyn FnMut(usize, &Job, &[Outcome], f64);
type Outcome = (String, Option<String>);

pub struct Indexer<'a> {
    store: &'a IndexStore,
    options: IndexOptions,
    make_embedder: MakeEmbedder,
    worker: Option<Box<dyn Embedder>>,
    /// The kinds this build reads. The others stay pending until it learns them.
    pub kinds: Vec<FileKind>,
    /// When set, an index built with another model or other settings is emptied and embedded again, and this hears
    /// about it. When None, indexing stops with `FingerprintMismatch`.
    pub reset_on_model_change: Option<OnReset>,
    /// The vector source of the embedders `make_embedder` makes, when it's known without starting one (llama.cpp's
    /// is): then `check_model` can notice a new model before anything needs embedding.
    pub embedder_source: Option<String>,
}

/// Files read and embedded together, or one step of a long file (`step`: the chunks of a document to read, from 0).
#[derive(Clone)]
struct Job {
    files: Vec<FileRecord>,
    step: Option<Range<usize>>,
}

/// A job read from disk and decoded, ready for the model.
struct Prepared {
    batch: Vec<FileRecord>,
    step: Option<Range<usize>>,
    /// Files already settled (skipped or failed while reading), by their place in the batch.
    outcomes: HashMap<usize, (String, String)>,
    payload: Payload,
}

enum Payload {
    Nothing,
    /// `chunks`: the ones in `read` (from 0) of the document's `total` (counted up to `max_chunks` + 1).
    Doc {
        chunks: Vec<String>,
        read: Range<usize>,
        total: usize,
        characters: usize,
    },
    Code(CodeFile),
}

impl<'a> Indexer<'a> {
    /// The kinds this build can read so far.
    pub const READABLE: [FileKind; 2] = [FileKind::Doc, FileKind::Code];

    pub fn new(store: &'a IndexStore, options: IndexOptions, make_embedder: MakeEmbedder) -> Indexer<'a> {
        Indexer {
            store,
            options,
            make_embedder,
            worker: None,
            kinds: Self::READABLE.to_vec(),
            reset_on_model_change: None,
            embedder_source: None,
        }
    }

    /// Frees the embedding model until the next file needs it.
    pub fn close_worker(&mut self) {
        self.worker = None;
    }

    pub fn worker_is_running(&self) -> bool {
        self.worker.is_some()
    }

    // Sync

    /// Crawls `roots` and reconciles the index: new files are queued, changed ones re-queued, moved ones re-pathed (no
    /// re-embedding), and files that are gone or newly excluded are removed.
    pub fn sync(&self, roots: &[PathBuf]) -> Result<SyncSummary> {
        let crawl = crawler::crawl(roots, &self.options);
        let root_paths: Vec<String> = roots.iter().map(|root| crawler::standardized(root)).collect();
        let existing = self.store.files_under(&root_paths)?;
        let by_path: HashMap<&str, &FileRecord> = existing.iter().map(|file| (file.path.as_str(), file)).collect();
        let by_inode: HashMap<(u64, u64), &FileRecord> =
            existing.iter().map(|file| ((file.device, file.inode), file)).collect();
        let seen: HashSet<&str> = crawl.candidates.iter().map(|file| file.path.as_str()).collect();
        let mut moved_from: HashSet<String> = HashSet::new();
        let mut summary = SyncSummary::default();
        self.store.transaction(|_| {
            for file in &crawl.candidates {
                if let Some(row) = by_path.get(file.path.as_str()) {
                    if row.size != file.size || (row.mtime - file.mtime).abs() > 0.001 || row.kind != file.kind {
                        self.store.requeue_as(row.id, file)?;
                        summary.changed += 1;
                        summary.queued.push(row.id);
                    } else {
                        summary.unchanged += 1;
                    }
                } else if let Some(row) = by_inode.get(&(file.device, file.inode))
                    && !seen.contains(row.path.as_str())
                    && !moved_from.contains(&row.path)
                    && row.size == file.size
                    && (row.mtime - file.mtime).abs() < 0.001
                {
                    self.store.move_file(row.id, &file.path)?;
                    moved_from.insert(row.path.clone());
                    summary.moved += 1;
                } else {
                    summary.queued.push(self.store.insert(file)?);
                    summary.added += 1;
                }
            }
            // A root that went away while it was being walked (a drive unplugged mid-crawl) looked emptier than it
            // is: keep its rows rather than take that for "deleted".
            let vanished: Vec<String> = root_paths
                .iter()
                .filter(|root| !Path::new(root).exists())
                .map(|root| format!("{}/", root.trim_end_matches('/')))
                .collect();
            for row in &existing {
                if !seen.contains(row.path.as_str())
                    && !moved_from.contains(&row.path)
                    && !vanished.iter().any(|root| row.path.starts_with(root))
                {
                    self.store.delete(row.id)?;
                    summary.removed += 1;
                }
            }
            Ok(())
        })?;
        summary.crawl = crawl;
        Ok(summary)
    }

    // Run

    /// Processes pending files in indexing order (`only`: just these files). Long documents get their first step and
    /// stay `partial` for `finish_long_files`. Per-file problems are recorded on the file; a dead model or a settings
    /// mismatch stops the run (pending files stay pending, so the next run resumes). `should_stop` is asked between
    /// jobs.
    pub fn run(
        &mut self,
        limit: Option<usize>,
        only: Option<&[i64]>,
        should_stop: &mut dyn FnMut() -> bool,
        on_event: &mut dyn FnMut(&Event),
    ) -> Result<BTreeMap<String, usize>> {
        let queue = self.store.pending(limit, only, Some(&self.kinds))?;
        let jobs: Vec<Job> = queue
            .iter()
            .map(|file| Job {
                files: vec![file.clone()],
                step: None,
            })
            .collect();
        let mut states = BTreeMap::new();
        let mut done = 0;
        let total = queue.len();
        self.process(
            jobs,
            should_stop,
            &mut |_, job, results, seconds| {
                for (offset, file) in job.files.iter().enumerate() {
                    let (state, note) = &results[offset];
                    *states.entry(state.clone()).or_default() += 1;
                    on_event(&Event {
                        done: done + offset + 1,
                        total,
                        path: file.path.clone(),
                        kind: file.kind,
                        state: state.clone(),
                        seconds,
                        note: note.clone(),
                    });
                }
                done += job.files.len();
            },
            &|_| false,
        )?;
        Ok(states)
    }

    /// The later pass: reads the rest of long documents (`partial` after their first step), in indexing order, a
    /// step at a time. Each step is saved as one transaction, so stopping between steps loses nothing, and the next
    /// pass goes on from there.
    pub fn finish_long_files(
        &mut self,
        should_stop: &mut dyn FnMut() -> bool,
        on_step: &mut dyn FnMut(&Step),
    ) -> Result<BTreeMap<String, usize>> {
        let mut jobs = Vec::new();
        for file in self.store.unfinished(Some(&self.kinds))? {
            let info = self.store.info(file.id)?;
            let size = if file.kind == FileKind::Pdf {
                self.options.pdf_step
            } else {
                self.options.doc_step
            };
            // Where it stopped (after the last page or chunk stored, should its info not say; nothing stored: the start).
            let mut start = match info.get("read_to").and_then(|value| value.parse().ok()) {
                Some(start) => start,
                None => self.store.last_place(file.id)?.unwrap_or(0),
            };
            let end = info
                .get("read_end")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
                .max(start + 1);
            while start < end {
                jobs.push(Job {
                    files: vec![file.clone()],
                    step: Some(start..(start + size).min(end)),
                });
                start += size;
            }
        }
        // Progress counts pages and chunks: what's read once each job is through.
        let mut through = Vec::new();
        for job in &jobs {
            through.push(through.last().copied().unwrap_or(0) + job.step.as_ref().map_or(0, |step| step.len()));
        }
        let mut files = jobs.iter().map(|job| job.files[0].id).collect::<HashSet<_>>().len();
        let mut states = BTreeMap::new();
        let finished = std::cell::RefCell::new(HashSet::new());
        let total = through.last().copied().unwrap_or(0);
        self.process(
            jobs,
            should_stop,
            &mut |index, job, results, seconds| {
                let file = &job.files[0];
                let (state, note) = &results[0];
                if state != "partial" && finished.borrow_mut().insert(file.id) {
                    *states.entry(state.clone()).or_default() += 1;
                    files -= 1;
                }
                on_step(&Step {
                    done: through[index],
                    total,
                    files,
                    path: file.path.clone(),
                    kind: file.kind,
                    state: state.clone(),
                    seconds,
                    note: note.clone(),
                });
            },
            &|job| finished.borrow().contains(&job.files[0].id),
        )?;
        Ok(states)
    }

    /// Runs `jobs` in order until `should_stop` (asked between jobs), passing each one's results to `committed`. Two
    /// stages overlap: while the model embeds one job, the next one is read on another thread. Jobs that `skip` turns
    /// down when their turn comes are left out.
    fn process(
        &mut self,
        jobs: Vec<Job>,
        should_stop: &mut dyn FnMut() -> bool,
        committed: Committed,
        skip: &dyn Fn(&Job) -> bool,
    ) -> Result<()> {
        let mut ahead: Option<thread::JoinHandle<Prepared>> = None;
        for (index, job) in jobs.iter().enumerate() {
            if should_stop() {
                break;
            }
            let started = Instant::now();
            let prepared = match ahead.take() {
                Some(handle) => handle.join().expect("the preparing thread panicked"),
                None => prepare(job.clone(), &self.options),
            };
            if let Some(next) = jobs.get(index + 1).cloned() {
                let options = self.options.clone();
                ahead = Some(thread::spawn(move || prepare(next, &options)));
            }
            if skip(job) {
                continue;
            }
            let results = self.commit(prepared)?;
            committed(
                index,
                job,
                &results,
                started.elapsed().as_secs_f64() / job.files.len() as f64,
            );
        }
        if let Some(handle) = ahead {
            let _ = handle.join();
        }
        Ok(())
    }

    /// Compares the index with the current model and settings (`embedder_source`), without starting the model; a
    /// change empties the index (see `reset_on_model_change`) or fails with `FingerprintMismatch`.
    pub fn check_model(&mut self) -> Result<()> {
        if let Some(source) = self.embedder_source.clone() {
            self.adopt(&format!("{source} | {}", self.options.fingerprint()))?;
        }
        Ok(())
    }

    fn adopt(&mut self, fingerprint: &str) -> Result<()> {
        let Some(indexed) = self.store.meta("fingerprint")? else {
            return Ok(());
        };
        if indexed == fingerprint {
            return Ok(());
        }
        let Some(reset) = self.reset_on_model_change.as_mut() else {
            return Err(IndexError::FingerprintMismatch {
                index: indexed,
                now: fingerprint.to_string(),
            });
        };
        // Everything is queued again: a run that's under way goes on with its batch, and later runs take the rest.
        self.store.reset_all()?;
        reset(&indexed, fingerprint);
        Ok(())
    }

    fn full_worker(&mut self) -> Result<&mut Box<dyn Embedder>> {
        if self.worker.is_none() {
            let started = (self.make_embedder)()?;
            let fingerprint = format!("{} | {}", started.info().vector_source(), self.options.fingerprint());
            self.adopt(&fingerprint)?;
            self.store.set_meta("fingerprint", Some(&fingerprint))?;
            self.store
                .set_meta("vector_source", Some(&started.info().vector_source()))?;
            self.worker = Some(started);
        }
        Ok(self.worker.as_mut().unwrap())
    }

    // Commit (this thread: the model and the database)

    fn commit(&mut self, prepared: Prepared) -> Result<Vec<Outcome>> {
        let Prepared {
            batch,
            step,
            outcomes,
            payload,
        } = prepared;
        if let (Some(step), Some((_, detail))) = (&step, outcomes.get(&0)) {
            return Ok(vec![self.stop_reading(&batch[0], step.start, detail)?]);
        }
        let mut results: Vec<Outcome> = vec![("failed".into(), None); batch.len()];
        for (slot, (state, detail)) in &outcomes {
            self.store.finish(batch[*slot].id, state, Some(detail), &Info::new())?;
            results[*slot] = (state.clone(), Some(detail.clone()));
        }
        let later = step.is_some();
        let outcome = match payload {
            Payload::Nothing => return Ok(results),
            Payload::Doc {
                chunks,
                read,
                total,
                characters,
            } => self.doc(&batch[0], &chunks, read, total, characters, later),
            Payload::Code(code) => self.code(&batch[0], &code),
        };
        match outcome {
            Ok(outcome) => results[0] = outcome,
            Err(IndexError::Database(error)) => {
                // A file the database turned down (its text, its size): that file fails, the run goes on.
                let detail = error.to_string();
                if let Some(step) = step {
                    return Ok(vec![self.stop_reading(&batch[0], step.start, &detail)?]);
                }
                self.store.finish(batch[0].id, "failed", Some(&detail), &Info::new())?;
                results[0] = ("failed".into(), Some(detail));
            }
            Err(error) => return Err(error),
        }
        Ok(results)
    }

    /// A long file that can't be read any further: it keeps the pages or chunks read so far, which stay searchable,
    /// and counts as done rather than failed or waiting to be tried again and again.
    fn stop_reading(&self, file: &FileRecord, place: usize, problem: &str) -> Result<Outcome> {
        let mut info = self.store.info(file.id)?;
        info.remove("read_to");
        info.remove("read_end");
        let unit = if file.kind == FileKind::Pdf { "page" } else { "chunk" };
        self.store.finish(
            file.id,
            "done",
            Some(&format!("read to {unit} {place}, not after: {problem}")),
            &info,
        )?;
        Ok((
            "done".into(),
            Some(format!("stopped before {unit} {}: {problem}", place + 1)),
        ))
    }

    /// Chunks `read` of a document (all of a short one, a step of a long one). A long one stays `partial` with
    /// `read_to` and `read_end` (chunks, from 0) in its info until its last step.
    fn doc(
        &mut self,
        file: &FileRecord,
        chunks: &[String],
        read: Range<usize>,
        total: usize,
        characters: usize,
        later: bool,
    ) -> Result<Outcome> {
        if total == 0 {
            self.store.finish(file.id, "skipped", Some("no text"), &Info::new())?;
            return Ok(("skipped".into(), Some("no text".into())));
        }
        let title = Path::new(&file.path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string();
        // Text that's mostly digits (a data dump saved as .txt) gets keyword search only.
        let worded: Vec<usize> = (0..chunks.len()).filter(|index| has_words(&chunks[*index])).collect();
        let vectors = if worded.is_empty() {
            Vec::new()
        } else {
            let texts: Vec<String> = worded
                .iter()
                .map(|index| prompts::document(Some(&title), &chunks[*index]))
                .collect();
            self.full_worker()?.embed_texts(&texts)?
        };
        let vector: HashMap<usize, &Vec<f32>> = worded.iter().copied().zip(&vectors).collect();
        let end = total.min(self.options.max_chunks);
        let unread = read.end < end;
        self.store.transaction(|_| {
            for (offset, chunk) in chunks.iter().enumerate() {
                self.store.add_segment(
                    file.id,
                    SegmentKind::Chunk,
                    Modality::Text,
                    Some((read.start + offset + 1) as f64),
                    None,
                    Some(&excerpt(chunk, 240)),
                    Some(chunk),
                    vector.get(&offset).map(|vector| vector.as_slice()),
                )?;
            }
            let mut info = Info::from([
                ("chunks".into(), end.to_string()),
                ("chars".into(), characters.to_string()),
            ]);
            if unread {
                info.insert("read_to".into(), read.end.to_string());
                info.insert("read_end".into(), end.to_string());
            }
            let detail = if unread {
                Some(format!("read to chunk {} of {end}", read.end))
            } else if end < total {
                Some(format!("the first {end} chunks"))
            } else {
                None
            };
            self.store.finish(
                file.id,
                if unread { "partial" } else { "done" },
                detail.as_deref(),
                &info,
            )
        })?;
        let span = if later {
            format!("chunks {}–{} of {end}", read.start + 1, read.end)
        } else if unread {
            format!("{} of {end} chunks", chunks.len())
        } else {
            format!("{} chunks", chunks.len())
        };
        let keywords_only = chunks.len() - worded.len();
        let note = if keywords_only > 0 {
            format!("{span}, {keywords_only} for keywords only")
        } else {
            span
        };
        Ok((if unread { "partial" } else { "done" }.into(), Some(note)))
    }

    /// A code file's stretches of lines (a notebook's of cells), each embedded with the file's path in its repo as its
    /// title, and kept with its lines for keyword search and for opening at the place.
    fn code(&mut self, file: &FileRecord, code: &CodeFile) -> Result<Outcome> {
        if code.pieces.is_empty() {
            self.store.finish(file.id, "skipped", Some("no code"), &Info::new())?;
            return Ok(("skipped".into(), Some("no code".into())));
        }
        // Lines that are mostly numbers and symbols (a table of constants) get keyword search only.
        let worded: Vec<usize> = (0..code.pieces.len())
            .filter(|index| has_words(&code.pieces[*index].text))
            .collect();
        let vectors = if worded.is_empty() {
            Vec::new()
        } else {
            let texts: Vec<String> = worded
                .iter()
                .map(|index| prompts::document(Some(&code.title), &code.pieces[*index].text))
                .collect();
            self.full_worker()?.embed_texts(&texts)?
        };
        let vector: HashMap<usize, &Vec<f32>> = worded.iter().copied().zip(&vectors).collect();
        let kind = if code.cells.is_none() {
            SegmentKind::Lines
        } else {
            SegmentKind::Cells
        };
        self.store.transaction(|_| {
            for (offset, piece) in code.pieces.iter().enumerate() {
                self.store.add_segment(
                    file.id,
                    kind,
                    Modality::Text,
                    Some(piece.first as f64),
                    Some(piece.last as f64),
                    Some(&code::summary(&piece.text)),
                    Some(&piece.text),
                    vector.get(&offset).map(|vector| vector.as_slice()),
                )?;
            }
            let mut info = Info::from([("lines".into(), code.lines.to_string())]);
            if let Some(cells) = code.cells {
                info.insert("cells".into(), cells.to_string());
            }
            if let Some(repo) = &code.repo {
                info.insert("repo".into(), repo.clone());
            }
            self.store.finish(file.id, "done", None, &info)
        })?;
        let size = match code.cells {
            Some(cells) => format!("{cells} cells"),
            None => format!("{} lines", code.lines),
        };
        let keywords_only = code.pieces.len() - worded.len();
        let mut note = format!("{} stretches, {size}", code.pieces.len());
        if keywords_only > 0 {
            note += &format!(", {keywords_only} for keywords only");
        }
        Ok(("done".into(), Some(note)))
    }
}

// Prepare (any thread: files only, no database, no model)

fn prepare(job: Job, options: &IndexOptions) -> Prepared {
    let Job { files: batch, step } = job;
    let mut prepared = Prepared {
        batch,
        step,
        outcomes: HashMap::new(),
        payload: Payload::Nothing,
    };
    let path = PathBuf::from(&prepared.batch[0].path);
    let read = match prepared.batch[0].kind {
        FileKind::Doc => extract::doc_text(&path).map(|text| {
            // The chunks are cut the same way every time, so a later step goes on where the last one stopped.
            let chunks = extract::chunks(&text, 1800, 200, options.max_chunks + 1);
            let wanted = prepared.step.clone().unwrap_or(0..options.doc_step);
            let read = clamp(wanted, chunks.len().min(options.max_chunks));
            Payload::Doc {
                chunks: chunks[read.clone()].to_vec(),
                read,
                total: chunks.len(),
                characters: text.chars().count(),
            }
        }),
        FileKind::Code => code::read(&path, options).map(Payload::Code),
        other => Err(ExtractError::Unreadable(format!("{other} files aren't read yet"))),
    };
    match read {
        Ok(payload) => prepared.payload = payload,
        Err(ExtractError::LeftOut(why)) => {
            prepared.outcomes.insert(0, ("skipped".into(), why));
        }
        Err(error) => {
            for slot in 0..prepared.batch.len() {
                prepared.outcomes.insert(slot, ("failed".into(), error.to_string()));
            }
        }
    }
    prepared
}

fn clamp(range: Range<usize>, end: usize) -> Range<usize> {
    range.start.min(end)..range.end.min(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::{Embedding, WorkerInfo};
    use crate::searcher::{SearchOptions, Searcher};
    use std::cell::Cell;
    use std::fs;
    use std::rc::Rc;

    /// Vectors made from the text's bytes: enough to fill an index without a model.
    struct FakeEmbedder {
        info: WorkerInfo,
        texts: Rc<Cell<usize>>,
    }

    impl Embedder for FakeEmbedder {
        fn info(&self) -> &WorkerInfo {
            &self.info
        }

        fn embed_texts(&mut self, texts: &[String]) -> std::result::Result<Vec<Vec<f32>>, WorkerError> {
            self.texts.set(self.texts.get() + texts.len());
            Ok(texts
                .iter()
                .map(|text| {
                    let mut vector = vec![0.01f32; 768];
                    for (index, byte) in text.bytes().enumerate() {
                        vector[(index * 31 + byte as usize) % 768] += 1.0;
                    }
                    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
                    vector.iter().map(|value| value / norm).collect()
                })
                .collect())
        }

        fn embed_images(&mut self, _: &[String], _: u32) -> std::result::Result<Vec<Embedding>, WorkerError> {
            Err(WorkerError::Failed("no images here".into()))
        }

        fn embed_audio(&mut self, _: &[String]) -> std::result::Result<Vec<Embedding>, WorkerError> {
            Err(WorkerError::Failed("no audio here".into()))
        }
    }

    fn fake(texts: Rc<Cell<usize>>) -> MakeEmbedder {
        Box::new(move || {
            let info = WorkerInfo {
                model: "fake".into(),
                revision: "0000000".into(),
                runtime: "test".into(),
                dtype: "f32".into(),
                dim: 768,
                text_only: false,
            };
            Ok(Box::new(FakeEmbedder {
                info,
                texts: texts.clone(),
            }) as Box<dyn Embedder>)
        })
    }

    fn write(root: &Path, path: &str, text: &str) {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }

    fn function(name: &str, body: usize) -> String {
        let lines: Vec<String> = (1..=body)
            .map(|n| format!("    let v{n} = \"{}\"", "x".repeat(40)))
            .collect();
        format!("func {name}() {{\n{}\n}}\n", lines.join("\n"))
    }

    #[test]
    fn code_index_is_a_file_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "billing/.git/HEAD", "ref: refs/heads/main\n");
        write(
            root,
            "billing/retry.swift",
            &[
                function("retryWithBackoff", 22),
                function("circuit", 22),
                function("other", 22),
            ]
            .join("\n"),
        );
        write(
            root,
            "billing/gen_pb2.py",
            "# Generated by the protocol buffer compiler.  DO NOT EDIT!\nx = 1\n",
        );
        let index = root.join("index");
        let store = IndexStore::open(&index, IndexStore::CODE_FILE).unwrap();
        let texts = Rc::new(Cell::new(0));
        let options = IndexOptions {
            code: true,
            ..Default::default()
        };
        let mut indexer = Indexer::new(&store, options, fake(texts.clone()));
        indexer.sync(&[root.join("billing")]).unwrap();
        let states = indexer.run(None, None, &mut || false, &mut |_| {}).unwrap();
        assert_eq!(states, BTreeMap::from([("done".into(), 1), ("skipped".into(), 1)]));
        let locs: Vec<i64> = store
            .db
            .prepare("SELECT loc FROM segments WHERE kind = 'lines' ORDER BY loc")
            .unwrap()
            .query_map([], |row| row.get::<_, f64>(0).map(|loc| loc as i64))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(locs, [1, 26, 51]);
        assert_eq!(texts.get(), 3);
        let file = store.files_under(&[crawler::standardized(root)]).unwrap();
        let retry = file.iter().find(|file| file.path.ends_with("retry.swift")).unwrap();
        assert_eq!(
            store.info(retry.id).unwrap().get("lines").map(String::as_str),
            Some("74")
        );
        let mut searcher = Searcher::new(&store, true).unwrap();
        let hits = searcher
            .search("retryWithBackoff", None, &SearchOptions::default())
            .unwrap();
        assert_eq!(
            (hits[0].segment, hits[0].loc, hits[0].loc_end),
            (SegmentKind::Lines, Some(1.0), Some(24.0))
        );
        assert!(
            IndexStore::open_main(&index)
                .unwrap()
                .files_under(&[crawler::standardized(root)])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_name_looked_up_points_at_its_definition() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let calls: Vec<String> = (1..=30)
            .map(|n| format!("    refreshAccessToken()  // call {n} of many"))
            .collect();
        let text = format!(
            "func run() {{\n{}\n}}\n\nfunc refreshAccessToken() -> String {{\n    return \"token\"\n}}\n",
            calls.join("\n")
        );
        write(root, "app/.git/HEAD", "ref: refs/heads/main\n");
        write(root, "app/Auth.swift", &text);
        let store = IndexStore::open(&root.join("index"), IndexStore::CODE_FILE).unwrap();
        let mut indexer = Indexer::new(
            &store,
            IndexOptions {
                code: true,
                ..Default::default()
            },
            fake(Rc::new(Cell::new(0))),
        );
        indexer.sync(&[root.join("app")]).unwrap();
        indexer.run(None, None, &mut || false, &mut |_| {}).unwrap();
        let mut searcher = Searcher::new(&store, true).unwrap();
        let hit = searcher
            .search("refreshAccessToken", None, &SearchOptions::default())
            .unwrap()
            .remove(0);
        assert_eq!(hit.segment, SegmentKind::Lines);
        assert_eq!(hit.focus, Some(34.0)); // the line that defines it, in the stretch that holds it
        assert!(hit.loc.unwrap() <= 34.0 && hit.loc_end.unwrap() >= 34.0);
    }

    #[test]
    fn long_documents_finish_later_where_they_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // ~60 chunks of 1,800 characters: 40 in the first pass, the rest later.
        let words: Vec<String> = (0..20_000).map(|n| format!("word{n}")).collect();
        write(root, "docs/book.txt", &words.join(" "));
        write(root, "docs/note.md", "Flight to Tokyo on November 3.");
        let store = IndexStore::open_main(&root.join("index")).unwrap();
        let mut indexer = Indexer::new(&store, IndexOptions::default(), fake(Rc::new(Cell::new(0))));
        let sync = indexer.sync(&[root.join("docs")]).unwrap();
        assert_eq!(sync.added, 2);
        let states = indexer.run(None, None, &mut || false, &mut |_| {}).unwrap();
        assert_eq!(states, BTreeMap::from([("done".into(), 1), ("partial".into(), 1)]));
        let (files, left) = store.unfinished_count().unwrap();
        assert_eq!(files, 1);
        assert!(left > 10, "{left}");
        let mut steps = Vec::new();
        let finished = indexer
            .finish_long_files(&mut || false, &mut |step| steps.push(step.clone()))
            .unwrap();
        assert_eq!(finished, BTreeMap::from([("done".into(), 1)]));
        assert_eq!(steps.last().unwrap().files, 0);
        assert_eq!(store.unfinished_count().unwrap().0, 0);
        // The chunks go on where the first step stopped: 41, 42, … with none missing or doubled.
        let locs: Vec<i64> = store
            .db
            .prepare("SELECT loc FROM segments WHERE kind = 'chunk' ORDER BY loc")
            .unwrap()
            .query_map([], |row| row.get::<_, f64>(0).map(|loc| loc as i64))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let book: Vec<i64> = locs.iter().copied().filter(|loc| *loc > 1).collect();
        assert_eq!(book, (2..=*book.last().unwrap()).collect::<Vec<_>>());
        // A change on disk queues the file again; a move only renames it.
        fs::rename(root.join("docs/note.md"), root.join("docs/tokyo.md")).unwrap();
        let sync = indexer.sync(&[root.join("docs")]).unwrap();
        assert_eq!((sync.moved, sync.added, sync.removed), (1, 0, 0));
    }

    #[test]
    fn another_model_needs_reindexing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "docs/note.md", "Flight to Tokyo on November 3.");
        let store = IndexStore::open_main(&root.join("index")).unwrap();
        let mut indexer = Indexer::new(&store, IndexOptions::default(), fake(Rc::new(Cell::new(0))));
        indexer.sync(&[root.join("docs")]).unwrap();
        indexer.run(None, None, &mut || false, &mut |_| {}).unwrap();
        let mut other = Indexer::new(&store, IndexOptions::default(), fake(Rc::new(Cell::new(0))));
        other.embedder_source = Some("another model".into());
        assert!(matches!(
            other.check_model(),
            Err(IndexError::FingerprintMismatch { .. })
        ));
    }
}
