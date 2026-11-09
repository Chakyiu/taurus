//! Qwen3-ASR on this Mac, through ONNX Runtime. The export (andrewleech's
//! qwen3-asr-onnx) splits the model into an audio encoder, a prefill decoder
//! that splices the audio features into the prompt, and a one-token decoder
//! that carries the KV cache; both decoders share int4 weights.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use anyhow::{Context as _, Result, anyhow, bail};
use half::f16;
use ort::session::Session;
use ort::session::builder::{AutoDevicePolicy, GraphOptimizationLevel};
use ort::value::{DynValue, Tensor};

use super::mel::Mel;
use crate::store;

pub struct QwenModel {
    pub id: &'static str,
    pub label: &'static str,
    dir: &'static str,
    repo: &'static str,
    pub size: &'static str,
    pub note: &'static str,
}

pub const QWEN_MODELS: &[QwenModel] = &[
    QwenModel {
        id: "0.6b",
        label: "0.6B",
        dir: "qwen3-asr-0.6b",
        repo: "andrewleech/qwen3-asr-0.6b-onnx",
        size: "2.0 GB",
        note: "fast, good for live",
    },
    QwenModel {
        id: "1.7b",
        label: "1.7B",
        dir: "qwen3-asr-1.7b",
        repo: "andrewleech/qwen3-asr-1.7b-onnx",
        size: "4.0 GB",
        note: "most accurate",
    },
];

const FILES: &[&str] = &[
    "config.json",
    "tokenizer.json",
    "decoder_init.int4.onnx",
    "decoder_step.int4.onnx",
    "embed_tokens.bin",
    "encoder.int4.onnx",
    "decoder_weights.int4.data",
];

impl QwenModel {
    pub fn find(id: &str) -> Option<&'static QwenModel> {
        QWEN_MODELS.iter().find(|m| m.id == id)
    }

    /// Download ids share one map with Whisper's, so they carry a prefix.
    pub fn download_id(&self) -> String {
        format!("qwen-{}", self.id)
    }

    pub fn from_download_id(id: &str) -> Option<&'static QwenModel> {
        id.strip_prefix("qwen-").and_then(QwenModel::find)
    }

    pub fn dir(&self) -> PathBuf {
        store::models_dir().join(self.dir)
    }

    pub fn is_downloaded(&self) -> bool {
        FILES.iter().all(|f| self.dir().join(f).is_file())
    }

    /// Fetches whichever files are missing. Sizes come from the Hub first, so
    /// progress covers the whole set rather than restarting per file.
    pub fn download(&self, progress: impl Fn(f32), cancel: &AtomicBool) -> Result<()> {
        let dir = self.dir();
        let missing: Vec<&str> = FILES
            .iter()
            .copied()
            .filter(|f| !dir.join(f).is_file())
            .collect();
        let sizes = self.remote_sizes().unwrap_or_default();
        let total: u64 = missing
            .iter()
            .map(|f| sizes.get(*f).copied().unwrap_or(0))
            .sum();
        let mut before = 0u64;
        for f in missing {
            let url = format!("https://huggingface.co/{}/resolve/main/{f}", self.repo);
            super::fetch(
                &url,
                &dir.join(f),
                &mut |done, _| {
                    if total > 0 {
                        progress((before + done) as f32 / total as f32);
                    }
                },
                cancel,
            )
            .with_context(|| format!("downloading {f}"))?;
            before += sizes.get(f).copied().unwrap_or(0);
        }
        Ok(())
    }

    fn remote_sizes(&self) -> Result<HashMap<String, u64>> {
        let url = format!("https://huggingface.co/api/models/{}/tree/main", self.repo);
        let mut resp = super::check(
            super::agent(std::time::Duration::from_secs(20))
                .get(&url)
                .call()?,
        )?;
        let list: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e["path"].as_str()?.to_string(), e["size"].as_u64()?)))
            .collect())
    }
}

const ENDOFTEXT: i64 = 151643;
const IM_START: i64 = 151644;
const IM_END: i64 = 151645;
const AUDIO_START: i64 = 151669;
const AUDIO_END: i64 = 151670;
const AUDIO_PAD: i64 = 151676;
const ASR_TEXT: i64 = 151704;
const NEWLINE: i64 = 198;
const SYSTEM: i64 = 9125;
const USER: i64 = 882;
const ASSISTANT: i64 = 77091;
/// Everything from here up is a control token, never text.
const FIRST_SPECIAL: i64 = 151643;

