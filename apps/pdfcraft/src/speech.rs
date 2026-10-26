//! View ▸ Read Out Loud's speech: asks the speech server set in Preferences and plays its answer.
//!
//! The server takes an OpenAI-style request (`POST /v1/audio/speech` with `input`, `voice`,
//! `language` and `response_format: "wav"`) and answers with WAV audio, which plays on the
//! default output device. A server that also offers `/v1/audio/speech/with-timestamps` answers
//! with JSON holding the WAV (`audio_base64`) and when each word starts (`words`: `start` in
//! seconds, `text_start` in characters of `input`); that is asked first, and a server without it
//! (404 or 405) is asked the plain way from then on. Nothing is sent until the user starts
//! reading, and only to the server they entered.

use std::cell::RefCell;
use std::num::NonZero;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use pdfcraft_ui_egui::read_aloud::{Control, Speaker, Speech, SpeechRequest};

/// The largest answer read: about five minutes of 24 kHz 16-bit stereo, far more than one
/// sentence takes.
const MAX_AUDIO_BYTES: u64 = 32 << 20;
/// The largest JSON answer read: the audio in base64 (a third larger) and the word timings.
const MAX_TIMED_BYTES: u64 = MAX_AUDIO_BYTES * 3 / 2;
/// The most word timings kept from one answer: far more than one request's words.
const MAX_WORDS: usize = 4096;
/// A server loading its model answers the first request late; later ones take seconds.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Reads aloud through a speech server.
#[derive(Default)]
pub struct HttpSpeaker {
    agent: OnceLock<Result<ureq::Agent, String>>,
    /// The server answered that it has no timestamps endpoint.
    untimed: AtomicBool,
}

