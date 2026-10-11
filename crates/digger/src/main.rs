//! Digger's command line: download the model, index folders, search them, and measure search with the evals. Every
//! command works on an explicit index folder (`--index`), so tests never touch the real one.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use digger_core::embedder::Embedder;
use digger_core::indexer::Indexer;
use digger_core::model::SegmentKind;
use digger_core::model::{FileKind, IndexOptions, code_query, prompts};
use digger_core::pdf;
use digger_core::searcher::{SearchHit, SearchOptions, Searcher, strong_count};
use digger_core::store::IndexStore;
use digger_embed::{LlamaEmbedder, LlamaModels, llama, models};
use protocol::{Client, Request};

mod config;
mod daemon;
mod protocol;

#[derive(Parser)]
#[command(
    name = "digger",
    version,
    about = "Search your files by what's in them, with EmbeddingGemma 2"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// What llama.cpp was built with and the devices (GPUs) it can use.
    Info,
    /// Downloads the model (865 MB, once) and checks it.
    Download,
    /// Syncs the index with these folders, embeds what's new, then the rest of long documents.
    Index {
        folders: Vec<PathBuf>,
        #[command(flatten)]
        index: IndexArgs,
        /// Stop after this many files.
        #[arg(long)]
        limit: Option<usize>,
        /// Drop vectors and embed everything again (after changing settings or model).
        #[arg(long)]
        reindex: bool,
    },
    /// Searches the index. "code: <words>" searches the code index.
    Search {
        words: Vec<String>,
        #[command(flatten)]
        index: IndexArgs,
        #[command(flatten)]
        tuning: Tuning,
        /// Only these kinds: screenshot, image, pdf, doc, audio, video.
        #[arg(long, value_delimiter = ',')]
        kind: Vec<FileKind>,
        #[arg(short = 'n', default_value_t = 10)]
        n: usize,
        #[arg(long)]
        json: bool,
    },
    /// What's in the index, and what the daemon is doing.
    Status {
        #[command(flatten)]
        index: IndexArgs,
        /// One line of JSON for a Waybar custom module.
        #[arg(long)]
        waybar: bool,
        /// With --waybar: a new line each time it changes (Waybar's continuous mode).
        #[arg(long)]
        follow: bool,
    },
    /// Indexes the folders in ~/.config/digger/config.toml and answers searches: the systemd user service.
    Daemon,
    /// Adds folders to search (or, with --code, to code search) and tells the daemon.
    Add {
        folders: Vec<PathBuf>,
        #[arg(long)]
        code: bool,
    },
    /// Stops searching folders (inside a searched one: skips them) and tells the daemon.
    Remove {
        folders: Vec<PathBuf>,
        #[arg(long)]
        code: bool,
    },
    /// Pauses the big first pass (new files are still indexed).
    Pause,
    /// Lets it go on.
    Resume,
    /// Makes the daemon read config.toml again.
    Reload,
    /// Recall of known answers (see evals/; "code: …" queries search the code index).
    Eval {
        file: PathBuf,
        #[command(flatten)]
        index: IndexArgs,
        #[command(flatten)]
        tuning: Tuning,
        #[arg(short = 'n', default_value_t = 10)]
        n: usize,
    },
}

#[derive(Args, Clone)]
struct IndexArgs {
    /// The index folder (default: ~/.local/share/digger/index).
    #[arg(long)]
    index: Option<PathBuf>,
    /// The code index of these folders (code.sqlite in the index folder): source files, their repos' docs and
    /// notebooks, minus what git ignores.
    #[arg(long)]
    code: bool,
    /// Also index loose source code and data files as documents (outside code projects).
    #[arg(long)]
    include_code: bool,
    /// Don't skip folders that look like datasets.
    #[arg(long)]
    include_datasets: bool,
    /// Don't index this folder (repeatable).
    #[arg(long)]
    exclude: Vec<PathBuf>,
    /// Don't index files with this extension (repeatable, or comma-separated: "heic,txt").
    #[arg(long)]
    exclude_ext: Vec<String>,
    /// Characters a stretch of code holds at most.
    #[arg(long)]
    code_stretch: Option<usize>,
}

