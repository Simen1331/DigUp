//! Pictures: a downscaled copy for the model (turned upright, as the camera held it), the date it was taken, the words
//! in a screenshot (Tesseract), and a tiny fingerprint that tells near-identical video frames apart.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use image::{DynamicImage, ImageDecoder, ImageReader, imageops::FilterType};

use crate::extract::ExtractError;
use crate::rules::extension;

/// A picture ready for the model.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub path: String,
    /// The original's size, in pixels.
    pub width: u32,
    pub height: u32,
    /// EXIF DateTimeOriginal, as written by the camera.
    pub taken: Option<String>,
}

fn unreadable(error: impl std::fmt::Display) -> ExtractError {
    ExtractError::Unreadable(error.to_string())
}

/// RAW photos aren't decoded (yet): their previews would need a RAW library.
const RAW_EXTENSIONS: &[&str] = &["dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf", "srw", "pef"];

/// The size of the picture in `path`, from its header (nothing is decoded).
pub fn dimensions(path: &Path) -> Result<(u32, u32), ExtractError> {
    if is_heif(path) {
        return heif::dimensions(path);
    }
    ImageReader::open(path)
        .map_err(unreadable)?
        .with_guessed_format()
        .map_err(unreadable)?
        .into_dimensions()
        .map_err(unreadable)
}

fn is_heif(path: &Path) -> bool {
    matches!(extension(path).as_str(), "heic" | "heif")
}

/// The whole picture, decoded and turned upright.
pub fn decode(path: &Path) -> Result<DynamicImage, ExtractError> {
    let ext = extension(path);
    if RAW_EXTENSIONS.contains(&ext.as_str()) {
        return Err(ExtractError::LeftOut("RAW photos aren't read yet".into()));
    }
    if is_heif(path) {
        return heif::decode(path); // libheif turns it upright itself
    }
    let mut decoder = ImageReader::open(path)
        .map_err(unreadable)?
        .with_guessed_format()
        .map_err(unreadable)?
        .into_decoder()
        .map_err(unreadable)?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder).map_err(unreadable)?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    Ok(image)
}

/// The camera's DateTimeOriginal, when the file has one.
pub fn taken(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let exif = exif::Reader::new()
        .read_from_container(&mut BufReader::new(file))
        .ok()?;
    let field = exif.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)?;
    match &field.value {
        exif::Value::Ascii(parts) => parts
            .first()
            .map(|bytes| String::from_utf8_lossy(bytes).trim().to_string()),
        _ => None,
    }
}

/// Writes a copy for the model, its longest side at most `max_pixel`, as PNG (screenshots: sharp text) or JPEG.
/// Pictures whose longest side is under `min_side` are left out: icons and thumbnails, not photos.
pub fn prepare(path: &Path, max_pixel: u32, min_side: u32, out: &Path, png: bool) -> Result<Prepared, ExtractError> {
    let (width, height) = dimensions(path)?;
    if width.max(height) < min_side {
        return Err(ExtractError::TooSmall(width, height));
    }
    let image = decode(path)?;
    let image = fit(image, max_pixel);
    write(&image, out, png)?;
    Ok(Prepared {
        path: out.to_string_lossy().into_owned(),
        width,
        height,
        taken: taken(path),
    })
}

/// `image` scaled down (never up) to fit a square of `side`.
pub fn fit(image: DynamicImage, side: u32) -> DynamicImage {
    if image.width().max(image.height()) <= side {
        return image;
    }
    image.resize(side, side, FilterType::Triangle)
}

pub fn write(image: &DynamicImage, out: &Path, png: bool) -> Result<(), ExtractError> {
    if png {
        image.save_with_format(out, image::ImageFormat::Png).map_err(unreadable)
    } else {
        let file = fs::File::create(out).map_err(unreadable)?;
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 90);
        image.to_rgb8().write_with_encoder(encoder).map_err(unreadable)
    }
}

