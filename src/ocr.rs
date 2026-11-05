//! Copying text from a picture with the recognition engines built into Windows.
//!
//! One engine reads only its own alphabet well: the Russian one turns "PIN" into "РЛ",
//! the English one turns "код" into "K0A". Screen text mixes both, so the picture is
//! read twice and the results are merged word by word.

use crate::image::Image;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::core::HSTRING;

/// Screen text is small for the engine, which expects scanned pages. Twice the size
/// gives clearly better results, up to the largest picture it accepts.
const MAX_DIMENSION: u32 = 10_000;
const PREVIEW_CHARS: usize = 110;
/// Latin capitals that have no look-alike in Cyrillic. A word with one of them is not
/// a Russian word misread by the English engine.
const ONLY_LATIN: &str = "DFGIJLNQRSUVWZ";
const MAX_ABBREVIATION: usize = 4;

fn is_cyrillic(c: char) -> bool {
    matches!(c, '\u{0400}'..='\u{04FF}')
}

/// Letters, then at most digits at the end: "Windows", "Win11", not "3a6n0Kn".
fn looks_like_latin_word(text: &str) -> bool {
    let core = text.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let digits_start = core.find(|c: char| c.is_ascii_digit()).unwrap_or(core.len());
    text.is_ascii()
        && core.chars().any(|c| c.is_ascii_alphabetic())
        && core[digits_start..].chars().all(|c| c.is_ascii_digit() || !c.is_ascii_alphanumeric())
}

/// Whether the English reading of a part of a word is the right one.
fn english_reading_wins(main: &str, english: &str) -> bool {
    // Digits and a percent sign: the Russian engine reads "100%" as "10096".
    if english.contains('%') && !main.contains('%') && !main.chars().any(char::is_alphabetic) {
        return true;
    }
    if !looks_like_latin_word(english) {
        return false;
    }
    // Latin letters seen by the main engine mean a Latin word, even if some letters
    // came out Cyrillic: "Micposoft" with a Cyrillic "р".
    if main.chars().any(|c| c.is_ascii_alphabetic()) {
        return true;
    }
    // A short word in capitals may be a Latin abbreviation read as Cyrillic: "РЛ" for "PIN".
    // Longer ones are ordinary Russian words, and "РАБОЧАЯ" reads as "PASOHAA" in English.
    let core = english.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    main.chars().any(is_cyrillic)
        && !main.chars().any(char::is_lowercase)
        && main.chars().filter(|c| c.is_alphabetic()).count() <= MAX_ABBREVIATION
        && core.len() >= 2
        && core.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && core.chars().any(|c| ONLY_LATIN.contains(c))
        && english.chars().count() <= main.chars().count() + 1
}

/// The best reading of one word. Parts around a hyphen are decided separately,
/// because "PIN-код" is half Latin and half Cyrillic.
fn merge_word(main: &str, english: &str) -> String {
    let main_parts: Vec<&str> = main.split('-').collect();
    let english_parts: Vec<&str> = english.split('-').collect();
    if main_parts.len() != english_parts.len() {
        return main.to_string();
    }
    let merged: Vec<&str> = main_parts
        .iter()
        .zip(&english_parts)
        .map(|(m, e)| if english_reading_wins(m, e) { *e } else { *m })
        .collect();
    merged.join("-")
}

#[derive(Clone, Copy)]
struct Area {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Area {
    fn overlap(&self, o: &Area) -> f32 {
        let w = (self.x + self.w).min(o.x + o.w) - self.x.max(o.x);
        let h = (self.y + self.h).min(o.y + o.h) - self.y.max(o.y);
        w.max(0.0) * h.max(0.0)
    }
}

struct Word {
    text: String,
    area: Area,
}

fn read_lines(engine: &OcrEngine, bitmap: &SoftwareBitmap) -> windows::core::Result<Vec<Vec<Word>>> {
    let result = engine.RecognizeAsync(bitmap)?.join()?;
    let mut lines = Vec::new();
    for line in result.Lines()? {
        let mut words = Vec::new();
        for word in line.Words()? {
            let r = word.BoundingRect()?;
            words.push(Word {
                text: word.Text()?.to_string(),
                area: Area { x: r.X, y: r.Y, w: r.Width, h: r.Height },
            });
        }
        lines.push(words);
    }
    Ok(lines)
}

/// The word of the other reading that covers the same place on the picture.
fn same_place<'a>(word: &Word, others: &'a [Vec<Word>]) -> Option<&'a Word> {
    let own_area = word.area.w * word.area.h;
    others
        .iter()
        .flatten()
        .map(|o| (word.area.overlap(&o.area), o))
        .filter(|(shared, _)| *shared * 2.0 >= own_area)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, o)| o)
}

fn merge_lines(main: &[Vec<Word>], english: &[Vec<Word>]) -> Vec<String> {
    main.iter()
        .map(|line| {
            let words: Vec<String> = line
                .iter()
                .map(|w| match same_place(w, english) {
                    Some(other) => merge_word(&w.text, &other.text),
                    None => w.text.clone(),
                })
                .collect();
            words.join(" ")
        })
        .collect()
}

fn recognize(img: &Image) -> windows::core::Result<String> {
    // Windows picks the first language of the user that has a recognizer.
    let main = OcrEngine::TryCreateFromUserProfileLanguages()?;
    let doubled;
    let source = if img.w.max(img.h) * 2 <= MAX_DIMENSION {
        doubled = img.doubled();
        &doubled
    } else {
        img
    };
    let buffer = CryptographicBuffer::CreateFromByteArray(&source.bgra)?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        source.w as i32,
        source.h as i32,
    )?;
    let lines = read_lines(&main, &bitmap)?;
    // Without an English recognizer the main reading is simply used as it is.
    let english = Language::CreateLanguage(&HSTRING::from("en-US"))
        .and_then(|language| OcrEngine::TryCreateFromLanguage(&language))
        .and_then(|engine| read_lines(&engine, &bitmap))
        .unwrap_or_default();
    Ok(join_lines(merge_lines(&lines, &english)))
}

