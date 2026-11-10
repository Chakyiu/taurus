//! The engines behind the workbench: speech recognition (Whisper or Qwen3-ASR
//! on this Mac, or a Qwen3-ASR server) and translation (any OpenAI-compatible
//! chat API).

pub mod asr;
mod mel;
pub mod mt;
pub mod qwen;
pub mod speaker;

pub use qwen::{QWEN_MODELS, QwenModel};

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::store;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AsrChoice {
    Whisper,
    QwenLocal,
    /// Qwen3-ASR behind a server; the name predates the local option.
    Qwen,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub asr: AsrChoice,
    pub whisper_model: String,
    pub qwen_local_model: String,
    pub qwen_url: String,
    pub qwen_model: String,
    pub qwen_key: String,
    pub mt_url: String,
    pub mt_model: String,
    pub mt_key: String,
    /// Whether new projects are translated; the switch remembers its last use.
    pub translate: bool,
    /// Language code for summaries. Empty means the project's reading language.
    pub summary_lang: String,
    /// Chat model for summaries. Empty means the translation model.
    pub summary_model: String,
    /// A longer summary instead of a few sentences.
    pub summary_long: bool,
    /// How much a summary may reason. Medium when an older settings file has none.
    #[serde(default = "medium_think")]
    pub summary_think: mt::Think,
    /// How much a correction may reason.
    pub correct_think: mt::Think,
    /// Named capture setups, offered at the top of the live caption dialog.
    pub profiles: Vec<CaptureProfile>,
    /// The profile picked last. Empty for none.
    pub profile: String,
    /// The choices of the last capture started, which the dialog opens with.
    pub last_capture: Option<CaptureProfile>,
    /// Look for a new release once a day and install it in the background.
    pub auto_update: bool,
    /// When a new release was last looked for, in seconds since 1970.
    pub update_checked: i64,
}

/// What to listen to and record, saved under a name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureProfile {
    pub name: String,
    /// `both`, `system` or `mic`.
    pub source: String,
    pub sound_from: String,
    pub microphone: String,
    pub screen: String,
    /// A `capture::VideoQuality` key.
    pub quality: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            asr: AsrChoice::Whisper,
            whisper_model: "base".into(),
            qwen_local_model: "1.7b".into(),
            qwen_url: "http://localhost:8000/v1".into(),
            qwen_model: "Qwen/Qwen3-ASR-1.7B".into(),
            qwen_key: String::new(),
            mt_url: "http://localhost:11434/v1".into(),
            mt_model: "qwen3:8b".into(),
            mt_key: String::new(),
            translate: false,
            summary_lang: String::new(),
            summary_model: String::new(),
            summary_long: false,
            summary_think: mt::Think::Medium,
            correct_think: mt::Think::Off,
            profiles: Vec::new(),
            profile: String::new(),
            last_capture: None,
            auto_update: true,
            update_checked: 0,
        }
    }
}

impl Settings {
    pub fn mt(&self) -> mt::MtConfig {
        mt::MtConfig {
            url: self.mt_url.trim().trim_end_matches('/').to_string(),
            model: self.mt_model.trim().to_string(),
            key: self.mt_key.trim().to_string(),
        }
    }

    pub fn mt_ready(&self) -> bool {
        !self.mt_url.trim().is_empty() && !self.mt_model.trim().is_empty()
    }

    pub fn asr_ready(&self) -> bool {
        match self.asr {
            AsrChoice::Whisper => {
                WhisperModel::find(&self.whisper_model).is_some_and(|m| m.is_downloaded())
            }
            AsrChoice::QwenLocal => {
                QwenModel::find(&self.qwen_local_model).is_some_and(|m| m.is_downloaded())
            }
            AsrChoice::Qwen => {
                !self.qwen_url.trim().is_empty() && !self.qwen_model.trim().is_empty()
            }
        }
    }

    /// One line for the sidebar card, e.g. "Whisper base · qwen3:8b".
    pub fn summary(&self) -> String {
        let asr = match self.asr {
            AsrChoice::Whisper => format!("Whisper {}", self.whisper_model),
            AsrChoice::QwenLocal => format!("Qwen3-ASR {}", self.qwen_local_model.to_uppercase()),
            AsrChoice::Qwen => short_model(&self.qwen_model),
        };
        if self.translate && self.mt_ready() {
            format!("{asr} · {}", short_model(&self.mt_model))
        } else {
            asr
        }
    }
}

