//! Reading videos through ffprobe and ffmpeg. Apps started from Finder do not
//! inherit the shell's PATH, so the usual Homebrew locations are searched too.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::Value;

use crate::subs;

pub const SAMPLE_RATE: usize = 16_000;

pub fn tool(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

fn need(name: &str) -> Result<PathBuf> {
    tool(name)
        .ok_or_else(|| anyhow!("{name} is not installed. Install it with `brew install ffmpeg`"))
}

#[derive(Clone, Debug)]
pub struct SubTrack {
    pub stream: usize,
    pub codec: String,
    pub lang: Option<String>,
    pub title: Option<String>,
    /// Image subtitles (PGS, VobSub) cannot be read as text.
    pub text: bool,
    pub cues: Option<usize>,
    pub span_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct AudioTrack {
    pub stream: usize,
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub lang: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub duration_ms: i64,
    pub video: Option<String>,
    pub subs: Vec<SubTrack>,
    pub audio: Vec<AudioTrack>,
}

const TEXT_SUBS: &[&str] = &[
    "subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text", "tx3g",
];

pub fn probe(path: &Path) -> Result<Probe> {
    let out = Command::new(need("ffprobe")?)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
        ])
        .arg(path)
        .output()
        .context("running ffprobe")?;
    if !out.status.success() {
        bail!(
            "{}",
            first_line(&out.stderr).unwrap_or("ffprobe could not read this file".into())
        );
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("reading ffprobe output")?;
    let mut probe = Probe {
        duration_ms: v["format"]["duration"]
            .as_str()
            .and_then(|d| d.parse::<f64>().ok())
            .map(|d| (d * 1000.0) as i64)
            .unwrap_or(0),
        ..Default::default()
    };
    for s in v["streams"].as_array().into_iter().flatten() {
        let stream = s["index"].as_u64().unwrap_or(0) as usize;
        let codec = s["codec_name"].as_str().unwrap_or("unknown").to_string();
        let tag = |k: &str| s["tags"][k].as_str().map(String::from);
        match s["codec_type"].as_str() {
            Some("video") if s["disposition"]["attached_pic"].as_u64() != Some(1) => {
                if probe.video.is_none() {
                    let w = s["width"].as_u64().unwrap_or(0);
                    let h = s["height"].as_u64().unwrap_or(0);
                    probe.video = Some(format!("{} {w}×{h}", video_codec_name(&codec)));
                }
            }
            Some("audio") => probe.audio.push(AudioTrack {
                stream,
                codec,
                sample_rate: s["sample_rate"].as_str().and_then(|r| r.parse().ok()),
                channels: s["channels"].as_u64().map(|c| c as u32),
                lang: tag("language"),
            }),
            Some("subtitle") => probe.subs.push(SubTrack {
                stream,
                text: TEXT_SUBS.contains(&codec.as_str()),
                codec,
                lang: tag("language"),
                title: tag("title"),
                cues: None,
                span_ms: None,
            }),
            _ => {}
        }
    }
    for t in probe.subs.iter_mut().filter(|t| t.text) {
        if let Ok(segs) = extract_subtitle(path, t.stream) {
            t.cues = Some(segs.len());
            t.span_ms = segs.last().map(|s| s.out_ms);
        }
    }
    Ok(probe)
}

fn video_codec_name(codec: &str) -> String {
    match codec {
        "h264" => "H.264".into(),
        "hevc" => "HEVC".into(),
        "av1" => "AV1".into(),
        "vp9" => "VP9".into(),
        "prores" => "ProRes".into(),
        c => c.to_uppercase(),
    }
}

pub fn extract_subtitle(path: &Path, stream: usize) -> Result<Vec<crate::data::Segment>> {
    let out = Command::new(need("ffmpeg")?)
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", &format!("0:{stream}"), "-f", "srt", "-"])
        .output()
        .context("running ffmpeg")?;
    if !out.status.success() {
        bail!(
            "{}",
            first_line(&out.stderr).unwrap_or("ffmpeg could not read that track".into())
        );
    }
    subs::parse("track.srt", &String::from_utf8_lossy(&out.stdout))
}

/// Decodes one audio stream to 16 kHz mono float samples, reporting how many
/// milliseconds have been decoded so far.
pub fn decode_audio(
    path: &Path,
    stream: Option<usize>,
    mut progress: impl FnMut(i64),
    cancelled: impl Fn() -> bool,
) -> Result<Vec<f32>> {
    let map = stream
        .map(|s| format!("0:{s}"))
        .unwrap_or_else(|| "0:a:0".into());
    let mut child = Command::new(need("ffmpeg")?)
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            &map,
            "-vn",
            "-ac",
            "1",
            "-ar",
            &SAMPLE_RATE.to_string(),
            "-f",
            "f32le",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg")?;
    let mut stdout = child.stdout.take().context("ffmpeg output")?;
    let mut bytes = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if cancelled() {
            let _ = child.kill();
            bail!("cancelled");
        }
        let n = stdout.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        progress((bytes.len() / 4 / (SAMPLE_RATE / 1000)) as i64);
    }
    let status = child.wait()?;
    if !status.success() {
        let mut err = Vec::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_end(&mut err);
        }
        bail!(
            "{}",
            first_line(&err).unwrap_or("ffmpeg could not decode the audio".into())
        );
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn first_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
}

