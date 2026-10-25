//! English text as a speech engine should hear it ([`speakable`]).
//!
//! Speech models often stumble over figures: "194" may come out digit by digit or not at all,
//! and "0.6%–0.9%" loses its dash. Ranges between numbers become "to", and, when asked, numbers
//! become words ("one hundred ninety-four", "zero point six percent", "twenty-first", years after
//! "in" or "since" as "nineteen ninety-four"). The wording is English, so this is only for text
//! read in English. Abbreviations such as "Fig." and "e.g." are read in full
//! ([`expand_abbreviations`]).

/// The most characters [`speakable`] returns; past this the text goes out unchanged, which a
/// speech server (capped at a few thousand characters) still takes.
pub const MAX_SPOKEN_CHARS: usize = 2_800;

/// Integers with more digits than this are read digit by digit.
const MAX_CARDINAL_DIGITS: usize = 15;

const ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];
const SCALES: [&str; 5] = ["", "thousand", "million", "billion", "trillion"];

/// Words after which a four-digit number is a year.
const YEAR_CUES: [&str; 14] = ["in", "since", "by", "from", "until", "till", "during", "year", "around", "circa", "before", "after", "early", "late"];

/// `text` with ranges between numbers ("0.6%–0.9%", "10 - 20" with an en dash) read as "to"
/// and, with `spell_numbers`, numbers as words. Numbers glued to letters on their left ("x2",
/// "GPT3") stay as they are.
pub fn speakable(text: &str, spell_numbers: bool, format: NumberFormat) -> String {
    let mut out = String::with_capacity(text.len().saturating_mul(2));
    let mut at = 0;
    // Set right after a range dash: the number that follows reads like the one before it.
    let mut range_year = None;
    while let Some(c) = text.get(at..).and_then(|rest| rest.chars().next()) {
        // ".897": a decimal point with no digit before it.
        let bare_point =
            spell_numbers && c == format.decimal() && text.get(at + 1..).is_some_and(|r| r.starts_with(|d: char| d.is_ascii_digit())) && {
                let before = text.get(..at).unwrap_or_default().chars().next_back();
                before.is_none_or(|b| b.is_whitespace() || matches!(b, '(' | '[' | '=' | '−' | '-' | '±' | '<' | '>' | '≈'))
            };
        if (c.is_ascii_digit() && !glued_left(text, at)) || bare_point {
            let n = number(text, at, format);
            let year = range_year.take().unwrap_or_else(|| year_cue(text, at));
            if spell_numbers {
                out.push_str(&n.words(year));
                if text.get(n.end..).and_then(|r| r.chars().next()).is_some_and(char::is_alphabetic) {
                    out.push(' ');
                }
            } else {
                out.push_str(text.get(at..n.end).unwrap_or_default());
            }
            at = n.end;
            if let Some(next) = range_dash(text, at) {
                out.push_str(" to ");
                range_year = Some(year);
                at = next;
            }
            continue;
        }
        if spell_numbers && is_minus(text, at, c) {
            out.push_str("minus ");
        } else {
            out.push(c);
        }
        at += c.len_utf8();
    }
    if out.chars().count() > MAX_SPOKEN_CHARS { text.to_string() } else { out }
}

/// Where an abbreviation may be expanded.
#[derive(Clone, Copy, PartialEq)]
enum Before {
    /// Anywhere.
    Any,
    /// Only before a number or an opening bracket ("p. 3", "App. (B)"): elsewhere the same
    /// letters are often a word ("No.").
    Number,
    /// Anywhere, and it may end a sentence: the full stop stays when one could end there.
    End,
}

