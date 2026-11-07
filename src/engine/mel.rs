//! Whisper's log-mel spectrogram: a centred 400-point STFT every 160 samples,
//! a Slaney-normalised mel filterbank as librosa builds it, log10 clamped to
//! 8 below the peak, then scaled to roughly -1..1.

use std::f32::consts::PI;
use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

const N_FFT: usize = 400;
const HOP: usize = 160;
const BINS: usize = N_FFT / 2 + 1;

pub struct Mel {
    n_mels: usize,
    filters: Vec<f32>,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
}

impl Mel {
    pub fn new(n_mels: usize, sample_rate: f32, fmax: f32) -> Mel {
        let window = (0..N_FFT).map(|n| 0.5 - 0.5 * (2.0 * PI * n as f32 / N_FFT as f32).cos()).collect();
        Mel {
            n_mels,
            filters: slaney_filters(n_mels, sample_rate, 0.0, fmax),
            window,
            fft: FftPlanner::new().plan_fft_forward(N_FFT),
        }
    }

    pub fn n_mels(&self) -> usize {
        self.n_mels
    }

    /// Returns `[n_mels, frames]` row-major and the frame count.
    pub fn compute(&self, pcm: &[f32]) -> (Vec<f32>, usize) {
        let pad = N_FFT / 2;
        let padded: Vec<f32> = (0..pcm.len() + 2 * pad).map(|i| reflect(pcm, i as isize - pad as isize)).collect();
        let frames = (padded.len() - N_FFT) / HOP + 1;
        let kept = frames.saturating_sub(1).max(1);
        let mut power = vec![0f32; BINS];
        let mut buf = vec![Complex32::default(); N_FFT];
        let mut out = vec![0f32; self.n_mels * kept];
        for f in 0..kept {
            let at = f * HOP;
            for (i, c) in buf.iter_mut().enumerate() {
                *c = Complex32::new(padded.get(at + i).copied().unwrap_or(0.0) * self.window[i], 0.0);
            }
            self.fft.process(&mut buf);
            for (k, p) in power.iter_mut().enumerate() {
                *p = buf[k].norm_sqr();
            }
            for m in 0..self.n_mels {
                let row = &self.filters[m * BINS..(m + 1) * BINS];
                let e: f32 = row.iter().zip(&power).map(|(w, p)| w * p).sum();
                out[m * kept + f] = e.max(1e-10).log10();
            }
        }
        let peak = out.iter().copied().fold(f32::MIN, f32::max);
        for v in &mut out {
            *v = (v.max(peak - 8.0) + 4.0) / 4.0;
        }
        (out, kept)
    }
}

fn reflect(pcm: &[f32], i: isize) -> f32 {
    let n = pcm.len() as isize;
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return pcm[0];
    }
    let period = 2 * (n - 1);
    let mut j = i.rem_euclid(period);
    if j >= n {
        j = period - j;
    }
    pcm[j as usize]
}

fn hz_to_mel(hz: f32) -> f32 {
    let f_sp = 200.0 / 3.0;
    let min_log_hz = 1000.0;
    let min_log_mel = min_log_hz / f_sp;
    let logstep = 6.4f32.ln() / 27.0;
    if hz >= min_log_hz { min_log_mel + (hz / min_log_hz).ln() / logstep } else { hz / f_sp }
}

fn mel_to_hz(mel: f32) -> f32 {
    let f_sp = 200.0 / 3.0;
    let min_log_hz = 1000.0;
    let min_log_mel = min_log_hz / f_sp;
    let logstep = 6.4f32.ln() / 27.0;
    if mel >= min_log_mel { min_log_hz * (logstep * (mel - min_log_mel)).exp() } else { f_sp * mel }
}

fn slaney_filters(n_mels: usize, sr: f32, fmin: f32, fmax: f32) -> Vec<f32> {
    let (lo, hi) = (hz_to_mel(fmin), hz_to_mel(fmax));
    let pts: Vec<f32> = (0..n_mels + 2).map(|i| mel_to_hz(lo + (hi - lo) * i as f32 / (n_mels + 1) as f32)).collect();
    let freqs: Vec<f32> = (0..BINS).map(|k| k as f32 * sr / N_FFT as f32).collect();
    let mut w = vec![0f32; n_mels * BINS];
    for m in 0..n_mels {
        let (l, c, r) = (pts[m], pts[m + 1], pts[m + 2]);
        let norm = 2.0 / (r - l);
        for (k, &f) in freqs.iter().enumerate() {
            let up = (f - l) / (c - l);
            let down = (r - f) / (r - c);
            w[m * BINS + k] = up.min(down).max(0.0) * norm;
        }
    }
    w
}
