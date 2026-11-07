//! Who is speaking: each cue's audio becomes a voice fingerprint (CAM++ from
//! 3D-Speaker, through ONNX Runtime), and fingerprints that sound alike are
//! grouped into speakers. Nothing leaves the machine.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow, bail};
use ort::session::Session;
use ort::session::builder::{AutoDevicePolicy, GraphOptimizationLevel};
use ort::value::Tensor;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

use crate::store;

pub const SPEAKER_DOWNLOAD_ID: &str = "speaker-campplus";
pub const SPEAKER_SIZE: &str = "28 MB";
const FILE: &str = "campplus-zh-en-16k.onnx";
const URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx";

pub fn model_path() -> PathBuf {
    store::models_dir().join("speaker").join(FILE)
}

pub fn is_downloaded() -> bool {
    model_path().exists()
}

pub fn download(progress: impl Fn(f32), cancel: &AtomicBool) -> Result<()> {
    super::fetch(URL, &model_path(), &mut |done, len| {
        if len > 0 {
            progress(done as f32 / len as f32);
        }
    }, cancel)
}

pub struct SpeakerModel {
    session: Mutex<Session>,
    fbank: Fbank,
}

impl SpeakerModel {
    pub fn load() -> Result<Self> {
        let path = model_path();
        if !path.exists() {
            bail!("the speaker model is not downloaded yet");
        }
        let env = super::ort_env()?;
        let fail = |e: &dyn std::fmt::Display| anyhow!("could not load the speaker model: {e}");
        let session = Session::builder(&env)
            .map_err(|e| fail(&e))?
            .with_auto_device(AutoDevicePolicy::PreferCPU)
            .map_err(|e| fail(&e))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| fail(&e))?
            .commit_from_file(&path)
            .map_err(|e| fail(&e))?;
        Ok(Self { session: Mutex::new(session), fbank: Fbank::new() })
    }

    /// A unit-length voice fingerprint for one stretch of 16 kHz audio.
    pub fn embed(&self, pcm: &[f32]) -> Result<Vec<f32>> {
        let (mut feats, frames) = self.fbank.compute(pcm);
        if frames < 10 {
            bail!("too short to recognise a voice");
        }
        // The model was trained on features with their mean over time removed.
        for b in 0..N_MELS {
            let mean = (0..frames).map(|t| feats[t * N_MELS + b]).sum::<f32>() / frames as f32;
            for t in 0..frames {
                feats[t * N_MELS + b] -= mean;
            }
        }
        let mut s = self.session.lock().unwrap();
        let out = s.run(ort::inputs!["x" => Tensor::from_array(([1usize, frames, N_MELS], feats))?])?;
        let (_, v) = out["embedding"].try_extract_tensor::<f32>()?;
        let mut v = v.to_vec();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter_mut().for_each(|x| *x /= norm);
        Ok(v)
    }
}

/// Shortest stretch a fingerprint is taken from; shorter cues borrow the
/// audio around them.
const MIN_MS: i64 = 1_200;
/// Longer cues are fingerprinted from their middle.
const MAX_MS: i64 = 12_000;
/// Two groups whose voices are at least this alike are the same speaker.
const SAME_SPEAKER: f32 = 0.5;

/// Labels each cue with a speaker number, numbered in order of first
/// appearance. `spans` are cue times in milliseconds on `pcm`'s clock.
pub fn identify(
    model: &SpeakerModel,
    pcm: &[f32],
    spans: &[(i64, i64)],
    mut progress: impl FnMut(usize),
    cancel: &AtomicBool,
) -> Result<Vec<usize>> {
    let len_ms = (pcm.len() / 16) as i64;
    let mut embs: Vec<Option<Vec<f32>>> = Vec::with_capacity(spans.len());
    for (i, &(a, b)) in spans.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let (mut a, mut b) = (a.clamp(0, len_ms), b.clamp(0, len_ms));
        if b - a > MAX_MS {
            let mid = (a + b) / 2;
            (a, b) = (mid - MAX_MS / 2, mid + MAX_MS / 2);
        }
        if b - a < MIN_MS {
            let pad = (MIN_MS - (b - a)) / 2;
            (a, b) = ((a - pad).max(0), (b + pad).min(len_ms));
        }
        let slice = &pcm[(a * 16) as usize..(b * 16) as usize];
        embs.push(model.embed(slice).ok());
        progress(i + 1);
    }
    Ok(cluster(&embs))
}

