//! Speech recognition. Whisper runs inside the app through whisper.cpp and
//! Qwen3-ASR through ONNX Runtime; either can also be any server speaking
//! OpenAI's `/audio/transcriptions`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::Value;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::qwen::{Qwen, QwenModel};
use super::{AsrChoice, Settings, WhisperModel, agent, check};
use crate::media::SAMPLE_RATE;
use crate::subs::is_cjk;

#[derive(Clone, Debug, PartialEq)]
pub struct AsrSeg {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

pub type Progress = Arc<dyn Fn(f32) + Send + Sync>;

pub enum Engine {
    Whisper(Arc<WhisperContext>),
    Qwen(Arc<Qwen>),
    Remote {
        url: String,
        model: String,
        key: String,
    },
}

static WHISPER: Mutex<Option<(String, Arc<WhisperContext>)>> = Mutex::new(None);
static QWEN: Mutex<Option<(String, Arc<Qwen>)>> = Mutex::new(None);

/// Frees the cached model. Metal asserts at process exit if a model is still
/// alive, so this runs on quit; `true` means another thread still holds one.
pub fn unload() -> bool {
    QWEN.lock().unwrap().take();
    match WHISPER.lock().unwrap().take() {
        Some((_, ctx)) => Arc::strong_count(&ctx) > 1,
        None => false,
    }
}

impl Engine {
    /// Loading a model takes a moment, so the last one stays in memory.
    pub fn load(s: &Settings) -> Result<Engine> {
        match s.asr {
            AsrChoice::Whisper => {
                let m = WhisperModel::find(&s.whisper_model)
                    .ok_or_else(|| anyhow!("unknown Whisper model {}", s.whisper_model))?;
                if !m.is_downloaded() {
                    bail!("the Whisper {} model is not downloaded yet", m.id);
                }
                whisper_rs::install_logging_hooks();
                let mut cache = WHISPER.lock().unwrap();
                if let Some((id, ctx)) = cache.as_ref()
                    && id == m.id
                {
                    return Ok(Engine::Whisper(ctx.clone()));
                }
                let path = m.path();
                let ctx =
                    WhisperContext::new_with_params(&path, WhisperContextParameters::default())
                        .map_err(|e| anyhow!("could not load {}: {e}", path.display()))?;
                let ctx = Arc::new(ctx);
                *cache = Some((m.id.to_string(), ctx.clone()));
                Ok(Engine::Whisper(ctx))
            }
            AsrChoice::QwenLocal => {
                let m = QwenModel::find(&s.qwen_local_model)
                    .ok_or_else(|| anyhow!("unknown Qwen3-ASR model {}", s.qwen_local_model))?;
                if !m.is_downloaded() {
                    bail!("the Qwen3-ASR {} model is not downloaded yet", m.label);
                }
                let mut cache = QWEN.lock().unwrap();
                if let Some((id, q)) = cache.as_ref()
                    && id == m.id
                {
                    return Ok(Engine::Qwen(q.clone()));
                }
                cache.take();
                let q = Arc::new(Qwen::load(m)?);
                *cache = Some((m.id.to_string(), q.clone()));
                Ok(Engine::Qwen(q))
            }
            AsrChoice::Qwen => Ok(Engine::Remote {
                url: s.qwen_url.trim().trim_end_matches('/').to_string(),
                model: s.qwen_model.trim().to_string(),
                key: s.qwen_key.trim().to_string(),
            }),
        }
    }

    /// How often live captioning re-reads the line in progress; the slower
    /// engines get more room.
    pub fn partial_every_ms(&self) -> u64 {
        match self {
            Engine::Whisper(_) => 900,
            Engine::Qwen(_) => 1400,
            Engine::Remote { .. } => 1800,
        }
    }

    /// Loads the model or reaches the server with a second of silence.
    pub fn test(s: &Settings) -> Result<String> {
        let engine = Engine::load(s)?;
        let silence = vec![0.0f32; SAMPLE_RATE];
        match &engine {
            Engine::Whisper(_) => {
                engine.transcribe(&silence, "en", true, None)?;
                Ok(format!(
                    "Whisper {} is loaded and listening.",
                    s.whisper_model
                ))
            }
            Engine::Qwen(_) => {
                let t = std::time::Instant::now();
                engine.transcribe(&silence, "en", true, None)?;
                Ok(format!(
                    "Qwen3-ASR {} is loaded; a second of audio took {} ms.",
                    s.qwen_local_model.to_uppercase(),
                    t.elapsed().as_millis()
                ))
            }
            Engine::Remote { url, model, .. } => {
                engine.transcribe(&silence, "en", true, None)?;
                Ok(format!("{model} answered at {url}."))
            }
        }
    }

