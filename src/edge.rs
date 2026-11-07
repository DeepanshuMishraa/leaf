//! Microsoft Edge's online read-aloud voices: speech for a stretch of text,
//! with the time each word is spoken, and the list of voices there are.
//!
//! The protocol is the one the Edge browser speaks (as reverse engineered
//! by the `edge-tts` project): a websocket that takes SSML and sends back
//! MP3 frames plus word-boundary metadata.

use std::hash::{BuildHasher, Hasher, RandomState};
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::gec;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{Message, WebSocket};

const HOST: &str = "speech.platform.bing.com";
const BASE: &str = "speech.platform.bing.com/consumer/speech/synthesize/readaloud";
const TRUSTED_CLIENT_TOKEN: &str = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";
const CHROMIUM_FULL_VERSION: &str = "143.0.3650.75";
const CHROMIUM_MAJOR_VERSION: &str = "143";
/// Seconds from 1601-01-01 (Windows file time) to 1970-01-01.
const WINDOWS_EPOCH: u64 = 11_644_473_600;
/// Metadata times are in 100 ns ticks.
pub const TICKS_PER_SECOND: u64 = 10_000_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
const READ_TIMEOUT: Duration = Duration::from_secs(15);

pub const DEFAULT_VOICE: &str = "en-US-EmmaMultilingualNeural";

/// How far the server's clock is ahead of ours, learned from a rejected request.
static CLOCK_SKEW: AtomicI64 = AtomicI64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum EdgeFailed {
    #[error(
        "Couldn't reach Microsoft's speech service ({cause}). Check your connection; reading aloud picks up again with the next sentence."
    )]
    Connect { cause: String },
    #[error(
        "Microsoft's speech service refused the request (HTTP {status}). The voice may be gone, or the service changed; try another voice with s."
    )]
    Refused { status: u16 },
    #[error(
        "The speech service dropped the connection mid-sentence ({cause}). Press a to try again."
    )]
    Dropped { cause: String },
    #[error("The speech service sent no audio for voice {voice}. Pick another voice with s.")]
    Silent { voice: String },
    #[error(
        "The voice list from Microsoft couldn't be read ({cause}). The built-in voices are still there."
    )]
    Voices { cause: String },
}

/// A voice to read with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voice {
    /// What the service calls it, e.g. `en-US-AriaNeural`.
    pub name: String,
    /// e.g. `en-US`.
    pub locale: String,
    /// e.g. `English (United States)`.
    pub language: String,
    pub gender: String,
}

impl Voice {
    /// The name people know it by: `en-GB-SoniaNeural` is "Sonia".
    pub fn short(&self) -> &str {
        let rest = self
            .name
            .strip_prefix(&self.locale)
            .map_or(self.name.as_str(), |r| r.trim_start_matches('-'));
        rest.strip_suffix("Neural").unwrap_or(rest)
    }
}

/// A word in the speech and when it's said, in ticks from the start of the audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub at: u64,
    pub text: String,
}

/// Speech for one stretch of text.
#[derive(Debug, Clone, Default)]
pub struct Spoken {
    /// MP3, 24 kHz mono.
    pub audio: Vec<u8>,
    pub words: Vec<Word>,
}

fn unix_now() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    since.as_secs() as i64 + CLOCK_SKEW.load(Ordering::Relaxed)
}

/// The `Sec-MS-GEC` token for a moment: the Windows file time rounded down
/// to five minutes, with the client token, hashed.
fn sec_ms_gec(unix: i64) -> String {
    let seconds = unix.max(0) as u64 + WINDOWS_EPOCH;
    let ticks = (seconds - seconds % 300) * TICKS_PER_SECOND;
    gec::token(&format!("{ticks}{TRUSTED_CLIENT_TOKEN}"))
}

/// 32 random hex digits, for connection and request ids and the MUID cookie.
fn random_hex() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let half = || {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        hasher.finish()
    };
    format!("{:016X}{:016X}", half(), half())
}

/// Year, month (1–12) and day of a day counted from 1970-01-01.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Days from 1970-01-01 to a date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

