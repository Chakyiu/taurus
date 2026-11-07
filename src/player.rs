//! Plays a project's audio on the default output device. Audio is held as
//! 16 kHz mono, the same clock the cues use, and resampled on the fly.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context as _, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::media::SAMPLE_RATE;

pub struct Player {
    pcm: Arc<Vec<f32>>,
    /// Position in source samples, as `f64` bits. Seeking writes it; the
    /// output callback only advances it if no seek happened meanwhile.
    pos: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
    _stream: cpal::Stream,
}

impl Player {
    pub fn new(pcm: Vec<f32>) -> Result<Self> {
        let pcm = Arc::new(pcm);
        let pos = Arc::new(AtomicU64::new(0f64.to_bits()));
        let playing = Arc::new(AtomicBool::new(false));
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| anyhow!("no speaker or headphones are connected"))?;
        let config = device.default_output_config().context("reading the output format")?;
        let channels = config.channels() as usize;
        let step = SAMPLE_RATE as f64 / config.sample_rate() as f64;
        let err = |e: cpal::Error| eprintln!("taurus: playback: {e}");
        let (p, q, r) = (pcm.clone(), pos.clone(), playing.clone());
        let render = move |out: &mut dyn FnMut(usize, f32), frames: usize| {
            let start = q.load(Ordering::Relaxed);
            let mut at = f64::from_bits(start);
            let on = r.load(Ordering::Relaxed);
            for i in 0..frames {
                let v = if on { sample(&p, at) } else { 0.0 };
                if on {
                    at += step;
                }
                for c in 0..channels {
                    out(i * channels + c, v);
                }
            }
            if on {
                if at >= p.len() as f64 {
                    r.store(false, Ordering::Relaxed);
                    at = p.len() as f64;
                }
                let _ = q.compare_exchange(start, at.to_bits(), Ordering::Relaxed, Ordering::Relaxed);
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
                    render(&mut |i, v| data[i] = (v.clamp(-1.0, 1.0) * 32767.0) as i16, n)
                },
                err,
                None,
            )?,
            f => bail!("the speaker uses an unsupported format ({f})"),
        };
        stream.play().context("starting playback")?;
        Ok(Self {
            pcm,
            pos,
            playing,
            _stream: stream,
        })
    }

    pub fn duration_ms(&self) -> i64 {
        (self.pcm.len() * 1000 / SAMPLE_RATE) as i64
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
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("{} is not a WAV file", path.display());
    }
    Ok(bytes[44..]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect())
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
