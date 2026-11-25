//! App-aware formatting: fits a dictation into the text around the cursor.
//!
//! A port of `DictationInsertionTextFormatter` in the macOS app
//! (`TypeWhisper/ViewModels/DictationViewModel.swift`), so that a dictation
//! lands the same way on both. As with Swift's `Character`, a character here is
//! a grapheme cluster.

use std::sync::LazyLock;

use regex::Regex;
use tw_platform::accessibility::InsertionContext;
use unicode_segmentation::UnicodeSegmentation;

const OPENING_PUNCTUATION: &[&str] = &["(", "[", "{", "\"", "'", "“", "‘"];
const CLOSING_PUNCTUATION: &[&str] = &[
    ".", ",", "!", "?", ";", ":", ")", "]", "}", "\"", "'", "”", "’",
];
const PUNCTUATION_THAT_TAKES_FOLLOWING_SPACE: &[&str] = CLOSING_PUNCTUATION;

/// Adapts `text` to the field it goes into: spaces at the edges, a lowercase
/// first word and no final period in the middle of a sentence, and no final
/// period after a value such as an email address in an empty field. Without a
/// context, or with app-aware formatting off, the text stays exactly as it is.
pub fn text_for_insertion(
    text: &str,
    context: Option<&InsertionContext>,
    app_formatting: bool,
    strip_standalone_value_period: bool,
) -> String {
    let Some(boundaries) = context.filter(|_| app_formatting).and_then(Boundaries::of) else {
        return text.to_owned();
    };

    let mut result = text.trim().to_owned();
    if boundaries.is_mid_sentence() {
        result = lowercasing_first_word_if_safe(&result);
    }
    if boundaries.should_strip_final_period() {
        result = stripping_single_final_period(&result);
    }
    // Needs a field without other text, while the rule above needs text on
    // both sides, so a period is stripped at most once.
    if strip_standalone_value_period
        && boundaries.previous_non_whitespace.is_none()
        && boundaries.next_non_whitespace.is_none()
        && is_standalone_value_with_final_period(&result)
    {
        result = stripping_single_final_period(&result);
    }

    let leading = match (boundaries.previous, result.graphemes(true).next()) {
        (Some(previous), Some(first)) => should_insert_space(previous, first),
        _ => false,
    };
    let trailing = match (boundaries.next, result.graphemes(true).next_back()) {
        (Some(next), Some(last)) => should_insert_space(last, next),
        _ => false,
    };
    if leading {
        result.insert(0, ' ');
    }
    if trailing {
        result.push(' ');
    }
    result
}

/// The characters next to the selection.
struct Boundaries<'a> {
    previous: Option<&'a str>,
    next: Option<&'a str>,
    previous_non_whitespace: Option<&'a str>,
    next_non_whitespace: Option<&'a str>,
}

impl<'a> Boundaries<'a> {
    /// `None` when the selection does not lie within the value.
    fn of(context: &'a InsertionContext) -> Option<Self> {
        let value = context.value.as_str();
        let start = byte_offset(value, context.selection.start)?;
        let end = byte_offset(value, context.selection.end)?;
        if start > end {
            return None;
        }
        let before = &value[..start];
        let after = &value[end..];
        Some(Self {
            previous: before.graphemes(true).next_back(),
            next: after.graphemes(true).next(),
            previous_non_whitespace: before.graphemes(true).rev().find(|g| !is_whitespace(g)),
            next_non_whitespace: after.graphemes(true).find(|g| !is_whitespace(g)),
        })
    }

    fn is_mid_sentence(&self) -> bool {
        self.previous_non_whitespace.is_some_and(is_word_like)
    }

    fn should_strip_final_period(&self) -> bool {
        self.is_mid_sentence()
            && self
                .next_non_whitespace
                .is_some_and(|next| is_word_like(next) || CLOSING_PUNCTUATION.contains(&next))
    }
}

fn byte_offset(text: &str, characters: usize) -> Option<usize> {
    text.char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .nth(characters)
}

