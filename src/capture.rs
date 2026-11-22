//! Live audio in: this Mac's own output through ScreenCaptureKit, a
//! microphone through Core Audio, or the two mixed. All arrive as 16 kHz
//! mono floats.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use gpui::RenderImage;

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

const MAX_AUDIO_QUEUE_BLOCKS: usize = 1024;

/// Non-blocking sender for the live capture queue. Full or busy queues drop new blocks.
pub(crate) struct AudioSender(Arc<AudioQueueShared>);

struct AudioQueueShared {
    state: Mutex<AudioQueueState>,
    ready: Condvar,
    max_samples: usize,
    max_blocks: usize,
    dropped: AtomicBool,
}

struct AudioQueueState {
    blocks: VecDeque<Vec<f32>>,
    samples: usize,
    closed: bool,
}

pub(crate) struct AudioReceiver(Arc<AudioQueueShared>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioRecvError {
    Timeout,
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioSendStatus {
    Sent,
    Dropped,
    Closed,
}

trait PcmSink: Send + Sync + 'static {
    fn send_pcm(&self, pcm: Vec<f32>) -> AudioSendStatus;
}

impl PcmSink for Sender<Vec<f32>> {
    fn send_pcm(&self, pcm: Vec<f32>) -> AudioSendStatus {
        if self.send(pcm).is_ok() {
            AudioSendStatus::Sent
        } else {
            AudioSendStatus::Closed
        }
    }
}

impl PcmSink for AudioSender {
    fn send_pcm(&self, pcm: Vec<f32>) -> AudioSendStatus {
        self.try_send(pcm)
    }
}

/// A hard cap measured in samples. The capture callback never waits on the consumer.
pub(crate) fn audio_queue(max_samples: usize) -> (AudioSender, AudioReceiver) {
    let shared = Arc::new(AudioQueueShared {
        state: Mutex::new(AudioQueueState {
            blocks: VecDeque::new(),
            samples: 0,
            closed: false,
        }),
        ready: Condvar::new(),
        max_samples,
        max_blocks: MAX_AUDIO_QUEUE_BLOCKS,
        dropped: AtomicBool::new(false),
    });
    (AudioSender(shared.clone()), AudioReceiver(shared))
}

impl AudioSender {
    fn note_dropped(&self) {
        self.0.dropped.store(true, Ordering::Relaxed);
    }

    fn try_send(&self, pcm: Vec<f32>) -> AudioSendStatus {
        if pcm.is_empty() {
            return AudioSendStatus::Sent;
        }
        let mut state = match self.0.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                self.0.dropped.store(true, Ordering::Relaxed);
                return AudioSendStatus::Dropped;
            }
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
        };
        if state.closed {
            return AudioSendStatus::Closed;
        }
        if state.blocks.len() >= self.0.max_blocks
            || pcm.len() > self.0.max_samples.saturating_sub(state.samples)
        {
            self.0.dropped.store(true, Ordering::Relaxed);
            return AudioSendStatus::Dropped;
        }
        state.samples += pcm.len();
        state.blocks.push_back(pcm);
        self.0.ready.notify_one();
        AudioSendStatus::Sent
    }
}

impl Drop for AudioSender {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        self.0.ready.notify_all();
    }
}

impl AudioReceiver {
    pub fn take_dropped(&self) -> bool {
        self.0.dropped.swap(false, Ordering::Relaxed)
    }

    pub fn try_recv(&self) -> std::result::Result<Vec<f32>, TryRecvError> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(block) = state.blocks.pop_front() {
            state.samples -= block.len();
            Ok(block)
        } else if state.closed {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> std::result::Result<Vec<f32>, AudioRecvError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(block) = state.blocks.pop_front() {
                state.samples -= block.len();
                return Ok(block);
            }
            if state.closed {
                return Err(AudioRecvError::Disconnected);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(AudioRecvError::Timeout);
            }
            let (next, result) = self
                .0
                .ready
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            state = next;
            if result.timed_out() && state.blocks.is_empty() {
                return Err(AudioRecvError::Timeout);
            }
        }
    }
}

impl Drop for AudioReceiver {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        self.0.ready.notify_all();
    }
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
    /// A smaller stream for the live picture. `None` when preview is off or
    /// could not start; the movie still runs.
    preview: Option<Preview>,
}

/// The latest preview frame sits here until `preview_frame` takes it.
struct Preview {
    stream: Mutex<Option<screencapturekit::prelude::SCStream>>,
    frames: Arc<Mutex<Option<Arc<RenderImage>>>>,
}

