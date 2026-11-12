//! A video played in the terminal: ffmpeg decodes frames, which are drawn
//! over Kitty graphics like any picture, and the sound, which plays through
//! the same audio output as reading aloud. Both are read at the video's own
//! pace (`-re`), so they keep time together.

use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use image::RgbaImage;

const USER_AGENT: &str = "Mozilla/5.0 (leaf reader)";
/// Frames past this rate cost more to send than they add in a terminal.
const FPS: u32 = 24;
/// The widest a frame is decoded; the picture is scaled to its cells anyway.
const MAX_WIDTH: u32 = 960;
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;

#[derive(Debug, thiserror::Error)]
pub enum PlayFailed {
    #[error("Playing videos here needs ffmpeg, which isn't installed (brew install ffmpeg)")]
    NoFfmpeg,
    #[error("ffmpeg couldn't start ({0}). The video is unchanged; click it again to retry.")]
    Start(String),
}

/// A video playing: the newest frame, and the processes making it.
pub struct Playback {
    frames: Receiver<RgbaImage>,
    current: Option<RgbaImage>,
    /// Counts frames shown, so each is uploaded once.
    pub frame: u64,
    ended: bool,
    picture: Child,
    sound: Option<Child>,
    /// Kept open while the sound plays; dropping it stops it.
    _output: Option<rodio::MixerDeviceSink>,
    stop: Arc<AtomicBool>,
}

/// The size to decode a `native` video at so it fills `width` pixels: even
/// numbers, as the scaler wants, and no wider than the video or `MAX_WIDTH`.
pub fn decode_size(native: (u32, u32), width: u32) -> (u32, u32) {
    let (w, h) = (native.0.max(2), native.1.max(2));
    let target = width.min(w).clamp(2, MAX_WIDTH) & !1;
    let height = ((u64::from(target) * u64::from(h) / u64::from(w)) as u32).max(2) & !1;
    (target, height)
}

fn ffmpeg(args: &[String]) -> Result<Child, PlayFailed> {
    Command::new("ffmpeg")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => PlayFailed::NoFfmpeg,
            _ => PlayFailed::Start(e.to_string()),
        })
}

fn input(url: &str) -> Vec<String> {
    ["-v", "error", "-re", "-user_agent", USER_AGENT, "-i", url]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

impl Playback {
    /// Starts playing `url`, decoded at `size` pixels.
    pub fn start(url: &str, size: (u32, u32)) -> Result<Self, PlayFailed> {
        let (width, height) = size;
        let mut args = input(url);
        args.extend(
            [
                "-an",
                "-vf",
                &format!("scale={width}:{height},fps={FPS}"),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-",
            ]
            .iter()
            .map(|s| (*s).to_owned()),
        );
        let mut picture = ffmpeg(&args)?;
        let stop = Arc::new(AtomicBool::new(false));

        let (tx, frames) = mpsc::channel();
        let Some(mut out) = picture.stdout.take() else {
            return Err(PlayFailed::Start("no output pipe".to_owned()));
        };
        let quit = stop.clone();
        std::thread::spawn(move || {
            let mut buffer = vec![0u8; width as usize * height as usize * 4];
            while !quit.load(Ordering::Relaxed) && out.read_exact(&mut buffer).is_ok() {
                let Some(frame) = RgbaImage::from_raw(width, height, buffer.clone()) else {
                    break;
                };
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });

        // Sound is extra: a video without it, or a machine without output, still plays.
        let (sound, output) = match Self::sound(url, stop.clone()) {
            Some((child, output)) => (Some(child), Some(output)),
            None => (None, None),
        };
        Ok(Playback {
            frames,
            current: None,
            frame: 0,
            ended: false,
            picture,
            sound,
            _output: output,
            stop,
        })
    }

    fn sound(url: &str, stop: Arc<AtomicBool>) -> Option<(Child, rodio::MixerDeviceSink)> {
        let mut args = input(url);
        args.extend(
            [
                "-vn",
                "-f",
                "f32le",
                "-ac",
                &CHANNELS.to_string(),
                "-ar",
                &SAMPLE_RATE.to_string(),
                "-",
            ]
            .iter()
            .map(|s| (*s).to_owned()),
        );
        let mut child = ffmpeg(&args).ok()?;
        let mut out = child.stdout.take()?;
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        std::thread::spawn(move || {
            // Whole blocks, so a sample is never split between reads; a
            // short last block at the end is a few milliseconds of silence.
            let mut bytes = [0u8; 4 * 2048];
            while !stop.load(Ordering::Relaxed) && out.read_exact(&mut bytes).is_ok() {
                let (samples, _) = bytes.as_chunks::<4>();
                let samples = samples.iter().map(|b| f32::from_le_bytes(*b)).collect();
                if tx.send(samples).is_err() {
                    break;
                }
            }
        });
        let mut output = crate::aloud::open_device().ok()?;
        output.log_on_drop(false);
        output.mixer().add(Sound {
            rx,
            block: Vec::new(),
            next: 0,
        });
        Some((child, output))
    }

    /// Takes the newest decoded frame; `true` when there's a new one to draw.
    pub fn poll(&mut self) -> bool {
        let mut fresh = false;
        loop {
            match self.frames.try_recv() {
                Ok(frame) => {
                    self.current = Some(frame);
                    fresh = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.ended = true;
                    break;
                }
            }
        }
        if fresh {
            self.frame += 1;
        }
        fresh
    }

    pub fn current(&self) -> Option<&RgbaImage> {
        self.current.as_ref()
    }

    pub fn ended(&self) -> bool {
        self.ended
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.picture.kill();
        let _ = self.picture.wait();
        if let Some(sound) = &mut self.sound {
            let _ = sound.kill();
            let _ = sound.wait();
        }
    }
}

/// The sound as it arrives from ffmpeg; silence while it's behind, and the end when it stops.
struct Sound {
    rx: Receiver<Vec<f32>>,
    block: Vec<f32>,
    next: usize,
}

impl Iterator for Sound {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.next >= self.block.len() {
            match self.rx.try_recv() {
                Ok(block) => {
                    self.block = block;
                    self.next = 0;
                }
                Err(TryRecvError::Empty) => return Some(0.0),
                Err(TryRecvError::Disconnected) => return None,
            }
            if self.block.is_empty() {
                return Some(0.0);
            }
        }
        let sample = self.block[self.next];
        self.next += 1;
        Some(sample)
    }
}

impl rodio::Source for Sound {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> rodio::ChannelCount {
        rodio::ChannelCount::new(CHANNELS).unwrap_or(rodio::ChannelCount::MIN)
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        rodio::SampleRate::new(SAMPLE_RATE).unwrap_or(rodio::SampleRate::MIN)
    }
    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_decode_even_and_never_wider_than_needed() {
        assert_eq!(decode_size((1920, 1080), 640), (640, 360));
        assert_eq!(decode_size((320, 176), 640), (320, 176));
        assert_eq!(decode_size((4000, 3000), 2000), (960, 720));
        assert_eq!(decode_size((641, 361), 641), (640, 360));
    }

    #[test]
    fn sound_is_silent_while_behind_and_ends_when_ffmpeg_does() {
        let (tx, rx) = mpsc::channel();
        let mut sound = Sound {
            rx,
            block: Vec::new(),
            next: 0,
        };
        assert_eq!(sound.next(), Some(0.0));
        tx.send(vec![0.5, -0.5]).unwrap();
        assert_eq!(sound.next(), Some(0.5));
        assert_eq!(sound.next(), Some(-0.5));
        drop(tx);
        assert_eq!(sound.next(), None);
    }
}