/// Whether a text standing on its own is a value whose final period came from
/// the model rather than from the speaker, as in `name@example.com.` dictated
/// into an empty field.
///
/// Conservative on purpose: only whole-text email addresses, bare-domain URLs,
/// decimal numbers, phone numbers and version strings qualify. Abbreviations
/// (`Dr.`, `U.S.`), prose, ambiguous numbers such as dates, and URLs with a
/// path, query or fragment keep their period, since a dot is a legal part of
/// those (RFC 3986 section 2.3).
fn is_standalone_value_with_final_period(text: &str) -> bool {
    let mut last = text.graphemes(true).rev();
    if last.next() != Some(".") || last.next() == Some(".") {
        return false;
    }
    let candidate = &text[..text.len() - 1];
    if candidate.is_empty() || ABBREVIATION.is_match(text) || DATE.is_match(candidate) {
        return false;
    }
    EMAIL.is_match(candidate)
        || WEB_ADDRESS.is_match(candidate)
        || DECIMAL.is_match(candidate)
        || VERSION.is_match(candidate)
        || is_phone_number(candidate)
}

fn is_phone_number(candidate: &str) -> bool {
    PHONE.is_match(candidate) && candidate.chars().filter(|c| c.is_numeric()).count() >= 6
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid pattern")
}

/// `Dr.`, `U.S.`, `e.g.`, `Dr.med.`, `Ph.D.`: never values. Each dot-separated
/// group is a single letter or a known abbreviation, so `file.txt.` still counts
/// as a value.
static ABBREVIATION: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)^(?:(?:[A-Za-z]|Dr|Mr|Mrs|Ms|No|St|Jr|Sr|Prof|Inc|Ltd|Co|etc|vs|bzw|ca|ggf|evtl|Nr|Tel|med|rer|nat|ing|dipl|phil|Ph)\.)+$",
    )
});
static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}$"));
/// Bare-domain URLs only: `example.com`, `www.example.com`, `https://example.com`.
/// Once a path, query or fragment follows, the dot may belong to the resource
/// (`https://example.com/search?q=Dr.`).
static WEB_ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)^(?:https?://|ftp://|www\.)?(?:[A-Za-z0-9](?:[A-Za-z0-9\-]*[A-Za-z0-9])?\.)+[A-Za-z]{2,}(?::\d+)?$",
    )
});
/// `3.14`, `1,5`, `1,000.50`, `1.000,50`: English and German forms.
static DECIMAL: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^\d{1,3}(?:[.,]\d{3})+(?:[.,]\d+)?$|^\d+[.,]\d+$"));
/// `19.04.2026`, `2026-09-27`, `27. 09. 2026`: ambiguous with versions, so left
/// alone. Dictation often puts spaces around the separators; without them such
/// dates would pass as phone numbers and lose their period.
static DATE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^\d{1,2}\s*[./\-]\s*\d{1,2}\s*[./\-]\s*\d{2,4}$|^\d{4}\s*[./\-]\s*\d{1,2}\s*[./\-]\s*\d{1,2}$",
    )
});
static VERSION: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)^v?\d+(?:\.\d+){2,}$"));
static PHONE: LazyLock<Regex> = LazyLock::new(|| regex(r"^[+\d(][\d\s\-/.()]*$"));

fn lowercasing_first_word_if_safe(text: &str) -> String {
    let graphemes: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    let Some(start) = graphemes.iter().position(|(_, g)| !is_whitespace(g)) else {
        return text.to_owned();
    };
    let length = graphemes[start..]
        .iter()
        .take_while(|(_, g)| is_word_like(g))
        .count();
    let word: Vec<&str> = graphemes[start..start + length]
        .iter()
        .map(|(_, g)| *g)
        .collect();
    if !should_lowercase_first_word(&word) {
        return text.to_owned();
    }
    let (offset, first) = graphemes[start];
    format!(
        "{}{}{}",
        &text[..offset],
        first.to_lowercase(),
        &text[offset + first.len()..]
    )
}

/// Only a capitalised word: `Presented`, not `NASA`, `TypeWhisper` or `I`.
fn should_lowercase_first_word(word: &[&str]) -> bool {
    let [first, remainder @ ..] = word else {
        return false;
    };
    !remainder.is_empty()
        && is_uppercase_letter(first)
        && remainder.iter().any(|g| is_lowercase_letter(g))
        && !remainder.iter().any(|g| is_uppercase_letter(g))
}