/// A JavaScript-style UTC date, as the browser sends in `X-Timestamp`.
fn js_date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (year, month, day) = civil(days);
    format!(
        "{} {} {day:02} {year} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        WEEKDAYS[days.rem_euclid(7) as usize],
        MONTHS[month as usize - 1],
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// Reads an HTTP date such as `Sat, 07 Nov 2026 17:16:06 GMT`.
fn parse_http_date(date: &str) -> Option<i64> {
    let mut parts = date.split_whitespace().skip(1);
    let day: u32 = parts.next()?.parse().ok()?;
    let month = parts.next()?;
    let month = MONTHS.iter().position(|m| *m == month)? as u32 + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut time = parts.next()?.split(':').map(|n| n.parse::<i64>().ok());
    let (h, m, s) = (time.next()??, time.next()??, time.next()??);
    Some(days_from_civil(year, month, day) * 86_400 + h * 3600 + m * 60 + s)
}

/// The full name the service wants for a short voice name:
/// `en-US-AriaNeural` is `Microsoft Server Speech Text to Speech Voice (en-US, AriaNeural)`.
fn long_voice_name(short: &str) -> String {
    let mut parts = short.splitn(3, '-');
    let (Some(lang), Some(region), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
        return short.to_owned();
    };
    // `zh-CN-liaoning-XiaobeiNeural` has a region with a dash in it.
    let (region, name) = match name.split_once('-') {
        Some((sub, name)) => (format!("{region}-{sub}"), name),
        None => (region.to_owned(), name),
    };
    format!("Microsoft Server Speech Text to Speech Voice ({lang}-{region}, {name})")
}

/// Text made safe inside SSML. Control characters the service rejects
/// (common in OCR'd PDFs) become spaces.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn user_agent() -> String {
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{v}.0.0.0 Safari/537.36 Edg/{v}.0.0.0",
        v = CHROMIUM_MAJOR_VERSION
    )
}

/// The words in an `audio.metadata` message.
fn parse_metadata(body: &str) -> Vec<Word> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(items) = json.get("Metadata").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item.get("Type").and_then(|t| t.as_str()) == Some("WordBoundary"))
        .filter_map(|item| {
            let data = item.get("Data")?;
            Some(Word {
                at: data.get("Offset")?.as_u64()?,
                text: unescape(data.get("text")?.get("Text")?.as_str()?),
            })
        })
        .collect()
}

/// A text message's `Path` header and body.
fn split_text_message(message: &str) -> (Option<&str>, &str) {
    let (head, body) = message.split_once("\r\n\r\n").unwrap_or((message, ""));
    let path = head
        .split("\r\n")
        .find_map(|line| line.strip_prefix("Path:"))
        .map(str::trim);
    (path, body)
}

/// The audio in a binary message: two bytes of header length, the header, then the data.
fn binary_audio(data: &[u8]) -> Option<&[u8]> {
    let length = u16::from_be_bytes([*data.first()?, *data.get(1)?]) as usize;
    let header = std::str::from_utf8(data.get(2..2 + length)?).ok()?;
    if !header.split("\r\n").any(|l| l.trim() == "Path:audio") {
        return None;
    }
    data.get(2 + length..)
}

fn connect(
    url: &str,
) -> Result<WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>, EdgeFailed> {
    let connect_failed = |cause: String| EdgeFailed::Connect { cause };
    let mut request = url
        .into_client_request()
        .map_err(|e| connect_failed(e.to_string()))?;
    let headers = request.headers_mut();
    let fixed = [
        ("Pragma", "no-cache".to_owned()),
        ("Cache-Control", "no-cache".to_owned()),
        (
            "Origin",
            "chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold".to_owned(),
        ),
        ("User-Agent", user_agent()),
        ("Accept-Language", "en-US,en;q=0.9".to_owned()),
        ("Cookie", format!("muid={};", random_hex())),
    ];
    for (name, value) in fixed {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    let address = (HOST, 443)
        .to_socket_addrs()
        .map_err(|e| connect_failed(e.to_string()))?
        .next()
        .ok_or_else(|| connect_failed(format!("{HOST} has no address")))?;
    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .map_err(|e| connect_failed(e.to_string()))?;
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_nodelay(true);
    match tungstenite::client_tls(request, stream) {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response))) => {
            // A clock too far off makes the token stale; learn the server's time for next try.
            if response.status().as_u16() == 403
                && let Some(server) = response
                    .headers()
                    .get("Date")
                    .and_then(|d| d.to_str().ok())
                    .and_then(parse_http_date)
            {
                CLOCK_SKEW.fetch_add(server - unix_now(), Ordering::Relaxed);
            }
            Err(EdgeFailed::Refused {
                status: response.status().as_u16(),
            })
        }
        Err(e) => Err(connect_failed(e.to_string())),
    }
}

