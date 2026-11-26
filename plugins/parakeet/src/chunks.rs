//! Splits long recordings for Parakeet and joins the transcripts again, ported
//! from the Windows app's sherpa-onnx plugin (`CreateParakeetChunks` and
//! `MergeChunkTranscripts` in `SherpaOnnxPlugin.cs`), so that both apps handle
//! long dictations alike.
//!
//! The exported model fails on about ten minutes of audio in one pass, and its
//! memory grows with the length. So a recording longer than 300 s is split into
//! chunks of at most 300 s that overlap by 5 s, and the words that the overlap
//! repeats are dropped when the transcripts are joined.

use std::ops::Range;

pub const SAMPLE_RATE: usize = 16_000;
const MAXIMUM_CHUNK_SECONDS: usize = 300;
const OVERLAP_SECONDS: usize = 5;
/// Words at the end of one transcript that must match the start of the next
/// before they count as the overlap.
const MINIMUM_OVERLAP_WORDS: usize = 2;
const MAXIMUM_OVERLAP_WORDS: usize = 40;

/// Transcribes `samples` with `recognize`, in chunks when the recording is long.
pub fn transcribe(samples: &[f32], mut recognize: impl FnMut(&[f32]) -> String) -> String {
    let ranges = chunks(samples.len());
    if ranges.len() > 1 {
        eprintln!(
            "Splitting {:.1} s of audio into {} chunks.",
            samples.len() as f64 / SAMPLE_RATE as f64,
            ranges.len()
        );
    }
    let mut transcript = String::new();
    for range in ranges {
        transcript = merge(&transcript, &recognize(&samples[range]));
    }
    transcript
}

/// Sample ranges of at most 300 s that overlap by 5 s and share the length
/// evenly, so that the last chunk is not much shorter than the others.
fn chunks(sample_count: usize) -> Vec<Range<usize>> {
    let maximum = MAXIMUM_CHUNK_SECONDS * SAMPLE_RATE;
    if sample_count <= maximum {
        return vec![Range {
            start: 0,
            end: sample_count,
        }];
    }
    let overlap = OVERLAP_SECONDS * SAMPLE_RATE;
    let stride = maximum - overlap;
    let count = (sample_count - overlap).div_ceil(stride);
    let processed = sample_count + (count - 1) * overlap;
    let base = processed / count;
    let longer = processed % count;
    let mut ranges = Vec::with_capacity(count);
    let mut offset = 0;
    for index in 0..count {
        let length = base + usize::from(index < longer);
        ranges.push(offset..offset + length);
        offset += length - overlap;
    }
    ranges
}

/// Appends `next` to `transcript`. When the start of `next` repeats the end of
/// `transcript`, as the overlapping audio makes it, the repeated words go.
fn merge(transcript: &str, next: &str) -> String {
    let transcript = transcript.trim();
    let next = next.trim();
    if transcript.is_empty() {
        return next.to_owned();
    }
    if next.is_empty() {
        return transcript.to_owned();
    }
    let ending = words(transcript);
    let starting = words(next);
    let maximum = MAXIMUM_OVERLAP_WORDS.min(ending.len()).min(starting.len());
    for overlap in (MINIMUM_OVERLAP_WORDS..=maximum).rev() {
        let start = ending.len() - overlap;
        if (0..overlap).all(|i| ending[start + i].normalized == starting[i].normalized) {
            return append(transcript, &next[starting[overlap - 1].end..]);
        }
    }
    append(transcript, next)
}

struct Word {
    /// Upper case, with the typographic apostrophe as `'`.
    normalized: String,
    /// Byte offset just after the word.
    end: usize,
}

/// Runs of letters and digits, joined by an apostrophe between them, as the
/// Windows plugin's `[\p{L}\p{N}]+(?:['’][\p{L}\p{N}]+)*`.
fn words(text: &str) -> Vec<Word> {
    let mut found = Vec::new();
    let mut characters = text.char_indices().peekable();
    while let Some((start, character)) = characters.next() {
        if !character.is_alphanumeric() {
            continue;
        }
        let mut end = start + character.len_utf8();
        while let Some(&(index, next)) = characters.peek() {
            if next.is_alphanumeric() {
                end = index + next.len_utf8();
                characters.next();
            } else if (next == '\'' || next == '’')
                && text[index + next.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric)
            {
                characters.next();
            } else {
                break;
            }
        }
        found.push(Word {
            normalized: text[start..end].replace('’', "'").to_uppercase(),
            end,
        });
    }
    found
}