/// Removes one final period, but not the last dot of an ellipsis.
fn stripping_single_final_period(text: &str) -> String {
    let graphemes: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    let Some(last) = graphemes.iter().rposition(|(_, g)| !is_whitespace(g)) else {
        return text.to_owned();
    };
    let (offset, character) = graphemes[last];
    if character != "." || (last > 0 && graphemes[last - 1].1 == ".") {
        return text.to_owned();
    }
    format!("{}{}", &text[..offset], &text[offset + 1..])
}

fn should_insert_space(left: &str, right: &str) -> bool {
    if is_whitespace(left) || is_whitespace(right) {
        return false;
    }
    if CLOSING_PUNCTUATION.contains(&right) || OPENING_PUNCTUATION.contains(&left) {
        return false;
    }
    if is_cjk(left) && is_cjk(right) {
        return false;
    }
    if is_word_like(left) && is_word_like(right) {
        return true;
    }
    is_word_like(right) && PUNCTUATION_THAT_TAKES_FOLLOWING_SPACE.contains(&left)
}

/// Letters, marks and numbers, as Foundation's `alphanumerics`.
static WORD_LIKE: LazyLock<Regex> = LazyLock::new(|| regex(r"[\p{L}\p{M}\p{N}]"));
static UPPERCASE: LazyLock<Regex> = LazyLock::new(|| regex(r"[\p{Lu}\p{Lt}]"));
static LOWERCASE: LazyLock<Regex> = LazyLock::new(|| regex(r"\p{Ll}"));
/// Script extensions rather than scripts, so shared kana marks such as `ー`,
/// `ｰ`, `ﾞ` and `ﾟ` count as CJK. Only the first scalar is checked: combining
/// marks such as U+0323 also belong to Han, and a decomposed Latin `ạ` must not
/// become a CJK character.
static CJK: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[\p{scx=Han}\p{scx=Hiragana}\p{scx=Katakana}\p{scx=Hangul}]"));

fn is_word_like(character: &str) -> bool {
    WORD_LIKE.is_match(character)
}

fn is_uppercase_letter(character: &str) -> bool {
    UPPERCASE.is_match(character)
}

fn is_lowercase_letter(character: &str) -> bool {
    LOWERCASE.is_match(character)
}

fn is_cjk(character: &str) -> bool {
    CJK.is_match(character)
}

/// Unicode White_Space, the same set as Foundation's `whitespacesAndNewlines`.
fn is_whitespace(character: &str) -> bool {
    character.chars().all(char::is_whitespace)
}

/// Ported from `DictationInsertionTextFormatterTests` in the macOS app
/// (`TypeWhisperTests/DictationShortSpeechTests.swift`). Locations there are
/// UTF-16 offsets; in these texts they equal character offsets.
#[cfg(test)]
mod tests {
    use super::*;

    fn context(value: &str, location: usize, length: usize) -> InsertionContext {
        InsertionContext {
            value: value.to_owned(),
            selection: location..location + length,
        }
    }

    fn format(text: &str, context: &InsertionContext) -> String {
        text_for_insertion(text, Some(context), true, true)
    }

    fn without_context(text: &str) -> String {
        text_for_insertion(text, None, true, true)
    }

    #[test]
    fn does_not_add_trailing_space_to_non_empty_text() {
        assert_eq!(without_context("Hello"), "Hello");
    }

    #[test]
    fn leaves_existing_trailing_space_untouched() {
        assert_eq!(without_context("Hello "), "Hello ");
    }

    #[test]
    fn leaves_existing_trailing_newline_untouched() {
        assert_eq!(without_context("Hello\n"), "Hello\n");
    }

    #[test]
    fn leaves_empty_text_untouched() {
        assert_eq!(without_context(""), "");
    }

    #[test]
    fn disabled_contextual_insertion_does_not_add_trailing_space() {
        let context = context("coffeemachine", 6, 0);
        assert_eq!(
            text_for_insertion("Strong.", Some(&context), false, true),
            "Strong."
        );
    }

    #[test]
    fn missing_context_does_not_add_trailing_space() {
        assert_eq!(without_context("Strong."), "Strong.");
    }

