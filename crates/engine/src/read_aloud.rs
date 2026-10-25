//! View ▸ Read Out Loud: what is read, in what order, and in what pieces.
//!
//! A page is read paragraph by paragraph in reading order, as export to Word or HTML sees it
//! ([`pdfcraft_edit::reading_blocks`]), so text drawn by form XObjects is read too. Those blocks
//! break wherever the style changes (an italic word, a citation marker), so they are joined
//! again by the whitespace between them ([`merge`]): a run that continues the same row always
//! joins, and a row below joins unless the gap above it is clearly wider than the page's typical
//! line spacing. Optional [`Filters`] leave out page numbers and citations.
//!
//! Each paragraph goes to the speech engine in sentence-sized pieces ([`speech_chunks`]): short
//! pieces start sounding sooner, and a speech server caps how much it takes at once.
//!
//! Reading aloud never asks the copy permission: assistive technology may read the content
//! regardless (ISO 32000-2 §7.6.4.2), as [`Document::allows_copying`] notes.

use std::ops::Range;

mod english;
mod rules;
pub use english::{MAX_SPOKEN_CHARS, NumberFormat, expand_abbreviations as expand_english_abbreviations, speakable as speakable_english};
pub use rules::{MAX_RULES_BYTES, Rules};

use crate::Document;

/// The most characters one speech request carries. Sentences longer than this are cut at a
/// space (or, with no space, at a character boundary).
pub const MAX_CHUNK_CHARS: usize = 400;

/// Short sentences are joined into one request up to this many characters, so a run of
/// "Yes. No. Maybe." doesn't pause between every word.
const JOIN_CHARS: usize = 240;

/// The typical line gap (in font sizes) where a page has too few lines to measure one.
const DEFAULT_LINE_GAP: f64 = 0.25;

/// A paragraph to read.
#[derive(Clone, Debug, PartialEq)]
pub struct Passage {
    /// 0-based page.
    pub page: usize,
    /// Where it starts among the page's reading blocks ([`pdfcraft_edit::reading_blocks`]): the
    /// block's index times 256, plus the paragraph's place in a block that holds several. Not
    /// an Edit text index. Later passages on a page have larger ones.
    pub paragraph: usize,
    /// Its box in user space, for the highlight.
    pub rect: [f64; 4],
    /// Its text, with runs of whitespace (line ends, tabs) as single spaces.
    pub text: String,
    /// Its rows (and runs along a row) in reading order: their boxes in user space and how many
    /// characters each holds, for finding the sentence under the pointer ([`Passage::sentence_at`]).
    pub lines: Vec<([f64; 4], usize)>,
    /// The word boxes (user space, left to right) of each row in `lines`, where they are
    /// known; an empty list where they aren't. Estimates snap to them.
    pub ink: Vec<Vec<[f64; 4]>>,
    /// The text of each row in `lines`, as the page has it (before filters took citations or
    /// bullets out of `text`): `text`'s words are found on their rows through it.
    pub rows_text: Vec<String>,
    /// A heading the document's bookmarks (its table of contents) name on this page: the
    /// bookmark's depth, 1 for the top level.
    pub heading: Option<usize>,
    /// What it is: text, a figure's or table's caption, or text inside a figure or table.
    pub role: Role,
}

/// What a passage is, as far as its text and type tell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    Text,
    /// "Figure 3: …", "Table 2. …", "Algorithm 1 …".
    Caption,
    /// A label, a small-print fragment or a table row: short pieces without sentence
    /// punctuation, which is how text inside figures and tables reads.
    FigureText,
}

impl Passage {
    /// The byte offset in `text` where the sentence at `point` (user space) starts: the
    /// nearest row's characters are spread evenly across it to estimate the character there.
    /// 0 when the passage has no rows.
    pub fn sentence_at(&self, point: [f64; 2]) -> usize {
        let distance = |r: &[f64; 4]| {
            let dx = (r[0] - point[0]).max(point[0] - r[2]).max(0.0);
            let dy = (r[1] - point[1]).max(point[1] - r[3]).max(0.0);
            dy * 4.0 + dx
        };
        // The word nearest the point, among those found on their rows.
        let placed = self.placed();
        let nearest = placed
            .iter()
            .filter_map(|w| Some((distance(&[w.x0, self.lines.get(w.row)?.0[1], w.x1, self.lines.get(w.row)?.0[3]]), w.byte)))
            .filter(|(d, _)| d.is_finite())
            .min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((_, byte)) = nearest {
            return sentence_starts(&self.text).into_iter().take_while(|&s| s <= byte).last().unwrap_or(0);
        }
        let mut before = 0usize;
        let mut best: Option<(f64, f64)> = None;
        for (rect, chars) in &self.lines {
            let d = distance(rect);
            if d.is_finite() && best.is_none_or(|(bd, _)| d < bd) {
                let width = rect[2] - rect[0];
                let across = if width > 0.0 { ((point[0] - rect[0]) / width).clamp(0.0, 1.0) } else { 0.0 };
                best = Some((d, before as f64 + across * *chars as f64));
            }
            before = before.saturating_add(*chars);
        }
        let Some((_, at)) = best else { return 0 };
        // Filters may have taken characters out of the text since: scale to what is left.
        let fraction = if before > 0 { (at / before as f64).clamp(0.0, 1.0) } else { 0.0 };
        let target = (fraction * self.text.chars().count() as f64) as usize;
        let mut start = 0;
        for s in sentence_starts(&self.text) {
            if self.text.get(..s).map_or(usize::MAX, |t| t.chars().count()) > target {
                break;
            }
            start = s;
        }
        start
    }
}

impl Passage {
    /// The boxes (user space) the sentence starting at byte `start` of `text` covers, one per
    /// row it reaches, estimated like [`Passage::sentence_at`]: characters spread evenly along
    /// each row, then snapped to the nearest word edge where the row's words are known. The
    /// first box starts where the sentence does.
    pub fn sentence_boxes(&self, start: usize) -> Vec<[f64; 4]> {
        let starts = sentence_starts(&self.text);
        let end = starts.iter().copied().find(|&s| s > start).unwrap_or(self.text.len());
        self.range_boxes(start..end)
    }

    /// The boxes (user space) the bytes `range` of `text` cover, one per row they reach,
    /// estimated as [`Passage::sentence_boxes`] does.
    pub fn range_boxes(&self, range: Range<usize>) -> Vec<[f64; 4]> {
        let (start, end) = (range.start, range.end.min(self.text.len()));
        let placed = self.placed();
        let inside = |w: &&Placed| w.byte >= start && w.byte < end;
        if let (Some(first), Some(last)) = (placed.iter().find(inside), placed.iter().rev().find(inside))
            && first.row <= last.row
        {
            // The range runs on up to where the next word read starts, so what the text lost
            // after its last word (a citation, the full stop) is covered too; the last range
            // runs to the passage's end.
            let (end_row, end_x) = match placed.iter().find(|w| w.byte >= end) {
                Some(next) if next.row >= last.row => (next.row, Some(next.x0 - 2.0)),
                Some(_) => (last.row, Some(last.x1)),
                None => (self.lines.len().saturating_sub(1).max(last.row), None),
            };
            return (first.row..=end_row)
                .filter_map(|row| {
                    let rect = self.lines.get(row)?.0;
                    let x0 = if row == first.row { first.x0 } else { rect[0] };
                    let x1 = match end_x {
                        Some(x) if row == end_row => x,
                        _ => rect[2],
                    };
                    let x1 = if row == last.row { x1.max(last.x1) } else { x1 };
                    (x1 > x0).then_some([x0, rect[1], x1, rect[3]])
                })
                .collect();
        }
        let total_text = self.text.chars().count();
        let total_rows: usize = self.lines.iter().map(|l| l.1).fold(0, usize::saturating_add);
        if total_text == 0 || total_rows == 0 {
            return Vec::new();
        }
        let scale = total_rows as f64 / total_text as f64;
        let chars = |byte: usize| self.text.get(..byte).map_or(total_text, |t| t.chars().count()) as f64 * scale;
        let (a, b) = (chars(start), chars(end));
        let mut out = Vec::new();
        let mut r0 = 0.0;
        for (k, (rect, n)) in self.lines.iter().enumerate() {
            let r1 = r0 + *n as f64;
            let (from, to) = (a.max(r0), b.min(r1));
            if to > from && *n > 0 {
                let width = rect[2] - rect[0];
                let x = |c: f64| rect[0] + (c - r0) / *n as f64 * width;
                let words = self.ink.get(k).map(Vec::as_slice).unwrap_or_default();
                // The nearest word's left (or right) edge to an estimate, or the estimate.
                let snap = |x: f64, edge: usize| {
                    words.iter().filter_map(|w| w.get(edge).copied()).min_by(|p, q| (p - x).abs().total_cmp(&(q - x).abs())).unwrap_or(x)
                };
                let x0 = if from > r0 { snap(x(from), 0) } else { rect[0] };
                let x1 = if to < r1 { snap(x(to), 2) } else { rect[2] };
                out.push([x0, rect[1], x1.max(x0), rect[3]]);
            }
            r0 = r1;
        }
        out
    }
}

/// A word of a passage's text found on one of its rows: where it starts in the text, the row
/// (an index into [`Passage::lines`]) and its left and right edge there.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Placed {
    byte: usize,
    row: usize,
    x0: f64,
    x1: f64,
}

/// How far ahead on the rows a word of the text is looked for: citations and bullets the
/// filters took out of the text are skipped over this way.
const PLACE_WINDOW: usize = 40;

impl Passage {
    /// `text`'s words found on their rows, in order. The rows' words are matched to the text's
    /// in turn, ignoring case and punctuation, looking a little ahead on the rows for what the
    /// filters removed; a word hyphenated over two rows matches its two halves. A word's edges
    /// are its box where the row's word boxes are known and match its words, else its share of
    /// the row by characters. Words not found are left out.
    fn placed(&self) -> Vec<Placed> {
        let key = |w: &str| w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
        let mut rows: Vec<(String, usize, f64, f64)> = Vec::new();
        // A word hyphenated at its row's end joins the next row's first word: "exam-" "ple".
        let mut hyphenated = false;
        for (row, (text, (rect, _))) in self.rows_text.iter().zip(&self.lines).enumerate() {
            let words: Vec<(f64, &str)> = text.split_whitespace().map(|w| (text.get(..offset_in(text, w)).map_or(0.0, advance), w)).collect();
            // Where each word starts and ends along the row, in typical glyph widths.
            let total = advance(text).max(f64::EPSILON);
            let width = rect[2] - rect[0];
            let ink = self.ink.get(row).filter(|ink| ink.len() == words.len());
            for (index, (at, w)) in words.iter().enumerate() {
                let edges = match ink.and_then(|ink| ink.get(index)) {
                    Some(b) => (b[0], b[2]),
                    None => (rect[0] + at / total * width, rect[0] + (at + advance(w)) / total * width),
                };
                let word = key(w);
                if word.is_empty() {
                    continue;
                }
                match rows.last_mut() {
                    Some(last) if hyphenated => last.0.push_str(&word),
                    _ => rows.push((word, row, edges.0, edges.1)),
                }
                hyphenated = index + 1 == words.len() && w.ends_with('-') && w.chars().count() > 1;
            }
        }
        let text: Vec<(usize, String)> =
            self.text.split_whitespace().map(|w| (offset_in(&self.text, w), key(w))).filter(|(_, k)| !k.is_empty()).collect();
        if let Some(pairs) = align(&text, &rows) {
            return pairs
                .into_iter()
                .filter_map(|(t, r)| {
                    let (byte, _) = text.get(t)?;
                    let (_, row, x0, x1) = rows.get(r)?;
                    Some(Placed { byte: *byte, row: *row, x0: *x0, x1: *x1 })
                })
                .collect();
        }
        let mut out = Vec::new();
        let mut next = 0usize;
        for w in self.text.split_whitespace() {
            let k = key(w);
            if k.is_empty() {
                continue;
            }
            let ahead = rows.get(next..next.saturating_add(PLACE_WINDOW).min(rows.len())).unwrap_or_default();
            let byte = offset_in(&self.text, w);
            if let Some(found) = ahead.iter().position(|r| r.0 == k) {
                if let Some(r) = rows.get(next + found) {
                    out.push(Placed { byte, row: r.1, x0: r.2, x1: r.3 });
                }
                next += found + 1;
            } else if let (Some(a), Some(b)) = (ahead.first(), ahead.get(1))
                && format!("{}{}", a.0, b.0) == k
            {
                // "exam-" and "ple" on two rows read as "example".
                out.push(Placed { byte, row: a.1, x0: a.2, x1: a.3 });
                next += 2;
            }
        }
        out
    }
}

/// How wide `text` sets, in ems, by typical widths of a serif text face (Times, Computer
/// Modern): narrow `i`, `l` and punctuation, wide `m`, `w` and capitals. Positions inside a row
/// whose words' boxes aren't known are estimated by it.
fn advance(text: &str) -> f64 {
    text.chars()
        .map(|c| match c {
            ' ' => 0.33,
            'i' | 'j' | 'l' | '.' | ',' | ';' | ':' | '!' | '\'' | '|' | '’' | '‘' => 0.28,
            'f' | 't' | 'r' | 'I' | 'J' | '(' | ')' | '[' | ']' | '-' | '/' => 0.36,
            's' | 'c' | 'z' | '"' | '“' | '”' => 0.42,
            'm' | 'w' => 0.78,
            'M' | 'W' => 0.95,
            '–' | '%' => 0.6,
            '—' => 1.0,
            c if c.is_ascii_digit() => 0.5,
            c if c.is_uppercase() => 0.72,
            _ => 0.5,
        })
        .sum()
}

/// The most cells [`align`] fills in; longer passages fall back to matching word by word.
const MAX_ALIGN_CELLS: usize = 250_000;