    /// Transcribes one stretch of 16 kHz mono audio. Times are relative to its start.
    pub fn transcribe(
        &self,
        pcm: &[f32],
        lang: &str,
        live: bool,
        progress: Option<Progress>,
    ) -> Result<Vec<AsrSeg>> {
        if pcm.len() < SAMPLE_RATE / 4 {
            return Ok(Vec::new());
        }
        match self {
            Engine::Whisper(ctx) => whisper(ctx, pcm, lang, live, progress),
            Engine::Qwen(q) => {
                let text = q.transcribe(pcm, lang)?;
                let dur = (pcm.len() / (SAMPLE_RATE / 1000)) as i64;
                Ok(if text.is_empty() || is_noise_caption(&text) {
                    vec![]
                } else {
                    vec![AsrSeg {
                        start_ms: 0,
                        end_ms: dur,
                        text,
                    }]
                })
            }
            Engine::Remote { url, model, key } => remote(url, model, key, pcm, lang),
        }
    }

    /// A whole file. Whisper handles long audio itself; the others get it in
    /// utterances cut at pauses, which also gives every piece a real timestamp.
    pub fn transcribe_long(
        &self,
        pcm: &[f32],
        lang: &str,
        progress: Progress,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<AsrSeg>> {
        let segs = match self {
            Engine::Whisper(_) => self.transcribe(pcm, lang, false, Some(progress))?,
            Engine::Qwen(_) | Engine::Remote { .. } => {
                let mut out = Vec::new();
                let pieces = split_utterances(pcm, 12_000);
                let total: usize = pieces.iter().map(|(a, b)| b - a).sum::<usize>().max(1);
                let mut done = 0;
                for (a, b) in &pieces {
                    if cancelled() {
                        bail!("cancelled");
                    }
                    let offset = (*a / (SAMPLE_RATE / 1000)) as i64;
                    for mut s in self.transcribe(&pcm[*a..*b], lang, false, None)? {
                        s.start_ms += offset;
                        s.end_ms += offset;
                        s.end_ms = s
                            .end_ms
                            .min(offset + ((b - a) / (SAMPLE_RATE / 1000)) as i64);
                        out.push(s);
                    }
                    done += b - a;
                    progress(done as f32 / total as f32);
                }
                out
            }
        };
        Ok(resegment(segs, is_cjk(lang)))
    }
}

fn whisper_lang(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "en" => "en",
        "zh" | "zh-Hant" => "zh",
        "ja" => "ja",
        "ko" => "ko",
        "de" => "de",
        "es" => "es",
        "fr" => "fr",
        _ => return None,
    })
}

