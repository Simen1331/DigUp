//! `showme daemon`: keeps the indexes in step with the folders in `config.toml` and answers searches over a Unix
//! socket (`protocol`). Runs as a systemd user service.
//!
//! Threads:
//! - indexing: the loop DigUp's index helper runs. Sync, index what the sync just queued (a few new files go first,
//!   on any power source), then the rest (the backfill) when it may run, then the code index the same way, then the
//!   rest of long PDFs and documents. Between batches it yields to a new sync, a pause, a reload and a stop. With
//!   nothing left it keeps the model warm for `linger_seconds` (screenshots come in bursts), then lets it go.
//! - watching: inotify on the folders. A change syncs once things settle (0.75 s); the code index catches up once the
//!   code folders have been quiet for `code_quiet_seconds` (or every 10 minutes while they aren't).
//! - power: every 30 s, whether the backfill may run (on AC power, or on battery when allowed; not in power-saver
//!   mode), and whether the query encoder has been idle long enough to let go.
//! - one per client: searches (the query encoder loads on the first one), status, and commands.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use showme_core::embedder::{Embedder, Embedding, WorkerError, WorkerInfo};
use showme_core::indexer::{Indexer, MakeEmbedder};
use showme_core::model::{code_query, prompts};
use showme_core::searcher::{SearchOptions, Searcher, strong_count};
use showme_core::store::IndexStore;
use showme_embed::{LlamaEmbedder, LlamaModels, llama, models};
use std::collections::VecDeque;

use showme_core::proto::{self as protocol, FolderProgress, Hit, Request, Response, Status};
use showme_core::settings::Config;

/// What every thread shares.
struct Shared {
    status: Mutex<Status>,
    /// Bumped on every status change; `watch` clients wait on it.
    status_version: Mutex<u64>,
    status_changed: Condvar,
    control: Mutex<Control>,
    wake: Condvar,
    config: Mutex<Config>,
    /// Bumped whenever an index gains or loses vectors: searchers reload before their next search.
    generation: AtomicU64,
    /// The search thread: it owns the searchers and the query encoder.
    search: mpsc::Sender<SearchJob>,
    /// Files indexed since things were last quiet: a big pass that ends is worth a notification.
    indexed: AtomicU64,
}

#[derive(Default)]
struct Control {
    sync: bool,
    code_sync: bool,
    stop: bool,
    reload: bool,
    paused: bool,
    /// The pause came at the end of a step (`step_minutes`), not from the user.
    step_done: bool,
    /// When the current step of the first pass began.
    step_started: Option<Instant>,
    /// Why the backfill has to wait (power), or None.
    blocker: Option<String>,
}

/// Where a pause is kept, so it lasts through a restart: "step" after a step, "you" when asked for.
fn pause_file() -> PathBuf {
    showme_core::paths::data_folder().join("paused")
}

fn remember_pause(paused: bool, step: bool) {
    let file = pause_file();
    let _ = if paused { std::fs::write(&file, if step { "step" } else { "you" }) } else { std::fs::remove_file(&file) };
}

/// A pause from before the last stop (the computer was turned off while paused).
fn remembered_pause() -> Control {
    match std::fs::read_to_string(pause_file()) {
        Ok(reason) => Control { paused: true, step_done: reason.trim() == "step", ..Default::default() },
        Err(_) => Control::default(),
    }
}

impl Shared {
    fn control(&self) -> MutexGuard<'_, Control> {
        self.control.lock().unwrap()
    }

    /// Changes the status and tells the watchers.
    fn update(&self, change: impl FnOnce(&mut Status)) {
        {
            let mut status = self.status.lock().unwrap();
            let before = status.clone();
            change(&mut status);
            if *status == before {
                return;
            }
        }
        *self.status_version.lock().unwrap() += 1;
        self.status_changed.notify_all();
    }

    fn request(&self, change: impl FnOnce(&mut Control)) {
        change(&mut self.control());
        self.wake.notify_all();
    }
}

pub fn run() -> Result<()> {
    let socket = protocol::socket_path();
    if protocol::Client::connect().is_some() {
        bail!("a ShowMe daemon is already running ({})", socket.display());
    }
    let _ = std::fs::remove_file(&socket); // left by one that died
    std::fs::create_dir_all(socket.parent().unwrap())?;
    let listener = UnixListener::bind(&socket).with_context(|| format!("can't listen on {}", socket.display()))?;
    remove_stale_scratch_folders();

    let config = Config::load()?;
    if !config.ocr_languages.is_empty() {
        // SAFETY: no other thread is running yet.
        unsafe { std::env::set_var("SHOWME_OCR_LANGS", config.ocr_languages.join("+")) };
    }
    let (search_jobs, search_queue) = mpsc::channel();
    let shared = Arc::new(Shared {
        status: Mutex::new(Status {
            state: "starting".into(),
            model: protocol::ModelStatus {
                state: if LlamaModels::complete() { "ready" } else { "missing" }.into(),
                fraction: if LlamaModels::complete() { 1.0 } else { 0.0 },
                text: None,
            },
            encoder: "idle".into(),
            ..Default::default()
        }),
        status_version: Mutex::new(0),
        status_changed: Condvar::new(),
        control: Mutex::new(Control { sync: true, code_sync: true, ..remembered_pause() }),
        wake: Condvar::new(),
        config: Mutex::new(config),
        generation: AtomicU64::new(0),
        search: search_jobs,
        indexed: AtomicU64::new(0),
    });
    eprintln!("showme daemon: listening on {}", socket.display());

    let indexing = {
        let shared = shared.clone();
        thread::Builder::new().name("indexing".into()).spawn(move || indexing_loop(&shared))?
    };
    {
        let shared = shared.clone();
        thread::Builder::new().name("watching".into()).spawn(move || watch_loop(&shared))?;
    }
    {
        let shared = shared.clone();
        thread::Builder::new().name("power".into()).spawn(move || power_loop(&shared))?;
    }
    {
        let shared = shared.clone();
        thread::Builder::new().name("search".into()).spawn(move || search_loop(&shared, search_queue))?;
    }
    {
        let shared = shared.clone();
        thread::Builder::new().name("clients".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = shared.clone();
                let _ = thread::Builder::new().name("client".into()).spawn(move || serve(stream, &shared));
            }
        })?;
    }
    // SIGTERM (systemctl stop) and Ctrl-C end the process directly; "quit" over the socket ends it cleanly.
    let _ = indexing.join();
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