/// The text's words matched to the rows' words, in order, as pairs of indexes: a longest
/// common subsequence of their keys, so a word the text lost (a citation) or the rows lack
/// doesn't pull later words onto a wrong match. `None` when the passage is too long for it.
fn align(text: &[(usize, String)], rows: &[(String, usize, f64, f64)]) -> Option<Vec<(usize, usize)>> {
    let (n, m) = (text.len(), rows.len());
    let cells = n.checked_add(1)?.checked_mul(m.checked_add(1)?)?;
    if cells > MAX_ALIGN_CELLS {
        return None;
    }
    // lengths[i * (m + 1) + j]: the longest match of text[i..] and rows[j..].
    let mut lengths = vec![0u32; cells];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let same = text.get(i).zip(rows.get(j)).is_some_and(|(t, r)| t.1 == r.0);
            let value = if same {
                lengths.get(at(i + 1, j + 1)).copied().unwrap_or(0) + 1
            } else {
                lengths.get(at(i + 1, j)).copied().unwrap_or(0).max(lengths.get(at(i, j + 1)).copied().unwrap_or(0))
            };
            if let Some(cell) = lengths.get_mut(at(i, j)) {
                *cell = value;
            }
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    let same = |i: usize, j: usize| text.get(i).zip(rows.get(j)).is_some_and(|(t, r)| t.1 == r.0);
    let length = |i: usize, j: usize| lengths.get(at(i, j)).copied().unwrap_or(0);
    while i < n && j < m {
        // Of equally long matches, the one whose words follow each other: a common word
        // ("the") is matched where the next word matches too, or where nothing later would do.
        if same(i, j) && (length(i, j + 1) < length(i, j) || same(i + 1, j + 1) || i + 1 == n) {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if length(i + 1, j) >= length(i, j + 1) {
            i += 1;
        } else {
            j += 1;
        }
    }
    Some(pairs)
}

/// The byte offsets in `text` where its sentences start, the first at 0 or later (after
/// leading whitespace), in order.
pub fn sentence_starts(text: &str) -> Vec<usize> {
    // The sentences are slices of `text`, so their address tells where they start.
    sentences(text).iter().map(|s| offset_in(text, s)).collect()
}

/// What is left out of reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Filters {
    /// Paragraphs that are only a page number ("12", "xiv", "- 3 -", "Page 4", "3 / 10").
    pub page_numbers: bool,
    /// Bullets and footnote symbols ("•", "◦", "∗", "†", "‡"), and a dash opening a paragraph.
    pub bullets: bool,
    /// Display equations: paragraphs set mostly in math fonts, and equation numbers ("(1)").
    /// Math inside a sentence is still read.
    pub equations: bool,
    /// Paragraphs of numbers without words: table cells ("5.29 24.9", "1 512 512").
    pub numbers: bool,
    /// Citations in the style a page mostly uses: numeric in brackets ("[13]", "[1, 4–6]") or
    /// author and year in parentheses ("(Smith et al., 2020)").
    pub citations: bool,
    /// Text inside figures and tables ([`Role::FigureText`]).
    pub figures: bool,
    /// Figure and table captions ([`Role::Caption`]).
    pub captions: bool,
    /// Running headers and footers: the page's top or bottom row when a page or two before or
    /// after has the same words there (page numbers aside), in type no larger than the body's.
    pub headers: bool,
}

impl Default for Filters {
    fn default() -> Self {
        Self { page_numbers: true, bullets: true, equations: true, numbers: true, citations: true, figures: true, captions: false, headers: true }
    }
}

impl Document {
    /// The paragraphs on `page` (0-based) that have something to read, in reading order.
    /// A page without text (a scan with no recognized text), or one that can't be read, has none.
    pub fn read_aloud_passages(&self, page: usize, filters: Filters) -> Vec<Passage> {
        self.read_aloud_passages_with(page, filters, &Rules::default())
    }

    /// [`Document::read_aloud_passages`] by `rules` rather than the default ones.
    pub fn read_aloud_passages_with(&self, page: usize, filters: Filters, rules: &Rules) -> Vec<Passage> {
        let Some(editor) = self.editor.as_ref() else { return Vec::new() };
        let (blocks, lines) = pdfcraft_edit::reading_blocks_and_lines(&editor.cos, page).unwrap_or_default();
        let blocks: Vec<Block> = blocks
            .into_iter()
            .enumerate()
            .filter_map(|(index, b)| {
                let text = normalize(&b.text);
                let keep = !text.is_empty();
                let size = if b.size.is_finite() && b.size > 0.0 { b.size } else { 1.0 };
                let rows = b.rows.len().max(1);
                let measured = row_lines(&b, &lines);
                let ink = if measured.is_some() { row_ink(&b, &lines) } else { Vec::new() };
                let is_measured = measured.is_some();
                let lines = measured.unwrap_or_else(|| row_boxes(b.rect, &b.editor_text, rows));
                let ink = if ink.len() == lines.len() { ink } else { vec![Vec::new(); lines.len()] };
                // Each row's own text; past the rows `lines` has, the rest goes to the last.
                let mut row_texts: Vec<String> = b.editor_text.split('\n').map(normalize).collect();
                if row_texts.len() > lines.len() {
                    let rest = row_texts.split_off(lines.len().saturating_sub(1)).join(" ");
                    row_texts.push(rest);
                }
                row_texts.resize(lines.len(), String::new());
                let chars = text.chars().filter(|c| !c.is_whitespace()).count();
                let (math, words) = if is_math_font(&b.base_font) { (chars, 0) } else { (0, prose_words(&text)) };
                keep.then_some(Block {
                    index: index.saturating_mul(PARTS),
                    rect: b.rect,
                    size,
                    upright: b.rotation.abs() < 1.0,
                    bold: b.bold || is_bold_font(&b.base_font) || font_named(&b.base_font, &rules.bold_fonts),
                    italic: b.italic || is_italic_font(&b.base_font) || font_named(&b.base_font, &rules.italic_fonts),
                    rows,
                    chars,
                    math,
                    words,
                    text,
                    lines,
                    ink,
                    row_texts,
                    measured: is_measured,
                })
            })
            .collect();
        let mut blocks = split_paragraphs(blocks, rules);
        let body = body_size(&blocks);
        if filters.headers {
            let span = rules.header_pages;
            let neighbours: Vec<Edges> = (page.saturating_sub(span)..=page.saturating_add(span))
                .filter(|&p| p != page && p < self.info.pages.len())
                .filter_map(|p| pdfcraft_edit::reading_blocks_and_lines(&editor.cos, p).ok())
                .map(|(_, lines)| edges(&lines, rules.header_letters))
                .collect();
            let own = edges(&lines, rules.header_letters);
            let running = |top: bool| {
                own.at(top).filter(|e| e.size <= body * rules.header_size && neighbours.iter().any(|n| n.at(top).is_some_and(|o| o.key == e.key)))
            };
            let bands: Vec<&Edge> = [running(true), running(false)].into_iter().flatten().collect();
            let running = |b: &Block| bands.iter().any(|e| e.holds(b.rect));
            // A page whose only text is that row (a page of slides' titles, a numbered blank
            // page) is read: the row is what the page says.
            if !blocks.iter().all(running) {
                blocks.retain(|b| !running(b));
            }
        }
        let mut passages: Vec<Passage> = merge(&blocks, rules)
            .into_iter()
            // After merging: a lone "2" or "d" is often part of a formula, not a page number.
            .filter(|g| !(filters.page_numbers && is_page_number(&g.text)))
            .filter(|g| !(filters.equations && (is_equation(g) || is_equation_number(&g.text))))
            .filter(|g| !(filters.numbers && prose_words(&g.text) == 0 && g.text.contains(|c: char| c.is_ascii_digit())))
            .map(|g| Passage {
                role: role(&g, &tidy_spaces(&g.text), body, rules),
                page,
                paragraph: g.index,
                rect: g.rect,
                text: tidy_spaces(&g.text),
                lines: g.lines,
                ink: g.ink,
                rows_text: g.row_texts,
                heading: None,
            })
            .collect();
        if filters.bullets {
            for p in &mut passages {
                p.text = strip_bullets(&p.text);
            }
            passages.retain(|p| !p.text.is_empty());
        }
        if filters.citations {
            let style = citation_style(passages.iter().map(|p| p.text.as_str()));
            if let Some(style) = style {
                for p in &mut passages {
                    p.text = strip_citations(&p.text, style);
                }
                passages.retain(|p| !p.text.is_empty());
            }
        }
        let columns = columns(&blocks, body);
        let rows = text_rows(&lines);
        let regions = figure_regions(&passages, &rows, &columns, body, rules);
        mark_figure_regions(&mut passages, &regions);
        mark_headings(&mut passages, &crate::outline_rows(&self.info.outline), page);
        // A bookmarked heading is read whatever it looks like.
        passages.retain(|p| {
            p.heading.is_some()
                || match p.role {
                    Role::Text => true,
                    Role::Caption => !filters.captions,
                    Role::FigureText => !filters.figures,
                }
        });
        passages
    }
}

/// A page's top or bottom row of upright text.
#[derive(Clone, Debug, PartialEq)]
struct Edge {
    bottom: f64,
    top: f64,
    /// The largest font size in it.
    size: f64,
    /// Its letters in lower case, without digits, spaces or punctuation: a running header's
    /// words whatever the page number beside them.
    key: String,
}

impl Edge {
    /// `rect` lies within the row.
    fn holds(&self, rect: [f64; 4]) -> bool {
        let slack = self.size * 0.3;
        rect[1] >= self.bottom - slack && rect[3] <= self.top + slack
    }
}

/// A page's top and bottom rows.
#[derive(Clone, Debug, Default, PartialEq)]
struct Edges {
    top: Option<Edge>,
    bottom: Option<Edge>,
}

impl Edges {
    fn at(&self, top: bool) -> Option<&Edge> {
        if top { self.top.as_ref() } else { self.bottom.as_ref() }
    }
}

/// The page's top and bottom rows of upright text, when they have `min_letters` letters or more
/// to compare.
fn edges(lines: &[pdfcraft_edit::TextLine], min_letters: usize) -> Edges {
    let upright: Vec<&pdfcraft_edit::TextLine> = lines
        .iter()
        .filter(|l| {
            let r = l.rect;
            !l.text.trim().is_empty()
                && l.size.is_finite()
                && l.size > 0.0
                && r.iter().all(|v| v.is_finite())
                && r[3] > r[1]
                // Turned text, such as an archive's stamp up the margin, is taller than its size.
                && r[3] - r[1] < l.size * 3.0
        })
        .collect();
    let row = |first: Option<&&pdfcraft_edit::TextLine>| -> Option<Edge> {
        let first = first?;
        let mut row: Vec<&pdfcraft_edit::TextLine> = upright
            .iter()
            .copied()
            .filter(|l| (l.rect[1] - first.rect[1]).abs() < 0.35 * l.size.max(first.size) && l.rect[3] > first.rect[1] && l.rect[1] < first.rect[3])
            .collect();
        row.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
        let key: String = row.iter().flat_map(|l| l.text.chars()).filter(|c| c.is_alphabetic()).flat_map(char::to_lowercase).collect();
        (key.chars().count() >= min_letters).then(|| Edge {
            bottom: row.iter().map(|l| l.rect[1]).fold(f64::INFINITY, f64::min),
            top: row.iter().map(|l| l.rect[3]).fold(f64::NEG_INFINITY, f64::max),
            size: row.iter().map(|l| l.size).fold(0.0, f64::max),
            key,
        })
    };
    Edges {
        top: row(upright.iter().max_by(|a, b| a.rect[3].total_cmp(&b.rect[3]))),
        bottom: row(upright.iter().min_by(|a, b| a.rect[1].total_cmp(&b.rect[1]))),
    }
}

/// The font size most of the page's characters are set in.
fn body_size(blocks: &[Block]) -> f64 {
    let mut sizes: Vec<(i64, usize)> = Vec::new();
    for b in blocks.iter().filter(|b| b.math == 0) {
        let key = (b.size * 2.0).round() as i64;
        match sizes.iter_mut().find(|(s, _)| *s == key) {
            Some((_, n)) => *n += b.chars,
            None => sizes.push((key, b.chars)),
        }
    }
    sizes.iter().max_by_key(|(_, n)| *n).map_or(10.0, |(s, _)| *s as f64 / 2.0)
}

/// Whether `text` opens with a figure's or table's label: "Figure 3:", "Fig. 3.", "Table A1",
/// "Algorithm 2", then a colon, a full stop or a capitalized word ("Figure 3 shows" is prose).
/// `extra` labels count too.
fn is_caption(text: &str, extra: &[String]) -> bool {
    const LABELS: [&str; 12] = ["Figure", "Fig.", "Table", "Tab.", "Algorithm", "Listing", "Chart", "Exhibit", "Abbildung", "Abb.", "Tabelle", "Fig"];
    let Some(rest) = LABELS.iter().copied().chain(extra.iter().map(String::as_str)).find_map(|l| text.strip_prefix(l)) else { return false };
    let rest = rest.trim_start();
    let number = rest.split(|c: char| c.is_whitespace() || c == ':' || c == '|').next().unwrap_or_default();
    let label = number.trim_end_matches('.');
    if label.is_empty() || !label.chars().next().is_some_and(|c| c.is_ascii_digit() || c.is_ascii_uppercase()) || label.len() > 6 {
        return false;
    }
    if !label.chars().any(|c| c.is_ascii_digit()) && label.len() > 1 {
        return false;
    }
    let after = rest.get(number.len()..).unwrap_or_default();
    number.ends_with('.')
        || after.starts_with([':', '|', '.'])
        || after.trim_start().starts_with(|c: char| c.is_uppercase())
        || after.trim().is_empty()
}

/// What `g` (whose text, tidied, is `text`) is ([`Role`]); `body` is the page's body font size.
fn role(g: &Block, text: &str, body: f64, rules: &Rules) -> Role {
    let text = text.trim();
    if is_caption(text, &rules.captions) {
        return Role::Caption;
    }
    // Headings are bold, larger or numbered; they stay text.
    if g.bold || g.size > body * 1.05 || split_heading_number(text).0.is_some() {
        return Role::Text;
    }
    let words = text.split_whitespace().count();
    // Sentence punctuation: a comma, full stop and the like followed by a space or the end.
    let punctuated = text.char_indices().any(|(i, c)| {
        matches!(c, '.' | ',' | ';' | ':' | '?' | '!') && text.get(i + c.len_utf8()..).is_none_or(|r| r.is_empty() || r.starts_with(' '))
    });
    let small = g.size < body * 0.92;
    if (words < 5 && !punctuated) || (small && words < 15 && !punctuated) || (words >= 8 && !punctuated) { Role::FigureText } else { Role::Text }
}

