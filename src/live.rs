//! The live caption pipeline. Audio capture stays responsive while one
//! serialized inference worker re-reads provisional lines and commits finals.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{
    Receiver, Sender, SyncSender, TryRecvError, TrySendError, channel, sync_channel,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::{self, Capture, CaptureKind};
use crate::engine::Settings;
use crate::engine::asr::{AsrSeg, Engine, resegment, rms};
use crate::media::SAMPLE_RATE;
use crate::store;
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
    let (audio_tx, audio_rx) = capture::audio_queue(3 * SAMPLE_RATE);
    let capture = match capture::start(kind, audio_tx) {
        Ok(c) => c,
        Err(e) => {
            drop(recording);
            store::discard_owned(record);
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
                store::discard_owned(record);
                store::discard_owned(&v.path);
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
            let worker_tx = tx.clone();
            match Worker::new(engine, lang, offset_ms, tx, recording) {
                Ok(worker) => worker.run(audio_rx, &p, &s),
                Err(e) => {
                    let _ = worker_tx.send(LiveEvent::Error(format!("{e:#}")));
                    let _ = worker_tx.send(LiveEvent::Stopped);
                }
            }
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
/// Stands in for a stretch the recogniser could not read, so the words are
/// visibly missing instead of silently dropped.
pub const MISSED_CUE: &str = "[audio not transcribed]";

struct Worker {
    jobs: Option<SyncSender<InferenceJob>>,
    inference: Option<std::thread::JoinHandle<()>>,
    final_done: Receiver<()>,
    pending_finals: usize,
    partial_every: Duration,
    generation: Arc<AtomicUsize>,
    publication: Arc<Mutex<()>>,
    partial_busy: Arc<AtomicBool>,
    audio_drop_reported: bool,
    inference_error_reported: bool,
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
    recording: Option<Recording>,
}

impl Worker {
    fn new(
        engine: Engine,
        lang: String,
        offset_ms: i64,
        tx: Sender<LiveEvent>,
        recording: Recording,
    ) -> Result<Self> {
        let partial_every = Duration::from_millis(engine.partial_every_ms());
        let (jobs, job_rx) = sync_channel(1);
        let (done_tx, final_done) = channel();
        let generation = Arc::new(AtomicUsize::new(0));
        let publication = Arc::new(Mutex::new(()));
        let partial_busy = Arc::new(AtomicBool::new(false));
        let worker_generation = generation.clone();
        let worker_publication = publication.clone();
        let worker_busy = partial_busy.clone();
        let worker_tx = tx.clone();
        let inference = std::thread::Builder::new()
            .name("taurus-live-inference".into())
            .spawn(move || {
                inference_loop(
                    engine,
                    worker_generation,
                    worker_publication,
                    worker_busy,
                    job_rx,
                    done_tx,
                    worker_tx,
                );
            })?;
        Ok(Self {
            jobs: Some(jobs),
            inference: Some(inference),
            final_done,
            pending_finals: 0,
            partial_every,
            generation,
            publication,
            partial_busy,
            audio_drop_reported: false,
            inference_error_reported: false,
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
            recording: Some(recording),
        })
    }

    fn run(mut self, audio: capture::AudioReceiver, paused: &AtomicBool, stop: &AtomicBool) {
        let mut carry: Vec<f32> = Vec::new();
        loop {
            self.drain_final_done();
            self.report_audio_drops(&audio);
            if stop.load(Ordering::Relaxed) {
                self.commit();
                self.finish_finals();
                self.report_audio_drops(&audio);
                self.shutdown_inference();
                let _ = self.tx.send(LiveEvent::Stopped);
                return;
            }
            let block = match audio.recv_timeout(Duration::from_millis(100)) {
                Ok(b) => b,
                Err(capture::AudioRecvError::Timeout) => continue,
                Err(capture::AudioRecvError::Disconnected) => {
                    let _ = self
                        .tx
                        .send(LiveEvent::Error("the audio source stopped".into()));
                    self.commit();
                    self.finish_finals();
                    self.report_audio_drops(&audio);
                    self.shutdown_inference();
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
            if speaking
                && self.last_partial.elapsed() >= self.partial_every
                && since > SAMPLE_RATE / 2
            {
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
        let _publication = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.pending.clear();
        self.speech_frames = 0;
        self.silence_ms = 0;
        self.partial_len = 0;
    }

    fn partial(&mut self) {
        if !acquire_partial(&self.partial_busy) {
            return;
        }
        self.last_partial = Instant::now();
        let expected_generation = self.generation.load(Ordering::Relaxed);
        let job = InferenceJob::Partial {
            pcm: self.pending.clone(),
            lang: self.lang.clone(),
            expected_generation,
        };
        match self
            .jobs
            .as_ref()
            .expect("inference worker is live")
            .try_send(job)
        {
            Ok(()) => self.partial_len = self.pending.len(),
            Err(TrySendError::Full(_)) => {
                self.partial_busy.store(false, Ordering::Release);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.partial_busy.store(false, Ordering::Release);
                if !self.inference_error_reported {
                    self.inference_error_reported = true;
                    let _ = self.tx.send(LiveEvent::Error(
                        "Live transcription worker stopped unexpectedly.".into(),
                    ));
                }
            }
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
        let job = InferenceJob::Final {
            pcm,
            lang: self.lang.clone(),
            base,
            dur,
            cjk: is_cjk(&self.lang),
        };
        match self
            .jobs
            .as_ref()
            .expect("inference worker is live")
            .send(job)
        {
            Ok(()) => self.pending_finals += 1,
            Err(e) => {
                let InferenceJob::Final { base, dur, .. } = e.0 else {
                    unreachable!("only final jobs are sent synchronously")
                };
                self.emit_missed_final(base, dur);
            }
        }
        let _ = self.tx.send(LiveEvent::Partial(String::new()));
    }

    fn report_audio_drops(&mut self, audio: &capture::AudioReceiver) {
        if audio.take_dropped() && !self.audio_drop_reported {
            self.audio_drop_reported = true;
            let _ = self.tx.send(LiveEvent::Error(
                "Live audio was dropped because the capture queue filled.".into(),
            ));
        }
    }

    fn emit_missed_final(&self, base: i64, dur: i64) {
        let _ = self.tx.send(LiveEvent::Final {
            start_ms: base,
            end_ms: base + dur.max(300),
            text: MISSED_CUE.to_string(),
        });
        let _ = self.tx.send(LiveEvent::Error(
            "The transcription worker stopped before this audio could be decoded.".into(),
        ));
    }

    fn drain_final_done(&mut self) {
        while self.final_done.try_recv().is_ok() {
            self.pending_finals = self.pending_finals.saturating_sub(1);
        }
    }

    fn finish_finals(&mut self) {
        while self.pending_finals > 0 {
            match self.final_done.recv_timeout(Duration::from_secs(1)) {
                Ok(()) => self.pending_finals -= 1,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.pending_finals = 0;
                    let _ = self.tx.send(LiveEvent::Error(
                        "The transcription worker stopped before all audio was committed.".into(),
                    ));
                }
            }
        }
    }

    fn shutdown_inference(&mut self) {
        self.jobs.take();
        if let Some(handle) = self.inference.take() {
            let _ = handle.join();
        }
    }
}

enum InferenceJob {
    Partial {
        pcm: Vec<f32>,
        lang: String,
        expected_generation: usize,
    },
    Final {
        pcm: Vec<f32>,
        lang: String,
        base: i64,
        dur: i64,
        cjk: bool,
    },
}

struct PartialGuard(Arc<AtomicBool>);

impl Drop for PartialGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn acquire_partial(busy: &AtomicBool) -> bool {
    busy.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
}

fn inference_loop(
    engine: Engine,
    generation: Arc<AtomicUsize>,
    publication: Arc<Mutex<()>>,
    busy: Arc<AtomicBool>,
    jobs: Receiver<InferenceJob>,
    final_done: Sender<()>,
    tx: Sender<LiveEvent>,
) {
    let mut alerts = Alerts::default();
    while let Ok(job) = jobs.recv() {
        match job {
            InferenceJob::Partial {
                pcm,
                lang,
                expected_generation,
            } => {
                let _busy = PartialGuard(busy.clone());
                if generation.load(Ordering::Relaxed) != expected_generation {
                    continue;
                }
                let result = engine.transcribe(&pcm, &lang, true, None);
                let _publication = publication.lock().unwrap_or_else(|e| e.into_inner());
                if generation.load(Ordering::Relaxed) != expected_generation {
                    continue;
                }
                match result {
                    Ok(segs) => {
                        let sep = if is_cjk(&lang) { "" } else { " " };
                        let text = segs
                            .iter()
                            .map(|s| s.text.trim())
                            .collect::<Vec<_>>()
                            .join(sep);
                        alerts.recovered();
                        let _ = tx.send(LiveEvent::Partial(text));
                    }
                    Err(e) => {
                        if let Some(text) = alerts.report(&e) {
                            let _ = tx.send(LiveEvent::Error(text));
                        }
                    }
                }
            }
            InferenceJob::Final {
                pcm,
                lang,
                base,
                dur,
                cjk,
            } => {
                commit_stretch(
                    &mut || engine.transcribe(&pcm, &lang, true, None),
                    &tx,
                    &mut alerts,
                    base,
                    dur,
                    cjk,
                );
                let _ = final_done.send(());
            }
        }
    }
}

/// Commits one stretch of speech. The recogniser gets one more go at the same
/// audio, and when that fails too the stretch still reaches the cue list, as a
/// marked line rather than nothing at all.
fn commit_stretch(
    attempt: &mut dyn FnMut() -> Result<Vec<AsrSeg>>,
    tx: &Sender<LiveEvent>,
    alerts: &mut Alerts,
    base: i64,
    dur: i64,
    cjk: bool,
) {
    match transcribe_with_retry(attempt) {
        Ok(segs) => {
            for s in resegment(segs, cjk) {
                let text = s.text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let _ = tx.send(LiveEvent::Final {
                    start_ms: base + s.start_ms.clamp(0, dur),
                    end_ms: base + s.end_ms.clamp(0, dur).max(s.start_ms + 300),
                    text,
                });
            }
            alerts.recovered();
        }
        Err(e) => {
            let _ = tx.send(LiveEvent::Final {
                start_ms: base,
                end_ms: base + dur.max(300),
                text: MISSED_CUE.to_string(),
            });
            if let Some(text) = alerts.report(&e) {
                let _ = tx.send(LiveEvent::Error(text));
            }
        }
    }
}

/// One retry of the same audio: a transient failure often passes the second time.
fn transcribe_with_retry(attempt: &mut dyn FnMut() -> Result<Vec<AsrSeg>>) -> Result<Vec<AsrSeg>> {
    match attempt() {
        Err(_) => attempt(),
        ok => ok,
    }
}

/// The failures already shown to the user. One message per run of failures,
/// but a different message is news even while the first one is still repeating.
#[derive(Default)]
struct Alerts {
    last: Option<String>,
}

impl Alerts {
    /// The text to show, or `None` when this is the same failure as last time.
    fn report(&mut self, e: &anyhow::Error) -> Option<String> {
        let text = format!("{e:#}");
        if self.last.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.last = Some(text.clone());
        Some(text)
    }

    fn recovered(&mut self) {
        self.last = None;
    }
}

/// A 16-bit mono WAV written as it is heard. The header is brought up to date
/// every few seconds, so a crash still leaves a playable file.
///
/// RIFF counts bytes in a `u32`, so the file stops at the last sample the
/// header can describe — about 37.3 hours at 16 kHz, 16-bit mono.
const MAX_SAMPLES: u64 = (u32::MAX as u64 - 36) / 2;

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
        let room = (MAX_SAMPLES - self.samples.min(MAX_SAMPLES)) as usize;
        let take = pcm.len().min(room);
        for s in &pcm[..take] {
            self.out
                .write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
        }
        self.samples += take as u64;
        self.unpatched += take;
        if self.unpatched >= SAMPLE_RATE * 3 {
            self.patch()?;
        }
        if take < pcm.len() {
            // The header has no room for more; write what fits and stop, rather
            // than keep appending samples no player will reach.
            self.patch()?;
            return Err(std::io::Error::other(
                "the recording reached the 4 GB limit of a WAV file",
            ));
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
    fn only_one_partial_decode_can_be_in_flight() {
        let busy = AtomicBool::new(false);
        assert!(acquire_partial(&busy));
        assert!(!acquire_partial(&busy));
        busy.store(false, Ordering::Release);
        assert!(acquire_partial(&busy));
    }

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

    #[test]
    fn retries_the_same_audio_once() {
        let (tx, rx) = channel();
        let mut alerts = Alerts::default();
        let mut calls = 0;
        let mut attempt = || {
            calls += 1;
            if calls == 1 {
                Err(anyhow::anyhow!("the recogniser stopped"))
            } else {
                Ok(vec![AsrSeg {
                    start_ms: 0,
                    end_ms: 500,
                    text: "hello".into(),
                }])
            }
        };
        commit_stretch(&mut attempt, &tx, &mut alerts, 1_000, 900, false);
        drop(tx);
        assert_eq!(calls, 2);
        let finals: Vec<_> = rx
            .try_iter()
            .filter_map(|e| match e {
                LiveEvent::Final {
                    start_ms,
                    end_ms,
                    text,
                } => Some((start_ms, end_ms, text)),
                _ => None,
            })
            .collect();
        assert_eq!(finals, vec![(1_000, 1_500, "hello".to_string())]);
    }

    #[test]
    fn a_stretch_that_fails_twice_leaves_a_marked_cue() {
        let (tx, rx) = channel();
        let mut alerts = Alerts::default();
        let mut calls = 0;
        let mut attempt = || {
            calls += 1;
            Err::<Vec<AsrSeg>, _>(anyhow::anyhow!("the recogniser stopped"))
        };
        commit_stretch(&mut attempt, &tx, &mut alerts, 1_000, 900, false);
        drop(tx);
        assert_eq!(calls, 2);
        let events: Vec<LiveEvent> = rx.try_iter().collect();
        assert!(
            matches!(&events[0], LiveEvent::Final { start_ms: 1_000, end_ms: 1_900, text } if text.as_str() == MISSED_CUE),
            "the stretch is marked in the cue list"
        );
        assert!(
            matches!(&events[1], LiveEvent::Error(m) if m.contains("the recogniser stopped")),
            "the failure is shown"
        );
    }

    #[test]
    fn a_changed_error_reaches_the_ui() {
        let mut alerts = Alerts::default();
        assert_eq!(
            alerts.report(&anyhow::anyhow!("boom")).as_deref(),
            Some("boom")
        );
        assert_eq!(alerts.report(&anyhow::anyhow!("boom")), None);
        assert_eq!(
            alerts.report(&anyhow::anyhow!("bang")).as_deref(),
            Some("bang")
        );
        // After a good stretch, the same words are news again.
        alerts.recovered();
        assert_eq!(
            alerts.report(&anyhow::anyhow!("bang")).as_deref(),
            Some("bang")
        );
    }

    #[test]
    fn recording_stops_at_the_wav_limit() {
        let path = std::env::temp_dir().join(format!("taurus-limit-{}.wav", std::process::id()));
        let mut r = Recording::create(&path).unwrap();
        // Four samples short of the largest length a RIFF header can describe.
        r.samples = MAX_SAMPLES - 4;
        let err = r.push(&[0.5; 16]).unwrap_err();
        assert!(err.to_string().contains("4 GB"), "{err}");
        assert_eq!(r.samples, MAX_SAMPLES);
        // Nothing more goes in once the header is full.
        assert!(r.push(&[0.5; 16]).is_err());
        assert_eq!(r.samples, MAX_SAMPLES);
        drop(r);
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes.len(), 44 + 4 * 2, "only the four samples that fit");
        let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        let riff = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(data as u64, MAX_SAMPLES * 2);
        assert_eq!(riff as u64, 36 + data as u64);
    }
}
