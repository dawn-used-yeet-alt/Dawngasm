//! Playback: symphonia decoding into a bounded buffer, rodio for output.
//!
//! rodio's own decoder is deliberately unused: it pins symphonia 0.5, which
//! cannot read hotaudio's fragmented MP4. Instead this module owns the decode
//! loop and hands rodio a [`Source`] of its own design.
//!
//! # Why a worker thread
//!
//! Resolving a post is network I/O (three requests for audiochan, a signed
//! handshake for hotaudio) and decoding is CPU work, so neither belongs on the
//! thread that draws the terminal. One long-lived worker thread does both:
//!
//! 1. resolve the post to a seekable [`MediaSource`];
//! 2. probe it and publish the format (rate, channels, duration);
//! 3. decode packets into a bounded ring buffer.
//!
//! The rodio [`Source`] the UI thread attaches simply drains that buffer. It is
//! created only after step 2, so its fixed channel count and sample rate are
//! real, and control (seek, stop) is passed by atomics rather than by locking
//! the decoder — the worker is the only thing that ever touches the reader.

pub mod hotaudio;
pub mod http_source;

use std::collections::VecDeque;
use std::io;
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rodio::Source;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use crate::models::Post;
use crate::services::extractors;

/// The desktop app caps the volume at 200%; keep the same limit.
pub const MAX_VOLUME: f32 = 2.0;

/// Seek-step sizes, matching the desktop player.
pub const NUDGE_SMALL_MS: i64 = 5_000;
pub const NUDGE_LARGE_MS: i64 = 10_000;

/// Decode-ahead, in seconds of stereo audio. Two seconds is roughly 700 KB of
/// f32 samples — enough slack that a slow range request does not starve the
/// output, small enough to stay well clear of a memory hog.
const BUFFER_SECONDS: usize = 2;

/// What the UI shows for the player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerStatus {
    Idle,
    Loading,
    Playing,
    Paused,
}

/// Something the worker finished that the UI has to act on.
#[derive(Debug)]
pub enum AudioEvent {
    /// The track's format was resolved and its source attached to the player,
    /// so playback has genuinely begun.
    Started,
    /// The track reached its end.
    Ended,
    /// Playback failed; the message is user-facing.
    Failed(String),
}

/// The decode-ahead buffer plus every cross-thread control flag.
struct Shared {
    buffer: Mutex<Buffer>,
    /// Pending seek target in milliseconds, or `-1` for none.
    seek_ms: AtomicI64,
    /// Set to abort the worker's current track.
    stop: AtomicBool,
    /// Set once the format is known and decoding has begun.
    ready: AtomicBool,
    /// Set when the worker could not play the track.
    error: Mutex<Option<String>>,
    channels: AtomicU32,
    sample_rate: AtomicU32,
    duration_ms: AtomicI64,
    /// Millisecond position the currently buffered samples start at.
    base_ms: AtomicI64,
    /// Interleaved samples handed to rodio since the last seek.
    yielded: AtomicU64,
    /// Set when the source has drained the finished track.
    eof: AtomicBool,
}

struct Buffer {
    samples: VecDeque<f32>,
    /// The worker decoded the whole track into this buffer.
    finished: bool,
}

impl Shared {
    const fn new() -> Self {
        Self {
            buffer: Mutex::new(Buffer {
                samples: VecDeque::new(),
                finished: false,
            }),
            seek_ms: AtomicI64::new(-1),
            stop: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            error: Mutex::new(None),
            channels: AtomicU32::new(0),
            sample_rate: AtomicU32::new(0),
            duration_ms: AtomicI64::new(0),
            base_ms: AtomicI64::new(0),
            yielded: AtomicU64::new(0),
            eof: AtomicBool::new(false),
        }
    }

    fn fail(&self, message: String) {
        *self.error.lock().unwrap() = Some(message);
        self.buffer.lock().unwrap().finished = true;
    }