/// Abbreviations read in full. Longer forms come before their prefixes ("Figs." before "Fig.").
const ABBREVIATIONS: [(&str, &str, Before); 52] = [
    ("e.g.", "for example", Before::Any),
    ("E.g.", "For example", Before::Any),
    ("i.e.", "that is", Before::Any),
    ("I.e.", "That is", Before::Any),
    ("w.r.t.", "with respect to", Before::Any),
    ("a.k.a.", "also known as", Before::Any),
    ("cf.", "compare", Before::Any),
    ("Cf.", "Compare", Before::Any),
    ("vs.", "versus", Before::Any),
    ("approx.", "approximately", Before::Any),
    ("resp.", "respectively", Before::Any),
    ("et al.", "and others", Before::End),
    ("etc.", "et cetera", Before::End),
    ("Figs.", "Figures", Before::Any),
    ("Fig.", "Figure", Before::Any),
    ("figs.", "figures", Before::Any),
    ("fig.", "figure", Before::Any),
    ("Secs.", "Sections", Before::Any),
    ("Sect.", "Section", Before::Any),
    ("Sec.", "Section", Before::Any),
    ("sec.", "section", Before::Any),
    ("Eqs.", "Equations", Before::Any),
    ("Eq.", "Equation", Before::Any),
    ("eqs.", "equations", Before::Any),
    ("eq.", "equation", Before::Any),
    ("Tabs.", "Tables", Before::Number),
    ("Tab.", "Table", Before::Number),
    ("Chap.", "Chapter", Before::Number),
    ("Ch.", "Chapter", Before::Number),
    ("Refs.", "References", Before::Number),
    ("Ref.", "Reference", Before::Number),
    ("App.", "Appendix", Before::Number),
    ("Thm.", "Theorem", Before::Any),
    ("Def.", "Definition", Before::Any),
    ("Lem.", "Lemma", Before::Any),
    ("Prop.", "Proposition", Before::Any),
    ("Alg.", "Algorithm", Before::Any),
    ("Cor.", "Corollary", Before::Any),
    ("Nos.", "numbers", Before::Number),
    ("No.", "number", Before::Number),
    ("no.", "number", Before::Number),
    ("pp.", "pages", Before::Number),
    ("p.", "page", Before::Number),
    ("Vols.", "Volumes", Before::Number),
    ("Vol.", "Volume", Before::Number),
    ("vol.", "volume", Before::Number),
    ("Eds.", "Editors", Before::Any),
    ("Ed.", "Editor", Before::Number),
    ("Dr.", "Doctor", Before::Any),
    ("Prof.", "Professor", Before::Any),
    ("Mr.", "Mister", Before::Any),
    ("Mrs.", "Missus", Before::Any),
];

/// `text` with common English abbreviations ("Fig.", "Sec.", "e.g.") read in full, "et al." as "and others".
/// Run it on a whole paragraph before it is cut into sentences: the expansion has no full stop
/// to end a sentence at.
pub fn expand_abbreviations(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(text.len() / 4));
    let mut at = 0;
    while let Some(c) = text.get(at..).and_then(|rest| rest.chars().next()) {
        let word_start =
            !text.get(..at).unwrap_or_default().chars().next_back().is_some_and(|b| b.is_alphanumeric() || matches!(b, '.' | '_' | '\''));
        if let Some((end, letters)) = word_start.then(|| acronym(text, at)).flatten() {
            out.push_str(&letters);
            at = end;
            continue;
        }
        let found = word_start.then(|| abbreviation(text, at)).flatten();
        let Some((short, long, before)) = found else {
            out.push(c);
            at += c.len_utf8();
            continue;
        };
        at += short.len();
        out.push_str(long);
        let rest = text.get(at..).unwrap_or_default();
        if before == Before::End && rest.trim_start().chars().next().is_none_or(char::is_uppercase) {
            out.push('.');
        } else if rest.starts_with(|n: char| n.is_alphanumeric()) {
            out.push(' ');
        }
    }
    out
}

/// The acronym at `at` spelled letter by letter ("VCS" → "V C S", "RQ3" → "R Q 3", "APIs" →
/// "A P I's"), and where it ends. Only acronyms a reader would spell: two or three capitals, or
/// longer ones without a vowel. "NASA" or "BLEU" are said as words and Roman numerals ("IV") as
/// numbers, so those stay.
fn acronym(text: &str, at: usize) -> Option<(usize, String)> {
    let rest = text.get(at..)?;
    let caps = rest.find(|c: char| !c.is_ascii_uppercase()).unwrap_or(rest.len());
    let letters = rest.get(..caps)?;
    let vowels = letters.contains(['A', 'E', 'I', 'O', 'U', 'Y']);
    let roman = letters.chars().all(|c| matches!(c, 'I' | 'V' | 'X'));
    if !(2..=8).contains(&caps) || (caps > 3 && vowels) || roman {
        return None;
    }
    let after = rest.get(caps..).unwrap_or_default();
    let digits = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
    let plural = digits == 0 && after.starts_with('s') && !after.get(1..).is_some_and(|r| r.starts_with(char::is_alphanumeric));
    let tail = after.get(digits + usize::from(plural)..).unwrap_or_default();
    // "VCSmith" or "ABCd" is a word, not an acronym.
    if tail.starts_with(char::is_alphanumeric) {
        return None;
    }
    let mut spelled = letters.chars().map(String::from).collect::<Vec<_>>().join(" ");
    if plural {
        spelled.push_str("'s");
    } else if digits > 0 {
        spelled.push(' ');
        spelled.push_str(after.get(..digits).unwrap_or_default());
    }
    Some((text.len() - tail.len(), spelled))
}