fn whisper(
    ctx: &WhisperContext,
    pcm: &[f32],
    lang: &str,
    live: bool,
    progress: Option<Progress>,
) -> Result<Vec<AsrSeg>> {
    let mut state = ctx.create_state().map_err(|e| anyhow!("whisper: {e}"))?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    params.set_n_threads(threads as i32);
    params.set_language(whisper_lang(lang));
    params.set_translate(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_no_context(live);
    if lang == "zh" {
        params.set_initial_prompt("以下是普通话的句子，使用简体中文。");
    } else if lang == "zh-Hant" {
        params.set_initial_prompt("以下是普通話的句子，使用繁體中文。");
    }
    if live {
        params.set_single_segment(false);
    }
    if let Some(p) = progress {
        params.set_progress_callback_safe(move |pct: i32| p(pct as f32 / 100.0));
    }
    state
        .full(params, pcm)
        .map_err(|e| anyhow!("whisper: {e}"))?;
    let mut out = Vec::new();
    for seg in state.as_iter() {
        let text = seg
            .to_str_lossy()
            .map(|t| t.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() || seg.no_speech_probability() > 0.6 || is_noise_caption(&text) {
            continue;
        }
        out.push(AsrSeg {
            start_ms: seg.start_timestamp() * 10,
            end_ms: seg.end_timestamp() * 10,
            text,
        });
    }
    Ok(out)
}

/// Whisper writes these over music and silence.
fn is_noise_caption(t: &str) -> bool {
    let t = t.trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace());
    let l = t.to_lowercase();
    (t.starts_with('[') || t.starts_with('(') || t.starts_with('♪'))
        || matches!(
            l.as_str(),
            "" | "music" | "applause" | "silence" | "blank_audio"
        )
}

fn remote(url: &str, model: &str, key: &str, pcm: &[f32], lang: &str) -> Result<Vec<AsrSeg>> {
    let boundary = format!("taurus{:x}", crate::data::now_ms());
    let mut body = Vec::new();
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    };
    field("model", model);
    field("response_format", "verbose_json");
    if let Some(l) = whisper_lang(lang) {
        field("language", l);
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&wav(pcm));
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let mut req = agent(Duration::from_secs(300))
        .post(format!("{url}/audio/transcriptions"))
        .content_type(format!("multipart/form-data; boundary={boundary}"));
    if !key.is_empty() {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req
        .send(&body[..])
        .with_context(|| format!("contacting {url}"))?;
    let mut resp = check(resp)?;
    let text = resp.body_mut().read_to_string()?;
    let dur = (pcm.len() / (SAMPLE_RATE / 1000)) as i64;
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            let t = text.trim().to_string();
            return Ok(if t.is_empty() {
                vec![]
            } else {
                vec![AsrSeg {
                    start_ms: 0,
                    end_ms: dur,
                    text: t,
                }]
            });
        }
    };
    let mut out = Vec::new();
    if let Some(segs) = v["segments"].as_array().filter(|s| !s.is_empty()) {
        for s in segs {
            let text = s["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                continue;
            }
            let ms = |k: &str| (s[k].as_f64().unwrap_or(0.0) * 1000.0) as i64;
            out.push(AsrSeg {
                start_ms: ms("start"),
                end_ms: ms("end").max(ms("start") + 300),
                text,
            });
        }
    } else {
        let text = v["text"].as_str().unwrap_or("").trim().to_string();
        let text = strip_language_tag(&text);
        if !text.is_empty() {
            out.push(AsrSeg {
                start_ms: 0,
                end_ms: dur,
                text,
            });
        }
    }
    Ok(out)
}

/// Qwen3-ASR can prefix its text with `language English<asr_text>`.
fn strip_language_tag(t: &str) -> String {
    match t.find("<asr_text>") {
        Some(i) => t[i + 10..].trim().to_string(),
        None => t.trim().to_string(),
    }
}

fn wav(pcm: &[f32]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE as u32).to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE as u32 * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    out
}

pub fn rms(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt()
}

/// Cuts audio into pieces of at most `max_ms`, preferring the quietest
/// 100 ms window in each piece's last third.
pub fn split_at_pauses(pcm: &[f32], max_ms: usize) -> Vec<(usize, usize)> {
    let max = max_ms * SAMPLE_RATE / 1000;
    let win = SAMPLE_RATE / 10;
    let mut out = Vec::new();
    let mut start = 0;
    while start < pcm.len() {
        let end = (start + max).min(pcm.len());
        if end == pcm.len() {
            out.push((start, end));
            break;
        }
        let from = start + max * 2 / 3;
        let mut best = (f32::MAX, end);
        let mut i = from;
        while i + win <= end {
            let e = rms(&pcm[i..i + win]);
            if e < best.0 {
                best = (e, i + win / 2);
            }
            i += win / 2;
        }
        out.push((start, best.1));
        start = best.1;
    }
    out
}

/// Stretches of speech between pauses, silence trimmed, each at most
/// `max_ms`. Short phrases close together are kept as one, so a subtitle
/// is not a single word.
pub fn split_utterances(pcm: &[f32], max_ms: usize) -> Vec<(usize, usize)> {
    const FRAME: usize = SAMPLE_RATE * 30 / 1000;
    const GAP: usize = 12;
    const PAD: usize = 5;
    let levels: Vec<f32> = pcm.chunks(FRAME).map(rms).collect();
    if levels.is_empty() {
        return Vec::new();
    }
    let mut sorted = levels.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[sorted.len() / 5];
    let threshold = (floor * 3.0).max(0.004);

    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (i, _) in levels.iter().enumerate().filter(|(_, l)| **l > threshold) {
        match runs.last_mut() {
            Some(r) if i - r.1 <= GAP => r.1 = i + 1,
            _ => runs.push((i, i + 1)),
        }
    }
    let max_frames = max_ms * SAMPLE_RATE / 1000 / FRAME;
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in runs {
        match merged.last_mut() {
            Some(m) if m.1 - m.0 < 50 && r.0 - m.1 < 27 && r.1 - m.0 <= max_frames => m.1 = r.1,
            _ => merged.push(r),
        }
    }

    let mut out = Vec::new();
    for (a, b) in merged {
        let start = a.saturating_sub(PAD) * FRAME;
        let end = ((b + PAD) * FRAME).min(pcm.len());
        if end - start < SAMPLE_RATE / 4 {
            continue;
        }
        for (x, y) in split_at_pauses(&pcm[start..end], max_ms) {
            out.push((start + x, start + y));
        }
    }
    out
}