    fn finish(&self) {
        self.buffer.lock().unwrap().finished = true;
    }

    /// Playhead in milliseconds: where the last seek landed plus the audio since.
    fn position_ms(&self) -> i64 {
        let channels = u64::from(self.channels.load(Ordering::Relaxed).max(1));
        let rate = u64::from(self.sample_rate.load(Ordering::Relaxed).max(1));
        let frames = self.yielded.load(Ordering::Relaxed) / channels;
        self.base_ms.load(Ordering::Relaxed) + (frames as i64 * 1000) / rate as i64
    }
}

/// The rodio source for the track currently loaded into a [`Shared`].
///
/// It is a thin, non-blocking consumer: it never performs I/O, never locks the
/// decoder, and yields silence while the worker catches up so the output thread
/// is never stalled by a slow network.
struct TuiSource {
    shared: Arc<Shared>,
    channels: NonZeroU16,
    sample_rate: NonZeroU32,
    duration: Option<Duration>,
    ended: bool,
}

impl Iterator for TuiSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.ended {
            return None;
        }
        let mut buffer = self.shared.buffer.lock().unwrap();
        if let Some(sample) = buffer.samples.pop_front() {
            drop(buffer);
            self.shared.yielded.fetch_add(1, Ordering::Relaxed);
            return Some(sample);
        }
        if buffer.finished {
            drop(buffer);
            self.ended = true;
            self.shared.eof.store(true, Ordering::SeqCst);
            return None;
        }
        drop(buffer);
        // Underrun: silence keeps the output fed until the worker refills.
        Some(0.0)
    }
}

impl Source for TuiSource {
    fn current_span_len(&self) -> Option<usize> {
        // Unknown and may change as packets arrive; rodio will re-query.
        None
    }

    fn channels(&self) -> NonZeroU16 {
        self.channels
    }

    fn sample_rate(&self) -> NonZeroU32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.duration
    }
}

enum WorkerMessage {
    Play { shared: Arc<Shared>, post: Post },
}

/// A handle to the audio output and the decode worker behind it.
pub struct AudioEngine {
    /// Keeps the cpal stream alive for as long as the engine exists.
    _sink: rodio::MixerDeviceSink,
    player: rodio::Player,
    worker: Sender<WorkerMessage>,
    /// The track being resolved or played, if any.
    current: Mutex<Option<Arc<Shared>>>,
    /// Whether the current track's source has been attached to the player.
    attached: bool,
    volume: f32,
    /// Pause as soon as the track's source is attached, used by session
    /// restore. Pausing before the source exists would be undone by the
    /// `play()` the attach performs, so the intent is held here instead.
    start_paused: bool,
}

