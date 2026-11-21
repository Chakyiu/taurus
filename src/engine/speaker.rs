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
    super::fetch(
        URL,
        &model_path(),
        &mut |done, len| {
            if len > 0 {
                progress(done as f32 / len as f32);
            }
        },
        cancel,
    )
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
        Ok(Self {
            session: Mutex::new(session),
            fbank: Fbank::new(),
        })
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
        let out =
            s.run(ort::inputs!["x" => Tensor::from_array(([1usize, frames, N_MELS], feats))?])?;
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
/// A fingerprint joins an existing voice only at or above this cosine score.
const SAME_VOICE: f32 = 0.7;

/// Labels each cue with a speaker number, numbered in order of first
/// appearance. `spans` are cue times in milliseconds on `pcm`'s clock.
/// The live paths load audio window by window through [`identify_from`];
/// only the e2e test hands the whole PCM over at once.
#[cfg(test)]
pub fn identify(
    model: &SpeakerModel,
    pcm: &[f32],
    spans: &[(i64, i64)],
    progress: impl FnMut(usize),
    cancel: &AtomicBool,
) -> Result<Vec<usize>> {
    let len_ms = (pcm.len() / 16) as i64;
    identify_from(
        model,
        len_ms,
        spans,
        |a, b| Ok(pcm[(a * 16) as usize..(b * 16) as usize].to_vec()),
        progress,
        cancel,
    )
}

/// Fingerprints cues by loading only the audio around each cue.
pub fn identify_from(
    model: &SpeakerModel,
    duration_ms: i64,
    spans: &[(i64, i64)],
    mut load_window: impl FnMut(i64, i64) -> Result<Vec<f32>>,
    mut progress: impl FnMut(usize),
    cancel: &AtomicBool,
) -> Result<Vec<usize>> {
    let mut embs: Vec<Option<(Vec<f32>, f32)>> = Vec::with_capacity(spans.len());
    for (i, &span) in spans.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let (a, b) = cue_window(span, duration_ms);
        let pcm = load_window(a, b)?;
        embs.push(model.embed(&pcm).ok().map(|v| (v, (b - a) as f32)));
        progress(i + 1);
    }
    Ok(cluster(&embs))
}

fn cue_window((start, end): (i64, i64), duration_ms: i64) -> (i64, i64) {
    let duration_ms = duration_ms.max(0);
    let mut a = start.clamp(0, duration_ms);
    let mut b = end.clamp(a, duration_ms);
    if b - a > MAX_MS {
        let mid = a + (b - a) / 2;
        a = (mid - MAX_MS / 2).clamp(0, duration_ms);
        b = (a + MAX_MS).min(duration_ms);
    }
    if b - a < MIN_MS {
        let missing = MIN_MS - (b - a);
        a = (a - (missing + 1) / 2).max(0);
        b = (b + missing / 2).min(duration_ms);
    }
    (a, b)
}

/// Assigns each fingerprint once to its nearest sufficiently similar voice.
/// Existing labels never change when a group's centre moves.
fn cluster(embs: &[Option<(Vec<f32>, f32)>]) -> Vec<usize> {
    let mut centres: Vec<Vec<f32>> = Vec::new();
    let mut weights: Vec<f32> = Vec::new();
    let mut out = Vec::with_capacity(embs.len());
    let mut last = 0;
    for emb in embs {
        let Some((v, weight)) = emb else {
            out.push(last);
            continue;
        };
        let near = centres
            .iter()
            .enumerate()
            .map(|(i, c)| (i, dot(v, c)))
            .filter(|(_, similarity)| *similarity >= SAME_VOICE)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i);
        let label = if let Some(i) = near {
            let total = weights[i] + weight;
            centres[i]
                .iter_mut()
                .zip(v)
                .for_each(|(c, x)| *c = (*c * weights[i] + x * weight) / total);
            let norm = centres[i]
                .iter()
                .map(|x| x * x)
                .sum::<f32>()
                .sqrt()
                .max(1e-6);
            centres[i].iter_mut().for_each(|x| *x /= norm);
            weights[i] = total;
            i
        } else {
            centres.push(v.clone());
            weights.push(*weight);
            centres.len() - 1
        };
        last = label;
        out.push(label);
    }
    out
}

fn dot(x: &[f32], y: &[f32]) -> f32 {
    x.iter().zip(y).map(|(p, q)| p * q).sum()
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
                let hann =
                    0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FRAME - 1) as f32).cos();
                hann.powf(0.85)
            })
            .collect();
        let mel = |f: f32| 1127.0 * (1.0 + f / 700.0).ln();
        let (lo, hi) = (mel(20.0), mel(8000.0));
        let step = (hi - lo) / (N_MELS + 1) as f32;
        let bins = (0..N_MELS)
            .map(|m| {
                let (l, c, r) = (
                    lo + m as f32 * step,
                    lo + (m + 1) as f32 * step,
                    lo + (m + 2) as f32 * step,
                );
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
                *c = Complex::new(
                    if i < FRAME {
                        x[i] * self.window[i]
                    } else {
                        0.0
                    },
                    0.0,
                );
            }
            fft.process(&mut buf);
            for (first, w) in &self.bins {
                let e: f32 = w
                    .iter()
                    .enumerate()
                    .map(|(j, wj)| wj * buf[first + j].norm_sqr())
                    .sum();
                out.push(e.max(f32::EPSILON).ln());
            }
        }
        (out, frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fingerprint pointing along `x`, from a 3 s line.
    fn v(x: &[f32]) -> Option<(Vec<f32>, f32)> {
        let n = x.iter().map(|a| a * a).sum::<f32>().sqrt();
        Some((x.iter().map(|a| a / n).collect(), 3_000.0))
    }

    #[test]
    fn groups_alike_voices_in_order_of_appearance() {
        let embs = vec![
            v(&[1.0, 0.1]),
            v(&[0.1, 1.0]),
            None,
            v(&[0.9, 0.2]),
            v(&[0.2, 0.9]),
        ];
        assert_eq!(cluster(&embs), vec![0, 1, 1, 0, 1]);
    }

    #[test]
    fn one_pass_assignment_never_moves_an_earlier_cue() {
        let embs = vec![
            v(&[1.0, 0.0]),
            v(&[0.0, 1.0]),
            v(&[0.8, 0.6]), // joins speaker 0
            v(&[0.6, 0.8]), // joins speaker 0's updated centre
        ];
        let labels = cluster(&embs);
        assert_eq!(labels, vec![0, 1, 0, 0]);
    }

    #[test]
    fn cue_windows_are_bounded_and_cover_short_cues() {
        assert_eq!(cue_window((1_000, 2_000), 10_000), (900, 2_100));
        assert_eq!(cue_window((100_000, 115_000), 120_000), (101_500, 113_500));
        assert_eq!(cue_window((-500, 300), 10_000), (0, 750));
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