impl ScreenRecording {
    /// Stops the preview stream. The movie keeps writing until this value is dropped.
    pub(crate) fn stop_preview(&self) {
        let Some(preview) = &self.preview else {
            return;
        };
        if let Some(stream) = preview
            .stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = stream.stop_capture();
        }
    }

    /// The newest frame `pump` has not taken. A frame still sitting here when
    /// the next one arrives is dropped without `drop_image`.
    pub(crate) fn preview_frame(&self) -> Option<Arc<RenderImage>> {
        let preview = self.preview.as_ref()?;
        preview
            .frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

impl Drop for ScreenRecording {
    fn drop(&mut self) {
        self.stop_preview();
        let _ = self.stream.remove_recording_output(&self.output);
        let _ = self.stream.stop_capture();
    }
}

/// A rectangle on the screen in points, the unit ScreenCaptureKit reports both
/// displays and windows in.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn new(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    /// How much of `other` lies inside this rectangle.
    fn overlap(&self, other: &Rect) -> f64 {
        let w = (self.x + self.w).min(other.x + other.w) - self.x.max(other.x);
        let h = (self.y + self.h).min(other.y + other.h) - self.y.max(other.y);
        if w > 0.0 && h > 0.0 { w * h } else { 0.0 }
    }
}

/// The display holding most of `windows`, by covered area. A chosen app can be
/// on a second monitor, so the recording has to follow its windows there. Falls
/// back to the first display when no window is on any of them, since a display
/// still has to be picked.
fn display_for_windows(windows: &[Rect], displays: &[Rect]) -> Option<usize> {
    if displays.is_empty() {
        return None;
    }
    let mut best = 0;
    let mut best_area = 0.0;
    for (i, d) in displays.iter().enumerate() {
        let area: f64 = windows.iter().map(|w| d.overlap(w)).sum();
        if area > best_area {
            best = i;
            best_area = area;
        }
    }
    Some(best)
}

/// Live preview height. Both sides of the picture stay even.
pub const PREVIEW_MAX_HEIGHT: u32 = 480;
/// Live preview frame rate.
pub const PREVIEW_FPS: u32 = 8;

/// The movie's size for a display whose current mode is `pixels` wide and
/// tall: that size itself, capped. Full quality keeps the display's own pixels
/// up to 4K across; the other qualities cap the height. Both sides are rounded
/// down to even numbers.
fn movie_size(pixels: (u32, u32), quality: VideoQuality) -> (u32, u32) {
    let w = pixels.0.max(1) as f64;
    let h = pixels.1.max(1) as f64;
    let shrink = match quality.max_height() {
        Some(max) => (max as f64 / h).min(1.0),
        None => (3840.0 / w).min(1.0),
    };
    (even_side(w * shrink), even_side(h * shrink))
}

/// Preview size for a display of `pixels`: the same even rounding as
/// `movie_size`, with the height capped at [`PREVIEW_MAX_HEIGHT`].
fn preview_size(pixels: (u32, u32)) -> (u32, u32) {
    let w = pixels.0.max(1) as f64;
    let h = pixels.1.max(1) as f64;
    let shrink = (PREVIEW_MAX_HEIGHT as f64 / h).min(1.0);
    (even_side(w * shrink), even_side(h * shrink))
}

fn even_side(v: f64) -> u32 {
    ((v.round() as u32) / 2 * 2).max(2)
}

#[cfg_attr(
    target_vendor = "apple",
    link(name = "CoreGraphics", kind = "framework")
)]
unsafe extern "C" {
    fn CGDisplayCopyDisplayMode(display: u32) -> *mut std::ffi::c_void;
    fn CGDisplayModeGetPixelWidth(mode: *mut std::ffi::c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *mut std::ffi::c_void) -> usize;
    fn CGDisplayModeRelease(mode: *mut std::ffi::c_void);
}