impl HttpSpeaker {
    fn agent(&self) -> Result<&ureq::Agent, String> {
        self.agent
            .get_or_init(|| {
                let roots = os_roots()?;
                Ok(ureq::Agent::config_builder()
                    .timeout_global(Some(TIMEOUT))
                    .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build())
                    .http_status_as_error(false)
                    .build()
                    .new_agent())
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

impl HttpSpeaker {
    /// POST `request` to `path` and read at most `limit` bytes of the answer. `Ok(None)` when
    /// the server has no such endpoint (404 or 405).
    fn post(&self, request: &SpeechRequest, path: &str, limit: u64) -> Result<Option<Vec<u8>>, String> {
        let url = format!("{}{path}", request.server);
        let mut body = serde_json::json!({ "input": request.text, "language": request.language, "response_format": "wav" });
        if let Some(voice) = &request.voice {
            body["voice"] = serde_json::Value::String(voice.clone());
        }
        let mut response = self
            .agent()?
            .post(&url)
            .header("User-Agent", concat!("PdfCraft/", env!("CARGO_PKG_VERSION")))
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())
            .map_err(|e| format!("couldn't reach the speech server ({e})"))?;
        let status = response.status().as_u16();
        if matches!(status, 404 | 405) {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            let detail = response.body_mut().with_config().limit(4096).read_to_string().unwrap_or_default();
            return Err(server_error(status, &detail));
        }
        response.body_mut().with_config().limit(limit).read_to_vec().map(Some).map_err(|e| format!("the speech server's answer was unreadable ({e})"))
    }
}

impl Speaker for HttpSpeaker {
    fn synthesize(&self, request: &SpeechRequest) -> Result<Speech, String> {
        if !self.untimed.load(Ordering::Relaxed) {
            match self.post(request, "/v1/audio/speech/with-timestamps", MAX_TIMED_BYTES)? {
                Some(answer) => return timed(&answer),
                None => self.untimed.store(true, Ordering::Relaxed),
            }
        }
        let audio = self.post(request, "/v1/audio/speech", MAX_AUDIO_BYTES)?.ok_or_else(|| server_error(404, ""))?;
        let duration = Wav::parse(&audio).ok().map(|w| w.seconds());
        Ok(Speech { audio, duration, words: Vec::new() })
    }

    fn play(&self, audio: &[u8], control: &Control, at: &mut dyn FnMut(f64)) -> Result<(), String> {
        let wav = Wav::parse(audio)?;
        // One output stream per reading thread: opening the device for every sentence would put
        // a gap between them. It closes when the reading ends and its thread exits.
        thread_local! {
            static OUTPUT: RefCell<Option<rodio::MixerDeviceSink>> = const { RefCell::new(None) };
        }
        OUTPUT.with(|output| {
            let mut output = output.borrow_mut();
            if output.is_none() {
                let mut sink = rodio::DeviceSinkBuilder::open_default_sink().map_err(|e| format!("no sound output ({e})"))?;
                sink.log_on_drop(false);
                *output = Some(sink);
            }
            let Some(sink) = output.as_ref() else { return Err("no sound output".to_string()) };
            let player = rodio::Player::connect_new(sink.mixer());
            player.append(rodio::buffer::SamplesBuffer::new(wav.channels, wav.rate, wav.samples));
            loop {
                if control.stopped() {
                    player.stop();
                    return Ok(());
                }
                if control.paused() != player.is_paused() {
                    if control.paused() { player.pause() } else { player.play() }
                }
                if player.empty() {
                    return Ok(());
                }
                at(player.get_pos().as_secs_f64());
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    }
}

/// The timestamps endpoint's answer: the WAV in `audio_base64`, `duration` in seconds, and
/// `words` with `start` (seconds) and `text_start` (characters). The answer is the server's and
/// untrusted: timings that aren't finite, non-negative numbers are dropped, and so are words the
/// server says it couldn't align.
fn timed(answer: &[u8]) -> Result<Speech, String> {
    const BAD: &str = "the speech server's timed answer is malformed";
    let v: serde_json::Value = serde_json::from_slice(answer).map_err(|_| BAD.to_string())?;
    let audio = base64::engine::general_purpose::STANDARD.decode(v["audio_base64"].as_str().ok_or(BAD)?.as_bytes()).map_err(|_| BAD.to_string())?;
    let mut words: Vec<(f64, usize)> = v["words"]
        .as_array()
        .map(|words| {
            words
                .iter()
                .filter(|w| w["aligned"].as_bool() != Some(false))
                .filter_map(|w| Some((w["start"].as_f64().filter(|s| s.is_finite() && *s >= 0.0)?, usize::try_from(w["text_start"].as_u64()?).ok()?)))
                .take(MAX_WORDS)
                .collect()
        })
        .unwrap_or_default();
    words.sort_by_key(|(_, at)| *at);
    let duration = v["duration"].as_f64().filter(|d| d.is_finite() && *d >= 0.0).or_else(|| Wav::parse(&audio).ok().map(|w| w.seconds()));
    Ok(Speech { audio, duration, words })
}

/// What to say when the server refuses: its own `detail` when it gives one (FastAPI's error
/// body), shortened.
fn server_error(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body).ok().and_then(|v| match &v["detail"] {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => items.first().and_then(|i| i["msg"].as_str()).map(str::to_string),
        _ => None,
    });
    match detail {
        Some(d) => format!("the speech server refused (http status {status}: {})", d.chars().take(200).collect::<String>()),
        None => format!("the speech server refused (http status {status})"),
    }
}

/// Decoded WAV audio.
#[derive(Debug)]
struct Wav {
    channels: NonZero<u16>,
    rate: NonZero<u32>,
    samples: Vec<rodio::Sample>,
}

impl Wav {
    /// PCM 8/16/24/32-bit integer or 32-bit float WAV. The bytes are the server's and untrusted:
    /// every length is checked against what is there.
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        const BAD: &str = "the speech server's answer isn't WAV audio";
        if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
            return Err(BAD.into());
        }
        // `get` returned exactly 2 (or 4) bytes, so the indexes below are in range.
        let u16_at = |at: usize| bytes.get(at..at.checked_add(2)?).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let u32_at = |at: usize| bytes.get(at..at.checked_add(4)?).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        let mut format = None;
        let mut data = None;
        let mut at = 12usize;
        while let (Some(id), Some(len)) = (bytes.get(at..at.saturating_add(4)), u32_at(at.saturating_add(4))) {
            let start = at.saturating_add(8);
            // Streamed WAV often says 0 or 0xFFFFFFFF for the data length: take what is there.
            let end = start.saturating_add(len as usize).min(bytes.len());
            match id {
                b"fmt " => {
                    let tag = u16_at(start).ok_or(BAD)?;
                    let channels = u16_at(start + 2).and_then(NonZero::new).ok_or(BAD)?;
                    let rate = u32_at(start + 4).and_then(NonZero::new).ok_or(BAD)?;
                    let bits = u16_at(start + 14).ok_or(BAD)?;
                    // WAVE_FORMAT_EXTENSIBLE names the real format in its sub-format GUID.
                    let tag = if tag == 0xFFFE { u16_at(start + 24).ok_or(BAD)? } else { tag };
                    format = Some((tag, channels, rate, bits));
                }
                b"data" => {
                    data = bytes.get(start..end);
                    break;
                }
                _ => {}
            }
            // Chunks are padded to an even length.
            at = start.saturating_add(len as usize).saturating_add(len as usize & 1);
        }
        let (tag, channels, rate, bits) = format.ok_or(BAD)?;
        let data = data.ok_or(BAD)?;
        if channels.get() > 8 || rate.get() > 384_000 {
            return Err(BAD.into());
        }
        let samples: Vec<rodio::Sample> = match (tag, bits) {
            (1, 8) => data.iter().map(|&b| (f32::from(b) - 128.0) / 128.0).map(to_sample).collect(),
            (1, 16) => data.as_chunks::<2>().0.iter().map(|b| f32::from(i16::from_le_bytes(*b)) / 32_768.0).map(to_sample).collect(),
            (1, 24) => {
                data.as_chunks::<3>().0.iter().map(|&[a, b, c]| (i32::from_le_bytes([0, a, b, c]) >> 8) as f32 / 8_388_608.0).map(to_sample).collect()
            }
            (1, 32) => data.as_chunks::<4>().0.iter().map(|b| i32::from_le_bytes(*b) as f32 / 2_147_483_648.0).map(to_sample).collect(),
            (3, 32) => data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .map(|s| if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 })
                .map(to_sample)
                .collect(),
            _ => return Err(format!("the speech server's audio format isn't supported (format {tag}, {bits} bits)")),
        };
        Ok(Self { channels, rate, samples })
    }

    /// How long it plays.
    fn seconds(&self) -> f64 {
        self.samples.len() as f64 / f64::from(self.channels.get()) / f64::from(self.rate.get())
    }
}

fn to_sample(s: f32) -> rodio::Sample {
    rodio::Sample::from(s)
}


/// The certificate authorities the operating system trusts.
fn os_roots() -> Result<ureq::tls::RootCerts, String> {
    let found = rustls_native_certs::load_native_certs();
    let certs: Vec<ureq::tls::Certificate<'static>> = found.certs.iter().map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned()).collect();
    if certs.is_empty() {
        return Err("no trusted certificates found on this system".into());
    }
    Ok(ureq::tls::RootCerts::new_with_certs(&certs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(tag: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        let mut out = b"RIFF\0\0\0\0WAVE".to_vec();
        out.extend(b"fmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(tag.to_le_bytes());
        out.extend(channels.to_le_bytes());
        out.extend(rate.to_le_bytes());
        out.extend((rate * u32::from(channels) * u32::from(bits / 8)).to_le_bytes());
        out.extend((channels * bits / 8).to_le_bytes());
        out.extend(bits.to_le_bytes());
        // An unknown chunk of odd length, padded, before the data.
        out.extend(b"LIST");
        out.extend(3u32.to_le_bytes());
        out.extend([1, 2, 3, 0]);
        out.extend(b"data");
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        out
    }

    #[test]
    fn pcm16_mono_decodes_to_samples() {
        let data: Vec<u8> = [0i16, 16_384, -32_768].iter().flat_map(|s| s.to_le_bytes()).collect();
        let w = Wav::parse(&wav(1, 1, 24_000, 16, &data)).unwrap();
        assert_eq!((w.channels.get(), w.rate.get()), (1, 24_000));
        assert_eq!(w.samples, [0.0, 0.5, -1.0].map(to_sample));
    }

    #[test]
    fn a_streamed_data_length_takes_what_is_there() {
        let mut bytes = wav(1, 1, 24_000, 16, &[0, 0, 0, 64]);
        let len_at = bytes.len() - 8;
        bytes[len_at..len_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Wav::parse(&bytes).unwrap().samples.len(), 2);
    }

    #[test]
    fn malformed_audio_is_an_error_not_a_crash() {
        let good = wav(1, 2, 44_100, 16, &[0; 16]);
        for cut in 0..good.len() {
            let _ = Wav::parse(&good[..cut]);
        }
        assert!(Wav::parse(b"{\"detail\":\"no\"}").is_err());
        assert!(Wav::parse(&wav(1, 0, 24_000, 16, &[0; 4])).is_err(), "no channels");
        assert!(Wav::parse(&wav(1, 1, 0, 16, &[0; 4])).is_err(), "no sample rate");
        assert!(Wav::parse(&wav(2, 1, 24_000, 4, &[0; 4])).is_err(), "ADPCM");
        // A chunk length that runs past the end.
        let mut huge = good.clone();
        huge[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Wav::parse(&huge).is_err());
    }

    #[test]
    fn float_samples_are_clamped_and_nan_is_silence() {
        let data: Vec<u8> = [0.25f32, 7.0, f32::NAN].iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(Wav::parse(&wav(3, 1, 24_000, 32, &data)).unwrap().samples, [0.25, 1.0, 0.0].map(to_sample));
    }

    #[test]
    fn timed_answers_give_the_audio_and_the_words_start() {
        let wav_bytes = wav(1, 1, 24_000, 16, &[0; 96]);
        let audio = base64::engine::general_purpose::STANDARD.encode(&wav_bytes);
        let answer = serde_json::json!({
            "audio_base64": audio,
            "duration": 1.5,
            "words": [
                { "word": "Hello", "start": 0.02, "end": 0.2, "text_start": 0, "text_end": 5, "aligned": true },
                { "word": "lost", "start": 0.3, "text_start": 6, "aligned": false },
                { "word": "bad", "start": -1.0, "text_start": 11 },
                { "word": "there.", "start": 0.4, "text_start": 15 }
            ]
        });
        let speech = timed(answer.to_string().as_bytes()).unwrap();
        assert_eq!(speech.audio, wav_bytes);
        assert_eq!(speech.duration, Some(1.5));
        assert_eq!(speech.words, [(0.02, 0), (0.4, 15)]);
        // Without a duration, the WAV says how long it plays: 48 samples at 24 kHz.
        let short = serde_json::json!({ "audio_base64": audio });
        assert_eq!(timed(short.to_string().as_bytes()).unwrap().duration, Some(0.002));
        for bad in [&b"not json"[..], br#"{"audio_base64": 3}"#, br#"{"audio_base64": "!!"}"#] {
            assert!(timed(bad).is_err());
        }
    }

    #[test]
    fn server_errors_quote_the_servers_detail() {
        assert_eq!(server_error(503, r#"{"detail":"model not loaded"}"#), "the speech server refused (http status 503: model not loaded)");
        let validation = r#"{"detail":[{"loc":["body","input"],"msg":"String should have at most 3000 characters"}]}"#;
        assert!(server_error(422, validation).ends_with("at most 3000 characters)"));
        assert_eq!(server_error(500, "<html>"), "the speech server refused (http status 500)");
    }
}