/// Burns `script` (an ASS document) into a new video at `dest`. The source
/// file is only read. Audio is copied. `progress` receives 0–1 from ffmpeg's
/// `out_time_us` when `duration_ms` is known. A set `cancel` flag kills the
/// encode and removes the partial file.
pub fn burn_subtitles(
    video: &Path,
    script: &str,
    dest: &Path,
    duration_ms: i64,
    cancel: &Arc<AtomicBool>,
    mut progress: impl FnMut(f32),
) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    let ffmpeg = need("ffmpeg")?;
    let ass_path = std::env::temp_dir().join(format!(
        "taurus-{}-{}.ass",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::write(&ass_path, script.as_bytes()).context("writing the subtitle script")?;
    let _ass = Scratch::remove(ass_path.clone());

    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = dest.with_file_name(format!(
        ".{}.part",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video.mp4")
    ));
    let mut partial = Scratch::remove(tmp.clone());

    let filter = format!(
        "subtitles='{}':charenc=UTF-8:fontsdir='{}'",
        filter_escape(&ass_path),
        filter_escape(Path::new("/System/Library/Fonts"))
    );
    let mut child = Command::new(&ffmpeg)
        .args(["-hide_banner", "-nostdin", "-y", "-i"])
        .arg(video)
        .args(["-vf"])
        .arg(&filter)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a?",
            "-sn",
            "-dn",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "20",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "copy",
            "-movflags",
            "+faststart",
            "-progress",
            "pipe:1",
            "-nostats",
        ])
        .arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg")?;

    let finished = Arc::new(AtomicBool::new(false));
    let finished2 = Arc::clone(&finished);
    let cancel2 = Arc::clone(cancel);
    let pid = child.id();
    std::thread::spawn(move || {
        while !finished2.load(Ordering::Relaxed) {
            if cancel2.load(Ordering::Relaxed) {
                let _ = Command::new("/bin/kill")
                    .args(["-9", &pid.to_string()])
                    .status();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
    let _stop_watch = Flag(finished);

    let stderr = child.stderr.take();
    let err_task = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_end(&mut buf);
        }
        buf
    });
    let mut reader = BufReader::new(child.stdout.take().context("ffmpeg progress")?);
    let mut line = String::new();
    let mut last_pct = -1.0f32;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = err_task.join();
            bail!("cancelled");
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                let us = trimmed
                    .strip_prefix("out_time_us=")
                    .and_then(|v| v.parse::<i64>().ok());
                if let Some(us) = us
                    && duration_ms > 0
                {
                    let pct = (us as f32 / (duration_ms as f32 * 1000.0)).clamp(0.0, 0.99);
                    if pct - last_pct >= 0.01 {
                        last_pct = pct;
                        progress(pct);
                    }
                }
            }
            Err(_) if cancel.load(Ordering::Relaxed) => {
                let _ = child.wait();
                let _ = err_task.join();
                bail!("cancelled");
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = err_task.join();
                return Err(e).context("reading ffmpeg progress");
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = err_task.join();
        bail!("cancelled");
    }
    let status = child.wait().context("waiting for ffmpeg")?;
    let err = err_task.join().unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    if !status.success() {
        bail!(
            "{}",
            last_line(&err).unwrap_or_else(|| "ffmpeg could not write the video".into())
        );
    }
    std::fs::rename(&tmp, dest).context("saving the video")?;
    partial.keep = true;
    progress(1.0);
    Ok(())
}

/// Characters that end a filter option or a filtergraph. The path is also
/// wrapped in single quotes.
fn filter_escape(path: &Path) -> String {
    let mut out = String::new();
    for c in path.to_string_lossy().chars() {
        if matches!(c, '\\' | ':' | '\'' | ',' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn last_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .next_back()
        .map(str::to_string)
}

/// Deletes `path` on drop unless `keep` is set after a successful rename.
struct Scratch {
    path: PathBuf,
    keep: bool,
}

impl Scratch {
    fn remove(path: PathBuf) -> Self {
        Self { path, keep: false }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct Flag(Arc<AtomicBool>);

impl Drop for Flag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub fn file_size_label(path: &Path) -> String {
    match std::fs::metadata(path).map(|m| m.len()) {
        Ok(n) if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        Ok(n) if n >= 1 << 20 => format!("{:.0} MB", n as f64 / (1u64 << 20) as f64),
        Ok(n) => format!("{:.0} KB", (n as f64 / 1024.0).max(1.0)),
        Err(_) => "–".into(),
    }
}