/// One row of a page's text: its vertical extent, and the horizontal extents of its runs,
/// sorted and merged where they overlap.
#[derive(Clone, Debug, PartialEq)]
struct TextRow {
    bottom: f64,
    top: f64,
    size: f64,
    /// Each run's left and right edge and how many characters (spaces included) it holds.
    runs: Vec<(f64, f64, usize)>,
}

/// The page's upright lines grouped into rows by where they sit.
fn text_rows(lines: &[pdfcraft_edit::TextLine]) -> Vec<TextRow> {
    let mut rows: Vec<TextRow> = Vec::new();
    for l in lines.iter().filter(|l| !l.text.trim().is_empty() && l.size.is_finite() && l.size > 0.0) {
        let r = l.rect;
        if !r.iter().all(|v| v.is_finite()) || r[2] <= r[0] {
            continue;
        }
        // The words' own boxes where the line has them: a line's box also spans the gaps a
        // table's cells leave inside one text object.
        let chars = l.text.chars().count();
        let words: Vec<[f64; 4]> = l.ink.iter().copied().filter(|w| w[2] > w[0] && w[0].is_finite() && w[2].is_finite()).collect();
        let runs: Vec<(f64, f64, usize)> = if words.is_empty() {
            vec![(r[0], r[2], chars)]
        } else {
            // Each word's characters, with the space after it, where the words line up with the
            // boxes; else the line's characters shared out.
            let texts: Vec<&str> = l.text.split_whitespace().collect();
            let each = chars / words.len().max(1);
            words
                .iter()
                .enumerate()
                .map(|(k, w)| (w[0], w[2], if texts.len() == words.len() { texts.get(k).map_or(each, |t| t.chars().count() + 1) } else { each }))
                .collect()
        };
        let row = rows.iter_mut().find(|row| (row.bottom - r[1]).abs() < 0.35 * row.size.max(l.size) && r[3] > row.bottom && r[1] < row.top);
        match row {
            Some(row) => {
                row.runs.extend(runs);
                row.top = row.top.max(r[3]);
                row.bottom = row.bottom.min(r[1]);
                row.size = row.size.max(l.size);
            }
            None => rows.push(TextRow { bottom: r[1], top: r[3], size: l.size, runs }),
        }
    }
    for row in &mut rows {
        row.runs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f64, f64, usize)> = Vec::with_capacity(row.runs.len());
        for &(a, b, n) in &row.runs {
            match merged.last_mut() {
                Some(last) if a <= last.1 => {
                    last.1 = last.1.max(b);
                    last.2 = last.2.saturating_add(n);
                }
                _ => merged.push((a, b, n)),
            }
        }
        row.runs = merged;
    }
    rows.sort_by(|a, b| b.top.total_cmp(&a.top));
    rows
}

/// The page's text columns: the horizontal extents of its prose paragraphs (three rows or more
/// in the body size), merged where they overlap.
fn columns(blocks: &[Block], body: f64) -> Vec<(f64, f64)> {
    let mut cols: Vec<(f64, f64)> = Vec::new();
    for b in blocks.iter().filter(|b| b.rows >= 3 && b.math == 0 && (b.size - body).abs() < body * 0.1 && b.words >= 15) {
        let (x0, x1) = (b.rect[0], b.rect[2]);
        match cols.iter_mut().find(|c| x0 < c.1 && c.0 < x1 && (x1.min(c.1) - x0.max(c.0)) > 0.5 * (x1 - x0).min(c.1 - c.0)) {
            Some(c) => *c = (c.0.min(x0), c.1.max(x1)),
            None => cols.push((x0, x1)),
        }
    }
    cols
}

/// Whether `row`, in the column `col`, is laid out like a figure or a table rather than
/// continuous text:
/// - a wide gap between its runs, other than one right after a short label opening the row
///   (a list item: its number or bullet, then its text);
/// - too few characters for its width, as a table row drawn as one text object spanning its
///   cells has (prose fills about half an em per character);
/// - or text that keeps clear of both of the column's edges (labels, centered pieces).
///
/// A paragraph's short last row starts at the column's edge, dense, and isn't.
fn is_sparse(row: &TextRow, col: (f64, f64), rules: &Rules) -> bool {
    let size = row.size.max(1.0);
    let inside: Vec<(f64, f64, usize)> = row.runs.iter().copied().filter(|r| r.1 > col.0 && r.0 < col.1).collect();
    let (Some(first), Some(last)) = (inside.first(), inside.last()) else { return false };
    let label = first.1 - first.0 < rules.label_width * size;
    let gap = inside.windows(2).enumerate().any(|(i, w)| matches!(w, [a, b] if b.0 - a.1 > rules.cell_gap * size) && !(i == 0 && label));
    let thin = density(row, col).is_some_and(|d| d < rules.sparse_density);
    let margin = rules.figure_margin * size;
    gap || thin || (first.0 - col.0 > margin && col.1 - last.1 > margin)
}

/// How full of text `row` is inside the column `col`: its characters at half an em each over
/// the width from its first run to its last. Prose comes to about one; `None` for a row too
/// short to tell.
fn density(row: &TextRow, col: (f64, f64)) -> Option<f64> {
    let size = row.size.max(1.0);
    let inside: Vec<&(f64, f64, usize)> = row.runs.iter().filter(|r| r.1 > col.0 && r.0 < col.1).collect();
    let span = inside.last()?.1 - inside.first()?.0;
    let chars: usize = inside.iter().map(|r| r.2).fold(0, usize::saturating_add);
    (span > 6.0 * size).then(|| chars as f64 * 0.5 * size / span)
}

/// The areas figures and tables take: from each caption, the rows next to it above and below
/// in its column that are laid out like a figure or table ([`is_sparse`]), passing over up to
/// two other rows at a time, up to the first row of prose: as dense as prose ([`density`]) and
/// reaching the column's right edge.
fn figure_regions(passages: &[Passage], rows: &[TextRow], columns: &[(f64, f64)], body: f64, rules: &Rules) -> Vec<[f64; 4]> {
    let mut regions = Vec::new();
    for caption in passages.iter().filter(|p| p.role == Role::Caption) {
        let c = caption.rect;
        let col = columns.iter().copied().find(|col| c[0] < col.1 && col.0 < c[2]).unwrap_or((c[0], c[2]));
        let in_col = |r: &&TextRow| r.runs.iter().any(|run| run.1 > col.0 && run.0 < col.1);
        let rows: Vec<&TextRow> = rows.iter().filter(in_col).collect();
        // Rows above the caption, nearest first, then below it.
        let above = rows.iter().rev().filter(|r| r.bottom >= c[3] - 0.2 * body);
        let below = rows.iter().filter(|r| r.top <= c[1] + 0.2 * body);
        for side in [above.collect::<Vec<_>>(), below.collect::<Vec<_>>()] {
            let mut area: Option<[f64; 4]> = None;
            // Short rows passed over since the last sparse one: a lone tick label looks like a
            // paragraph's last row, but prose reaching the column's right edge ends the area.
            let mut short = 0;
            for r in side {
                if is_sparse(r, col, rules) {
                    short = 0;
                    let b = [col.0, r.bottom, col.1, r.top];
                    area = Some(area.map_or(b, |a| union(a, b)));
                    continue;
                }
                // Prose: as full as text gets, out to the column's right edge. A table row with
                // a long cell can come near that without being prose.
                let reaches = r.runs.iter().any(|run| run.1 >= col.1 - rules.figure_margin * r.size.max(1.0) && run.0 < col.1);
                let reaches = reaches && density(r, col).is_none_or(|d| d >= rules.prose_density);
                short += 1;
                if reaches || short > rules.passed_rows {
                    break;
                }
            }
            regions.extend(area);
        }
    }
    regions
}

/// Text passages inside a figure's or table's area become [`Role::FigureText`].
fn mark_figure_regions(passages: &mut [Passage], regions: &[[f64; 4]]) {
    for p in passages.iter_mut().filter(|p| p.role == Role::Text) {
        let (x, y) = ((p.rect[0] + p.rect[2]) / 2.0, (p.rect[1] + p.rect[3]) / 2.0);
        if regions.iter().any(|r| x >= r[0] && x <= r[2] && y >= r[1] - 1.0 && y <= r[3] + 1.0) {
            p.role = Role::FigureText;
        }
    }
}

/// Mark the passages on `page` that a bookmark going to that page names, with the bookmark's
/// depth. A heading matches its bookmark's title word for word, ignoring case and punctuation,
/// with or without its number ("1 Introduction" for "Introduction"). Each bookmark marks one
/// passage, the first.
fn mark_headings(passages: &mut [Passage], bookmarks: &[crate::BookmarkRow], page: usize) {
    let key = |s: &str| s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect::<Vec<_>>().join(" ");
    let mut titles: Vec<(String, usize)> =
        bookmarks.iter().filter(|b| b.page == Some(page)).map(|b| (key(&b.title), b.path.len())).filter(|(t, _)| !t.is_empty()).collect();
    for p in passages.iter_mut() {
        if titles.is_empty() {
            break;
        }
        if p.text.chars().count() > 200 {
            continue;
        }
        let text = key(&p.text);
        let (number, rest) = split_heading_number(&p.text);
        let unnumbered = number.map(|_| key(rest));
        if let Some(k) = titles.iter().position(|(t, _)| *t == text || unnumbered.as_deref() == Some(t.as_str())) {
            p.heading = titles.get(k).map(|(_, depth)| *depth);
            titles.remove(k);
        }
    }
}

/// A heading's number and the rest: "5.2 Results" → ("5.2", "Results"), "A.1 Proofs" →
/// ("A.1", "Proofs"). `None` when it doesn't start with one (a lone letter is a word: "A Note").
fn split_heading_number(text: &str) -> (Option<&str>, &str) {
    let text = text.trim();
    let Some((first, rest)) = text.split_once(char::is_whitespace) else { return (None, text) };
    let token = first.trim_end_matches('.');
    let mut parts = token.split('.');
    let lead = parts.next().unwrap_or_default();
    let lead_ok = (!lead.is_empty() && lead.chars().all(|c| c.is_ascii_digit()) && lead.len() <= 3)
        || (lead.len() == 1 && lead.chars().all(|c| c.is_ascii_uppercase()) && token.contains('.'));
    let rest_ok = parts.all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()));
    if lead_ok && rest_ok { (Some(token), rest.trim()) } else { (None, text) }
}

/// A heading read as a sentence of its own: in English its number named ("Chapter 5.
/// Conclusion.", "Section 5.2. Results.", "Appendix A.1. Proofs."), elsewhere only closed with
/// a full stop, so the speech pauses after it.
pub fn heading_sentence(text: &str, english: bool) -> String {
    let close = |s: &str| {
        let s = s.trim().trim_end_matches(':');
        if s.ends_with(['.', '!', '?', '…']) { s.to_string() } else { format!("{s}.") }
    };
    match split_heading_number(text) {
        (Some(number), title) if english => {
            let name = if number.starts_with(|c: char| c.is_ascii_uppercase()) {
                "Appendix"
            } else if number.contains('.') {
                "Section"
            } else {
                "Chapter"
            };
            format!("{name} {number}. {}", close(title))
        }
        _ => close(text),
    }
}

/// A reading block, as [`merge`] needs it.
#[derive(Clone, Debug, PartialEq)]
struct Block {
    index: usize,
    rect: [f64; 4],
    size: f64,
    /// Its text runs left to right (merging only measures upright text).
    upright: bool,
    bold: bool,
    italic: bool,
    rows: usize,
    /// Its characters, how many of them are set in a math font, and its words of three letters
    /// or more set in text fonts (prose).
    chars: usize,
    math: usize,
    words: usize,
    text: String,
    /// Its rows' boxes and character counts ([`Passage::lines`]).
    lines: Vec<([f64; 4], usize)>,
    /// Its rows' word boxes ([`Passage::ink`]), one list per row in `lines`.
    ink: Vec<Vec<[f64; 4]>>,
    /// Each row's text, one per row in `lines`.
    row_texts: Vec<String>,
    /// `lines` are the rows' measured boxes, not equal bands of the block.
    measured: bool,
}

/// A reading block may be split into at most this many paragraphs; its index is the block's
/// times this, plus the part.
const PARTS: usize = 256;

/// `blocks` with blocks that hold several paragraphs split ([`split_rows`]).
fn split_paragraphs(blocks: Vec<Block>, rules: &Rules) -> Vec<Block> {
    let mut out = Vec::with_capacity(blocks.len());
    for b in &blocks {
        if !b.measured || b.row_texts.len() < 2 || b.math > 0 || !b.upright {
            out.push(b.clone());
            continue;
        }
        let typical = line_gap(&blocks, b.size);
        out.extend(split_rows(b, typical, rules));
    }
    out
}

/// `b` cut where a paragraph starts inside it: below a gap clearly wider than `typical` (in
/// font sizes), or at a first-line indent, a row starting well right of the row above whose
/// next row goes back to the margin (a hanging indent keeps its later rows indented), or that
/// is the block's last row after two rows at the same margin.
fn split_rows(b: &Block, typical: f64, rules: &Rules) -> Vec<Block> {
    let rows: Vec<([f64; 4], usize)> = b.lines.clone();
    let left = |i: usize| rows.get(i).map(|r| r.0[0]);
    let mut cuts = Vec::new();
    for i in 1..rows.len() {
        let (Some(prev), Some(row)) = (rows.get(i - 1), rows.get(i)) else { continue };
        let gap = (prev.0[1] - row.0[3]) / b.size;
        let indent = (row.0[0] - prev.0[0]) / b.size;
        // The row after goes back to the margin; or, on the block's last row, the two rows
        // before start at the same margin (a hanging indent's later rows don't).
        let back = match left(i + 1) {
            Some(next) => (next - prev.0[0]).abs() < 0.3 * b.size,
            None => i >= 2 && left(i - 2).is_some_and(|before| (before - prev.0[0]).abs() < 0.3 * b.size),
        };
        if gap > typical + rules.paragraph_gap || ((rules.indent_min..=rules.indent_max).contains(&indent) && back) {
            cuts.push(i);
        }
    }
    if cuts.is_empty() {
        return vec![b.clone()];
    }
    cuts.truncate(PARTS - 1);
    let mut parts = Vec::new();
    let bounds: Vec<usize> = std::iter::once(0).chain(cuts).chain(std::iter::once(rows.len())).collect();
    for (part, w) in bounds.windows(2).enumerate() {
        let (Some(&from), Some(&to)) = (w.first(), w.get(1)) else { continue };
        let mut text = String::new();
        for t in b.row_texts.get(from..to).unwrap_or_default() {
            join_text(&mut text, t, Join::NewRow, &rules.compound_ends);
        }
        let text = normalize(&text);
        let lines: Vec<([f64; 4], usize)> = rows.get(from..to).unwrap_or_default().to_vec();
        let Some(rect) = lines.iter().map(|l| l.0).reduce(union) else { continue };
        if text.is_empty() {
            continue;
        }
        parts.push(Block {
            index: b.index.saturating_add(part),
            rect,
            rows: lines.len(),
            chars: text.chars().filter(|c| !c.is_whitespace()).count(),
            words: prose_words(&text),
            row_texts: b.row_texts.get(from..to).unwrap_or_default().to_vec(),
            ink: b.ink.get(from..to).unwrap_or_default().to_vec(),
            text,
            lines,
            ..b.clone()
        });
    }
    parts
}