/// An 8×8 average hash: one bit per cell, set where it's brighter than the average. Two frames of the same shot
/// differ in a few bits at most.
pub fn average_hash(image: &DynamicImage) -> u64 {
    let small = image.resize_exact(8, 8, FilterType::Triangle).to_luma8();
    let pixels = small.as_raw();
    let mean = pixels.iter().map(|pixel| *pixel as u32).sum::<u32>() / pixels.len() as u32;
    pixels
        .iter()
        .enumerate()
        .filter(|(_, pixel)| **pixel as u32 > mean)
        .fold(0, |hash, (index, _)| hash | 1 << index)
}

/// The words in a picture, by Tesseract: what keyword search matches codes, names and error messages in screenshots
/// against. "" when Tesseract isn't installed or reads nothing.
pub mod ocr {
    use super::*;

    /// The languages Tesseract reads with, in this order when they're installed: `$DIGGER_OCR_LANGS` ("eng+nor"), or
    /// English, Norwegian and the other languages the evals cover. More languages are slower.
    const PREFERRED: &[&str] = &["eng", "nor", "dan", "swe", "deu", "fra", "spa", "ben", "ara"];

    /// The installed languages Tesseract reads with ("eng+nor"), or None when it isn't installed.
    pub fn languages() -> Option<&'static str> {
        static LANGUAGES: OnceLock<Option<String>> = OnceLock::new();
        LANGUAGES
            .get_or_init(|| {
                let output = Command::new("tesseract")
                    .arg("--list-langs")
                    .stderr(Stdio::null())
                    .output()
                    .ok()?;
                let installed: Vec<String> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .skip(1)
                    .map(|line| line.trim().to_string())
                    .collect();
                if let Some(chosen) = std::env::var("DIGGER_OCR_LANGS").ok().filter(|value| !value.is_empty()) {
                    return Some(chosen);
                }
                let found: Vec<&str> = PREFERRED
                    .iter()
                    .copied()
                    .filter(|language| installed.iter().any(|have| have == language))
                    .collect();
                if found.is_empty() {
                    installed.into_iter().find(|language| language != "osd")
                } else {
                    Some(found.join("+"))
                }
            })
            .as_deref()
    }

    /// Whether Tesseract can read `language` ("ara").
    pub fn reads(language: &str) -> bool {
        languages().is_some_and(|languages| languages.split('+').any(|have| have == language))
    }

    /// Reads the picture in `path` at full size: small interface text is what screenshots are full of. Pictures
    /// Tesseract can't open itself (HEIC, AVIF) go through a PNG in `scratch`.
    pub fn text(path: &Path, scratch: &Path) -> String {
        let Some(languages) = languages() else {
            return String::new();
        };
        let readable = matches!(
            extension(path).as_str(),
            "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" | "gif" | "webp"
        );
        let mut converted: Option<PathBuf> = None;
        if !readable {
            let Ok(image) = decode(path) else { return String::new() };
            let out = scratch.join(format!("ocr-{}.png", std::process::id()));
            if write(&image, &out, true).is_err() {
                return String::new();
            }
            converted = Some(out);
        }
        let text = run(converted.as_deref().unwrap_or(path), languages);
        if let Some(converted) = converted {
            let _ = fs::remove_file(converted);
        }
        text
    }

    /// Reads an image already decoded (a rendered PDF page).
    pub fn image_text(image: &DynamicImage, scratch: &Path, name: &str) -> String {
        let Some(languages) = languages() else {
            return String::new();
        };
        let out = scratch.join(format!("{name}.png"));
        if write(image, &out, true).is_err() {
            return String::new();
        }
        let text = run(&out, languages);
        let _ = fs::remove_file(out);
        text
    }

    fn run(path: &Path, languages: &str) -> String {
        let output = Command::new("tesseract")
            .arg(path)
            .arg("stdout")
            .args(["-l", languages])
            .env("OMP_THREAD_LIMIT", "1") // one picture at a time: its own threads only fight the model's
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        let Ok(output) = output else { return String::new() };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| line.chars().any(char::is_alphanumeric)) // stray marks read from lines and icons
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(feature = "heif")]
mod heif {
    use super::*;
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