/// The abbreviation at `at`, if one is there and what follows allows it.
fn abbreviation(text: &str, at: usize) -> Option<(&'static str, &'static str, Before)> {
    let rest = text.get(at..)?;
    ABBREVIATIONS.iter().copied().find(|&(short, _, before)| {
        let Some(after) = rest.strip_prefix(short) else { return false };
        // "Fig.3" is fine; "p.m" or "No.Of" is something else.
        if after.starts_with(char::is_alphabetic) {
            return false;
        }
        before != Before::Number || after.trim_start_matches([' ', '\u{a0}', '\u{202f}']).starts_with(|n: char| n.is_ascii_digit() || n == '(')
    })
}

/// How numbers are written: which character groups thousands and which starts the decimals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NumberFormat {
    /// 1,234.5
    #[default]
    DecimalPoint,
    /// 1.234,5
    DecimalComma,
}

impl NumberFormat {
    fn decimal(self) -> char {
        match self {
            Self::DecimalPoint => '.',
            Self::DecimalComma => ',',
        }
    }

    fn group(self) -> char {
        match self {
            Self::DecimalPoint => ',',
            Self::DecimalComma => '.',
        }
    }
}

/// A number in the text: its digits and what follows them.
#[derive(Debug, PartialEq)]
struct Number<'a> {
    /// The integer part's digits, thousands separators removed.
    int: String,
    /// The digits after each decimal point.
    fractions: Vec<&'a str>,
    percent: bool,
    ordinal: bool,
    /// The byte offset just past the number and its `%` or ordinal suffix.
    end: usize,
}

impl Number<'_> {
    fn words(&self, year: bool) -> String {
        let int = if self.int.is_empty() {
            String::new()
        } else if (self.int.len() > 1 && self.int.starts_with('0')) || self.int.len() > MAX_CARDINAL_DIGITS {
            digits(&self.int)
        } else {
            let n = self.int.parse::<u64>().unwrap_or(0);
            match (year && self.fractions.is_empty() && !self.percent && !self.ordinal).then(|| year_words(n)).flatten() {
                Some(y) => y,
                None => cardinal(n),
            }
        };
        let mut words = if self.ordinal && self.fractions.is_empty() { ordinal(&int) } else { int };
        for fraction in &self.fractions {
            words.push_str(" point ");
            words.push_str(&digits(fraction));
        }
        if self.percent {
            words.push_str(" percent");
        }
        words.trim_start().to_string()
    }
}

/// The number starting at `start` (an ASCII digit, or a decimal point before digits): digits
/// with optional thousands separators ("1,000,000", or a no-break or thin space), decimal parts
/// ("3.14", "1.2.3", ".897"), then `%` (after at most one space) or an ordinal suffix ("21st").
fn number(text: &str, start: usize, format: NumberFormat) -> Number<'_> {
    let b = text.as_bytes();
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let run = |mut i: usize| {
        while digit(i) {
            i += 1;
        }
        i
    };
    let mut end = run(start);
    let mut int = text.get(start..end).unwrap_or_default().to_string();
    // Thousands groups: a separator and exactly three digits, after a first group of one to three.
    if (1..=3).contains(&(end - start)) {
        loop {
            let sep =
                text.get(end..).and_then(|r| r.chars().next()).filter(|&c| c == format.group() || matches!(c, '\u{a0}' | '\u{202f}' | '\u{2009}'));
            let Some(sep) = sep else { break };
            let from = end + sep.len_utf8();
            if run(from) != from + 3 {
                break;
            }
            int.push_str(text.get(from..from + 3).unwrap_or_default());
            end = from + 3;
        }
    }
    let point = format.decimal() as u8;
    let mut fractions = Vec::new();
    while b.get(end) == Some(&point) && digit(end + 1) {
        let next = run(end + 1);
        fractions.push(text.get(end + 1..next).unwrap_or_default());
        end = next;
    }
    let rest = text.get(end..).unwrap_or_default();
    let space = rest.chars().next().filter(|&c| matches!(c, ' ' | '\u{a0}' | '\u{202f}')).map_or(0, char::len_utf8);
    let (percent, ordinal) = (rest.get(space..).is_some_and(|r| r.starts_with('%')), fractions.is_empty() && is_ordinal_suffix(rest));
    if percent {
        end += space + 1;
    } else if ordinal {
        end += 2;
    }
    Number { int, fractions, percent, ordinal, end }
}

