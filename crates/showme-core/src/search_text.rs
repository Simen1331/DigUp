//! Text the way keyword search keeps it and matches it, so that a word is found however it was typed or extracted.

use std::ops::Range;

use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

/// NFKC (Arabic presentation forms and ligatures become plain letters, full-width becomes ASCII, and Bengali's two
/// spellings of "ো" become one), invisible formatting characters dropped (Bengali's zero-width joiners, direction
/// marks), and Arabic the way people type it: no tatweel, no short-vowel marks, one alef.
pub fn normalized(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.nfkc() {
        match c as u32 {
            0x200B => out.push(' '), // zero-width space: a word break (Thai, Khmer)
            0x00AD | 0x061C | 0x200C..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x2069 | 0xFEFF => {} // joiners, direction marks, soft hyphen, BOM
            0x0640 | 0x064B..=0x065F | 0x0670 => {} // tatweel; harakat and the dagger alef
            0x0622 | 0x0623 | 0x0625 | 0x0671 => out.push('\u{0627}'), // alef with hamza or madda, wasla → alef
            _ => out.push(c),
        }
    }
    out
}

const INDIC: Range<u32> = 0x0900..0x0E00;
const ARABIC: Range<u32> = 0x0600..0x0700;

/// True for words of scripts that attach endings to words (Bengali, Hindi, Arabic): keyword search matches them as
/// prefixes, so "কুমির" finds "কুমিরও" and "مدرس" finds "مدرسة".
pub fn inflects(word: &str) -> bool {
    word.chars().any(|c| INDIC.contains(&(c as u32)) || ARABIC.contains(&(c as u32)))
}

/// Letters and marks (Foundation's `CharacterSet.letters`): marks count, such as Bengali vowel signs.
pub(crate) fn is_letter(c: char) -> bool {
    use GeneralCategory::*;
    matches!(
        get_general_category(c),
        UppercaseLetter
            | LowercaseLetter
            | TitlecaseLetter
            | ModifierLetter
            | OtherLetter
            | NonspacingMark
            | SpacingMark
            | EnclosingMark
    )
}

/// Letters, marks and numbers (Foundation's `CharacterSet.alphanumerics`): what a word is made of.
pub(crate) fn is_word_char(c: char) -> bool {
    use GeneralCategory::*;
    is_letter(c) || matches!(get_general_category(c), DecimalNumber | LetterNumber | OtherNumber)
}

/// Whether text says something in words: at least a fifth of what isn't space is letters. A data dump or a table of
/// numbers isn't, and gets keyword search only: a meaning vector of digits matches nothing anyone would describe.
pub fn has_words(text: &str) -> bool {
    let (mut letters, mut others) = (0usize, 0usize);
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        if is_letter(c) {
            letters += 1;
        } else {
            others += 1;
        }
    }
    letters > 0 && letters * 4 >= others
}

/// For matching: normalized, lowercased, and without Latin accents (as FTS5's remove_diacritics does).
fn fold(text: &str) -> String {
    let lower = normalized(text).to_lowercase();
    lower.nfd().filter(|c| !(0x0300..0x0370).contains(&(*c as u32))).nfc().collect()
}

/// `text` folded one grapheme at a time, remembering where each piece came from.
struct Folded {
    text: String,
    /// For each byte of `text`, the grapheme of the source it came from.
    origin: Vec<usize>,
    /// Each grapheme of the source, as a byte range.
    graphemes: Vec<Range<usize>>,
}

impl Folded {
    fn new(source: &str) -> Folded {
        let (mut text, mut origin, mut graphemes) = (String::new(), Vec::new(), Vec::new());
        for (start, grapheme) in source.grapheme_indices(true) {
            let piece = fold(grapheme);
            text.push_str(&piece);
            origin.extend(std::iter::repeat_n(graphemes.len(), piece.len()));
            graphemes.push(start..start + grapheme.len());
        }
        Folded { text, origin, graphemes }
    }

    /// The bytes of the source that `range` (in `text`) came from.
    fn original(&self, range: Range<usize>) -> Option<Range<usize>> {
        if range.is_empty() || range.end > self.origin.len() {
            return None;
        }
        let (first, last) = (self.origin[range.start], self.origin[range.end - 1]);
        Some(self.graphemes[first].start..self.graphemes[last].end)
    }
}

fn is_word_grapheme(grapheme: &str) -> bool {
    grapheme.chars().any(is_word_char)
}