/// A block's rows, as [`Passage::lines`] holds them, from the lines each row holds: `None`
/// when a row's lines are missing or lie outside the block (a regrouped contents entry).
fn row_lines(b: &pdfcraft_edit::TextBlock, lines: &[pdfcraft_edit::TextLine]) -> Option<Vec<([f64; 4], usize)>> {
    let counts: Vec<usize> = b.editor_text.split('\n').map(|r| r.chars().count()).collect();
    if b.rows.is_empty() || counts.len() != b.rows.len() {
        return None;
    }
    let inside = |r: &[f64; 4]| r[0] >= b.rect[0] - 1.0 && r[1] >= b.rect[1] - 1.0 && r[2] <= b.rect[2] + 1.0 && r[3] <= b.rect[3] + 1.0;
    b.rows
        .iter()
        .zip(counts)
        .map(|(row, chars)| {
            let rect = row.iter().map(|&i| lines.get(i).map(|l| l.rect)).reduce(|a, b| a.zip(b).map(|(a, b)| union(a, b)))??;
            inside(&rect).then_some((rect, chars))
        })
        .collect()
}

/// The word boxes of each of a block's rows, left to right ([`Passage::ink`]).
fn row_ink(b: &pdfcraft_edit::TextBlock, lines: &[pdfcraft_edit::TextLine]) -> Vec<Vec<[f64; 4]>> {
    b.rows
        .iter()
        .map(|row| {
            let mut words: Vec<[f64; 4]> = row.iter().filter_map(|&i| lines.get(i)).flat_map(|l| l.ink.iter().copied()).collect();
            words.sort_by(|p, q| p[0].total_cmp(&q[0]));
            words
        })
        .collect()
}

fn union(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// A block's rows, as [`Passage::lines`] holds them: `rect` cut into `rows` equal bands from the
/// top, each with the characters of its row in `editor_text` (rows split at newlines).
fn row_boxes(rect: [f64; 4], editor_text: &str, rows: usize) -> Vec<([f64; 4], usize)> {
    let counts: Vec<usize> = editor_text.split('\n').map(|r| r.chars().count()).collect();
    let rows = rows.max(1).min(counts.len().max(1));
    let band = (rect[3] - rect[1]) / rows as f64;
    (0..rows)
        .map(|i| {
            let top = rect[3] - band * i as f64;
            let chars = if i + 1 == rows { counts.iter().skip(i).sum() } else { counts.get(i).copied().unwrap_or(0) };
            ([rect[0], top - band, rect[2], top], chars)
        })
        .collect()
}

/// The typical whitespace between two rows of a paragraph on this page, in font sizes: the
/// median of the gaps inside multi-row blocks and between blocks stacked one under the other.
/// Text of each size is measured on its own where there is enough of it (footnotes are often
/// set tighter than the body), and against the whole page otherwise.
fn line_gap(blocks: &[Block], size: f64) -> f64 {
    let similar = |s: f64| (s - size).abs() <= size * 0.1;
    let mine = median_gap(blocks, &similar);
    mine.or_else(|| median_gap(blocks, &|_| true)).unwrap_or(DEFAULT_LINE_GAP)
}

/// The median line gap (in font sizes) measured on text whose size passes `keep`, if there are
/// at least three gaps to measure.
fn median_gap(blocks: &[Block], keep: &dyn Fn(f64) -> bool) -> Option<f64> {
    let mut gaps: Vec<f64> = Vec::new();
    for b in blocks.iter().filter(|b| b.upright && b.rows > 1 && keep(b.size)) {
        // Rows of glyphs about one font size tall, the rest of the height between them.
        let rows = b.rows as f64;
        let gap = ((b.rect[3] - b.rect[1]) - rows * b.size) / (rows - 1.0) / b.size;
        // Each gap inside the block counts once.
        gaps.extend(std::iter::repeat_n(gap, b.rows - 1));
    }
    for w in blocks.windows(2) {
        let [a, b] = w else { continue };
        let size = a.size.max(b.size);
        let gap = (a.rect[1] - b.rect[3]) / size;
        if a.upright && b.upright && keep(a.size) && keep(b.size) && overlaps_across(a.rect, b.rect) && (-0.3..2.0).contains(&gap) {
            gaps.push(gap);
        }
    }
    gaps.retain(|g| g.is_finite() && (-0.3..2.0).contains(g));
    if gaps.len() < 3 {
        return None;
    }
    gaps.sort_by(f64::total_cmp);
    gaps.get(gaps.len() / 2).copied()
}

/// The two boxes share some of their horizontal extent (one sits in the other's column).
fn overlaps_across(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2]
}

/// `blocks` joined into paragraphs by the whitespace between them.
fn merge(blocks: &[Block], rules: &Rules) -> Vec<Block> {
    // The line gap per font size, measured once per size seen.
    let mut gaps: Vec<(f64, f64)> = Vec::new();
    let mut gap_for = |size: f64| match gaps.iter().find(|(s, _)| *s == size) {
        Some((_, g)) => *g,
        None => {
            let g = line_gap(blocks, size);
            gaps.push((size, g));
            g
        }
    };
    let mut out: Vec<Block> = Vec::new();
    // The bottom of the row the current paragraph ends with, and that row's body font size.
    let mut last_row: Option<(f64, f64)> = None;
    // Where the paragraph's first and last rows start.
    let (mut first_left, mut last_left) = (0.0, 0.0);
    // The paragraph opens with a bold run-in label ("RQ1:"), and its last run is italic.
    let (mut labelled, mut last_italic) = (false, false);
    for b in blocks {
        let (b_first_left, b_last_left) = (b.lines.first().map_or(b.rect[0], |l| l.0[0]), b.lines.last().map_or(b.rect[0], |l| l.0[0]));
        let join = out.last().zip(last_row).and_then(|(g, (bottom, size))| {
            if !(g.upright && b.upright) {
                return None;
            }
            let l = size.max(b.size);
            // Sub- and superscripts, fractions and math sit further off the row's bottom than
            // text in the body size does.
            let loose = b.size < size * 0.85 || b.math > 0 || g.math > 0;
            let (below, above) = if loose { (1.0, 1.3) } else { (0.3, 0.8) };
            // A run on the paragraph's last row, just right of where the paragraph reaches (a
            // raised footnote marker, a subscript, a formula or a run-in heading's text).
            let same_row =
                b.rect[1] >= bottom - below * l && b.rect[1] <= bottom + above * l && b.rect[0] >= g.rect[0] - l && b.rect[0] - g.rect[2] < 1.5 * l;
            // A block of several rows whose first row is that last row: the paragraph goes on
            // after an inline formula, which the reading blocks cut out into runs of their own.
            // Its top is on the row (above the row's bottom), not on the row below.
            let continues_row =
                b.rows > 1 && b.rect[3] > bottom + 0.3 * l && b.rect[3] < bottom + 1.6 * l && b.rect[0] <= g.rect[2] && b.rect[1] < bottom;
            // The next row: below, in the same column, about the same size, no wider a gap than
            // the page's lines of that size usually have, and not a list item of its own.
            let gap = (bottom - b.rect[3]) / l;
            // A first-line indent starts a paragraph where the spacing doesn't: the row starts
            // well right of the row above, and the paragraph isn't a list item or one with a
            // hanging indent, whose later rows all start right of its first.
            let indent = (b_first_left - last_left) / l;
            let hanging = labelled || last_left > first_left + 0.5 * l || g.text.starts_with(is_bullet) || starts_with_label(&g.text);
            // A sentence left open, going on in lower case on the next row, goes on whatever the
            // row's indent: an item's hanging indent can look like a first-line one.
            let unfinished = !ends_sentence(&g.text) && starts_lowercase(&b.text);
            let indented = (rules.indent_min..=rules.indent_max).contains(&indent) && !hanging && !unfinished;
            // After a labelled item set in italics ("RQ1: How …?"), upright text on a row of its
            // own is the discussion that follows it.
            let after_item = labelled && last_italic && !b.italic && !unfinished;
            // A bold term back left of the row above, after a finished sentence, opens the next
            // entry of a description list ("LastRead  Such edges are …").
            let next_term = b.bold && ends_sentence(&g.text) && (last_left - b_first_left) / l >= 0.8;
            let next_row = gap >= -0.3
                && !indented
                && !after_item
                && !next_term
                && !is_run_in_label(b)
                && gap <= gap_for(b.size) + rules.paragraph_gap
                // A short run that opens the row may end left of a first row's indent.
                && (overlaps_across(g.rect, b.rect) || (b.rect[0] >= g.rect[0] - 2.0 * l && b.rect[0] < g.rect[2]))
                && size.min(b.size) >= l * 0.75
                && !b.text.starts_with(is_bullet)
                // A word broken at a hyphen goes on, even where its rest looks like a label.
                && (!starts_list_item(&b.text) || g.text.ends_with('-'));
            if same_row {
                Some(Join::SameRow)
            } else if continues_row || next_row {
                Some(Join::NewRow)
            } else {
                None
            }
        });
        match (join, out.last_mut(), last_row) {
            (Some(join), Some(g), Some((bottom, size))) => {
                join_text(&mut g.text, &b.text, join, &rules.compound_ends);
                g.rect = [g.rect[0].min(b.rect[0]), g.rect[1].min(b.rect[1]), g.rect[2].max(b.rect[2]), g.rect[3].max(b.rect[3])];
                g.rows += b.rows;
                g.math += b.math;
                g.chars += b.chars;
                g.words += b.words;
                g.lines.extend_from_slice(&b.lines);
                g.ink.extend_from_slice(&b.ink);
                g.row_texts.extend_from_slice(&b.row_texts);
                last_row = Some(match join {
                    // Only a run in the body size moves the row's bottom: a subscript hangs
                    // below it without starting a row.
                    Join::SameRow if b.size >= size * 0.85 && b.math == 0 => (bottom.min(b.rect[1]), size),
                    Join::SameRow => (bottom, size),
                    Join::NewRow => (b.rect[1], b.size),
                });
                if join == Join::NewRow {
                    last_left = b_last_left;
                }
                last_italic = b.italic;
            }
            _ => {
                out.push(b.clone());
                last_row = Some((b.rect[1], b.size));
                (first_left, last_left) = (b_first_left, b_last_left);
                (labelled, last_italic) = (is_run_in_label(b), b.italic);
            }
        }
    }
    out
}

/// Whether `text` ends a sentence or clause: ".", "!", "?", ":" or ";", maybe followed by closing
/// brackets and quotes.
fn ends_sentence(text: &str) -> bool {
    text.trim_end().trim_end_matches([')', ']', '"', '\'', '”', '’', '»']).ends_with(['.', '!', '?', ':', ';'])
}

/// Whether `text`'s first word, past opening brackets and quotes, starts in lower case.
fn starts_lowercase(text: &str) -> bool {
    text.trim_start().trim_start_matches(['(', '[', '"', '\'', '“', '‘', '«']).chars().next().is_some_and(char::is_lowercase)
}

/// Whether `text` starts with a list label: "1.", "2)", "(a)", "iv.".
fn starts_with_label(text: &str) -> bool {
    let word = text.split_whitespace().next().unwrap_or_default();
    let inner = word.trim_start_matches('(').trim_end_matches(['.', ')']);
    word.len() > inner.len()
        && (1..=4).contains(&inner.len())
        && (inner.chars().all(|c| c.is_ascii_digit()) || inner.chars().all(|c| c.is_ascii_lowercase()) && inner.len() <= 2)
}

/// A font named bold, including TeX's Computer Modern and Latin Modern bold faces (CMBX10,
/// LMRoman10-Bold), which often carry no bold flag.
fn is_bold_font(base_font: &str) -> bool {
    let name = base_font.rsplit('+').next().unwrap_or(base_font).to_ascii_lowercase();
    ["bold", "black", "heavy", "semibold", "demi"].iter().any(|w| name.contains(w))
        || name.starts_with("cmbx")
        || name.starts_with("cmb10")
        || libertine_style(&name).is_some_and(|s| s.contains(['b', 'z']))
}

/// `base_font` (any case, subset prefix aside) contains one of `parts` (any case).
fn font_named(base_font: &str, parts: &[String]) -> bool {
    let name = base_font.rsplit('+').next().unwrap_or(base_font).to_lowercase();
    parts.iter().any(|p| name.contains(&p.to_lowercase()))
}

/// The style letters of a Linux Libertine or Biolinum font name (lower case): "b" in
/// LinLibertineTB, "bi" in LinLibertineTBI, "z" (semibold) in LinLibertineTZ; "" for the
/// regular LinLibertineT.
fn libertine_style(name: &str) -> Option<&str> {
    ["linlibertine", "linbiolinum"].iter().find_map(|p| name.strip_prefix(p)).map(|rest| rest.get(1..).unwrap_or_default())
}

/// A font named italic or oblique, including Computer Modern's (CMTI10, CMSL10).
fn is_italic_font(base_font: &str) -> bool {
    let name = base_font.rsplit('+').next().unwrap_or(base_font).to_ascii_lowercase();
    name.contains("italic")
        || name.contains("oblique")
        || name.starts_with("cmti")
        || name.starts_with("cmsl")
        || name.starts_with("cmbxti")
        || libertine_style(&name).is_some_and(|s| s.contains('i'))
}

/// A bold label that opens an item or a run-in heading: "RQ1:", "Definition 2.", "Note:".
fn is_run_in_label(b: &Block) -> bool {
    b.bold && b.rows == 1 && b.text.chars().count() <= 30 && b.text.split_whitespace().count() <= 4 && b.text.trim_end().ends_with([':', '.'])
}

