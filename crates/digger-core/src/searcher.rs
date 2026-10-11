//! Hybrid search: meaning (EmbeddingGemma's vectors) + words (FTS5).
//!
//! Meaning is the model's cosine similarity, put on one scale per query (`meaning`): every vector is scored anyway,
//! so the distribution comes for free, and only segments that stand out (`z_gate`) count as meaning matches. That
//! also gives an honest "nothing found". Words rescue what a vector can't hold exactly: codes, IDs and names.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use rusqlite::{OptionalExtension, params};

use crate::code::lookup;
use crate::model::{FileKind, IndexOptions, Modality, SegmentKind};
use crate::search_text::{self, excerpt, is_word_char};
use crate::store::{IndexStore, Info, Result};
use crate::vectors::{DIMENSION, VectorStorage};

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: String,
    pub kind: FileKind,
    /// Fused rank score, for ordering only.
    pub score: f64,
    /// How far the best segment stands out for this query (`meaning`).
    pub z: Option<f64>,
    pub cosine: Option<f64>,
    /// 1-based rank among keyword matches.
    pub keyword_rank: Option<usize>,
    pub segment: SegmentKind,
    /// The segment the hit points at (its text is `Searcher::text_of_segment`).
    pub segment_id: i64,
    /// PDF page, or seconds into audio/video, or a code stretch's first line.
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    pub excerpt: Option<String>,
    /// The file's modification date (seconds since 1970), size, and what indexing learned about it.
    pub modified: f64,
    pub size: i64,
    pub info: Info,
    /// Code: the line to open at, in the stretch: where the name looked up is defined, or the first line with the
    /// query's words. None: the stretch's first line.
    pub focus: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub kinds: Option<HashSet<FileKind>>,
    pub limit: usize,
    pub z_gate: f64,
    /// None: the index's own (`WORD_WEIGHT`, or `CODE_WORD_WEIGHT` for code).
    pub word_weight: Option<f64>,
    /// The last word is still being typed: match it as a prefix.
    pub prefix_last: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            kinds: None,
            limit: 20,
            z_gate: 2.0,
            word_weight: None,
            prefix_last: false,
        }
    }
}

/// A keyword match: (file, segment, snippet).
type WordMatch = (i64, i64, Option<String>);

pub struct Searcher<'a> {
    store: &'a IndexStore,
    matrix: Option<VectorStorage>,
    row_file: Vec<i64>,
    row_segment: Vec<i64>,
    row_modality: Vec<Modality>,
    /// False for the vectors the later pass adds (`Indexer::finish_long_files`): scored, but they don't set the scale.
    row_sets_scale: Vec<bool>,
    file_kind: HashMap<i64, FileKind>,
    pub vector_source: Option<String>,
    pub vectors_loaded: bool,
}

/// How much finding the query's words counts against meaning. Code's words count for less: they're everywhere in
/// code ("app", "running", "location").
pub const WORD_WEIGHT: f64 = 2.0;
pub const CODE_WORD_WEIGHT: f64 = 1.0;