/// Speaks `text` in `voice`, at normal speed. A connection already open is
/// used when there is one, which saves the half second a new one takes.
pub fn speak(text: &str, voice: &str) -> Result<Spoken, EdgeFailed> {
    if let Some(mut socket) = take_warm() {
        // A warm connection the service has since dropped just costs a new one.
        if let Ok(spoken) = request(&mut socket, text, voice) {
            keep_warm(socket);
            return Ok(spoken);
        }
    }
    let mut socket = match open() {
        // The first refusal may have been a stale token from a skewed clock.
        Err(EdgeFailed::Refused { status: 403 }) => open()?,
        other => other?,
    };
    let spoken = request(&mut socket, text, voice)?;
    keep_warm(socket);
    Ok(spoken)
}

type Socket = WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

/// The service closes an idle connection within about half a minute; this
/// is well short of that.
const WARM_FOR: Duration = Duration::from_secs(15);
/// Connections kept open, enough for the passages asked for at once.
const SPARE: usize = 4;

/// Open connections, and when each was last used.
static WARM: Mutex<Vec<(Instant, Socket)>> = Mutex::new(Vec::new());

fn take_warm() -> Option<Socket> {
    let mut warm = WARM.lock().ok()?;
    warm.retain(|(used, _)| used.elapsed() < WARM_FOR);
    warm.pop().map(|(_, socket)| socket)
}

fn keep_warm(socket: Socket) {
    if let Ok(mut warm) = WARM.lock()
        && warm.len() < SPARE
    {
        warm.push((Instant::now(), socket));
    }
}

/// Opens a connection in the background if none is open, so the next
/// request (a new voice, say) starts sooner.
pub fn warm_up() {
    let fresh = WARM
        .lock()
        .is_ok_and(|warm| warm.iter().any(|(used, _)| used.elapsed() < WARM_FOR));
    if !fresh {
        thread::spawn(|| {
            if let Ok(socket) = open() {
                keep_warm(socket);
            }
        });
    }
}

/// A new connection to the speech service.
fn open() -> Result<Socket, EdgeFailed> {
    let url = format!(
        "wss://{BASE}/edge/v1?TrustedClientToken={TRUSTED_CLIENT_TOKEN}&ConnectionId={}&Sec-MS-GEC={}&Sec-MS-GEC-Version=2-{CHROMIUM_FULL_VERSION}",
        random_hex().to_lowercase(),
        sec_ms_gec(unix_now()),
    );
    connect(&url)
}