// Indexing

/// One model for both indexes, started when the first file needs it. The indexers get it as an `Embedder` that
/// borrows it, so letting it go is one `close` here.
#[derive(Clone, Default)]
struct SharedModel(std::rc::Rc<std::cell::RefCell<Option<Box<dyn Embedder>>>>);

impl SharedModel {
    fn maker(&self, log: PathBuf) -> MakeEmbedder {
        let model = self.clone();
        Box::new(move || {
            if model.0.borrow().is_none() {
                let found = LlamaModels::locate()?;
                *model.0.borrow_mut() = Some(Box::new(LlamaEmbedder::new(&found, false, Some(&log))?));
            }
            let info = model.0.borrow().as_ref().unwrap().info().clone();
            Ok(Box::new(Borrowed { model: model.clone(), info }) as Box<dyn Embedder>)
        })
    }

    fn is_running(&self) -> bool {
        self.0.borrow().is_some()
    }

    fn close(&self) {
        *self.0.borrow_mut() = None;
    }
}

struct Borrowed {
    model: SharedModel,
    info: WorkerInfo,
}

impl Borrowed {
    fn with<T>(&self, work: impl FnOnce(&mut Box<dyn Embedder>) -> Result<T, WorkerError>) -> Result<T, WorkerError> {
        let mut model = self.model.0.borrow_mut();
        work(model.as_mut().ok_or_else(|| WorkerError::Failed("the model was let go".into()))?)
    }
}

impl Embedder for Borrowed {
    fn info(&self) -> &WorkerInfo {
        &self.info
    }

    fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, WorkerError> {
        self.with(|model| model.embed_texts(texts))
    }

    fn embed_images(&mut self, paths: &[String], budget: u32) -> Result<Vec<Embedding>, WorkerError> {
        self.with(|model| model.embed_images(paths, budget))
    }

    fn embed_audio(&mut self, paths: &[String]) -> Result<Vec<Embedding>, WorkerError> {
        self.with(|model| model.embed_audio(paths))
    }
}

/// One index: its store, its indexer, its folders.
struct Part<'a> {
    code: bool,
    store: &'a IndexStore,
    indexer: Indexer<'a>,
    roots: Vec<PathBuf>,
    /// Folders reported missing last time (said again only when it changes).
    missing: Vec<String>,
}

/// Rebuilds everything when the settings change ("reload"), and stops on "quit".
fn indexing_loop(shared: &Arc<Shared>) {
    loop {
        let config = shared.config.lock().unwrap().clone();
        match index_with(shared, &config) {
            Ok(Stop::Quit) => break,
            Ok(Stop::Reload) => {
                let reloaded = Config::load();
                match reloaded {
                    Ok(config) => *shared.config.lock().unwrap() = config,
                    Err(error) => shared.update(|status| status.last_error = Some(format!("{error:#}"))),
                }
                shared.request(|control| {
                    control.reload = false;
                    control.sync = true;
                    control.code_sync = true;
                });
                let _ = shared.search.send(SearchJob::Forget);
            }
            Err(error) => {
                // A fatal problem (another model's index, a database error): say so, and wait to be told what to do.
                notify("ShowMe stopped indexing", &format!("{error:#}"));
                shared.update(|status| {
                    status.state = "error".into();
                    status.detail = Some(format!("{error:#}"));
                    status.last_error = Some(format!("{error:#}"));
                });
                let mut control = shared.control();
                while !control.stop && !control.reload {
                    control = shared.wake.wait(control).unwrap();
                }
                let quit = control.stop;
                drop(control);
                if quit {
                    break;
                }
            }
        }
    }
}

enum Stop {
    Quit,
    Reload,
}

