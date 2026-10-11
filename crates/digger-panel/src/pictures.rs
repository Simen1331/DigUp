//! Pictures for rows and previews: a photo scaled down, a PDF page drawn (its words marked), a video's frame at the
//! moment. Made off the main thread and kept for as long as the panel is shown.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use digger_core::model::FileKind;
use digger_core::{images, pdf};
use gtk::{gdk, gio, glib};
use image::{DynamicImage, RgbaImage};

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

/// Forgets the pictures (the panel was hidden).
pub fn clear() {
    CACHE.with(|cache| cache.borrow_mut().clear());
}

/// Calls `done` with the picture (None: there's none to make) on the main loop: at once when it's made already.
pub fn load(key: Key, done: impl FnOnce(Option<gdk::Texture>) + 'static) {
    if !key.drawable() {
        return done(None);
    }
    if let Some(known) = CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return done(known);
    }
    glib::spawn_future_local(async move {
        let job = key.clone();
        let picture = gio::spawn_blocking(move || draw(&job)).await.ok().flatten();
        let texture = picture.as_ref().map(texture);
        CACHE.with(|cache| cache.borrow_mut().insert(key, texture.clone()));
        done(texture);
    });
}