/// Asks for `text` in `voice` over an open connection and reads the reply.
fn request(socket: &mut Socket, text: &str, voice: &str) -> Result<Spoken, EdgeFailed> {
    let dropped = |e: tungstenite::Error| EdgeFailed::Dropped {
        cause: e.to_string(),
    };
    let now = js_date(unix_now());
    let config = format!(
        "X-Timestamp:{now}\r\nContent-Type:application/json; charset=utf-8\r\nPath:speech.config\r\n\r\n\
         {{\"context\":{{\"synthesis\":{{\"audio\":{{\"metadataoptions\":{{\"sentenceBoundaryEnabled\":\"false\",\"wordBoundaryEnabled\":\"true\"}},\"outputFormat\":\"audio-24khz-48kbitrate-mono-mp3\"}}}}}}}}\r\n"
    );
    socket.send(Message::text(config)).map_err(dropped)?;
    let ssml = format!(
        "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'>\
         <voice name='{}'><prosody pitch='+0Hz' rate='+0%' volume='+0%'>{}</prosody></voice></speak>",
        long_voice_name(voice),
        escape(text)
    );
    let request = format!(
        "X-RequestId:{}\r\nContent-Type:application/ssml+xml\r\nX-Timestamp:{now}Z\r\nPath:ssml\r\n\r\n{ssml}",
        random_hex().to_lowercase()
    );
    socket.send(Message::text(request)).map_err(dropped)?;

    let mut spoken = Spoken::default();
    loop {
        match socket.read() {
            Ok(Message::Text(message)) => match split_text_message(message.as_str()) {
                (Some("audio.metadata"), body) => spoken.words.extend(parse_metadata(body)),
                (Some("turn.end"), _) => break,
                _ => {}
            },
            Ok(Message::Binary(data)) => {
                if let Some(audio) = binary_audio(&data) {
                    spoken.audio.extend_from_slice(audio);
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                return Err(EdgeFailed::Dropped {
                    cause: "no reply for 15 seconds".to_owned(),
                });
            }
            Err(e) => return Err(dropped(e)),
        }
    }
    if spoken.audio.is_empty() {
        return Err(EdgeFailed::Silent {
            voice: voice.to_owned(),
        });
    }
    Ok(spoken)
}

/// Every voice the service offers.
pub fn voices() -> Result<Vec<Voice>, EdgeFailed> {
    let failed = |cause: String| EdgeFailed::Voices { cause };
    let url = format!(
        "https://{BASE}/voices/list?trustedclienttoken={TRUSTED_CLIENT_TOKEN}&Sec-MS-GEC={}&Sec-MS-GEC-Version=2-{CHROMIUM_FULL_VERSION}",
        sec_ms_gec(unix_now())
    );
    let body = ureq::get(&url)
        .header("User-Agent", user_agent())
        .header("Cookie", format!("muid={};", random_hex()))
        .call()
        .map_err(|e| failed(e.to_string()))?
        .into_body()
        .read_to_string()
        .map_err(|e| failed(e.to_string()))?;
    parse_voices(&body).ok_or_else(|| failed("not the expected JSON".to_owned()))
}

fn parse_voices(body: &str) -> Option<Vec<Voice>> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let field = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|f| f.as_str())
            .map(str::to_owned)
            .unwrap_or_default()
    };
    Some(
        json.as_array()?
            .iter()
            .map(|v| Voice {
                name: field(v, "ShortName"),
                locale: field(v, "Locale"),
                // "Microsoft Aria Online (Natural) - English (United States)"
                language: field(v, "FriendlyName")
                    .rsplit_once(" - ")
                    .map(|(_, language)| language.to_owned())
                    .unwrap_or_default(),
                gender: field(v, "Gender"),
            })
            .filter(|v| !v.name.is_empty())
            .collect(),
    )
}

