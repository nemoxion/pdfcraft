//! View ▸ Read Out Loud: the document read aloud by a text-to-speech server, as in Acrobat.
//!
//! The headphones tool in the quick bar (or View ▸ Read Out Loud ▸ Activate) turns it on: a
//! click on a sentence reads from there to the end of the document, and a bar in the margin
//! marks the sentences being read as they play (timed by the server's word timestamps where it gives them).
//! Read This Page Only, Read To End of Document, Pause and Stop are commands.
//!
//! This crate has no network or audio code. The desktop app supplies a [`Speaker`] that asks the
//! speech server and plays its answer ([`PdfCraftApp::speaker`]); without one (the web build,
//! tests) the commands say reading aloud isn't available. Nothing is sent anywhere until the user
//! has entered a server in Preferences ▸ Read Out Loud and starts reading.
//!
//! Two threads do the work while the window stays responsive: one asks the server for each
//! sentence-sized piece ([`pdfcraft_engine::read_aloud::speech_chunks`]) a couple of pieces ahead,
//! the other plays them in order. The window feeds them a few paragraphs at a time, a page at a
//! time, so a long document is never read into memory at once.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pdfcraft_engine::DocId;
use std::ops::Range;

use pdfcraft_engine::read_aloud::{Filters, MAX_RULES_BYTES, Passage, Rules};

use crate::PdfCraftApp;

/// Paragraphs handed to the speech threads ahead of the one being read.
const AHEAD: usize = 4;
/// Pages looked at per frame while looking for the next paragraph, so a run of pages without
/// text (scans) doesn't stall a frame.
const PAGES_PER_FRAME: usize = 3;
/// The longest server address, voice and language kept from settings.
const MAX_SERVER_CHARS: usize = 2048;
const MAX_VOICE_CHARS: usize = 128;
const MAX_LANGUAGE_CHARS: usize = 16;

/// One request to the speech server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeechRequest {
    /// The server's base address, such as `https://tts.example:8443`.
    pub server: String,
    /// The voice to read with; `None` is the server's default.
    pub voice: Option<String>,
    /// The language of the text, such as `en`.
    pub language: String,
    /// What to say (at most [`pdfcraft_engine::read_aloud::MAX_CHUNK_CHARS`] characters).
    pub text: String,
}

/// Pause and stop, shared with the speech threads and the [`Speaker`].
#[derive(Debug, Default)]
pub struct Control {
    stop: AtomicBool,
    paused: AtomicBool,
}

impl Control {
    /// Reading was stopped: playback ends as soon as it can and no more is asked.
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Reading is paused: playback holds where it is.
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

/// What the server answered: the audio, how long it plays and, where the server says, when
/// each word starts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Speech {
    /// In a form [`Speaker::play`] takes (the desktop app: WAV).
    pub audio: Vec<u8>,
    /// Seconds, when known.
    pub duration: Option<f64>,
    /// Each word's start in seconds and the character (not byte) offset in the request's text
    /// where it starts, in order; empty when the server gives no timestamps.
    pub words: Vec<(f64, usize)>,
}

/// Turns text into sound and plays it. Both calls block; they run on the speech threads.
pub trait Speaker: Send + Sync {
    /// The speech for `request`.
    fn synthesize(&self, request: &SpeechRequest) -> Result<Speech, String>;
    /// Play `audio` to the end, holding while `control` is paused and returning early once it
    /// is stopped. `at` is told how far playback has got, in seconds, as it goes.
    fn play(&self, audio: &[u8], control: &Control, at: &mut dyn FnMut(f64)) -> Result<(), String>;
}

/// Preferences ▸ Read Out Loud.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prefs {
    /// The speech server's base address; empty until the user enters one.
    pub server: String,
    /// The voice id; empty for the server's default.
    pub voice: String,
    /// The language of the text read.
    pub language: String,
    /// What is left out of reading (page numbers, citations).
    pub filters: Filters,
    /// Numbers are sent as words ("one hundred ninety-four"); English only.
    pub spell_numbers: bool,
    /// The view scrolls to each paragraph as it is read (scroll lock).
    pub follow: bool,
    /// Numbers are written 1.234,5 rather than 1,234.5.
    pub decimal_comma: bool,
    /// The first request of a reading is only its first sentence, or clause of a long one,
    /// so speech starts sooner.
    pub fast_start: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            server: String::new(),
            voice: String::new(),
            language: "en".into(),
            filters: Filters::default(),
            spell_numbers: true,
            follow: true,
            decimal_comma: false,
            fast_start: true,
        }
    }
}

impl Prefs {
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "server": self.server,
            "voice": self.voice,
            "language": self.language,
            "skip_page_numbers": self.filters.page_numbers,
            "skip_citations": self.filters.citations,
            "skip_bullets": self.filters.bullets,
            "skip_equations": self.filters.equations,
            "skip_numbers": self.filters.numbers,
            "spell_numbers": self.spell_numbers,
            "follow": self.follow,
            "decimal_comma": self.decimal_comma,
            "fast_start": self.fast_start,
            "skip_figures": self.filters.figures,
            "skip_captions": self.filters.captions,
            "skip_headers": self.filters.headers,
        })
    }

    /// Settings are untrusted: a value that isn't a string, or is too long, keeps the default.
    pub(crate) fn from_json(v: &serde_json::Value) -> Self {
        let text = |key: &str, max: usize| v[key].as_str().map(str::trim).filter(|s| s.chars().count() <= max).map(str::to_string);
        let mut prefs = Self::default();
        if let Some(server) = text("server", MAX_SERVER_CHARS) {
            prefs.server = server;
        }
        if let Some(voice) = text("voice", MAX_VOICE_CHARS) {
            prefs.voice = voice;
        }
        if let Some(language) = text("language", MAX_LANGUAGE_CHARS).filter(|l| !l.is_empty()) {
            prefs.language = language;
        }
        if let Some(on) = v["skip_page_numbers"].as_bool() {
            prefs.filters.page_numbers = on;
        }
        if let Some(on) = v["skip_bullets"].as_bool() {
            prefs.filters.bullets = on;
        }
        if let Some(on) = v["skip_equations"].as_bool() {
            prefs.filters.equations = on;
        }
        if let Some(on) = v["skip_numbers"].as_bool() {
            prefs.filters.numbers = on;
        }
        if let Some(on) = v["skip_citations"].as_bool() {
            prefs.filters.citations = on;
        }
        if let Some(on) = v["spell_numbers"].as_bool() {
            prefs.spell_numbers = on;
        }
        if let Some(on) = v["follow"].as_bool() {
            prefs.follow = on;
        }
        if let Some(on) = v["decimal_comma"].as_bool() {
            prefs.decimal_comma = on;
        }
        if let Some(on) = v["fast_start"].as_bool() {
            prefs.fast_start = on;
        }
        if let Some(on) = v["skip_figures"].as_bool() {
            prefs.filters.figures = on;
        }
        if let Some(on) = v["skip_captions"].as_bool() {
            prefs.filters.captions = on;
        }
        if let Some(on) = v["skip_headers"].as_bool() {
            prefs.filters.headers = on;
        }
        prefs
    }

    /// Why reading can't start with these settings, if it can't.
    fn problem(&self) -> Option<&'static str> {
        let server = self.server.trim();
        if server.is_empty() {
            Some("Enter a speech server in Preferences ▸ Read Out Loud first.")
        } else if !(server.starts_with("https://") || server.starts_with("http://")) {
            Some("The speech server's address must start with https:// or http://.")
        } else {
            None
        }
    }

    /// Whether the text is read in English, where number ranges and words apply.
    pub(crate) fn english(&self) -> bool {
        let language = self.language.trim().to_ascii_lowercase();
        language == "en" || language.starts_with("en-") || language.starts_with("en_")
    }

    /// A sentence as it is sent: in English, abbreviations read in full, short acronyms spelled,
    /// ranges and (with the option) numbers as words.
    fn spoken(&self, text: &str) -> String {
        use pdfcraft_engine::read_aloud::{NumberFormat, expand_english_abbreviations, speakable_english};
        if !self.english() {
            return text.to_string();
        }
        let format = if self.decimal_comma { NumberFormat::DecimalComma } else { NumberFormat::DecimalPoint };
        speakable_english(&expand_english_abbreviations(text), self.spell_numbers, format)
    }

    fn request(&self, text: String) -> SpeechRequest {
        let voice = self.voice.trim();
        SpeechRequest {
            server: self.server.trim().trim_end_matches('/').to_string(),
            voice: (!voice.is_empty()).then(|| voice.to_string()),
            language: self.language.trim().to_string(),
            text,
        }
    }
}