    #[test]
    fn adds_missing_leading_and_trailing_spaces_between_words() {
        assert_eq!(
            format("strong", &context("coffeemachine", 6, 0)),
            " strong "
        );
    }

    #[test]
    fn does_not_add_spaces_between_cjk_characters() {
        for (script, value, insertion) in [
            ("Han", "你好", "世"),
            ("Kana", "あい", "カ"),
            ("Hangul", "가나", "다"),
        ] {
            assert_eq!(
                format(insertion, &context(value, 1, 0)),
                insertion,
                "{script}"
            );
        }
    }

    #[test]
    fn does_not_add_spaces_around_shared_kana_marks() {
        for (name, value, insertion) in [
            ("Prolonged sound mark", "カー", "ド"),
            ("Halfwidth voiced sound mark", "ｶｷ", "ﾞ"),
        ] {
            assert_eq!(
                format(insertion, &context(value, 1, 0)),
                insertion,
                "{name}"
            );
        }
    }

    #[test]
    fn keeps_space_between_decomposed_latin_characters() {
        // U+0323 carries scx=Han; a decomposed Latin letter must still count as Latin.
        assert_eq!(
            format("o\u{0323}c", &context("Ha\u{0323}", 3, 0)),
            " o\u{0323}c"
        );
    }

    #[test]
    fn preserves_spaces_at_mixed_latin_cjk_boundaries() {
        assert_eq!(format("中", &context("AB", 1, 0)), " 中 ");
    }

    #[test]
    fn avoids_duplicate_leading_space() {
        assert_eq!(
            format("strong", &context("coffee machine", 7, 0)),
            "strong "
        );
    }

    #[test]
    fn does_not_add_space_before_punctuation() {
        assert_eq!(format("friend", &context("Hello,", 5, 0)), " friend");
    }

    #[test]
    fn strips_final_period_before_existing_comma() {
        assert_eq!(
            format(
                "Dictation in the middle of a sentence before the comma.",
                &context("start, I will begin", 5, 0)
            ),
            " dictation in the middle of a sentence before the comma"
        );
    }

    #[test]
    fn strips_final_period_before_existing_period() {
        assert_eq!(
            format("my first sentence.", &context("dictating.", 9, 0)),
            " my first sentence"
        );
    }

    #[test]
    fn does_not_lowercase_after_sentence_ending_punctuation() {
        assert_eq!(
            format("Another item", &context("Done.Next", 5, 0)),
            " Another item "
        );
    }

    #[test]
    fn lowercases_titlecase_first_word_in_mid_sentence() {
        assert_eq!(
            format(
                "Presented tomorrow",
                &context("The presentation will bemachine", 24, 0)
            ),
            " presented tomorrow "
        );
    }

    #[test]
    fn lowercases_and_strips_period_after_existing_word_separated_by_space() {
        assert_eq!(
            format("Immediately.", &context("will begin", 5, 0)),
            "immediately "
        );
    }

    #[test]
    fn trims_dictated_boundary_whitespace_before_punctuation() {
        assert_eq!(
            format(" my first sentence ", &context("dictating.", 9, 0)),
            " my first sentence"
        );
    }

    #[test]
    fn preserves_all_caps_first_word() {
        assert_eq!(
            format("NASA tools.", &context("we use", 6, 0)),
            " NASA tools."
        );
    }

    #[test]
    fn preserves_camel_case_first_word() {
        assert_eq!(
            format("TypeWhisper", &context("about", 5, 0)),
            " TypeWhisper"
        );
    }

    #[test]
    fn strips_single_final_period_before_existing_word() {
        assert_eq!(
            format("Strong.", &context("coffeemachine", 6, 0)),
            " strong "
        );
    }

    #[test]
    fn preserves_question_punctuation() {
        assert_eq!(
            format("Really?", &context("coffeemachine", 6, 0)),
            " really? "
        );
    }

    // Standalone value final-period cleanup.

    fn assert_standalone(input: &str, expected: &str) {
        assert_standalone_with(input, expected, true);
    }

    fn assert_standalone_with(input: &str, expected: &str, enabled: bool) {
        assert_eq!(
            text_for_insertion(input, Some(&context("", 0, 0)), true, enabled),
            expected,
            "{input}"
        );
    }

