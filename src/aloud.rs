//! Reading a document aloud: the text is cut into passages a sentence or a
//! paragraph long, each spoken by an online voice a little ahead of time,
//! and played back in order at any speed, with the word being said known
//! from moment to moment so the reader can mark it.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::edge::{self, EdgeFailed, Voice, Word};
use crate::tempo::Stretch;

/// Speech comes at 24 kHz mono.
pub const SAMPLE_RATE: u32 = 24_000;
/// The speeds `[` and `]` step through.
pub const SPEEDS: [f32; 10] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0];
/// Passages spoken ahead of the one playing, so the next is ready in time.
const AHEAD: usize = 3;
/// The first passage after starting is one sentence, so speech starts at
/// once; the rest run to about this many characters.
const PASSAGE: usize = 480;
/// The most a passage holds; the service takes up to 4 KB of text.
const LONGEST: usize = 1500;
/// Output samples worked out at a time on the audio thread (about 10 ms).
const BLOCK: usize = 256;
/// How far ahead a spoken word is looked for in the text.
const LOOKAHEAD: usize = 80;

/// A place in a page's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Spot {
    pub page: usize,
    pub offset: usize,
}

/// The word being spoken: characters `start..end` of a page's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Said {
    pub page: usize,
    pub start: usize,
    pub end: usize,
}

/// Speech for a passage: samples at [`SAMPLE_RATE`] and the words in it.
pub struct Speech {
    pub samples: Vec<f32>,
    pub words: Vec<Word>,
}

/// Turns text into speech in a voice. Called off the main thread.
pub type Synth = Arc<dyn Fn(&str, &str) -> Result<Speech, EdgeFailed> + Send + Sync>;

/// Where speech goes.
pub enum Backend {
    /// Microsoft's online voices, played on the default audio device.
    Edge,
    /// Speech from `synth`, played nowhere; tests pull samples from the deck.
    #[cfg(test)]
    Silent(Synth),
}

/// Microsoft's online voices.
fn edge_synth() -> Synth {
    Arc::new(|text, voice| {
        let spoken = edge::speak(text, voice)?;
        let samples = decode(&spoken.audio).ok_or_else(|| EdgeFailed::Silent {
            voice: voice.to_owned(),
        })?;
        Ok(Speech {
            samples,
            words: spoken.words,
        })
    })
}

/// The samples of an MP3, mixed down to mono at [`SAMPLE_RATE`].
fn decode(mp3: &[u8]) -> Option<Vec<f32>> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let source = MediaSourceStream::new(
        Box::new(std::io::Cursor::new(mp3.to_vec())),
        Default::default(),
    );
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .ok()?;
    let track = format.default_track(TrackType::Audio)?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(
            track.codec_params.as_ref()?.audio()?,
            &AudioDecoderOptions::default(),
        )
        .ok()?;
    let (mut samples, mut rate) = (Vec::new(), SAMPLE_RATE);
    let mut frame: Vec<f32> = Vec::new();
    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != track_id {
            continue;
        }
        let Ok(audio) = decoder.decode(&packet) else {
            continue;
        };
        let channels = audio.spec().channels().count().max(1);
        rate = audio.spec().rate();
        frame.resize(audio.samples_interleaved(), 0.0);
        audio.copy_to_slice_interleaved(&mut frame);
        samples.extend(
            frame
                .chunks(channels)
                .map(|c| c.iter().sum::<f32>() / channels as f32),
        );
    }
    if samples.is_empty() {
        return None;
    }
    Some(resample(&samples, rate))
}