impl AudioEngine {
    /// Opens the default output device and starts the decode worker.
    pub fn new() -> Result<Self, String> {
        let mut sink = rodio::DeviceSinkBuilder::open_default_sink().map_err(|e| e.to_string())?;
        // The sink outlives the alternate screen, where its drop notice would
        // scribble over the restored shell prompt.
        sink.log_on_drop(false);
        let player = rodio::Player::connect_new(sink.mixer());

        let (worker, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("audiogasm-decode".into())
            .spawn(move || worker_loop(receiver))
            .map_err(|e| format!("could not start the audio worker: {e}"))?;

        Ok(Self {
            _sink: sink,
            player,
            worker,
            current: Mutex::new(None),
            attached: false,
            volume: 1.0,
            start_paused: false,
        })
    }

    /// Arranges for the next track to begin paused, for session restore.
    pub const fn set_start_paused(&mut self, paused: bool) {
        self.start_paused = paused;
    }

    pub const fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, MAX_VOLUME);
        self.player.set_volume(self.volume);
    }

    pub fn status(&self) -> PlayerStatus {
        let current = self.current.lock().unwrap().clone();
        let Some(shared) = current else {
            return PlayerStatus::Idle;
        };
        if !self.attached || !shared.ready.load(Ordering::SeqCst) {
            return PlayerStatus::Loading;
        }
        if self.player.is_paused() {
            PlayerStatus::Paused
        } else {
            PlayerStatus::Playing
        }
    }

    pub fn position_ms(&self) -> i64 {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |shared| shared.position_ms())
    }

    pub fn duration_ms(&self) -> i64 {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |shared| shared.duration_ms.load(Ordering::Relaxed))
    }

    pub fn is_seekable(&self) -> bool {
        self.duration_ms() > 0
    }

    /// Begins resolving and playing `post`. Replaces whatever was playing.
    pub fn play(&mut self, post: Post) {
        self.stop_current();
        self.start_paused = false;
        let shared = Arc::new(Shared::new());
        *self.current.lock().unwrap() = Some(Arc::clone(&shared));
        let _ = self.worker.send(WorkerMessage::Play { shared, post });
    }

    /// Stops playback and forgets the current track.
    pub fn stop(&mut self) {
        self.stop_current();
    }

    fn stop_current(&mut self) {
        let previous = self.current.lock().unwrap().take();
        if let Some(shared) = previous {
            shared.stop.store(true, Ordering::SeqCst);
        }
        // `clear` skips every queued source and leaves the player paused; the
        // next attach calls `play` again.
        self.player.play();
        self.player.clear();
        self.attached = false;
    }

    pub fn pause(&self) {
        self.player.pause();
    }

    pub fn resume(&self) {
        self.player.play();
    }

    pub fn toggle_playback(&mut self) {
        match self.status() {
            // A track still resolving cannot be paused; ignore the key rather
            // than pause an empty player.
            PlayerStatus::Loading | PlayerStatus::Idle => {}
            PlayerStatus::Playing => self.player.pause(),
            PlayerStatus::Paused => self.player.play(),
        }
    }

    /// Seeks to `target_ms`, clamped to the known duration.
    pub fn seek_ms(&mut self, target_ms: i64) {
        let current = self.current.lock().unwrap().clone();
        let Some(shared) = current else {
            return;
        };
        let duration = shared.duration_ms.load(Ordering::Relaxed);
        let target = target_ms.max(0);
        let target = if duration > 0 {
            target.min(duration)
        } else {
            target
        };
        shared.buffer.lock().unwrap().samples.clear();
        shared.yielded.store(0, Ordering::SeqCst);
        shared.base_ms.store(target, Ordering::SeqCst);
        shared.eof.store(false, Ordering::SeqCst);
        shared.seek_ms.store(target, Ordering::SeqCst);
    }

    /// Moves the playhead by `delta_ms`, clamped to the track.
    pub fn nudge(&mut self, delta_ms: i64) {
        if self.duration_ms() <= 0 {
            return;
        }
        self.seek_ms(self.position_ms() + delta_ms);
    }

    /// Drains finished work. Called once per UI tick.
    pub fn poll(&mut self) -> Vec<AudioEvent> {
        let mut events = Vec::new();
        let current = self.current.lock().unwrap().clone();
        let Some(shared) = current else {
            return events;
        };

        if let Some(error) = shared.error.lock().unwrap().take() {
            self.stop_current();
            events.push(AudioEvent::Failed(error));
            return events;
        }

        if !self.attached && shared.ready.load(Ordering::SeqCst) {
            let channels = NonZeroU16::new(shared.channels.load(Ordering::Relaxed) as u16)
                .unwrap_or(NonZeroU16::new(2).expect("2 is non-zero"));
            let rate = NonZeroU32::new(shared.sample_rate.load(Ordering::Relaxed))
                .unwrap_or(NonZeroU32::new(44_100).expect("44100 is non-zero"));
            let duration_ms = shared.duration_ms.load(Ordering::Relaxed);
            let duration = (duration_ms > 0).then(|| Duration::from_millis(duration_ms as u64));
            self.player.append(TuiSource {
                shared: Arc::clone(&shared),
                channels,
                sample_rate: rate,
                duration,
                ended: false,
            });
            // `append` on a cleared player starts it playing; a restored track
            // that was paused has to be paused again here, since pausing before
            // the source existed had nothing to act on.
            if self.start_paused {
                self.player.pause();
            } else {
                self.player.play();
            }
            self.attached = true;
            events.push(AudioEvent::Started);
        }

        if shared.eof.swap(false, Ordering::SeqCst) {
            // The source has drained; let go of it so the player is genuinely
            // idle rather than paused on a finished track.
            self.stop_current();
            *self.current.lock().unwrap() = None;
            events.push(AudioEvent::Ended);
        }

        events
    }
}

