//! Reading files: what can go wrong, and the text of documents (txt, md, html, docx, odt, rtf, doc).

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use regex::Regex;

use crate::rules;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractError {
    Unreadable(String),
    TooSmall(u32, u32),
    Locked,
    Empty(String),
    /// Not for the index after all, once read (code that a program made, or that holds a private key).
    LeftOut(String),
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExtractError::Unreadable(why) => write!(f, "unreadable: {why}"),
            ExtractError::TooSmall(width, height) => write!(f, "small image {width}×{height}"),
            ExtractError::Locked => f.write_str("password-protected"),
            ExtractError::Empty(why) | ExtractError::LeftOut(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ExtractError {}

fn unreadable(error: impl fmt::Display) -> ExtractError {
    ExtractError::Unreadable(error.to_string())
}

/// Plain text from txt/md directly, HTML by stripping tags, docx and odt from their XML, rtf by dropping its control
/// words, and old Word files through antiword or catdoc when one is installed.
pub fn doc_text(path: &Path) -> Result<String, ExtractError> {
    match rules::extension(path).as_str() {
        "html" | "htm" => Ok(plain_text_from_html(&read_text(path)?)),
        "docx" => office_text(path, "word/document.xml", "w:p"),
        "odt" => office_text(path, "content.xml", "text:p"),
        "rtf" => Ok(plain_text_from_rtf(&read_text(path)?)),
        "doc" => word97_text(path),
        _ => read_text(path),
    }
}

/// UTF-8 (with or without a byte order mark), UTF-16 with one, else ISO Latin 1.
fn read_text(path: &Path) -> Result<String, ExtractError> {
    let data = fs::read(path).map_err(unreadable)?;
    if let Some(rest) = data.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Ok(String::from_utf8_lossy(rest).into_owned());
    }
    let utf16 = |bytes: &[u8], little: bool| {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| if little { u16::from_le_bytes(*pair) } else { u16::from_be_bytes(*pair) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = data.strip_prefix(&[0xFF, 0xFE]) {
        return Ok(utf16(rest, true));
    }
    if let Some(rest) = data.strip_prefix(&[0xFE, 0xFF]) {
        return Ok(utf16(rest, false));
    }
    match String::from_utf8(data) {
        Ok(text) => Ok(text),
        Err(error) => Ok(error.into_bytes().iter().map(|byte| *byte as char).collect()),
    }
}

/// The text of an Office Open XML or OpenDocument file: its paragraphs (`paragraph` elements), one a line.
fn office_text(path: &Path, part: &str, paragraph: &str) -> Result<String, ExtractError> {
    let file = fs::File::open(path).map_err(unreadable)?;
    let mut archive = zip::ZipArchive::new(file).map_err(unreadable)?;
    let mut xml = String::new();
    archive.by_name(part).map_err(unreadable)?.read_to_string(&mut xml).map_err(unreadable)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    let (mut text, mut depth) = (String::new(), 0usize);
    loop {
        match reader.read_event().map_err(unreadable)? {
            quick_xml::events::Event::Start(element) => {
                if element.name().as_ref() == paragraph {
                    depth += 1;
                }
            }
            quick_xml::events::Event::End(element) => {
                if element.name().as_ref() == paragraph {
                    depth = depth.saturating_sub(1);
                    text.push('\n');
                }
            }
            quick_xml::events::Event::Empty(element) => match element.name().as_ref() {
                "w:tab" | "text:tab" => text.push('\t'),
                "w:br" | "text:line-break" => text.push('\n'),
                "text:s" => text.push(' '),
                _ => {}
            },
            quick_xml::events::Event::Text(chunk) => {
                if depth > 0 {
                    text.push_str(&chunk.xml_content(quick_xml::XmlVersion::Implicit1_0));
                }
            }
            quick_xml::events::Event::GeneralRef(reference) => {
                if depth > 0 {
                    text.push_str(&decode_entity(&reference));
                }
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    Ok(text.trim().to_string())
}

fn decode_entity(name: &str) -> String {
    match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        _ => {
            let code = name
                .strip_prefix("#x")
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|decimal| decimal.parse().ok()));
            code.and_then(char::from_u32).map(String::from).unwrap_or_default()
        }
    }
}

/// Old Word files: antiword, then catdoc, whichever is installed.
fn word97_text(path: &Path) -> Result<String, ExtractError> {
    for (tool, arguments) in [("antiword", vec!["-w", "0"]), ("catdoc", vec!["-w"])] {
        let output = Command::new(tool).args(arguments).arg(path).stdin(Stdio::null()).stderr(Stdio::null()).output();
        if let Ok(output) = output
            && output.status.success()
        {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
    }
    Err(ExtractError::Unreadable("a .doc needs antiword or catdoc".into()))
}

/// Good enough for search: drops groups that hold no text (fonts, colours, pictures), control words and braces, and
/// decodes \'hh and \uN characters.
pub fn plain_text_from_rtf(rtf: &str) -> String {
    const SKIPPED: &[&str] =
        &["fonttbl", "colortbl", "stylesheet", "info", "pict", "header", "footer", "listtable", "generator", "*"];
    let chars: Vec<char> = rtf.chars().collect();
    let (mut out, mut i) = (String::new(), 0);
    let mut skip_depth: Option<usize> = None;
    let mut depth = 0usize;
    let mut skip_after_unicode = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '{' => {
                depth += 1;
                i += 1;
            }
            '}' => {
                if skip_depth == Some(depth) {
                    skip_depth = None;
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            '\\' => {
                i += 1;
                let Some(&next) = chars.get(i) else { break };
                if next == '\'' {
                    let hex: String = chars.iter().skip(i + 1).take(2).collect();
                    if skip_depth.is_none()
                        && let Ok(byte) = u8::from_str_radix(&hex, 16)
                    {
                        if skip_after_unicode > 0 {
                            skip_after_unicode -= 1;
                        } else {
                            out.push(byte as char); // Windows-1252 ≈ Latin 1
                        }
                    }
                    i += 3;
                } else if next.is_ascii_alphabetic() {
                    let start = i;
                    while i < chars.len() && chars[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    let word: String = chars[start..i].iter().collect();
                    let number_start = i;
                    if i < chars.len() && (chars[i] == '-' || chars[i].is_ascii_digit()) {
                        i += 1;
                        while i < chars.len() && chars[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                    let number: String = chars[number_start..i].iter().collect();
                    if i < chars.len() && chars[i] == ' ' {
                        i += 1;
                    }
                    if skip_depth.is_some() {
                        continue;
                    }
                    match word.as_str() {
                        "par" | "line" | "row" | "sect" | "page" => out.push('\n'),
                        "tab" | "cell" => out.push('\t'),
                        "u" => {
                            if let Ok(code) = number.parse::<i32>() {
                                let code = if code < 0 { code + 65536 } else { code };
                                out.extend(char::from_u32(code as u32));
                                skip_after_unicode = 1;
                            }
                        }
                        _ if SKIPPED.contains(&word.as_str()) => skip_depth = Some(depth),
                        _ => {}
                    }
                } else {
                    if next == '*' {
                        skip_depth = Some(depth);
                    } else if skip_depth.is_none() && "\\{}".contains(next) {
                        out.push(next);
                    }
                    i += 1;
                }
            }
            '\r' | '\n' => i += 1,
            _ => {
                if skip_depth.is_none() {
                    if skip_after_unicode > 0 {
                        skip_after_unicode -= 1;
                    } else {
                        out.push(c);
                    }
                }
                i += 1;
            }
        }
    }
    out.trim().to_string()
}

/// Good enough for search: drops scripts, styles and comments, turns block ends into line breaks, strips the remaining
/// tags, and decodes common entities.
pub fn plain_text_from_html(html: &str) -> String {
    static DROPPED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [
            r"(?is)<script\b.*?</script\s*>",
            r"(?is)<style\b.*?</style\s*>",
            r"(?s)<!--.*?-->",
            r"(?is)<head\b.*?</head\s*>",
        ]
        .iter()
        .map(|pattern| Regex::new(pattern).unwrap())
        .collect()
    });
    static BLOCK_END: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)<(br|/p|/div|/li|/h[1-6]|/tr|/section|/article)\b[^>]*>").unwrap());
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
    static NUMERIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#(x[0-9a-fA-F]+|[0-9]+);").unwrap());
    static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").unwrap());
    static LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\n\s*").unwrap());
    let mut text = html.to_string();
    for pattern in DROPPED.iter() {
        text = pattern.replace_all(&text, " ").into_owned();
    }
    text = BLOCK_END.replace_all(&text, "\n").into_owned();
    text = TAG.replace_all(&text, " ").into_owned();
    for (entity, character) in [
        ("&nbsp;", " "),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&mdash;", "—"),
        ("&ndash;", "–"),
        ("&hellip;", "…"),
    ] {
        text = text.replace(entity, character);
    }
    text = NUMERIC
        .replace_all(&text, |captures: &regex::Captures| {
            let code = &captures[1];
            let value = match code.strip_prefix('x') {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => code.parse().ok(),
            };
            value.and_then(char::from_u32).map(String::from).unwrap_or_else(|| " ".into())
        })
        .into_owned();
    text = SPACES.replace_all(&text, " ").into_owned();
    LINES.replace_all(&text, "\n").trim().to_string()
}

/// Splits text into ~`size`-character pieces (~450 tokens) that overlap a little, breaking at whitespace.
pub fn chunks(text: &str, size: usize, overlap: usize, limit: usize) -> Vec<String> {
    static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\u{00A0}]+").unwrap());
    static BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n\s*\n+").unwrap());
    let clean = SPACES.replace_all(text, " ");
    let clean = BLANK_LINES.replace_all(&clean, "\n\n");
    let clean: Vec<char> = clean.trim().chars().collect();
    let n = clean.len();
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < n && chunks.len() < limit {
        let mut end = (start + size).min(n);
        if end < n
            && let Some(space) = (start..end).rev().find(|i| clean[*i].is_whitespace())
            && space - start > size / 2
        {
            end = space;
        }
        let chunk: String = clean[start..end].iter().collect::<String>().trim().to_string();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        if end >= n {
            break;
        }
        let mut next = if end >= start + overlap { end - overlap } else { end };
        if next <= start {
            next = end;
        }
        // Start the next chunk on a word boundary.
        while next < end && !clean[next].is_whitespace() {
            next += 1;
        }
        start = if next < end { next } else { end };
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_overlap_and_respect_words() {
        let text: Vec<String> = (1..=600).map(|n| format!("word{n}")).collect();
        let found = chunks(&text.join(" "), 500, 100, 100);
        assert!(found.len() > 5);
        assert!(found.iter().all(|chunk| chunk.chars().count() <= 500));
        assert!(found.iter().all(|chunk| chunk.starts_with("word"))); // never starts mid-word
        assert!(found.last().unwrap().ends_with("word600"));
    }

    #[test]
    fn html_becomes_plain_text() {
        let html = "<html><head><title>T</title><style>p { color: red }</style></head>\n\
            <body><h1>Trip &amp; plans</h1><p>Fly to <b>Tokyo</b>&nbsp;on Friday.</p><script>var x = 1;</script>\n\
            <p>Caf&#233; &#x2014; ramen</p><!-- hidden --></body></html>";
        assert_eq!(plain_text_from_html(html), "Trip & plans\nFly to Tokyo on Friday.\nCafé — ramen");
    }

    #[test]
    fn rtf_becomes_plain_text() {
        let rtf = r"{\rtf1\ansi{\fonttbl\f0\fswiss Helvetica;}{\colortbl;\red255\green0\blue0;}\f0\pard Lease for the flat\par Pets: one cat allowed, caf\'e9 downstairs \u8212? no dogs.\par}";
        assert_eq!(plain_text_from_rtf(rtf), "Lease for the flat\nPets: one cat allowed, café downstairs — no dogs.");
    }

    #[test]
    fn docx_paragraphs_become_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease.docx");
        let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        writer.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(
            &mut writer,
            br#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body><w:p><w:r><w:t>Pets &amp; cats</w:t></w:r></w:p><w:p><w:r><w:t xml:space="preserve">are </w:t></w:r><w:r><w:t>allowed</w:t></w:r></w:p></w:body></w:document>"#,
        )
        .unwrap();
        writer.finish().unwrap();
        assert_eq!(doc_text(&path).unwrap(), "Pets & cats\nare allowed");
    }
}