#[derive(Args, Clone)]
struct Tuning {
    /// How far a meaning match must stand out.
    #[arg(long, default_value_t = 2.0)]
    zgate: f64,
    /// Bonus for containing all of the query's words (default 2.0, and 1.0 for code).
    #[arg(long)]
    word_weight: Option<f64>,
}

impl IndexArgs {
    fn folder(&self) -> PathBuf {
        self.index
            .clone()
            .map(|path| absolute(&path))
            .unwrap_or_else(|| models::data_folder().join("index"))
    }

    fn store(&self) -> Result<IndexStore> {
        let name = if self.code {
            IndexStore::CODE_FILE
        } else {
            IndexStore::MAIN_FILE
        };
        IndexStore::open(&self.folder(), name)
            .with_context(|| format!("can't open the index in {}", self.folder().display()))
    }

    fn options(&self) -> IndexOptions {
        let mut options = IndexOptions {
            code: self.code,
            include_code: self.include_code,
            include_datasets: self.include_datasets,
            excluded_folders: self
                .exclude
                .iter()
                .map(|path| digger_core::crawler::standardized(&absolute(path)))
                .collect(),
            excluded_extensions: IndexOptions::extensions(&self.exclude_ext.join(",")),
            ..Default::default()
        };
        if let Some(stretch) = self.code_stretch {
            options.code_stretch = stretch;
        }
        options
    }
}

fn absolute(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let path = match text.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => path.to_path_buf(),
    };
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Info => {
            for line in llama::devices() {
                println!("{line}");
            }
            match LlamaModels::locate() {
                Ok(found) => {
                    println!("model: {}", found.text.display());
                    match &found.media {
                        Some(media) => println!("image and audio encoders: {}", media.display()),
                        None => println!("image and audio encoders: not downloaded"),
                    }
                }
                Err(error) => println!("{error}"),
            }
            println!("vectors: version {}", llama::VECTOR_VERSION);
            Ok(())
        }
        Command::Download => download(),
        Command::Index {
            folders,
            index,
            limit,
            reindex,
        } => index_folders(&folders, &index, limit, reindex),
        Command::Search {
            words,
            index,
            tuning,
            kind,
            n,
            json,
        } => search(&words.join(" "), &index, &tuning, &kind, n, json),
        Command::Status {
            index,
            waybar: true,
            follow,
        } => waybar(&index, follow),
        Command::Status { index, .. } => status(&index),
        Command::Daemon => daemon::run(),
        Command::Add { folders, code } => change_folders(&folders, code, true),
        Command::Remove { folders, code } => change_folders(&folders, code, false),
        Command::Pause => daemon_command(&Request::Pause),
        Command::Resume => daemon_command(&Request::Resume),
        Command::Reload => daemon_command(&Request::Reload),
        Command::Eval { file, index, tuning, n } => evaluate(&file, &index, &tuning, n),
    }
}

// Formatting