/// Where `word` is in `text` as keyword search would find it (normalized, ignoring case and accents), as byte ranges
/// of the text as it's written: whole words, and for a word of a language that attaches endings, the whole word it
/// starts ("কুমির" marks all of "কুমিরের"). For marking matches on what's shown.
pub fn matches(word: &str, text: &str) -> Vec<Range<usize>> {
    let needle = fold(word);
    if needle.is_empty() {
        return Vec::new();
    }
    let folded = Folded::new(text);
    let prefix = inflects(word);
    let mut found: Vec<Range<usize>> = Vec::new();
    for (at, _) in folded.text.match_indices(&needle) {
        if found.len() >= 500 {
            break;
        }
        let Some(mut range) = folded.original(at..at + needle.len()) else {
            continue;
        };
        if let Some(before) = text[..range.start].graphemes(true).next_back()
            && is_word_grapheme(before)
        {
            continue;
        }
        if let Some(after) = text[range.end..].graphemes(true).next()
            && is_word_grapheme(after)
        {
            if !prefix {
                continue;
            }
            let mut end = range.end;
            for grapheme in text[range.end..].graphemes(true) {
                if !is_word_grapheme(grapheme) {
                    break;
                }
                end += grapheme.len();
            }
            range = range.start..end;
        }
        if found.last().is_some_and(|last| last.start < range.end && range.start < last.end) {
            continue;
        }
        found.push(range);
    }
    found
}