/// How far to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The page only.
    Page,
    /// From the start to the end of the document.
    ToEnd,
}

/// One paragraph for the speech threads: its number, and the pieces it is spoken in, each the
/// sentences it joins as their byte range in the paragraph's text and the words to say.
#[derive(Clone, Debug, PartialEq)]
struct Job {
    seq: u64,
    pieces: Vec<Vec<(Range<usize>, String)>>,
}

/// One piece's audio, with when (seconds in) each of its sentences starts.
struct Clip {
    seq: u64,
    audio: Vec<u8>,
    marks: Vec<(f64, Range<usize>)>,
}

/// What the speech threads report.
#[derive(Debug, PartialEq)]
enum Event {
    /// The paragraph with this number started playing.
    Speaking(u64),
    /// The sentence at these bytes of that paragraph's text started playing.
    At(u64, Range<usize>),
    /// Asking the server or playing failed; reading stops.
    Failed(String),
    /// Everything handed over has been read.
    Finished,
}

/// One reading of a document.
struct Reading {
    doc: DocId,
    /// The next page to look for paragraphs on, and the last page to read (inclusive).
    next_page: usize,
    last_page: usize,
    /// What is left out of reading.
    filters: Filters,
    /// The rules the reading started with.
    rules: Arc<Rules>,
    /// Read only from this paragraph on, on the first page, and in it from this byte offset
    /// (the sentence clicked).
    from: Option<(usize, usize, usize)>,
    /// Start the next paragraph handed over at this byte offset of its text (the sentence
    /// clicked): the paragraph keeps its whole text so its rows still line up.
    skip: Option<(usize, usize)>,
    /// Paragraphs found on the page being fed, not yet handed over.
    found: VecDeque<Passage>,
    /// Paragraphs handed over and not yet started, by number.
    handed: VecDeque<(u64, Passage)>,
    next_seq: u64,
    /// The paragraph being read, its number, and the bytes of its text being read now.
    current: Option<Passage>,
    current_seq: Option<u64>,
    current_range: Option<Range<usize>>,
    /// The paragraph the view last scrolled to; cleared to scroll to the current one again.
    followed: Option<u64>,
    /// Hands paragraphs to the speech threads; dropped once every page has been fed.
    jobs: Option<std::sync::mpsc::Sender<Job>>,
    events: std::sync::mpsc::Receiver<Event>,
    control: Arc<Control>,
}

impl Drop for Reading {
    fn drop(&mut self) {
        // Ends playback and the speech threads, whatever ends the reading.
        self.control.stop.store(true, Ordering::Relaxed);
    }
}

/// Read Out Loud's state in the window.
#[derive(Default)]
pub struct ReadAloud {
    pub prefs: Prefs,
    reading: Option<Reading>,
    pub(crate) rules: RulesFile,
}

/// The name of Read Out Loud's rules file in the settings folder.
pub const RULES_FILE: &str = "read-aloud-rules.toml";
/// How often the rules file is looked at for a change, in seconds.
const RULES_CHECK_SECONDS: f64 = 1.0;

/// Read Out Loud's rules file ([`Rules`]): where it is, the rules it holds (the defaults when
/// there is none or it has a mistake, which `error` names), and a number that changes whenever
/// they do. The file is read again at most once a second, and its rules again when it changed.
#[derive(Default)]
pub(crate) struct RulesFile {
    path: Option<std::path::PathBuf>,
    rules: Arc<Rules>,
    /// A hash of the file's bytes when last read (or why it couldn't be read); `None` when it
    /// was missing.
    stamp: Option<u64>,
    checked: Option<f64>,
    error: Option<String>,
    generation: u64,
}

impl RulesFile {
    pub(crate) fn rules(&self) -> Arc<Rules> {
        self.rules.clone()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Read the rules again if the file changed and it wasn't looked at in the last second;
    /// `now` is the UI's clock in seconds.
    pub(crate) fn refresh(&mut self, now: f64) {
        if self.checked.is_some_and(|t| (now - t).abs() < RULES_CHECK_SECONDS) {
            return;
        }
        self.checked = Some(now);
        self.reload();
    }

    fn set_path(&mut self, path: std::path::PathBuf) {
        *self = Self { path: Some(path), generation: self.generation.wrapping_add(1), ..Self::default() };
        self.reload();
    }

    fn reload(&mut self) {
        use std::hash::{Hash, Hasher};
        let Some(path) = &self.path else { return };
        let bytes = read_capped(path);
        let stamp = bytes.as_ref().map(|b| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            b.hash(&mut h);
            h.finish()
        });
        if stamp == self.stamp {
            return;
        }
        let read = match bytes {
            None => Ok(Rules::default()),
            Some(Err(e)) => Err(e),
            Some(Ok(bytes)) if bytes.len() > MAX_RULES_BYTES => Err(format!("the rules file is larger than {} KiB", MAX_RULES_BYTES / 1024)),
            Some(Ok(bytes)) => String::from_utf8(bytes).map_err(|_| "the rules file isn't UTF-8 text".to_string()).and_then(|t| Rules::from_toml(&t)),
        };
        self.stamp = stamp;
        (self.rules, self.error) = match read {
            Ok(rules) => (Arc::new(rules), None),
            Err(e) => (Arc::new(Rules::default()), Some(e)),
        };
        self.generation = self.generation.wrapping_add(1);
    }