fn tilde(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn duration(seconds: f64) -> String {
    if seconds < 90.0 {
        format!("{} s", seconds.round())
    } else if seconds < 5400.0 {
        format!("{} min", (seconds / 60.0).round())
    } else {
        format!("{:.1} h", seconds / 3600.0)
    }
}

fn clock(seconds: f64) -> String {
    let total = seconds as u64;
    if total >= 3600 {
        format!("{}:{:02}:{:02}", total / 3600, total / 60 % 60, total % 60)
    } else {
        format!("{}:{:02}", total / 60, total % 60)
    }
}

fn location(hit: &SearchHit) -> String {
    let Some(loc) = hit.loc else {
        return if hit.segment == SegmentKind::Ocr {
            "text in image".into()
        } else {
            String::new()
        };
    };
    let end = hit.loc_end.unwrap_or(loc);
    match hit.segment {
        SegmentKind::Page | SegmentKind::PageImage => format!("p. {}", loc as i64),
        SegmentKind::Audio => format!("at {} (sound)", clock(loc)),
        SegmentKind::Frame => format!("at {}", clock(loc)),
        SegmentKind::Chunk => format!("part {}", loc as i64),
        SegmentKind::Lines => {
            let opens = hit
                .focus
                .map(|line| format!(", opens at {}", line as i64))
                .unwrap_or_default();
            format!("lines {}–{}{opens}", loc as i64, end as i64)
        }
        SegmentKind::Cells if loc == end => format!("cell {}", loc as i64),
        SegmentKind::Cells => format!("cells {}–{}", loc as i64, end as i64),
        _ => String::new(),
    }
}

fn counted(map: &BTreeMap<String, usize>) -> String {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    entries
        .iter()
        .map(|(reason, count)| format!("{count} {reason}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn states_line(states: &BTreeMap<String, usize>) -> String {
    states
        .iter()
        .map(|(state, count)| format!("{count} {state}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

// Commands

fn download() -> Result<()> {
    let folder = models::installed();
    if models::missing(&folder).is_empty() {
        println!("the model is in {}", folder.display());
    } else {
        let mut last = (String::new(), 0u64);
        models::download(&folder, &mut |file, have, total| {
            let percent = have * 100 / total.max(1);
            if file.name != last.0 || percent != last.1 {
                eprint!("\r{}  {percent}% of {} MB   ", file.name, total >> 20);
                last = (file.name.to_string(), percent);
            }
        })
        .map_err(anyhow::Error::msg)?;
        eprintln!();
        println!("the model is in {}", folder.display());
    }
    download_pdfium()?;
    for (tool, package, what) in [
        ("ffmpeg", "ffmpeg", "recordings and videos"),
        (
            "tesseract",
            "tesseract tesseract-data-eng tesseract-data-nor",
            "the words in screenshots",
        ),
    ] {
        if !have(tool) {
            println!("for {what}, install {tool}: sudo pacman -S {package}");
        }
    }
    Ok(())
}

/// The kinds this machine can read, and what's missing for the rest: what can't be read without a tool stays
/// pending until it's installed.
pub(crate) fn readable_kinds() -> (Vec<FileKind>, Vec<String>) {
    let mut kinds = Indexer::READABLE.to_vec();
    let mut notes = Vec::new();
    if pdf::pdfium().is_none() {
        kinds.retain(|kind| *kind != FileKind::Pdf);
        notes.push("PDFs wait for pdfium: digger download".to_string());
    }
    if !have("ffmpeg") {
        kinds.retain(|kind| !matches!(kind, FileKind::Audio | FileKind::Video));
        notes.push("recordings and videos wait for ffmpeg: sudo pacman -S ffmpeg".to_string());
    }
    if !have("tesseract") {
        notes.push("screenshots are read without their words: sudo pacman -S tesseract tesseract-data-eng".to_string());
    }
    (kinds, notes)
}

/// Whether a command-line tool is installed.
fn have(tool: &str) -> bool {
    std::process::Command::new(tool)
        .arg(if tool == "tesseract" { "--version" } else { "-version" })
        .output()
        .is_ok_and(|output| output.status.success())
}

/// pdfium, the PDF library Chrome uses, from bblanchon/pdfium-binaries at the build pdfium-render speaks to, checked
/// against its SHA-256. A copy from `$DIGGER_PDFIUM` or the system works too.
fn download_pdfium() -> Result<()> {
    use sha2::Digest;
    let target = pdf::installed_library();
    if target.exists() {
        println!("pdfium is in {}", target.display());
        return Ok(());
    }
    let url = format!(
        "https://github.com/bblanchon/pdfium-binaries/releases/download/{}/{}",
        pdf::PDFIUM_RELEASE.replace('/', "%2F"),
        pdf::PDFIUM_ARCHIVE
    );
    eprintln!("downloading pdfium ({})", pdf::PDFIUM_RELEASE);
    let mut archive = Vec::new();
    std::io::Read::read_to_end(
        &mut models::agent().get(&url).call()?.into_body().into_reader(),
        &mut archive,
    )?;
    let sum: String = sha2::Sha256::digest(&archive)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if sum != pdf::PDFIUM_SHA256 {
        bail!("pdfium didn't arrive whole (its checksum is {sum}); run it again");
    }
    let mut entries = tar::Archive::new(flate2::read::GzDecoder::new(archive.as_slice()));
    for entry in entries.entries()? {
        let mut entry = entry?;
        if entry.path()?.ends_with("lib/libpdfium.so") {
            std::fs::create_dir_all(target.parent().unwrap())?;
            let part = target.with_extension("so.part");
            entry.unpack(&part)?;
            std::fs::rename(&part, &target)?;
            println!("pdfium is in {}", target.display());
            return Ok(());
        }
    }
    bail!("no libpdfium.so in {}", pdf::PDFIUM_ARCHIVE)
}

/// The embedder for indexing: the whole model, with its log in the index folder.
fn make_embedder(log: PathBuf) -> digger_core::indexer::MakeEmbedder {
    Box::new(move || {
        let found = LlamaModels::locate()?;
        Ok(Box::new(LlamaEmbedder::new(&found, false, Some(&log))?) as Box<dyn Embedder>)
    })
}

fn index_folders(folders: &[PathBuf], args: &IndexArgs, limit: Option<usize>, reindex: bool) -> Result<()> {
    if folders.is_empty() {
        bail!("which folders? e.g. digger index ~/Documents");
    }
    let mut roots = Vec::new();
    for folder in folders {
        let folder = absolute(folder);
        if !folder.is_dir() {
            bail!("not a folder: {}", folder.display());
        }
        // A folder that can't be listed looks empty to a crawl, which must never be taken for "everything in it was
        // deleted".
        std::fs::read_dir(&folder).with_context(|| format!("can't read {}", folder.display()))?;
        roots.push(folder);
    }
    let store = args.store()?;
    if reindex {
        store.reset_all()?;
    }
    let mut indexer = Indexer::new(
        &store,
        args.options(),
        make_embedder(store.directory.join("worker.log")),
    );
    indexer.embedder_source = Some(llama::model_info(false).vector_source());
    indexer.check_model()?;
    let sync = indexer.sync(&roots)?;
    println!("index: {}", tilde(&store.file.to_string_lossy()));
    println!(
        "sync: {} new · {} changed · {} moved · {} removed · {} unchanged",
        sync.added, sync.changed, sync.moved, sync.removed, sync.unchanged
    );
    if !sync.crawl.skipped.is_empty() {
        println!("skipped: {}", counted(&sync.crawl.skipped));
    }
    for (folder, count) in &sync.crawl.datasets {
        println!("looks like a dataset (skipped): {} ({count} files)", tilde(folder));
    }
    let (kinds, notes) = readable_kinds();
    indexer.kinds = kinds;
    for note in notes {
        println!("{note}");
    }
    let waiting = store.pending_count()? - store.pending(None, None, Some(&indexer.kinds))?.len();
    if waiting > 0 {
        println!("{waiting} files wait for those");
    }

    let started = Instant::now();
    let states = indexer.run(limit, None, &mut || false, &mut |event| {
        let name = event.path.rsplit('/').next().unwrap_or(&event.path);
        let note = event
            .note
            .as_ref()
            .map(|note| format!("  ({note})"))
            .unwrap_or_default();
        println!(
            "[{:>4}/{}] {:<10} {:<7} {:5.2}s  {name}{note}",
            event.done,
            event.total,
            event.kind.as_str(),
            event.state,
            event.seconds
        );
    })?;
    if states.is_empty() {
        println!("nothing pending");
    } else {
        println!(
            "done in {}: {}",
            duration(started.elapsed().as_secs_f64()),
            states_line(&states)
        );
    }
    // The rest of long files, once nothing is pending (not after a --limit run: things still are).
    if limit.is_some() || store.unfinished_count()?.0 == 0 {
        return Ok(());
    }
    let started = Instant::now();
    let finished = indexer.finish_long_files(&mut || false, &mut |step| {
        let name = step.path.rsplit('/').next().unwrap_or(&step.path);
        let note = step.note.as_ref().map(|note| format!("  ({note})")).unwrap_or_default();
        println!(
            "[rest {:>5}/{}] {:<4} {:<7} {:5.2}s  {name}{note}",
            step.done,
            step.total,
            step.kind.as_str(),
            step.state,
            step.seconds
        );
    })?;
    let finished = if finished.is_empty() {
        "none finished".into()
    } else {
        states_line(&finished)
    };
    println!(
        "the rest of long files in {}: {finished}",
        duration(started.elapsed().as_secs_f64())
    );
    Ok(())
}

/// Embeds queries with the text model alone (~0.25 GB): the query encoder.
fn query_vectors(
    queries: &[String],
    store: &IndexStore,
    searcher: &Searcher,
    code: bool,
) -> Result<Option<Vec<Vec<f32>>>> {
    if searcher.vector_count() == 0 || queries.is_empty() {
        return Ok(None);
    }
    let found = LlamaModels::locate()?;
    let mut encoder = LlamaEmbedder::new(&found, true, Some(&store.directory.join("worker-query.log")))?;
    if let Some(source) = &searcher.vector_source
        && *source != encoder.info().vector_source()
    {
        bail!(
            "index vectors come from {source} but the query encoder is {}",
            encoder.info().vector_source()
        );
    }
    let prompt = if code { prompts::code_query } else { prompts::query };
    let texts: Vec<String> = queries.iter().map(|query| prompt(query)).collect();
    Ok(Some(encoder.embed_texts(&texts)?))
}

fn search(query: &str, args: &IndexArgs, tuning: &Tuning, kinds: &[FileKind], n: usize, json: bool) -> Result<()> {
    if query.trim().is_empty() {
        bail!("search for what?");
    }
    let code = code_query::text(query);
    let query = code.clone().unwrap_or_else(|| query.to_string());
    if query.is_empty() {
        bail!("search the code for what?");
    }
    // The daemon answers at once (its query encoder is warm): ask it, unless this search is for another index or uses
    // other settings.
    let plain = args.index.is_none() && tuning.zgate == 2.0 && tuning.word_weight.is_none();
    if plain && let Some(mut client) = Client::connect() {
        let request = Request::Search {
            query: code
                .as_ref()
                .map(|code| format!("code: {code}"))
                .unwrap_or_else(|| query.clone()),
            limit: n,
            kinds: kinds.iter().map(|kind| kind.as_str().to_string()).collect(),
            prefix_last: false,
            words_only: false,
        };
        let response = client.send(&request)?;
        if !response.ok {
            bail!(response.error.unwrap_or_default());
        }
        let hits: Vec<SearchHit> = response
            .hits
            .unwrap_or_default()
            .iter()
            .map(protocol::Hit::to_search_hit)
            .collect();
        let footer = format!(
            "({} vectors · search by the daemon {:.0} ms)",
            response.vectors.unwrap_or(0),
            response.ms.unwrap_or(0.0)
        );
        return print_hits(&hits, json, &footer);
    }
    let store = IndexArgs {
        code: code.is_some(),
        ..args.clone()
    }
    .store()?;
    let mut searcher = Searcher::new(&store, true)?;
    let started = Instant::now();
    let vector =
        query_vectors(std::slice::from_ref(&query), &store, &searcher, code.is_some())?.map(|mut v| v.remove(0));
    let encoded = Instant::now();
    let options = SearchOptions {
        kinds: if kinds.is_empty() {
            None
        } else {
            Some(kinds.iter().copied().collect())
        },
        limit: n,
        z_gate: tuning.zgate,
        word_weight: tuning.word_weight,
        prefix_last: false,
    };
    let hits = searcher.search(&query, vector.as_deref(), &options)?;
    let searched = encoded.elapsed();
    let footer = format!(
        "({} vectors · query encoder {:.2} s incl. start-up · search {:.0} ms)",
        searcher.vector_count(),
        encoded.duration_since(started).as_secs_f64(),
        searched.as_secs_f64() * 1000.0
    );
    print_hits(&hits, json, &footer)
}

fn print_hits(hits: &[SearchHit], json: bool, footer: &str) -> Result<()> {
    if json {
        let rows: Vec<serde_json::Value> = hits
            .iter()
            .map(|hit| {
                serde_json::json!({
                    "path": hit.path, "kind": hit.kind.as_str(), "segment": hit.segment.as_str(), "score": hit.score,
                    "z": hit.z, "cosine": hit.cosine, "keyword_rank": hit.keyword_rank, "loc": hit.loc,
                    "loc_end": hit.loc_end, "focus": hit.focus, "excerpt": hit.excerpt,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("no good matches");
    }
    for (rank, hit) in hits.iter().enumerate() {
        let why: Vec<String> = [
            hit.z.map(|z| format!("z {z:.1}")),
            hit.keyword_rank.map(|rank| format!("words #{rank}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        let place = location(hit);
        let (folder, name) = hit.path.rsplit_once('/').unwrap_or(("", &hit.path));
        let place = if place.is_empty() {
            String::new()
        } else {
            format!("  [{place}]")
        };
        println!(
            "{:>2}. {:<10} {name}{place}   ({})",
            rank + 1,
            hit.kind.as_str(),
            why.join(", ")
        );
        println!("      {}", tilde(folder));
        if let Some(excerpt) = hit.excerpt.as_ref().filter(|excerpt| !excerpt.is_empty()) {
            println!("      “{}”", excerpt.chars().take(160).collect::<String>());
        }
    }
    println!("{footer}");
    Ok(())
}

fn status(args: &IndexArgs) -> Result<()> {
    let store = args.store()?;
    let counts = store.counts()?;
    println!("index: {}", tilde(&store.file.to_string_lossy()));
    println!("vectors: {} in {} segments", counts.vectors, counts.segments);
    for kind in FileKind::ALL {
        let Some(states) = counts.by_kind_and_state.get(kind.as_str()) else {
            continue;
        };
        let total: usize = states.values().sum();
        println!("  {:<11} {:>6}   {}", kind.as_str(), total, states_line(states));
    }
    if counts.unfinished > 0 {
        println!(
            "long files still being read: {} ({} pages and chunks left)",
            counts.unfinished, counts.left
        );
    }
    if let Some(fingerprint) = store.meta("fingerprint")? {
        println!("built with: {fingerprint}");
    }
    println!("this digger: vectors {}", llama::VECTOR_VERSION);
    match Client::connect().map(|mut client| client.send(&Request::Status)) {
        Some(Ok(response)) => {
            if let Some(status) = response.status {
                let detail = status.detail.map(|detail| format!(" ({detail})")).unwrap_or_default();
                println!(
                    "daemon: {}{detail}, {} of {} files searchable",
                    status.state, status.searchable, status.files
                );
                if let Some(error) = status.last_error {
                    println!("  last problem: {error}");
                }
            }
        }
        _ => println!("daemon: not running (systemctl --user start digger)"),
    }
    println!("size: {:.1} MB", IndexStore::size(&store.file) as f64 / 1e6);
    Ok(())
}

/// Sends the daemon a command that only says ok or not.
fn daemon_command(request: &Request) -> Result<()> {
    let mut client = Client::connect().context("the daemon isn't running (systemctl --user start digger)")?;
    client.command(request)
}

fn change_folders(folders: &[PathBuf], code: bool, add: bool) -> Result<()> {
    if folders.is_empty() {
        bail!("which folders?");
    }
    let mut settings = config::Config::load()?;
    let (chosen, skipped) = if code {
        (&mut settings.code_folders, &mut settings.code_skipped)
    } else {
        (&mut settings.folders, &mut settings.skipped)
    };
    let expanded = |list: &Vec<String>| -> Vec<String> {
        list.iter()
            .map(|path| config::expand(path).to_string_lossy().into_owned())
            .collect()
    };
    let mut selection = {
        let (chosen, skipped) = (expanded(chosen), expanded(skipped));
        let chosen: Vec<&str> = chosen.iter().map(String::as_str).collect();
        let skipped: Vec<&str> = skipped.iter().map(String::as_str).collect();
        digger_core::folder_selection::FolderSelection::new(&chosen, &skipped, &[])
    };
    for folder in folders {
        let folder = config::expand(&folder.to_string_lossy());
        if add && !folder.is_dir() {
            bail!("not a folder: {}", folder.display());
        }
        selection.set_folder(&folder.to_string_lossy(), add);
    }
    *chosen = selection
        .chosen()
        .iter()
        .map(|path| config::shorten(Path::new(path)))
        .collect();
    *skipped = selection
        .skipped()
        .iter()
        .map(|path| config::shorten(Path::new(path)))
        .collect();
    settings.save()?;
    println!("{}", config::Config::file().display());
    for folder in if code {
        &settings.code_folders
    } else {
        &settings.folders
    } {
        println!("  {folder}");
    }
    if let Some(mut client) = Client::connect() {
        client.command(&Request::Reload)?;
        println!("the daemon is reading them");
    }
    Ok(())
}

/// Waybar's custom module: `{"text", "tooltip", "class", "percentage"}`, once or (`follow`) each time it changes.
fn waybar(args: &IndexArgs, follow: bool) -> Result<()> {
    let _ = args;
    loop {
        match Client::connect() {
            Some(mut client) if follow => {
                let first = client.send(&Request::Watch);
                if let Ok(response) = first {
                    print_waybar(response.status.as_ref());
                    while let Ok(response) = client.read() {
                        print_waybar(response.status.as_ref());
                    }
                }
            }
            Some(mut client) => {
                let response = client.send(&Request::Status)?;
                print_waybar(response.status.as_ref());
                return Ok(());
            }
            None => print_waybar(None),
        }
        if !follow {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(5)); // the daemon is starting, or stopped
    }
}

fn print_waybar(status: Option<&protocol::Status>) {
    use std::io::Write;
    let icon = "\u{f0349}"; // nf-md-magnify
    let line = match status {
        None => serde_json::json!({"text": "", "tooltip": "Digger isn't running", "class": "offline"}),
        Some(status) => {
            let percent = (status.searchable * 100).checked_div(status.files).unwrap_or(100);
            let mut tooltip = format!("{} of {} files searchable", status.searchable, status.files);
            if status.code_files > 0 {
                tooltip += &format!("\ncode: {} files, {} waiting", status.code_files, status.code_pending);
            }
            if let Some(current) = &status.current {
                tooltip += &format!("\nreading {}", current.rsplit('/').next().unwrap_or(current));
            }
            if let Some(detail) = &status.detail {
                tooltip += &format!("\n{}: {detail}", status.state);
            }
            if !status.missing.is_empty() {
                tooltip += &format!("\nnot there now: {}", status.missing.join(", "));
            }
            if let Some(error) = &status.last_error {
                tooltip += &format!("\nlast problem: {error}");
            }
            let busy = matches!(status.state.as_str(), "syncing" | "indexing" | "finishing");
            let text = if busy && status.pending > 0 {
                format!("{icon} {percent}%")
            } else {
                icon.to_string()
            };
            serde_json::json!({"text": text, "tooltip": tooltip, "class": status.state, "alt": status.state, "percentage": percent})
        }
    };
    println!("{line}");
    let _ = std::io::stdout().flush();
}

#[derive(serde::Deserialize)]
struct EvalFile {
    base: String,
    queries: Vec<EvalQuery>,
}

#[derive(serde::Deserialize)]
struct EvalQuery {
    q: String,
    /// Paths relative to `base`; "path@6-12" also requires the hit to start in [6, 12] s (or lines, for code).
    expect: Vec<String>,
    tag: Option<String>,
    kind: Option<String>,
}

#[derive(Default)]
struct Row {
    n: usize,
    at1: usize,
    at5: usize,
    at10: usize,
    mrr: f64,
}

fn evaluate(file: &Path, args: &IndexArgs, tuning: &Tuning, depth: usize) -> Result<()> {
    let eval: EvalFile = serde_json::from_slice(&std::fs::read(absolute(file))?).context("not an eval file")?;
    let base = absolute(Path::new(&eval.base)).to_string_lossy().into_owned();
    // "code: …" queries search the code index (with its prompt), the others the main one.
    let is_code: Vec<bool> = eval
        .queries
        .iter()
        .map(|query| code_query::text(&query.q).is_some())
        .collect();
    let texts: Vec<String> = eval
        .queries
        .iter()
        .map(|query| code_query::text(&query.q).unwrap_or(query.q.clone()))
        .collect();
    let mut vectors: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
    let stores: Vec<(bool, IndexStore)> = is_code
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|code| Ok((code, IndexArgs { code, ..args.clone() }.store()?)))
        .collect::<Result<_>>()?;
    let mut searchers = Vec::new();
    for (code, store) in &stores {
        let searcher = Searcher::new(store, true)?;
        let indices: Vec<usize> = (0..texts.len()).filter(|index| is_code[*index] == *code).collect();
        let queries: Vec<String> = indices.iter().map(|index| texts[*index].clone()).collect();
        if let Some(embedded) = query_vectors(&queries, store, &searcher, *code)? {
            for (index, vector) in indices.into_iter().zip(embedded) {
                vectors[index] = Some(vector);
            }
        }
        searchers.push((*code, searcher));
    }

    let mut by_tag: BTreeMap<String, Row> = BTreeMap::new();
    let (mut shown, mut listed, mut folded) = (0, 0, 0);
    for (index, query) in eval.queries.iter().enumerate() {
        let searcher = &mut searchers
            .iter_mut()
            .find(|(code, _)| *code == is_code[index])
            .unwrap()
            .1;
        let options = SearchOptions {
            kinds: query
                .kind
                .as_ref()
                .and_then(|kind| kind.parse().ok())
                .map(|kind| HashSet::from([kind])),
            limit: depth,
            z_gate: tuning.zgate,
            word_weight: tuning.word_weight,
            prefix_last: false,
        };
        let hits = searcher.search(&texts[index], vectors[index].as_deref(), &options)?;
        let strong = strong_count(&hits, 0.5);
        shown += strong;
        listed += hits.len();
        let targets: Vec<(String, Option<(f64, f64)>)> = query
            .expect
            .iter()
            .map(|entry| {
                let (path, range) = entry.split_once('@').unwrap_or((entry, ""));
                let bounds: Vec<f64> = range.split('-').filter_map(|part| part.parse().ok()).collect();
                (
                    format!("{base}/{path}"),
                    (bounds.len() == 2).then(|| (bounds[0], bounds[1])),
                )
            })
            .collect();
        // A place to be at: a page or moment starts in the range; a stretch of code overlaps it.
        let at = |hit: &SearchHit, (low, high): (f64, f64)| {
            let Some(loc) = hit.loc else { return false };
            if matches!(hit.segment, SegmentKind::Lines | SegmentKind::Cells) {
                return loc <= high && hit.loc_end.unwrap_or(loc) >= low;
            }
            (low..=high).contains(&loc)
        };
        let rank = hits
            .iter()
            .position(|hit| {
                targets
                    .iter()
                    .any(|(path, range)| hit.path == *path && range.is_none_or(|range| at(hit, range)))
            })
            .map(|position| position + 1);
        if rank.is_some_and(|rank| rank > strong) {
            folded += 1;
        }
        let tag = query.tag.clone().unwrap_or_else(|| "all".into());
        for key in [tag.clone(), "ALL".into()].into_iter().collect::<HashSet<_>>() {
            let row = by_tag.entry(key).or_default();
            row.n += 1;
            if let Some(rank) = rank {
                row.at1 += (rank <= 1) as usize;
                row.at5 += (rank <= 5) as usize;
                row.at10 += (rank <= 10) as usize;
                row.mrr += 1.0 / rank as f64;
            }
        }
        let top = hits
            .first()
            .map(|hit| {
                let place = location(hit);
                let name = hit.path.rsplit('/').next().unwrap_or(&hit.path).to_string();
                if place.is_empty() {
                    name
                } else {
                    format!("{name} [{place}]")
                }
            })
            .unwrap_or_else(|| "-".into());
        let found = rank
            .map(|rank| format!("{:>4}", format!("#{rank}")))
            .unwrap_or_else(|| "miss".into());
        println!("{found}  {tag:<11} {:<42} top: {top}", format!("“{}”", query.q));
    }
    println!();
    println!(
        "{:<12} {:>3}  {:>5}  {:>5}  {:>5}  {:>5}",
        "tag", "n", "R@1", "R@5", "R@10", "MRR"
    );
    let mut rows: Vec<_> = by_tag.iter().collect();
    rows.sort_by_key(|(tag, _)| {
        if tag.as_str() == "ALL" {
            "~".to_string()
        } else {
            tag.to_string()
        }
    });
    for (tag, row) in rows {
        let n = row.n as f64;
        println!(
            "{tag:<12} {:>3}  {:5.2}  {:5.2}  {:5.2}  {:5.2}",
            row.n,
            row.at1 as f64 / n,
            row.at5 as f64 / n,
            row.at10 as f64 / n,
            row.mrr / n
        );
    }
    let count = eval.queries.len().max(1) as f64;
    println!(
        "weaker-matches fold (below 0.5× the top score): {:.1} of {:.1} results shown per query; {folded} expected {} folded",
        shown as f64 / count,
        listed as f64 / count,
        if folded == 1 { "answer" } else { "answers" }
    );
    Ok(())
}