    #[test]
    fn standalone_strips_final_period_from_email() {
        assert_standalone("name@example.com.", "name@example.com");
    }

    #[test]
    fn standalone_strips_final_period_from_urls() {
        assert_standalone("https://example.com.", "https://example.com");
        assert_standalone("www.example.com.", "www.example.com");
        assert_standalone("example.com.", "example.com");
    }

    #[test]
    fn standalone_preserves_terminal_period_in_url_paths_and_queries() {
        assert_standalone("https://example.com/docs.", "https://example.com/docs.");
        assert_standalone(
            "https://example.com/files/report.",
            "https://example.com/files/report.",
        );
        assert_standalone(
            "https://example.com/search?q=Dr.",
            "https://example.com/search?q=Dr.",
        );
    }

    #[test]
    fn standalone_preserves_spaced_dates() {
        assert_standalone("27. 09. 2026.", "27. 09. 2026.");
        assert_standalone("27 / 09 / 2026.", "27 / 09 / 2026.");
        assert_standalone("2026 - 09 - 27.", "2026 - 09 - 27.");
    }

    #[test]
    fn standalone_strips_final_period_from_decimal_numbers() {
        assert_standalone("3.14.", "3.14");
        assert_standalone("1,5.", "1,5");
        assert_standalone("1,000.50.", "1,000.50");
        assert_standalone("1.000,50.", "1.000,50");
    }

    #[test]
    fn standalone_strips_final_period_from_phone_numbers() {
        assert_standalone("+49 171 2345678.", "+49 171 2345678");
        assert_standalone("(030) 123456.", "(030) 123456");
    }

    #[test]
    fn standalone_strips_final_period_from_version_strings() {
        assert_standalone("1.2.3.", "1.2.3");
        assert_standalone("v2.10.4.", "v2.10.4");
    }

    #[test]
    fn standalone_preserves_abbreviations() {
        for abbreviation in ["Dr.", "U.S.", "e.g.", "Dr.med.", "Ph.D."] {
            assert_standalone(abbreviation, abbreviation);
        }
    }

    #[test]
    fn standalone_preserves_prose_and_sentences_ending_in_values() {
        assert_standalone("Hello world.", "Hello world.");
        assert_standalone(
            "Contact me at name@example.com.",
            "Contact me at name@example.com.",
        );
    }

    #[test]
    fn standalone_preserves_ambiguous_numeric_forms() {
        assert_standalone("19.04.2026.", "19.04.2026.");
        assert_standalone("2026-09-27.", "2026-09-27.");
        assert_standalone("123.", "123.");
    }

    #[test]
    fn standalone_preserves_ellipses_and_other_punctuation() {
        assert_standalone("Wait...", "Wait...");
        assert_standalone("Really?", "Really?");
    }

    #[test]
    fn standalone_respects_opt_out() {
        assert_standalone_with("name@example.com.", "name@example.com.", false);
    }

    #[test]
    fn standalone_does_not_apply_at_end_of_existing_sentence() {
        assert_eq!(
            format("name@example.com.", &context("Email: ", 7, 0)),
            "name@example.com."
        );
    }

    #[test]
    fn standalone_does_not_double_strip_mid_sentence() {
        assert_eq!(
            format("name@example.com.", &context("ab", 1, 0)),
            " name@example.com "
        );
    }

    // Linux additions: AT-SPI reports offsets in characters, and a selection
    // is replaced by the paste.

    #[test]
    fn counts_offsets_in_characters() {
        assert_eq!(format("Größe.", &context("Die Maße", 4, 0)), "größe ");
    }

    #[test]
    fn looks_at_the_characters_around_a_selection() {
        assert_eq!(format("Quick.", &context("a slow fox", 2, 4)), "quick");
    }

    #[test]
    fn a_selection_outside_the_text_gives_no_formatting() {
        assert_eq!(format("Hello.", &context("ab", 1, 5)), "Hello.");
    }

    #[test]
    fn an_empty_field_keeps_capitalisation_and_gets_no_spaces() {
        assert_eq!(format(" Hello world. ", &context("", 0, 0)), "Hello world.");
    }
}