/// The pixel size macOS reports for the display's current mode, which is the
/// display's own resolution: a 1512x982-point Retina screen answers
/// 3024x1964 here. `None` when the display has no mode to read.
fn display_pixels(id: u32) -> Option<(u32, u32)> {
    unsafe {
        let mode = CGDisplayCopyDisplayMode(id);
        if mode.is_null() {
            return None;
        }
        let (w, h) = (
            CGDisplayModeGetPixelWidth(mode),
            CGDisplayModeGetPixelHeight(mode),
        );
        CGDisplayModeRelease(mode);
        (w > 0 && h > 0).then_some((w as u32, h as u32))
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
    let displays = content.displays();
    if displays.is_empty() {
        bail!("no display to record");
    }
    let apps = content.applications();
    let picked: Vec<&SCRunningApplication> = if target == WHOLE_SCREEN {
        Vec::new()
    } else {
        let picked: Vec<&SCRunningApplication> = apps
            .iter()
            .filter(|a| a.application_name() == target)
            .collect();
        if picked.is_empty() {
            bail!("{target} is not running, so it cannot be recorded");
        }
        picked
    };
    // Follow the chosen app onto whichever display its windows are on.
    let chosen = if picked.is_empty() {
        0
    } else {
        let windows: Vec<Rect> = content
            .windows()
            .iter()
            .filter(|w| w.window_layer() == 0 && w.frame().size.width > 1.0)
            .filter(|w| {
                w.owning_application()
                    .is_some_and(|a| picked.iter().any(|p| p.process_id() == a.process_id()))
            })
            .map(|w| {
                let f = w.frame();
                Rect::new(f.origin.x, f.origin.y, f.size.width, f.size.height)
            })
            .collect();
        let display_rects: Vec<Rect> = displays
            .iter()
            .map(|d| {
                let f = d.frame();
                Rect::new(f.origin.x, f.origin.y, f.size.width, f.size.height)
            })
            .collect();
        display_for_windows(&windows, &display_rects).unwrap_or(0)
    };
    let display = &displays[chosen];
    let filter = if picked.is_empty() {
        SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build()
    } else {
        SCContentFilter::create()
            .with_display(display)
            .with_including_applications(&picked, &[])
            .build()
    }
    .map_err(|e| anyhow!("could not set up the screen recording: {e}"))?;

    // `SCDisplay`'s own width and height are points, and the movie wants the
    // display's pixels, which macOS reports per display mode. Asking for
    // anything else gets the picture scaled up and blurry on Retina.
    let points = (display.width(), display.height());
    let pixels = display_pixels(display.display_id()).unwrap_or(points);
    let (w, h) = movie_size(pixels, quality);
    eprintln!(
        "taurus: recording display {} ({}x{} points, {}x{} pixels) as {w}x{h}",
        display.display_id(),
        points.0,
        points.1,
        pixels.0,
        pixels.1
    );
    let config = SCStreamConfiguration::new()
        .with_width(w)
        .with_height(h)
        .with_scales_to_fit(true)
        .with_minimum_frame_interval(&CMTime::new(1, quality.fps()))
        .with_shows_cursor(true)
        .with_captures_audio(system_audio)
        .with_excludes_current_process_audio(true)
        .with_sample_rate(48_000)
        .with_channel_count(2)
        .with_captures_microphone(microphone)
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
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
    let preview = match std::env::var("TAURUS_PREVIEW").ok().as_deref() {
        Some("0") => None,
        Some("fail") => {
            eprintln!(
                "taurus: live preview did not start (TAURUS_PREVIEW=fail); the movie continues"
            );
            None
        }
        _ => match open_preview(&filter, pixels) {
            Ok(preview) => Some(preview),
            Err(e) => {
                eprintln!("taurus: live preview did not start ({e:#}); the movie continues");
                None
            }
        },
    };
    Ok(ScreenRecording {
        stream,
        output,
        preview,
    })
}

/// A second stream on `filter`, scaled for the live well. The recording stream
/// is left alone: a screen handler on it shares the movie's surface pool.
fn open_preview(
    filter: &screencapturekit::prelude::SCContentFilter,
    pixels: (u32, u32),
) -> Result<Preview> {
    use screencapturekit::prelude::*;

    let (w, h) = preview_size(pixels);
    let config = SCStreamConfiguration::new()
        .with_width(w)
        .with_height(h)
        .with_scales_to_fit(true)
        .with_fps(PREVIEW_FPS)
        .with_pixel_format(PixelFormat::BGRA)
        .with_queue_depth(3)
        .with_shows_cursor(true)
        .with_captures_audio(false);
    let frames = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&frames);
    let mut stream = SCStream::new(filter, &config).map_err(|e| anyhow!("{e}"))?;
    stream
        .add_output_handler(
            move |sample, kind| {
                if !matches!(kind, SCStreamOutputType::Screen) {
                    return;
                }
                let Some(image) = preview_image(&sample) else {
                    return;
                };
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(image);
            },
            SCStreamOutputType::Screen,
        )
        .map_err(|e| anyhow!("{e}"))?;
    stream
        .start_capture()
        .map_err(|e| anyhow!("could not start the live preview: {e}"))?;
    eprintln!("taurus: previewing the screen as {w}x{h}");
    Ok(Preview {
        stream: Mutex::new(Some(stream)),
        frames,
    })
}