    /// Write the starting rules file ([`Rules::template`]) where there is none yet.
    fn create(&mut self) -> Result<(), String> {
        use std::io::Write;
        let Some(path) = &self.path else { return Err("There is no settings folder".into()) };
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| e.to_string())?;
        file.write_all(Rules::template().as_bytes()).map_err(|e| e.to_string())?;
        self.reload();
        Ok(())
    }
}

/// The file at `path`, no more than one byte past [`MAX_RULES_BYTES`] of it (enough for
/// [`Rules::from_toml`] to say it is too large); `None` when there is no file.
fn read_capped(path: &std::path::Path) -> Option<Result<Vec<u8>, String>> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Err(e.to_string())),
    };
    let mut bytes = Vec::new();
    Some(file.take(MAX_RULES_BYTES as u64 + 1).read_to_end(&mut bytes).map(|_| bytes).map_err(|e| e.to_string()))
}

impl ReadAloud {
    /// Read the rules from `path` (a file that may not exist yet), and from then on whenever it
    /// changes.
    pub fn set_rules_path(&mut self, path: std::path::PathBuf) {
        self.rules.set_path(path);
    }

    /// A document is being read (paused or not).
    pub fn active(&self) -> bool {
        self.reading.is_some()
    }

    /// Reading has started but nothing has played yet: the server is still answering (a server
    /// loading its model takes a while for the first sentence).
    pub fn waiting(&self) -> bool {
        self.reading.as_ref().is_some_and(|r| r.current.is_none())
    }

    /// Reading is paused.
    pub fn paused(&self) -> bool {
        self.reading.as_ref().is_some_and(|r| r.control.paused())
    }

    /// What is being read in `doc`: its page, boxes in user space, one per row of the
    /// sentences playing (grouped to at least
    /// [`pdfcraft_engine::read_aloud::MIN_HIGHLIGHT_WORDS`] words), or the paragraph's box
    /// where rows aren't known, and the paragraph's left edge.
    pub fn highlight(&self, doc: DocId) -> Option<(usize, Vec<[f64; 4]>, f64)> {
        use pdfcraft_engine::read_aloud::{MIN_HIGHLIGHT_WORDS, highlight_range};
        let r = self.reading.as_ref().filter(|r| r.doc == doc)?;
        let p = r.current.as_ref()?;
        let boxes =
            r.current_range.as_ref().map(|range| p.range_boxes(highlight_range(&p.text, range.start, MIN_HIGHLIGHT_WORDS))).unwrap_or_default();
        Some((p.page, if boxes.is_empty() { vec![p.rect] } else { boxes }, p.rect[0]))
    }

    /// Pause or resume. Returns whether anything was being read.
    pub fn toggle_pause(&mut self) -> bool {
        let Some(r) = &self.reading else { return false };
        let paused = r.control.paused();
        r.control.paused.store(!paused, Ordering::Relaxed);
        true
    }

    /// Stop reading. Returns whether anything was being read.
    pub fn stop(&mut self) -> bool {
        self.reading.take().is_some()
    }

    /// Turn following the reading (scroll lock) on or off; turned on, the view goes to the
    /// paragraph being read.
    pub fn toggle_follow(&mut self) {
        self.prefs.follow = !self.prefs.follow;
        if let Some(r) = &mut self.reading {
            r.followed = None;
        }
    }
}

impl PdfCraftApp {
    /// Read the active document aloud from `from` on `page` (a paragraph and the byte offset of
    /// a sentence in it) or from the top of the page, to the end of the page or of the document.
    /// Anything being read stops first.
    pub(crate) fn read_aloud_start(&mut self, page: usize, from: Option<(usize, usize)>, scope: Scope) {
        self.read_aloud.stop();
        let Some((_, id)) = self.active_ids() else { return };
        let Some(pages) = self.session.get(id).map(|d| d.info.pages.len()) else { return };
        if page >= pages {
            return;
        }
        let Some(speaker) = self.speaker.clone() else {
            self.notify_tr("Read Out Loud needs the desktop app.");
            return;
        };
        if let Some(problem) = self.read_aloud.prefs.problem() {
            self.notify_tr(problem);
            return;
        }
        let control = Arc::new(Control::default());
        let Some((jobs, events)) = spawn(speaker, self.read_aloud.prefs.clone(), control.clone(), self.ctx.clone()) else {
            self.notify_tr("Read Out Loud isn't available here.");
            return;
        };
        self.read_aloud.reading = Some(Reading {
            doc: id,
            next_page: page,
            last_page: if scope == Scope::Page { page } else { pages - 1 },
            from: from.map(|(paragraph, offset)| (page, paragraph, offset)),
            skip: None,
            filters: self.read_aloud.prefs.filters,
            rules: self.read_aloud.rules.rules(),
            found: VecDeque::new(),
            handed: VecDeque::new(),
            next_seq: 0,
            current: None,
            current_seq: None,
            current_range: None,
            followed: None,
            jobs: Some(jobs),
            events,
            control,
        });
        self.read_aloud_poll();
    }