/// Linear resampling to [`SAMPLE_RATE`]; the service already sends that, so
/// this is only a safeguard.
fn resample(samples: &[f32], rate: u32) -> Vec<f32> {
    if rate == SAMPLE_RATE || rate == 0 {
        return samples.to_vec();
    }
    let step = rate as f64 / SAMPLE_RATE as f64;
    let len = (samples.len() as f64 / step) as usize;
    (0..len)
        .map(|i| {
            let at = i as f64 * step;
            let (j, t) = (at as usize, (at.fract()) as f32);
            let a = samples[j.min(samples.len() - 1)];
            let b = samples[(j + 1).min(samples.len() - 1)];
            a + (b - a) * t
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Cutting the text.

/// Whether the sentence ends at character `i`: a line break, or `.`, `!`,
/// `?` or `…` (and any closing quotes or brackets) before a space.
fn ends_sentence(text: &[char], i: usize) -> bool {
    match text.get(i) {
        Some('\n') => true,
        Some('.' | '!' | '?' | '…') => {
            let mut next = i + 1;
            while matches!(
                text.get(next),
                Some('"' | '\'' | '”' | '’' | ')' | ']' | '.' | '!' | '?')
            ) {
                next += 1;
            }
            text.get(next).is_none_or(|c| c.is_whitespace())
        }
        _ => false,
    }
}

/// Where the sentence holding character `at` ends: just past its last mark.
pub fn sentence_end(text: &[char], at: usize) -> usize {
    let mut i = at;
    while i < text.len() {
        if ends_sentence(text, i) {
            i += 1;
            while matches!(
                text.get(i),
                Some('"' | '\'' | '”' | '’' | ')' | ']' | '.' | '!' | '?')
            ) {
                i += 1;
            }
            return i;
        }
        i += 1;
    }
    text.len()
}

/// Where the sentence holding character `at` starts.
pub fn sentence_start(text: &[char], at: usize) -> usize {
    let at = at.min(text.len());
    let mut i = at;
    while i > 0 && !ends_sentence(text, i - 1) {
        i -= 1;
    }
    // Past closing quotes left over from the sentence before.
    while i < at && text[i].is_whitespace() {
        i += 1;
    }
    i
}

/// The next passage from `from`: whole sentences, at least `budget`
/// characters of them unless the page ends first, and never more than
/// [`LONGEST`]. `None` once only spaces are left.
pub fn cut(text: &[char], from: usize, budget: usize) -> Option<(usize, usize)> {
    let start = (from..text.len()).find(|&i| !text[i].is_whitespace())?;
    let mut end = start;
    while end < text.len() && end - start < budget.max(1) {
        end = sentence_end(text, end);
    }
    if end - start > LONGEST {
        let limit = start + LONGEST;
        end = (start + LONGEST / 2..limit)
            .rev()
            .find(|&i| text[i].is_whitespace())
            .unwrap_or(limit);
    }
    Some((start, end))
}

/// Whether there's anything in a passage to say.
fn sayable(text: &[char]) -> bool {
    text.iter().any(|c| c.is_alphanumeric())
}

/// Each spoken word's sample and characters in `text`, found in order.
/// Words that can't be found are left out.
fn align(text: &[char], words: &[Word]) -> Vec<(usize, usize, usize)> {
    let same = |a: char, b: char| a == b || a.to_lowercase().eq(b.to_lowercase());
    let mut cursor = 0;
    let mut timed = Vec::with_capacity(words.len());
    for word in words {
        let needle: Vec<char> = word.text.chars().collect();
        if !sayable(&needle) {
            continue;
        }
        let last = (cursor + LOOKAHEAD).min(text.len().saturating_sub(needle.len()));
        let found = (cursor..=last).find(|&i| {
            text.get(i..i + needle.len())
                .is_some_and(|span| span.iter().zip(&needle).all(|(&a, &b)| same(a, b)))
        });
        if let Some(i) = found {
            let sample =
                (word.at as u128 * SAMPLE_RATE as u128 / edge::TICKS_PER_SECOND as u128) as usize;
            timed.push((sample, i, i + needle.len()));
            cursor = i + needle.len();
        }
    }
    timed
}

/// Voices matching what's typed, best first: every word typed must start a
/// word of the voice's name, language, locale or gender, or be spelled out
/// in order in its name.
pub fn rank<'a>(voices: &'a [Voice], typed: &str) -> Vec<&'a Voice> {
    let terms: Vec<String> = typed.split_whitespace().map(str::to_lowercase).collect();
    let subsequence = |hay: &str, term: &str| {
        let mut hay = hay.chars();
        term.chars().all(|c| hay.any(|h| h == c))
    };
    let mut scored: Vec<(usize, &Voice)> = voices
        .iter()
        .filter_map(|voice| {
            let short = voice.short().to_lowercase();
            let about =
                format!("{} {} {}", voice.language, voice.locale, voice.gender).to_lowercase();
            let locale = voice.locale.to_lowercase();
            let words: Vec<&str> = about
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .chain([locale.as_str()])
                .collect();
            terms
                .iter()
                .try_fold(0, |score, term| {
                    if short.starts_with(term.as_str()) {
                        Some(score + 3)
                    } else if words.iter().any(|w| w.starts_with(term.as_str())) {
                        Some(score + 2)
                    } else if subsequence(&short, term) {
                        Some(score)
                    } else {
                        None
                    }
                })
                .map(|score| (score, voice))
        })
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, voice)| voice).collect()
}

/// The next speed up or down from `speed`.
pub fn step_speed(speed: f32, faster: bool) -> f32 {
    if faster {
        SPEEDS.into_iter().find(|&s| s > speed + 0.01)
    } else {
        SPEEDS.into_iter().rev().find(|&s| s < speed - 0.01)
    }
    .unwrap_or(speed)
}