/// `st`, `nd`, `rd` or `th` not followed by another letter or digit.
fn is_ordinal_suffix(rest: &str) -> bool {
    let lower = rest.get(..2).map(str::to_ascii_lowercase);
    matches!(lower.as_deref(), Some("st" | "nd" | "rd" | "th")) && !rest.get(2..).and_then(|r| r.chars().next()).is_some_and(char::is_alphanumeric)
}

/// Whether the character before `at` makes the digits there part of a word ("x2", "a_1").
fn glued_left(text: &str, at: usize) -> bool {
    text.get(..at).unwrap_or_default().chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// Where the number after a range dash starts, if one follows `at`: an en or em dash with
/// optional spaces around it, or a hyphen with none ("3-4", but not "2020-01-15").
fn range_dash(text: &str, at: usize) -> Option<usize> {
    let rest = text.get(at..)?;
    let trimmed = rest.trim_start_matches(' ');
    let dash = trimmed.chars().next()?;
    let spaced = trimmed.len() < rest.len();
    let after = trimmed.get(dash.len_utf8()..)?;
    let next = match dash {
        '–' | '—' => after.trim_start_matches(' '),
        '-' | '‐' | '‑' if !spaced => after,
        _ => return None,
    };
    let start = text.len() - next.len();
    if !next.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    if dash != '–' && dash != '—' {
        // A hyphen chain is a date, a phone number or an identifier, not a range.
        let before_hyphen = text.get(..at).unwrap_or_default().trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ',');
        let after_right = text.get(number(text, start, NumberFormat::DecimalPoint).end..).unwrap_or_default();
        if before_hyphen.ends_with(['-', '‐', '‑']) || after_right.starts_with(['-', '‐', '‑']) {
            return None;
        }
    }
    Some(start)
}

/// A minus sign before a number: U+2212, or a hyphen right before the digits with a space,
/// an opening bracket or nothing before it.
fn is_minus(text: &str, at: usize, c: char) -> bool {
    let next = at + c.len_utf8();
    let digit_follows = text.get(next..).is_some_and(|r| r.starts_with(|d: char| d.is_ascii_digit()));
    let before = text.get(..at).unwrap_or_default().chars().next_back();
    digit_follows && (c == '−' || c == '-' && before.is_none_or(|b| b.is_whitespace() || matches!(b, '(' | '[' | '=')))
}

/// Whether the word before `at` says a year follows ("in 1994").
fn year_cue(text: &str, at: usize) -> bool {
    let word = text.get(..at).unwrap_or_default().split_whitespace().next_back().unwrap_or_default();
    let word = word.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase();
    YEAR_CUES.contains(&word.as_str())
}

/// `n` as a year ("nineteen ninety-four", "nineteen oh five", "two thousand five"), for years
/// 1100 to 2099.
fn year_words(n: u64) -> Option<String> {
    if !(1100..=2099).contains(&n) {
        return None;
    }
    if (2000..2010).contains(&n) {
        return Some(cardinal(n));
    }
    let (high, low) = (cardinal(n / 100), n % 100);
    Some(match low {
        0 => format!("{high} hundred"),
        1..=9 => format!("{high} oh {}", cardinal(low)),
        _ => format!("{high} {}", cardinal(low)),
    })
}