    /// Each frame: hand the speech threads more paragraphs, follow what they read, and end the
    /// reading when it is done, failed, or its document closed.
    pub(crate) fn read_aloud_poll(&mut self) {
        if let Some(now) = self.ctx.as_ref().map(|c| c.input(|i| i.time)) {
            self.read_aloud.rules.refresh(now);
        }
        let english = self.read_aloud.prefs.english();
        let fast_start = self.read_aloud.prefs.fast_start;
        let Some(r) = self.read_aloud.reading.as_mut() else { return };
        let Some(doc) = self.session.get(r.doc) else {
            self.read_aloud.stop();
            return;
        };
        // Feed: a few paragraphs ahead, a few pages per frame.
        let mut pages_looked = 0;
        while r.handed.len() < AHEAD && r.jobs.is_some() {
            if let Some(p) = r.found.pop_front() {
                let skip = match r.skip.take() {
                    Some((paragraph, offset)) if paragraph == p.paragraph => offset,
                    _ => 0,
                };
                let mut pieces = match p.heading {
                    Some(_) => vec![vec![(0..p.text.len(), pdfcraft_engine::read_aloud::heading_sentence(&p.text, english))]],
                    None => pieces_of(&p.text, skip),
                };
                // A sentence running on over the page break is read whole, with this paragraph.
                if r.found.is_empty() && p.heading.is_none() && r.next_page <= r.last_page && !ends_sentence(&p.text) {
                    let page = r.next_page;
                    r.found = doc.read_aloud_passages_with(page, r.filters, &r.rules).into();
                    r.next_page = page.saturating_add(1);
                    pages_looked += 1;
                    if let Some(rest) = take_continuation(&mut r.found)
                        && let Some((_, last)) = pieces.last_mut().and_then(|piece| piece.last_mut())
                    {
                        join_across_pages(last, &rest);
                    }
                }
                let seq = r.next_seq;
                r.next_seq = seq.saturating_add(1);
                if fast_start && seq == 0 {
                    fast_first(&mut pieces);
                }
                if r.jobs.as_ref().is_some_and(|tx| tx.send(Job { seq, pieces }).is_err()) {
                    r.jobs = None;
                    break;
                }
                r.handed.push_back((seq, p));
                continue;
            }
            if r.next_page > r.last_page {
                // Every page fed: the threads finish what they have, then say so.
                r.jobs = None;
                break;
            }
            if pages_looked == PAGES_PER_FRAME {
                if let Some(ctx) = &self.ctx {
                    ctx.request_repaint();
                }
                break;
            }
            let page = r.next_page;
            let mut found = doc.read_aloud_passages_with(page, r.filters, &r.rules);
            if let Some((from_page, from, offset)) = r.from
                && from_page == page
            {
                found.retain(|p| p.paragraph >= from);
                r.skip = Some((from, offset));
            }
            r.found = found.into();
            r.next_page = page.saturating_add(1);
            pages_looked += 1;
        }
        // Follow.
        let mut end: Option<Option<String>> = None;
        while let Ok(event) = r.events.try_recv() {
            match event {
                Event::Speaking(seq) => {
                    while let Some((s, p)) = r.handed.pop_front() {
                        if s == seq {
                            r.current = Some(p);
                            r.current_seq = Some(seq);
                            r.current_range = None;
                            break;
                        }
                    }
                }
                Event::At(seq, range) => {
                    if r.current_seq == Some(seq) {
                        r.current_range = Some(range);
                    }
                }
                Event::Failed(e) => end = Some(Some(e)),
                Event::Finished => end = Some(None),
            }
        }
        // Nothing found to read at all: say so rather than end silently.
        let nothing = r.current.is_none() && r.handed.is_empty() && r.found.is_empty() && r.jobs.is_none() && r.next_seq == 0;
        let id = r.doc;
        // Scroll lock: go to each paragraph as it starts, once, so the reader may still scroll
        // away while it is read.
        let follow = (self.read_aloud.prefs.follow && r.followed != r.current_seq).then(|| r.current.as_ref().map(|p| (p.page, p.rect))).flatten();
        if follow.is_some() {
            r.followed = r.current_seq;
        }
        if let Some((page, rect)) = follow {
            self.read_aloud_follow(id, page, rect);
        }

        if let Some(end) = end {
            self.read_aloud.stop();
            if let Some(e) = end {
                self.notify_fmt("Read Out Loud stopped: {e}", &[("e", &e)]);
            }
        } else if nothing {
            self.read_aloud.stop();
            self.notify_tr("There is no text to read here.");
        }
    }

    /// Bring the paragraph being read into view unless it shows already (or, taller than the
    /// window, its top does).
    fn read_aloud_follow(&mut self, doc: DocId, page: usize, rect: [f64; 4]) {
        let Some(view) = self.views.iter_mut().find(|v| v.id == doc) else { return };
        let Some(document) = self.session.get(doc) else { return };
        let viewport = view.viewport_rect();
        if let Some(xf) = view.page_xform(page) {
            let r = xf.user_rect(&document.info, page, rect.map(|v| v as f32));
            let top_shows = r.top() >= viewport.top() && r.top() < viewport.bottom();
            if viewport.contains_rect(r) || (r.height() > viewport.height() && top_shows) {
                return;
            }
        }
        let Some(info) = document.info.pages.get(page) else { return };
        // The paragraph's top, a little below the window's top edge.
        let [_, y] = info.user_to_view(rect[0] as f32, rect[3] as f32);
        let height = info.height.max(1.0);
        let fraction = (y / height - 0.05).clamp(0.0, 1.0);
        view.goto = Some((page, if fraction.is_finite() { fraction } else { 0.0 }));
    }
}

/// The pieces `text` is spoken in from byte `skip` on ([`pdfcraft_engine::read_aloud::speech_pieces`]),
/// each sentence with its byte range in `text` and its words.
fn pieces_of(text: &str, skip: usize) -> Vec<Vec<(Range<usize>, String)>> {
    let skip = if text.is_char_boundary(skip) { skip } else { 0 };
    let rest = text.get(skip..).unwrap_or_default();
    pdfcraft_engine::read_aloud::speech_pieces(rest)
        .into_iter()
        .map(|piece| piece.into_iter().filter_map(|r| Some((r.start + skip..r.end + skip, rest.get(r)?.to_string()))).collect::<Vec<_>>())
        .filter(|piece| !piece.is_empty())
        .collect()
}

/// A first piece this short (in characters) asks the server for the least before speech starts.
const FAST_CHARS: usize = 100;

/// `pieces` with the first one only its first sentence, and a first sentence longer than
/// [`FAST_CHARS`] cut after its last comma, semicolon or colon that comes before that.
fn fast_first(pieces: &mut Vec<Vec<(Range<usize>, String)>>) {
    let Some(first) = pieces.first_mut() else { return };
    if first.len() > 1 {
        let rest = first.split_off(1);
        pieces.insert(1, rest);
    }
    let Some((range, text)) = pieces.first().and_then(|p| p.first()).cloned() else { return };
    if text.chars().count() <= FAST_CHARS {
        return;
    }
    let limit = text.char_indices().nth(FAST_CHARS).map_or(text.len(), |(i, _)| i);
    let cut = text.get(..limit).and_then(|head| head.rfind([',', ';', ':'])).filter(|&at| at >= 20);
    let Some(at) = cut else { return };
    let head_end = at + 1;
    let (Some(head), Some(tail)) = (text.get(..head_end), text.get(head_end..)) else { return };
    let tail_trimmed = tail.trim_start();
    let tail_start = head_end + (tail.len() - tail_trimmed.len());
    let head_range = range.start..range.start.saturating_add(head_end).min(range.end);
    let tail_range = range.start.saturating_add(tail_start).min(range.end)..range.end;
    if tail_trimmed.is_empty() {
        return;
    }
    if let Some(p) = pieces.first_mut() {
        *p = vec![(head_range, head.to_string())];
    }
    pieces.insert(1, vec![(tail_range, tail_trimmed.to_string())]);
}