impl<'a> Searcher<'a> {
    /// With `load_vectors: false` only keyword search is ready; vectors load on the first meaning search.
    pub fn new(store: &'a IndexStore, load_vectors: bool) -> Result<Searcher<'a>> {
        let mut searcher = Searcher {
            store,
            matrix: None,
            row_file: Vec::new(),
            row_segment: Vec::new(),
            row_modality: Vec::new(),
            row_sets_scale: Vec::new(),
            file_kind: HashMap::new(),
            vector_source: None,
            vectors_loaded: false,
        };
        if load_vectors {
            searcher.reload()?
        } else {
            searcher.reload_files()?
        }
        Ok(searcher)
    }

    pub fn vector_count(&self) -> usize {
        self.row_file.len()
    }

    /// This is the code index's searcher: names are looked up as names (`lookup`).
    pub fn is_code(&self) -> bool {
        self.store.is_code()
    }

    /// Reads the files and all vectors again (after indexing added some).
    pub fn reload(&mut self) -> Result<()> {
        self.release_vectors();
        self.reload_files()?;
        let rows: i64 = self
            .store
            .db
            .query_row("SELECT COUNT(*) FROM segments WHERE vec IS NOT NULL", [], |row| {
                row.get(0)
            })?;
        let mut matrix = VectorStorage::new(rows as usize);
        let steps = IndexOptions::default();
        let mut statement = self.store.db.prepare(
            "SELECT s.id, s.file, s.modality, s.vec, s.kind, s.loc, f.kind, CAST(json_extract(f.info, '$.pages') AS INTEGER)
            FROM segments s JOIN files f ON f.id = s.file WHERE s.vec IS NOT NULL ORDER BY s.id",
        )?;
        let mut found = statement.query([])?;
        while let Some(row) = found.next()? {
            let blob: Vec<u8> = row.get(3)?;
            if !matrix.push(&blob) {
                continue;
            }
            self.row_segment.push(row.get(0)?);
            self.row_file.push(row.get(1)?);
            self.row_modality.push(Modality::parse(&row.get::<_, String>(2)?));
            // Past the first step of a long PDF (its pages) or document (its chunks).
            let kind: String = row.get(4)?;
            let place = row.get::<_, Option<f64>>(5)?.unwrap_or(0.0) as usize;
            let later = if row.get::<_, String>(6)? == "pdf" {
                kind != "chunk"
                    && row.get::<_, Option<i64>>(7)?.unwrap_or(0) as usize > steps.big_pdf_pages
                    && place > steps.pdf_step
            } else {
                kind == "chunk" && place > steps.doc_step
            };
            self.row_sets_scale.push(!later);
        }
        self.matrix = Some(matrix);
        self.vectors_loaded = true;
        Ok(())
    }

    /// Frees the vectors' memory; keyword search keeps working.
    pub fn release_vectors(&mut self) {
        self.matrix = None;
        self.row_file.clear();
        self.row_segment.clear();
        self.row_modality.clear();
        self.row_sets_scale.clear();
        self.vectors_loaded = false;
    }

    /// Reads the files (and what made the vectors) again, leaving the vectors as they are.
    pub fn reload_files(&mut self) -> Result<()> {
        self.file_kind = self.store.file_kinds()?;
        self.vector_source = self.store.meta("vector_source")?;
        Ok(())
    }

    pub fn search(
        &mut self,
        text: &str,
        query_vector: Option<&[f32]>,
        options: &SearchOptions,
    ) -> Result<Vec<SearchHit>> {
        let word_weight = options
            .word_weight
            .unwrap_or(if self.is_code() { CODE_WORD_WEIGHT } else { WORD_WEIGHT });
        if query_vector.is_some() && !self.vectors_loaded {
            self.reload()?;
        }
        let allowed = |file: i64, kinds: &HashMap<i64, FileKind>| {
            options
                .kinds
                .as_ref()
                .is_none_or(|wanted| kinds.get(&file).is_some_and(|kind| wanted.contains(kind)))
        };

        // Meaning: score every vector on this query's scale (`meaning`), keep each file's best segment.
        let mut best: HashMap<i64, (f64, f64, usize)> = HashMap::new(); // file → (z, cosine, row)
        if let (Some(query), Some(matrix)) = (query_vector, self.matrix.as_mut())
            && query.len() == DIMENSION
            && !self.row_file.is_empty()
        {
            let scores = matrix.scores(query);
            let meaning = meaning(&scores, &self.row_modality, Some(&self.row_sets_scale));
            for (row, z) in meaning.iter().enumerate() {
                let file = self.row_file[row];
                if !allowed(file, &self.file_kind) {
                    continue;
                }
                if best.get(&file).is_none_or(|known| *z > known.0) {
                    best.insert(file, (*z, scores[row] as f64, row));
                }
            }
        }

        // Words: FTS5 with Porter stemming over file names, folder names, OCR, page and document text. A name from
        // code ("quantize_row_q8_0_ref") is matched as one phrase, as it's written.
        let terms = terms(text);
        let identifier = if self.is_code() { lookup::identifier(text) } else { None };
        let patterns = match &identifier {
            Some(words) => vec![lookup::phrase(words)],
            None => patterns(&terms, options.prefix_last),
        };
        let mut words: Vec<WordMatch> = Vec::new();
        let mut coverage: HashMap<i64, f64> = HashMap::new(); // share of the query's words found in the file
        if !patterns.is_empty() {
            let mut seen = HashSet::new();
            let mut statement = self.store.db.prepare(
                "SELECT s.file, s.id, snippet(fts, 1, '«', '»', '…', 12), fts.original FROM fts
                JOIN segments s ON s.id = fts.rowid
                WHERE fts MATCH ? ORDER BY bm25(fts, 3.0, 1.0) LIMIT 1000",
            )?;
            let mut rows = statement.query([patterns.join(" OR ")])?;
            while let Some(row) = rows.next()? {
                let file: i64 = row.get(0)?;
                if words.len() >= 100 || !allowed(file, &self.file_kind) || !seen.insert(file) {
                    continue;
                }
                // Text that normalizing changed is quoted as it's written (with its hamza, harakat, joiners).
                let original: Option<String> = row.get(3)?;
                let snippet = original
                    .and_then(|original| search_text::snippet(&original, &terms, 12))
                    .or(row.get::<_, Option<String>>(2)?);
                words.push((file, row.get(1)?, snippet));
            }
            if !words.is_empty() {
                let files = words
                    .iter()
                    .map(|word| word.0.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let mut statement = self.store.db.prepare(&format!(
                    "SELECT DISTINCT s.file FROM fts JOIN segments s ON s.id = fts.rowid
                    WHERE fts MATCH ? AND s.file IN ({files})"
                ))?;
                for pattern in &patterns {
                    let mut rows = statement.query([pattern])?;
                    while let Some(row) = rows.next()? {
                        *coverage.entry(row.get(0)?).or_default() += 1.0 / patterns.len() as f64;
                    }
                }
            }
        }

        // A file qualifies by standing out in meaning, or by containing at least half of the query's words. Score:
        // meaning z + a bonus that grows with word coverage, + 1 for the kind the query asks for.
        let is_lookup = is_lookup(&terms) || identifier.is_some();
        let (meaning_weight, words_weight) = if is_lookup {
            (0.5, word_weight * 2.0)
        } else {
            (1.0, word_weight)
        };
        let hinted = kind_hints(text);
        let mut candidates: HashSet<i64> = best
            .iter()
            .filter(|(_, found)| found.0 >= options.z_gate)
            .map(|(file, _)| *file)
            .collect();
        candidates.extend(
            coverage
                .iter()
                .filter(|(_, share)| **share >= 0.5)
                .map(|(file, _)| *file),
        );
        let mut scored: Vec<(i64, f64)> = candidates
            .into_iter()
            .map(|file| {
                let mut score = meaning_weight * best.get(&file).map_or(0.0, |found| found.0).max(0.0)
                    + words_weight * coverage.get(&file).copied().unwrap_or(0.0).powf(1.5);
                if self.file_kind.get(&file).is_some_and(|kind| hinted.contains(kind)) {
                    score += 1.0;
                }
                (file, score)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

        let mut word_rank: HashMap<i64, (usize, &WordMatch)> = HashMap::new();
        for (offset, word) in words.iter().enumerate() {
            word_rank.entry(word.0).or_insert((offset + 1, word));
        }
        let mut hits = Vec::new();
        for (file, score) in scored.into_iter().take(options.limit) {
            let rank = hits.len();
            let semantic = best.get(&file).copied();
            let keyword = word_rank.get(&file).copied();
            // Point at the best meaning segment if it stands out; else at the segment the words matched.
            let use_meaning = semantic.is_some_and(|found| found.0 >= options.z_gate || keyword.is_none());
            let segment = if use_meaning {
                semantic.map(|found| self.row_segment[found.2])
            } else {
                keyword.map(|(_, word)| word.1)
            };
            let Some(mut segment_id) = segment else { continue };
            let mut focus = None;
            if self.is_code() {
                // Where a name is defined is looked for in the first 10 results only: a name used all over a big repo
                // is in hundreds of stretches.
                let words_here = keyword.is_some_and(|(_, word)| word.1 == segment_id) || identifier.is_some();
                let looked_up = if rank < 10 { identifier.as_deref() } else { None };
                (segment_id, focus) = self.code_place(file, segment_id, looked_up, text, &terms, words_here)?;
            }
            let (path, kind, modified, size, info): (String, String, f64, i64, Option<String>) =
                self.store.db.query_row(
                    "SELECT path, kind, mtime, size, info FROM files WHERE id = ?",
                    [file],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )?;
            let (segment, loc, loc_end, stored_excerpt): (String, Option<f64>, Option<f64>, Option<String>) =
                self.store.db.query_row(
                    "SELECT kind, loc, loc_end, excerpt FROM segments WHERE id = ?",
                    [segment_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?;
            // Quote the matched words only when they come from the segment we point at, so a PDF hit on page 5 never
            // shows page 2's words. A segment with no place and no text of its own (a screenshot's image vector) can
            // borrow them from its file's OCR text, which describes the same picture.
            let mut snippet = keyword
                .filter(|(_, word)| word.1 == segment_id)
                .and_then(|(_, word)| word.2.clone())
                .filter(|text| text.contains('«'))
                .map(|text| excerpt(&text, 400));
            if snippet.is_none()
                && stored_excerpt.is_none()
                && loc.is_none()
                && let Some((_, word)) = keyword
                && let Some(text) = word.2.as_ref().filter(|text| text.contains('«'))
            {
                let matched: Option<String> = self
                    .store
                    .db
                    .query_row("SELECT kind FROM segments WHERE id = ?", [word.1], |row| row.get(0))
                    .optional()?;
                if matched.as_deref() != Some("name") {
                    snippet = Some(excerpt(text, 400));
                }
            }
            hits.push(SearchHit {
                path,
                kind: kind.parse().unwrap_or(FileKind::Doc),
                score,
                z: semantic.map(|found| found.0),
                cosine: semantic.map(|found| found.1),
                keyword_rank: keyword.map(|(rank, _)| rank),
                segment: SegmentKind::parse(&segment).unwrap_or(SegmentKind::Name),
                segment_id,
                loc,
                loc_end,
                excerpt: snippet.or(stored_excerpt),
                modified,
                size,
                info: info
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
                focus,
            });
        }
        Ok(hits)
    }

    /// Code: the stretch to point at, and the line in it to open at. A name looked up points at the stretch that
    /// defines it, when one does (not the ones that call it). Otherwise the line is the first with the query's words,
    /// when they matched in this stretch; a match by meaning alone opens at the stretch's first line.
    fn code_place(
        &self,
        file: i64,
        segment: i64,
        identifier: Option<&[String]>,
        query: &str,
        terms: &[String],
        words_here: bool,
    ) -> Result<(i64, Option<f64>)> {
        let name = query.trim().replace("()", "");
        if let Some(identifier) = identifier {
            let mut best: Option<(i64, usize, u8)> = None;
            let mut statement = self.store.db.prepare(
                "SELECT s.id, s.loc, coalesce(fts.original, fts.body) FROM fts JOIN segments s ON s.id = fts.rowid
                WHERE fts MATCH ? AND s.file = ? ORDER BY s.loc LIMIT 300",
            )?;
            let mut rows = statement.query(params![lookup::phrase(identifier), file])?;
            while let Some(row) = rows.next()? {
                if best.is_some_and(|best| best.2 >= 2) {
                    break;
                }
                let Some(text) = row.get::<_, Option<String>>(2)? else {
                    continue;
                };
                let first = row.get::<_, Option<f64>>(1)?.unwrap_or(1.0) as usize;
                if let Some((line, strength)) = lookup::definition(&name, &text, first)
                    && strength > best.map_or(0, |best| best.2)
                {
                    best = Some((row.get(0)?, line, strength));
                }
            }
            if let Some((segment, line, _)) = best {
                return Ok((segment, Some(line as f64)));
            }
        }
        if !words_here && identifier.is_none() {
            return Ok((segment, None));
        }
        let Some(text) = self.text_of_segment(segment)? else {
            return Ok((segment, None));
        };
        let first: f64 = self
            .store
            .db
            .query_row("SELECT loc FROM segments WHERE id = ?", [segment], |row| {
                row.get::<_, Option<f64>>(0)
            })?
            .unwrap_or(1.0);
        let words = if identifier.is_none() {
            terms.to_vec()
        } else {
            vec![name.to_lowercase()]
        };
        Ok((
            segment,
            lookup::first_line(&words, &text, first as usize).map(|line| line as f64),
        ))
    }

    /// The text of a page, a document's passage or a screenshot's OCR, as it's written.
    pub fn text_of_segment(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .store
            .db
            .query_row(
                "SELECT coalesce(original, body) FROM fts WHERE rowid = ?",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }
}

/// Codes and IDs ("K7Q2LM", "TRX50") and long numbers ("4815162342") mean nothing to an embedding but everything to
/// keyword search: a query made of them weighs words double and meaning half.
pub fn is_lookup(terms: &[String]) -> bool {
    !terms.is_empty()
        && terms.iter().all(|term| {
            term.chars().any(char::is_numeric) && (term.chars().any(char::is_alphabetic) || term.chars().count() >= 4)
        })
}

/// How many of `hits` (best first) are strong: scoring at least `ratio` × the top hit. The rest pass the gates but
/// trail far behind, and are folded under "weaker matches".
pub fn strong_count(hits: &[SearchHit], ratio: f64) -> usize {
    let Some(top) = hits.first().map(|hit| hit.score).filter(|top| *top > 0.0) else {
        return hits.len();
    };
    hits.iter().take_while(|hit| hit.score >= top * ratio).count()
}

/// This query's meaning scores: each vector's cosine similarity minus the average of its modality, in standard
/// deviations of all of them together. The scale comes from the vectors in `sets_scale` (all by default).
pub fn meaning(scores: &[f32], modality: &[Modality], sets_scale: Option<&[bool]>) -> Vec<f64> {
    let counts = |row: usize| sets_scale.is_none_or(|sets| sets[row]);
    let means = modality_means(scores, modality, sets_scale);
    let centered: Vec<f64> = scores
        .iter()
        .zip(modality)
        .map(|(score, kind)| *score as f64 - means[kind])
        .collect();
    let (mut count, mut sum) = (0.0, 0.0);
    for (row, value) in centered.iter().enumerate() {
        if counts(row) {
            count += 1.0;
            sum += value;
        }
    }
    let mean = sum / f64::max(count, 1.0);
    let squares: f64 = centered
        .iter()
        .enumerate()
        .filter(|(row, _)| counts(*row))
        .map(|(_, value)| (value - mean).powi(2))
        .sum();
    let deviation = (squares / f64::max(count, 1.0)).sqrt().max(0.02);
    centered.iter().map(|value| (value - mean) / deviation).collect()
}

/// The average score of each modality for this query. Small groups (< 20 vectors) take the average of all.
pub fn modality_means(scores: &[f32], modality: &[Modality], sets_scale: Option<&[bool]>) -> HashMap<Modality, f64> {
    let mut sum: HashMap<Modality, f64> = HashMap::new();
    let mut count: HashMap<Modality, usize> = HashMap::new();
    for (row, score) in scores.iter().enumerate() {
        if sets_scale.is_none_or(|sets| sets[row]) {
            *sum.entry(modality[row]).or_default() += *score as f64;
            *count.entry(modality[row]).or_default() += 1;
        }
    }
    let all = sum.values().sum::<f64>() / count.values().sum::<usize>().max(1) as f64;
    Modality::ALL
        .into_iter()
        .map(|kind| match count.get(&kind) {
            Some(n) if *n >= 20 => (kind, sum[&kind] / *n as f64),
            _ => (kind, all),
        })
        .collect()
}

static STOPWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "a",
        "an",
        "the",
        "of",
        "in",
        "on",
        "at",
        "to",
        "for",
        "with",
        "and",
        "or",
        "my",
        "me",
        "that",
        "this",
        "these",
        "those",
        "from",
        "about",
        "where",
        "what",
        "which",
        "who",
        "when",
        "is",
        "was",
        "are",
        "were",
        "be",
        "it",
        "its",
        "some",
        "any",
        "find",
        "show",
        "file",
        "files",
        "photo",
        "photos",
        "picture",
        "image",
        "images",
        "screenshot",
        "screenshots",
        "video",
        "videos",
        "pdf",
        "document",
        "documents",
        "recording",
        // Bengali and Arabic (normalized, as `terms` sees them)
        "এবং",
        "ও",
        "এই",
        "সেই",
        "একটি",
        "আমার",
        "থেকে",
        "জন্য",
        "সাথে",
        "মধ্যে",
        "কোথায়",
        "কোন",
        "কী",
        "কি",
        "যে",
        "ছবি",
        "স্ক্রিনশট",
        "ভিডিও",
        "ফাইল",
        "পিডিএফ",
        "في",
        "من",
        "على",
        "الى",
        "عن",
        "مع",
        "هذا",
        "هذه",
        "ذلك",
        "التي",
        "الذي",
        "اين",
        "ما",
        "صورة",
        "صور",
        "لقطة",
        "شاشة",
        "فيديو",
        "ملف",
        "مستند",
    ]
    .into_iter()
    .collect()
});

fn words_of(text: &str) -> Vec<String> {
    search_text::normalized(text)
        .to_lowercase()
        .split(|c: char| !is_word_char(c))
        .filter(|word| !word.is_empty())
        .map(String::from)
        .collect()
}

/// The content words of a query, normalized as keyword search keeps its text. Kind words ("screenshot", "video") and
/// stopwords are dropped: they describe what to look for, not what's in it. Words of two letters or more count.
pub fn terms(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    words_of(text)
        .into_iter()
        .filter(|word| word.chars().count() >= 2 && !STOPWORDS.contains(word.as_str()) && seen.insert(word.clone()))
        .collect()
}

/// FTS5 patterns for the query's words: quoted, and as prefixes for the last word while it's being typed and for
/// words of languages that attach endings.
pub fn patterns(terms: &[String], prefix_last: bool) -> Vec<String> {
    terms
        .iter()
        .enumerate()
        .map(|(index, term)| {
            let prefix = (prefix_last && index == terms.len() - 1) || search_text::inflects(term);
            format!("\"{term}\"{}", if prefix { "*" } else { "" })
        })
        .collect()
}

/// The query's words as an FTS5 OR-query.
pub fn fts_query(text: &str, prefix_last: bool) -> Option<String> {
    let words = terms(text);
    if words.is_empty() {
        None
    } else {
        Some(patterns(&words, prefix_last).join(" OR "))
    }
}

static KIND_WORDS: LazyLock<Vec<(&'static str, FileKind)>> = LazyLock::new(|| {
    use FileKind::*;
    vec![
        ("screenshot", Screenshot),
        ("screenshots", Screenshot),
        ("photo", Image),
        ("photos", Image),
        ("picture", Image),
        ("pictures", Image),
        ("image", Image),
        ("images", Image),
        ("pdf", Pdf),
        ("pdfs", Pdf),
        ("paper", Pdf),
        ("papers", Pdf),
        ("document", Doc),
        ("documents", Doc),
        ("note", Doc),
        ("notes", Doc),
        ("video", Video),
        ("videos", Video),
        ("clip", Video),
        ("movie", Video),
        ("audio", Audio),
        ("song", Audio),
        ("podcast", Audio),
        ("voice", Audio),
        ("sound", Audio),
        ("স্ক্রিনশট", Screenshot),
        ("ছবি", Image),
        ("পিডিএফ", Pdf),
        ("নোট", Doc),
        ("ভিডিও", Video),
        ("অডিও", Audio),
        ("গান", Audio),
        ("রেকর্ডিং", Audio),
        ("لقطة", Screenshot),
        ("صورة", Image),
        ("صور", Image),
        ("مستند", Doc),
        ("ملاحظات", Doc),
        ("فيديو", Video),
        ("مقطع", Video),
        ("صوت", Audio),
        ("تسجيل", Audio),
        ("اغنية", Audio),
    ]
});

/// Kinds the query asks for ("…in a video", "screenshot of…", "ভিডিওতে"): a soft boost, not a filter. Bengali
/// attaches case endings, so a Bengali kind word also counts at the start of a longer word.
pub fn kind_hints(text: &str) -> HashSet<FileKind> {
    words_of(text)
        .iter()
        .filter_map(|word| {
            KIND_WORDS
                .iter()
                .find(|(key, _)| key == word)
                .map(|(_, kind)| *kind)
                .or_else(|| {
                    KIND_WORDS
                        .iter()
                        .find(|(key, _)| {
                            search_text::inflects(key) && key.chars().count() >= 3 && word.starts_with(key)
                        })
                        .map(|(_, kind)| *kind)
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Candidate;

    #[test]
    fn query_words_of_languages_with_endings_are_prefixes() {
        assert_eq!(fts_query("কুমির", false).as_deref(), Some("\"কুমির\"*"));
        assert_eq!(
            fts_query("مدرسة الأطفال", false).as_deref(),
            Some("\"مدرسة\"* OR \"الاطفال\"*")
        );
        assert_eq!(fts_query("মা", false).as_deref(), Some("\"মা\"*"));
        assert_eq!(
            fts_query("payment error", false).as_deref(),
            Some("\"payment\" OR \"error\"")
        );
        assert_eq!(kind_hints("জেব্রার ভিডিওতে"), HashSet::from([FileKind::Video]));
        assert_eq!(kind_hints("صورة حمار وحشي"), HashSet::from([FileKind::Image]));
    }

    #[test]
    fn fts_query_drops_stopwords_and_kind_words() {
        assert_eq!(
            fts_query("the screenshot of my payment error", false).as_deref(),
            Some("\"payment\" OR \"error\"")
        );
        assert_eq!(fts_query("zebr", true).as_deref(), Some("\"zebr\"*"));
        assert_eq!(fts_query("a screenshot", false), None);
    }

    #[test]
    fn bengali_and_arabic_words_stay_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = IndexStore::open_main(dir.path()).unwrap();
        let id = store
            .insert(&Candidate {
                path: "/notes/trip.txt".into(),
                kind: FileKind::Doc,
                size: 1,
                mtime: 1.0,
                inode: 1,
                device: 1,
            })
            .unwrap();
        let text = "সুন্দরবনে আমরা কুমিরও দেখেছি। قَالَ إِنَّ الْمَكْتَبَةَ تُغْلَقُ";
        store
            .add_segment(
                id,
                SegmentKind::Chunk,
                Modality::Text,
                Some(1.0),
                None,
                None,
                Some(text),
                None,
            )
            .unwrap();
        let mut searcher = Searcher::new(&store, false).unwrap();
        let options = SearchOptions::default();
        let bengali = searcher.search("কুমির", None, &options).unwrap();
        assert_eq!(
            bengali.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(),
            ["/notes/trip.txt"]
        );
        assert!(
            bengali[0].excerpt.as_deref().unwrap().contains("«কুমিরও»"),
            "{:?}",
            bengali[0].excerpt
        );
        assert_eq!(searcher.search("المكتبة", None, &options).unwrap().len(), 1);
        assert!(searcher.search("কলকাতা", None, &options).unwrap().is_empty());
    }

    #[test]
    fn meaning_takes_out_each_modalitys_level_on_one_scale() {
        let text: Vec<f32> = (0..30).map(|i| 0.70 + (i % 3) as f32 * 0.01).collect();
        let image: Vec<f32> = (0..30).map(|i| 0.55 + (i % 3) as f32 * 0.002).collect();
        let scores = [text, image].concat();
        let modality = [vec![Modality::Text; 30], vec![Modality::Image; 30]].concat();
        let z = meaning(&scores, &modality, None);
        assert!(z[1].abs() < 1e-4 && z[31].abs() < 1e-4);
        assert!((z[2] / z[32] - 5.0).abs() < 0.01);
        let means = modality_means(&scores, &modality, None);
        assert!((means[&Modality::Text] - 0.71).abs() < 0.001 && (means[&Modality::Image] - 0.552).abs() < 0.001);
        assert!((means[&Modality::Audio] - 0.631).abs() < 0.001);
    }

    #[test]
    fn later_pages_are_scored_without_moving_the_scale() {
        let scores: Vec<f32> = (0..30)
            .map(|i| 0.70 + (i % 3) as f32 * 0.01)
            .chain((0..30).map(|i| 0.55 + (i % 3) as f32 * 0.002))
            .collect();
        let modality = [vec![Modality::Text; 30], vec![Modality::Image; 30]].concat();
        let book: Vec<f32> = (0..400).map(|i| 0.60 + (i % 5) as f32 * 0.004).chain([0.80]).collect();
        let z = meaning(&scores, &modality, None);
        let sets = [vec![true; 60], vec![false; 401]].concat();
        let with_book = meaning(
            &[scores.clone(), book].concat(),
            &[modality, vec![Modality::Text; 401]].concat(),
            Some(&sets),
        );
        for row in 0..60 {
            assert!((z[row] - with_book[row]).abs() < 1e-9);
        }
        assert!(*with_book.last().unwrap() > z.iter().cloned().fold(f64::MIN, f64::max));
    }

    #[test]
    fn codes_and_long_numbers_are_lookups() {
        let words = |list: &[&str]| list.iter().map(|word| word.to_string()).collect::<Vec<_>>();
        assert!(is_lookup(&words(&["k7q2lm"])));
        assert!(is_lookup(&words(&["trx50"])));
        assert!(is_lookup(&words(&["4815162342"])));
        assert!(is_lookup(&words(&["2041"])));
        assert!(!is_lookup(&words(&["417"])));
        assert!(!is_lookup(&words(&["inv", "2041"])));
        assert!(!is_lookup(&[]));
    }
}