/// `1.5×`, `2×`.
pub fn speed_label(speed: f32) -> String {
    let text = format!("{speed:.2}");
    format!("{}×", text.trim_end_matches('0').trim_end_matches('.'))
}

// ---------------------------------------------------------------------------
// Playing.

struct Clip {
    serial: u64,
    samples: Vec<f32>,
}

/// What the audio thread plays, shared with the reader.
struct Deck {
    clips: VecDeque<Clip>,
    stretch: Stretch,
    speed: f32,
    paused: bool,
    /// The clip playing and the sample of it being heard.
    at: Option<(u64, usize)>,
    /// The last clip played to its end.
    done: Option<u64>,
}

impl Deck {
    fn new(speed: f32) -> Self {
        Deck {
            clips: VecDeque::new(),
            stretch: Stretch::default(),
            speed,
            paused: false,
            at: None,
            done: None,
        }
    }

    fn clear(&mut self) {
        self.clips.clear();
        self.stretch = Stretch::default();
        self.at = None;
        self.done = None;
        self.paused = false;
    }

    /// Fills `block` with what's heard next: silence when paused or waiting.
    fn play(&mut self, block: &mut [f32]) {
        let Deck {
            clips,
            stretch,
            speed,
            paused,
            done,
            ..
        } = self;
        for slot in block.iter_mut() {
            *slot = 0.0;
            if *paused {
                continue;
            }
            while let Some(clip) = clips.front() {
                match stretch.next(&clip.samples, *speed) {
                    Some(sample) => {
                        *slot = sample;
                        break;
                    }
                    None => {
                        *done = Some(clip.serial);
                        clips.pop_front();
                        *stretch = Stretch::default();
                    }
                }
            }
        }
        self.at = self
            .clips
            .front()
            .map(|clip| (clip.serial, self.stretch.position()));
    }
}

/// The audio thread's end of the deck: an endless stream of samples.
struct Playhead {
    deck: Arc<Mutex<Deck>>,
    block: Vec<f32>,
    next: usize,
}

impl Playhead {
    fn new(deck: Arc<Mutex<Deck>>) -> Self {
        Playhead {
            deck,
            block: vec![0.0; BLOCK],
            next: BLOCK,
        }
    }
}

impl Iterator for Playhead {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.next >= self.block.len() {
            match self.deck.lock() {
                Ok(mut deck) => deck.play(&mut self.block),
                Err(_) => self.block.fill(0.0),
            }
            self.next = 0;
        }
        let sample = self.block[self.next];
        self.next += 1;
        Some(sample)
    }
}

impl rodio::Source for Playhead {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> rodio::ChannelCount {
        rodio::ChannelCount::MIN
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        rodio::SampleRate::new(SAMPLE_RATE).unwrap_or(rodio::SampleRate::MIN)
    }
    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

/// Opens an audio output, trying what's most likely to work first. Bluetooth
/// headphones on macOS refuse the fixed 50 ms buffer rodio asks for, so the
/// device's own buffer size comes first, then each config the device lists,
/// then the other output devices. Stream errors are dropped rather than
/// printed over the reader.
pub(crate) fn open_device() -> Result<rodio::MixerDeviceSink, rodio::DeviceSinkError> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    use rodio::{DeviceSinkBuilder, cpal};

    let quiet = |_: cpal::Error| {};
    let attempt = |device: cpal::Device| {
        let builder = DeviceSinkBuilder::from_device(device)?;
        builder
            .with_buffer_size(cpal::BufferSize::Default)
            .with_error_callback(quiet)
            .open_sink_or_fallback()
    };
    let default = cpal::default_host()
        .default_output_device()
        .ok_or(rodio::DeviceSinkError::NoDevice)?;
    let default_name = default.id().ok();
    attempt(default).or_else(|first| {
        let others = cpal::default_host()
            .output_devices()
            .map_err(|_| rodio::DeviceSinkError::NoDevice)?;
        others
            .filter(|d| d.id().ok() != default_name)
            .find_map(|d| attempt(d).ok())
            .ok_or(first)
    })
}

// ---------------------------------------------------------------------------
// The narrator.

enum Stage {
    /// Being spoken by the service.
    Asked,
    /// Spoken, waiting for the passages before it to be.
    Ready {
        samples: Vec<f32>,
        timed: Vec<(usize, usize, usize)>,
    },
    /// On the deck. Each word: its sample and characters.
    Queued(Vec<(usize, usize, usize)>),
    /// Nothing came back to say; passed over.
    Skipped,
}