/// Copies one BGRA frame out of a locked pixel buffer. Rows are padded to
/// `bytes_per_row`; copying that stride into a packed image tears the picture.
fn preview_image(sample: &screencapturekit::prelude::CMSampleBuffer) -> Option<Arc<RenderImage>> {
    use screencapturekit::cv::CVPixelBufferLockFlags;
    use screencapturekit::prelude::CMSampleBufferExt;

    let buffer = sample.pixel_buffer()?;
    let guard = buffer.lock(CVPixelBufferLockFlags::READ_ONLY).ok()?;
    // The guard keeps this mapping alive; the copy finishes before it drops.
    let src = unsafe { guard.as_slice() }?;
    let image = copy_bgra(src, guard.width(), guard.height(), guard.bytes_per_row())?;
    drop(guard);
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(image)])))
}

fn copy_bgra(
    src: &[u8],
    width: usize,
    height: usize,
    bytes_per_row: usize,
) -> Option<image::RgbaImage> {
    let row_bytes = width.checked_mul(4)?;
    if width == 0 || height == 0 || bytes_per_row < row_bytes {
        return None;
    }
    let need = bytes_per_row.checked_mul(height)?;
    if src.len() < need {
        return None;
    }
    let (w, h) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    let mut packed = vec![0u8; row_bytes * height];
    for y in 0..height {
        let from = y * bytes_per_row;
        let to = y * row_bytes;
        packed[to..to + row_bytes].copy_from_slice(&src[from..from + row_bytes]);
    }
    image::RgbaImage::from_raw(w, h, packed)
}

pub(crate) fn start(kind: CaptureKind, sink: AudioSender) -> Result<Capture> {
    if let CaptureKind::Both { app, mic } = kind {
        return start_both(app, mic, sink);
    }
    start_source(kind, sink)
}

