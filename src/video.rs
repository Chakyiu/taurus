//! Plays a project's video in step with the audio clock, one JPEG frame at a
//! time. The picture is muted: the audio player remains the clock, and the
//! editor draws the cue for that moment over the frame.

use std::io::{ErrorKind, Read};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use gpui::RenderImage;

const FPS: i64 = 12;

/// The line to draw on the picture. The translation is the line being read
/// when the project has one; the original sits above it.
pub(crate) struct OverlayLine {
    pub primary: String,
    pub secondary: Option<String>,
}

pub(crate) fn overlay_line(translate: bool, orig: &str, trans: &str) -> Option<OverlayLine> {
    let orig = orig.trim();
    let trans = trans.trim();
    if translate && !trans.is_empty() {
        let secondary = (orig != trans && !orig.is_empty()).then(|| orig.to_string());
        Some(OverlayLine {
            primary: trans.to_string(),
            secondary,
        })
    } else if !orig.is_empty() {
        Some(OverlayLine {
            primary: orig.to_string(),
            secondary: None,
        })
    } else if !trans.is_empty() {
        Some(OverlayLine {
            primary: trans.to_string(),
            secondary: None,
        })
    } else {
        None
    }
}

pub(crate) struct VideoDeck {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    latest: Arc<Mutex<Option<Arc<RenderImage>>>>,
    error: Arc<Mutex<Option<String>>>,
    epoch: Arc<AtomicU64>,
    clock: Arc<AtomicI64>,
    playing: Arc<AtomicBool>,
}