/// `n` in words: "one hundred ninety-four", "two million three".
fn cardinal(n: u64) -> String {
    if n == 0 {
        return ONES[0].to_string();
    }
    let mut groups = Vec::new();
    let mut rest = n;
    while rest > 0 {
        groups.push(rest % 1000);
        rest /= 1000;
    }
    let mut words: Vec<String> = Vec::new();
    for (scale, &group) in groups.iter().enumerate().rev() {
        if group == 0 {
            continue;
        }
        words.push(below_thousand(group));
        // Integers above 15 digits are read digit by digit, so the scale is in range.
        if let Some(name) = SCALES.get(scale).filter(|s| !s.is_empty()) {
            words.push((*name).to_string());
        }
    }
    words.join(" ")
}

/// 1 to 999 in words.
fn below_thousand(n: u64) -> String {
    let (hundreds, rest) = (n / 100, n % 100);
    let mut words = Vec::new();
    if hundreds > 0 {
        words.push(format!("{} hundred", word(hundreds)));
    }
    if rest >= 20 {
        let tens = TENS.get(rest as usize / 10).copied().unwrap_or_default();
        words.push(if rest % 10 == 0 { tens.to_string() } else { format!("{tens}-{}", word(rest % 10)) });
    } else if rest > 0 {
        words.push(word(rest).to_string());
    }
    words.join(" ")
}

fn word(n: u64) -> &'static str {
    usize::try_from(n).ok().and_then(|n| ONES.get(n)).copied().unwrap_or_default()
}

/// Each digit as a word: "zero zero seven".
fn digits(s: &str) -> String {
    s.chars().filter_map(|c| c.to_digit(10)).map(|d| word(u64::from(d))).collect::<Vec<_>>().join(" ")
}

