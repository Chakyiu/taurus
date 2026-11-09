//! The live caption pipeline. One worker thread owns the recogniser: audio is
//! gathered from the first sound, re-read every second or so while someone is
//! speaking (the provisional line), and committed at the next pause.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::{self, Capture, CaptureKind};
use crate::engine::Settings;
use crate::engine::asr::{Engine, resegment, rms};
use crate::media::SAMPLE_RATE;
use crate::subs::is_cjk;

pub enum LiveEvent {
    /// The recogniser is loaded and listening.
    Ready,
    Level(f32),
    Partial(String),
    Final {
        start_ms: i64,
        end_ms: i64,
        text: String,
    },
    Error(String),
    /// Everything heard before Stop has been committed.
    Stopped,
}

pub struct LiveSession {
    pub events: Receiver<LiveEvent>,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    _capture: Capture,
    video: Option<capture::ScreenRecording>,
}

/// A movie of the screen to make alongside the captions.
pub struct VideoPlan {
    /// `capture::WHOLE_SCREEN` or an app name.
    pub target: String,
    pub quality: capture::VideoQuality,
    pub system_audio: bool,
    pub microphone: bool,
    pub path: PathBuf,
}

impl LiveSession {
    /// The movie cannot skip paused stretches, so a session with one never pauses.
    pub fn records_video(&self) -> bool {
        self.video.is_some()
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    /// The worker commits what it still holds, then sends `Stopped`.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `offset_ms` continues the timeline of a capture that was resumed. Everything
/// heard while not paused is also written to `record`, on the same clock as
/// the cues.
pub fn start(
    settings: &Settings,
    kind: CaptureKind,
    lang: String,
    offset_ms: i64,
    record: &Path,
    video: Option<VideoPlan>,
) -> Result<LiveSession> {
    let recording = Recording::create(record)?;
    let (audio_tx, audio_rx) = channel::<Vec<f32>>();
    let capture = match capture::start(kind, audio_tx) {
        Ok(c) => c,
        Err(e) => {
            drop(recording);
            let _ = std::fs::remove_file(record);
            return Err(e);
        }
    };
    let video = match video {
        Some(v) => match capture::record_screen(
            &v.target,
            v.quality,
            v.system_audio,
            v.microphone,
            &v.path,
        ) {
            Ok(r) => Some(r),
            Err(e) => {
                drop(capture);
                drop(recording);
                let _ = std::fs::remove_file(record);
                let _ = std::fs::remove_file(&v.path);
                return Err(e);
            }
        },
        None => None,
    };
    let (tx, events) = channel();
    let paused = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let settings = settings.clone();
    let (p, s) = (paused.clone(), stop.clone());
    std::thread::Builder::new()
        .name("taurus-live".into())
        .spawn(move || {
            let engine = match Engine::load(&settings) {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx.send(LiveEvent::Error(format!("{e:#}")));
                    let _ = tx.send(LiveEvent::Stopped);
                    return;
                }
            };
            let _ = tx.send(LiveEvent::Ready);
            Worker::new(engine, lang, offset_ms, tx, recording).run(audio_rx, &p, &s);
        })?;
    Ok(LiveSession {
        events,
        paused,
        stop,
        _capture: capture,
        video,
    })
}

const FRAME: usize = SAMPLE_RATE / 50; // 20 ms
const PRE_ROLL: usize = SAMPLE_RATE * 3 / 10;
const COMMIT_SILENCE_MS: usize = 650;
const MAX_LINE_MS: usize = 9_000;

struct Worker {
    engine: Engine,
    lang: String,
    tx: Sender<LiveEvent>,
    /// Samples heard since the capture began, which is the project's clock.
    heard: usize,
    offset_ms: i64,
    pending: Vec<f32>,
    pending_start: usize,
    speech_frames: usize,
    silence_ms: usize,
    noise: f32,
    last_partial: Instant,
    partial_len: usize,
    level_acc: Vec<f32>,
    recent: std::collections::VecDeque<f32>,
    errors: usize,
    recording: Option<Recording>,
}

impl Worker {
    fn new(
        engine: Engine,
        lang: String,
        offset_ms: i64,
        tx: Sender<LiveEvent>,
        recording: Recording,
    ) -> Self {
        Self {
            engine,
            lang,
            tx,
            heard: 0,
            offset_ms,
            pending: Vec::new(),
            pending_start: 0,
            speech_frames: 0,
            silence_ms: 0,
            noise: 0.004,
            last_partial: Instant::now(),
            partial_len: 0,
            level_acc: Vec::new(),
            recent: std::collections::VecDeque::with_capacity(PRE_ROLL + FRAME),
            errors: 0,
            recording: Some(recording),
        }
    }