struct Passage {
    serial: u64,
    page: usize,
    start: usize,
    /// The passage's characters, from `start` on the page.
    text: Vec<char>,
    stage: Stage,
}

struct Session {
    paused: bool,
    passages: VecDeque<Passage>,
    /// Where the next passage starts; `None` past the end of the document.
    next: Option<Spot>,
    /// Whether a passage has been cut yet; the first is kept short.
    started: bool,
    said: Option<Said>,
}

/// A passage cut from the text, or why there isn't one.
pub enum Cut {
    Passage {
        page: usize,
        start: usize,
        text: Vec<char>,
    },
    /// The page has nothing more to say.
    PageDone,
    /// The document ended.
    End,
}

struct Synthesized {
    generation: u64,
    serial: u64,
    result: Result<Speech, EdgeFailed>,
}

/// The full voice list: asked for once, when the picker first opens.
enum Listing {
    NotAsked,
    Fetching(mpsc::Receiver<Result<Vec<Voice>, EdgeFailed>>),
    /// Arrived, or failed and left at the built-in few.
    Asked,
}

/// What the narrator is doing, for the footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    /// Waiting on the first speech.
    Loading,
    Speaking,
    Paused,
}

pub struct Narrator {
    synth: Synth,
    /// Whether to play on the audio device.
    audible: bool,
    deck: Arc<Mutex<Deck>>,
    output: Option<rodio::MixerDeviceSink>,
    voice: String,
    speed: f32,
    voices: Vec<Voice>,
    listing: Listing,
    results_tx: mpsc::Sender<Synthesized>,
    results_rx: mpsc::Receiver<Synthesized>,
    /// Bumped at each start and stop, so speech asked for before is dropped.
    generation: u64,
    next_serial: u64,
    session: Option<Session>,
    /// Where reading stopped, to pick up from.
    resume: Option<Spot>,
    /// Something to tell the reader, once.
    notice: Option<String>,
}

impl Narrator {
    pub fn new(voice: String, speed: f32, backend: Backend) -> Self {
        let (synth, audible) = match backend {
            Backend::Edge => (edge_synth(), true),
            #[cfg(test)]
            Backend::Silent(synth) => (synth, false),
        };
        let speed = SPEEDS
            .into_iter()
            .min_by(|a, b| (a - speed).abs().total_cmp(&(b - speed).abs()))
            .unwrap_or(1.0);
        let (results_tx, results_rx) = mpsc::channel();
        Narrator {
            synth,
            audible,
            deck: Arc::new(Mutex::new(Deck::new(speed))),
            output: None,
            voice,
            speed,
            voices: edge::built_in(),
            listing: Listing::NotAsked,
            results_tx,
            results_rx,
            generation: 0,
            next_serial: 0,
            session: None,
            resume: None,
            notice: None,
        }
    }

    pub fn voice(&self) -> &str {
        &self.voice
    }

