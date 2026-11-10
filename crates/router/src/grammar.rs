//! Deterministic keyword grammar for L0 command parsing (Path B).
//!
//! The parser is intentionally small and predictable: input is normalised
//! (lowercase, Polish diacritics removed, whitespace collapsed) and then
//! matched against an ordered keyword table. The first match wins; anything
//! that does not match is forwarded to the LLM as [`Action::QueryL1`].

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::Action;

/// Keywords that terminate a process, checked in order.
const KILL_KEYWORDS: &[&str] = &["zabij", "zamknij", "kill", "terminate"];
/// Keywords that suspend a process, checked in order.
const FREEZE_KEYWORDS: &[&str] = &["zamroz", "freeze", "stop", "wstrzymaj"];
/// Keywords that resume a process, checked in order.
const RESUME_KEYWORDS: &[&str] = &["wznow", "resume", "odblokuj", "odmroz"];
/// Explicit verb+object phrases that request page-cache cleanup.
const CLEAN_CACHE_PHRASES: &[&str] = &[
    "wyczysc cache",
    "wyczysc pamiec podreczna",
    "oproznij cache",
    "drop cache",
    "clean cache",
    "clear cache",
];

/// Parse raw input into an [`Action`].
///
/// Matching is case-insensitive, diacritic-insensitive and whitespace
/// tolerant. The process name of a kill/freeze/resume action is the remaining
/// text after the keyword; when nothing remains the input is not actionable on
/// its own and falls back to [`Action::QueryL1`] with the original text.
pub fn parse(input: &str) -> Action {
    let normalized = normalize(input);

    if let Some(rest) = rest_after_keyword(&normalized, KILL_KEYWORDS) {
        return with_process(rest, Action::Kill, input);
    }
    if let Some(rest) = rest_after_keyword(&normalized, FREEZE_KEYWORDS) {
        return with_process(rest, Action::Freeze, input);
    }
    if let Some(rest) = rest_after_keyword(&normalized, RESUME_KEYWORDS) {
        return with_process(rest, Action::Resume, input);
    }
    if CLEAN_CACHE_PHRASES.iter().any(|phrase| {
        normalized == *phrase
            || normalized
                .strip_prefix(phrase)
                .is_some_and(|rest| rest.starts_with(' '))
    }) {
        return Action::CleanCache;
    }

    Action::QueryL1(input.to_owned())
}

/// Lowercase, strip diacritics and collapse all whitespace runs to one space.
fn normalize(input: &str) -> String {
    let lowered = input.to_lowercase();
    strip_diacritics(&lowered)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Return the text following the first matching keyword, if any.
///
/// The keyword must be followed by whitespace or end-of-input to avoid
/// matching inside longer words (e.g. "stopwatch" must not match "stop").
fn rest_after_keyword<'a>(normalized: &'a str, keywords: &[&str]) -> Option<&'a str> {
    keywords.iter().find_map(|keyword| {
        let rest = normalized.strip_prefix(keyword)?;
        if rest.is_empty() || rest.starts_with(' ') {
            Some(rest)
        } else {
            None
        }
    })
}

/// Build a process action from the leftover text, or fall back to L1 when the
/// user named no process.
fn with_process(rest: &str, build: fn(String) -> Action, original: &str) -> Action {
    let name = rest.trim();
    if name.is_empty() {
        Action::QueryL1(original.to_owned())
    } else {
        build(name.to_owned())
    }
}