/// Joins with a space, or without one before closing punctuation, which then
/// replaces the punctuation at the end of `transcript`.
fn append(transcript: &str, tail: &str) -> String {
    let tail = tail.trim_start();
    let Some(first) = tail.chars().next() else {
        return transcript.to_owned();
    };
    let quote_before_text = (first == '"' || first == '\'')
        && tail[first.len_utf8()..]
            .chars()
            .next()
            .is_some_and(|next| !next.is_whitespace());
    if is_punctuation(first) && !is_opening(first) && !quote_before_text {
        let kept = transcript.trim_end_matches(is_punctuation);
        return format!("{kept}{tail}");
    }
    format!("{transcript} {tail}")
}

/// Unicode punctuation (categories P*), for the characters a transcript has.
fn is_punctuation(character: char) -> bool {
    matches!(
        character,
        '!' | '"'
            | '#'
            | '%'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | ','
            | '-'
            | '.'
            | '/'
            | ':'
            | ';'
            | '?'
            | '@'
            | '['
            | '\\'
            | ']'
            | '_'
            | '{'
            | '}'
            | '¡'
            | '§'
            | '«'
            | '¶'
            | '·'
            | '»'
            | '¿'
            | '‐'..='‧'
            | '‹'
            | '›'
            | '、'..='〃'
            | '〈'..='】'
            | '！'
            | '（'
            | '）'
            | '，'
            | '．'
            | '：'
            | '；'
            | '？'
    )
}

/// Opening brackets and opening quotes (categories Ps and Pi).
fn is_opening(character: char) -> bool {
    matches!(
        character,
        '(' | '['
            | '{'
            | '«'
            | '‘'
            | '‚'
            | '‛'
            | '“'
            | '„'
            | '‟'
            | '‹'
            | '〈'
            | '《'
            | '「'
            | '『'
            | '【'
            | '（'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: usize = SAMPLE_RATE;

    #[test]
    fn short_recordings_stay_whole() {
        let whole = |end| vec![Range { start: 0, end }];
        assert_eq!(chunks(0), whole(0));
        assert_eq!(chunks(300 * SECOND), whole(300 * SECOND));
    }

    #[test]
    fn long_recordings_split_into_even_overlapping_chunks() {
        for seconds in [301, 360, 600, 1200, 3601] {
            let total = seconds * SECOND;
            let ranges = chunks(total);
            assert!(ranges.len() > 1, "{seconds} s");
            assert_eq!(ranges[0].start, 0);
            assert_eq!(ranges.last().unwrap().end, total);
            for range in &ranges {
                assert!(range.len() <= MAXIMUM_CHUNK_SECONDS * SECOND, "{seconds} s");
            }
            for pair in ranges.windows(2) {
                assert_eq!(pair[0].end - pair[1].start, OVERLAP_SECONDS * SECOND);
                assert!(pair[0].len().abs_diff(pair[1].len()) <= 1);
            }
        }
        // 360 s: two chunks of 182.5 s that share 5 s.
        assert_eq!(chunks(360 * SECOND), [0..2_920_000, 2_840_000..5_760_000]);
    }

    #[test]
    fn transcribe_hands_each_chunk_to_the_recognizer() {
        let samples = vec![0.0; 360 * SECOND];
        let mut lengths = Vec::new();
        let text = transcribe(&samples, |chunk| {
            lengths.push(chunk.len());
            if lengths.len() == 1 {
                "We went to the market on Monday".into()
            } else {
                "on Monday and bought apples.".into()
            }
        });
        assert_eq!(lengths, [2_920_000, 2_920_000]);
        assert_eq!(text, "We went to the market on Monday and bought apples.");
    }

    #[test]
    fn overlap_words_are_dropped_once() {
        assert_eq!(
            merge("one two three four", "three four five"),
            "one two three four five"
        );
        // Case and the apostrophe form do not matter.
        assert_eq!(
            merge("I think it's done", "IT’S DONE now"),
            "I think it's done now"
        );
        // A single repeated word is not taken for the overlap.
        assert_eq!(merge("we waited", "waited again"), "we waited waited again");
    }

    #[test]
    fn closing_punctuation_replaces_the_punctuation_before_it() {
        assert_eq!(merge("Hello there.", ", said she"), "Hello there, said she");
        assert_eq!(merge("Hello there", "(aside)"), "Hello there (aside)");
        assert_eq!(merge("He said", "\"yes\" twice"), "He said \"yes\" twice");
    }

    #[test]
    fn empty_sides_leave_the_other_alone() {
        assert_eq!(merge("", "  text "), "text");
        assert_eq!(merge(" text ", ""), "text");
    }

    #[test]
    fn words_keep_inner_apostrophes() {
        let found: Vec<String> = words("don't stop, rock’n’roll 2026!")
            .into_iter()
            .map(|word| word.normalized)
            .collect();
        assert_eq!(found, ["DON'T", "STOP", "ROCK'N'ROLL", "2026"]);
    }
}