    fn context(path: &Path) -> Result<HeifContext<'static>, ExtractError> {
        HeifContext::read_from_file(&path.to_string_lossy()).map_err(unreadable)
    }

    pub fn dimensions(path: &Path) -> Result<(u32, u32), ExtractError> {
        let handle = context(path)?.primary_image_handle().map_err(unreadable)?;
        Ok((handle.width(), handle.height()))
    }

    pub fn decode(path: &Path) -> Result<DynamicImage, ExtractError> {
        let handle = context(path)?.primary_image_handle().map_err(unreadable)?;
        let decoded = LibHeif::new()
            .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)
            .map_err(unreadable)?;
        let plane = decoded.planes().interleaved.ok_or_else(|| unreadable("no pixels"))?;
        let (width, height) = (plane.width, plane.height);
        let mut pixels = Vec::with_capacity((width * height * 3) as usize);
        for row in plane.data.chunks(plane.stride).take(height as usize) {
            pixels.extend_from_slice(&row[..(width * 3) as usize]);
        }
        image::RgbImage::from_raw(width, height, pixels)
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(|| unreadable("bad HEIC pixels"))
    }
}

#[cfg(not(feature = "heif"))]
mod heif {
    use super::*;

    pub fn dimensions(_: &Path) -> Result<(u32, u32), ExtractError> {
        Err(ExtractError::LeftOut(
            "this Digger was built without HEIC support".into(),
        ))
    }

    pub fn decode(_: &Path) -> Result<DynamicImage, ExtractError> {
        Err(ExtractError::LeftOut(
            "this Digger was built without HEIC support".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    #[test]
    fn copies_are_smaller_and_small_pictures_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("photo.jpg");
        DynamicImage::ImageRgb8(RgbImage::from_fn(2000, 1000, |x, _| Rgb([(x % 256) as u8, 90, 160])))
            .save(&photo)
            .unwrap();
        let out = dir.path().join("copy.jpg");
        let prepared = prepare(&photo, 1024, 256, &out, false).unwrap();
        assert_eq!((prepared.width, prepared.height), (2000, 1000));
        assert_eq!(image::image_dimensions(&out).unwrap(), (1024, 512));
        let icon = dir.path().join("icon.png");
        DynamicImage::ImageRgb8(RgbImage::new(64, 64)).save(&icon).unwrap();
        assert_eq!(
            prepare(&icon, 1024, 256, &out, true).unwrap_err(),
            ExtractError::TooSmall(64, 64)
        );
        assert!(matches!(
            decode(Path::new("/x/photo.NEF")),
            Err(ExtractError::LeftOut(_))
        ));
    }

    #[test]
    fn near_identical_frames_hash_alike() {
        let gradient = |shift: u32| {
            DynamicImage::ImageRgb8(RgbImage::from_fn(320, 240, |x, y| {
                Rgb([((x + shift) % 256) as u8, (y % 256) as u8, 40])
            }))
        };
        let (a, b) = (average_hash(&gradient(0)), average_hash(&gradient(2)));
        assert!((a ^ b).count_ones() <= 4);
        let other = DynamicImage::ImageRgb8(RgbImage::from_fn(320, 240, |x, y| {
            Rgb([((y * 3) % 256) as u8, (x % 256) as u8, 200])
        }));
        assert!((a ^ average_hash(&other)).count_ones() > 4);
    }

    #[test]
    fn tesseract_reads_a_screenshot() {
        if ocr::languages().is_none() || crate::pdf::pdfium().is_none() {
            eprintln!("skipped: no tesseract or pdfium");
            return;
        }
        // A page of text rendered as a picture: what a screenshot of an error message is.
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("error.pdf");
        crate::pdf::write_test_pdf(
            &pdf,
            &[vec!["Payment declined", "Error code K7Q2LM: the card has expired."]],
        );
        let page = crate::pdf::pages(&pdf, |total| 0..total, 10_000, 2000, dir.path())
            .unwrap()
            .pages
            .remove(0);
        let text = ocr::text(Path::new(page.image_path.as_ref().unwrap()), dir.path());
        assert!(text.contains("Payment declined"), "{text}");
        assert!(text.contains("K7Q2LM"), "{text}");
    }
}