/// Replace Polish diacritics with their ASCII counterparts.
///
/// Most Polish letters decompose under NFD into a base letter plus a combining
/// mark, which is then filtered out. `ł`/`Ł` have no canonical decomposition,
/// so they are mapped explicitly before normalisation.
fn strip_diacritics(s: &str) -> String {
    let replaced: String = s
        .chars()
        .map(|c| match c {
            'ł' => 'l',
            'Ł' => 'L',
            other => other,
        })
        .collect();

    replaced.nfd().filter(|c| !is_combining_mark(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_diacritics_covers_full_polish_alphabet() {
        assert_eq!(
            strip_diacritics("ąćęłńóśźż ĄĆĘŁŃÓŚŹŻ"),
            "acelnoszz ACELNOSZZ"
        );
    }

    #[test]
    fn kills_polish_process() {
        assert_eq!(parse("zamknij firefox"), Action::Kill("firefox".to_owned()));
    }

    #[test]
    fn freezes_process_with_diacritics() {
        assert_eq!(parse("zamroź chrome"), Action::Freeze("chrome".to_owned()));
        assert_eq!(parse("zamróź chrome"), Action::Freeze("chrome".to_owned()));
    }

    #[test]
    fn resumes_polish_process() {
        assert_eq!(parse("wznów cargo"), Action::Resume("cargo".to_owned()));
    }

    #[test]
    fn alternative_polish_keywords() {
        assert_eq!(parse("zabij node"), Action::Kill("node".to_owned()));
        assert_eq!(parse("wstrzymaj node"), Action::Freeze("node".to_owned()));
        assert_eq!(parse("odblokuj node"), Action::Resume("node".to_owned()));
        assert_eq!(parse("odmroź node"), Action::Resume("node".to_owned()));
    }

    #[test]
    fn kills_english_process() {
        assert_eq!(parse("kill node"), Action::Kill("node".to_owned()));
    }

    #[test]
    fn freezes_english_process() {
        assert_eq!(parse("freeze rustc"), Action::Freeze("rustc".to_owned()));
    }

    #[test]
    fn resumes_english_process() {
        assert_eq!(
            parse("resume python3"),
            Action::Resume("python3".to_owned())
        );
    }

    #[test]
    fn cleans_cache_polish() {
        assert_eq!(parse("wyczyść cache"), Action::CleanCache);
        assert_eq!(parse("wyczysc pamiec podreczna"), Action::CleanCache);
    }

    #[test]
    fn cleans_cache_english() {
        assert_eq!(parse("drop cache"), Action::CleanCache);
        assert_eq!(parse("clean cache"), Action::CleanCache);
    }

    #[test]
    fn falls_back_to_l1_for_questions() {
        let input = "dlaczego kompilacja się wywala?";
        assert_eq!(parse(input), Action::QueryL1(input.to_owned()));
    }

    #[test]
    fn empty_input_falls_back_to_l1() {
        assert_eq!(parse(""), Action::QueryL1(String::new()));
        assert_eq!(parse("   \t\n "), Action::QueryL1("   \t\n ".to_owned()));
    }

    #[test]
    fn extra_whitespace_is_collapsed() {
        assert_eq!(parse("  kill    node  "), Action::Kill("node".to_owned()));
    }

    #[test]
    fn mixed_case_is_normalised() {
        assert_eq!(parse("KiLL NoDe"), Action::Kill("node".to_owned()));
        assert_eq!(
            parse("Zamknij  FireFox"),
            Action::Kill("firefox".to_owned())
        );
    }

    #[test]
    fn keyword_without_target_falls_back_to_l1() {
        assert_eq!(parse("zabij"), Action::QueryL1("zabij".to_owned()));
        assert_eq!(parse("kill   "), Action::QueryL1("kill   ".to_owned()));
    }

    #[test]
    fn first_matching_rule_wins() {
        // "stop" freezes before the cache rule can see "cache".
        assert_eq!(
            parse("stop cache hog"),
            Action::Freeze("cache hog".to_owned())
        );
    }

    #[test]
    fn process_name_keeps_inner_spacing() {
        assert_eq!(
            parse("kill steam  runtime"),
            Action::Kill("steam runtime".to_owned())
        );
    }

    #[test]
    fn keyword_requires_word_boundary() {
        assert_eq!(parse("stopwatch"), Action::QueryL1("stopwatch".to_owned()));
        assert_eq!(
            parse("killall firefox"),
            Action::QueryL1("killall firefox".to_owned())
        );
        assert_eq!(
            parse("resumed something"),
            Action::QueryL1("resumed something".to_owned())
        );
        assert_eq!(
            parse("freezer status"),
            Action::QueryL1("freezer status".to_owned())
        );
    }

    #[test]
    fn cache_rule_requires_explicit_phrase() {
        assert_eq!(
            parse("dlaczego cache rośnie?"),
            Action::QueryL1("dlaczego cache rośnie?".to_owned())
        );
        assert_eq!(
            parse("wyczyść kosz"),
            Action::QueryL1("wyczyść kosz".to_owned())
        );
        assert_eq!(
            parse("ile mam cache"),
            Action::QueryL1("ile mam cache".to_owned())
        );
        assert_eq!(
            parse("czemu cache rośnie?"),
            Action::QueryL1("czemu cache rośnie?".to_owned())
        );
    }

    #[test]
    fn cache_rule_accepts_new_phrases() {
        assert_eq!(parse("opróżnij cache"), Action::CleanCache);
        assert_eq!(parse("clear cache"), Action::CleanCache);
    }
}
