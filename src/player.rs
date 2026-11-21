//! Plays a project's audio on the default output device. Audio is held as
//! 16 kHz mono, the same clock the cues use, and resampled on the fly.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::media::SAMPLE_RATE;

struct Window {
    start: usize,
    pcm: Vec<f32>,
}

pub struct Player {
    duration_ms: i64,
    pos: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
    stop_window: Option<Arc<AtomicBool>>,
    _stream: cpal::Stream,
}

impl Player {
    pub fn new(pcm: Vec<f32>) -> Result<Self> {
        let duration_ms = (pcm.len() * 1000 / SAMPLE_RATE) as i64;
        Self::build(pcm, 0, duration_ms, None)
    }

    /// Plays a moving, bounded decode window; the next window is loaded off
    /// the audio callback as playback approaches its end.
    pub fn new_windowed(
        pcm: Vec<f32>,
        start_ms: i64,
        duration_ms: i64,
        load: impl Fn(i64, i64, &AtomicBool) -> Result<Vec<f32>> + Send + 'static,
    ) -> Result<Self> {
        Self::build(pcm, start_ms, duration_ms, Some(Box::new(load)))
    }

    fn build(
        pcm: Vec<f32>,
        start_ms: i64,
        duration_ms: i64,
        load: Option<Box<dyn Fn(i64, i64, &AtomicBool) -> Result<Vec<f32>> + Send>>,
    ) -> Result<Self> {
        let pcm = Arc::new(RwLock::new(Window {
            start: (start_ms.max(0) as usize) * SAMPLE_RATE / 1000,
            pcm,
        }));
        let total_samples = (duration_ms.max(0) as usize) * SAMPLE_RATE / 1000;
        let pos = Arc::new(AtomicU64::new(
            ((start_ms.max(0) as usize * SAMPLE_RATE / 1000) as f64).to_bits(),
        ));
        let playing = Arc::new(AtomicBool::new(false));
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| anyhow!("no speaker or headphones are connected"))?;
        let config = device
            .default_output_config()
            .context("reading the output format")?;
        let channels = config.channels() as usize;
        let step = SAMPLE_RATE as f64 / config.sample_rate() as f64;
        let err = |e: cpal::Error| eprintln!("taurus: playback: {e}");
        let (p, q, r) = (pcm.clone(), pos.clone(), playing.clone());
        let render = move |out: &mut dyn FnMut(usize, f32), frames: usize| {
            let start = q.load(Ordering::Relaxed);
            let mut at = f64::from_bits(start);
            let on = r.load(Ordering::Relaxed);
            let window = p.try_read().ok();
            for i in 0..frames {
                let local = window.as_ref().and_then(|w| {
                    let offset = at - w.start as f64;
                    (offset >= 0.0 && offset < w.pcm.len() as f64).then(|| sample(&w.pcm, offset))
                });
                let v = if on { local.unwrap_or(0.0) } else { 0.0 };
                if on && local.is_some() {
                    at += step;
                }
                for c in 0..channels {
                    out(i * channels + c, v);
                }
            }
            if on {
                if at >= total_samples as f64 {
                    r.store(false, Ordering::Relaxed);
                    at = total_samples as f64;
                }
                let _ =
                    q.compare_exchange(start, at.to_bits(), Ordering::Relaxed, Ordering::Relaxed);
            }
        };
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                config.into(),
                move |data: &mut [f32], _: &_| {
                    let n = data.len() / channels;
                    render(&mut |i, v| data[i] = v, n)
                },
                err,
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_output_stream(
                config.into(),
                move |data: &mut [i16], _: &_| {
                    let n = data.len() / channels;
                    render(
                        &mut |i, v| data[i] = (v.clamp(-1.0, 1.0) * 32767.0) as i16,
                        n,
                    )
                },
                err,
                None,
            )?,
            f => bail!("the speaker uses an unsupported format ({f})"),
        };
        stream.play().context("starting playback")?;
        let stop_window = load.map(|load| {
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let worker_pcm = pcm.clone();
            let worker_pos = pos.clone();
            let _worker = thread::spawn(move || {
                let total = (duration_ms.max(0) as usize) * SAMPLE_RATE / 1000;
                while !worker_stop.load(Ordering::Relaxed) {
                    let current = f64::from_bits(worker_pos.load(Ordering::Relaxed)) as usize;
                    let (base, end) = worker_pcm
                        .read()
                        .map(|w| (w.start, w.start.saturating_add(w.pcm.len())))
                        .unwrap_or((current, current));
                    let threshold = end.saturating_sub(10 * SAMPLE_RATE);
                    let prefetch =
                        current >= base && current < end && current >= threshold && end < total;
                    if let Some(start) = next_window_start(base, end, current, total) {
                        let start_ms = (start * 1000 / SAMPLE_RATE) as i64;
                        let length_ms = ((total - start) * 1000 / SAMPLE_RATE).min(60_000) as i64;
                        if let Ok(samples) = load(start_ms, length_ms, &worker_stop) {
                            let ready_at = start;
                            let ready_end = ready_at.saturating_add(samples.len());
                            let mut ready = false;
                            loop {
                                if worker_stop.load(Ordering::Relaxed) {
                                    break;
                                }
                                let now =
                                    f64::from_bits(worker_pos.load(Ordering::Relaxed)) as usize;
                                if prefetch {
                                    if now < threshold || now >= end {
                                        break;
                                    }
                                    if now >= ready_at {
                                        ready = true;
                                        break;
                                    }
                                } else if now >= ready_at && now < ready_end {
                                    ready = true;
                                    break;
                                } else {
                                    break;
                                }
                                thread::sleep(Duration::from_millis(50));
                            }
                            if ready && !worker_stop.load(Ordering::Relaxed) {
                                if let Ok(mut window) = worker_pcm.write() {
                                    window.start = ready_at;
                                    window.pcm = samples;
                                }
                            }
                        } else {
                            thread::sleep(Duration::from_millis(500));
                        }
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            });
            stop
        });
        Ok(Self {
            duration_ms,
            pos,
            playing,
            stop_window,
            _stream: stream,
        })
    }

    pub fn duration_ms(&self) -> i64 {
        self.duration_ms
    }

    pub fn position_ms(&self) -> i64 {
        (f64::from_bits(self.pos.load(Ordering::Relaxed)) * 1000.0 / SAMPLE_RATE as f64) as i64
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn seek(&self, ms: i64) {
        let at = (ms.clamp(0, self.duration_ms()) as f64) * SAMPLE_RATE as f64 / 1000.0;
        self.pos.store(at.to_bits(), Ordering::Relaxed);
    }

    pub fn set_playing(&self, on: bool) {
        if on && self.position_ms() >= self.duration_ms() {
            self.seek(0);
        }
        self.playing.store(on, Ordering::Relaxed);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop_window {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

fn sample(pcm: &[f32], at: f64) -> f32 {
    let i = at as usize;
    let Some(&a) = pcm.get(i) else { return 0.0 };
    let b = pcm.get(i + 1).copied().unwrap_or(a);
    let t = (at - i as f64) as f32;
    a + (b - a) * t
}

/// Reads a capture's 16-bit mono WAV. Samples run to the end of the file, so
/// a recording whose header was never brought up to date still plays whole.
pub fn read_recording(path: &Path) -> Result<Vec<f32>> {
    read_recording_window(path, 0, recording_duration_ms(path)?)
}

pub fn recording_duration_ms(path: &Path) -> Result<i64> {
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut header = [0; 12];
    file.read_exact(&mut header)?;
    if &header[..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        bail!("{} is not a WAV file", path.display());
    }
    let bytes = file.metadata()?.len().saturating_sub(44);
    Ok(bytes.div_ceil(32) as i64)
}

/// Reads only the requested range of a 16-bit, 16 kHz mono capture WAV.
pub fn read_recording_window(path: &Path, start_ms: i64, duration_ms: i64) -> Result<Vec<f32>> {
    let total_ms = recording_duration_ms(path)?;
    let (start, end) = recording_window_range(total_ms, start_ms, duration_ms);
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    file.seek(SeekFrom::Start(44 + start as u64 * 32))?;
    let mut bytes = vec![0; (end - start) * 32];
    let n = file.read(&mut bytes)?;
    Ok(bytes[..n / 2 * 2]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect())
}

fn next_window_start(base: usize, end: usize, current: usize, total: usize) -> Option<usize> {
    if current >= total {
        None
    } else if current < base || current >= end {
        Some(current.saturating_sub(5 * SAMPLE_RATE))
    } else if current >= end.saturating_sub(10 * SAMPLE_RATE) && end < total {
        Some(end.saturating_sub(5 * SAMPLE_RATE))
    } else {
        None
    }
}

fn recording_window_range(total_ms: usize, start_ms: i64, duration_ms: i64) -> (usize, usize) {
    let start = (start_ms.max(0) as usize).min(total_ms);
    let end = start
        .saturating_add(duration_ms.max(0) as usize)
        .min(total_ms);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_between_samples() {
        let pcm = [0.0, 1.0];
        assert_eq!(sample(&pcm, 0.5), 0.5);
        assert_eq!(sample(&pcm, 1.0), 1.0);
        assert_eq!(sample(&pcm, 5.0), 0.0);
    }

    #[test]
    fn playback_window_is_clamped_to_the_recording() {
        assert_eq!(recording_window_range(100, -10, 20), (0, 20));
        assert_eq!(recording_window_range(100, 95, 20), (95, 100));
        assert_eq!(recording_window_range(100, 120, 20), (100, 100));

        let total = 120 * SAMPLE_RATE;
        assert_eq!(
            next_window_start(0, 60 * SAMPLE_RATE, 5 * SAMPLE_RATE, total),
            None
        );
        assert_eq!(
            next_window_start(0, 60 * SAMPLE_RATE, 50 * SAMPLE_RATE, total),
            Some(55 * SAMPLE_RATE)
        );
        assert_eq!(
            next_window_start(0, 60 * SAMPLE_RATE, 80 * SAMPLE_RATE, total),
            Some(75 * SAMPLE_RATE)
        );
    }

    #[test]
    fn one_second_window_reads_16000_samples() {
        let path = std::env::temp_dir().join(format!("taurus-window-{}.wav", std::process::id()));
        let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
        bytes.resize(44, 0);
        bytes.resize(44 + 32_000, 0);
        std::fs::write(&path, bytes).unwrap();
        let pcm = read_recording_window(&path, 0, 1_000).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(pcm.len(), 16_000);
    }

    #[test]
    fn reads_past_a_stale_header() {
        let path = std::env::temp_dir().join(format!("taurus-play-{}.wav", std::process::id()));
        let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
        bytes.resize(44, 0);
        bytes.extend_from_slice(&16384i16.to_le_bytes());
        bytes.extend_from_slice(&(-16384i16).to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        let pcm = read_recording(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(pcm, vec![0.5, -0.5]);
    }
}