/// ONNX Runtime allows one environment per process, ever, so every model
/// shares this one.
pub fn ort_env() -> Result<ort::environment::Environment> {
    static ENV: std::sync::Mutex<Option<ort::environment::Environment>> =
        std::sync::Mutex::new(None);
    let mut env = ENV.lock().unwrap();
    if env.is_none() {
        *env = Some(
            ort::init()
                .with_name("taurus")
                .build()
                .map_err(|e| anyhow::anyhow!("ONNX Runtime: {e}"))?,
        );
    }
    Ok(env.clone().unwrap())
}

/// How a download id reads in the interface, e.g. "Qwen3-ASR 1.7B".
pub fn model_name(download_id: &str) -> String {
    if download_id == speaker::SPEAKER_DOWNLOAD_ID {
        return "the speaker model".into();
    }
    match QwenModel::from_download_id(download_id) {
        Some(m) => format!("Qwen3-ASR {}", m.label),
        None => format!("Whisper {download_id}"),
    }
}

fn short_model(m: &str) -> String {
    m.rsplit('/').next().unwrap_or(m).to_string()
}

fn medium_think() -> mt::Think {
    mt::Think::Medium
}

pub struct WhisperModel {
    pub id: &'static str,
    pub file: &'static str,
    pub size: &'static str,
    pub note: &'static str,
}

pub const WHISPER_MODELS: &[WhisperModel] = &[
    WhisperModel {
        id: "tiny",
        file: "ggml-tiny.bin",
        size: "75 MB",
        note: "fastest, rough",
    },
    WhisperModel {
        id: "base",
        file: "ggml-base.bin",
        size: "142 MB",
        note: "fast, good for live",
    },
    WhisperModel {
        id: "small",
        file: "ggml-small.bin",
        size: "466 MB",
        note: "balanced",
    },
    WhisperModel {
        id: "large-v3-turbo-q5",
        file: "ggml-large-v3-turbo-q5_0.bin",
        size: "547 MB",
        note: "accurate, quantised",
    },
    WhisperModel {
        id: "large-v3-turbo",
        file: "ggml-large-v3-turbo.bin",
        size: "1.6 GB",
        note: "most accurate",
    },
];

impl WhisperModel {
    pub fn find(id: &str) -> Option<&'static WhisperModel> {
        WHISPER_MODELS.iter().find(|m| m.id == id)
    }

    pub fn path(&self) -> PathBuf {
        store::models_dir().join(self.file)
    }

    pub fn is_downloaded(&self) -> bool {
        std::fs::metadata(self.path()).is_ok_and(|m| m.len() > 1 << 20)
    }

    fn url(&self) -> String {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            self.file
        )
    }

    pub fn download(&self, progress: impl Fn(f32), cancel: &AtomicBool) -> Result<()> {
        let mut total = 0;
        fetch(
            &self.url(),
            &self.path(),
            &mut |done, len| {
                total = len;
                if total > 0 {
                    progress(done as f32 / total as f32);
                }
            },
            cancel,
        )
    }
}

/// Streams `url` to a temporary file, then moves it into place, so a
/// cancelled download never leaves a truncated file behind. `on_bytes` gets
/// the bytes so far and the length the server announced (0 if none).
pub(crate) fn fetch(
    url: &str,
    dest: &std::path::Path,
    on_bytes: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .build()
        .into();
    let mut resp = agent
        .get(url)
        .call()
        .with_context(|| format!("contacting {}", url.split('/').nth(2).unwrap_or(url)))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    let tmp = dest.with_file_name(name);
    let mut file = std::fs::File::create(&tmp)?;
    let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(&tmp);
            bail!("cancelled");
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        on_bytes(done, total);
    }
    file.flush()?;
    drop(file);
    if total > 0 && done < total {
        let _ = std::fs::remove_file(&tmp);
        bail!("the download ended early");
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

pub(crate) fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

/// For replies that take as long as they take: only reaching the server is
/// timed, and the caller stops the rest when it wants to.
pub(crate) fn untimed_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Turns a non-2xx response into an error carrying the server's own message.
pub(crate) fn check(
    mut resp: ureq::http::Response<ureq::Body>,
) -> Result<ureq::http::Response<ureq::Body>> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    let msg = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or(v["error"].as_str())
                .or(v["detail"].as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    bail!("{} {}", status.as_u16(), msg.trim())
}

pub(crate) fn friendly(e: &anyhow::Error, url: &str) -> String {
    let text = format!("{e:#}");
    if text.contains("Connection refused")
        || text.contains("ConnectionFailed")
        || text.contains("connect")
    {
        format!("nothing is answering at {url}")
    } else {
        text
    }
}