fn index_with(shared: &Arc<Shared>, config: &Config) -> Result<Stop> {
    let folder = config.index_folder();
    let main_store = IndexStore::open_main(&folder)?;
    let code_store = IndexStore::open(&folder, IndexStore::CODE_FILE)?;
    let model = SharedModel::default();
    let log = folder.join("worker.log");
    let source = llama::model_info(false).vector_source();
    let kinds = crate::readable_kinds().0;
    let make_part = |store, code: bool| -> Result<Part> {
        let (roots, options) = if code { config.code() } else { config.main() };
        let mut indexer = Indexer::new(store, options, model.maker(log.clone()));
        indexer.embedder_source = Some(source.clone());
        indexer.kinds = kinds.clone();
        // A new model or new settings: the index starts over by itself (names stay searchable meanwhile).
        indexer.reset_on_model_change =
            Some(Box::new(|from, to| eprintln!("showme daemon: index starts over ({from} → {to})")));
        indexer.check_model()?;
        // Folders taken out of the settings leave the index.
        let keep: Vec<String> = roots.iter().map(|root| root.to_string_lossy().into_owned()).collect();
        store.prune(&keep)?;
        Ok(Part { code, store, indexer, roots, missing: Vec::new() })
    };
    let mut main = make_part(&main_store, false)?;
    let mut code = make_part(&code_store, true)?;
    let has_code = !code.roots.is_empty();
    shared.update(|status| status.has_code = has_code);
    let mut new_files: Vec<i64> = Vec::new();
    let mut new_code: Vec<i64> = Vec::new();
    counts(shared, &main, &code);

    loop {
        if take(shared, |control| &mut control.sync) {
            sync(shared, &mut main, &mut new_files);
            counts(shared, &main, &code);
        }
        if let Some(stop) = stopping(shared) {
            return Ok(stop);
        }
        if !LlamaModels::complete() {
            // Nothing to embed with yet: names are searchable, the rest waits for `showme download`.
            shared.update(|status| {
                status.state = "waiting".into();
                status.detail = Some("model: showme download".into());
            });
            if !wait_for_work(shared, Duration::from_secs(30), &[&main, &code]) {
                continue;
            }
            continue;
        }
        if !new_files.is_empty() {
            embed(shared, &mut main, Some(&new_files))?;
            new_files = left(shared, &main, &new_files)?;
            counts(shared, &main, &code);
            continue;
        }
        if main.store.pending_count()? > 0 && backfill_allowed(shared) {
            embed(shared, &mut main, None)?;
            counts(shared, &main, &code);
            if shared.control().sync {
                continue;
            }
        }
        // The code index once the main one has nothing waiting (or the backfill can't run anyway).
        if has_code && (main.store.pending_count()? == 0 || !backfill_allowed(shared)) {
            if take(shared, |control| &mut control.code_sync) {
                sync(shared, &mut code, &mut new_code);
                counts(shared, &main, &code);
            }
            if !new_code.is_empty() {
                embed(shared, &mut code, Some(&new_code))?;
                new_code = left(shared, &code, &new_code)?;
                counts(shared, &main, &code);
                continue;
            }
            if code.store.pending_count()? > 0 && backfill_allowed(shared) {
                embed(shared, &mut code, None)?;
                counts(shared, &main, &code);
                let control = shared.control();
                if control.sync || control.code_sync {
                    continue;
                }
            }
        }
        // Once nothing is waiting: the rest of long PDFs and documents, the same way.
        if backfill_allowed(shared)
            && main.store.pending_count()? == 0
            && code.store.pending_count()? == 0
            && main.store.unfinished_count()?.0 > 0
        {
            finish_long_files(shared, &mut main, has_code)?;
            counts(shared, &main, &code);
            let control = shared.control();
            if control.sync || control.code_sync && has_code {
                continue;
            }
        }
        if has_code && shared.control().code_sync {
            continue;
        }
        if let Some(stop) = stopping(shared) {
            return Ok(stop);
        }
        let (paused, step_done) = {
            let mut control = shared.control();
            // Nothing waiting: the next pass starts a new step.
            if !control.paused {
                control.step_started = None;
            }
            (control.paused, control.step_done)
        };
        let blocker = shared.control().blocker.clone();
        let waiting = main.store.pending_count()? + code.store.pending_count()?;
        shared.update(|status| {
            (status.state, status.detail) = match (paused, &blocker) {
                (true, _) if waiting > 0 => ("paused".into(), step_done.then(|| "step".into())),
                (false, Some(why)) if waiting > 0 => ("waiting".into(), Some(why.clone())),
                _ => ("idle".into(), None),
            };
            status.current = None;
        });
        // A big pass is through: say so once (a few new files at a time aren't news).
        let indexed = shared.indexed.swap(0, Ordering::Relaxed);
        if waiting == 0 && indexed >= 50 {
            let files = shared.status.lock().unwrap().searchable;
            notify("Your folders are searchable", &format!("{indexed} files read, {files} searchable in all."));
        }
        // Keep the model warm a little (screenshots come in bursts), then let it go.
        let linger = Duration::from_secs_f64(config.linger_seconds.max(1.0));
        if model.is_running() && !wait_for_work(shared, linger, &[&main, &code]) {
            main.indexer.close_worker();
            code.indexer.close_worker();
            model.close();
        }
        if !model.is_running() {
            wait_for_work(shared, Duration::from_secs(3600), &[&main, &code]);
        }
    }
}

/// Reads and clears a request flag.
fn take(shared: &Shared, flag: impl FnOnce(&mut Control) -> &mut bool) -> bool {
    std::mem::take(flag(&mut shared.control()))
}

fn stopping(shared: &Shared) -> Option<Stop> {
    let control = shared.control();
    if control.stop {
        Some(Stop::Quit)
    } else if control.reload {
        Some(Stop::Reload)
    } else {
        None
    }
}

fn backfill_allowed(shared: &Shared) -> bool {
    let control = shared.control();
    !control.paused && control.blocker.is_none()
}