pub struct Qwen {
    inner: Mutex<Inner>,
    embed: Vec<f16>,
    hidden: usize,
    tokens: Vec<Vec<u8>>,
    vocab: HashMap<String, i64>,
    mel: Mel,
}

struct Inner {
    encoder: Session,
    init: Session,
    step: Session,
}

impl Qwen {
    pub fn load(m: &QwenModel) -> Result<Qwen> {
        let dir = m.dir();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("config.json"))?)?;
        let hidden = config["decoder"]["hidden_size"]
            .as_u64()
            .ok_or_else(|| anyhow!("config.json has no hidden size"))?
            as usize;
        let n_mels = config["mel"]["n_mels"].as_u64().unwrap_or(128) as usize;

        let raw = std::fs::read(dir.join("embed_tokens.bin"))?;
        let embed: Vec<f16> = raw
            .chunks_exact(2)
            .map(|b| f16::from_le_bytes([b[0], b[1]]))
            .collect();
        if embed.len() % hidden != 0 {
            bail!("embed_tokens.bin does not match the model width");
        }

        let tok: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("tokenizer.json"))?)?;
        let unbyte = byte_decoder();
        let mut vocab = HashMap::new();
        let mut tokens = vec![Vec::new(); embed.len() / hidden];
        let entries = tok["model"]["vocab"]
            .as_object()
            .ok_or_else(|| anyhow!("tokenizer.json has no vocabulary"))?;
        for (piece, id) in entries {
            let Some(id) = id.as_i64() else { continue };
            if let Some(slot) = tokens.get_mut(id as usize) {
                *slot = piece
                    .chars()
                    .filter_map(|c| unbyte.get(&c).copied())
                    .collect();
            }
            vocab.insert(piece.clone(), id);
        }

        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        // Core ML cannot compile these graphs' int4 matmuls and slices.
        let env = super::ort_env()?;
        let session = |file: &str| -> Result<Session> {
            let fail = |e: &dyn std::fmt::Display| anyhow!("could not load {file}: {e}");
            Session::builder(&env)
                .map_err(|e| fail(&e))?
                .with_auto_device(AutoDevicePolicy::PreferCPU)
                .map_err(|e| fail(&e))?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(|e| fail(&e))?
                .with_intra_threads(threads)
                .map_err(|e| fail(&e))?
                .commit_from_file(dir.join(file))
                .map_err(|e| fail(&e))
        };
        let inner = Inner {
            encoder: session("encoder.int4.onnx")?,
            init: session("decoder_init.int4.onnx")?,
            step: session("decoder_step.int4.onnx")?,
        };
        Ok(Qwen {
            inner: Mutex::new(inner),
            embed,
            hidden,
            tokens,
            vocab,
            mel: Mel::new(n_mels, 16000.0, 8000.0),
        })
    }

    /// Transcribes one stretch of 16 kHz audio into plain text.
    pub fn transcribe(&self, pcm: &[f32], lang: &str) -> Result<String> {
        let (mel, frames) = self.mel.compute(pcm);
        let mut inner = self.inner.lock().unwrap();
        let Inner {
            encoder,
            init,
            step,
        } = &mut *inner;

        let mut out = encoder.run(
            ort::inputs!["mel" => Tensor::from_array(([1usize, self.mel.n_mels(), frames], mel))?],
        )?;
        let features: DynValue = out
            .remove("audio_features")
            .ok_or_else(|| anyhow!("the encoder gave no features"))?;
        drop(out);
        let audio_len = features.shape()[1] as usize;

        let mut prompt = vec![
            IM_START,
            SYSTEM,
            NEWLINE,
            IM_END,
            NEWLINE,
            IM_START,
            USER,
            NEWLINE,
            AUDIO_START,
        ];
        let audio_offset = prompt.len() as i64;
        prompt.extend(std::iter::repeat_n(AUDIO_PAD, audio_len));
        prompt.extend([AUDIO_END, IM_END, NEWLINE, IM_START, ASSISTANT, NEWLINE]);
        let forced = self.language_prefix(lang);
        let is_forced = forced.is_some();
        prompt.extend(forced.unwrap_or_default());

        let n = prompt.len();
        let positions: Vec<i64> = (0..n as i64).collect();
        let mut out = init.run(ort::inputs![
            "input_ids" => Tensor::from_array(([1usize, n], prompt))?,
            "position_ids" => Tensor::from_array(([1usize, n], positions))?,
            "audio_features" => features,
            "audio_offset" => Tensor::from_array(([1usize], vec![audio_offset]))?,
        ])?;
        let mut next = argmax_last(&out.remove("logits").ok_or_else(|| anyhow!("no logits"))?)?;
        let mut keys = out
            .remove("present_keys")
            .ok_or_else(|| anyhow!("no KV cache"))?;
        let mut values = out
            .remove("present_values")
            .ok_or_else(|| anyhow!("no KV cache"))?;
        drop(out);

        // Speech runs well under ten tokens a second; anything past this is a loop.
        let budget = 24 + pcm.len() / 16000 * 10;
        let mut generated = Vec::new();
        let mut pos = n as i64;
        while next != ENDOFTEXT && next != IM_END && generated.len() < budget {
            generated.push(next);
            let row = next as usize * self.hidden;
            let emb: Vec<f32> = self.embed[row..row + self.hidden]
                .iter()
                .map(|v| v.to_f32())
                .collect();
            let mut out = step.run(ort::inputs![
                "input_embeds" => Tensor::from_array(([1usize, 1, self.hidden], emb))?,
                "position_ids" => Tensor::from_array(([1usize, 1], vec![pos]))?,
                "past_keys" => keys,
                "past_values" => values,
            ])?;
            next = argmax_last(&out.remove("logits").ok_or_else(|| anyhow!("no logits"))?)?;
            keys = out
                .remove("present_keys")
                .ok_or_else(|| anyhow!("no KV cache"))?;
            values = out
                .remove("present_values")
                .ok_or_else(|| anyhow!("no KV cache"))?;
            pos += 1;
        }
        Ok(self.text(&generated, is_forced))
    }

    /// `language English<asr_text>` pins the language. Skipped when the
    /// name is not a single token, letting the model detect it instead.
    fn language_prefix(&self, lang: &str) -> Option<Vec<i64>> {
        let name = match lang {
            "en" => "English",
            "zh" | "zh-Hant" => "Chinese",
            "yue" => "Cantonese",
            "ja" => "Japanese",
            "ko" => "Korean",
            "de" => "German",
            "fr" => "French",
            "es" => "Spanish",
            "pt" => "Portuguese",
            "it" => "Italian",
            "ru" => "Russian",
            _ => return None,
        };
        let word = *self.vocab.get("language")?;
        let name = *self.vocab.get(&format!("\u{120}{name}"))?;
        Some(vec![word, name, ASR_TEXT])
    }

    /// The model writes `language X<asr_text>` before the words unless the
    /// prompt already did.
    fn text(&self, ids: &[i64], forced: bool) -> String {
        let body = match (forced, ids.iter().position(|&t| t == ASR_TEXT)) {
            (false, Some(i)) => &ids[i + 1..],
            _ => ids,
        };
        let bytes: Vec<u8> = body
            .iter()
            .filter(|&&t| t < FIRST_SPECIAL)
            .flat_map(|&t| self.tokens.get(t as usize).cloned().unwrap_or_default())
            .collect();
        String::from_utf8_lossy(&bytes).trim().to_string()
    }
}

