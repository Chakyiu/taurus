//! Live audio in: this Mac's own output through ScreenCaptureKit, a
//! microphone through Core Audio, or the two mixed. All arrive as 16 kHz
//! mono floats.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::media::SAMPLE_RATE;

pub const ALL_SYSTEM: &str = "All system output";
pub const DEFAULT_MIC: &str = "Default microphone";
pub const NO_VIDEO: &str = "Don't record";
pub const WHOLE_SCREEN: &str = "Entire screen";

#[derive(Clone, Debug)]
pub enum CaptureKind {
    /// `None` hears every app; `Some(name)` hears one application.
    System(Option<String>),
    Mic(Option<String>),
    /// The Mac's output and a microphone together, e.g. both sides of a call.
    Both {
        app: Option<String>,
        mic: Option<String>,
    },
}

/// Stops the capture when dropped.
pub struct Capture {
    stop: Arc<AtomicBool>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn list_mics() -> Vec<String> {
    let mut out = vec![DEFAULT_MIC.to_string()];
    if let Ok(devs) = cpal::default_host().input_devices() {
        for d in devs {
            if let Ok(desc) = d.description() {
                let name = desc.name().to_string();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// Apps with a window on screen. Needs the Screen Recording permission, so an
/// empty list here is normal before it is granted.
pub fn list_apps() -> Vec<String> {
    use screencapturekit::prelude::*;
    let mut out = vec![ALL_SYSTEM.to_string()];
    let Ok(content) = SCShareableContent::get() else {
        return out;
    };
    let me = std::process::id() as i32;
    let mut names: Vec<String> = content
        .applications()
        .into_iter()
        .filter(|a| a.process_id() != me)
        .map(|a| a.application_name())
        .filter(|n| !n.is_empty())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    out.extend(names);
    out
}

/// Writing the screen straight to a movie needs macOS 15.
pub fn can_record_screen() -> bool {
    screencapturekit::recording_output::SCRecordingOutput::is_available()
}

/// What the screen recording can follow: nothing, the whole screen, or the
/// windows of one app.
pub fn list_video_targets() -> Vec<String> {
    let mut out = vec![NO_VIDEO.to_string(), WHOLE_SCREEN.to_string()];
    out.extend(list_apps().into_iter().filter(|a| a != ALL_SYSTEM));
    out
}

/// How big the screen recording is. Size on disk follows the picture size,
/// since the system encoder picks its own bitrate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VideoQuality {
    /// 720p at 15 frames a second: smallest, fine for reading slides and text.
    Small,
    /// 1080p at 30 frames a second.
    Standard,
    /// The display's own resolution at 30 frames a second.
    Full,
}

impl VideoQuality {
    pub const ALL: [VideoQuality; 3] = [
        VideoQuality::Small,
        VideoQuality::Standard,
        VideoQuality::Full,
    ];

    pub fn label(self) -> &'static str {
        match self {
            VideoQuality::Small => "Small · 720p",
            VideoQuality::Standard => "Standard · 1080p",
            VideoQuality::Full => "Full resolution",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            VideoQuality::Small => "Smallest movie; slides and text stay readable.",
            VideoQuality::Standard => "A good balance of size and sharpness.",
            VideoQuality::Full => "Sharpest, and the largest movie.",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            VideoQuality::Small => "small",
            VideoQuality::Standard => "standard",
            VideoQuality::Full => "full",
        }
    }

    pub fn from_key(key: &str) -> Option<VideoQuality> {
        VideoQuality::ALL.into_iter().find(|q| q.key() == key)
    }

    fn max_height(self) -> Option<u32> {
        match self {
            VideoQuality::Small => Some(720),
            VideoQuality::Standard => Some(1080),
            VideoQuality::Full => None,
        }
    }

    fn fps(self) -> i32 {
        if self == VideoQuality::Small { 15 } else { 30 }
    }
}

/// A movie of the screen being written; it is finished when dropped.
pub struct ScreenRecording {
    stream: screencapturekit::prelude::SCStream,
    output: screencapturekit::recording_output::SCRecordingOutput,
}

impl Drop for ScreenRecording {
    fn drop(&mut self) {
        let _ = self.stream.remove_recording_output(&self.output);
        let _ = self.stream.stop_capture();
    }
}

/// Records `target` (`WHOLE_SCREEN` or an app name) to an H.264 movie at
/// `path`, with this Mac's sound and, when asked, the default microphone.
pub fn record_screen(
    target: &str,
    quality: VideoQuality,
    system_audio: bool,
    microphone: bool,
    path: &Path,
) -> Result<ScreenRecording> {
    use screencapturekit::prelude::*;
    use screencapturekit::recording_output::{
        SCRecordingOutput, SCRecordingOutputCodec, SCRecordingOutputConfiguration,
        SCRecordingOutputFileType,
    };
    if !can_record_screen() {
        bail!("recording the screen needs macOS 15 or later");
    }
    let content = SCShareableContent::get().map_err(|e| {
        anyhow!("Taurus needs the Screen Recording permission to record the screen ({e})")
    })?;
    let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no display to record"))?;
    let filter = if target == WHOLE_SCREEN {
        SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build()
    } else {
        let apps = content.applications();
        let picked: Vec<&SCRunningApplication> = apps
            .iter()
            .filter(|a| a.application_name() == target)
            .collect();
        if picked.is_empty() {
            bail!("{target} is not running, so it cannot be recorded");
        }
        SCContentFilter::create()
            .with_display(&display)
            .with_including_applications(&picked, &[])
            .build()
    }
    .map_err(|e| anyhow!("could not set up the screen recording: {e}"))?;

    // Retina displays report points; full quality records every pixel, within 4K.
    let (w, h) = (
        display.width().max(1) as f64 * 2.0,
        display.height().max(1) as f64 * 2.0,
    );
    let scale = match quality.max_height() {
        Some(max) => (max as f64 / h).min(1.0),
        None => (3840.0 / w).min(1.0),
    };
    let even = |v: f64| ((v.round() as u32) / 2 * 2).max(2);
    let config = SCStreamConfiguration::new()
        .with_width(even(w * scale))
        .with_height(even(h * scale))
        .with_scales_to_fit(true)
        .with_minimum_frame_interval(&CMTime::new(1, quality.fps()))
        .with_shows_cursor(true)
        .with_captures_audio(system_audio)
        .with_excludes_current_process_audio(true)
        .with_sample_rate(48_000)
        .with_channel_count(2)
        .with_captures_microphone(microphone)
        .map_err(|e| anyhow!("{e}"))?;
    let _ = std::fs::remove_file(path);
    let out_config = SCRecordingOutputConfiguration::new()
        .map_err(|e| anyhow!("{e}"))?
        .with_output_url(path)
        .map_err(|e| anyhow!("{e:?}"))?
        .with_output_file_type(SCRecordingOutputFileType::MP4);
    // HEVC is about half the size of H.264 for the same picture.
    let codec = std::env::var("TAURUS_VIDEO_CODEC").ok();
    let hevc = codec.as_deref() != Some("h264")
        && out_config
            .available_video_codecs()
            .contains(&SCRecordingOutputCodec::HEVC);
    let out_config = out_config.with_video_codec(if hevc {
        SCRecordingOutputCodec::HEVC
    } else {
        SCRecordingOutputCodec::H264
    });
    let output = SCRecordingOutput::new(&out_config)
        .ok_or_else(|| anyhow!("could not create the movie file"))?;
    let stream = SCStream::new(&filter, &config).map_err(|e| anyhow!("{e}"))?;
    stream
        .add_recording_output(&output)
        .map_err(|e| anyhow!("could not start the screen recording: {e}"))?;
    stream
        .start_capture()
        .map_err(|e| anyhow!("could not start the screen recording: {e}"))?;
    Ok(ScreenRecording { stream, output })
}

pub fn start(kind: CaptureKind, sink: Sender<Vec<f32>>) -> Result<Capture> {
    if let CaptureKind::Both { app, mic } = kind {
        return start_both(app, mic, sink);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = channel::<Result<()>>();
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("taurus-capture".into())
        .spawn(move || {
            let run = match kind {
                CaptureKind::System(app) => run_system(app, sink, &flag, &ready_tx),
                CaptureKind::Mic(dev) => run_mic(dev, sink, &flag, &ready_tx),
                CaptureKind::Both { .. } => unreachable!(),
            };
            if let Err(e) = run {
                let _ = ready_tx.send(Err(e));
            }
        })?;
    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Ok(Capture { stop }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow!("the audio source did not start")),
    }
}

/// How far one source may run ahead before the other is taken to have gone
/// quiet and is filled with silence, in samples.
const MAX_LEAD: usize = SAMPLE_RATE / 2;
/// How much of that lead is kept after filling, so late audio still lines up.
const KEEP_LEAD: usize = SAMPLE_RATE / 4;

fn start_both(app: Option<String>, mic: Option<String>, sink: Sender<Vec<f32>>) -> Result<Capture> {
    let (sys_tx, sys_rx) = channel();
    let system = start(CaptureKind::System(app), sys_tx)?;
    let (mic_tx, mic_rx) = channel();
    let microphone = start(CaptureKind::Mic(mic), mic_tx)
        .map_err(|e| anyhow!("the microphone did not start: {e:#}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("taurus-mix".into())
        .spawn(move || {
            let _sources = (system, microphone);
            let mut mixer = Mixer::default();
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                let a = drain(&sys_rx);
                let b = drain(&mic_rx);
                let out = mixer.push(&a, &b);
                if !out.is_empty() && sink.send(out).is_err() {
                    break;
                }
            }
        })?;
    Ok(Capture { stop })
}

fn drain(rx: &Receiver<Vec<f32>>) -> Vec<f32> {
    let mut out = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(v) => out.extend(v),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
        }
    }
}

/// Sums two live streams sample for sample. Whichever arrives first waits for
/// the other, up to `MAX_LEAD`; a source that stops sending counts as silence.
#[derive(Default)]
struct Mixer {
    a: VecDeque<f32>,
    b: VecDeque<f32>,
}

impl Mixer {
    fn push(&mut self, a: &[f32], b: &[f32]) -> Vec<f32> {
        self.a.extend(a);
        self.b.extend(b);
        let (la, lb) = (self.a.len(), self.b.len());
        if la.abs_diff(lb) > MAX_LEAD {
            let short = if la < lb { &mut self.a } else { &mut self.b };
            short.extend(std::iter::repeat_n(0.0, la.abs_diff(lb) - KEEP_LEAD));
        }
        let n = self.a.len().min(self.b.len());
        self.a
            .drain(..n)
            .zip(self.b.drain(..n))
            .map(|(x, y)| (x + y).clamp(-1.0, 1.0))
            .collect()
    }
}

fn run_mic(
    dev: Option<String>,
    sink: Sender<Vec<f32>>,
    stop: &AtomicBool,
    ready: &Sender<Result<()>>,
) -> Result<()> {
    let host = cpal::default_host();
    let device = match dev.as_deref() {
        None | Some(DEFAULT_MIC) => host.default_input_device(),
        Some(name) => host
            .input_devices()?
            .find(|d| d.description().is_ok_and(|x| x.name() == name)),
    }
    .ok_or_else(|| anyhow!("that microphone is not connected"))?;
    let config = device
        .default_input_config()
        .context("reading the microphone format")?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate() as usize;
    let mut rs = Resampler::new(rate, channels);
    let err = |e: cpal::Error| eprintln!("taurus: microphone: {e}");
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config.into(),
            move |data: &[f32], _: &_| {
                let _ = sink.send(rs.push(data.iter().copied()));
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            config.into(),
            move |data: &[i16], _: &_| {
                let _ = sink.send(rs.push(data.iter().map(|s| *s as f32 / 32768.0)));
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I32 => device.build_input_stream(
            config.into(),
            move |data: &[i32], _: &_| {
                let _ = sink.send(rs.push(data.iter().map(|s| *s as f32 / 2_147_483_648.0)));
            },
            err,
            None,
        )?,
        f => bail!("the microphone uses an unsupported format ({f})"),
    };
    stream.play().context("starting the microphone")?;
    let _ = ready.send(Ok(()));
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
    Ok(())
}

fn run_system(
    app: Option<String>,
    sink: Sender<Vec<f32>>,
    stop: &AtomicBool,
    ready: &Sender<Result<()>>,
) -> Result<()> {
    use screencapturekit::prelude::*;

    struct Handler(Sender<Vec<f32>>);
    impl SCStreamOutputTrait for Handler {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, kind: SCStreamOutputType) {
            if !matches!(kind, SCStreamOutputType::Audio) {
                return;
            }
            let Ok(list) = sample.audio_buffer_list() else {
                return;
            };
            // One channel was asked for, so the first buffer holds everything.
            if let Some(buf) = (&list).into_iter().next() {
                let pcm: Vec<f32> = buf
                    .data()
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                if !pcm.is_empty() {
                    let _ = self.0.send(pcm);
                }
            }
        }
    }

    let content = SCShareableContent::get().map_err(|e| {
        anyhow!("Taurus needs the Screen Recording permission to hear this Mac ({e}). Allow it in System Settings → Privacy & Security, then try again")
    })?;
    let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no display to capture from"))?;
    let filter = match app.as_deref() {
        None | Some(ALL_SYSTEM) => SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build(),
        Some(name) => {
            let apps = content.applications();
            let picked: Vec<&SCRunningApplication> = apps
                .iter()
                .filter(|a| a.application_name() == name)
                .collect();
            if picked.is_empty() {
                bail!("{name} is not running");
            }
            SCContentFilter::create()
                .with_display(&display)
                .with_including_applications(&picked, &[])
                .build()
        }
    }
    .map_err(|e| anyhow!("could not set up the capture: {e}"))?;
    let config = SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_minimum_frame_interval(&CMTime::new(1, 2))
        .with_captures_audio(true)
        .with_excludes_current_process_audio(true)
        .with_sample_rate(SAMPLE_RATE as i32)
        .with_channel_count(1);
    let mut stream = SCStream::new(&filter, &config).map_err(|e| anyhow!("{e}"))?;
    stream
        .add_output_handler(Handler(sink), SCStreamOutputType::Audio)
        .map_err(|e| anyhow!("{e}"))?;
    stream.start_capture().map_err(|e| {
        anyhow!("could not start hearing this Mac ({e}). Check the Screen Recording permission in System Settings")
    })?;
    let _ = ready.send(Ok(()));
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = stream.stop_capture();
    Ok(())
}

/// Down-mixes interleaved frames and resamples them to 16 kHz by linear
/// interpolation, which is plenty for speech recognition.
pub struct Resampler {
    step: f64,
    channels: usize,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(rate: usize, channels: usize) -> Self {
        Self {
            step: rate as f64 / SAMPLE_RATE as f64,
            channels: channels.max(1),
            pos: 0.0,
            prev: 0.0,
        }
    }

    pub fn push(&mut self, interleaved: impl Iterator<Item = f32>) -> Vec<f32> {
        let mut mono = Vec::new();
        let mut acc = 0.0;
        let mut n = 0;
        for s in interleaved {
            acc += s;
            n += 1;
            if n == self.channels {
                mono.push(acc / self.channels as f32);
                acc = 0.0;
                n = 0;
            }
        }
        let mut out = Vec::with_capacity((mono.len() as f64 / self.step) as usize + 1);
        // `pos` is measured from the previous block's last sample, at -1.
        while self.pos < mono.len() as f64 - 1.0 {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let a = if i < 0.0 { self.prev } else { mono[i as usize] };
            let b = mono[(i + 1.0) as usize];
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        if let Some(&last) = mono.last() {
            self.prev = last;
            self.pos -= mono.len() as f64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixer_lines_up_both_sources() {
        let mut m = Mixer::default();
        assert!(m.push(&[0.25; 100], &[]).is_empty());
        let out = m.push(&[], &[0.5; 60]);
        assert_eq!(out, vec![0.75; 60]);
        assert_eq!(m.push(&[0.9; 10], &[0.9; 50]), [vec![1.0; 50]].concat());
    }

    #[test]
    fn a_quiet_source_is_filled_with_silence() {
        let mut m = Mixer::default();
        let out = m.push(&vec![0.1; MAX_LEAD + 1000], &[]);
        assert_eq!(out.len(), MAX_LEAD + 1000 - KEEP_LEAD);
        assert!(out.iter().all(|s| (*s - 0.1).abs() < 1e-6));
    }

    #[test]
    fn resamples_48k_stereo_to_16k_mono() {
        let mut r = Resampler::new(48_000, 2);
        let block: Vec<f32> = (0..9600)
            .map(|i| if i % 2 == 0 { 0.5 } else { -0.5 })
            .collect();
        let mut total = 0;
        for _ in 0..10 {
            let out = r.push(block.iter().copied());
            assert!(out.iter().all(|s| s.abs() < 1e-6));
            total += out.len();
        }
        assert!((total as i64 - 16_000).abs() < 4, "{total}");
    }
}