/// Whether a row starting with `text` is a numbered list item: a label followed by a capital
/// ("2. The", "(b) Each") or a label alone (set as a run of its own), not a number wrapped to
/// the row's start.
fn starts_list_item(text: &str) -> bool {
    let mut words = text.split_whitespace();
    let label = words.next().is_some_and(starts_with_label);
    label && words.next().is_none_or(|w| w.starts_with(char::is_uppercase))
}

/// How a block joins the paragraph before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Join {
    /// It goes on along the paragraph's last row.
    SameRow,
    /// It starts the next row (or goes on from the last row onto further ones).
    NewRow,
}

/// The second halves of compounds that keep their hyphen when it falls at a row's end
/// ("semantics-" + "aware"), where a hyphenated word ("detec-" + "tion") loses it.
const COMPOUND_ENDS: [&str; 24] = [
    "aware",
    "based",
    "specific",
    "level",
    "driven",
    "oriented",
    "like",
    "free",
    "wise",
    "related",
    "dependent",
    "independent",
    "agnostic",
    "scale",
    "grained",
    "intensive",
    "sensitive",
    "friendly",
    "only",
    "time",
    "world",
    "art",
    "of",
    "the",
];

/// Append `next` to `text`: a word hyphenated at a row's end ("exam-" + "ple") is put back
/// together, a compound ("language-" + "specific") keeps its hyphen, anything else gets a
/// space.
fn join_text(text: &mut String, next: &str, join: Join, compound_ends: &[String]) {
    let mut before = text.chars().rev();
    let hyphenated = join == Join::NewRow && before.next() == Some('-') && before.next().is_some_and(char::is_alphabetic);
    if hyphenated && next.starts_with(|c: char| c.is_lowercase()) {
        let word = next.split(|c: char| !c.is_alphabetic()).next().unwrap_or_default();
        if !COMPOUND_ENDS.contains(&word) && !compound_ends.iter().any(|w| w == word) {
            text.pop();
        }
    } else if hyphenated && next.starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit()) {
        // A name broken at its own hyphen ("SEED-" + "1", "non-" + "English") keeps it, unspaced.
    } else if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(next);
}

/// `text` without the spaces joining puts inside brackets and before punctuation, where runs
/// touched ("(" and a linked "Smith 2020", "orange" and ";"). A space before a number that
/// starts with its decimal point (".63", ",5") stays.
fn tidy_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' {
            let mut ahead = chars.clone();
            let next = ahead.next();
            let decimal = matches!(next, Some('.' | ',')) && ahead.next().is_some_and(|d| d.is_ascii_digit());
            if !decimal && (out.ends_with(['(', '[']) || next.is_some_and(|n| matches!(n, ')' | ']' | ',' | ';' | ':' | '.' | '!' | '?'))) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// `text` with every run of whitespace as one space, trimmed.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` (normalized) is only a page number: a number or Roman numeral, maybe with "Page",
/// dashes around it, or "of"/"/" and the page count.
fn is_page_number(text: &str) -> bool {
    if text.chars().count() > 24 {
        return false;
    }
    let lower = text.to_lowercase();
    let core = lower.trim_matches(|c: char| matches!(c, '-' | '–' | '—' | ' ' | '|' | '·' | '•'));
    let core = ["page ", "seite ", "p. ", "s. "].iter().find_map(|p| core.strip_prefix(p)).unwrap_or(core).trim();
    let number = |s: &str| {
        let s = s.trim();
        (!s.is_empty() && s.len() <= 5 && s.chars().all(|c| c.is_ascii_digit()))
            || (!s.is_empty() && s.len() <= 8 && s.chars().all(|c| matches!(c, 'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm')))
    };
    if number(core) {
        return true;
    }
    [" of ", " von ", "/"].iter().any(|sep| core.split_once(sep).is_some_and(|(a, b)| number(a) && number(b)))
}

/// `base_font` (a subset prefix such as `ABCDEF+` allowed) is a math font: TeX's math italic,
/// symbols and extensions, the AMS symbol fonts, and fonts named for math (Cambria Math, STIX
/// Two Math, Latin Modern Math).
fn is_math_font(base_font: &str) -> bool {
    let name = base_font.split_once('+').map_or(base_font, |(_, n)| n).to_lowercase();
    ["cmmi", "cmsy", "cmex", "cmbsy", "msam", "msbm", "eufm", "rsfs", "math", "stix", "esint", "wasy"].iter().any(|m| name.contains(m))
}

/// A paragraph is a display equation: at least a third of it is set in math fonts, and it has
/// fewer than five words of prose ("where", "MultiHead", "Concat" are allowed; a sentence
/// with formulas in it is not an equation).
fn is_equation(g: &Block) -> bool {
    g.math > 0 && g.math * 3 >= g.chars && g.words < 5
}

/// The words of three letters or more in `text`.
fn prose_words(text: &str) -> usize {
    text.split(|c: char| !c.is_alphabetic()).filter(|w| w.chars().count() >= 3).count()
}

/// `text` is only an equation number: "(1)", "(12a)", "(3.4)".
fn is_equation_number(text: &str) -> bool {
    text.strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .is_some_and(|n| n.len() <= 8 && n.starts_with(|c: char| c.is_ascii_digit()) && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'))
}

/// A bullet, a list marker or a footnote symbol: never read.
fn is_bullet(c: char) -> bool {
    matches!(
        c,
        '•' | '◦' | '▪' | '▫' | '■' | '□' | '●' | '○' | '‣' | '⁃' | '∙' | '➢' | '►' | '▶' | '▸' | '✓' | '✔' | '✗' | '★' | '☐' | '☑' | '◆'
            | '◇' | '❖' | '∗' | '*' | '†' | '‡' | '§' | '¶'
            // Word's Symbol-font bullets, mapped to the private use area.
            | '\u{f0b7}' | '\u{f0a7}'
    )
}

/// `text` (normalized) without bullets and footnote symbols: words made only of them, the ones
/// a word opens with ("•First"), and a dash opening the text.
fn strip_bullets(text: &str) -> String {
    let mut words: Vec<&str> = text.split(' ').map(|w| w.trim_start_matches(is_bullet)).filter(|w| !w.is_empty()).collect();
    if words.first().is_some_and(|w| matches!(*w, "-" | "–" | "—")) {
        words.remove(0);
    }
    words.join(" ")
}

/// A way of citing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CitationStyle {
    /// "[13]", "[1, 4–6]".
    Numeric,
    /// "(Smith, 2020)", "(Smith & Jones 2019; Lee et al., 2021a)".
    AuthorYear,
}

/// The citation style `texts` (one page) use most, if any.
fn citation_style<'a>(texts: impl Iterator<Item = &'a str>) -> Option<CitationStyle> {
    let (mut numeric, mut author_year) = (0usize, 0usize);
    for t in texts {
        numeric += citations(t, CitationStyle::Numeric).len();
        author_year += citations(t, CitationStyle::AuthorYear).len();
    }
    match (numeric, author_year) {
        (0, 0) => None,
        (n, a) if n >= a => Some(CitationStyle::Numeric),
        _ => Some(CitationStyle::AuthorYear),
    }
}

/// The byte ranges of `text`'s citations in `style`, brackets included.
fn citations(text: &str, style: CitationStyle) -> Vec<Range<usize>> {
    let (open, close) = match style {
        CitationStyle::Numeric => ('[', ']'),
        CitationStyle::AuthorYear => ('(', ')'),
    };
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(start) = text.get(from..).and_then(|t| t.find(open)).map(|i| i + from) {
        let inner_start = start + open.len_utf8();
        let Some(len) = text.get(inner_start..).and_then(|t| t.find(close)) else { break };
        let end = inner_start + len + close.len_utf8();
        let inner = text.get(inner_start..inner_start + len).unwrap_or_default();
        let ok = match style {
            CitationStyle::Numeric => {
                !inner.is_empty()
                    && inner.len() <= 40
                    && inner.chars().any(|c| c.is_ascii_digit())
                    && inner.chars().all(|c| c.is_ascii_digit() || matches!(c, ',' | ';' | '-' | '–' | '—' | ' '))
            }
            CitationStyle::AuthorYear => author_year(inner),
        };
        if ok {
            out.push(start..end);
            from = end;
        } else {
            from = inner_start;
        }
    }
    out
}

/// `inner` (between parentheses) reads as author-year citations: it starts with a name (or
/// "see", "e.g.", "cf."), holds a year from 1500 to 2099, and nothing nested.
fn author_year(inner: &str) -> bool {
    let inner = inner.trim();
    if inner.len() > 600 || inner.contains('(') {
        return false;
    }
    let lower = inner.to_lowercase();
    let rest = ["see also ", "see ", "e.g., ", "e.g. ", "cf. ", "vgl. "].iter().find_map(|p| lower.strip_prefix(p).map(|r| inner.len() - r.len()));
    let names = inner.get(rest.unwrap_or(0)..).unwrap_or_default();
    if !names.starts_with(|c: char| c.is_uppercase()) {
        return false;
    }
    // A year: four digits on their own, maybe followed by a letter (2020a).
    let bytes = names.as_bytes();
    (0..bytes.len()).any(|i| {
        let four = bytes.get(i..i + 4).is_some_and(|w| w.iter().all(u8::is_ascii_digit));
        let alone =
            i.checked_sub(1).and_then(|j| bytes.get(j)).is_none_or(|b| !b.is_ascii_digit()) && bytes.get(i + 4).is_none_or(|b| !b.is_ascii_digit());
        four && alone && names.get(i..i + 4).and_then(|y| y.parse::<u32>().ok()).is_some_and(|y| (1500..2100).contains(&y))
    })
}

/// `text` without its citations in `style`, and without the space left before punctuation.
fn strip_citations(text: &str, style: CitationStyle) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for r in citations(text, style) {
        out.push_str(text.get(at..r.start).unwrap_or_default());
        at = r.end;
    }
    out.push_str(text.get(at..).unwrap_or_default());
    let out = normalize(&out);
    let mut tidy = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' && chars.peek().is_some_and(|n| matches!(n, '.' | ',' | ';' | ':' | '!' | '?' | ')')) {
            continue;
        }
        tidy.push(c);
    }
    tidy
}

/// `text` cut into the pieces it is spoken in: whole sentences where they fit in
/// [`MAX_CHUNK_CHARS`], short ones joined. Every piece is non-empty and at most
/// [`MAX_CHUNK_CHARS`] characters; the pieces hold all of `text`'s words in order.
pub fn speech_chunks(text: &str) -> Vec<String> {
    speech_pieces(text).iter().map(|piece| piece.iter().filter_map(|r| text.get(r.clone())).collect::<Vec<_>>().join(" ")).collect()
}

/// [`speech_chunks`] as byte ranges of `text`: each piece is the sentences (or the parts of an
/// overlong one) it joins, in order.
pub fn speech_pieces(text: &str) -> Vec<Vec<Range<usize>>> {
    let mut pieces: Vec<(Vec<Range<usize>>, usize)> = Vec::new();
    for sentence in sentences(text) {
        for part in cut(sentence, MAX_CHUNK_CHARS) {
            let range = offset_in(text, part)..offset_in(text, part) + part.len();
            let chars = part.chars().count();
            match pieces.last_mut() {
                Some((ranges, len)) if *len + 1 + chars <= JOIN_CHARS => {
                    ranges.push(range);
                    *len += 1 + chars;
                }
                _ => pieces.push((vec![range], chars)),
            }
        }
    }
    pieces.into_iter().map(|(ranges, _)| ranges).collect()
}

/// Where `part`, a slice of `text`, starts in it.
fn offset_in(text: &str, part: &str) -> usize {
    (part.as_ptr() as usize).saturating_sub(text.as_ptr() as usize).min(text.len())
}

/// Sentences shorter than this are highlighted together with the next while they are read.
pub const MIN_HIGHLIGHT_WORDS: usize = 8;

/// The bytes of `text` to highlight while the sentence at byte `at` is read: its sentences
/// grouped in reading order until each group has at least `min_words` words, the last group
/// taking any short remainder.
pub fn highlight_range(text: &str, at: usize, min_words: usize) -> Range<usize> {
    let mut groups: Vec<(Range<usize>, usize)> = Vec::new();
    for s in sentences(text) {
        let start = offset_in(text, s);
        let range = start..start + s.len();
        let words = s.split_whitespace().count();
        match groups.last_mut() {
            Some((g, n)) if *n < min_words => {
                g.end = range.end;
                *n += words;
            }
            _ => groups.push((range, words)),
        }
    }
    // A short last group joins the one before it.
    if groups.len() > 1
        && groups.last().is_some_and(|(_, n)| *n < min_words)
        && let Some((last, _)) = groups.pop()
        && let Some((g, _)) = groups.last_mut()
    {
        g.end = last.end;
    }
    groups.into_iter().map(|(g, _)| g).find(|g| at < g.end).unwrap_or(0..text.len())
}

/// The sentences of `text`, trimmed and non-empty. A sentence ends after `.`, `!`, `?`, `…` or
/// their CJK forms (with any closing quotes or brackets) where whitespace or the end follows,
/// except a `.` before a lowercase word or a digit, or right after a CJK full stop, which needs
/// no space.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let cjk = matches!(c, '。' | '！' | '？');
        if !(cjk || matches!(c, '.' | '!' | '?' | '…')) {
            continue;
        }
        // Closing punctuation stays with its sentence.
        let mut end = i + c.len_utf8();
        while let Some(&(j, next)) = chars.peek() {
            if matches!(next, '"' | '\'' | ')' | ']' | '”' | '’' | '」' | '』' | '）') {
                end = j + next.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        // A full stop before a lowercase word or a number ends an abbreviation ("Fig. 3",
        // "e.g. this"), not a sentence.
        let continues = text.get(end..).map(str::trim_start).and_then(|r| r.chars().next()).is_some_and(|n| n.is_lowercase() || n.is_ascii_digit());
        if cjk || (chars.peek().is_none_or(|(_, next)| next.is_whitespace()) && !(c == '.' && continues)) {
            if let Some(s) = text.get(start..end).map(str::trim).filter(|s| !s.is_empty()) {
                out.push(s);
            }
            start = end;
        }
    }
    if let Some(s) = text.get(start..).map(str::trim).filter(|s| !s.is_empty()) {
        out.push(s);
    }
    out
}

