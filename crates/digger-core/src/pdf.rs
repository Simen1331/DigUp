//! PDFs, through pdfium (Chrome's PDF library), loaded at run time from `$DIGGER_PDFIUM`, Digger's data folder
//! (`digger download` puts it there) or the system.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pdfium_render::prelude::*;

use crate::extract::ExtractError;
use crate::images;
use crate::paths;
use crate::search_text::PageTextProblem;

/// The pdfium build pdfium-render 0.9 speaks to (bblanchon/pdfium-binaries' chromium/7881).
pub const PDFIUM_RELEASE: &str = "chromium/7881";
pub const PDFIUM_ARCHIVE: &str = "pdfium-linux-x64.tgz";
pub const PDFIUM_SHA256: &str = "1470e21b8b4a3b4ad7f85684e2da11d94f3b69a86d81dee11b9b6709d927ac1d";

/// Where `digger download` puts libpdfium.so.
pub fn installed_library() -> PathBuf {
    paths::data_folder().join("lib").join("libpdfium.so")
}

/// pdfium, loaded once per process; None when it isn't there (PDFs then wait in the queue).
pub fn pdfium() -> Option<&'static Pdfium> {
    static PDFIUM: OnceLock<Option<Pdfium>> = OnceLock::new();
    PDFIUM
        .get_or_init(|| {
            let chosen = std::env::var_os("DIGGER_PDFIUM").map(PathBuf::from);
            let bindings = chosen
                .into_iter()
                .chain([installed_library()])
                .filter(|path| path.exists())
                .find_map(|path| Pdfium::bind_to_library(path).ok())
                .or_else(|| Pdfium::bind_to_system_library().ok())?;
            Some(Pdfium::new(bindings))
        })
        .as_ref()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// From 1.
    pub number: usize,
    /// May be empty for scans.
    pub text: String,
    /// Set when the page had too little text and was rendered for the vision encoder.
    pub image_path: Option<String>,
    /// The text came from OCR: the text layer was garbled (`PageTextProblem::LostLetters`).
    pub ocr: bool,
    /// Text read in glyph order (`PageTextProblem::GlyphOrder`): too garbled to show or match words against, but
    /// enough of its words survive for a meaning vector beside the page's picture.
    pub garbled: Option<String>,
}

pub struct Read {
    pub pages: Vec<Page>,
    pub total: usize,
    pub title: Option<String>,
}

fn unreadable(error: impl std::fmt::Display) -> ExtractError {
    ExtractError::Unreadable(error.to_string())
}

/// Reads the pages in `range(page_count)` (from 0). Pages with real text become text; scans and slides are rendered
/// (`render_pixels` square). Text layers that come out garbled are replaced: by OCR where letters were lost (Arabic
/// ligatures), by the rendered page where the words came out in glyph order (Bengali, which OCR reads poorly).
pub fn pages(
    path: &Path,
    range: impl FnOnce(usize) -> std::ops::Range<usize>,
    min_text: usize,
    render_pixels: u32,
    folder: &Path,
) -> Result<Read, ExtractError> {
    let pdfium = pdfium().ok_or_else(|| ExtractError::Unreadable("pdfium isn't installed (digger download)".into()))?;
    let document = pdfium.load_pdf_from_file(path, None).map_err(|error| match error {
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => ExtractError::Locked,
        error => unreadable(format!("can't open PDF ({error})")),
    })?;
    let total = document.pages().len() as usize;
    let wanted = range(total);
    let wanted = wanted.start.min(total)..wanted.end.min(total);
    let mut pages = Vec::new();
    for index in wanted {
        let Ok(page) = document.pages().get(index as i32) else {
            continue;
        };
        let mut text = page
            .text()
            .map(|text| text.all())
            .unwrap_or_default()
            .trim()
            .to_string();
        let mut garbled = None;
        match PageTextProblem::check(&text) {
            Some(PageTextProblem::LostLetters) => {
                // OCR's reading replaces the text layer only when it's real Arabic; otherwise the layer stays.
                if images::ocr::reads("ara")
                    && let Some(image) = render(&page, 2000)
                {
                    let read = images::ocr::image_text(&image, folder, &format!("page-{index}-ocr"));
                    let arabic = read.chars().any(|c| (0x0600..0x0700).contains(&(c as u32)));
                    if read.chars().count() >= min_text && PageTextProblem::check(&read).is_none() && arabic {
                        pages.push(Page {
                            number: index + 1,
                            text: read,
                            image_path: None,
                            ocr: true,
                            garbled: None,
                        });
                        continue;
                    }
                }
                if text.chars().count() >= min_text {
                    pages.push(Page {
                        number: index + 1,
                        text,
                        image_path: None,
                        ocr: false,
                        garbled: None,
                    });
                    continue;
                }
            }
            Some(PageTextProblem::GlyphOrder) => garbled = Some(std::mem::take(&mut text)),
            None => {
                if text.chars().count() >= min_text {
                    pages.push(Page {
                        number: index + 1,
                        text,
                        image_path: None,
                        ocr: false,
                        garbled: None,
                    });
                    continue;
                }
            }
        }
        let Some(image) = render(&page, render_pixels) else {
            continue;
        };
        let out = folder.join(format!("page-{index}.png"));
        images::write(&image, &out, true)?;
        pages.push(Page {
            number: index + 1,
            text,
            image_path: Some(out.to_string_lossy().into_owned()),
            ocr: false,
            garbled,
        });
    }
    let title = document
        .metadata()
        .get(PdfDocumentMetadataTagType::Title)
        .map(|tag| tag.value().trim().to_string())
        .filter(|title| !title.is_empty());
    Ok(Read { pages, total, title })
}