/// Waits up to `timeout` for something to do. False: time's up.
fn wait_for_work(shared: &Shared, timeout: Duration, parts: &[&Part]) -> bool {
    let deadline = Instant::now() + timeout;
    let mut control = shared.control();
    loop {
        if control.stop
            || control.reload
            || control.sync
            || control.code_sync && parts.iter().any(|part| part.code && !part.roots.is_empty())
        {
            return true;
        }
        if !control.paused && control.blocker.is_none() {
            drop(control);
            let waiting = parts.iter().any(|part| {
                part.store.pending_count().unwrap_or(0) > 0
                    || !part.code && part.store.unfinished_count().unwrap_or((0, 0)).0 > 0
            }) && LlamaModels::complete();
            if waiting {
                return true;
            }
            control = shared.control();
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        control = shared.wake.wait_timeout(control, deadline - now).unwrap().0;
    }
}

/// New files a run stopped before (a sync or stop came first): kept for the next round.
fn left(shared: &Shared, part: &Part, files: &[i64]) -> Result<Vec<i64>> {
    let control = shared.control();
    let yielded = control.sync || control.stop || control.reload || control.paused || part.code && control.code_sync;
    drop(control);
    if !yielded {
        return Ok(Vec::new());
    }
    Ok(part.store.pending(None, Some(files), None)?.into_iter().map(|file| file.id).collect())
}

/// A part's chosen folders that are there and readable right now; also what `pending` and the counts look at.
fn available_roots(shared: &Shared, part: &mut Part) -> Vec<PathBuf> {
    let (present, missing): (Vec<PathBuf>, Vec<PathBuf>) =
        part.roots.iter().cloned().partition(|root| std::fs::read_dir(root).is_ok());
    let missing: Vec<String> = missing.iter().map(|root| showme_core::settings::shorten(root)).collect();
    if missing != part.missing {
        part.missing = missing.clone();
        let code = part.code;
        shared.update(|status| if code { status.code_missing = missing } else { status.missing = missing });
    }
    // `pending` and the counts look only at folders that are there (an unplugged drive's files neither fail nor wait).
    part.store.set_scope(Some(present.iter().map(|root| root.to_string_lossy().into_owned()).collect()));
    present
}

fn sync(shared: &Shared, part: &mut Part, new_files: &mut Vec<i64>) {
    shared.update(|status| {
        status.state = "syncing".into();
        status.detail = part.code.then(|| "code".into());
    });
    let roots = available_roots(shared, part);
    match part.indexer.sync(&roots) {
        Ok(summary) => {
            // A few new files go first, whatever the power source; a big batch is the backfill.
            if summary.queued.len() <= 50 {
                new_files.extend(summary.queued);
            }
            if summary.added + summary.changed + summary.moved + summary.removed > 0 {
                shared.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(error) => shared.update(|status| status.last_error = Some(format!("sync failed: {error}"))),
    }
}

/// Embeds a part's pending files (`only` some). An error stops indexing (the model, or another model's index).
fn embed(shared: &Shared, part: &mut Part, only: Option<&[i64]>) -> Result<()> {
    let new = only.is_some();
    let code = part.code;
    let mut should_stop = || {
        let control = shared.control();
        control.sync
            || control.stop
            || control.reload
            || code && control.code_sync
            || (!new && (control.paused || control.blocker.is_some() || step_over(shared, control)))
    };
    shared.update(|status| {
        status.state = "indexing".into();
        status.detail = code.then(|| "code".into());
    });
    let result = part.indexer.run(None, only, &mut should_stop, &mut |event| {
        shared.generation.fetch_add(1, Ordering::Relaxed);
        shared.indexed.fetch_add(1, Ordering::Relaxed);
        shared.update(|status| {
            status.current = Some(event.path.clone());
            status.done = event.done;
            status.total = event.total;
            if event.state == "failed" {
                let name = event.path.rsplit('/').next().unwrap_or(&event.path);
                status.last_error = Some(format!("{name}: {}", event.note.clone().unwrap_or_default()));
            }
        });
    });
    result.map(|_| ()).map_err(anyhow::Error::from)
}

/// Whether the first pass has read for a whole step (`step_minutes`): then it pauses, says so, and waits to be told to
/// go on. The step's clock starts with its first file.
fn step_over(shared: &Shared, mut control: MutexGuard<'_, Control>) -> bool {
    let minutes = shared.config.lock().unwrap().step_minutes;
    if minutes <= 0.0 {
        return false;
    }
    let started = *control.step_started.get_or_insert_with(Instant::now);
    if started.elapsed() < Duration::from_secs_f64(minutes * 60.0) {
        return false;
    }
    control.paused = true;
    control.step_done = true;
    control.step_started = None;
    drop(control);
    remember_pause(true, true);
    let status = shared.status.lock().unwrap().clone();
    let share = (100 * (status.files - status.pending)).checked_div(status.files).unwrap_or(100);
    notify(
        "ShowMe paused after a step",
        &format!("{share}% of your folders are read. Resume in ShowMe's settings, or with: showme resume"),
    );
    true
}

fn finish_long_files(shared: &Shared, part: &mut Part, has_code: bool) -> Result<()> {
    shared.update(|status| {
        status.state = "finishing".into();
        status.detail = None;
    });
    let mut should_stop = || {
        let control = shared.control();
        // Without code folders nothing takes `code_sync`: left to count, it would stop every run before its first step.
        control.sync
            || control.code_sync && has_code
            || control.stop
            || control.reload
            || control.paused
            || control.blocker.is_some()
            || step_over(shared, control)
    };
    part.indexer.finish_long_files(&mut should_stop, &mut |step| {
        shared.generation.fetch_add(1, Ordering::Relaxed);
        shared.update(|status| {
            status.current = Some(step.path.clone());
            status.done = step.done;
            status.total = step.total;
        });
    })?;
    Ok(())
}

/// Both indexes' counts into the status.
fn counts(shared: &Shared, main: &Part, code: &Part) {
    // All of what's indexed, a folder that isn't there right now included: its files are still found.
    let unscoped = |store: &IndexStore| {
        let scope = store.scope();
        store.set_scope(None);
        let counts = store.counts();
        store.set_scope(scope);
        counts
    };
    let (Ok(main_counts), Ok(code_counts)) = (unscoped(main.store), unscoped(code.store)) else {
        return;
    };
    let tally = |counts: &showme_core::store::Counts| {
        let (mut files, mut searchable, mut pending) = (0, 0, 0);
        for states in counts.by_kind_and_state.values() {
            for (state, count) in states {
                files += count;
                if state == "done" || state == "partial" {
                    searchable += count;
                }
                if state == "pending" {
                    pending += count;
                }
            }
        }
        (files, searchable, pending)
    };
    let (files, searchable, pending) = tally(&main_counts);
    let (code_files, _, code_pending) = tally(&code_counts);
    let mut folders = Vec::new();
    for part in [main, code] {
        let roots: Vec<String> = part.roots.iter().map(|root| root.to_string_lossy().into_owned()).collect();
        if let Ok(progress) = part.store.progress(&roots) {
            folders.extend(roots.into_iter().zip(progress).map(|(path, (files, read))| FolderProgress {
                path,
                files,
                read,
                code: part.code,
            }));
        }
    }
    shared.update(|status| {
        status.files = files;
        status.searchable = searchable;
        status.pending = pending;
        status.vectors = main_counts.vectors + code_counts.vectors;
        status.unfinished = main_counts.unfinished;
        status.code_files = code_files;
        status.code_pending = code_pending;
        status.folders = folders;
    });
}

/// Scratch folders of indexing processes that are gone (killed mid-batch).
fn remove_stale_scratch_folders() {
    let Ok(entries) = std::fs::read_dir(showme_core::paths::scratch_root()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix("showme-").and_then(|pid| pid.parse::<u32>().ok()) else {
            continue;
        };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

// Watching

/// Folders whose changes don't concern the code index.
const CODE_NOISE: &[&str] = &[
    "node_modules",
    "bower_components",
    "venv",
    "site-packages",
    "__pycache__",
    "vendor",
    "Vendor",
    "third_party",
    "build",
    "Build",
    "dist",
    "out",
    "target",
    "obj",
    "coverage",
    "wandb",
    "mlruns",
    "lightning_logs",
];

fn watch_loop(shared: &Arc<Shared>) {
    loop {
        let config = shared.config.lock().unwrap().clone();
        let (main_roots, _) = config.main();
        let (code_roots, _) = config.code();
        let (sender, events) = mpsc::channel();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let _ = sender.send(event);
        });
        let mut watcher = match watcher {
            Ok(watcher) => watcher,
            Err(error) => {
                shared.update(|status| status.last_error = Some(format!("can't watch the folders: {error}")));
                return;
            }
        };
        let mut failed = Vec::new();
        for root in main_roots.iter().chain(&code_roots) {
            if root.exists()
                && let Err(error) = notify::Watcher::watch(&mut watcher, root, notify::RecursiveMode::Recursive)
            {
                failed.push(format!("{}: {error}", showme_core::settings::shorten(root)));
            }
        }
        if !failed.is_empty() {
            // Most often inotify's limit (fs.inotify.max_user_watches): changes there are found by the hourly sync.
            shared.update(|status| status.last_error = Some(format!("can't watch {}", failed.join("; "))));
        }
        let generation = config.clone();
        let mut main_due: Option<Instant> = None;
        let (mut code_due, mut code_since): (Option<Instant>, Option<Instant>) = (None, None);
        let mut hourly = Instant::now() + Duration::from_secs(3600);
        loop {
            let next = [main_due, code_due, Some(hourly)].into_iter().flatten().min().unwrap();
            match events.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(Ok(event)) => {
                    // A file or folder opened or read changes nothing, and syncing opens every folder: left to count,
                    // each sync would set off the next. A file closed after writing is a change.
                    use notify::event::{AccessKind, AccessMode, EventKind};
                    if matches!(event.kind, EventKind::Access(kind) if kind != AccessKind::Close(AccessMode::Write)) {
                        continue;
                    }
                    let rescan = event.need_rescan();
                    for path in &event.paths {
                        let hidden = path.components().any(|part| part.as_os_str().to_string_lossy().starts_with('.'));
                        if hidden && !rescan {
                            continue;
                        }
                        if main_roots.iter().any(|root| path.starts_with(root)) || rescan {
                            main_due = Some(Instant::now() + Duration::from_millis(750));
                        }
                        let noise = path
                            .components()
                            .any(|part| CODE_NOISE.contains(&part.as_os_str().to_string_lossy().as_ref()));
                        if code_roots.iter().any(|root| path.starts_with(root)) && (!noise || rescan) {
                            let now = Instant::now();
                            let since = *code_since.get_or_insert(now);
                            let quiet = Duration::from_secs_f64(config.code_quiet_seconds.max(1.0));
                            let longest = Duration::from_secs(600).saturating_sub(now - since);
                            code_due = Some(now + quiet.min(longest));
                        }
                    }
                }
                Ok(Err(_)) => main_due = Some(Instant::now() + Duration::from_secs(1)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            if main_due.is_some_and(|due| due <= now) {
                main_due = None;
                shared.request(|control| control.sync = true);
            }
            if code_due.is_some_and(|due| due <= now) {
                (code_due, code_since) = (None, None);
                shared.request(|control| control.code_sync = true);
            }
            if hourly <= now {
                hourly = now + Duration::from_secs(3600);
                shared.request(|control| {
                    control.sync = true;
                    control.code_sync = true;
                });
            }
            // New settings: watch the new folders.
            if *shared.config.lock().unwrap() != generation {
                break;
            }
            if shared.control().stop {
                return;
            }
        }
    }
}

// Power

fn power_loop(shared: &Arc<Shared>) {
    loop {
        let (on_battery, power_saver) = {
            let config = shared.config.lock().unwrap();
            (config.index_on_battery, config.pause_in_power_saver)
        };
        let blocker = power_blocker(on_battery, power_saver);
        let changed = shared.control().blocker != blocker;
        if changed {
            shared.request(|control| control.blocker = blocker);
        }
        if shared.control().stop {
            return;
        }
        thread::sleep(Duration::from_secs(30));
    }
}

/// Why the backfill has to wait, or None: power-saver mode, or on battery when that's not allowed. A machine without
/// a battery is always on AC power.
pub fn power_blocker(index_on_battery: bool, pause_in_power_saver: bool) -> Option<String> {
    let profile = pause_in_power_saver.then(|| std::process::Command::new("powerprofilesctl").arg("get").output().ok());
    if profile.flatten().is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "power-saver") {
        return Some("power saver".into());
    }
    if !index_on_battery && on_battery() {
        return Some("on battery".into());
    }
    None
}

fn on_battery() -> bool {
    let Ok(supplies) = std::fs::read_dir("/sys/class/power_supply") else {
        return false;
    };
    let mut mains = Vec::new();
    for supply in supplies.flatten() {
        let kind = std::fs::read_to_string(supply.path().join("type")).unwrap_or_default();
        if kind.trim() == "Mains" {
            mains.push(std::fs::read_to_string(supply.path().join("online")).unwrap_or_default().trim() == "1");
        }
    }
    !mains.is_empty() && !mains.iter().any(|online| *online)
}

// Searching

/// What the search thread is asked to do.
enum SearchJob {
    Search {
        query: String,
        limit: usize,
        kinds: Vec<String>,
        prefix_last: bool,
        words_only: bool,
        seq: Option<u64>,
        reply: mpsc::Sender<Response>,
    },
    Text {
        segment_id: i64,
        code: bool,
        reply: mpsc::Sender<Response>,
    },
    /// The panel opened: load the query encoder and the vectors before the first search needs them.
    Prewarm,
    /// New settings (maybe another index folder): open the indexes again.
    Forget,
}

#[derive(Default)]
struct SearchState {
    /// The searchers, each with the generation its vectors are from. Their stores are opened once and kept.
    main: Option<(Searcher<'static>, u64)>,
    code: Option<(Searcher<'static>, u64)>,
    encoder: Option<LlamaEmbedder>,
    last_used: Option<Instant>,
    last_reload: Option<Instant>,
}

/// Searches one at a time, on a thread of their own: the searchers and the query encoder live here. When nobody has
/// searched for `encoder_idle_seconds`, the encoder and the vectors in memory are let go.
fn search_loop(shared: &Shared, jobs: mpsc::Receiver<SearchJob>) {
    let mut state = SearchState::default();
    loop {
        match jobs.recv_timeout(Duration::from_secs(30)) {
            Ok(SearchJob::Search { query, limit, kinds, prefix_last, words_only, seq, reply }) => {
                let mut response = search(shared, &mut state, &query, limit, &kinds, prefix_last, words_only);
                response.seq = seq;
                let _ = reply.send(response);
            }
            Ok(SearchJob::Text { segment_id, code, reply }) => {
                let response = match searcher(shared, &mut state, code) {
                    Ok(searcher) => match searcher.text_of_segment(segment_id) {
                        Ok(text) => Response { text, ..Response::ok() },
                        Err(error) => Response::error(error.to_string()),
                    },
                    Err(response) => response,
                };
                let _ = reply.send(response);
            }
            Ok(SearchJob::Prewarm) => {
                state.last_used = Some(Instant::now());
                let loaded = match searcher(shared, &mut state, false) {
                    Ok(searcher) if searcher.vector_source.is_some() => {
                        if !searcher.vectors_loaded {
                            let _ = searcher.reload();
                        }
                        true
                    }
                    _ => false,
                };
                if loaded {
                    let folder = shared.config.lock().unwrap().index_folder();
                    let _ = encoder(shared, &mut state, &folder);
                }
            }
            Ok(SearchJob::Forget) => {
                state.main = None;
                state.code = None;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let idle = Duration::from_secs_f64(shared.config.lock().unwrap().encoder_idle_seconds.max(1.0));
        if state.last_used.is_some_and(|used| used.elapsed() > idle) {
            state.encoder = None;
            shared.update(|status| status.encoder = "idle".into());
            for searcher in [&mut state.main, &mut state.code].into_iter().flatten() {
                searcher.0.release_vectors(); // they load again with the next meaning search
            }
            state.last_used = None;
        }
    }
}

/// The searcher of the main or the code index, opened (once) and reloaded after indexing changed things (at most every
/// 5 s while it's busy).
#[allow(clippy::result_large_err)] // the error is the reply to send
fn searcher<'s>(
    shared: &Shared,
    state: &'s mut SearchState,
    code: bool,
) -> Result<&'s mut Searcher<'static>, Response> {
    let folder = shared.config.lock().unwrap().index_folder();
    let generation = shared.generation.load(Ordering::Relaxed);
    let reload_due = state.last_reload.is_none_or(|last| last.elapsed() > Duration::from_secs(5));
    let slot = if code { &mut state.code } else { &mut state.main };
    if slot.is_none() {
        let name = if code { IndexStore::CODE_FILE } else { IndexStore::MAIN_FILE };
        let store = match IndexStore::open(&folder, name) {
            Ok(store) => &*Box::leak(Box::new(store)), // opened once for the daemon's life
            Err(error) => return Err(Response::error(format!("can't open the index: {error}"))),
        };
        match Searcher::new(store, false) {
            Ok(searcher) => *slot = Some((searcher, generation)),
            Err(error) => return Err(Response::error(format!("can't read the index: {error}"))),
        }
    }
    let (searcher, seen) = slot.as_mut().unwrap();
    if *seen != generation && reload_due {
        let result = if searcher.vectors_loaded { searcher.reload() } else { searcher.reload_files() };
        if let Err(error) = result {
            return Err(Response::error(format!("can't read the index: {error}")));
        }
        *seen = generation;
        state.last_reload = Some(Instant::now());
    }
    let slot = if code { &mut state.code } else { &mut state.main };
    Ok(&mut slot.as_mut().unwrap().0)
}

/// The query encoder (the text part of the model, ~0.25 GB), loaded on first use.
fn encoder<'s>(shared: &Shared, state: &'s mut SearchState, folder: &Path) -> Result<&'s mut LlamaEmbedder, String> {
    if state.encoder.is_none() {
        shared.update(|status| status.encoder = "starting".into());
        let log = folder.join("worker-query.log");
        match LlamaModels::locate().and_then(|models| LlamaEmbedder::new(&models, true, Some(&log))) {
            Ok(encoder) => state.encoder = Some(encoder),
            Err(error) => {
                shared.update(|status| status.encoder = "failed".into());
                return Err(error.to_string());
            }
        }
        shared.update(|status| status.encoder = "ready".into());
    }
    Ok(state.encoder.as_mut().unwrap())
}

fn search(
    shared: &Shared,
    state: &mut SearchState,
    query: &str,
    limit: usize,
    kinds: &[String],
    prefix_last: bool,
    words_only: bool,
) -> Response {
    let started = Instant::now();
    let kinds = match protocol::kinds(kinds) {
        Ok(kinds) => kinds,
        Err(error) => return Response::error(error),
    };
    let code_text = code_query::text(query);
    let is_code = code_text.is_some();
    let text = code_text.unwrap_or_else(|| query.to_string());
    let terms = showme_core::searcher::terms(&text);
    if text.trim().is_empty() {
        let empty = Response { hits: Some(Vec::new()), meaning: Some(false), terms: Some(terms), ..Response::ok() };
        return Response { code: Some(is_code), ..empty };
    }
    let folder = shared.config.lock().unwrap().index_folder();
    state.last_used = Some(Instant::now());
    let has_vectors = match searcher(shared, state, is_code) {
        Ok(searcher) => searcher.vector_source.is_some(),
        Err(response) => return response,
    };
    // Meaning needs the query encoder, loaded on the first search (or when the panel opens: `prewarm`).
    let mut vector = None;
    if !words_only && has_vectors {
        let prompt = if is_code { prompts::code_query(&text) } else { prompts::query(&text) };
        let embedded = encoder(shared, state, &folder)
            .and_then(|encoder| encoder.embed_texts(&[prompt]).map_err(|error| error.to_string()));
        match embedded {
            Ok(mut vectors) => vector = vectors.pop(),
            Err(error) => {
                state.encoder = None;
                return Response::error(error);
            }
        }
    }
    let meaning = vector.is_some();
    let searcher = match searcher(shared, state, is_code) {
        Ok(searcher) => searcher,
        Err(response) => return response,
    };
    let options = SearchOptions { kinds, limit, prefix_last, ..Default::default() };
    match searcher.search(&text, vector.as_deref(), &options) {
        Ok(hits) => {
            let strong = strong_count(&hits, 0.5);
            Response {
                hits: Some(hits.iter().enumerate().map(|(rank, hit)| Hit::new(hit, rank < strong)).collect()),
                vectors: Some(searcher.vector_count()),
                ms: Some((started.elapsed().as_secs_f64() * 10000.0).round() / 10.0),
                meaning: Some(meaning),
                code: Some(is_code),
                terms: Some(terms),
                ..Response::ok()
            }
        }
        Err(error) => Response::error(format!("search failed: {error}")),
    }
}

// Clients

fn serve(stream: UnixStream, shared: &Arc<Shared>) {
    let Ok(reader) = stream.try_clone() else { return };
    let mut writer = stream;
    // Requests are read ahead into `queue`, so a full search can see that a newer one has come in behind it.
    let (lines, incoming) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { return };
            if lines.send(line).is_err() {
                return;
            }
        }
    });
    let mut queue: VecDeque<Request> = VecDeque::new();
    loop {
        // Everything that has arrived; with nothing to do, wait for the next line.
        let mut arrived: Vec<String> = incoming.try_iter().collect();
        if arrived.is_empty() && queue.is_empty() {
            match incoming.recv() {
                Ok(line) => arrived.push(line),
                Err(_) => return, // the client left
            }
        }
        for line in arrived {
            match parse(&line) {
                Ok(Some(request)) => queue.push_back(request),
                Ok(None) => {}
                Err(response) => {
                    if send(&mut writer, &response).is_err() {
                        return;
                    }
                }
            }
        }
        let Some(request) = queue.pop_front() else { continue };
        let response = match request {
            Request::Search { query, limit, kinds, prefix_last, words_only, seq } => {
                // A full search a newer one on this connection has already replaced: not worth an embedding.
                let replaced =
                    !words_only && queue.iter().any(|later| matches!(later, Request::Search { words_only: false, .. }));
                if replaced {
                    if send(&mut writer, &Response { seq, ..Response::error("superseded") }).is_err() {
                        return;
                    }
                    continue;
                }
                let (reply, answer) = mpsc::channel();
                let job = SearchJob::Search { query, limit, kinds, prefix_last, words_only, seq, reply };
                match shared.search.send(job).ok().and_then(|_| answer.recv().ok()) {
                    Some(response) => response,
                    None => Response::error("the search thread stopped"),
                }
            }
            Request::SegmentText { segment_id, code } => {
                let (reply, answer) = mpsc::channel();
                let job = SearchJob::Text { segment_id, code, reply };
                match shared.search.send(job).ok().and_then(|_| answer.recv().ok()) {
                    Some(response) => response,
                    None => Response::error("the search thread stopped"),
                }
            }
            Request::Prewarm => {
                let _ = shared.search.send(SearchJob::Prewarm);
                Response::ok()
            }
            Request::Download => {
                download(shared);
                Response::ok()
            }
            Request::Status => {
                Response { status: Some(Box::new(shared.status.lock().unwrap().clone())), ..Response::ok() }
            }
            Request::Watch => {
                watch(&mut writer, shared);
                return;
            }
            Request::Pause => {
                shared.request(|control| {
                    control.paused = true;
                    control.step_done = false;
                });
                remember_pause(true, false);
                shared.update(|status| {
                    status.state = "paused".into();
                    status.detail = None;
                });
                Response::ok()
            }
            Request::Resume => {
                shared.request(|control| {
                    control.paused = false;
                    control.step_done = false;
                    control.step_started = None;
                });
                remember_pause(false, false);
                Response::ok()
            }
            Request::Sync => {
                shared.request(|control| {
                    control.sync = true;
                    control.code_sync = true;
                });
                Response::ok()
            }
            Request::Reload => match Config::load() {
                Ok(config) => {
                    *shared.config.lock().unwrap() = config;
                    shared.request(|control| control.reload = true);
                    Response::ok()
                }
                Err(error) => Response::error(format!("{error:#}")),
            },
            Request::Quit => {
                shared.request(|control| control.stop = true);
                let _ = send(&mut writer, &Response::ok());
                // The indexing thread stops between batches; the process ends when it has.
                return;
            }
        };
        if send(&mut writer, &response).is_err() {
            return;
        }
    }
}

/// A request line: the request, nothing (a blank line), or the error to answer.
#[allow(clippy::result_large_err)] // the error is the reply to send
fn parse(line: &str) -> Result<Option<Request>, Response> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line).map(Some).map_err(|error| Response::error(format!("bad request: {error}")))
}