fn join_lines(lines: Vec<String>) -> String {
    lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\r\n")
}

/// One line of the recognized text for the notification.
fn preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(PREVIEW_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Recognizes the text, puts it on the clipboard and says what happened.
pub fn copy_text(img: Image) {
    crate::notify::in_background(move || {
        let text = recognize(&img)?;
        if text.is_empty() {
            return crate::notify::info("Текст не найден", "Попробуйте выделить область крупнее", None);
        }
        crate::clipboard::set_text(&text)?;
        crate::notify::info("Текст скопирован", &preview(&text), None)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_lines_are_dropped() {
        let lines = vec!["  один ".to_string(), "   ".to_string(), "два".to_string()];
        assert_eq!(join_lines(lines), "один\r\nдва");
    }

    #[test]
    fn preview_is_one_short_line() {
        assert_eq!(preview("один\r\nдва   три"), "один два три");
        let long = "я".repeat(300);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW_CHARS + 1);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn latin_abbreviations_are_taken_from_the_english_reading() {
        assert_eq!(merge_word("РЛ", "PIN"), "PIN");
        assert_eq!(merge_word("РЛ-код?", "PIN-K0A?"), "PIN-код?");
        assert_eq!(merge_word("СТЯЬ", "CTRL"), "CTRL");
    }

    #[test]
    fn russian_words_are_not_replaced_by_their_english_misreadings() {
        // All letters have Cyrillic twins: nothing proves this is Latin.
        assert_eq!(merge_word("ВНИМАНИЕ", "BHMMAHME"), "ВНИМАНИЕ");
        // Far more letters than were seen by the Russian engine.
        assert_eq!(merge_word("ОШИБКА", "OLUVIEKA"), "ОШИБКА");
        // Ordinary words with lowercase letters are always Russian.
        assert_eq!(merge_word("заблокирован", "3a6n0KnpoBaH"), "заблокирован");
        assert_eq!(merge_word("Забыли", "3a6blJ1H"), "Забыли");
        // Digits inside the English reading are the usual sign of a misread word.
        assert_eq!(merge_word("ЗАГРУЗКА", "3ArPY3KA"), "ЗАГРУЗКА");
    }

    #[test]
    fn latin_read_correctly_by_the_main_engine_stays() {
        assert_eq!(merge_word("DEMO", "DEMO"), "DEMO");
        assert_eq!(merge_word("(Ctrl+S)", "(Ctrl+S)"), "(Ctrl+S)");
    }

    #[test]
    fn latin_words_garbled_by_the_main_engine_are_repaired() {
        // Real results: the first has a Cyrillic letter inside, the rest are misread Latin.
        assert_eq!(merge_word("Micрosoft", "Microsoft"), "Microsoft");
        assert_eq!(merge_word("PowerShett", "PowerShell"), "PowerShell");
        assert_eq!(merge_word("pights", "rights"), "rights");
        assert_eq!(merge_word("Win11", "Win11"), "Win11");
    }

    #[test]
    fn long_russian_words_in_capitals_stay_russian() {
        // A real misreading that also looks like a Latin word.
        assert_eq!(merge_word("РАБОЧАЯ", "PASOHAA"), "РАБОЧАЯ");
        assert_eq!(merge_word("ОБЛАСТЬ", "OBLAST"), "ОБЛАСТЬ");
    }

    #[test]
    fn percent_sign_comes_from_the_english_reading() {
        assert_eq!(merge_word("10096", "100%"), "100%");
        assert_eq!(merge_word("100", "100"), "100");
    }

    #[test]
    fn different_hyphenation_keeps_the_main_reading() {
        assert_eq!(merge_word("РЛкод", "PIN-K0A"), "РЛкод");
    }

    #[test]
    fn words_are_matched_by_place_not_by_order() {
        let word = |text: &str, x: f32| Word {
            text: text.to_string(),
            area: Area { x, y: 0.0, w: 30.0, h: 10.0 },
        };
        let main = vec![vec![word("Забыли", 0.0), word("РЛ", 40.0)]];
        // The English engine split the line differently and found one word more.
        let english = vec![vec![word("x", 200.0)], vec![word("PIN", 40.0), word("3a6b", 0.0)]];
        assert_eq!(merge_lines(&main, &english), vec!["Забыли PIN"]);
    }

    #[test]
    fn a_word_without_a_match_is_kept() {
        let word = Word { text: "РЛ".to_string(), area: Area { x: 0.0, y: 0.0, w: 10.0, h: 10.0 } };
        assert_eq!(merge_lines(&[vec![word]], &[]), vec!["РЛ"]);
    }

    /// Reads the real test screenshot with the engines of this Windows.
    #[test]
    #[ignore]
    fn reads_russian_and_latin_text_from_a_screenshot() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ui.png");
        let img = Image::from_png_file(std::path::Path::new(path)).unwrap();
        let text = recognize(&img).expect("recognition");
        eprintln!("---\n{text}\n---");
        assert!(text.contains("DEMO заблокирован"), "{text}");
        assert!(text.contains("PIN-код"), "{text}");
        assert!(text.contains("Сохранить как (Ctrl+S)"), "{text}");
    }
}
