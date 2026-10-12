//! Pictures for rows and previews: a photo scaled down, a PDF page drawn (its words marked), a video's frame at the
//! moment. Made off the main thread and kept for as long as the panel is shown.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex};

use gtk::{gdk, glib};
use image::{DynamicImage, RgbaImage};
use showme_core::model::FileKind;
use showme_core::{images, pdf};

use crate::results::ResultRow;

/// What to draw, and how big (the longest side, in pixels).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub path: String,
    pub kind: FileKind,
    /// The page or the moment (as a whole number of tenths of a second).
    pub at: i64,
    pub size: u32,
    /// Words to mark (PDF pages).
    pub words: Vec<String>,
}

impl Key {
    pub fn new(row: &ResultRow, size: u32, marks: bool) -> Key {
        let at = match row.kind {
            FileKind::Pdf => row.page().unwrap_or(1) as i64,
            FileKind::Video => (row.loc.unwrap_or(0.0) * 10.0) as i64,
            _ => 0,
        };
        let words =
            if marks { row.found_words().into_iter().chain(row.terms.iter().cloned()).collect() } else { Vec::new() };
        Key { path: row.path.clone(), kind: row.kind, at, size, words }
    }

    /// Whether there's a picture to make at all (recordings, documents and code show an icon).
    pub fn drawable(&self) -> bool {
        matches!(self.kind, FileKind::Image | FileKind::Screenshot | FileKind::Pdf | FileKind::Video)
    }
}

/// Makes the picture (any thread).
fn draw(key: &Key) -> Option<RgbaImage> {
    let path = Path::new(&key.path);
    let picture: DynamicImage = match key.kind {
        FileKind::Image | FileKind::Screenshot => images::fit(images::decode(path).ok()?, key.size),
        FileKind::Pdf => {
            let (page, mark) = pdf::render_page(path, key.at.max(1) as u32, key.size, &key.words).ok()?;
            // With words marked: the part of the page around the first mark (its full width, 2:3), as DigUp shows it.
            match mark {
                Some((_, y, _, _)) if !key.words.is_empty() => {
                    let height = (page.width() * 2 / 3).min(page.height());
                    let top = y.saturating_sub(height / 3).min(page.height() - height);
                    page.crop_imm(0, top, page.width(), height)
                }
                _ => page,
            }
        }
        FileKind::Video => {
            let seconds = key.at as f64 / 10.0;
            let output = Command::new("ffmpeg")
                .args(["-v", "error", "-nostdin", "-ss", &format!("{seconds:.1}"), "-i"])
                .arg(path)
                .args(["-frames:v", "1", "-vf"])
                .arg(format!("scale='min({0},iw)':'min({0},ih)':force_original_aspect_ratio=decrease", key.size))
                .args(["-f", "image2pipe", "-vcodec", "png", "-"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            image::load_from_memory(&output.stdout).ok()?
        }
        _ => return None,
    };
    Some(picture.to_rgba8())
}

pub fn texture(picture: &RgbaImage) -> gdk::Texture {
    let (width, height) = picture.dimensions();
    let bytes = glib::Bytes::from(picture.as_raw());
    gdk::MemoryTexture::new(width as i32, height as i32, gdk::MemoryFormat::R8g8b8a8, &bytes, width as usize * 4).into()
}

thread_local! {
    static CACHE: RefCell<HashMap<Key, Option<gdk::Texture>>> = RefCell::new(HashMap::new());
}

/// Forgets the pictures (the panel was hidden), and drops the ones still waiting to be made.
pub fn clear() {
    CACHE.with(|cache| cache.borrow_mut().clear());
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Pictures are made by a few threads, newest first. A picture is made from the whole file (a 4K screenshot or a
/// 12 MP photo is 30–200 MB while it's decoded), so GLib's pool, which runs dozens at once, took the panel past 10 GB
/// as results came in.
const WORKERS: usize = 3;

struct Job {
    key: Key,
    generation: u64,
    reply: async_channel::Sender<Option<RgbaImage>>,
}

/// Bumped by `clear`: jobs from before it are dropped.
static GENERATION: AtomicU64 = AtomicU64::new(0);

static QUEUE: LazyLock<(Mutex<Vec<Job>>, Condvar)> = LazyLock::new(|| {
    for number in 0..WORKERS {
        std::thread::Builder::new().name(format!("pictures-{number}")).spawn(work).expect("can't start a thread");
    }
    (Mutex::new(Vec::new()), Condvar::new())
});

fn work() {
    let (jobs, ready) = &*QUEUE;
    loop {
        let job = {
            let mut waiting = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                match waiting.pop() {
                    Some(job) => break job,
                    None => waiting = ready.wait(waiting).unwrap_or_else(|poisoned| poisoned.into_inner()),
                }
            }
        };
        if job.generation == GENERATION.load(Ordering::Relaxed) {
            let _ = job.reply.send_blocking(draw(&job.key));
        }
        if jobs.lock().is_ok_and(|waiting| waiting.is_empty()) {
            // SAFETY: malloc_trim only hands free memory back to the system.
            unsafe { libc::malloc_trim(0) };
        }
    }
}

/// Calls `done` with the picture (None: there's none to make) on the main loop: at once when it's made already.
pub fn load(key: Key, done: impl FnOnce(Option<gdk::Texture>) + 'static) {
    if !key.drawable() {
        return done(None);
    }
    if let Some(known) = CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return done(known);
    }
    let (reply, picture) = async_channel::bounded(1);
    let (jobs, ready) = &*QUEUE;
    let job = Job { key: key.clone(), generation: GENERATION.load(Ordering::Relaxed), reply };
    jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(job);
    ready.notify_one();
    glib::spawn_future_local(async move {
        // A dropped job (the panel hid) leaves nothing to show.
        let Ok(picture) = picture.recv().await else { return };
        let texture = picture.as_ref().map(texture);
        CACHE.with(|cache| cache.borrow_mut().insert(key, texture.clone()));
        done(texture);
    });
}