/// Downloads the model and pdfium in the background, with the progress in the status; indexing starts once they're in.
fn download(shared: &Arc<Shared>) {
    if shared.status.lock().unwrap().model.state == "downloading" {
        return;
    }
    let state = |state: &str, fraction: f64, text: Option<String>| protocol::ModelStatus {
        state: state.into(),
        fraction,
        text,
    };
    shared.update(|status| status.model = state("downloading", 0.0, None));
    let shared = shared.clone();
    thread::spawn(move || {
        let folder = models::installed();
        let sizes: Vec<(String, u64)> =
            models::missing(&folder).iter().map(|file| (file.name.to_string(), file.bytes)).collect();
        let total = sizes.iter().map(|(_, bytes)| bytes).sum::<u64>().max(1);
        let started = Instant::now();
        let mut last = Instant::now() - Duration::from_secs(1);
        let result = models::download(&folder, &mut |file, have, _| {
            if last.elapsed() < Duration::from_millis(250) {
                return;
            }
            last = Instant::now();
            // The files before this one are whole.
            let before: u64 = sizes.iter().take_while(|(name, _)| name != file.name).map(|(_, bytes)| bytes).sum();
            let fraction = ((before + have) as f64 / total as f64).min(1.0);
            let elapsed = started.elapsed().as_secs_f64();
            let left = if fraction > 0.02 { elapsed / fraction - elapsed } else { 0.0 };
            let percent = (fraction * 100.0) as u32;
            let text = if left > 90.0 {
                format!("{percent}% · about {} min left", (left / 60.0).round())
            } else {
                format!("{percent}%")
            };
            shared.update(|status| status.model = state("downloading", fraction, Some(text)));
        })
        .and_then(|_| models::download_pdfium().map(|_| ()));
        match result {
            Ok(()) => {
                shared.update(|status| status.model = state("ready", 1.0, None));
                notify("ShowMe's model is ready", "Your files are being read now.");
                shared.request(|control| control.sync = true);
            }
            Err(error) => {
                notify("ShowMe couldn't download its model", &error);
                shared.update(|status| status.model = state("failed", 0.0, Some(error)));
            }
        }
    });
}

fn send(writer: &mut UnixStream, response: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_string(response).unwrap_or_default();
    line.push('\n');
    writer.write_all(line.as_bytes())
}

/// The status now, then again each time it changes (at most twice a second), until the client leaves.
fn watch(writer: &mut UnixStream, shared: &Shared) {
    let mut seen = u64::MAX;
    loop {
        let version = {
            let mut version = shared.status_version.lock().unwrap();
            while *version == seen {
                version = shared.status_changed.wait_timeout(version, Duration::from_secs(30)).unwrap().0;
                if shared.control().stop {
                    return;
                }
            }
            *version
        };
        seen = version;
        let status = shared.status.lock().unwrap().clone();
        if send(writer, &Response { status: Some(Box::new(status)), ..Response::ok() }).is_err() {
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// A desktop notification (mako on Omarchy), through notify-send. Quietly nothing without it.
fn notify(summary: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=ShowMe", "--icon=system-search", summary, body])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