const MAX_CUE_MS: i64 = 6500;

/// Recognisers write long, multi-sentence segments. Subtitles want one or two
/// short lines, so long segments are split at punctuation, sharing their time
/// by length.
pub fn resegment(segs: Vec<AsrSeg>, cjk: bool) -> Vec<AsrSeg> {
    let max_chars = if cjk { 32 } else { 84 };
    let mut out = Vec::new();
    for s in segs {
        split_into(s, cjk, max_chars, &mut out);
    }
    out
}

fn split_into(s: AsrSeg, cjk: bool, max_chars: usize, out: &mut Vec<AsrSeg>) {
    let chars: Vec<char> = s.text.chars().collect();
    let long = chars.len() > max_chars
        || s.end_ms - s.start_ms > MAX_CUE_MS && chars.len() > max_chars / 3;
    if !long || chars.len() < 8 {
        out.push(s);
        return;
    }
    let mid = chars.len() / 2;
    let strong = |c: char| matches!(c, '.' | '?' | '!' | '。' | '？' | '！' | '；' | ';');
    let weak = |c: char| matches!(c, ',' | '，' | '、' | ':' | '：' | '—');
    let mut best: Option<(usize, usize)> = None;
    for (i, &c) in chars
        .iter()
        .enumerate()
        .skip(2)
        .take(chars.len().saturating_sub(4))
    {
        let rank = if strong(c) {
            0
        } else if weak(c) {
            1
        } else if !cjk && c == ' ' {
            2
        } else {
            continue;
        };
        let score = rank * chars.len() + i.abs_diff(mid);
        if best.is_none_or(|(b, _)| score < b) {
            best = Some((score, i + 1));
        }
    }
    let cut = match best {
        Some((_, i)) => i,
        None if cjk => mid,
        None => return out.push(s),
    };
    let left: String = chars[..cut].iter().collect();
    let right: String = chars[cut..].iter().collect();
    let (left, right) = (left.trim().to_string(), right.trim().to_string());
    if left.is_empty() || right.is_empty() {
        out.push(s);
        return;
    }
    let total = (left.chars().count() + right.chars().count()) as f64;
    let at =
        s.start_ms + ((s.end_ms - s.start_ms) as f64 * left.chars().count() as f64 / total) as i64;
    split_into(
        AsrSeg {
            start_ms: s.start_ms,
            end_ms: at,
            text: left,
        },
        cjk,
        max_chars,
        out,
    );
    split_into(
        AsrSeg {
            start_ms: at,
            end_ms: s.end_ms,
            text: right,
        },
        cjk,
        max_chars,
        out,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_long_segments_at_punctuation() {
        let s = AsrSeg {
            start_ms: 0,
            end_ms: 10_000,
            text: "This is the first sentence of a long stretch. And this is the second one, which keeps going for a while longer.".into(),
        };
        let out = resegment(vec![s], false);
        assert!(out.len() >= 2);
        assert!(out[0].text.ends_with('.'));
        assert_eq!(out.last().unwrap().end_ms, 10_000);
    }

    #[test]
    fn pauses_bound_pieces() {
        let pcm = vec![0.1f32; SAMPLE_RATE * 50];
        let pieces = split_at_pauses(&pcm, 20_000);
        assert!(pieces.len() >= 3);
        assert_eq!(pieces.last().unwrap().1, pcm.len());
    }

    #[test]
    fn utterances_follow_pauses() {
        let tone = |ms: usize| {
            (0..ms * 16)
                .map(|i| (i as f32 * 0.05).sin() * 0.3)
                .collect::<Vec<f32>>()
        };
        let quiet = |ms: usize| vec![0.0005f32; ms * 16];
        let pcm = [quiet(1000), tone(2000), quiet(1200), tone(3000), quiet(800)].concat();
        let u = split_utterances(&pcm, 12_000);
        assert_eq!(u.len(), 2);
        assert!((u[0].0 as i64 - 16_000).abs() < 4000);
        assert!((u[1].1 as i64 - 7200 * 16).abs() < 4000);
    }

    #[test]
    fn wav_header_is_sized() {
        let w = wav(&[0.0; 160]);
        assert_eq!(w.len(), 44 + 320);
    }
}