fn worker_loop(receiver: Receiver<WorkerMessage>) {
    let extractors = extractors::Extractors::new();
    while let Ok(message) = receiver.recv() {
        match message {
            WorkerMessage::Play { shared, post } => {
                if shared.stop.load(Ordering::SeqCst) {
                    continue;
                }
                match extractors.resolve_media(&post) {
                    Ok(resolved) => {
                        if let Some(hint) = resolved.duration_hint_ms {
                            if hint > 0 {
                                shared.duration_ms.store(hint, Ordering::Relaxed);
                            }
                        }
                        if shared.stop.load(Ordering::SeqCst) {
                            continue;
                        }
                        if let Err(error) = decode(&shared, resolved.media) {
                            // A stop mid-decode is not a failure.
                            if !shared.stop.load(Ordering::SeqCst) {
                                shared.fail(error);
                            }
                        }
                    }
                    Err(error) => {
                        if !shared.stop.load(Ordering::SeqCst) {
                            shared.fail(error.to_string());
                        }
                    }
                }
            }
        }
    }
}

/// Probes `media` and decodes it into `shared` until the track ends or the user
/// moves on. Returns an error message on unrecoverable failures.
fn decode(shared: &Shared, media: Box<dyn MediaSource>) -> Result<(), String> {
    let mss = MediaSourceStream::new(media, MediaSourceStreamOptions::default());
    let mut format = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| format!("unsupported or corrupt audio: {error}"))?;

    // Scoped so the `Track` borrow ends before `format.next_packet()` needs it.
    let (track_id, sample_rate, channels, duration_ms, mut decoder) = {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| "the file has no audio track".to_string())?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or_else(|| "the audio track has no codec parameters".to_string())?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|error| format!("unsupported audio codec: {error}"))?;
        let channels = params
            .channels
            .as_ref()
            .map_or(0, symphonia::core::audio::Channels::count);
        let channels = channels.clamp(1, u16::MAX as usize) as u16;
        let sample_rate = params.sample_rate.unwrap_or(44_100).max(1);
        (
            track.id,
            sample_rate,
            u32::from(channels),
            track_duration_ms(track),
            decoder,
        )
    };

    shared.channels.store(channels, Ordering::Relaxed);
    shared.sample_rate.store(sample_rate, Ordering::Relaxed);
    if let Some(duration_ms) = duration_ms {
        shared.duration_ms.store(duration_ms, Ordering::Relaxed);
    }
    shared.ready.store(true, Ordering::SeqCst);

    let capacity = (sample_rate as usize * channels as usize * BUFFER_SECONDS).max(16_384);
    let mut scratch: Vec<f32> = Vec::new();

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return Ok(());
        }

        if let Some(target) = take_seek(shared) {
            seek_reader(&mut *format, &mut *decoder, target)?;
            shared.base_ms.store(target, Ordering::SeqCst);
            continue;
        }

        if shared.buffer.lock().unwrap().samples.len() >= capacity {
            // Let the output thread drain a little before decoding more.
            thread::sleep(Duration::from_millis(4));
            continue;
        }

        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => {
                shared.finish();
                return Ok(());
            }
            Err(error) => {
                if is_eof(&error) {
                    shared.finish();
                    return Ok(());
                }
                return Err(format!("read error: {error}"));
            }
        };
        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio) => {
                let count = audio.samples_interleaved();
                if count == 0 {
                    continue;
                }
                scratch.resize(count, 0.0);
                audio.copy_to_slice_interleaved(&mut scratch);
                shared
                    .buffer
                    .lock()
                    .unwrap()
                    .samples
                    .extend(scratch.iter().copied());
            }
            // A single undecodeable packet is skipped, not fatal.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                decoder = rebuild_decoder(&mut *format).ok_or_else(|| {
                    "the audio stream changed and could not be resumed".to_string()
                })?;
            }
            Err(error) => return Err(format!("decode error: {error}")),
        }
    }
}