/// The page as a picture, fitted into a square of `pixels`, on white.
fn render(page: &PdfPage, pixels: u32) -> Option<image::DynamicImage> {
    let (width, height) = (page.width().value, page.height().value);
    let scale = pixels as f32 / width.max(height).max(1.0);
    let config = PdfRenderConfig::new()
        .set_target_size((width * scale).round() as Pixels, (height * scale).round() as Pixels)
        .render_form_data(true);
    let image = page.render_with_config(&config).ok()?.as_image().ok()?;
    Some(image::DynamicImage::ImageRgb8(image.to_rgb8()))
}

/// Every page's text, for tests and the preview.
pub fn texts(path: &Path) -> Result<Vec<String>, ExtractError> {
    let pdfium = pdfium().ok_or_else(|| ExtractError::Unreadable("pdfium isn't installed".into()))?;
    let document = pdfium.load_pdf_from_file(path, None).map_err(unreadable)?;
    Ok(document
        .pages()
        .iter()
        .map(|page| page.text().map(|text| text.all()).unwrap_or_default())
        .collect())
}

/// A small PDF of `pages` (each a list of lines; an empty page is blank), in Helvetica, for tests.
pub fn write_test_pdf(path: &Path, pages: &[Vec<&str>]) {
    let mut objects: Vec<String> = Vec::new();
    let count = pages.len();
    // 1: catalog, 2: pages, 3: font, then a page and its content for each page.
    let kids: Vec<String> = (0..count).map(|index| format!("{} 0 R", 4 + index * 2)).collect();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
    objects.push(format!("<< /Type /Pages /Kids [{}] /Count {count} >>", kids.join(" ")));
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
    for (index, lines) in pages.iter().enumerate() {
        let mut stream = String::from("BT /F1 12 Tf 50 750 Td 14 TL\n");
        for line in lines {
            stream += &format!(
                "({}) Tj T*\n",
                line.replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)")
            );
        }
        stream += "ET";
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + index * 2
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()));
    }
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out += &format!("{} 0 obj\n{object}\nendobj\n", index + 1);
    }
    let xref = out.len();
    out += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for offset in offsets {
        out += &format!("{offset:010} 00000 n \n");
    }
    out += &format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    );
    std::fs::write(path, out).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_with_text_and_a_blank_one_rendered() {
        if pdfium().is_none() {
            eprintln!("skipped: no pdfium");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("lease.pdf");
        let clause = "Pets: one cat is allowed in the flat, but no dogs, birds or reptiles of any kind.";
        write_test_pdf(&pdf, &[vec!["Lease agreement", clause, clause, clause], vec![]]);
        let read = pages(&pdf, |total| 0..total, 200, 1400, dir.path()).unwrap();
        assert_eq!(read.total, 2);
        assert_eq!(read.pages.len(), 2);
        assert!(
            read.pages[0].text.contains("one cat is allowed"),
            "{:?}",
            read.pages[0].text
        );
        assert!(read.pages[0].image_path.is_none());
        let rendered = read.pages[1].image_path.as_ref().unwrap();
        let (width, height) = image::image_dimensions(rendered).unwrap();
        assert_eq!(height, 1400);
        assert!(width < height);
        // Only the pages asked for.
        let first = pages(&pdf, |_| 1..5, 200, 700, dir.path()).unwrap();
        assert_eq!(first.pages.iter().map(|page| page.number).collect::<Vec<_>>(), [2]);
    }
}