    fn run(mut self, audio: Receiver<Vec<f32>>, paused: &AtomicBool, stop: &AtomicBool) {
        let mut carry: Vec<f32> = Vec::new();
        let partial_every = Duration::from_millis(self.engine.partial_every_ms());
        loop {
            if stop.load(Ordering::Relaxed) {
                self.commit();
                let _ = self.tx.send(LiveEvent::Stopped);
                return;
            }
            let block = match audio.recv_timeout(Duration::from_millis(100)) {
                Ok(b) => b,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => {
                    let _ = self
                        .tx
                        .send(LiveEvent::Error("the audio source stopped".into()));
                    self.commit();
                    let _ = self.tx.send(LiveEvent::Stopped);
                    return;
                }
            };
            // Drain whatever else is queued so a slow transcription never lags.
            let mut blocks = vec![block];
            loop {
                match audio.try_recv() {
                    Ok(b) => blocks.push(b),
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                }
            }
            if paused.load(Ordering::Relaxed) {
                if !self.pending.is_empty() {
                    self.commit();
                }
                carry.clear();
                let _ = self.tx.send(LiveEvent::Level(0.0));
                continue;
            }
            for b in blocks {
                carry.extend(b);
            }
            let whole = carry.len() / FRAME * FRAME;
            let frames: Vec<f32> = carry.drain(..whole).collect();
            for f in frames.chunks(FRAME) {
                self.frame(f);
            }
            let speaking = self.speech_frames > 3;
            let since = self.pending.len().saturating_sub(self.partial_len);
            if speaking && self.last_partial.elapsed() >= partial_every && since > SAMPLE_RATE / 2 {
                self.partial();
            }
            let ms = self.pending.len() * 1000 / SAMPLE_RATE;
            if speaking && (self.silence_ms >= COMMIT_SILENCE_MS && ms > 900 || ms >= MAX_LINE_MS) {
                self.commit();
            }
        }
    }

