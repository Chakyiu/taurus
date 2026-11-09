//! End-to-end checks against real tools: ffmpeg, a downloaded Whisper model
//! and an HTTP translation endpoint. They touch the network and the disk, so
//! they only run on request: `cargo test e2e -- --ignored --nocapture`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::engine::asr::Engine;
use crate::engine::{AsrChoice, Settings, WhisperModel, mt};
use crate::media;

const LINES: [&str; 2] = [
    "Good morning everyone, and welcome to the quarterly product review.",
    "Today we will look at the new decoder and how it cuts latency in half.",
];

fn fixture_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".shots/e2e");
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A short black video with spoken English and an English subtitle track.
fn sample_video() -> PathBuf {
    let dir = fixture_dir();
    let out = dir.join("demo.mp4");
    if out.exists() {
        return out;
    }
    let speech = dir.join("speech.aiff");
    let ok = Command::new("say")
        .args(["-v", "Samantha", "-o"])
        .arg(&speech)
        .arg(LINES.join(" "))
        .status()
        .unwrap()
        .success();
    assert!(ok, "say failed");
    let srt = dir.join("demo.en.srt");
    std::fs::write(
        &srt,
        format!(
            "1\n00:00:00,200 --> 00:00:03,800\n{}\n\n2\n00:00:04,000 --> 00:00:07,000\n{}\n",
            LINES[0], LINES[1]
        ),
    )
    .unwrap();
    let ffmpeg = media::tool("ffmpeg").expect("ffmpeg");
    let ok = Command::new(ffmpeg)
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", "color=c=black:s=640x360:r=10"])
        .arg("-i")
        .arg(&speech)
        .arg("-i")
        .arg(&srt)
        .args([
            "-map", "0:v", "-map", "1:a", "-map", "2:s", "-shortest", "-c:v", "libx264", "-c:a",
            "aac", "-c:s", "mov_text", "-metadata:s:s:0", "language=eng",
        ])
        .arg(&out)
        .status()
        .unwrap()
        .success();
    assert!(ok, "ffmpeg could not build the sample");
    out
}

#[test]
#[ignore]
fn e2e_probe_and_extract_subtitles() {
    let video = sample_video();
    let p = media::probe(&video).unwrap();
    println!("probe: {:?} · {} ms · subs {:?} · audio {:?}", p.video, p.duration_ms, p.subs, p.audio);
    assert_eq!(p.subs.len(), 1);
    assert_eq!(p.audio.len(), 1);
    assert_eq!(p.subs[0].cues, Some(2));
    let segs = media::extract_subtitle(&video, p.subs[0].stream).unwrap();
    assert_eq!(segs[1].orig, LINES[1]);
}

fn whisper_settings() -> Settings {
    let m = WhisperModel::find("tiny").unwrap();
    if !m.is_downloaded() {
        let cancel = AtomicBool::new(false);
        let last = std::sync::atomic::AtomicU32::new(0);
        m.download(
            |p| {
                let pct = (p * 10.0) as u32;
                if pct != last.swap(pct, std::sync::atomic::Ordering::Relaxed) {
                    println!("download {}%", pct * 10);
                }
            },
            &cancel,
        )
        .unwrap();
    }
    Settings {
        asr: AsrChoice::Whisper,
        whisper_model: "tiny".into(),
        ..Settings::default()
    }
}

#[test]
#[ignore]
fn e2e_whisper_transcribes_the_audio_track() {
    let video = sample_video();
    let settings = whisper_settings();
    let p = media::probe(&video).unwrap();
    let pcm = media::decode_audio(&video, Some(p.audio[0].stream), |_| {}, || false).unwrap();
    println!("decoded {} s", pcm.len() / media::SAMPLE_RATE);
    let engine = Engine::load(&settings).unwrap();
    let t = std::time::Instant::now();
    let segs = engine
        .transcribe_long(&pcm, "en", Arc::new(|_| {}), &|| false)
        .unwrap();
    for s in &segs {
        println!("{:>6}–{:>6}  {}", s.start_ms, s.end_ms, s.text);
    }
    println!("transcribed in {:?}", t.elapsed());
    let all = segs.iter().map(|s| s.text.to_lowercase()).collect::<Vec<_>>().join(" ");
    assert!(all.contains("quarterly") || all.contains("product"), "{all}");
    assert!(all.contains("decoder") || all.contains("latency"), "{all}");
    println!("{}", Engine::test(&settings).unwrap());
    drop(engine);
    assert!(!crate::engine::asr::unload());
}

/// Uses whichever Qwen3-ASR model is already downloaded; set `TAURUS_QWEN_WAV`
/// to also transcribe a 16 kHz mono 16-bit file.
#[test]
#[ignore]
fn e2e_qwen_transcribes_on_this_mac() {
    let Some(m) = crate::engine::QWEN_MODELS.iter().rev().find(|m| m.is_downloaded()) else {
        println!("no Qwen3-ASR model downloaded; skipping");
        return;
    };
    let settings = Settings { asr: AsrChoice::QwenLocal, qwen_local_model: m.id.into(), ..Settings::default() };
    let t = std::time::Instant::now();
    let engine = Engine::load(&settings).unwrap();
    println!("loaded Qwen3-ASR {} in {:?}", m.label, t.elapsed());

    if let Ok(wav) = std::env::var("TAURUS_QWEN_WAV") {
        let bytes = std::fs::read(wav).unwrap();
        let pcm: Vec<f32> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
        let t = std::time::Instant::now();
        let segs = engine.transcribe(&pcm, "", false, None).unwrap();
        println!("{} s of audio in {:?}: {:?}", pcm.len() / media::SAMPLE_RATE, t.elapsed(), segs.first().map(|s| &s.text));
    }

    let video = sample_video();
    let p = media::probe(&video).unwrap();
    let pcm = media::decode_audio(&video, Some(p.audio[0].stream), |_| {}, || false).unwrap();
    let t = std::time::Instant::now();
    let segs = engine.transcribe_long(&pcm, "en", Arc::new(|_| {}), &|| false).unwrap();
    for s in &segs {
        println!("{:>6}–{:>6}  {}", s.start_ms, s.end_ms, s.text);
    }
    println!("transcribed {} s in {:?}", pcm.len() / media::SAMPLE_RATE, t.elapsed());
    let all = segs.iter().map(|s| s.text.to_lowercase()).collect::<Vec<_>>().join(" ");
    assert!(all.contains("quarterly") && all.contains("decoder"), "{all}");
    assert!(segs.len() >= 2, "each sentence should be its own cue");
}