    /// The voice's short name, e.g. "Emma".
    pub fn voice_label(&self) -> String {
        self.voices
            .iter()
            .find(|v| v.name == self.voice)
            .map(|v| v.short().to_owned())
            .unwrap_or_else(|| self.voice.clone())
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    pub fn status(&self) -> Status {
        match &self.session {
            None => Status::Idle,
            Some(s) if s.paused => Status::Paused,
            Some(s) if s.said.is_none() => Status::Loading,
            Some(_) => Status::Speaking,
        }
    }

    pub fn active(&self) -> bool {
        self.session.is_some()
    }

    /// Makes the voice list be on its way; the test sends it when it likes.
    #[cfg(test)]
    pub fn fetching_voices(&mut self) -> mpsc::Sender<Result<Vec<Voice>, EdgeFailed>> {
        let (tx, rx) = mpsc::channel();
        self.listing = Listing::Fetching(rx);
        tx
    }

    /// Plays `seconds` of output into nowhere, as the audio device would.
    #[cfg(test)]
    pub fn play_for(&self, seconds: f32) {
        let mut head = Playhead::new(self.deck.clone());
        for _ in 0..(seconds * SAMPLE_RATE as f32).ceil() as usize {
            head.next();
        }
    }

    /// Whether anything is on its way: speech, or the voice list.
    pub fn busy(&self) -> bool {
        self.session.is_some() || matches!(self.listing, Listing::Fetching(_))
    }

    /// Jumps to the word at or after `to` if its speech is already on the
    /// deck, so skipping within what's been spoken ahead is instant.
    pub fn seek(&mut self, to: Spot) -> bool {
        let Some(session) = &mut self.session else {
            return false;
        };
        let Some(index) = session.passages.iter().position(|p| {
            p.page == to.page && p.start <= to.offset && to.offset < p.start + p.text.len()
        }) else {
            return false;
        };
        let passage = &session.passages[index];
        let Stage::Queued(timed) = &passage.stage else {
            return false;
        };
        let Some(&(sample, start, end)) = timed.iter().find(|t| t.1 >= to.offset) else {
            return false;
        };
        let (serial, page) = (passage.serial, passage.page);
        let Ok(mut deck) = self.deck.lock() else {
            return false;
        };
        if !deck.clips.iter().any(|c| c.serial == serial) {
            return false;
        }
        while deck.clips.front().is_some_and(|c| c.serial != serial) {
            deck.clips.pop_front();
        }
        deck.stretch = Stretch::at(sample);
        deck.at = Some((serial, sample));
        deck.done = serial.checked_sub(1);
        drop(deck);
        session.passages.drain(..index);
        session.said = Some(Said { page, start, end });
        true
    }

    /// The word being said.
    pub fn said(&self) -> Option<Said> {
        self.session.as_ref()?.said
    }

    /// Where reading stopped last.
    pub fn resume(&self) -> Option<Spot> {
        self.resume
    }

    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    fn with_deck(&self, f: impl FnOnce(&mut Deck)) {
        if let Ok(mut deck) = self.deck.lock() {
            f(&mut deck);
        }
    }

    /// Opens the audio device if it isn't open. False, with a notice, if it can't be.
    fn open_output(&mut self) -> bool {
        if !self.audible || self.output.is_some() {
            return true;
        }
        match open_device() {
            Ok(mut sink) => {
                sink.log_on_drop(false);
                sink.mixer().add(Playhead::new(self.deck.clone()));
                self.output = Some(sink);
                true
            }
            Err(e) => {
                self.notice = Some(format!(
                    "no audio output ({e}); check your sound settings and press a again"
                ));
                false
            }
        }
    }

    /// Starts reading from `from`, dropping whatever was being read.
    pub fn start(&mut self, from: Spot) {
        if !self.open_output() {
            return;
        }
        self.generation += 1;
        self.with_deck(Deck::clear);
        self.resume = None;
        // The voice picker is likely next; have the list there when it opens.
        if matches!(self.listing, Listing::NotAsked) {
            self.prepare_voices();
        }
        self.session = Some(Session {
            paused: false,
            passages: VecDeque::new(),
            next: Some(from),
            started: false,
            said: None,
        });
    }

    /// Pauses, or carries on.
    pub fn toggle(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        session.paused = !session.paused;
        let paused = session.paused;
        self.with_deck(|deck| deck.paused = paused);
    }

    /// Where the reader is: the word being said, or else the passage coming up.
    pub fn here(&self) -> Option<Spot> {
        let session = self.session.as_ref()?;
        session
            .said
            .map(|s| Spot {
                page: s.page,
                offset: s.start,
            })
            .or_else(|| {
                session.passages.front().map(|p| Spot {
                    page: p.page,
                    offset: p.start,
                })
            })
            .or(session.next)
    }

    /// Stops reading, remembering where.
    pub fn stop(&mut self) {
        self.resume = self.here().or(self.resume);
        self.session = None;
        self.generation += 1;
        self.with_deck(Deck::clear);
        // Let go of the device, so the system isn't kept awake by silence.
        self.output = None;
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.speed = speed;
        self.with_deck(|deck| deck.speed = speed);
    }

    /// Reads in `voice` from now on, going back to the start of the word being said.
    pub fn set_voice(&mut self, voice: String) {
        self.voice = voice;
        if let Some(here) = self.here() {
            let paused = self.session.as_ref().is_some_and(|s| s.paused);
            self.start(here);
            if paused {
                self.toggle();
            }
        }
    }

    /// Gets ready for a voice to be picked: fetches the full voice list
    /// (once) and opens a connection, so the voice picked starts at once.
    pub fn prepare_voices(&mut self) {
        if !self.audible {
            return;
        }
        if matches!(self.listing, Listing::NotAsked) {
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(edge::voices());
            });
            self.listing = Listing::Fetching(rx);
        }
        edge::warm_up();
    }

    /// Every voice there is, or `None` while the list is on its way. If
    /// the list couldn't be fetched, the built-in few.
    pub fn voices(&self) -> Option<&[Voice]> {
        match self.listing {
            Listing::Fetching(_) => None,
            Listing::NotAsked | Listing::Asked => Some(&self.voices),
        }
    }

    /// Where the next passage should be cut from, and about how long it
    /// should be, while more are wanted ahead of the one playing.
    pub fn wants(&self) -> Option<(Spot, usize)> {
        let session = self.session.as_ref()?;
        if session.passages.len() >= AHEAD {
            return None;
        }
        let budget = if session.started { PASSAGE } else { 1 };
        Some((session.next?, budget))
    }

    /// Takes the passage cut where [`Narrator::wants`] said.
    pub fn give(&mut self, cut: Cut) {
        let Some(session) = &mut self.session else {
            return;
        };
        let Some(next) = session.next else { return };
        match cut {
            Cut::End => session.next = None,
            Cut::PageDone => {
                session.next = Some(Spot {
                    page: next.page + 1,
                    offset: 0,
                })
            }
            Cut::Passage { page, start, text } => {
                session.next = Some(Spot {
                    page,
                    offset: start + text.len(),
                });
                if !sayable(&text) {
                    return;
                }
                session.started = true;
                let serial = self.next_serial;
                self.next_serial += 1;
                let spoken: String = text
                    .iter()
                    .map(|&c| if c.is_whitespace() { ' ' } else { c })
                    .collect();
                session.passages.push_back(Passage {
                    serial,
                    page,
                    start,
                    text,
                    stage: Stage::Asked,
                });
                let (synth, voice, tx, generation) = (
                    self.synth.clone(),
                    self.voice.clone(),
                    self.results_tx.clone(),
                    self.generation,
                );
                thread::spawn(move || {
                    let result = synth(&spoken, &voice);
                    let _ = tx.send(Synthesized {
                        generation,
                        serial,
                        result,
                    });
                });
            }
        }
    }

    /// Takes in speech that's come back, hands it to the deck in order, and
    /// follows the playing. True when the word being said, or what the
    /// narrator is doing, changed.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        if let Listing::Fetching(rx) = &self.listing
            && let Ok(fetched) = rx.try_recv()
        {
            self.listing = Listing::Asked;
            match fetched {
                Ok(mut voices) if !voices.is_empty() => {
                    // English first, then by language and name.
                    voices.sort_by(|a, b| {
                        (!a.locale.starts_with("en"), &a.language, &a.name).cmp(&(
                            !b.locale.starts_with("en"),
                            &b.language,
                            &b.name,
                        ))
                    });
                    self.voices = voices;
                }
                Ok(_) => {}
                Err(e) => self.notice = Some(e.to_string()),
            }
            changed = true;
        }
        let generation = self.generation;
        let mut failed = None;
        while let Ok(done) = self.results_rx.try_recv() {
            let Some(session) = &mut self.session else {
                continue;
            };
            if done.generation != generation {
                continue;
            }
            let Some(passage) = session
                .passages
                .iter_mut()
                .find(|p| p.serial == done.serial)
            else {
                continue;
            };
            passage.stage = match done.result {
                Ok(speech) => Stage::Ready {
                    timed: align(&passage.text, &speech.words)
                        .into_iter()
                        .map(|(sample, s, e)| (sample, passage.start + s, passage.start + e))
                        .collect(),
                    samples: speech.samples,
                },
                Err(EdgeFailed::Silent { .. }) => Stage::Skipped,
                Err(e) => {
                    failed = Some((e, passage.page, passage.start));
                    break;
                }
            };
        }
        if let Some((e, page, offset)) = failed {
            self.stop();
            self.resume = Some(Spot { page, offset });
            self.notice = Some(e.to_string());
            return true;
        }
        let Some(session) = &mut self.session else {
            return changed;
        };

        // Speech goes on the deck in reading order.
        let mut ready = Vec::new();
        for passage in session.passages.iter_mut() {
            match std::mem::replace(&mut passage.stage, Stage::Skipped) {
                Stage::Asked => {
                    passage.stage = Stage::Asked;
                    break;
                }
                Stage::Ready { samples, timed } => {
                    ready.push(Clip {
                        serial: passage.serial,
                        samples,
                    });
                    passage.stage = Stage::Queued(timed);
                }
                other => passage.stage = other,
            }
        }
        let (at, done) = match self.deck.lock() {
            Ok(mut deck) => {
                deck.clips.extend(ready);
                (deck.at, deck.done)
            }
            Err(_) => (None, None),
        };

        // Passages played through, or passed over, are finished with.
        while let Some(front) = session.passages.front() {
            let finished = match front.stage {
                Stage::Skipped => true,
                Stage::Queued(_) => done.is_some_and(|d| d >= front.serial),
                _ => false,
            };
            if !finished {
                break;
            }
            session.passages.pop_front();
        }
        if session.passages.is_empty() && session.next.is_none() {
            self.stop();
            self.resume = None;
            self.notice = Some("finished reading".to_owned());
            return true;
        }

        let said = match (session.passages.front(), at) {
            (Some(front), Some((serial, sample))) if serial == front.serial => match &front.stage {
                Stage::Queued(timed) => {
                    let heard = timed.partition_point(|&(at, _, _)| at <= sample);
                    timed
                        .get(heard.saturating_sub(1))
                        .map(|&(_, start, end)| Said {
                            page: front.page,
                            start,
                            end,
                        })
                        .or(session.said)
                }
                _ => session.said,
            },
            _ => session.said,
        };
        if said != session.said {
            session.said = said;
            changed = true;
        }
        changed
    }
}