fn take_seek(shared: &Shared) -> Option<i64> {
    let target = shared.seek_ms.load(Ordering::SeqCst);
    if target >= 0 {
        shared.seek_ms.store(-1, Ordering::SeqCst);
        Some(target)
    } else {
        None
    }
}

fn seek_reader(
    format: &mut dyn FormatReader,
    decoder: &mut dyn AudioDecoder,
    target_ms: i64,
) -> Result<(), String> {
    let seconds = target_ms.div_euclid(1_000);
    let nanos = (target_ms.rem_euclid(1_000) as u32) * 1_000_000;
    let time = Time::try_new(seconds, nanos).ok_or_else(|| "invalid seek position".to_string())?;
    format
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: None,
            },
        )
        .map_err(|error| format!("seek failed: {error}"))?;
    decoder.reset();
    Ok(())
}

fn rebuild_decoder(format: &mut dyn FormatReader) -> Option<Box<dyn AudioDecoder>> {
    let params = format
        .default_track(TrackType::Audio)?
        .codec_params
        .as_ref()?
        .audio()?;
    symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .ok()
}

fn is_eof(error: &SymphoniaError) -> bool {
    matches!(error, SymphoniaError::IoError(error) if error.kind() == io::ErrorKind::UnexpectedEof)
}

/// The track's duration in milliseconds, from the container duration when it
/// has a timebase, else from the frame count.
fn track_duration_ms(track: &Track) -> Option<i64> {
    if let (Some(duration), Some(time_base)) = (track.duration, track.time_base) {
        if let Some(time) = time_base.calc_duration(duration) {
            return i64::try_from(time.as_millis()).ok();
        }
    }
    let rate = track.codec_params.as_ref()?.audio()?.sample_rate?;
    let frames = track.num_frames?;
    Some((i128::from(frames) * 1000 / i128::from(rate.max(1))) as i64)
}

/// Formats a millisecond timestamp as `m:ss` or `h:mm:ss`.
#[must_use]
pub fn format_time(ms: i64) -> String {
    let total = (ms.max(0) as u64) / 1000;
    let seconds = total % 60;
    let minutes = (total / 60) % 60;
    let hours = total / 3600;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps_with_and_without_hours() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(65_000), "1:05");
        assert_eq!(format_time(3_600_000), "1:00:00");
        assert_eq!(format_time(3_661_000), "1:01:01");
        assert_eq!(format_time(-5), "0:00");
    }

    #[test]
    fn position_advances_with_yielded_samples() {
        let shared = Shared::new();
        shared.channels.store(2, Ordering::Relaxed);
        shared.sample_rate.store(48_000, Ordering::Relaxed);
        shared.base_ms.store(1_000, Ordering::Relaxed);
        // 48 000 stereo frames is one second.
        shared.yielded.store(96_000, Ordering::Relaxed);
        assert_eq!(shared.position_ms(), 2_000);
    }

    #[test]
    fn seek_is_taken_once() {
        let shared = Shared::new();
        assert_eq!(take_seek(&shared), None);
        shared.seek_ms.store(5_000, Ordering::SeqCst);
        assert_eq!(take_seek(&shared), Some(5_000));
        assert_eq!(take_seek(&shared), None);
    }
}
