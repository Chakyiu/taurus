//! Live audio in: this Mac's own output through ScreenCaptureKit, or a
//! microphone through Core Audio. Both arrive as 16 kHz mono floats.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::media::SAMPLE_RATE;

pub const ALL_SYSTEM: &str = "All system output";
pub const DEFAULT_MIC: &str = "Default microphone";

#[derive(Clone, Debug)]
pub enum CaptureKind {
    /// `None` hears every app; `Some(name)` hears one application.
    System(Option<String>),
    Mic(Option<String>),
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

pub fn start(kind: CaptureKind, sink: Sender<Vec<f32>>) -> Result<Capture> {
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = channel::<Result<()>>();
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("taurus-capture".into())
        .spawn(move || {
            let run = match kind {
                CaptureKind::System(app) => run_system(app, sink, &flag, &ready_tx),
                CaptureKind::Mic(dev) => run_mic(dev, sink, &flag, &ready_tx),
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
    let config = device.default_input_config().context("reading the microphone format")?;
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
            let Ok(list) = sample.audio_buffer_list() else { return };
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
        None | Some(ALL_SYSTEM) => SCContentFilter::create().with_display(&display).with_excluding_windows(&[]).build(),
        Some(name) => {
            let apps = content.applications();
            let picked: Vec<&SCRunningApplication> =
                apps.iter().filter(|a| a.application_name() == name).collect();
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
    fn resamples_48k_stereo_to_16k_mono() {
        let mut r = Resampler::new(48_000, 2);
        let block: Vec<f32> = (0..9600).map(|i| if i % 2 == 0 { 0.5 } else { -0.5 }).collect();
        let mut total = 0;
        for _ in 0..10 {
            let out = r.push(block.iter().copied());
            assert!(out.iter().all(|s| s.abs() < 1e-6));
            total += out.len();
        }
        assert!((total as i64 - 16_000).abs() < 4, "{total}");
    }
}