/// One line from `text` around the first of `terms` it has, the terms marked « » as SQLite's snippet() marks them:
/// the excerpt of a keyword match, from the text as it's written. None when none of the terms is in it.
pub fn snippet(text: &str, terms: &[String], words: usize) -> Option<String> {
    let mut marks: Vec<Range<usize>> = terms.iter().flat_map(|term| matches(term, text)).collect();
    marks.sort_by_key(|mark| mark.start);
    let first = marks.first()?.clone();
    // Work in characters: byte ranges → char indices.
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let index_of = |byte: usize| chars.partition_point(|(at, _)| *at < byte);
    let (first_start, first_end) = (index_of(first.start), index_of(first.end));
    let ws = |i: usize| chars[i].1.is_whitespace();
    let n = chars.len();
    // About `words` words, starting a few before the first match.
    let (mut start, mut before) = (first_start, 0);
    while start > 0 && before < words / 3 {
        start -= 1;
        if ws(start) && start < first_start && !ws(start + 1) {
            before += 1;
        }
    }
    if start > 0 || ws(start) {
        start += 1;
    }
    let (mut end, mut after) = (first_end, 0);
    while end < n && after < words.saturating_sub(before) {
        if ws(end) {
            after += 1;
        }
        end += 1;
    }
    let byte = |i: usize| if i < n { chars[i].0 } else { text.len() };
    let mut out = String::from(if start > 0 { "…" } else { "" });
    let mut cursor = start;
    for mark in &marks {
        let (lower, upper) = (index_of(mark.start), index_of(mark.end));
        if lower >= start && upper <= end && lower >= cursor {
            out.push_str(&text[byte(cursor)..byte(lower)]);
            out.push('«');
            out.push_str(&text[byte(lower)..byte(upper)]);
            out.push('»');
            cursor = upper;
        }
    }
    out.push_str(&text[byte(cursor)..byte(end)]);
    if end < n {
        out.push('…');
    }
    Some(out.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// What's wrong with the text a PDF reader got from a page, when the page's text layer can't be used as it is.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PageTextProblem {
    /// Bengali or Hindi read in glyph order: vowel signs ahead of their consonants, conjuncts lost.
    GlyphOrder,
    /// Arabic letters lost where the font drew a lam ligature: a bare alef before a "?" or ".".
    LostLetters,
}

impl PageTextProblem {
    pub fn check(text: &str) -> Option<PageTextProblem> {
        let (mut vowel_signs, mut misplaced, mut lost_letters) = (0, 0, 0);
        let (mut previous, mut before_previous) = (' ', ' ');
        for c in text.nfc() {
            if is_indic_vowel_sign(c as u32) {
                vowel_signs += 1;
                if !is_indic_consonant_or_nukta(previous as u32) {
                    misplaced += 1;
                }
            }
            if before_previous == '\u{0627}'
                && "?.\u{FFFD}".contains(previous)
                && ARABIC.contains(&(c as u32))
                && c.is_alphabetic()
            {
                lost_letters += 1;
            }
            before_previous = previous;
            previous = c;
        }
        if vowel_signs >= 10 && misplaced * 20 >= vowel_signs {
            return Some(PageTextProblem::GlyphOrder);
        }
        if lost_letters >= 2 {
            return Some(PageTextProblem::LostLetters);
        }
        None
    }
}

/// Dependent vowel signs of Bengali and Devanagari: in a real word, each follows a consonant (or its nukta).
fn is_indic_vowel_sign(value: u32) -> bool {
    matches!(value, 0x09BE..=0x09C4 | 0x09C7 | 0x09C8 | 0x09CB | 0x09CC | 0x09D7 | 0x09E2 | 0x09E3)
        || matches!(value, 0x093A | 0x093B | 0x093E..=0x094C | 0x094E | 0x094F | 0x0955..=0x0957 | 0x0962 | 0x0963)
}

fn is_indic_consonant_or_nukta(value: u32) -> bool {
    matches!(value, 0x0995..=0x09B9 | 0x09BC | 0x09CE | 0x09DC | 0x09DD | 0x09DF | 0x09F0 | 0x09F1)
        || matches!(value, 0x0915..=0x0939 | 0x093C | 0x0958..=0x095F | 0x0979..=0x097F)
}

/// One line, at most `limit` characters: for result lists.
pub fn excerpt(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > limit { line.chars().take(limit).collect::<String>() + "…" } else { line }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(word: &str, text: &str) -> Vec<String> {
        matches(word, text).into_iter().map(|range| text[range].to_string()).collect()
    }

    #[test]
    fn normalized_folds_what_is_typed_differently() {
        assert_eq!(normalized("الْمَكْتَبَةَ"), "المكتبة"); // harakat
        assert_eq!(normalized("إلى الأردن"), "الى الاردن"); // alef forms
        assert_eq!(normalized("مـــرحبا"), "مرحبا"); // tatweel
        assert_eq!(normalized("\u{FEFB}"), "\u{0644}\u{0627}"); // the lam-alef ligature → lam, alef
        assert_eq!(normalized("\u{FB01}le"), "file"); // ﬁ
        assert_eq!(normalized("র\u{200D}্যাব"), "র্যাব"); // zero-width joiner
        assert_eq!(normalized("\u{09C7}\u{09BE}"), "\u{09CB}"); // ে + া → ো
        assert_eq!(normalized("a\u{200B}b"), "a b"); // zero-width space breaks words
    }

    #[test]
    fn garbled_page_text_is_noticed() {
        let glyph_order = "বািড় ভাড়ার চিপ ধানমি ৭ নর রােডর িতন বডরেমর াট মািসক ২৫,০০০ টাকা ভাড়ায় দওয়া হেলা। ভাড়ায়া দুই মােসর";
        let bengali = "বাড়ি ভাড়ার চুক্তিপত্র। ধানমন্ডি ৭ নম্বর রোডের তিন বেডরুমের ফ্ল্যাটটি মাসিক ২৫,০০০ টাকা ভাড়ায় দেওয়া হলো।";
        let lost = "ارتفعت ا.بيعات بنسبة خمسة عشر با.ئة في الربع الثالث. زرنا مدينة البتراء ا?ثرية في جنوب ا?ردن.";
        let arabic = "ارتفعت المبيعات بنسبة خمسة عشر بالمئة في الربع الثالث. زرنا مدينة البتراء الأثرية في جنوب الأردن.";
        assert_eq!(PageTextProblem::check(glyph_order), Some(PageTextProblem::GlyphOrder));
        assert_eq!(PageTextProblem::check(bengali), None);
        assert_eq!(PageTextProblem::check(lost), Some(PageTextProblem::LostLetters));
        assert_eq!(PageTextProblem::check(arabic), None);
        assert_eq!(PageTextProblem::check("Revenue grew 20%. See p.4?No. Done."), None);
        assert_eq!(
            PageTextProblem::check("أ.د.محمد علي، أستاذ في الجامعة. د.خالد و د.سارة من شركة ذ.م.م للتجارة"),
            None
        );
        assert_eq!(PageTextProblem::check(&"\u{F0B7} Item one of the slide\n".repeat(6)), None);
    }

    #[test]
    fn matches_are_found_as_keyword_search_finds_them_but_shown_as_written() {
        assert_eq!(found("الاردن", "زرنا مدينة البتراء الأثرية في جنوب الأردن."), ["الأردن"]);
        assert_eq!(found("المكتبة", "إِنَّ الْمَكْتَبَةَ تُغْلَقُ"), ["الْمَكْتَبَةَ"]);
        assert_eq!(found("কুমির", "একটি বড় কুমিরের ছবি"), ["কুমিরের"]); // never "কুমি"
        assert_eq!(found("day", "Sunday trip: day two"), ["day"]); // not in "Sunday"
        let turkish = "İ".repeat(60) + " son kelime";
        assert_eq!(found("kelime", &turkish), ["kelime"]);
        assert_eq!(found("cafe", "Café au lait"), ["Café"]);
    }

    #[test]
    fn snippets_quote_the_text_as_written() {
        let text = "قَالَ مُحَمَّدٌ إِنَّ الْمَكْتَبَةَ الْعَامَّةَ تُغْلَقُ فِي السَّاعَةِ الثَّامِنَةِ مَسَاءً يَوْمَ الْخَمِيسِ";
        let snippet = snippet(text, &["المكتبة".into()], 12).unwrap();
        assert!(snippet.contains("«الْمَكْتَبَةَ»"), "{snippet}");
        assert_eq!(super::snippet(text, &["كلمة".into()], 12), None);
        let long = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen";
        assert_eq!(
            super::snippet(long, &["eight".into()], 12).unwrap(),
            "…five six seven «eight» nine ten eleven twelve thirteen fourteen fifteen …"
        );
    }

    #[test]
    fn words_or_digits() {
        assert!(has_words("Flight to Tokyo on November 3"));
        assert!(!has_words("12.5, 13.0, 17.25, 99, 100, 2026-10-09"));
        assert!(has_words("বাড়ি ভাড়ার চুক্তিপত্র"));
    }
}