fn argmax_last(logits: &DynValue) -> Result<i64> {
    let (shape, data) = logits.try_extract_tensor::<f32>()?;
    let vocab = *shape.last().ok_or_else(|| anyhow!("empty logits"))? as usize;
    let last = &data[data.len() - vocab..];
    let (i, _) =
        last.iter().enumerate().fold(
            (0, f32::MIN),
            |best, (i, &v)| if v > best.1 { (i, v) } else { best },
        );
    Ok(i as i64)
}

/// GPT-2's byte-level BPE stores each byte as a printable character.
fn byte_decoder() -> HashMap<char, u8> {
    let mut bytes: Vec<u32> = (33..=126).chain(161..=172).chain(174..=255).collect();
    let mut chars = bytes.clone();
    let mut n = 0;
    for b in 0..256 {
        if !bytes.contains(&b) {
            bytes.push(b);
            chars.push(256 + n);
            n += 1;
        }
    }
    chars
        .into_iter()
        .zip(bytes)
        .filter_map(|(c, b)| Some((char::from_u32(c)?, b as u8)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_decoder_covers_every_byte() {
        let d = byte_decoder();
        assert_eq!(d.len(), 256);
        assert_eq!(d[&'\u{120}'], b' ');
        assert_eq!(d[&'a'], b'a');
    }
}