/// Average-linkage grouping on cosine similarity. Cues without a fingerprint
/// join the speaker just before them.
fn cluster(embs: &[Option<Vec<f32>>]) -> Vec<usize> {
    let idx: Vec<usize> = (0..embs.len()).filter(|&i| embs[i].is_some()).collect();
    let mut groups: Vec<Vec<usize>> = idx.iter().map(|&i| vec![i]).collect();
    let sim = |a: usize, b: usize| -> f32 {
        let (x, y) = (embs[a].as_ref().unwrap(), embs[b].as_ref().unwrap());
        x.iter().zip(y).map(|(p, q)| p * q).sum()
    };
    loop {
        let mut best = (SAME_SPEAKER, None);
        for g in 0..groups.len() {
            for h in g + 1..groups.len() {
                let mut total = 0.0;
                for &a in &groups[g] {
                    for &b in &groups[h] {
                        total += sim(a, b);
                    }
                }
                let avg = total / (groups[g].len() * groups[h].len()) as f32;
                if avg > best.0 {
                    best = (avg, Some((g, h)));
                }
            }
        }
        let Some((g, h)) = best.1 else { break };
        let moved = groups.remove(h);
        groups[g].extend(moved);
    }
    let mut of = vec![None; embs.len()];
    for (n, g) in groups.iter().enumerate() {
        for &i in g {
            of[i] = Some(n);
        }
    }
    // Renumber by first appearance, so the first voice heard is speaker 1.
    let mut order: Vec<usize> = Vec::new();
    let mut out = Vec::with_capacity(embs.len());
    let mut last = 0;
    for g in of {
        let label = match g {
            Some(g) => match order.iter().position(|&o| o == g) {
                Some(p) => p,
                None => {
                    order.push(g);
                    order.len() - 1
                }
            },
            None => last,
        };
        last = label;
        out.push(label);
    }
    out
}

const N_MELS: usize = 80;
const FRAME: usize = 400;
const SHIFT: usize = 160;
const NFFT: usize = 512;

/// Kaldi-style log mel filterbank: 25 ms Povey windows every 10 ms, DC
/// removed, pre-emphasis 0.97, 80 bins from 20 Hz to 8 kHz.
struct Fbank {
    window: Vec<f32>,
    /// One sparse triangle per bin: first FFT bin and its weights.
    bins: Vec<(usize, Vec<f32>)>,
}

impl Fbank {
    fn new() -> Self {
        let window = (0..FRAME)
            .map(|i| {
                let hann = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FRAME - 1) as f32).cos();
                hann.powf(0.85)
            })
            .collect();
        let mel = |f: f32| 1127.0 * (1.0 + f / 700.0).ln();
        let (lo, hi) = (mel(20.0), mel(8000.0));
        let step = (hi - lo) / (N_MELS + 1) as f32;
        let bins = (0..N_MELS)
            .map(|m| {
                let (l, c, r) = (lo + m as f32 * step, lo + (m + 1) as f32 * step, lo + (m + 2) as f32 * step);
                let mut first = None;
                let mut w = Vec::new();
                for k in 0..NFFT / 2 {
                    let f = mel(k as f32 * 16000.0 / NFFT as f32);
                    let v = if f > l && f <= c {
                        (f - l) / (c - l)
                    } else if f > c && f < r {
                        (r - f) / (r - c)
                    } else {
                        0.0
                    };
                    if v > 0.0 {
                        first.get_or_insert(k);
                        w.push(v);
                    } else if first.is_some() {
                        break;
                    }
                }
                (first.unwrap_or(0), w)
            })
            .collect();
        Self { window, bins }
    }

    fn compute(&self, pcm: &[f32]) -> (Vec<f32>, usize) {
        if pcm.len() < FRAME {
            return (Vec::new(), 0);
        }
        let frames = 1 + (pcm.len() - FRAME) / SHIFT;
        let fft = FftPlanner::<f32>::new().plan_fft_forward(NFFT);
        let mut out = Vec::with_capacity(frames * N_MELS);
        let mut buf = vec![Complex::new(0.0, 0.0); NFFT];
        let mut x = [0f32; FRAME];
        for t in 0..frames {
            x.copy_from_slice(&pcm[t * SHIFT..t * SHIFT + FRAME]);
            let mean = x.iter().sum::<f32>() / FRAME as f32;
            x.iter_mut().for_each(|v| *v -= mean);
            for i in (1..FRAME).rev() {
                x[i] -= 0.97 * x[i - 1];
            }
            x[0] -= 0.97 * x[0];
            for (i, c) in buf.iter_mut().enumerate() {
                *c = Complex::new(if i < FRAME { x[i] * self.window[i] } else { 0.0 }, 0.0);
            }
            fft.process(&mut buf);
            for (first, w) in &self.bins {
                let e: f32 = w.iter().enumerate().map(|(j, wj)| wj * buf[first + j].norm_sqr()).sum();
                out.push(e.max(f32::EPSILON).ln());
            }
        }
        (out, frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: &[f32]) -> Option<Vec<f32>> {
        let n = x.iter().map(|a| a * a).sum::<f32>().sqrt();
        Some(x.iter().map(|a| a / n).collect())
    }

    #[test]
    fn groups_alike_voices_in_order_of_appearance() {
        let embs = vec![v(&[1.0, 0.1]), v(&[0.1, 1.0]), None, v(&[0.9, 0.2]), v(&[0.2, 0.9])];
        assert_eq!(cluster(&embs), vec![0, 1, 1, 0, 1]);
    }

    #[test]
    fn fbank_frames_follow_kaldi_snip_edges() {
        let fb = Fbank::new();
        let (f, n) = fb.compute(&vec![0.01; 16000]);
        assert_eq!(n, 98);
        assert_eq!(f.len(), 98 * N_MELS);
        assert!(fb.bins.iter().all(|(_, w)| !w.is_empty()));
    }
}