/// When each sentence of a piece starts, in seconds: from the word timestamps where the server
/// gives them, else in proportion to where the sentence starts in the text sent (`starts`, in
/// characters, of `total`).
fn marks(speech: &Speech, starts: &[(usize, Range<usize>)], total: usize) -> Vec<(f64, Range<usize>)> {
    let mut out: Vec<(f64, Range<usize>)> = Vec::with_capacity(starts.len());
    for (i, (at, range)) in starts.iter().enumerate() {
        let timed = speech.words.iter().find(|(_, w)| w >= at).map(|(t, _)| *t).filter(|t| t.is_finite());
        let estimated = speech.duration.filter(|d| d.is_finite() && total > 0).map(|d| d * *at as f64 / total as f64);
        let time = if i == 0 { 0.0 } else { timed.or(estimated).unwrap_or(0.0) };
        // Never earlier than the sentence before.
        let time = out.last().map_or(time, |(prev, _)| time.max(*prev));
        out.push((time, range.clone()));
    }
    out
}

/// The paragraphs handed to the speech threads, and what they report.
type Channels = (std::sync::mpsc::Sender<Job>, std::sync::mpsc::Receiver<Event>);

/// Start the speech threads. `None` where threads can't be started (the web build).
#[cfg(not(target_arch = "wasm32"))]
fn spawn(speaker: Arc<dyn Speaker>, prefs: Prefs, control: Arc<Control>, ctx: Option<egui::Context>) -> Option<Channels> {
    use std::sync::mpsc;
    let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
    let (events_tx, events_rx) = mpsc::channel::<Event>();
    // Two pieces of audio wait while one plays: the next sentence is ready when this one ends.
    let (audio_tx, audio_rx) = mpsc::sync_channel::<Clip>(2);
    let report = {
        let events_tx = events_tx.clone();
        move |e: Event| {
            let _ = events_tx.send(e);
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        }
    };
    let synth = {
        let (speaker, control, report) = (speaker.clone(), control.clone(), report.clone());
        move || {
            for Job { seq, pieces } in jobs_rx {
                for piece in pieces {
                    if control.stopped() {
                        return;
                    }
                    // Each sentence is put into words on its own, so where it starts in the text
                    // sent is known (in characters, as the server counts).
                    let mut text = String::new();
                    let mut starts = Vec::with_capacity(piece.len());
                    for (range, sentence) in &piece {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        starts.push((text.chars().count(), range.clone()));
                        text.push_str(&prefs.spoken(sentence));
                    }
                    let total = text.chars().count();
                    match speaker.synthesize(&prefs.request(text)) {
                        Ok(speech) => {
                            let marks = marks(&speech, &starts, total);
                            if audio_tx.send(Clip { seq, audio: speech.audio, marks }).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            report(Event::Failed(e));
                            return;
                        }
                    }
                }
            }
        }
    };
    let play = move || {
        let mut last = None;
        for Clip { seq, audio, marks } in audio_rx {
            if control.stopped() {
                return;
            }
            if last != Some(seq) {
                last = Some(seq);
                report(Event::Speaking(seq));
            }
            let mut marks = marks.into_iter().peekable();
            let mut at = |seconds: f64| {
                while let Some((_, range)) = marks.next_if(|(t, _)| *t <= seconds) {
                    report(Event::At(seq, range));
                }
            };
            at(0.0);
            if let Err(e) = speaker.play(&audio, &control, &mut at) {
                report(Event::Failed(e));
                return;
            }
        }
        // The synthesizing thread is done: everything handed over was read (or it failed and
        // said so first, which ends the reading before this arrives).
        if !control.stopped() {
            report(Event::Finished);
        }
    };
    std::thread::Builder::new().name("read-aloud-speech".into()).spawn(synth).ok()?;
    std::thread::Builder::new().name("read-aloud-play".into()).spawn(play).ok()?;
    Some((jobs_tx, events_rx))
}

#[cfg(target_arch = "wasm32")]
fn spawn(_speaker: Arc<dyn Speaker>, _prefs: Prefs, _control: Arc<Control>, _ctx: Option<egui::Context>) -> Option<Channels> {
    None
}

/// Whether `text` ends a sentence (closing quotes and brackets aside), so the next page starts
/// a new one.
fn ends_sentence(text: &str) -> bool {
    let end = text.trim_end().trim_end_matches(['"', '\'', ')', ']', '”', '’', '」', '）']);
    end.ends_with(['.', '!', '?', '…', ':', '。', '！', '？']) || end.is_empty()
}

/// The words that finish a sentence broken by a page break: the first sentence of the next
/// page's first paragraph that starts in lowercase (a running header may come before it), taken
/// out of that paragraph.
fn take_continuation(found: &mut VecDeque<Passage>) -> Option<String> {
    let at = found.iter().take(2).position(|p| p.text.starts_with(char::is_lowercase))?;
    // Only a short line without a full stop (a running header) may come before it.
    if at == 1 && found.front().is_some_and(|h| h.text.chars().count() > 100 || ends_sentence(&h.text)) {
        return None;
    }
    let p = found.get_mut(at)?;
    let end = pdfcraft_engine::read_aloud::sentence_starts(&p.text).get(1).copied().unwrap_or(p.text.len());
    let head = p.text.get(..end).unwrap_or_default().trim().to_string();
    p.text = p.text.get(end..).unwrap_or_default().trim_start().to_string();
    if p.text.is_empty() {
        found.remove(at);
    }
    Some(head)
}

/// `rest` appended to `text`, a word hyphenated over the break ("exam-" + "ple") put back together.
fn join_across_pages(text: &mut String, rest: &str) {
    let mut before = text.chars().rev();
    if before.next() == Some('-') && before.next().is_some_and(char::is_alphabetic) {
        text.pop();
    } else {
        text.push(' ');
    }
    text.push_str(rest);
}

/// The index of the paragraph in `passages` (all on one page) under `pointer`, if any.
pub(crate) fn hit(passages: &[Passage], xf: &crate::canvas::PageXform, info: &pdfcraft_render::DocInfo, pointer: egui::Pos2) -> Option<usize> {
    passages.iter().position(|p| xf.user_rect(info, p.page, p.rect.map(|v| v as f32)).expand(2.0).contains(pointer))
}

/// The paragraph being read, outlined.
const READING_STROKE: egui::Color32 = egui::Color32::from_rgb(0x3A, 0x7B, 0xF0);

