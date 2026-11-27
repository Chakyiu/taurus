//! Reading videos through ffprobe and ffmpeg. Apps started from Finder do not
//! inherit the shell's PATH, so the usual Homebrew locations are searched too.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
    /// Codec of `video` (`h264`, `hevc`, …) when there is a picture.
    pub video_codec: Option<String>,
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
                    probe.video_codec = Some(codec);
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
    // Subtitle payloads are read only after the user picks a track. Extracting
    // every text track here made the probe duplicate the chosen-track work.
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

/// Decodes a bounded section of one audio stream to 16 kHz mono floats.
pub fn decode_audio_window(
    path: &Path,
    stream: Option<usize>,
    start_ms: i64,
    duration_ms: i64,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<f32>> {
    let start_ms = start_ms.max(0);
    let map = stream
        .map(|s| format!("0:{s}"))
        .unwrap_or_else(|| "0:a:0".into());
    let mut child = Command::new(need("ffmpeg")?)
        .args([
            "-nostdin",
            "-v",
            "error",
            "-ss",
            &format!("{}.{:03}", start_ms / 1000, start_ms % 1000),
            "-t",
            &format!(
                "{}.{:03}",
                duration_ms.max(0) / 1000,
                duration_ms.max(0) % 1000
            ),
            "-i",
        ])
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

static SRT_SEQ: AtomicU64 = AtomicU64::new(0);

/// True when the picture and every audio stream already play in QuickTime
/// inside an mp4, so the export can copy them instead of re-encoding.
fn can_stream_copy(probe: &Probe) -> bool {
    let video = matches!(
        probe.video_codec.as_deref(),
        Some("h264" | "hevc" | "mpeg4")
    );
    let audio = probe
        .audio
        .iter()
        .all(|a| matches!(a.codec.as_str(), "aac" | "mp3" | "ac3" | "eac3"));
    video && audio
}

/// Muxes `script` (SRT) into a new video at `dest` as a subtitle track. The
/// source file is only read. Picture and sound that QuickTime already plays
/// are copied; anything else is re-encoded, because a normal ffmpeg build
/// cannot draw subtitles onto the picture. `progress` receives 0–1 from
/// ffmpeg's `out_time_us` when `duration_ms` is known. A set `cancel` flag
/// kills the encode and removes the partial file.
pub fn embed_subtitles(
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
    let srt_path = std::env::temp_dir().join(format!(
        "taurus-{}-{}.srt",
        std::process::id(),
        SRT_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&srt_path, script.as_bytes()).context("writing the subtitle file")?;
    let _srt = Scratch::remove(srt_path.clone());

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

    let probed = probe(video).ok();
    let mut copy = probed.as_ref().is_some_and(can_stream_copy);
    let hevc = probed.as_ref().and_then(|p| p.video_codec.as_deref()) == Some("hevc");
    loop {
        let job = Mux {
            ffmpeg: &ffmpeg,
            video,
            srt: &srt_path,
            tmp: &tmp,
            copy,
            tag_hevc: copy && hevc,
            duration_ms,
            cancel,
        };
        match mux(&job, &mut progress) {
            Ok(()) => break,
            Err(e) if e.to_string() == "cancelled" || !copy => return Err(e),
            Err(_) => {
                copy = false;
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    std::fs::rename(&tmp, dest).context("saving the video")?;
    partial.keep = true;
    progress(1.0);
    Ok(())
}

struct Mux<'a> {
    ffmpeg: &'a Path,
    video: &'a Path,
    srt: &'a Path,
    tmp: &'a Path,
    copy: bool,
    tag_hevc: bool,
    duration_ms: i64,
    cancel: &'a Arc<AtomicBool>,
}

fn mux(job: &Mux<'_>, progress: &mut impl FnMut(f32)) -> Result<()> {
    let mut cmd = Command::new(job.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-y", "-i"])
        .arg(job.video)
        .arg("-i")
        .arg(job.srt)
        .args(["-map", "0:v:0", "-map", "0:a?", "-map", "1:0"]);
    if job.copy {
        cmd.args(["-c:v", "copy", "-c:a", "copy"]);
        if job.tag_hevc {
            cmd.args(["-tag:v", "hvc1"]);
        }
    } else {
        cmd.args([
            "-vf",
            "scale=trunc(iw/2)*2:trunc(ih/2)*2",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "20",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ]);
    }
    let mut child = cmd
        .args([
            "-c:s",
            "mov_text",
            "-disposition:s:0",
            "default",
            "-movflags",
            "+faststart",
            "-progress",
            "pipe:1",
            "-nostats",
            "-f",
            "mp4",
        ])
        .arg(job.tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg")?;

    let finished = Arc::new(AtomicBool::new(false));
    let finished2 = Arc::clone(&finished);
    let cancel2 = Arc::clone(job.cancel);
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
        if job.cancel.load(Ordering::Relaxed) {
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
                    && job.duration_ms > 0
                {
                    let pct = (us as f32 / (job.duration_ms as f32 * 1000.0)).clamp(0.0, 0.99);
                    if pct - last_pct >= 0.01 {
                        last_pct = pct;
                        progress(pct);
                    }
                }
            }
            Err(_) if job.cancel.load(Ordering::Relaxed) => {
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
    if job.cancel.load(Ordering::Relaxed) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = err_task.join();
        bail!("cancelled");
    }
    let status = child.wait().context("waiting for ffmpeg")?;
    let err = err_task.join().unwrap_or_default();
    if job.cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    if !status.success() {
        bail!(
            "{}",
            last_line(&err).unwrap_or_else(|| "ffmpeg could not write the video".into())
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::{embed_subtitles, probe, tool};

    const CUE: &str = "1\n00:00:00,000 --> 00:00:00,300\n你好\n";

    fn ffmpeg() -> Option<PathBuf> {
        tool("ffprobe")?;
        tool("ffmpeg")
    }

    fn video_md5(ffmpeg: &std::path::Path, path: &std::path::Path) -> String {
        let out = Command::new(ffmpeg)
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(path)
            .args(["-map", "0:v:0", "-c", "copy", "-f", "md5", "-"])
            .output()
            .ok();
        out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    fn cue_text(ffmpeg: &std::path::Path, path: &std::path::Path) -> String {
        let out = Command::new(ffmpeg)
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(path)
            .args(["-map", "0:s:0", "-f", "srt", "-"])
            .output();
        out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    #[test]
    fn copies_h264_and_embeds_the_cue() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = std::env::temp_dir();
        let src = dir.join(format!("taurus-emb-h264-{}.mp4", std::process::id()));
        let dest = dir.join(format!("taurus-emb-h264-out-{}.mp4", std::process::id()));
        let built = Command::new(&ffmpeg)
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=0x2244aa:s=160x90:d=0.4",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "libx264",
                "-an",
            ])
            .arg(&src)
            .status();
        if built.is_err() || !built.unwrap().success() {
            let _ = std::fs::remove_file(&src);
            return;
        }
        let before = video_md5(&ffmpeg, &src);
        let cancel = Arc::new(AtomicBool::new(false));
        let result = embed_subtitles(&src, CUE, &dest, 400, &cancel, |_| {});
        let after = video_md5(&ffmpeg, &dest);
        let text = cue_text(&ffmpeg, &dest);
        let kind = probe(&dest).ok().and_then(|p| p.video_codec);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);
        result.expect("embed");
        assert_eq!(kind.as_deref(), Some("h264"));
        assert_eq!(before, after, "h264 picture should be copied");
        assert!(text.contains("你好"), "{text}");
    }

    #[test]
    fn reencodes_pcm_audio_and_embeds_the_cue() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = std::env::temp_dir();
        let src = dir.join(format!("taurus-emb-pcm-{}.mov", std::process::id()));
        let dest = dir.join(format!("taurus-emb-pcm-out-{}.mp4", std::process::id()));
        let built = Command::new(&ffmpeg)
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=0x2244aa:s=160x90:d=0.4",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.4",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "libx264",
                "-c:a",
                "pcm_s16le",
                "-shortest",
            ])
            .arg(&src)
            .status();
        if built.is_err() || !built.unwrap().success() {
            let _ = std::fs::remove_file(&src);
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let result = embed_subtitles(&src, CUE, &dest, 400, &cancel, |_| {});
        let text = cue_text(&ffmpeg, &dest);
        let audio = probe(&dest)
            .ok()
            .and_then(|p| p.audio.first().map(|a| a.codec.clone()));
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);
        result.expect("embed");
        assert_eq!(audio.as_deref(), Some("aac"));
        assert!(text.contains("你好"), "{text}");
    }
}