impl VideoDeck {
    pub(crate) fn start(path: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let latest = Arc::new(Mutex::new(None));
        let error = Arc::new(Mutex::new(None));
        let epoch = Arc::new(AtomicU64::new(0));
        let clock = Arc::new(AtomicI64::new(0));
        let playing = Arc::new(AtomicBool::new(false));
        let deck = Self {
            path: path.clone(),
            stop: stop.clone(),
            child: child.clone(),
            latest: latest.clone(),
            error: error.clone(),
            epoch: epoch.clone(),
            clock: clock.clone(),
            playing: playing.clone(),
        };
        let _ = thread::Builder::new()
            .name("taurus-video".into())
            .spawn(move || decode(path, stop, child, latest, error, epoch, clock, playing));
        deck
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn set_clock(&self, ms: i64, playing: bool) {
        self.clock.store(ms.max(0), Ordering::Relaxed);
        self.playing.store(playing, Ordering::Relaxed);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub(crate) fn frame(&self) -> Option<Arc<RenderImage>> {
        lock(&self.latest).clone()
    }

    pub(crate) fn error(&self) -> Option<String> {
        lock(&self.error).clone()
    }
}

impl Drop for VideoDeck {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

struct Pipe {
    stdout: ChildStdout,
    stderr: ChildStderr,
    slot: Arc<Mutex<Option<Child>>>,
    origin: i64,
    index: i64,
    buf: Vec<u8>,
}

impl Pipe {
    /// The timestamp of the frame on screen, or the seek point before the first one arrives.
    fn shown_ms(&self) -> i64 {
        if self.index == 0 {
            self.origin
        } else {
            self.origin + (self.index - 1) * 1000 / FPS
        }
    }
}

fn decode(
    path: PathBuf,
    stop: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<Child>>>,
    latest: Arc<Mutex<Option<Arc<RenderImage>>>>,
    error: Arc<Mutex<Option<String>>>,
    epoch: Arc<AtomicU64>,
    clock: Arc<AtomicI64>,
    playing: Arc<AtomicBool>,
) {
    let mut last_target = clock.load(Ordering::Relaxed);
    let mut pipe: Option<Pipe> = None;
    let mut finished = false;

    while !stop.load(Ordering::Acquire) {
        let target = clock.load(Ordering::Relaxed);
        let on = playing.load(Ordering::Relaxed);
        let jump = target - last_target;
        let drift = pipe.as_ref().map(|p| target - p.shown_ms()).unwrap_or(0);
        let restart = (pipe.is_some() || finished)
            && (jump.abs() > 400
                || (finished && jump.abs() > 80)
                || (pipe.is_some() && !on && drift.abs() > 120));
        if restart {
            close(&mut pipe);
            thread::sleep(Duration::from_millis(90));
            if stop.load(Ordering::Acquire) {
                break;
            }
            let settled = clock.load(Ordering::Relaxed);
            if (settled - target).abs() > 180 {
                last_target = target;
                continue;
            }
            finished = false;
            last_target = settled;
            continue;
        }
        last_target = target;

        if pipe.is_none() {
            if finished {
                thread::sleep(Duration::from_millis(120));
                continue;
            }
            match open_pipe(&path, target, &slot) {
                Ok(next) => pipe = Some(next),
                Err(msg) => {
                    report(&error, &epoch, msg);
                    finished = true;
                    continue;
                }
            }
        }

        let Some(current) = pipe.as_mut() else {
            continue;
        };
        let next_ms = current.origin + current.index * 1000 / FPS;
        if current.index > 0 && (!on || next_ms > target + 40) {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        let index = current.index;
        let polled = poll_jpeg(&mut current.stdout, &mut current.buf);
        match polled {
            Ok(Chunk::Frame(bytes)) => {
                if let Some(image) = decode_jpeg(&bytes) {
                    *lock(&latest) = Some(image);
                    *lock(&error) = None;
                    epoch.fetch_add(1, Ordering::Release);
                }
                if let Some(current) = pipe.as_mut() {
                    current.index += 1;
                }
            }
            Ok(Chunk::Wait) => thread::sleep(Duration::from_millis(15)),
            Ok(Chunk::End) | Err(_) => {
                let msg = pipe
                    .take()
                    .map(reap)
                    .unwrap_or_else(|| "The video can't be played.".into());
                if index == 0 && lock(&latest).is_none() {
                    report(&error, &epoch, msg);
                }
                finished = true;
            }
        }
    }
    close(&mut pipe);
}

fn report(error: &Mutex<Option<String>>, epoch: &AtomicU64, msg: String) {
    *lock(error) = Some(msg);
    epoch.fetch_add(1, Ordering::Release);
}

fn open_pipe(path: &Path, ms: i64, slot: &Arc<Mutex<Option<Child>>>) -> Result<Pipe, String> {
    if let Some(mut old) = lock(slot).take() {
        let _ = old.kill();
        let _ = old.wait();
    }
    let ffmpeg = crate::media::tool("ffmpeg")
        .ok_or_else(|| "ffmpeg is not installed, so the picture can't be shown.".to_string())?;
    let start = format!("{:.3}", ms.max(0) as f64 / 1000.0);
    let mut child = Command::new(ffmpeg)
        .args([
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-v",
            "error",
            "-ss",
            &start,
            "-i",
        ])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-vf",
            "scale=w='min(960,iw)':h=-2,fps=12",
            "-q:v",
            "8",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not play the video: {e}"))?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Could not read the video.".into());
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Could not read the video.".into());
    };
    let _ = set_nonblocking(stdout.as_raw_fd());
    *lock(slot) = Some(child);
    Ok(Pipe {
        stdout,
        stderr,
        slot: slot.clone(),
        origin: ms.max(0),
        index: 0,
        buf: Vec::new(),
    })
}

fn close(pipe: &mut Option<Pipe>) {
    if let Some(pipe) = pipe.take() {
        reap(pipe);
    }
}

fn reap(mut pipe: Pipe) -> String {
    drop(pipe.stdout);
    if let Some(mut child) = lock(&pipe.slot).take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let mut buf = Vec::new();
    let _ = pipe.stderr.read_to_end(&mut buf);
    short_error(&String::from_utf8_lossy(&buf))
}

fn short_error(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line
        .rsplit_once("] ")
        .map(|(_, rest)| rest.trim())
        .unwrap_or(line);
    if line.is_empty() {
        return "The video can't be played.".into();
    }
    if line.chars().count() > 160 {
        format!("{}…", line.chars().take(160).collect::<String>())
    } else {
        line.to_string()
    }
}

enum Chunk {
    Frame(Vec<u8>),
    Wait,
    End,
}

fn poll_jpeg(reader: &mut ChildStdout, buf: &mut Vec<u8>) -> std::io::Result<Chunk> {
    if let Some(frame) = take_jpeg(buf) {
        return Ok(Chunk::Frame(frame));
    }
    let mut tmp = [0u8; 16 * 1024];
    match reader.read(&mut tmp) {
        Ok(0) => Ok(Chunk::End),
        Ok(n) => {
            buf.extend_from_slice(&tmp[..n]);
            if let Some(frame) = take_jpeg(buf) {
                Ok(Chunk::Frame(frame))
            } else {
                Ok(Chunk::Wait)
            }
        }
        Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => {
            Ok(Chunk::Wait)
        }
        Err(e) => Err(e),
    }
}

/// Pulls one JPEG (from `FF D8` through `FF D9`) out of `buf`. A partial frame
/// stays in the buffer.
fn take_jpeg(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let start = match buf.windows(2).position(|w| w == [0xFF, 0xD8]) {
        Some(start) => start,
        None => {
            if buf.len() > 1 {
                let keep = usize::from(buf[buf.len() - 1] == 0xFF);
                buf.drain(..buf.len() - keep);
            }
            return None;
        }
    };
    let Some(end_rel) = buf[start + 2..].windows(2).position(|w| w == [0xFF, 0xD9]) else {
        if start > 0 {
            buf.drain(..start);
        }
        return None;
    };
    let end = start + 2 + end_rel + 2;
    Some(buf.drain(..end).skip(start).collect())
}

fn decode_jpeg(bytes: &[u8]) -> Option<Arc<RenderImage>> {
    let mut data = image::load_from_memory(bytes).ok()?.into_rgba8();
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(data)])))
}