/// A stand-in for an OpenAI-compatible server: replies to each chat request
/// with the numbered lines it was sent, prefixed, wrapped in a think block
/// like reasoning models do.
fn mock_chat_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0usize;
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let user = req["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .to_string();
            let lines: Vec<String> = serde_json::from_str::<serde_json::Value>(&user)
                .ok()
                .and_then(|v| v["lines"].as_array().cloned())
                .unwrap_or_default()
                .into_iter()
            .map(|v| format!("译:{}", v.as_str().unwrap_or("")))
            .collect();
            let content = format!("<think>ok</think>{}", serde_json::json!({ "t": lines }));
            let reply = serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": content } }]
            })
            .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
        }
    });
    format!("http://{addr}/v1")
}

#[test]
#[ignore]
fn e2e_translates_through_a_chat_endpoint() {
    let url = mock_chat_server();
    let cfg = mt::MtConfig { url, model: "mock".into(), key: String::new() };
    let lines: Vec<String> = LINES.iter().map(|s| s.to_string()).collect();
    let rules = vec!["Translate \"decoder\" as \"解码器\".".to_string()];
    let out = mt::translate(&cfg, "en", "zh", &rules, &[], &lines).unwrap();
    println!("{out:?}");
    assert_eq!(out.len(), 2);
    assert!(out[1].starts_with("译:") && out[1].contains("decoder"));
    assert!(mt::test(&cfg, "zh").unwrap().starts_with("译:"));
}

/// Two synthetic voices taking turns: the speakers should come back as the
/// same alternation.
#[test]
#[ignore]
fn e2e_tells_two_voices_apart() {
    use crate::engine::speaker;
    if !speaker::is_downloaded() {
        let cancel = AtomicBool::new(false);
        speaker::download(|_| {}, &cancel).unwrap();
    }
    let dir = fixture_dir();
    let turns = [
        ("Samantha", "Hello everyone, thanks for joining the call this morning."),
        ("Daniel", "Thanks, it is good to be here. Shall we start with the budget?"),
        ("Samantha", "Yes, the budget is first, then we will talk about hiring."),
        ("Daniel", "Great. I have the numbers from last quarter ready to share."),
    ];
    let mut pcm = Vec::new();
    let mut spans = Vec::new();
    for (i, (voice, text)) in turns.iter().enumerate() {
        let aiff = dir.join(format!("turn{i}.aiff"));
        let ok = Command::new("say").args(["-v", voice, "-o"]).arg(&aiff).arg(text).status().unwrap();
        assert!(ok.success());
        let a = media::decode_audio(&aiff, None, |_| {}, || false).unwrap();
        let start = (pcm.len() / 16) as i64;
        pcm.extend(&a);
        spans.push((start, (pcm.len() / 16) as i64));
        pcm.extend(std::iter::repeat_n(0.0, 8000));
        let _ = std::fs::remove_file(aiff);
    }
    let model = speaker::SpeakerModel::load().unwrap();
    let t = std::time::Instant::now();
    let labels = speaker::identify(&model, &pcm, &spans, |_| {}, &AtomicBool::new(false)).unwrap();
    println!("{labels:?} in {:?}", t.elapsed());
    assert_eq!(labels, vec![0, 1, 0, 1]);
}

/// Records the whole screen (or `TAURUS_SCREEN_APP`) with sound for a few
/// seconds; the movie should come out playable, with video and audio tracks.
#[test]
#[ignore]
fn e2e_records_the_screen() {
    use crate::capture;
    if !capture::can_record_screen() {
        println!("needs macOS 15; skipping");
        return;
    }
    let path = fixture_dir().join("screen.mp4");
    let target = std::env::var("TAURUS_SCREEN_APP").unwrap_or_else(|_| capture::WHOLE_SCREEN.into());
    let quality = std::env::var("TAURUS_SCREEN_QUALITY")
        .ok()
        .and_then(|k| capture::VideoQuality::from_key(&k))
        .unwrap_or(capture::VideoQuality::Standard);
    let secs = std::env::var("TAURUS_SCREEN_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    let rec = capture::record_screen(&target, quality, true, false, &path).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(secs));
    drop(rec);
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name,width,height,avg_frame_rate:format=duration", "-of", "compact"])
        .arg(&path)
        .output()
        .unwrap();
    let info = String::from_utf8_lossy(&out.stdout).to_string();
    println!("{info}{} bytes", std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0));
    assert!(info.contains("codec_type=video"), "{info}");
    assert!(info.contains("codec_type=audio"), "{info}");
    let secs: f64 = info.split("duration=").nth(1).and_then(|d| d.trim().parse().ok()).unwrap_or(0.0);
    assert!(secs > 2.5, "{secs} s");
}