/// Voices to pick from before (or without) the full list arriving.
pub fn built_in() -> Vec<Voice> {
    let (us, gb, au, india) = (
        "English (United States)",
        "English (United Kingdom)",
        "English (Australia)",
        "English (India)",
    );
    [
        ("en-US-EmmaMultilingualNeural", us, "Female"),
        ("en-US-AndrewMultilingualNeural", us, "Male"),
        ("en-US-AvaMultilingualNeural", us, "Female"),
        ("en-US-BrianMultilingualNeural", us, "Male"),
        ("en-US-AriaNeural", us, "Female"),
        ("en-US-GuyNeural", us, "Male"),
        ("en-US-JennyNeural", us, "Female"),
        ("en-US-ChristopherNeural", us, "Male"),
        ("en-GB-SoniaNeural", gb, "Female"),
        ("en-GB-RyanNeural", gb, "Male"),
        ("en-AU-NatashaNeural", au, "Female"),
        ("en-AU-WilliamMultilingualNeural", au, "Male"),
        ("en-IN-NeerjaNeural", india, "Female"),
        ("en-IN-PrabhatNeural", india, "Male"),
    ]
    .into_iter()
    .map(|(name, language, gender)| Voice {
        locale: name.splitn(3, '-').take(2).collect::<Vec<_>>().join("-"),
        name: name.to_owned(),
        language: language.to_owned(),
        gender: gender.to_owned(),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_the_hash_of_the_five_minute_windows_time() {
        // 2024-01-01T00:02:30Z rounds down to 00:00:00.
        let a = sec_ms_gec(1_704_067_350);
        let b = sec_ms_gec(1_704_067_200);
        assert_eq!(a, b);
        assert_ne!(a, sec_ms_gec(1_704_067_500));
        // Values from edge-tts 7.3.1's generate_sec_ms_gec_v2.
        assert_eq!(
            a,
            "9586E5FBB4BD2CFF-DE636BA8369C7431ED826BDEEB00ECA1CC52826A6BB736DCB150A2AF37DFC498"
        );
        assert_eq!(
            sec_ms_gec(1_800_000_000),
            "99E54519B490BE5C-E162DB0977FBDCB9B86DD8148BF1BCDC3C0303176CE39852A42B415935C20654"
        );
    }

    #[test]
    fn short_voice_names_are_expanded() {
        assert_eq!(
            long_voice_name("en-US-AriaNeural"),
            "Microsoft Server Speech Text to Speech Voice (en-US, AriaNeural)"
        );
        assert_eq!(
            long_voice_name("zh-CN-liaoning-XiaobeiNeural"),
            "Microsoft Server Speech Text to Speech Voice (zh-CN-liaoning, XiaobeiNeural)"
        );
    }

    #[test]
    fn dates_go_both_ways() {
        let unix = parse_http_date("Sat, 07 Nov 2026 17:16:06 GMT").unwrap();
        assert_eq!(
            js_date(unix),
            "Sat Nov 07 2026 17:16:06 GMT+0000 (Coordinated Universal Time)"
        );
        assert_eq!(
            js_date(0),
            "Thu Jan 01 1970 00:00:00 GMT+0000 (Coordinated Universal Time)"
        );
    }

    #[test]
    fn text_is_escaped_for_ssml() {
        assert_eq!(escape("a < b & c\u{b}d"), "a &lt; b &amp; c d");
        assert_eq!(unescape("a &lt; b &amp;amp;"), "a < b &amp;");
    }

    #[test]
    fn word_boundaries_are_read_from_metadata() {
        let message = "X-RequestId:1\r\nContent-Type:application/json\r\nPath:audio.metadata\r\n\r\n\
            {\"Metadata\":[{\"Type\":\"WordBoundary\",\"Data\":{\"Offset\":1000000,\"Duration\":3000000,\
            \"text\":{\"Text\":\"Hello\",\"Length\":5,\"BoundaryType\":\"WordBoundary\"}}}]}";
        let (path, body) = split_text_message(message);
        assert_eq!(path, Some("audio.metadata"));
        assert_eq!(
            parse_metadata(body),
            vec![Word {
                at: 1_000_000,
                text: "Hello".to_owned()
            }]
        );
    }

    #[test]
    fn audio_comes_after_the_binary_header() {
        let header = b"X-RequestId:1\r\nContent-Type:audio/mpeg\r\nPath:audio\r\n";
        let mut data = (header.len() as u16).to_be_bytes().to_vec();
        data.extend_from_slice(header);
        data.extend_from_slice(&[1, 2, 3]);
        assert_eq!(binary_audio(&data), Some(&[1u8, 2, 3][..]));
        assert_eq!(binary_audio(&[0]), None);
    }

    #[test]
    fn a_voice_goes_by_its_short_name() {
        let voice = &built_in()[8];
        assert_eq!(voice.locale, "en-GB");
        assert_eq!(voice.short(), "Sonia");
    }

    #[test]
    fn the_voice_list_is_parsed() {
        let body = r#"[{"ShortName":"en-US-AriaNeural","Locale":"en-US","Gender":"Female","FriendlyName":"Microsoft Aria Online (Natural) - English (United States)"},{"Locale":"x"}]"#;
        let voices = parse_voices(body).unwrap();
        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].short(), "Aria");
        assert_eq!(voices[0].language, "English (United States)");
    }
}