/// A second of speech per word, each word said at the top of its second.
#[cfg(test)]
pub fn fake_synth() -> Synth {
    Arc::new(|text: &str, _voice: &str| {
        let words: Vec<Word> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, w)| Word {
                at: i as u64 * edge::TICKS_PER_SECOND,
                text: w.trim_matches(|c: char| !c.is_alphanumeric()).to_owned(),
            })
            .collect();
        Ok(Speech {
            samples: vec![0.1; words.len() * SAMPLE_RATE as usize],
            words,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn sentences_end_at_marks_before_spaces_and_at_line_breaks() {
        let text = chars("Dr.Who said \"Hi.\" Then e.g. left\nNext one? Yes");
        let first = sentence_end(&text, 0);
        assert_eq!(
            text[..first].iter().collect::<String>(),
            "Dr.Who said \"Hi.\""
        );
        let second = sentence_end(&text, first);
        assert_eq!(text[first..second].iter().collect::<String>(), " Then e.g.");
        assert_eq!(sentence_start(&text, second + 3), second + 1);
        assert_eq!(sentence_start(&text, 5), 0);
    }

    #[test]
    fn passages_are_whole_sentences_up_to_the_budget() {
        let text = chars("  One. Two two. Three three three.\nFour.");
        assert_eq!(cut(&text, 0, 1), Some((2, 6)));
        let (start, end) = cut(&text, 6, 10).unwrap();
        assert_eq!(
            text[start..end].iter().collect::<String>(),
            "Two two. Three three three."
        );
        assert_eq!(cut(&text, text.len(), 10), None);
        assert_eq!(cut(&chars("   \n "), 0, 10), None);
    }

    #[test]
    fn a_passage_without_sentence_marks_is_cut_at_a_space() {
        let text: Vec<char> = "word ".repeat(1000).chars().collect();
        let (start, end) = cut(&text, 0, PASSAGE).unwrap();
        assert!(end - start <= LONGEST);
        assert!(text[end].is_whitespace());
    }

    #[test]
    fn spoken_words_are_found_in_order() {
        let text = chars("The cat, the hat. 3 cats");
        let word = |at: u64, text: &str| Word {
            at,
            text: text.to_owned(),
        };
        let timed = align(
            &text,
            &[
                word(0, "The"),
                word(10_000_000, "the"),
                word(15_000_000, ","),
                word(20_000_000, "zebra"),
                word(30_000_000, "cats"),
            ],
        );
        assert_eq!(timed, vec![(0, 0, 3), (24_000, 9, 12), (72_000, 20, 24)]);
    }

    #[test]
    fn voices_are_ranked_by_what_is_typed() {
        let voices = edge::built_in();
        let names = |typed: &str| -> Vec<String> {
            rank(&voices, typed)
                .iter()
                .map(|v| v.short().to_owned())
                .collect()
        };
        assert_eq!(names("sonia"), vec!["Sonia"]);
        assert_eq!(names("kingdom male"), vec!["Ryan"]);
        assert_eq!(names("").len(), voices.len());
        assert!(names("zzz").is_empty());
        // "aus" is in "Australia"; "nat" starts Natasha's name, so she leads.
        assert_eq!(names("aus nat")[0], "Natasha");
    }

    /// Talks to Microsoft's service: `cargo test -- --ignored edge_speaks`.
    #[test]
    #[ignore]
    fn edge_speaks_with_word_times() {
        let speech = edge_synth()("Hello there, reader.", edge::DEFAULT_VOICE).unwrap();
        assert!(speech.samples.len() > SAMPLE_RATE as usize / 2);
        let text = chars("Hello there, reader.");
        let timed = align(&text, &speech.words);
        assert_eq!(
            timed.iter().map(|t| (t.1, t.2)).collect::<Vec<_>>(),
            vec![(0, 5), (6, 11), (13, 19)]
        );
        assert!(timed.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(!edge::voices().unwrap().is_empty());
    }

    /// Opens the default audio device and plays silence into it.
    #[test]
    #[ignore]
    fn the_audio_device_opens() {
        let mut narrator = Narrator::new("v".into(), 1.0, Backend::Edge);
        assert!(narrator.open_output(), "{:?}", narrator.take_notice());
        thread::sleep(std::time::Duration::from_millis(200));
        assert!(narrator.deck.lock().is_ok_and(|d| d.at.is_none()));
    }

    #[test]
    fn speed_steps_and_labels() {
        assert_eq!(step_speed(1.0, true), 1.25);
        assert_eq!(step_speed(0.5, false), 0.5);
        assert_eq!(step_speed(4.0, true), 4.0);
        assert_eq!(speed_label(1.5), "1.5×");
        assert_eq!(speed_label(2.0), "2×");
        assert_eq!(speed_label(0.75), "0.75×");
    }

    fn pull(narrator: &Narrator, seconds: f32) {
        narrator.play_for(seconds);
    }

    fn settle(narrator: &mut Narrator, text: &[char]) {
        for _ in 0..200 {
            while let Some((from, budget)) = narrator.wants() {
                let cut = if from.page > 0 {
                    Cut::End
                } else {
                    match self::cut(text, from.offset, budget) {
                        Some((start, end)) => Cut::Passage {
                            page: 0,
                            start,
                            text: text[start..end].to_vec(),
                        },
                        None => Cut::PageDone,
                    }
                };
                narrator.give(cut);
            }
            narrator.tick();
            if narrator.said().is_some() {
                return;
            }
            // The audio thread plays on all the while, a block at a time.
            pull(narrator, 1.0 / SAMPLE_RATE as f32);
            thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn the_word_being_said_follows_the_audio_at_any_speed() {
        let text = chars("Alpha beta gamma. Delta epsilon.");
        let mut narrator = Narrator::new("v".into(), 1.0, Backend::Silent(fake_synth()));
        narrator.start(Spot { page: 0, offset: 0 });
        settle(&mut narrator, &text);
        assert_eq!(narrator.status(), Status::Speaking);
        assert_eq!(
            narrator.said(),
            Some(Said {
                page: 0,
                start: 0,
                end: 5
            })
        );
        pull(&narrator, 1.2);
        narrator.tick();
        assert_eq!(narrator.said().map(|s| s.start), Some(6));
        narrator.set_speed(2.0);
        pull(&narrator, 0.6);
        narrator.tick();
        assert_eq!(narrator.said().map(|s| s.start), Some(11));
    }

    #[test]
    fn reading_runs_on_into_the_next_passage_and_finishes() {
        let text = chars("One two. Three.");
        let mut narrator = Narrator::new("v".into(), 1.0, Backend::Silent(fake_synth()));
        narrator.start(Spot { page: 0, offset: 0 });
        settle(&mut narrator, &text);
        pull(&narrator, 2.1);
        settle(&mut narrator, &text);
        for _ in 0..50 {
            narrator.tick();
            if narrator.said().map(|s| s.start) == Some(9) {
                break;
            }
            pull(&narrator, 0.05);
        }
        assert_eq!(narrator.said().map(|s| s.start), Some(9));
        pull(&narrator, 1.5);
        narrator.tick();
        assert!(!narrator.active());
        assert_eq!(narrator.take_notice().as_deref(), Some("finished reading"));
    }

    #[test]
    fn pausing_holds_the_word_and_stopping_remembers_it() {
        let text = chars("Alpha beta gamma.");
        let mut narrator = Narrator::new("v".into(), 1.0, Backend::Silent(fake_synth()));
        narrator.start(Spot { page: 0, offset: 0 });
        settle(&mut narrator, &text);
        pull(&narrator, 1.2);
        narrator.toggle();
        pull(&narrator, 3.0);
        narrator.tick();
        assert_eq!(narrator.status(), Status::Paused);
        assert_eq!(narrator.said().map(|s| s.start), Some(6));
        narrator.stop();
        assert_eq!(narrator.resume(), Some(Spot { page: 0, offset: 6 }));
        assert_eq!(narrator.status(), Status::Idle);
    }

    #[test]
    fn a_failure_stops_reading_and_says_why() {
        let failing: Synth = Arc::new(|_: &str, _: &str| {
            Err(EdgeFailed::Connect {
                cause: "offline".into(),
            })
        });
        let text = chars("Alpha beta.");
        let mut narrator = Narrator::new("v".into(), 1.0, Backend::Silent(failing));
        narrator.start(Spot { page: 0, offset: 0 });
        for _ in 0..200 {
            settle(&mut narrator, &text);
            if !narrator.active() {
                break;
            }
        }
        assert!(!narrator.active());
        assert!(
            narrator
                .take_notice()
                .is_some_and(|n| n.contains("offline"))
        );
        assert_eq!(narrator.resume(), Some(Spot { page: 0, offset: 0 }));
    }
}