fn set_nonblocking(fd: i32) -> bool {
    unsafe {
        let flags = fcntl(fd, F_GETFL);
        if flags < 0 {
            return false;
        }
        fcntl(fd, F_SETFL, flags | O_NONBLOCK) >= 0
    }
}

const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0x0004;

unsafe extern "C" {
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_jpegs_out_of_a_byte_stream() {
        let mut buf = vec![0x00, 0xFF];
        assert!(take_jpeg(&mut buf).is_none());
        assert_eq!(buf, vec![0xFF]);
        buf.extend_from_slice(&[0xD8, 0x01, 0xFF, 0xD9, 0xFF, 0xD8, 0x02, 0xFF, 0xD9]);
        assert_eq!(
            take_jpeg(&mut buf).unwrap(),
            vec![0xFF, 0xD8, 0x01, 0xFF, 0xD9]
        );
        assert_eq!(
            take_jpeg(&mut buf).unwrap(),
            vec![0xFF, 0xD8, 0x02, 0xFF, 0xD9]
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn holds_a_jpeg_until_its_end_marker() {
        let mut buf = vec![0xFF, 0xD8, 0x01, 0xFF];
        assert!(take_jpeg(&mut buf).is_none());
        assert_eq!(buf, vec![0xFF, 0xD8, 0x01, 0xFF]);
        buf.push(0xD9);
        assert_eq!(
            take_jpeg(&mut buf).unwrap(),
            vec![0xFF, 0xD8, 0x01, 0xFF, 0xD9]
        );
    }

    #[test]
    fn overlay_prefers_the_translation() {
        let line = overlay_line(true, "Hello", "你好").unwrap();
        assert_eq!(line.primary, "你好");
        assert_eq!(line.secondary.as_deref(), Some("Hello"));
        let same = overlay_line(true, "Hello", "Hello").unwrap();
        assert!(same.secondary.is_none());
        let orig = overlay_line(false, "Hello", "你好").unwrap();
        assert_eq!(orig.primary, "Hello");
        assert!(orig.secondary.is_none());
        assert!(overlay_line(true, "  ", "  ").is_none());
    }

    #[test]
    fn decodes_one_frame_of_a_color_source() {
        let Some(ffmpeg) = crate::media::tool("ffmpeg") else {
            return;
        };
        let path = std::env::temp_dir().join(format!("taurus-vid-{}.mp4", std::process::id()));
        let built = Command::new(ffmpeg)
            .args([
                "-y",
                "-hide_banner",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=160x90:d=0.4",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "mpeg4",
            ])
            .arg(&path)
            .status();
        if built.is_err() || !built.unwrap().success() || !path.is_file() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let deck = VideoDeck::start(path.clone());
        let mut saw = false;
        for _ in 0..30 {
            if deck.frame().is_some() {
                saw = true;
                break;
            }
            if deck.error().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let err = deck.error();
        drop(deck);
        thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_file(&path);
        assert!(saw, "no frame arrived: {}", err.unwrap_or_default());
    }
}