fn start_source<S: PcmSink>(kind: CaptureKind, sink: S) -> Result<Capture> {
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

fn start_both(app: Option<String>, mic: Option<String>, sink: AudioSender) -> Result<Capture> {
    let (sys_tx, sys_rx) = audio_queue(MAX_LEAD);
    let system = start_source(CaptureKind::System(app), sys_tx)?;
    let (mic_tx, mic_rx) = audio_queue(MAX_LEAD);
    let microphone = start_source(CaptureKind::Mic(mic), mic_tx)
        .map_err(|e| anyhow!("the microphone did not start: {e:#}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("taurus-mix".into())
        .spawn(move || {
            let _sources = (system, microphone);
            let mut mixer = Mixer::default();
            let mut outward_drop_reported = false;
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                let a = drain_audio(&sys_rx);
                let b = drain_audio(&mic_rx);
                let source_dropped = sys_rx.take_dropped() | mic_rx.take_dropped();
                if source_dropped && !outward_drop_reported {
                    sink.note_dropped();
                    outward_drop_reported = true;
                }
                let out = mixer.push(&a, &b);
                if !out.is_empty() && sink.send_pcm(out) == AudioSendStatus::Closed {
                    break;
                }
            }
        })?;
    Ok(Capture { stop })
}

fn drain_audio(rx: &AudioReceiver) -> Vec<f32> {
    let mut out = Vec::new();
    while let Ok(block) = rx.try_recv() {
        out.extend(block);
    }
    out
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

fn run_mic<S: PcmSink>(
    dev: Option<String>,
    sink: S,
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
                let _ = sink.send_pcm(rs.push(data.iter().copied()));
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            config.into(),
            move |data: &[i16], _: &_| {
                let _ = sink.send_pcm(rs.push(data.iter().map(|s| *s as f32 / 32768.0)));
            },
            err,
            None,
        )?,
        cpal::SampleFormat::I32 => device.build_input_stream(
            config.into(),
            move |data: &[i32], _: &_| {
                let _ = sink.send_pcm(rs.push(data.iter().map(|s| *s as f32 / 2_147_483_648.0)));
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

fn run_system<S: PcmSink>(
    app: Option<String>,
    sink: S,
    stop: &AtomicBool,
    ready: &Sender<Result<()>>,
) -> Result<()> {
    use screencapturekit::prelude::*;

    struct Handler<S: PcmSink>(S);
    impl<S: PcmSink> SCStreamOutputTrait for Handler<S> {
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
                    let _ = self.0.send_pcm(pcm);
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
    fn bounded_audio_queue_drops_new_blocks_at_its_sample_limit() {
        let (tx, rx) = audio_queue(4);
        assert_eq!(tx.try_send(vec![0.0; 3]), AudioSendStatus::Sent);
        assert_eq!(tx.try_send(vec![1.0; 2]), AudioSendStatus::Dropped);
        assert!(rx.take_dropped());
        assert!(!rx.take_dropped());
        assert_eq!(rx.try_recv().unwrap(), vec![0.0; 3]);
        assert_eq!(tx.try_send(vec![1.0; 4]), AudioSendStatus::Sent);
        assert_eq!(rx.try_recv().unwrap(), vec![1.0; 4]);
    }

    #[test]
    fn busy_audio_queue_drops_instead_of_waiting_on_the_capture_callback() {
        let (tx, rx) = audio_queue(4);
        let state = rx.0.state.lock().unwrap();
        assert_eq!(tx.try_send(vec![0.0]), AudioSendStatus::Dropped);
        assert!(rx.take_dropped());
        drop(state);
    }

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

    fn display(w: f64, h: f64) -> Rect {
        Rect::new(0.0, 0.0, w, h)
    }

    #[test]
    fn a_one_x_display_is_asked_for_its_own_pixels() {
        assert_eq!(movie_size((1920, 1080), VideoQuality::Full), (1920, 1080));
        assert_eq!(
            movie_size((1920, 1080), VideoQuality::Standard),
            (1920, 1080)
        );
        assert_eq!(movie_size((1920, 1080), VideoQuality::Small), (1280, 720));
    }

    #[test]
    fn a_retina_display_keeps_every_pixel() {
        // What macOS reports for a 1512x982-point Retina screen.
        assert_eq!(movie_size((3024, 1964), VideoQuality::Full), (3024, 1964));
        assert_eq!(
            movie_size((3024, 1964), VideoQuality::Standard),
            (1662, 1080)
        );
        assert_eq!(movie_size((3024, 1964), VideoQuality::Small), (1108, 720));
    }

    #[test]
    fn a_movie_is_capped_at_4k_across() {
        assert_eq!(movie_size((5120, 1440), VideoQuality::Full), (3840, 1080));
    }

    #[test]
    fn a_display_that_reports_nothing_still_gets_a_valid_size() {
        assert_eq!(movie_size((0, 0), VideoQuality::Full), (2, 2));
    }

    #[test]
    fn preview_is_capped_at_480_and_even() {
        assert_eq!(preview_size((1920, 1080)), (852, 480));
        assert_eq!(preview_size((3024, 1964)), (738, 480));
        assert_eq!(preview_size((640, 360)), (640, 360));
        assert_eq!(preview_size((101, 80)), (100, 80));
        assert_eq!(preview_size((0, 0)), (2, 2));
    }

    #[test]
    fn padded_rows_are_not_copied_into_the_picture() {
        let mut src = vec![0xABu8; 12 * 2];
        src[0..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        src[12..20].copy_from_slice(&[10, 11, 12, 13, 14, 15, 16, 17]);
        let image = copy_bgra(&src, 2, 2, 12).unwrap();
        assert_eq!(
            image.as_raw(),
            &[1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 15, 16, 17]
        );
        assert!(copy_bgra(&[0; 8], 2, 2, 8).is_none());
        assert!(copy_bgra(&[1, 2, 3, 4], 1, 1, 4).is_some());
    }

    #[test]
    fn the_display_showing_the_windows_wins() {
        let displays = [
            display(1920.0, 1080.0),
            Rect::new(1920.0, 0.0, 1512.0, 982.0),
        ];
        // A window mostly on the second monitor.
        let windows = [Rect::new(2400.0, 100.0, 900.0, 700.0)];
        assert_eq!(display_for_windows(&windows, &displays), Some(1));
        // The monitor holding the larger share of a window that spans both.
        let windows = [Rect::new(1500.0, 100.0, 1000.0, 700.0)];
        assert_eq!(display_for_windows(&windows, &displays), Some(1));
        let windows = [Rect::new(1000.0, 100.0, 1000.0, 700.0)];
        assert_eq!(display_for_windows(&windows, &displays), Some(0));
    }

    #[test]
    fn no_windows_or_no_displays_falls_back_to_the_first_one() {
        let displays = [display(1920.0, 1080.0), display(1920.0, 1080.0)];
        assert_eq!(display_for_windows(&[], &displays), Some(0));
        let offscreen = [Rect::new(9000.0, 9000.0, 400.0, 400.0)];
        assert_eq!(display_for_windows(&offscreen, &displays), Some(0));
        assert_eq!(display_for_windows(&offscreen, &[]), None);
    }
}