/// Page `page`'s part of Read Out Loud, by `rules` (with their [`RulesFile::generation`]):
/// mark `reading` (the boxes of what is being read on this page) with a bar in the margin, left
/// of its rows and of `reading_left` (the paragraph's left edge, so the bar keeps one place while
/// a paragraph is read), and while the headphones tool is `armed`, outline the paragraph under
/// the pointer and set `start` to it when it is clicked. Returns whether the pointer is over one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn page_input(
    ui: &egui::Ui,
    resp: &egui::Response,
    xf: &crate::canvas::PageXform,
    page: usize,
    info: &pdfcraft_render::DocInfo,
    doc: &pdfcraft_engine::Document,
    view: &mut crate::canvas::DocView,
    armed: bool,
    filters: Filters,
    rules: (&Rules, u64),
    reading: &[[f64; 4]],
    reading_left: Option<f64>,
    start: &mut Option<(usize, usize, usize)>,
) -> bool {
    let painter = ui.painter();
    // What is being read: a bar in the margin left of its rows, from the first row's top to
    // the last row's bottom.
    let rows: Vec<egui::Rect> = reading.iter().map(|rect| xf.user_rect(info, page, rect.map(|v| v as f32))).collect();
    if let Some(span) = rows.iter().copied().reduce(|a, b| a.union(b)) {
        let first = reading.first().copied().unwrap_or_default();
        let edge = |l: f64| xf.user_rect(info, page, [l as f32, first[1] as f32, l as f32, first[3] as f32]).left();
        let left = reading_left.map_or(span.left(), |l| edge(l).min(span.left()));
        let x = left - 5.0;
        painter.line_segment([egui::pos2(x, span.top()), egui::pos2(x, span.bottom())], egui::Stroke::new(3.0, READING_STROKE));
    }
    if !armed {
        return false;
    }
    let Some(pointer) = ui.input(|i| i.pointer.hover_pos()).filter(|p| xf.rect.contains(*p) && resp.hovered()) else { return false };
    let key = (doc.edit_generation(), filters, rules.1);
    let passages = match view.read_aloud_passages.get(&page) {
        Some((k, _)) if *k == key => view.read_aloud_passages.get(&page).map(|(_, p)| p.as_slice()).unwrap_or_default(),
        _ => {
            view.read_aloud_passages.insert(page, (key, doc.read_aloud_passages_with(page, filters, rules.0)));
            view.read_aloud_passages.get(&page).map(|(_, p)| p.as_slice()).unwrap_or_default()
        }
    };
    let Some(passage) = hit(passages, xf, info, pointer).and_then(|k| passages.get(k)) else { return false };
    let r = xf.user_rect(info, page, passage.rect.map(|v| v as f32)).expand(3.0);
    painter.rect_stroke(r, egui::CornerRadius::same(3), egui::Stroke::new(1.0, READING_STROKE), egui::StrokeKind::Outside);
    // The sentence a click starts at: underlined.
    let (x, y) = xf.screen_to_view(pointer);
    let at = info.pages.get(page).map_or(0, |p| {
        let [ux, uy] = p.view_to_user(x, y);
        passage.sentence_at([f64::from(ux), f64::from(uy)])
    });
    let stroke = egui::Stroke::new(1.5, READING_STROKE);
    for b in passage.sentence_boxes(at) {
        let s = xf.user_rect(info, page, b.map(|v| v as f32));
        painter.line_segment([s.left_bottom(), s.right_bottom()], stroke);
    }
    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    if resp.clicked() {
        *start = Some((page, passage.paragraph, at));
    }
    true
}

/// The quick bar's headphones (Activate Read Out Loud), and Pause/Resume and Stop while reading.
pub(crate) fn quick_buttons(app: &mut PdfCraftApp, ui: &mut egui::Ui) {
    use crate::icons;
    let on = app.quick_tool == crate::QuickTool::ReadAloud;
    let tip = crate::commands::command_tip(ui.ctx(), tl!("Read Out Loud ({key})"), "view.read_aloud.activate");
    if icons::button(ui, "headphones", 32.0, on, &tip).clicked() {
        app.execute("view.read_aloud.activate");
    }
    if app.read_aloud.active() {
        let (icon, tip) = if app.read_aloud.paused() { ("play", tl!("Resume reading")) } else { ("pause", tl!("Pause reading")) };
        if icons::button(ui, icon, 32.0, false, tip).clicked() {
            app.execute("view.read_aloud.pause");
        }
        let tip = crate::commands::command_tip(ui.ctx(), tl!("Stop reading ({key})"), "view.read_aloud.stop");
        if icons::button(ui, "square", 32.0, false, &tip).clicked() {
            app.execute("view.read_aloud.stop");
        }
        let follow = app.read_aloud.prefs.follow;
        let tip = if follow { tl!("Following the reading: click to scroll freely") } else { tl!("Follow the reading (scroll lock)") };
        if icons::button(ui, if follow { "lock" } else { "lock-open" }, 32.0, follow, tip).clicked() {
            app.execute("view.read_aloud.follow");
        }
    }
}

/// Preferences ▸ Read Out Loud.
pub(crate) fn preferences_section(ui: &mut egui::Ui, app: &mut PdfCraftApp, t: &crate::theme::Tokens) {
    use crate::{theme, widgets};
    ui.label(egui::RichText::new(tl!("Read Out Loud")).font(theme::semibold(13.0)));
    let prefs = &mut app.read_aloud.prefs;
    egui::Grid::new("read-aloud-prefs").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
        let label = ui.label(tl!("Speech server"));
        ui.add(widgets::line(&mut prefs.server).hint_text("https://…").desired_width(300.0).char_limit(MAX_SERVER_CHARS)).labelled_by(label.id);
        ui.end_row();
        let label = ui.label(tl!("Voice"));
        ui.add(widgets::line(&mut prefs.voice).hint_text(tl!("Server default")).desired_width(300.0).char_limit(MAX_VOICE_CHARS))
            .labelled_by(label.id);
        ui.end_row();
        let label = ui.label(tl!("Language"));
        ui.add(widgets::line(&mut prefs.language).desired_width(60.0).char_limit(MAX_LANGUAGE_CHARS)).labelled_by(label.id);
        ui.end_row();
    });
    ui.add(widgets::checkbox(&mut prefs.filters.page_numbers, tl!("Skip page numbers")));
    ui.add(widgets::checkbox(&mut prefs.filters.headers, tl!("Skip running headers and footers")));
    ui.add(widgets::checkbox(&mut prefs.filters.bullets, tl!("Skip bullets and footnote symbols")));
    ui.add(widgets::checkbox(&mut prefs.filters.equations, tl!("Skip equations (math inside a sentence is still read)")));
    ui.add(widgets::checkbox(&mut prefs.filters.numbers, tl!("Skip number-only table cells")));
    ui.add(widgets::checkbox(&mut prefs.filters.citations, tl!("Skip citations such as [13] or (Smith, 2020)")));
    ui.add(widgets::checkbox(&mut prefs.filters.figures, tl!("Skip text inside figures and tables (labels, table rows)")));
    ui.add(widgets::checkbox(&mut prefs.filters.captions, tl!("Skip figure and table captions")));
    ui.add(widgets::checkbox(&mut prefs.fast_start, tl!("Fast start: send the first sentence on its own so speech starts sooner")));
    ui.add(widgets::checkbox(&mut prefs.decimal_comma, tl!("Numbers use a decimal comma, such as 1.234,5 (English only)")));
    ui.add(widgets::checkbox(&mut prefs.spell_numbers, tl!("Say numbers as words, such as 194 as one hundred ninety-four (English only)")));
    ui.label(
        egui::RichText::new(tl!(
            "The text read is sent to this server, which answers with speech (an OpenAI-style /v1/audio/speech endpoint). Nothing is sent until you start reading."
        ))
        .small()
        .color(t.text_muted),
    );
    rules_section(ui, app, t);
}