/// A cardinal's last word made ordinal: "twenty-one" → "twenty-first", "twelve" → "twelfth".
fn ordinal(cardinal: &str) -> String {
    let cut = cardinal.rfind([' ', '-']).map_or(0, |i| i + 1);
    let (head, last) = cardinal.split_at_checked(cut).unwrap_or(("", cardinal));
    let last = match last {
        "one" => "first".to_string(),
        "two" => "second".to_string(),
        "three" => "third".to_string(),
        "five" => "fifth".to_string(),
        "eight" => "eighth".to_string(),
        "nine" => "ninth".to_string(),
        "twelve" => "twelfth".to_string(),
        l if l.ends_with('y') => format!("{}ieth", l.trim_end_matches('y')),
        l => format!("{l}th"),
    };
    format!("{head}{last}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: NumberFormat = NumberFormat::DecimalPoint;

    #[test]
    fn ranges_read_as_to_without_spelling() {
        assert_eq!(speakable("rose 0.6%–0.9% in May", false, F), "rose 0.6% to 0.9% in May");
        assert_eq!(speakable("pages 10 – 20 and 3-4", false, F), "pages 10 to 20 and 3 to 4");
        assert_eq!(speakable("on 2020-01-15, call 555-1234-99", false, F), "on 2020-01-15, call 555-1234-99");
        assert_eq!(speakable("COVID-19 and well-known", false, F), "COVID-19 and well-known");
        assert_eq!(speakable("194 and −3", false, F), "194 and −3");
    }

    #[test]
    fn numbers_become_words() {
        assert_eq!(speakable("about 194 layers", true, F), "about one hundred ninety-four layers");
        assert_eq!(speakable("rose 0.6%–0.9%.", true, F), "rose zero point six percent to zero point nine percent.");
        assert_eq!(speakable("1,000,000 steps and 2,048", true, F), "one million steps and two thousand forty-eight");
        assert_eq!(speakable("lists 1,2", true, F), "lists one,two");
        assert_eq!(speakable("pi is 3.14", true, F), "pi is three point one four");
        assert_eq!(speakable("the 21st and 12th and 100th", true, F), "the twenty-first and twelfth and one hundredth");
        assert_eq!(speakable("agent 007 at 5 %", true, F), "agent zero zero seven at five percent");
        assert_eq!(speakable("a 3D model, 10x faster", true, F), "a three D model, ten x faster");
        assert_eq!(speakable("x2 and GPT3 and a_1", true, F), "x2 and GPT3 and a_1");
        assert_eq!(speakable("it fell to -3 (−2.5)", true, F), "it fell to minus three (minus two point five)");
        assert_eq!(speakable("9 - 3", true, F), "nine - three");
        assert_eq!(speakable(&format!("1{} atoms", "0".repeat(18)), true, F), format!("one{} atoms", " zero".repeat(18)));
    }

    #[test]
    fn bare_decimals_groups_and_the_decimal_comma() {
        assert_eq!(speakable("an AUC of .897 (.91)", true, F), "an AUC of point eight nine seven (point nine one)");
        assert_eq!(
            speakable("7,986 projects and 7\u{202f}986", true, F),
            "seven thousand nine hundred eighty-six projects and seven thousand nine hundred eighty-six"
        );
        assert_eq!(speakable("end. 5 more", true, F), "end. five more");
        let comma = NumberFormat::DecimalComma;
        assert_eq!(
            speakable("1.234,5 and 0,6 and ,5", true, comma),
            "one thousand two hundred thirty-four point five and zero point six and point five"
        );
        assert_eq!(speakable("3.14", false, F), "3.14");
    }

    #[test]
    fn years_after_a_cue() {
        assert_eq!(speakable("in 1994 and since 2017", true, F), "in nineteen ninety-four and since twenty seventeen");
        assert_eq!(speakable("by 1905, in 2005, from 1900", true, F), "by nineteen oh five, in two thousand five, from nineteen hundred");
        assert_eq!(speakable("from 1990–2000", true, F), "from nineteen ninety to two thousand");
        assert_eq!(speakable("2048 tokens", true, F), "two thousand forty-eight tokens");
    }

    #[test]
    fn abbreviations_are_read_in_full() {
        assert_eq!(expand_abbreviations("See Fig. 3 and Sec. 2.1, e.g. here."), "See Figure 3 and Section 2.1, for example here.");
        assert_eq!(expand_abbreviations("Figs. 1–2 (cf. Eq. (4))"), "Figures 1–2 (compare Equation (4))");
        assert_eq!(
            expand_abbreviations("Vaswani et al. showed it. So did Smith et al. Then"),
            "Vaswani and others showed it. So did Smith and others. Then"
        );
        assert_eq!(expand_abbreviations("apples, pears etc."), "apples, pears et cetera.");
        assert_eq!(expand_abbreviations("No. 5 on p. 12, but No. way"), "number 5 on page 12, but No. way");
        assert_eq!(expand_abbreviations("Fig.3 in Config. and Tab. key"), "Figure 3 in Config. and Tab. key");
        assert_eq!(expand_abbreviations("ip. the e.g.x"), "ip. the e.g.x");
    }

    #[test]
    fn short_acronyms_are_spelled() {
        assert_eq!(expand_abbreviations("RQ3: does the VCS help? APIs, GPT3"), "R Q 3: does the V C S help? A P I's, G P T 3");
        assert_eq!(
            expand_abbreviations("NASA, BLEU, JSON, LaTeX, iOS, I, A, IV and XVI stay"),
            "NASA, BLEU, JSON, LaTeX, iOS, I, A, IV and XVI stay"
        );
        assert_eq!(expand_abbreviations("HTTPS and RQ-2 but ABCd"), "H T T P S and R Q-2 but ABCd");
        assert_eq!(speakable(&expand_abbreviations("RQ3 and SQL"), true, F), "R Q three and S Q L");
    }

    #[test]
    fn cardinals_and_ordinals() {
        assert_eq!(cardinal(0), "zero");
        assert_eq!(cardinal(40), "forty");
        assert_eq!(cardinal(1_000_003), "one million three");
        assert_eq!(
            cardinal(999_999_999_999_999),
            "nine hundred ninety-nine trillion nine hundred ninety-nine billion nine hundred ninety-nine million nine hundred ninety-nine thousand nine hundred ninety-nine"
        );
        assert_eq!(ordinal("twenty"), "twentieth");
        assert_eq!(ordinal("one hundred three"), "one hundred third");
    }

    #[test]
    fn any_text_is_safe() {
        let long = "9".repeat(5000);
        for s in ["", "-", "−", "1-", "1–", "1.", "1,", "1,00", "%", "1st", "١٢٣ 12", "ü1–2ü", "1 –", long.as_str()] {
            let _ = speakable(s, true, F);
            let _ = speakable(s, false, F);
        }
        assert_eq!(speakable(&"1 ".repeat(2000), true, F), "1 ".repeat(2000), "too long to spell goes out unchanged");
    }
}