/// `sentence` in pieces of at most `max` characters (`max` ≥ 1), cut at the last space that
/// fits, or mid-word when a word alone is longer. Pieces are trimmed and non-empty.
fn cut(sentence: &str, max: usize) -> Vec<&str> {
    let max = max.max(1);
    let mut out = Vec::new();
    let mut rest = sentence.trim();
    while !rest.is_empty() {
        // The byte offset just past `max` characters, or the whole rest.
        let Some((limit, _)) = rest.char_indices().nth(max) else {
            out.push(rest);
            break;
        };
        let head = rest.get(..limit).unwrap_or(rest);
        let at = head.rfind(char::is_whitespace).filter(|&at| at > 0).unwrap_or(limit);
        let (piece, tail) = rest.split_at_checked(at).unwrap_or((rest, ""));
        let piece = piece.trim();
        if !piece.is_empty() {
            out.push(piece);
        }
        rest = tail.trim_start();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_split_at_full_stops_followed_by_space() {
        assert_eq!(sentences("One. Two! Three? Four"), ["One.", "Two!", "Three?", "Four"]);
        // A decimal point or an abbreviation glued to the next word is no sentence end.
        assert_eq!(sentences("Pi is 3.14 today.Really"), ["Pi is 3.14 today.Really"]);
        assert_eq!(sentences("He said \"stop.\" Then left."), ["He said \"stop.\"", "Then left."]);
        assert_eq!(sentences("今日は晴れ。明日は雨？"), ["今日は晴れ。", "明日は雨？"]);
        assert!(sentences("").is_empty());
        assert!(sentences("   ").is_empty());
        assert_eq!(sentences("..."), ["..."]);
        assert_eq!(sentences("See Fig. 3 and e.g. this. Next"), ["See Fig. 3 and e.g. this.", "Next"]);
    }

    #[test]
    fn chunks_join_short_sentences_and_keep_every_word() {
        let text = "Yes. No. Maybe.";
        assert_eq!(speech_chunks(text), ["Yes. No. Maybe."]);
        let long = format!("{} {}", "word ".repeat(60).trim(), "Second sentence here.");
        let chunks = speech_chunks(&long);
        assert!(chunks.iter().all(|c| !c.is_empty() && c.chars().count() <= MAX_CHUNK_CHARS));
        assert_eq!(chunks.join(" ").split_whitespace().collect::<Vec<_>>(), long.split_whitespace().collect::<Vec<_>>());
    }

    #[test]
    fn overlong_sentences_are_cut_at_spaces_or_char_boundaries() {
        let words = "abcdefghi ".repeat(100);
        for chunk in speech_chunks(&words) {
            assert!(chunk.chars().count() <= MAX_CHUNK_CHARS);
            assert!(chunk.split(' ').all(|w| w == "abcdefghi"), "a word was cut: {chunk:?}");
        }
        // One huge word of multi-byte characters: cut mid-word, never mid-character.
        let word = "é".repeat(MAX_CHUNK_CHARS * 2 + 7);
        let chunks = speech_chunks(&word);
        assert_eq!(chunks.iter().map(|c| c.chars().count()).collect::<Vec<_>>(), [MAX_CHUNK_CHARS, MAX_CHUNK_CHARS, 7]);
        assert_eq!(chunks.concat(), word);
    }

    #[test]
    fn cut_handles_tiny_limits_and_whitespace() {
        assert_eq!(cut("a b c", 1), ["a", "b", "c"]);
        assert_eq!(cut("  ab  ", 0), ["a", "b"]);
        assert!(cut("   ", 5).is_empty());
    }

    fn block(index: usize, rect: [f64; 4], size: f64, text: &str) -> Block {
        Block {
            index,
            rect,
            size,
            upright: true,
            bold: false,
            italic: false,
            rows: 1,
            chars: text.chars().count(),
            math: 0,
            words: prose_words(text),
            text: text.into(),
            lines: vec![(rect, text.chars().count())],
            ink: vec![Vec::new()],
            row_texts: vec![text.into()],
            measured: false,
        }
    }

    #[test]
    fn style_runs_and_tightly_spaced_rows_join_and_a_wider_gap_breaks() {
        // 10 pt text on 12 pt line pitch: rows 2 pt apart; a paragraph gap adds 8 pt more.
        let blocks = [
            block(0, [72.0, 700.0, 200.0, 710.0], 10.0, "First row with an"),
            block(1, [202.0, 700.0, 240.0, 710.0], 10.0, "italic"),
            block(2, [242.0, 700.0, 300.0, 710.0], 10.0, "word and a hy-"),
            block(3, [72.0, 688.0, 300.0, 698.0], 10.0, "phen on the second row."),
            block(4, [72.0, 676.0, 280.0, 686.0], 10.0, "Third row ends it."),
            block(5, [80.0, 656.0, 300.0, 666.0], 10.0, "A new paragraph, indented."),
            block(6, [72.0, 644.0, 300.0, 654.0], 10.0, "Its second row."),
        ];
        let merged = merge(&blocks, &Rules::default());
        let texts: Vec<&str> = merged.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(
            texts,
            ["First row with an italic word and a hyphen on the second row. Third row ends it.", "A new paragraph, indented. Its second row."]
        );
        assert_eq!(merged.iter().map(|b| b.index).collect::<Vec<_>>(), [0, 5]);
        assert_eq!(merged[0].rect, [72.0, 676.0, 300.0, 710.0]);
    }

    #[test]
    fn a_first_line_indent_starts_a_paragraph_without_extra_spacing() {
        let blocks = [
            block(0, [87.0, 700.0, 300.0, 710.0], 10.0, "An indented first row"),
            block(1, [72.0, 688.0, 300.0, 698.0], 10.0, "and its second."),
            block(2, [87.0, 676.0, 300.0, 686.0], 10.0, "The next paragraph, same spacing,"),
            block(3, [72.0, 664.0, 300.0, 674.0], 10.0, "goes on."),
            // A list item with a hanging indent keeps its later rows.
            block(4, [72.0, 652.0, 300.0, 662.0], 10.0, "1. A numbered item"),
            block(5, [84.0, 640.0, 300.0, 650.0], 10.0, "with a hanging indent"),
            block(6, [84.0, 628.0, 300.0, 638.0], 10.0, "over three rows."),
        ];
        let texts: Vec<String> = merge(&blocks, &Rules::default()).into_iter().map(|b| b.text).collect();
        assert_eq!(
            texts,
            [
                "An indented first row and its second.",
                "The next paragraph, same spacing, goes on.",
                "1. A numbered item with a hanging indent over three rows."
            ]
        );
    }

    #[test]
    fn an_indented_last_row_of_a_block_starts_a_paragraph() {
        let rows = [
            ([72.0, 700.0, 300.0, 710.0], 20),
            ([72.0, 688.0, 300.0, 698.0], 20),
            ([72.0, 676.0, 200.0, 686.0], 12),
            ([87.0, 664.0, 300.0, 674.0], 20),
        ];
        let texts = ["The first paragraph", "runs over three", "rows and ends.", "The next one starts"];
        let b = Block {
            rows: 4,
            lines: rows.to_vec(),
            ink: vec![Vec::new(); 4],
            row_texts: texts.iter().map(|t| t.to_string()).collect(),
            measured: true,
            ..block(0, [72.0, 664.0, 300.0, 710.0], 10.0, &texts.join(" "))
        };
        let parts: Vec<String> = split_rows(&b, 0.2, &Rules::default()).into_iter().map(|p| p.text).collect();
        assert_eq!(parts, ["The first paragraph runs over three rows and ends.", "The next one starts"]);
        // A hanging indent's last row stays.
        let hanging = Block { lines: vec![rows[0], ([84.0, 688.0, 300.0, 698.0], 20), ([84.0, 676.0, 200.0, 686.0], 12)], rows: 3, ..b.clone() };
        let hanging = Block { row_texts: hanging.row_texts.get(..3).unwrap_or_default().to_vec(), ink: vec![Vec::new(); 3], ..hanging };
        assert_eq!(split_rows(&hanging, 0.2, &Rules::default()).len(), 1);
    }

    #[test]
    fn a_figure_is_the_sparse_rows_next_to_its_caption() {
        // Runs as dense as prose: a character every half em.
        let row = |bottom: f64, runs: &[(f64, f64)]| TextRow {
            bottom,
            top: bottom + 9.0,
            size: 10.0,
            runs: runs.iter().map(|&(a, b)| (a, b, ((b - a) / 5.0) as usize)).collect(),
        };
        let col = (100.0, 500.0);
        // Top to bottom: prose, a short last row, a figure (two labels apart, a lone tick label,
        // a centred axis title), its caption, then prose again.
        let rows = vec![
            row(700.0, &[(100.0, 500.0)]),
            row(688.0, &[(100.0, 260.0)]),
            row(660.0, &[(150.0, 220.0), (330.0, 420.0)]),
            row(640.0, &[(112.0, 124.0)]),
            row(620.0, &[(220.0, 380.0)]),
            row(600.0, &[(160.0, 440.0)]),
            row(580.0, &[(100.0, 500.0)]),
        ];
        assert!(
            is_sparse(&rows[2], col, &Rules::default())
                && is_sparse(&rows[4], col, &Rules::default())
                && !is_sparse(&rows[3], col, &Rules::default())
                && !is_sparse(&rows[1], col, &Rules::default())
        );
        let passage = |rect: [f64; 4], text: &str, role: Role| Passage {
            page: 0,
            paragraph: 0,
            rect,
            text: text.into(),
            lines: Vec::new(),
            ink: Vec::new(),
            rows_text: Vec::new(),
            heading: None,
            role,
        };
        let mut passages = vec![
            passage([100.0, 688.0, 500.0, 709.0], "Prose that ends on a short row.", Role::Text),
            passage([150.0, 660.0, 420.0, 669.0], "(a) A sub-caption with a sentence. And another.", Role::Text),
            passage([220.0, 620.0, 380.0, 629.0], "Accuracy", Role::Text),
            passage([160.0, 600.0, 440.0, 609.0], "Fig. 1: The figure.", Role::Caption),
            passage([100.0, 580.0, 500.0, 589.0], "More prose below.", Role::Text),
        ];
        let regions = figure_regions(&passages, &rows, &[col], 10.0, &Rules::default());
        mark_figure_regions(&mut passages, &regions);
        let roles: Vec<Role> = passages.iter().map(|p| p.role).collect();
        assert_eq!(roles, [Role::Text, Role::FigureText, Role::FigureText, Role::Caption, Role::Text]);
        assert!(figure_regions(&passages, &[], &[col], 10.0, &Rules::default()).is_empty(), "no rows, no area");
        // A table row drawn as one text object across its cells: wide, but few characters.
        let table_row = TextRow { bottom: 0.0, top: 9.0, size: 10.0, runs: vec![(105.0, 480.0, 30)] };
        assert!(is_sparse(&table_row, col, &Rules::default()));
        // A list item: a short label, a gap, then continuous text.
        let item = TextRow { bottom: 0.0, top: 9.0, size: 10.0, runs: vec![(100.0, 110.0, 2), (130.0, 500.0, 74)] };
        assert!(!is_sparse(&item, col, &Rules::default()));
        // Two wide gaps along the row are a table's cells, label or not.
        let cells = TextRow { bottom: 0.0, top: 9.0, size: 10.0, runs: vec![(100.0, 110.0, 2), (130.0, 250.0, 24), (300.0, 500.0, 40)] };
        assert!(is_sparse(&cells, col, &Rules::default()));
    }

    #[test]
    fn captions_and_text_inside_figures_and_tables_are_told_apart() {
        for caption in [
            "Figure 1: The model.",
            "Fig. 3. Overview of the study",
            "Table 2: Results",
            "Table A1. Extra",
            "Fig. 4 Accuracy over time",
            "Algorithm 1 Merge",
        ] {
            assert!(is_caption(caption, &[]), "{caption}");
        }
        for prose in ["Figure 3 shows the results.", "Tables are hard.", "Figures", "Table of contents", "Fig. this out"] {
            assert!(!is_caption(prose, &[]), "{prose}");
        }
        let body = 10.0;
        let roles = |b: Block| role(&b, &b.text.clone(), body, &Rules::default());
        assert_eq!(roles(block(0, [0.0; 4], 10.0, "Add & Norm")), Role::FigureText, "a label");
        assert_eq!(roles(block(0, [0.0; 4], 7.0, "Accuracy over training steps (log scale)")), Role::FigureText, "small print");
        assert_eq!(
            roles(block(0, [0.0; 4], 10.0, "Layer Type Complexity per Layer Sequential Operations Maximum Path Length")),
            Role::FigureText,
            "a table row"
        );
        assert_eq!(roles(block(0, [0.0; 4], 10.0, "Figure 1: The Transformer - model architecture.")), Role::Caption);
        assert_eq!(roles(block(0, [0.0; 4], 10.0, "The model reads well, and it is fast.")), Role::Text);
        assert_eq!(roles(block(0, [0.0; 4], 7.0, "1 A footnote with a sentence, read as text.")), Role::Text, "numbered: heading-like, kept");
        assert_eq!(roles(Block { bold: true, ..block(0, [0.0; 4], 10.0, "Abstract") }), Role::Text, "a bold heading");
        assert_eq!(roles(block(0, [0.0; 4], 12.0, "Related Work")), Role::Text, "a larger heading");
        assert_eq!(roles(block(0, [0.0; 4], 10.0, "4.1 Dataset Curation")), Role::Text, "a numbered heading");
    }

    #[test]
    fn labelled_items_and_list_numbers_set_apart_start_paragraphs() {
        let bold = |b: Block| Block { bold: true, ..b };
        let italic = |b: Block| Block { italic: true, ..b };
        let blocks = [
            block(0, [72.0, 700.0, 300.0, 710.0], 10.0, "Intro text that ends here."),
            // A bold label, an italic question with a hanging indent, then the discussion.
            bold(block(1, [72.0, 688.0, 98.0, 698.0], 10.0, "RQ1:")),
            italic(block(2, [98.0, 688.0, 300.0, 698.0], 10.0, "How does it")),
            italic(block(3, [84.0, 676.0, 260.0, 686.0], 10.0, "work at all?")),
            block(4, [84.0, 664.0, 300.0, 674.0], 10.0, "We answer it by looking."),
            // A list number set as a run of its own.
            block(5, [72.0, 652.0, 80.0, 662.0], 10.0, "3."),
            block(6, [80.0, 652.0, 300.0, 662.0], 10.0, "We design a thing."),
        ];
        let texts: Vec<String> = merge(&blocks, &Rules::default()).into_iter().map(|b| b.text).collect();
        assert_eq!(texts, ["Intro text that ends here.", "RQ1: How does it work at all?", "We answer it by looking.", "3. We design a thing."]);
        assert!(is_bold_font("ABCDEF+CMBX10") && is_bold_font("Helvetica-Bold") && !is_bold_font("CMR10"));
        assert!(is_italic_font("XYZ+CMTI10") && is_italic_font("Times-Italic") && !is_italic_font("CMR10"));
    }

    #[test]
    fn an_open_sentence_goes_on_past_an_indent() {
        // A row starting right of the row above, as an item's hanging indent or a first-line
        // indent does: a sentence left open goes on in lower case, a capital starts anew.
        let rows = |next: &str| {
            [block(0, [64.0, 700.0, 440.0, 710.0], 10.0, "The model also sees the parts of"), block(1, [76.0, 688.0, 300.0, 698.0], 10.0, next)]
        };
        let texts = |next: &str| merge(&rows(next), &Rules::default()).into_iter().map(|b| b.text).collect::<Vec<_>>();
        assert_eq!(texts("the others, as before."), ["The model also sees the parts of the others, as before."]);
        assert_eq!(texts("Then a new paragraph."), ["The model also sees the parts of", "Then a new paragraph."]);
        // A description list: each bold term out at the left after its entry's last sentence.
        let bold = |b: Block| Block { bold: true, ..b };
        let list = [
            bold(block(0, [110.0, 700.0, 170.0, 709.0], 9.0, "LastRead")),
            block(1, [174.0, 700.0, 440.0, 709.0], 9.0, "Such edges join each use and the"),
            block(2, [134.0, 686.0, 400.0, 695.0], 9.0, "last time it was read."),
            bold(block(3, [110.0, 672.0, 170.0, 681.0], 9.0, "ComputedFrom")),
            block(4, [178.0, 672.0, 440.0, 681.0], 9.0, "These edges join the sides."),
        ];
        let texts: Vec<String> = merge(&list, &Rules::default()).into_iter().map(|b| b.text).collect();
        assert_eq!(texts, ["LastRead Such edges join each use and the last time it was read.", "ComputedFrom These edges join the sides."]);
        assert!(ends_sentence("done.") && ends_sentence("(see it).”") && ends_sentence("as follows:") && !ends_sentence("of 𝑠 siblings,"));
        assert!(
            starts_lowercase("the seeds")
                && starts_lowercase("(and more)")
                && !starts_lowercase("The")
                && !starts_lowercase("2 rows")
                && !starts_lowercase("")
        );
    }

    #[test]
    fn a_name_broken_at_its_hyphen_goes_on_left_of_an_indent() {
        let blocks = [
            // An indented first row ending in a name broken at its hyphen; its rest opens the
            // next row as a run of its own, left of where the first row starts.
            block(0, [56.0, 700.0, 400.0, 710.0], 10.0, "One example changes the picture: SEED-"),
            block(1, [46.0, 688.0, 50.0, 698.0], 10.0, "1"),
            block(2, [50.0, 688.0, 300.0, 698.0], 10.0, "reaches a high score."),
        ];
        let texts: Vec<String> = merge(&blocks, &Rules::default()).into_iter().map(|b| b.text).collect();
        assert_eq!(texts, ["One example changes the picture: SEED-1 reaches a high score."]);
        let mut text = String::from("a non-");
        join_text(&mut text, "English word", Join::NewRow, &[]);
        assert_eq!(text, "a non-English word");
    }

    #[test]
    fn rules_from_a_file_change_the_reading() {
        let rules = Rules {
            indent_min: 2.0,
            captions: vec!["Plate".into()],
            compound_ends: vec!["centric".into()],
            bold_fonts: vec!["Heavyish".into()],
            ..Rules::default()
        };
        // A 1.5 em indent starts a paragraph by default, not with indent_min = 2.
        let blocks = [
            block(0, [72.0, 700.0, 300.0, 710.0], 10.0, "A first paragraph ends here."),
            block(1, [87.0, 688.0, 300.0, 698.0], 10.0, "An indented row"),
            block(2, [72.0, 676.0, 300.0, 686.0], 10.0, "and its second."),
        ];
        assert_eq!(merge(&blocks, &Rules::default()).len(), 2);
        assert_eq!(merge(&blocks, &rules).len(), 1);
        let caption = block(0, [72.0, 700.0, 300.0, 710.0], 10.0, "Plate 3. A map.");
        assert_eq!(role(&caption, &caption.text, 10.0, &Rules::default()), Role::Text);
        assert_eq!(role(&caption, &caption.text, 10.0, &rules), Role::Caption);
        let joined = |rules: &Rules| {
            let mut text = String::from("user-");
            join_text(&mut text, "centric design", Join::NewRow, &rules.compound_ends);
            text
        };
        assert_eq!((joined(&Rules::default()).as_str(), joined(&rules).as_str()), ("usercentric design", "user-centric design"));
        assert!(font_named("ABC+SansHEAVYISH", &rules.bold_fonts) && !font_named("ABC+Sans", &rules.bold_fonts));
        let row = TextRow { bottom: 0.0, top: 10.0, size: 10.0, runs: vec![(0.0, 100.0, 22), (120.0, 200.0, 18)] };
        assert!(is_sparse(&row, (0.0, 200.0), &Rules::default()), "a 2 em gap sets cells apart");
        assert!(!is_sparse(&row, (0.0, 200.0), &Rules { cell_gap: 2.5, ..Rules::default() }));
    }

    #[test]
    fn linux_libertine_styles_are_recognized() {
        assert!(is_bold_font("RIVVHA+LinLibertineTB") && is_bold_font("LinLibertineTZ") && is_bold_font("X+LinBiolinumTB"));
        assert!(is_italic_font("VAYJAY+LinLibertineTI") && is_italic_font("LinLibertineTBI") && !is_italic_font("LinLibertineTB"));
        assert!(!is_bold_font("PHWNBH+LinLibertineT") && !is_italic_font("CHEQZC+LinBiolinumT") && !is_bold_font("LinLibertineTI"));
    }

    #[test]
    fn headings_columns_and_turned_text_stay_apart() {
        let blocks = [
            block(0, [72.0, 720.0, 300.0, 738.0], 18.0, "A heading"),
            block(1, [72.0, 700.0, 280.0, 710.0], 10.0, "Body text under it."),
            // The next column, at the same height: not the same row, not below.
            block(2, [320.0, 700.0, 520.0, 710.0], 10.0, "Column two."),
            Block { upright: false, ..block(3, [320.0, 688.0, 520.0, 698.0], 10.0, "Turned.") },
        ];
        assert_eq!(merge(&blocks, &Rules::default()).len(), 4);
    }

    #[test]
    fn page_numbers_are_recognized() {
        for yes in ["12", "xiv", "- 3 -", "— 7 —", "Page 4", "Seite 9", "3 / 10", "4 of 20", "p. 5"] {
            assert!(is_page_number(yes), "{yes}");
        }
        for no in ["12 apples", "Figure 3", "2020 was a year of change", "1.5", "", "123456"] {
            assert!(!is_page_number(no), "{no}");
        }
    }

    #[test]
    fn the_sentence_under_the_pointer_is_found_from_the_rows() {
        let text = "First sentence here. Second one starts mid row and runs on. Third.";
        let p = Passage {
            page: 0,
            paragraph: 0,
            rect: [0.0, 0.0, 100.0, 20.0],
            text: text.into(),
            lines: vec![([0.0, 10.0, 100.0, 20.0], 33), ([0.0, 0.0, 100.0, 10.0], 33)],
            ink: Vec::new(),
            rows_text: Vec::new(),
            heading: None,
            role: Role::Text,
        };
        let starts = sentence_starts(text);
        assert_eq!(starts, [0, 21, 60]);
        assert_eq!(p.sentence_at([5.0, 15.0]), 0);
        assert_eq!(p.sentence_at([90.0, 15.0]), 21, "late on the first row is the second sentence");
        assert_eq!(p.sentence_at([95.0, 5.0]), 60);
        assert_eq!(p.sentence_at([500.0, -50.0]), 60, "off the box: the nearest row");
        assert_eq!(
            p.sentence_boxes(21),
            [[0.0 + 21.0 / 66.0 * 66.0 / 33.0 * 100.0, 10.0, 100.0, 20.0], [0.0, 0.0, (60.0 - 33.0) / 33.0 * 100.0, 10.0]]
        );
        assert_eq!(p.sentence_boxes(0).len(), 1);
        // With the words' boxes known, the start snaps to the word's left edge.
        let words = vec![vec![[0.0, 10.0, 28.0, 20.0], [30.0, 10.0, 58.0, 20.0], [61.0, 10.0, 100.0, 20.0]], Vec::new()];
        let snapped = Passage { ink: words, ..p.clone() };
        assert_eq!(snapped.sentence_boxes(21).first().map(|b| b[0]), Some(61.0));
        assert_eq!(Passage { lines: Vec::new(), ..p.clone() }.sentence_boxes(0), Vec::<[f64; 4]>::new());
        assert_eq!(Passage { lines: Vec::new(), ..p.clone() }.sentence_at([5.0, 5.0]), 0);
        assert_eq!(Passage { text: String::new(), ..p }.sentence_at([95.0, 5.0]), 0);
        assert_eq!(
            row_boxes([0.0, 0.0, 10.0, 30.0], "a\nbb\nccc\ndd", 3),
            [([0.0, 20.0, 10.0, 30.0], 1), ([0.0, 10.0, 10.0, 20.0], 2), ([0.0, 0.0, 10.0, 10.0], 5)]
        );
        assert_eq!(row_boxes([0.0, 0.0, 10.0, 10.0], "", 0), [([0.0, 0.0, 10.0, 10.0], 0)]);
    }

    #[test]
    fn bookmarked_headings_are_read_as_sentences() {
        let passage = |text: &str| Passage {
            page: 2,
            paragraph: 0,
            rect: [0.0; 4],
            text: text.into(),
            lines: Vec::new(),
            ink: Vec::new(),
            rows_text: Vec::new(),
            heading: None,
            role: Role::Text,
        };
        let bookmark = |path: Vec<usize>, title: &str, page: usize| crate::BookmarkRow {
            path,
            title: title.into(),
            open: true,
            page: Some(page),
            view: pdfcraft_render::DestView::Top,
            color: None,
            italic: false,
            bold: false,
        };
        let mut passages = [passage("5 Conclusion"), passage("Conclusion"), passage("5.2 Results and Discussion"), passage("Other text.")];
        let bookmarks =
            [bookmark(vec![4], "Conclusion", 2), bookmark(vec![4, 1], "5.2 Results and discussion", 2), bookmark(vec![5], "Other text", 3)];
        mark_headings(&mut passages, &bookmarks, 2);
        assert_eq!(
            passages.iter().map(|p| p.heading).collect::<Vec<_>>(),
            [Some(1), None, Some(2), None],
            "each bookmark marks its first passage on its page"
        );
        assert_eq!(heading_sentence("5 Conclusion", true), "Chapter 5. Conclusion.");
        assert_eq!(heading_sentence("5.2 Results:", true), "Section 5.2. Results.");
        assert_eq!(heading_sentence("A.1 Proofs", true), "Appendix A.1. Proofs.");
        assert_eq!(heading_sentence("A Note on Style", true), "A Note on Style.");
        assert_eq!(heading_sentence("Why?", true), "Why?");
        assert_eq!(heading_sentence("5 Fazit", false), "5 Fazit.");
        assert_eq!(heading_sentence("2026 was a year", true), "2026 was a year.", "four digits are not a section number");
    }

    #[test]
    fn words_are_found_on_their_rows_past_removed_citations_and_hyphens() {
        // Two rows of 100 points; the text lost a citation and joins a word hyphenated over
        // the rows. Row 0's words have no boxes (spread by characters), row 1's have.
        let p = Passage {
            page: 0,
            paragraph: 0,
            rect: [0.0, 0.0, 100.0, 20.0],
            text: "As shown before, the effect holds. An example follows.".into(),
            lines: vec![([0.0, 10.0, 100.0, 20.0], 48), ([0.0, 0.0, 100.0, 10.0], 12)],
            ink: vec![Vec::new(), vec![[0.0, 0.0, 20.0, 10.0], [24.0, 0.0, 70.0, 10.0]]],
            rows_text: vec!["As shown before [13], the effect holds. An exam-".into(), "ple follows.".into()],
            heading: None,
            role: Role::Text,
        };
        let words: Vec<(usize, usize)> = p.placed().iter().map(|w| (w.byte, w.row)).collect();
        assert_eq!(words, [(0, 0), (3, 0), (9, 0), (17, 0), (21, 0), (28, 0), (35, 0), (38, 0), (46, 1)]);
        // "An" starts where the widths of the row's text before it put it.
        let an = advance("As shown before [13], the effect holds. ") / advance("As shown before [13], the effect holds. An exam-") * 100.0;
        assert!(advance("mill") > advance("illi") && advance("") == 0.0);
        // The second sentence runs from "An" to the passage's end.
        let second = p.range_boxes(35..p.text.len());
        assert_eq!(second.len(), 2);
        assert!((second[0][0] - an).abs() < 0.01 && second[1] == [0.0, 0.0, 100.0, 10.0], "{second:?}");
        // The first ends where the next starts, past the citation the text lost.
        let first = p.range_boxes(0..35);
        assert!(first.len() == 1 && first[0][2] >= an - 2.01 - 1.0 && first[0][2] < an, "{first:?}");
        // A point on "follows." finds the second sentence; one on "shown" the first.
        assert_eq!(p.sentence_at([50.0, 5.0]), 35);
        assert_eq!(p.sentence_at([15.0, 15.0]), 0);
        // A word the text lost doesn't pull later words onto a wrong match.
        let rows: Vec<(String, usize, f64, f64)> = ["the", "cat", "the", "dog", "sat"].iter().map(|w| (w.to_string(), 0, 0.0, 1.0)).collect();
        let text: Vec<(usize, String)> = ["the", "dog", "sat"].iter().enumerate().map(|(i, w)| (i, w.to_string())).collect();
        assert_eq!(align(&text, &rows), Some(vec![(0, 2), (1, 3), (2, 4)]));
    }

    #[test]
    fn pieces_keep_where_their_sentences_are_and_highlights_hold_enough_words() {
        let text = "Short one. Another short. Then a much longer sentence that has plenty of words in it. End.";
        let pieces = speech_pieces(text);
        let flat: Vec<&str> = pieces.iter().flatten().filter_map(|r| text.get(r.clone())).collect();
        assert_eq!(flat, ["Short one.", "Another short.", "Then a much longer sentence that has plenty of words in it.", "End."]);
        assert_eq!(speech_chunks(text), [text], "short sentences join into one request");
        let long = format!("{} Next.", "word ".repeat(100).trim());
        assert_eq!(speech_pieces(&long).len(), 2, "an overlong sentence is cut");
        // "Short one. Another short." has four words: it joins the next sentence.
        let three = "Short one. Another short. Then a much longer sentence that has plenty of words in it. And a last sentence long enough to stand.";
        let first = highlight_range(three, 0, 8);
        assert_eq!(three.get(first.clone()), Some("Short one. Another short. Then a much longer sentence that has plenty of words in it."));
        assert_eq!(highlight_range(three, 12, 8), first);
        assert_eq!(three.get(highlight_range(three, three.len() - 3, 8)), Some("And a last sentence long enough to stand."));
        // "End." alone is too short: it joins the group before.
        assert_eq!(highlight_range(text, text.len() - 2, 8), 0..text.len());
        assert_eq!(highlight_range("", 0, 8), 0..0);
        assert_eq!(highlight_range(text, 0, 1), 0..10);
    }

    #[test]
    fn spaces_inside_brackets_and_before_punctuation_go() {
        assert_eq!(tidy_spaces("merging ( Abel 2002 ) is hard ; see [ 4 ] , too ."), "merging (Abel 2002) is hard; see [4], too.");
        assert_eq!(tidy_spaces(" ( "), " (");
        assert_eq!(tidy_spaces("fell from .52 to .31 , ,5 and ."), "fell from .52 to .31, ,5 and.");
        let spoken = speakable_english(&tidy_spaces("it fell from .52 to .31 ."), true, NumberFormat::DecimalPoint);
        assert_eq!(spoken, "it fell from point five two to point three one.");
        assert_eq!(tidy_spaces(""), "");
    }

    #[test]
    fn citations_in_the_pages_style_are_left_out() {
        let numeric = "As shown before [13], the effect holds [1, 4–6]. See [ref] too.";
        assert_eq!(citation_style([numeric].into_iter()), Some(CitationStyle::Numeric));
        assert_eq!(strip_citations(numeric, CitationStyle::Numeric), "As shown before, the effect holds. See [ref] too.");
        let author = "Earlier work (Smith et al., 2020; Lee 2019a) disagrees (see Jones & Wu, 2018). In (2020) dollars, (not a citation).";
        assert_eq!(citation_style([author].into_iter()), Some(CitationStyle::AuthorYear));
        assert_eq!(strip_citations(author, CitationStyle::AuthorYear), "Earlier work disagrees. In (2020) dollars, (not a citation).");
        // Semicolon-separated, no comma before the year, several years for one author.
        let springer =
            "Tools compare trees (Abel 1990; Baker and van Dorn 2001; Cole et al. 2011, 2014) or graphs (Dunn et al.\n2008; Open Forum 2020).";
        assert_eq!(citation_style([springer].into_iter()), Some(CitationStyle::AuthorYear));
        assert_eq!(strip_citations(springer, CitationStyle::AuthorYear), "Tools compare trees or graphs.");
        assert_eq!(citation_style(["No citations here (really)."].into_iter()), None);
        // Unclosed and odd brackets are left alone, without panicking.
        for odd in ["[", "(", "[13", "(Smith, 2020", "]["] {
            assert_eq!(strip_citations(odd, CitationStyle::Numeric), odd);
            assert_eq!(strip_citations(odd, CitationStyle::AuthorYear), odd);
        }
    }

    #[test]
    fn a_display_equation_holds_together_and_is_recognized() {
        let math = |i, rect, size, text: &str| Block { math: text.chars().count(), words: 0, ..block(i, rect, size, text) };
        // Text, then on a row of its own: Attention(Q,K,V) = softmax(QK^T / √d_k) V   (1),
        // the fraction's numerator raised, its denominator lowered, then more text.
        let blocks = [
            block(0, [108.0, 341.6, 205.0, 350.5], 10.0, "the matrix of outputs as:"),
            math(1, [220.0, 309.3, 265.6, 318.1], 10.0, "Attention("),
            math(2, [265.6, 309.3, 296.8, 318.1], 10.0, "Q,K,V"),
            math(3, [299.0, 309.3, 354.4, 318.1], 10.0, ") = softmax("),
            math(4, [355.6, 316.0, 371.9, 324.9], 10.0, "QK"),
            math(5, [372.7, 320.2, 377.4, 326.4], 7.0, "T"),
            math(6, [358.1, 309.6, 366.4, 319.0], 10.0, "√"),
            math(7, [366.4, 301.9, 371.6, 310.7], 10.0, "d"),
            math(8, [371.6, 300.9, 375.8, 307.1], 7.0, "k"),
            math(9, [380.1, 309.3, 384.0, 318.1], 10.0, ")"),
            math(10, [384.0, 309.3, 389.8, 318.1], 10.0, "V"),
            block(11, [493.1, 309.1, 504.7, 318.0], 10.0, "(1)"),
            Block { rows: 3, ..block(12, [107.7, 261.0, 505.7, 291.7], 10.0, "The two most commonly used attention functions") },
        ];
        let merged = merge(&blocks, &Rules::default());
        let texts: Vec<&str> = merged.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(
            texts,
            ["the matrix of outputs as:", "Attention( Q,K,V ) = softmax( QK T √ d k ) V", "(1)", "The two most commonly used attention functions"]
        );
        assert!(is_equation(&merged[1]));
        assert!(!is_equation(&merged[0]) && !is_equation(&merged[3]));
        assert!(is_equation_number(merged[2].text.as_str()));
        assert!(is_math_font("LICAEO+CMMI10") && is_math_font("CambriaMath") && !is_math_font("AECCXO+NimbusRomNo9L-Regu") && !is_math_font("CMR10"));
        for no in ["(a)", "()", "(see 1)", "(1", "1)"] {
            assert!(!is_equation_number(no), "{no}");
        }
    }

    #[test]
    fn bullets_and_footnote_symbols_are_not_read() {
        assert_eq!(strip_bullets("• First item"), "First item");
        assert_eq!(strip_bullets("– Dashed item with a – dash inside"), "Dashed item with a – dash inside");
        assert_eq!(strip_bullets("Ashish Vaswani ∗ Google Brain"), "Ashish Vaswani Google Brain");
        assert_eq!(strip_bullets("•Glued ◦ ▪"), "Glued");
        assert_eq!(strip_bullets("† ‡"), "");
    }

    #[test]
    fn formulas_inside_a_paragraph_and_list_items() {
        // A paragraph of two rows, an inline formula cut into runs on its last row (a subscript
        // lower and smaller), then the rest of the paragraph as a block whose first row is that
        // row. Then two bullet items, tightly spaced.
        let mut rest = block(4, [72.0, 664.0, 300.0, 698.0], 10.0, ", and the paragraph goes on.");
        rest.rows = 3;
        let blocks = [
            Block { rows: 2, ..block(0, [72.0, 688.0, 300.0, 710.0], 10.0, "A paragraph with a formula") },
            block(1, [150.0, 688.0, 156.0, 697.0], 10.0, "h"),
            block(2, [156.0, 687.0, 160.0, 693.0], 7.0, "t"),
            block(3, [162.0, 688.0, 200.0, 697.0], 10.0, "= 6"),
            rest,
            block(5, [80.0, 642.0, 300.0, 652.0], 10.0, "• One item"),
            block(6, [80.0, 630.0, 300.0, 640.0], 10.0, "• Another item"),
        ];
        let texts: Vec<String> = merge(&blocks, &Rules::default()).into_iter().map(|b| b.text).collect();
        assert_eq!(texts, ["A paragraph with a formula h t = 6 , and the paragraph goes on.", "• One item", "• Another item"]);
    }

    #[test]
    fn line_gap_is_measured_from_the_page() {
        let tight: Vec<Block> = (0..6).map(|i| block(i, [72.0, 700.0 - 11.0 * i as f64, 300.0, 710.0 - 11.0 * i as f64], 10.0, "row")).collect();
        assert!((line_gap(&tight, 10.0) - 0.1).abs() < 1e-9);
        assert_eq!(line_gap(&tight[..1], 10.0), DEFAULT_LINE_GAP);
        // Footnotes set tighter than the body are measured on their own; a size with too few
        // rows of its own falls back to the page.
        let mut page = tight.clone();
        page.extend((0..4).map(|i| block(10 + i, [72.0, 100.0 - 7.0 * i as f64, 300.0, 107.0 - 7.0 * i as f64], 7.0, "note")));
        assert!((line_gap(&page, 10.0) - 0.1).abs() < 1e-9);
        assert!(line_gap(&page, 7.0).abs() < 1e-9);
        assert!((line_gap(&tight, 14.0) - 0.1).abs() < 1e-9);
    }

    #[test]
    fn whitespace_runs_become_single_spaces() {
        assert_eq!(normalize("  one\ttwo\n\nthree  "), "one two three");
        assert_eq!(normalize("\n\t "), "");
    }
}

/// Segmentation harness: `PDFCRAFT_READ_ALOUD_PDF=paper.pdf PDFCRAFT_READ_ALOUD_PAGES=2,3 cargo
/// test -p pdfcraft-engine --lib read_aloud::harness -- --ignored --nocapture` prints a local
/// PDF's reading blocks and the paragraphs read from them, by the rules in the file
/// `PDFCRAFT_READ_ALOUD_RULES` names if set. Never commit the PDFs it reads.
#[cfg(test)]
mod harness {
    #[test]
    #[ignore = "reads a local PDF named by PDFCRAFT_READ_ALOUD_PDF"]
    fn print_segmentation() {
        let Ok(path) = std::env::var("PDFCRAFT_READ_ALOUD_PDF") else { return };
        let bytes = std::fs::read(&path).expect("readable PDF");
        let mut session = crate::Session::new();
        let id = session.open("harness.pdf", None, std::sync::Arc::new(bytes), None).expect("opens");
        let doc = session.get(id).expect("open document");
        let pages: Vec<usize> = std::env::var("PDFCRAFT_READ_ALOUD_PAGES")
            .map(|s| s.split(',').filter_map(|p| p.trim().parse::<usize>().ok()).map(|p| p.saturating_sub(1)).collect())
            .unwrap_or_else(|_| (0..doc.info.pages.len()).collect());
        let cos = &doc.editor.as_ref().expect("editor").cos;
        // PDFCRAFT_READ_ALOUD_RULES=rules.toml reads by the rules in that file.
        let rules = match std::env::var("PDFCRAFT_READ_ALOUD_RULES").ok().filter(|p| !p.is_empty()) {
            Some(path) => super::Rules::from_toml(&std::fs::read_to_string(path).expect("readable rules")).expect("valid rules"),
            None => super::Rules::default(),
        };
        for page in pages {
            println!("=== page {} blocks", page + 1);
            for (i, b) in pdfcraft_edit::reading_blocks(cos, page).unwrap_or_default().iter().enumerate() {
                let r = b.rect;
                println!(
                    "{i:>3} [{:6.1} {:6.1} {:6.1} {:6.1}] size {:4.1} rows {} rot {:.0} {} {:?}",
                    r[0],
                    r[1],
                    r[2],
                    r[3],
                    b.size,
                    b.rows.len(),
                    b.rotation,
                    b.base_font,
                    b.text.chars().take(70).collect::<String>()
                );
            }
            if std::env::var("PDFCRAFT_READ_ALOUD_ROWS").is_ok() {
                let (_, lines) = pdfcraft_edit::reading_blocks_and_lines(cos, page).unwrap_or_default();
                for r in super::text_rows(&lines) {
                    let runs: Vec<String> = r.runs.iter().map(|(a, b, n)| format!("{a:.0}-{b:.0}:{n}")).collect();
                    println!(
                        "row {:6.1}-{:6.1} size {:4.1} sparse(col 124-495) {} {}",
                        r.bottom,
                        r.top,
                        r.size,
                        super::is_sparse(&r, (124.6, 495.2), &rules),
                        runs.join(" ")
                    );
                }
            }
            println!("=== page {} read", page + 1);
            // The sentences' boxes, for drawing over a rendering of the page: the crop box on the
            // first line, then one sentence a line, its boxes' numbers then a tab and its start.
            if let Ok(out) = std::env::var("PDFCRAFT_READ_ALOUD_BOXES") {
                let crop = doc.info.pages.get(page).map(|p| p.crop).unwrap_or_default();
                let mut text = format!("{} {} {} {}\n", crop[0], crop[1], crop[2], crop[3]);
                for p in doc.read_aloud_passages_with(page, super::Filters::default(), &rules) {
                    for start in super::sentence_starts(&p.text) {
                        let boxes: Vec<String> = p.sentence_boxes(start).iter().flatten().map(|v| format!("{v:.2}")).collect();
                        let head: String = p.text.get(start..).unwrap_or_default().chars().take(40).collect();
                        text.push_str(&format!("{}\t{head}\n", boxes.join(" ")));
                    }
                }
                std::fs::write(format!("{out}.{}.txt", page + 1), text).expect("writable");
            }
            let all = super::Filters { figures: false, captions: false, ..super::Filters::default() };
            let kept: Vec<usize> = doc.read_aloud_passages_with(page, all, &rules).iter().map(|p| p.paragraph).collect();
            for p in doc.read_aloud_passages_with(page, super::Filters { headers: false, ..all }, &rules) {
                let heading = p.heading.map(|d| format!("[H{d}] ")).unwrap_or_default();
                let role = if !kept.contains(&p.paragraph) {
                    "[Running] ".to_string()
                } else if p.role == super::Role::Text {
                    String::new()
                } else {
                    format!("[{:?}] ", p.role)
                };
                println!("#{:<3} {heading}{role}{:?}", p.paragraph, p.text.chars().take(110).collect::<String>());
            }
        }
    }
}