    fn frame(&mut self, f: &[f32]) {
        if let Some(r) = &mut self.recording {
            if let Err(e) = r.push(f) {
                let path = r.path.display().to_string();
                self.recording = None;
                let _ = self.tx.send(LiveEvent::Error(format!(
                    "stopped recording the audio to {path}: {e}; captions continue"
                )));
            }
        }
        let e = rms(f);
        self.level_acc.extend_from_slice(f);
        if self.level_acc.len() >= SAMPLE_RATE / 10 {
            let lvl = (rms(&self.level_acc) * 6.0).min(1.0);
            let _ = self.tx.send(LiveEvent::Level(lvl));
            self.level_acc.clear();
        }
        // The noise floor follows quiet stretches quickly and loud ones slowly.
        self.noise = if e < self.noise {
            self.noise * 0.9 + e * 0.1
        } else {
            self.noise * 0.999 + e * 0.001
        };
        let speech = e > (self.noise * 3.0).max(0.006);
        self.heard += f.len();
        if self.pending.is_empty() && !speech {
            self.recent.extend(f);
            while self.recent.len() > PRE_ROLL {
                self.recent.pop_front();
            }
            return;
        }
        if self.pending.is_empty() {
            // The energy gate clips the first sound; the audio just before it
            // is kept so words do not lose their first consonant.
            self.pending_start = self.heard - f.len() - self.recent.len();
            self.pending.extend(self.recent.drain(..));
        }
        self.pending.extend_from_slice(f);
        if speech {
            self.speech_frames += 1;
            self.silence_ms = 0;
        } else {
            self.silence_ms += 20;
            // A blip that never turned into speech is let go.
            if self.speech_frames <= 3 && self.silence_ms > 400 {
                self.reset();
            }
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.speech_frames = 0;
        self.silence_ms = 0;
        self.partial_len = 0;
    }

    fn partial(&mut self) {
        self.last_partial = Instant::now();
        self.partial_len = self.pending.len();
        let pcm = self.pending.clone();
        match self.engine.transcribe(&pcm, &self.lang, true, None) {
            Ok(segs) => {
                let sep = if is_cjk(&self.lang) { "" } else { " " };
                let text = segs
                    .iter()
                    .map(|s| s.text.trim())
                    .collect::<Vec<_>>()
                    .join(sep);
                let _ = self.tx.send(LiveEvent::Partial(text));
                self.errors = 0;
            }
            Err(e) => self.error(e),
        }
    }

    fn commit(&mut self) {
        if self.pending.len() < SAMPLE_RATE / 3 || self.speech_frames <= 3 {
            self.reset();
            let _ = self.tx.send(LiveEvent::Partial(String::new()));
            return;
        }
        let pcm = std::mem::take(&mut self.pending);
        let start = self.pending_start;
        let base = self.offset_ms + (start * 1000 / SAMPLE_RATE) as i64;
        let dur = (pcm.len() * 1000 / SAMPLE_RATE) as i64;
        self.reset();
        match self.engine.transcribe(&pcm, &self.lang, true, None) {
            Ok(segs) => {
                let segs = resegment(segs, is_cjk(&self.lang));
                for s in segs {
                    let text = s.text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    let _ = self.tx.send(LiveEvent::Final {
                        start_ms: base + s.start_ms.clamp(0, dur),
                        end_ms: base + s.end_ms.clamp(0, dur).max(s.start_ms + 300),
                        text,
                    });
                }
                self.errors = 0;
            }
            Err(e) => self.error(e),
        }
        let _ = self.tx.send(LiveEvent::Partial(String::new()));
    }

    fn error(&mut self, e: anyhow::Error) {
        self.errors += 1;
        // One message per run of failures, not one per second.
        if self.errors == 1 {
            let _ = self.tx.send(LiveEvent::Error(format!("{e:#}")));
        }
    }
}

/// A 16-bit mono WAV written as it is heard. The header is brought up to date
/// every few seconds, so a crash still leaves a playable file.
struct Recording {
    path: PathBuf,
    out: BufWriter<File>,
    samples: u64,
    unpatched: usize,
}

impl Recording {
    fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut r = Self {
            path: path.to_path_buf(),
            out: BufWriter::new(File::create(path)?),
            samples: 0,
            unpatched: 0,
        };
        let sr = SAMPLE_RATE as u32;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&36u32.to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&sr.to_le_bytes());
        h.extend_from_slice(&(sr * 2).to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&0u32.to_le_bytes());
        r.out.write_all(&h)?;
        Ok(r)
    }

    fn push(&mut self, pcm: &[f32]) -> std::io::Result<()> {
        for s in pcm {
            self.out
                .write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
        }
        self.samples += pcm.len() as u64;
        self.unpatched += pcm.len();
        if self.unpatched >= SAMPLE_RATE * 3 {
            self.patch()?;
        }
        Ok(())
    }

    fn patch(&mut self) -> std::io::Result<()> {
        self.unpatched = 0;
        let data = (self.samples * 2).min(u32::MAX as u64 - 36) as u32;
        self.out.flush()?;
        let f = self.out.get_mut();
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(36 + data).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&data.to_le_bytes())?;
        f.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let _ = self.patch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_header_matches_samples() {
        let path = std::env::temp_dir().join(format!("taurus-rec-{}.wav", std::process::id()));
        {
            let mut r = Recording::create(&path).unwrap();
            r.push(&vec![0.25; SAMPLE_RATE * 4]).unwrap();
            r.push(&[0.5; 160]).unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let data = (SAMPLE_RATE * 4 + 160) * 2;
        assert_eq!(bytes.len(), 44 + data);
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize,
            data
        );
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            36 + data
        );
    }
}