/// The rules file's place in Preferences: where it is, whether it is in use or has a mistake,
/// and buttons to create it or show it in the file manager.
fn rules_section(ui: &mut egui::Ui, app: &mut PdfCraftApp, t: &crate::theme::Tokens) {
    let now = ui.input(|i| i.time);
    let rules = &mut app.read_aloud.rules;
    rules.refresh(now);
    let Some(path) = rules.path.clone() else { return };
    ui.add_space(6.0);
    ui.label(tl!("Text rules (thresholds for paragraphs, tables, figures and running headers):"));
    ui.label(egui::RichText::new(path.display().to_string()).monospace().small().color(t.text_muted));
    let exists = rules.stamp.is_some();
    match (&rules.error, exists) {
        (Some(e), _) => {
            ui.label(egui::RichText::new(format!("{} {e}", tl!("Not used, the built-in rules apply:"))).color(ui.visuals().error_fg_color));
        }
        (None, true) => {
            ui.label(egui::RichText::new(tl!("In use. Changes apply within a second.")).small().color(t.text_muted));
        }
        (None, false) => {
            ui.label(egui::RichText::new(tl!("No rules file: the built-in rules apply.")).small().color(t.text_muted));
        }
    }
    let mut failed = None;
    ui.horizontal(|ui| {
        if !exists
            && ui.button(tl!("Create Rules File")).clicked()
            && let Err(e) = rules.create()
        {
            failed = Some(e);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if exists
            && ui.button(tl!("Show in Folder")).clicked()
            && let Err(e) = crate::tab_context::reveal(&path)
        {
            failed = Some(e);
        }
    });
    if let Some(e) = failed {
        app.notify_error(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The rules file is read when set, again once it changes (at most once a second), falls
    /// back to the defaults with its mistake named, and can be created from the template.
    #[test]
    fn the_rules_file_is_followed() {
        let dir = std::env::temp_dir().join(format!("pdfcraft-read-aloud-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(RULES_FILE);
        let mut file = RulesFile::default();
        file.set_path(path.clone());
        assert_eq!((file.rules().as_ref(), file.error.as_deref(), file.stamp.is_some()), (&Rules::default(), None, false));
        file.create().unwrap();
        assert!(file.stamp.is_some() && file.error.is_none());
        assert!(file.create().is_err(), "an existing file is kept");
        let generation = file.generation();
        std::fs::write(&path, "[figures]\ncell_gap = 2.5\n").unwrap();
        file.refresh(0.0);
        file.refresh(0.5);
        assert_eq!(file.rules().cell_gap, 2.5);
        assert_ne!(file.generation(), generation);
        // A change within the second waits for the next look.
        std::fs::write(&path, "[figures]\ncell_gap = 9e9\n").unwrap();
        file.refresh(0.9);
        assert_eq!(file.rules().cell_gap, 2.5);
        file.refresh(2.0);
        assert_eq!(*file.rules(), Rules::default());
        assert!(file.error.as_deref().is_some_and(|e| e.contains("figures.cell_gap")));
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        file.refresh(4.0);
        assert!(file.error.as_deref().is_some_and(|e| e.contains("UTF-8")));
        std::fs::write(&path, "é".repeat(MAX_RULES_BYTES)).unwrap();
        file.refresh(5.0);
        assert!(file.error.as_deref().is_some_and(|e| e.contains("larger than")));
        std::fs::remove_file(&path).unwrap();
        file.refresh(6.0);
        assert_eq!((file.rules().as_ref(), file.error.as_deref(), file.stamp.is_some()), (&Rules::default(), None, false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefs_round_trip_and_reject_bad_settings() {
        let filters = Filters {
            page_numbers: false,
            bullets: true,
            equations: false,
            numbers: true,
            citations: true,
            figures: false,
            captions: true,
            headers: false,
        };
        let prefs = Prefs {
            server: "https://tts.example:8443".into(),
            voice: "abc".into(),
            language: "de".into(),
            filters,
            spell_numbers: false,
            follow: false,
            decimal_comma: true,
            fast_start: false,
        };
        assert_eq!(Prefs::from_json(&prefs.to_json()), prefs);
        let bad = serde_json::json!({ "server": 7, "voice": "v".repeat(MAX_VOICE_CHARS + 1), "language": "" });
        assert_eq!(Prefs::from_json(&bad), Prefs::default());
        assert_eq!(Prefs::from_json(&serde_json::Value::Null), Prefs::default());
    }

    #[test]
    fn requests_trim_the_address_and_leave_an_empty_voice_to_the_server() {
        let prefs = Prefs { server: " https://tts.example/ ".into(), voice: "  ".into(), ..Prefs::default() };
        assert_eq!(prefs.problem(), None);
        let r = prefs.request("Hi.".into());
        assert_eq!((r.server.as_str(), r.voice, r.text.as_str()), ("https://tts.example", None, "Hi."));
        assert!(Prefs::default().problem().is_some());
        assert!(Prefs { server: "ftp://x".into(), ..Prefs::default() }.problem().is_some());
    }

    #[test]
    fn a_sentence_broken_by_a_page_break_is_read_whole() {
        let passage = |paragraph: usize, text: &str| Passage {
            page: 1,
            paragraph,
            rect: [0.0; 4],
            text: text.into(),
            lines: Vec::new(),
            ink: Vec::new(),
            rows_text: Vec::new(),
            heading: None,
            role: pdfcraft_engine::read_aloud::Role::Text,
        };
        assert!(ends_sentence("Done.") && ends_sentence("He said “so.”") && ends_sentence("") && !ends_sentence("runs on to the"));
        let mut found: VecDeque<Passage> = [passage(0, "Running Header"), passage(1, "next page. Then more."), passage(2, "Another.")].into();
        assert_eq!(take_continuation(&mut found).as_deref(), Some("next page."));
        assert_eq!(found.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["Running Header", "Then more.", "Another."]);
        let mut whole: VecDeque<Passage> = [passage(0, "only the end")].into();
        assert_eq!(take_continuation(&mut whole).as_deref(), Some("only the end"));
        assert!(whole.is_empty());
        let mut new: VecDeque<Passage> = [passage(0, "A new paragraph."), passage(1, "lowercase but after a sentence")].into();
        assert_eq!(take_continuation(&mut new), None);
        assert_eq!(take_continuation(&mut VecDeque::new()), None);
        let mut text = "a hyphen-".to_string();
        join_across_pages(&mut text, "ated word.");
        assert_eq!(text, "a hyphenated word.");
        let mut text = "the".to_string();
        join_across_pages(&mut text, "end.");
        assert_eq!(text, "the end.");
    }

    #[test]
    fn sentences_are_timed_by_the_words_or_by_their_length() {
        let starts = [(0, 0..10), (11, 11..30), (40, 31..60)];
        let timed = Speech { audio: Vec::new(), duration: Some(9.0), words: vec![(0.1, 0), (2.5, 11), (3.0, 20), (6.0, 40)] };
        assert_eq!(marks(&timed, &starts, 60), [(0.0, 0..10), (2.5, 11..30), (6.0, 31..60)]);
        let untimed = Speech { audio: Vec::new(), duration: Some(6.0), words: Vec::new() };
        assert_eq!(marks(&untimed, &starts, 60), [(0.0, 0..10), (1.1, 11..30), (4.0, 31..60)]);
        let nothing = Speech::default();
        assert_eq!(marks(&nothing, &starts, 0), [(0.0, 0..10), (0.0, 11..30), (0.0, 31..60)]);
        // Pieces keep their sentences' place in the paragraph, from the sentence clicked on.
        assert_eq!(pieces_of("One. Two. Three.", 5), [vec![(5..9, "Two.".to_string()), (10..16, "Three.".to_string())]]);
        assert_eq!(pieces_of("Ünï. Two.", 1), pieces_of("Ünï. Two.", 0), "a cut inside a character starts from the top");
    }

    #[test]
    fn a_fast_start_sends_the_first_sentence_or_clause_alone() {
        let mut pieces = pieces_of("One. Two. Three.", 0);
        fast_first(&mut pieces);
        assert_eq!(pieces, [vec![(0..4, "One.".to_string())], vec![(5..9, "Two.".to_string()), (10..16, "Three.".to_string())]]);
        let long = format!("Although the first clause runs on for a while, {} ends here.", "and on ".repeat(15));
        let mut pieces = pieces_of(&long, 0);
        fast_first(&mut pieces);
        assert_eq!(pieces.first().map(|p| p[0].1.as_str()), Some("Although the first clause runs on for a while,"));
        assert_eq!(pieces.get(1).map(|p| (p[0].0.start, p[0].1.starts_with("and on"))), Some((47, true)));
        let mut none = Vec::new();
        fast_first(&mut none);
        assert!(none.is_empty());
    }

    #[test]
    fn english_requests_say_ranges_and_numbers_as_words() {
        let english = Prefs { language: "en-GB".into(), ..Prefs::default() };
        assert_eq!(
            english.spoken("Rose 0.6%–0.9%, 194 times. See Fig. 2."),
            "Rose zero point six percent to zero point nine percent, one hundred ninety-four times. See Figure two."
        );
        let digits = Prefs { spell_numbers: false, ..Prefs::default() };
        assert_eq!(digits.spoken("Rose 0.6%–0.9%."), "Rose 0.6% to 0.9%.");
        let german = Prefs { language: "de".into(), ..Prefs::default() };
        assert_eq!(german.spoken("0,6–0,9 %"), "0,6–0,9 %");
    }

    /// Records what it is asked and "plays" instantly; `fail` makes synthesis fail.
    #[derive(Default)]
    struct Fake {
        said: Mutex<Vec<String>>,
        fail: bool,
    }

    impl Speaker for Fake {
        fn synthesize(&self, request: &SpeechRequest) -> Result<Speech, String> {
            if self.fail {
                return Err("server unreachable".into());
            }
            self.said.lock().map_err(|e| e.to_string())?.push(request.text.clone());
            // One second per word, each word timed.
            let mut words = Vec::new();
            let mut chars = 0;
            for (i, w) in request.text.split(' ').enumerate() {
                words.push((i as f64, chars));
                chars += w.chars().count() + 1;
            }
            Ok(Speech { audio: request.text.clone().into_bytes(), duration: Some(words.len() as f64), words })
        }
        fn play(&self, _: &[u8], _: &Control, at: &mut dyn FnMut(f64)) -> Result<(), String> {
            at(f64::MAX);
            Ok(())
        }
    }

    fn job(seq: u64, text: &str) -> Job {
        Job { seq, pieces: pieces_of(text, 0) }
    }

    fn drain(events: &std::sync::mpsc::Receiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = events.recv_timeout(std::time::Duration::from_secs(10)) {
            let end = matches!(e, Event::Finished | Event::Failed(_));
            out.push(e);
            if end {
                break;
            }
        }
        out
    }

    #[test]
    fn the_threads_speak_every_paragraph_in_order_then_finish() {
        let fake = Arc::new(Fake::default());
        let prefs = Prefs { server: "https://x".into(), ..Prefs::default() };
        let (jobs, events) = spawn(fake.clone(), prefs, Arc::new(Control::default()), None).unwrap();
        jobs.send(job(0, "One. Two.")).unwrap();
        jobs.send(job(1, "Three.")).unwrap();
        drop(jobs);
        assert_eq!(
            drain(&events),
            [Event::Speaking(0), Event::At(0, 0..4), Event::At(0, 5..9), Event::Speaking(1), Event::At(1, 0..6), Event::Finished]
        );
        assert_eq!(*fake.said.lock().unwrap(), ["One. Two.", "Three."]);
    }

    #[test]
    fn a_failing_server_ends_the_reading_with_its_reason() {
        let fake = Arc::new(Fake { fail: true, ..Fake::default() });
        let (jobs, events) = spawn(fake, Prefs::default(), Arc::new(Control::default()), None).unwrap();
        jobs.send(job(0, "Hello.")).unwrap();
        assert_eq!(drain(&events), [Event::Failed("server unreachable".into())]);
    }

    #[test]
    fn stopping_asks_for_nothing_more() {
        let fake = Arc::new(Fake::default());
        let control = Arc::new(Control::default());
        control.stop.store(true, Ordering::Relaxed);
        let (jobs, events) = spawn(fake.clone(), Prefs::default(), control, None).unwrap();
        jobs.send(job(0, "Hello.")).unwrap();
        drop(jobs);
        // Stopped threads end without a word.
        assert!(events.recv_timeout(std::time::Duration::from_secs(10)).is_err());
        assert!(fake.said.lock().unwrap().is_empty());
    }
}
