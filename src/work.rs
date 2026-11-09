//! Background work. Each job runs on its own thread and reports through one
//! channel, which the workbench drains every 100 ms on the UI thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use gpui::{Context, Window};

use crate::app::*;
use crate::capture::{self, CaptureKind};
use crate::data::*;
use crate::engine::asr::{AsrSeg, Engine};
use crate::engine::{AsrChoice, QwenModel, Settings, WhisperModel, friendly, model_name, mt, speaker};
use crate::live::{self, LiveEvent};
use crate::media::{self, Probe};
use crate::store;
use crate::subs::{self, is_cjk};

pub enum Bg {
    Lines { pid: String, job: u64, lines: Vec<(usize, String)> },
    MtFailed { pid: String, job: u64, error: String },
    MtDone { pid: String, job: u64 },
    LiveLine { pid: String, seg: usize, result: Result<String, String> },
    Probed { job: u64, result: Result<Probe, String> },
    Stage { job: u64, stage: usize, state: StageState, note: String },
    Extracted { job: u64, segs: Vec<Segment> },
    ExtractFailed { job: u64, error: String },
    Download { id: String, pct: f32 },
    Downloaded { id: String, result: Result<(), String> },
    AsrTest(Result<String, String>),
    MtTest(Result<String, String>),
    Playback { pid: String, result: Result<Vec<f32>, String> },
    SpeakerProgress { pid: String, done: usize },
    Speakers { pid: String, result: Result<Vec<usize>, String> },
    SummaryProgress { job: u64, done: usize, parts: usize },
    Summary { pid: String, job: u64, result: Result<String, String> },
    SummaryModels(Result<Vec<String>, String>),
}

pub struct SpeakerJob {
    pub pid: String,
    pub done: usize,
    pub total: usize,
    cancel: Arc<AtomicBool>,
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(format!("taurus-{name}")).spawn(f);
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

impl Workbench {
    // ─── the pump ──────────────────────────────────────────

    pub(crate) fn pump(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(msg) = self.bg_rx.try_recv() {
            changed = true;
            self.handle(msg, cx);
        }
        changed |= self.pump_live(cx);
        if self.live.status == LiveStatus::Running {
            changed = true;
        }
        changed |= self.follow_playback();
        if self.last_save.elapsed() > Duration::from_millis(1500) {
            self.last_save = Instant::now();
            self.autosave(cx);
        }
        if changed {
            cx.notify();
        }
    }

    fn handle(&mut self, msg: Bg, cx: &mut Context<Self>) {
        match msg {
            Bg::Lines { pid, job, lines } => self.apply_lines(&pid, job, lines),
            Bg::MtDone { pid, job } => self.mt_done(&pid, job, cx),
            Bg::MtFailed { pid, job, error } => self.mt_failed(&pid, job, error, cx),
            Bg::LiveLine { pid, seg, result } => self.live_translated(&pid, seg, result, cx),
            Bg::Playback { pid, result } => self.playback_loaded(pid, result, cx),
            Bg::Probed { job, result } => {
                self.probed(job, result);
                if cfg!(debug_assertions) {
                    if let Ok(which) = std::env::var("TAURUS_EXTRACT_RUN") {
                        let key = self.extract.tracks().into_iter().filter_map(|r| r.key).find(|k| {
                            matches!((which.as_str(), k), ("audio", TrackKey::Audio(_)) | ("sub", TrackKey::Sub(_)))
                        });
                        if let Some(k) = key {
                            self.select_track(k);
                            self.extract_run(cx);
                        }
                    }
                }
            }
            Bg::Stage { job, stage, state, note } => {
                if job == self.extract.job {
                    if let Some(s) = self.extract.stages.get_mut(stage) {
                        s.state = state;
                        s.note = note;
                    }
                }
            }
            Bg::Extracted { job, segs } => self.extracted(job, segs, cx),
            Bg::ExtractFailed { job, error } => self.extract_failed(job, error, cx),
            Bg::Download { id, pct } => {
                if let Some(d) = self.downloads.get_mut(&id) {
                    d.pct = pct;
                }
            }
            Bg::Downloaded { id, result } => {
                self.downloads.remove(&id);
                if id == speaker::SPEAKER_DOWNLOAD_ID {
                    if let Some(pid) = self.speaker_pending.take() {
                        if result.is_ok() {
                            self.run_speakers(pid, cx);
                            return;
                        }
                    }
                }
                match result {
                    Ok(()) => self.toast(format!("{} is downloaded and ready.", model_name(&id)), ToastKind::Ok, None, cx),
                    Err(e) if e == "cancelled" => {}
                    Err(e) => self.toast(format!("The {} download failed: {e}.", model_name(&id)), ToastKind::Bad, None, cx),
                }
            }
            Bg::SpeakerProgress { pid, done } => {
                if let Some(j) = self.speaking.as_mut().filter(|j| j.pid == pid) {
                    j.done = done;
                }
            }
            Bg::Speakers { pid, result } => self.speakers_found(pid, result, cx),
            Bg::SummaryProgress { job, done, parts } => {
                if let Some(j) = self.summary_job.as_mut().filter(|j| j.id == job) {
                    j.done = done;
                    j.parts = parts;
                }
            }
            Bg::Summary { pid, job, result } => self.summary_arrived(pid, job, result, cx),
            Bg::SummaryModels(result) => self.summary_models(result, cx),
            Bg::AsrTest(r) => {
                if let Some(Dialog::Engines { asr_test, testing_asr, .. }) = &mut self.dialog {
                    *asr_test = Some(r);
                    *testing_asr = false;
                }
            }
            Bg::MtTest(r) => {
                if let Some(Dialog::Engines { mt_test, testing_mt, .. }) = &mut self.dialog {
                    *mt_test = Some(r);
                    *testing_mt = false;
                }
            }
        }
    }

    // ─── saving ────────────────────────────────────────────

    fn library_json(&self) -> String {
        store::library_json(&store::LibraryRef {
            projects: &self.projects,
            glossary: &self.glossary,
        })
    }

    fn autosave(&mut self, cx: &mut Context<Self>) {
        let json = self.library_json();
        if json == self.saved_json {
            return;
        }
        match store::write_library(&json) {
            Ok(()) => {
                self.saved_json = json;
                self.save_failed = false;
            }
            Err(e) => {
                if !self.save_failed {
                    self.save_failed = true;
                    self.toast(format!("Could not save your projects: {e:#}."), ToastKind::Bad, None, cx);
                }
            }
        }
    }

    pub(crate) fn save_now(&mut self) {
        let json = self.library_json();
        if json != self.saved_json && store::write_library(&json).is_ok() {
            self.saved_json = json;
        }
    }

    // ─── translation ───────────────────────────────────────

    /// Translation and summaries share one endpoint. Without one the user is pointed at it.
    pub(crate) fn require_mt(&mut self, cx: &mut Context<Self>) -> bool {
        self.require_endpoint("Translation", cx)
    }

    fn require_endpoint(&mut self, what: &str, cx: &mut Context<Self>) -> bool {
        if self.settings.mt_ready() {
            return true;
        }
        self.toast(
            format!("{what} is not set up yet. Point Taurus at an OpenAI-compatible endpoint first."),
            ToastKind::Warn,
            Some(("Set up engines", ToastAction::Engines)),
            cx,
        );
        false
    }

    /// Translates the given segments, or every untranslated one.
    pub(crate) fn translate_project(&mut self, pid: &str, indices: Option<Vec<usize>>, cx: &mut Context<Self>) {
        let Some(p) = self.project(pid) else { return };
        let retranslate = indices.is_some();
        let idx: Vec<usize> = match indices {
            Some(v) => v.into_iter().filter(|&i| i < p.segs.len()).collect(),
            None => (0..p.segs.len())
                .filter(|&i| p.segs[i].trans.trim().is_empty() && !p.segs[i].orig.trim().is_empty())
                .collect(),
        };
        if idx.is_empty() {
            let g = self.glossary.clone();
            if let Some(p) = self.project_mut(pid) {
                if p.status == Status::Working {
                    p.status = Status::Review;
                }
                settle_status(&g, p);
            }
            return;
        }
        let origs: Vec<String> = p.segs.iter().map(|s| s.orig.clone()).collect();
        let (from, to) = (p.from.clone(), p.to.clone());
        let terms: Vec<&Term> = active_glossary(&self.glossary, Some(pid)).collect();
        let rules = mt::glossary_rules(&terms);
        let cfg = self.settings.mt();

        self.next_id += 1;
        let job = self.next_id;
        let cancel = Arc::new(AtomicBool::new(false));
        let old = self.jobs.insert(
            pid.to_string(),
            MtJob {
                id: job,
                done: 0,
                total: idx.len(),
                retranslate,
                filled: 0,
                changed: 0,
                cancel: cancel.clone(),
            },
        );
        if let Some(old) = old {
            old.cancel.store(true, Ordering::Relaxed);
        }
        if !retranslate {
            if let Some(p) = self.project_mut(pid) {
                p.status = Status::Working;
                p.progress = p.translated_pct();
            }
        }
        let tx = self.bg_tx.clone();
        let pid = pid.to_string();
        spawn("translate", move || {
            for chunk in idx.chunks(mt::BATCH) {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let first = chunk[0];
                let before = origs[first.saturating_sub(3)..first].to_vec();
                let lines: Vec<String> = chunk.iter().map(|&i| origs[i].clone()).collect();
                match mt::translate(&cfg, &from, &to, &rules, &before, &lines) {
                    Ok(out) => {
                        let lines = chunk.iter().copied().zip(out).collect();
                        let _ = tx.send(Bg::Lines { pid: pid.clone(), job, lines });
                    }
                    Err(e) => {
                        let _ = tx.send(Bg::MtFailed {
                            pid: pid.clone(),
                            job,
                            error: friendly(&e, &cfg.url),
                        });
                        return;
                    }
                }
            }
            let _ = tx.send(Bg::MtDone { pid, job });
        });
        cx.notify();
    }

    fn apply_lines(&mut self, pid: &str, job: u64, lines: Vec<(usize, String)>) {
        if self.jobs.get(pid).is_none_or(|j| j.id != job) {
            return;
        }
        let g = self.glossary.clone();
        let editing = (self.project_id.as_deref() == Some(pid)).then_some(self.editor.editing).flatten();
        let n = lines.len();
        let (mut filled, mut changed) = (0, 0);
        let here = self.project_id.as_deref() == Some(pid);
        for (i, _) in &lines {
            if here {
                self.editor.loading.remove(i);
            }
        }
        if let Some(p) = self.project_mut(pid) {
            for (i, text) in lines {
                if editing == Some(i) {
                    continue;
                }
                let Some(s) = p.segs.get_mut(i) else { continue };
                let (t, _) = apply_rules(&g, text.trim(), Some(pid));
                if s.trans.trim().is_empty() && !t.is_empty() {
                    filled += 1;
                } else if s.trans != t {
                    changed += 1;
                }
                s.base = text.trim().to_string();
                s.trans = t;
            }
            p.progress = p.translated_pct();
        }
        let Some(j) = self.jobs.get_mut(pid) else { return };
        j.done += n;
        j.filled += filled;
        j.changed += changed;
        let (done, total, retranslate) = (j.done, j.total, j.retranslate);
        if retranslate && self.project_id.as_deref() == Some(pid) && done >= total {
            self.editor.loading.clear();
        }
        if self.extract.project_id.as_deref() == Some(pid) {
            if let Some(s) = self.extract.stages.get_mut(3) {
                s.note = format!("{done} of {total}");
            }
        }
    }

    fn mt_done(&mut self, pid: &str, job: u64, cx: &mut Context<Self>) {
        if self.jobs.get(pid).is_none_or(|j| j.id != job) {
            return;
        }
        let Some(j) = self.jobs.remove(pid) else { return };
        let g = self.glossary.clone();
        let Some(p) = self.project_mut(pid) else { return };
        if p.status == Status::Working {
            p.status = Status::Review;
        }
        settle_status(&g, p);
        let name = p.name.clone();
        let n = p.segs.len();
        let open = project_stats(&g, p).flagged;
        if j.retranslate {
            if self.project_id.as_deref() == Some(pid) {
                self.editor.loading.clear();
            }
            let mut parts = Vec::new();
            if j.filled > 0 {
                parts.push(format!("{} filled", plural(j.filled, "empty segment", "empty segments")));
            }
            if j.changed > 0 {
                parts.push(format!("{} reworded", plural(j.changed, "segment", "segments")));
            }
            if parts.is_empty() {
                self.toast("Translated again; the wording came out the same.", ToastKind::Ok, None, cx);
            } else {
                self.toast(format!("{}.", parts.join(" · ")), ToastKind::Ok, None, cx);
            }
            return;
        }
        let tail = match open {
            0 => "all clear".to_string(),
            k => format!("{k} needing review"),
        };
        if self.extract.project_id.as_deref() == Some(pid) {
            if let Some(s) = self.extract.stages.get_mut(3) {
                s.state = StageState::Done;
                s.note = format!("{n} of {n}");
            }
            let file = self.extract.file.as_ref().map(|f| f.name.clone()).unwrap_or_default();
            self.toast(format!("Finished “{name}” from {file} · {n} segments, {tail}."), ToastKind::Ok, None, cx);
            return;
        }
        self.toast(
            format!("Translated “{name}” · {n} segments, {tail}."),
            ToastKind::Ok,
            Some(("Open", ToastAction::Open(pid.to_string()))),
            cx,
        );
    }

    fn mt_failed(&mut self, pid: &str, job: u64, error: String, cx: &mut Context<Self>) {
        if self.jobs.get(pid).is_none_or(|j| j.id != job) {
            return;
        }
        self.jobs.remove(pid);
        if self.project_id.as_deref() == Some(pid) {
            self.editor.loading.clear();
        }
        let g = self.glossary.clone();
        let mut name = String::new();
        if let Some(p) = self.project_mut(pid) {
            if p.status == Status::Working {
                p.status = Status::Review;
            }
            settle_status(&g, p);
            name = p.name.clone();
        }
        if self.extract.project_id.as_deref() == Some(pid) {
            if let Some(s) = self.extract.stages.get_mut(3) {
                s.state = StageState::Failed;
                s.note = "stopped".into();
            }
            self.extract.failed = Some(format!("Translation stopped: {error}. The track itself is saved in the project."));
        }
        self.toast(
            format!("Translation of “{name}” stopped: {error}."),
            ToastKind::Bad,
            Some(("Engines", ToastAction::Engines)),
            cx,
        );
    }

    // ─── import ────────────────────────────────────────────

    pub fn import_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut made = Vec::new();
        for path in paths {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "subtitles".into());
            if !subs::is_subtitle(&name) {
                self.toast(
                    format!("{name} is not a subtitle file. Taurus reads .srt, .vtt, .ass and .ssa."),
                    ToastKind::Warn,
                    None,
                    cx,
                );
                continue;
            }
            let segs = std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|b| subs::parse(&name, &decode_text(&b)));
            let segs = match segs {
                Ok(s) if !s.is_empty() => s,
                Ok(_) => {
                    self.toast(format!("{name} has no cues in it."), ToastKind::Warn, None, cx);
                    continue;
                }
                Err(e) => {
                    self.toast(format!("Could not read {name}: {e:#}."), ToastKind::Bad, None, cx);
                    continue;
                }
            };
            let from = subs::guess_lang(&name, &segs);
            let to = if self.extract.to != from {
                self.extract.to.clone()
            } else if from == "en" {
                "zh".into()
            } else {
                "en".into()
            };
            let id = self.fresh_id("p");
            let p = Project::new(
                id.clone(),
                Kind::Subtitle,
                title_from_file(&name),
                name.clone(),
                Some(path.to_string_lossy().to_string()),
                Status::Working,
                &from,
                &to,
                segs,
            );
            let mut p = p;
            p.translate = self.settings.translate;
            self.projects.insert(0, p);
            made.push(id);
        }
        let Some(last) = made.last().cloned() else { return };
        self.project_id = Some(last.clone());
        let ready = self.settings.mt_ready();
        for id in &made {
            let translate = self.project(id).is_some_and(|p| p.translate);
            if translate && ready {
                self.translate_project(id, None, cx);
            } else if let Some(p) = self.project_mut(id) {
                p.status = Status::Review;
            }
        }
        self.settle_all();
        self.save_now();
        let p = self.project(&last).expect("just imported");
        let what = if made.len() == 1 {
            if p.translate {
                format!("“{}” · {} cues, {} → {}", p.name, p.segs.len(), p.from, p.to)
            } else {
                format!("“{}” · {} cues", p.name, p.segs.len())
            }
        } else {
            format!("{} files", made.len())
        };
        if ready {
            self.toast(format!("Imported {what}. Translating now."), ToastKind::Ok, Some(("Open", ToastAction::Open(last))), cx);
        } else {
            self.toast(
                format!("Imported {what}. Set up translation to fill in the other language."),
                ToastKind::Warn,
                Some(("Set up engines", ToastAction::Engines)),
                cx,
            );
        }
    }

    // ─── extract ───────────────────────────────────────────

    pub fn pick_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "video".into());
        if subs::is_subtitle(&name) {
            self.import_paths(vec![path], cx);
            return;
        }
        self.extract.cancel.store(true, Ordering::Relaxed);
        self.extract.cancel = Arc::new(AtomicBool::new(false));
        self.next_id += 1;
        let job = self.next_id;
        self.extract.job = job;
        self.extract.file = Some(PickedFile {
            name,
            size: media::file_size_label(&path),
            path: path.clone(),
        });
        self.extract.probe = None;
        self.extract.scan_error = None;
        self.extract.track = None;
        self.extract.project_id = None;
        self.extract.failed = None;
        self.dialog = Some(Dialog::Extract);
        let tx = self.bg_tx.clone();
        spawn("probe", move || {
            let result = media::probe(&path).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Bg::Probed { job, result });
        });
        cx.notify();
    }

    fn probed(&mut self, job: u64, result: Result<Probe, String>) {
        if job != self.extract.job {
            return;
        }
        match result {
            Ok(p) => {
                self.extract.probe = Some(p);
                let rows = self.extract.tracks();
                // A text track in another language than the target is the fast path.
                let to = self.extract.to.clone();
                let pick = rows
                    .iter()
                    .filter_map(|r| r.key)
                    .find(|k| matches!(k, TrackKey::Sub(_)) && self.track_lang(*k).as_deref() != Some(to.as_str()))
                    .or_else(|| rows.iter().filter_map(|r| r.key).find(|k| matches!(k, TrackKey::Audio(_))))
                    .or_else(|| rows.iter().find_map(|r| r.key));
                if let Some(k) = pick {
                    self.select_track(k);
                }
            }
            Err(e) => self.extract.scan_error = Some(e),
        }
    }

    fn track_lang(&self, key: TrackKey) -> Option<String> {
        let p = self.extract.probe.as_ref()?;
        let tag = match key {
            TrackKey::Sub(s) => p.subs.iter().find(|t| t.stream == s)?.lang.clone(),
            TrackKey::Audio(s) => p.audio.iter().find(|t| t.stream == s)?.lang.clone(),
        }?;
        subs::lang_from_tag(&tag).map(String::from)
    }

    pub fn select_track(&mut self, key: TrackKey) {
        self.extract.track = Some(key);
        if let Some(lang) = self.track_lang(key) {
            self.extract.from = lang;
            if self.extract.to == self.extract.from {
                self.extract.to = if self.extract.from == "en" { "zh".into() } else { "en".into() };
            }
        }
    }

    pub fn extract_reset(&mut self, cx: &mut Context<Self>) {
        self.extract.cancel.store(true, Ordering::Relaxed);
        self.next_id += 1;
        self.extract.job = self.next_id;
        self.extract.file = None;
        self.extract.probe = None;
        self.extract.scan_error = None;
        self.extract.failed = None;
        self.extract.project_id = None;
        cx.notify();
    }

    /// Stops an extraction from its project's workspace. The project had
    /// nothing in it yet, so it goes too.
    pub fn cancel_extract(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.extract.project_id.clone() else { return };
        self.extract_reset(cx);
        self.remove_project(&pid);
        self.show_view(View::Projects, window, cx);
        self.toast("Cancelled the extraction.", ToastKind::Ok, None, cx);
    }

    /// Whether `pid` is the project an extraction is still filling.
    pub fn extracting(&self, pid: &str) -> bool {
        self.extract.project_id.as_deref() == Some(pid)
            && self.extract.failed.is_none()
            && self.project(pid).is_some_and(|p| p.segs.is_empty() && p.status == Status::Working)
    }

    pub fn extract_run(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.extract.track else { return };
        let Some(path) = self.extract.file.as_ref().map(|f| f.path.clone()) else { return };
        let transcribing = matches!(track, TrackKey::Audio(_));
        if transcribing && !self.settings.asr_ready() {
            self.toast(
                "Speech recognition is not set up yet. Download a Whisper model or point Taurus at a Qwen3-ASR server.",
                ToastKind::Warn,
                Some(("Set up engines", ToastAction::Engines)),
                cx,
            );
            return;
        }
        let duration = self.extract.probe.as_ref().map(|p| p.duration_ms).unwrap_or(0);
        let stage = |label: &str, state, note: String| Stage {
            label: label.into(),
            note,
            state,
        };
        self.extract.stages = vec![
            stage(
                "Reading container",
                StageState::Done,
                if duration > 0 { srt_time(duration)[..8].to_string() } else { "read".into() },
            ),
            stage(
                if transcribing { "Transcribing audio" } else { "Decoding the subtitle track" },
                StageState::Run,
                String::new(),
            ),
            stage("Re-timing and splitting long cues", StageState::Wait, String::new()),
        ];
        let translate = self.settings.translate;
        if translate {
            self.extract.stages.push(stage("Translating with the glossary", StageState::Wait, String::new()));
        }
        self.extract.transcribing = transcribing;
        self.extract.failed = None;
        let file = self.extract.file.as_ref().expect("a file was picked");
        let (name, file_path) = (file.name.clone(), file.path.to_string_lossy().to_string());
        let id = self.fresh_id("p");
        let p = Project::new(
            id.clone(),
            Kind::Video,
            title_from_file(&name),
            name,
            Some(file_path),
            Status::Working,
            &self.extract.from,
            &self.extract.to,
            Vec::new(),
        );
        let mut p = p;
        p.translate = translate;
        self.projects.insert(0, p);
        self.extract.project_id = Some(id.clone());
        self.project_id = Some(id);
        self.reset_editor_view();
        self.dialog = None;
        self.view = View::Editor;
        self.extract.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.extract.cancel = cancel.clone();
        self.next_id += 1;
        let job = self.next_id;
        self.extract.job = job;

        let tx = self.bg_tx.clone();
        let settings = self.settings.clone();
        let lang = self.extract.from.clone();
        spawn("extract", move || {
            let note = |stage: usize, state: StageState, note: String| {
                let _ = tx.send(Bg::Stage { job, stage, state, note });
            };
            let result = (|| -> anyhow::Result<Vec<Segment>> {
                let mut segs = match track {
                    TrackKey::Sub(stream) => {
                        let segs = media::extract_subtitle(&path, stream)?;
                        note(1, StageState::Done, format!("{} cues", segs.len()));
                        segs
                    }
                    TrackKey::Audio(stream) => {
                        transcribe_file(&path, stream, &settings, &lang, duration, &note, &tx, job, &cancel)?
                    }
                };
                if cancel.load(Ordering::Relaxed) {
                    anyhow::bail!("cancelled");
                }
                note(2, StageState::Run, String::new());
                retime(&mut segs);
                note(2, StageState::Done, format!("{} cues", segs.len()));
                Ok(segs)
            })();
            let _ = match result {
                Ok(segs) => tx.send(Bg::Extracted { job, segs }),
                Err(e) => tx.send(Bg::ExtractFailed { job, error: format!("{e:#}") }),
            };
        });
        cx.notify();
    }

    fn extracted(&mut self, job: u64, segs: Vec<Segment>, cx: &mut Context<Self>) {
        if job != self.extract.job {
            return;
        }
        if segs.is_empty() {
            let why = if self.extract.transcribing {
                "no speech was heard in the audio track"
            } else {
                "the track had no cues in it"
            };
            self.extract_failed(job, why.into(), cx);
            return;
        }
        let Some(id) = self.extract.project_id.clone() else { return };
        let name = self.extract.file.as_ref().map(|f| f.name.clone()).unwrap_or_default();
        let n = segs.len();
        let Some(p) = self.project_mut(&id) else { return };
        p.segs = segs;
        let translate = p.translate;
        if !translate {
            p.status = Status::Review;
        }
        self.save_now();
        if !translate {
            self.settle_all();
            self.toast(format!("Created the project from {name} · {n} cues."), ToastKind::Ok, None, cx);
        } else if self.settings.mt_ready() {
            if let Some(s) = self.extract.stages.get_mut(3) {
                s.state = StageState::Run;
                s.note = format!("0 of {n}");
            }
            self.translate_project(&id, None, cx);
        } else {
            if let Some(s) = self.extract.stages.get_mut(3) {
                s.state = StageState::Done;
                s.note = "skipped".into();
            }
            if let Some(p) = self.project_mut(&id) {
                p.status = Status::Review;
            }
            self.settle_all();
            self.toast(
                format!("Created the project from {name} · {n} cues, not translated yet because translation is not set up."),
                ToastKind::Warn,
                Some(("Set up engines", ToastAction::Engines)),
                cx,
            );
        }
    }

    fn extract_failed(&mut self, job: u64, error: String, cx: &mut Context<Self>) {
        if job != self.extract.job || error == "cancelled" {
            return;
        }
        for s in self.extract.stages.iter_mut() {
            if s.state == StageState::Run {
                s.state = StageState::Failed;
                s.note = "failed".into();
            }
        }
        let error = error.trim_end_matches('.').to_string();
        self.extract.failed = Some(format!("{}.", capitalise(&error)));
        if let Some(p) = self.extract.project_id.clone().and_then(|id| self.project_mut(&id)) {
            p.status = Status::Failed;
            p.error = Some(capitalise(&error));
        }
        self.toast(format!("Extraction failed: {error}."), ToastKind::Bad, None, cx);
    }

    // ─── live ──────────────────────────────────────────────

    pub fn open_capture(&mut self, cx: &mut Context<Self>) {
        self.new_menu = false;
        self.open_select = None;
        self.warm_engine();
        self.dialog = Some(Dialog::Capture);
        cx.notify();
    }

    pub fn start_live(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        match self.live.status {
            LiveStatus::Running => return,
            LiveStatus::Paused => {
                if let Some(s) = &self.live.session {
                    s.set_paused(false);
                }
                self.live.t0 = Some(Instant::now());
                self.live.status = LiveStatus::Running;
                if let Some(p) = self.live.project_id.clone().and_then(|id| self.project_mut(&id)) {
                    p.status = Status::Live;
                }
                cx.notify();
                return;
            }
            LiveStatus::Idle => {}
        }
        if self.live.starting {
            return;
        }
        if self.live.source == Source::Screen {
            return;
        }
        if !self.settings.asr_ready() {
            self.toast(
                "Speech recognition is not set up yet. Download a Whisper model or point Taurus at a Qwen3-ASR server.",
                ToastKind::Warn,
                Some(("Set up engines", ToastAction::Engines)),
                cx,
            );
            return;
        }
        let kind = match self.live.source {
            Source::Mic => CaptureKind::Mic(Some(self.live.mic.clone())),
            Source::Both => CaptureKind::Both {
                app: Some(self.live.detail.clone()),
                mic: Some(self.live.mic.clone()),
            },
            _ => CaptureKind::System(Some(self.live.detail.clone())),
        };
        self.live.starting = true;
        self.settings.last_capture = Some(self.profile_from_live(""));
        if let Err(e) = store::save_settings(&self.settings) {
            eprintln!("saving settings: {e:#}");
        }
        let settings = self.settings.clone();
        let lang = self.live.from.clone();
        let pid = self.fresh_id("p");
        let audio = store::recording_path(&pid);
        let video = (self.live.can_video && self.live.video != capture::NO_VIDEO).then(|| live::VideoPlan {
            target: self.live.video.clone(),
            quality: self.live.video_quality,
            system_audio: self.live.source != Source::Mic,
            microphone: self.live.source != Source::System,
            path: store::video_path(&pid),
        });
        let video_path = video.as_ref().map(|v| v.path.clone());
        cx.spawn(async move |this, cx| {
            let path = audio.clone();
            let res = cx
                .background_executor()
                .spawn(async move { live::start(&settings, kind, lang, 0, &path, video) })
                .await;
            this.update(cx, |this, cx| this.live_started(res, pid, audio, video_path, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn live_started(
        &mut self,
        res: anyhow::Result<live::LiveSession>,
        pid: String,
        audio: PathBuf,
        video: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.live.starting = false;
        let session = match res {
            Ok(s) => s,
            Err(e) => {
                self.toast(format!("Could not start listening: {e:#}."), ToastKind::Bad, None, cx);
                return;
            }
        };
        self.live.reset_session();
        self.live.engine_note = Some(match self.settings.asr {
            AsrChoice::Whisper => format!("Loading Whisper {}…", self.settings.whisper_model),
            AsrChoice::QwenLocal => format!("Loading Qwen3-ASR {}…", self.settings.qwen_local_model.to_uppercase()),
            AsrChoice::Qwen => "Connecting to Qwen3-ASR…".into(),
        });
        self.start_capture_project(pid.clone(), audio, cx);
        if let (Some(v), Some(p)) = (video, self.project_mut(&pid)) {
            p.video_path = Some(v.to_string_lossy().to_string());
        }
        self.project_id = self.live.project_id.clone();
        self.reset_editor_view();
        if matches!(self.dialog, Some(Dialog::Capture)) {
            self.dialog = None;
        }
        self.view = View::Editor;
        self.live.t0 = Some(Instant::now());
        self.live.status = LiveStatus::Running;
        self.live.session = Some(session);
        if self.settings.translate && !self.settings.mt_ready() {
            self.toast(
                format!(
                    "Translation is not set up, so this capture writes {} only.",
                    lang_name(&self.live.from)
                ),
                ToastKind::Warn,
                Some(("Set up engines", ToastAction::Engines)),
                cx,
            );
        }
        cx.notify();
    }

    /// Pressing Start creates the project that will hold the track and its
    /// audio, so a capture has no unsaved state.
    fn start_capture_project(&mut self, id: String, audio: PathBuf, cx: &mut Context<Self>) {
        let now = chrono::Local::now();
        let name = format!("{} · {}", self.live.source.label(), now.format("%-d %b %H:%M"));
        let p = Project::new(
            id.clone(),
            Kind::Audio,
            name.clone(),
            self.source_label(),
            Some(audio.to_string_lossy().to_string()),
            Status::Live,
            &self.live.from,
            &self.live.to,
            Vec::new(),
        );
        let mut p = p;
        p.translate = self.settings.translate;
        self.projects.insert(0, p);
        self.live.project_id = Some(id.clone());
        self.toast(
            format!("Recording into “{name}”."),
            ToastKind::Ok,
            Some(("Rename", ToastAction::Rename(id))),
            cx,
        );
    }

    pub fn pause_live(&mut self, cx: &mut Context<Self>) {
        if self.live.status != LiveStatus::Running {
            return;
        }
        if let Some(s) = &self.live.session {
            s.set_paused(true);
        }
        self.live.offset_ms = self.live.live_ms();
        self.live.t0 = None;
        self.live.status = LiveStatus::Paused;
        self.live.level = 0.0;
        cx.notify();
    }

    /// Stopping ends the capture. The project already holds every committed
    /// line; the last words still being heard are committed as they settle.
    pub fn stop_live(&mut self, cx: &mut Context<Self>) {
        if self.live.status == LiveStatus::Idle {
            return;
        }
        self.live.offset_ms = self.live.live_ms();
        let session = self.live.session.take();
        self.end_session();
        let Some(pid) = self.live.project_id.take() else {
            cx.notify();
            return;
        };
        if let Some(s) = session {
            s.stop();
            self.live.draining = Some((pid.clone(), s));
        }
        let g = self.glossary.clone();
        let Some(p) = self.project_mut(&pid) else { return };
        p.status = Status::Review;
        settle_status(&g, p);
        let name = p.name.clone();
        let n = p.segs.len();
        let open = project_stats(&g, p).flagged;
        self.save_now();
        if n == 0 && self.live.draining.is_none() {
            self.toast(
                format!("“{name}” ended with nothing captured, so it sits empty in Projects."),
                ToastKind::Warn,
                Some(("Delete it", ToastAction::Delete(pid))),
                cx,
            );
            return;
        }
        let tail = if open > 0 { format!("{open} needing review") } else { "all clear".into() };
        self.toast(
            format!("“{name}” is in Projects · {}, {tail}.", plural(n, "segment", "segments")),
            ToastKind::Ok,
            Some(("Open", ToastAction::Open(pid))),
            cx,
        );
    }

    fn pump_live(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        let mut events = Vec::new();
        if let Some(s) = &self.live.session {
            while let Ok(e) = s.events.try_recv() {
                events.push((None, e));
            }
        }
        if let Some((pid, s)) = &self.live.draining {
            while let Ok(e) = s.events.try_recv() {
                events.push((Some(pid.clone()), e));
            }
        }
        for (drain_pid, e) in events {
            changed = true;
            let live = drain_pid.is_none();
            match e {
                LiveEvent::Ready => self.live.engine_note = None,
                LiveEvent::Level(l) => {
                    if live {
                        self.live.level = l;
                    }
                }
                LiveEvent::Partial(text) => {
                    if live {
                        self.live.partial = (!text.trim().is_empty()).then(|| self.partial_line(&text));
                    }
                }
                LiveEvent::Final { start_ms, end_ms, text } => {
                    let pid = drain_pid.or_else(|| self.live.project_id.clone());
                    if let Some(pid) = pid {
                        self.commit_line(&pid, start_ms, end_ms, &text, live);
                    }
                }
                LiveEvent::Error(e) => {
                    let repeat = self
                        .live
                        .last_error
                        .as_ref()
                        .is_some_and(|(m, at)| *m == e && at.elapsed() < Duration::from_secs(20));
                    if !repeat {
                        self.live.last_error = Some((e.clone(), Instant::now()));
                        self.toast(format!("Speech recognition: {e}."), ToastKind::Bad, None, cx);
                    }
                }
                LiveEvent::Stopped => {
                    if live {
                        // The worker gave up (a model that would not load, a
                        // vanished device); the capture ends here.
                        self.stop_live(cx);
                    }
                    self.live.draining = None;
                }
            }
        }
        if changed {
            self.autoscroll();
        }
        changed
    }

    fn glossary_terms(&self, pid: &str) -> (Vec<String>, Vec<String>) {
        let mut src = Vec::new();
        let mut dst = Vec::new();
        for g in active_glossary(&self.glossary, Some(pid)) {
            match g.rule {
                Rule::Prefer => {
                    src.push(g.term.clone());
                    if !g.target.is_empty() {
                        dst.push(g.target.clone());
                    }
                }
                Rule::Keep => {
                    src.push(g.term.clone());
                    dst.push(g.term.clone());
                }
                Rule::Forbid => {}
            }
        }
        (src, dst)
    }

    fn partial_line(&self, text: &str) -> LiveLine {
        let pid = self.live.project_id.clone().unwrap_or_default();
        let (src, _) = self.glossary_terms(&pid);
        let cjk = is_cjk(&self.live.from);
        LiveLine {
            orig: tokenize(&mark_terms(text, &src, cjk), cjk),
            orig_cjk: cjk,
        }
    }

    fn commit_line(&mut self, pid: &str, start_ms: i64, end_ms: i64, text: &str, live: bool) {
        let Some(p) = self.project(pid) else { return };
        let (from, to) = (p.from.clone(), p.to.clone());
        let translate = p.translate && self.settings.mt_ready();
        let before: Vec<String> = p.segs.iter().rev().take(3).rev().map(|s| s.orig.clone()).collect();
        let Some(p) = self.project_mut(pid) else { return };
        if let Some(prev) = p.segs.last_mut() {
            if prev.out_ms > start_ms - MIN_GAP_MS {
                prev.out_ms = (start_ms - MIN_GAP_MS).max(prev.in_ms + 300);
            }
        }
        let start_ms = p.segs.last().map(|s| s.out_ms + MIN_GAP_MS).unwrap_or(0).max(start_ms);
        p.segs.push(Segment::new(start_ms, end_ms.max(start_ms + 500), text, ""));
        let seg = p.segs.len() - 1;
        if live || self.live.draining.as_ref().is_some_and(|(d, _)| d == pid) {
            self.live.partial = None;
        }
        if !translate {
            return;
        }
        let terms: Vec<&Term> = active_glossary(&self.glossary, Some(pid)).collect();
        let rules = mt::glossary_rules(&terms);
        let cfg = self.settings.mt();
        let tx = self.bg_tx.clone();
        let (pid, line) = (pid.to_string(), text.to_string());
        spawn("live-mt", move || {
            let result = mt::translate(&cfg, &from, &to, &rules, &before, &[line])
                .map(|mut v| v.remove(0))
                .map_err(|e| friendly(&e, &cfg.url));
            let _ = tx.send(Bg::LiveLine { pid, seg, result });
        });
    }

    fn live_translated(
        &mut self,
        pid: &str,
        seg: usize,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let text = match result {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                let repeat = self
                    .live
                    .last_error
                    .as_ref()
                    .is_some_and(|(m, at)| *m == e && at.elapsed() < Duration::from_secs(20));
                if !repeat {
                    self.live.last_error = Some((e.clone(), Instant::now()));
                    self.toast(format!("Translation: {e}."), ToastKind::Bad, Some(("Engines", ToastAction::Engines)), cx);
                }
                return;
            }
        };
        let g = self.glossary.clone();
        let (trans, _) = apply_rules(&g, &text, Some(pid));
        let was_live = self.project(pid).is_some_and(|p| p.status == Status::Live);
        if let Some(p) = self.project_mut(pid) {
            if let Some(s) = p.segs.get_mut(seg) {
                s.base = text.clone();
                s.trans = trans.clone();
            }
            if !was_live {
                settle_status(&g, p);
            }
        }
    }

    // ─── playback ──────────────────────────────────────────

    /// The file a project's audio plays from: a capture's own recording, or
    /// the video it was extracted from. A capture still recording has none yet.
    pub fn audio_source(&self, p: &Project) -> Option<PathBuf> {
        let path = PathBuf::from(p.source_path.as_ref()?);
        let recording = self.live.project_id.as_deref() == Some(p.id.as_str())
            || self.live.draining.as_ref().is_some_and(|(id, _)| *id == p.id);
        let ok = match p.kind {
            Kind::Audio => !recording,
            Kind::Video => media::tool("ffmpeg").is_some(),
            Kind::Subtitle => false,
        };
        (ok && path.exists()).then_some(path)
    }

    /// Plays from `ms`, or toggles play and pause when `ms` is `None`.
    pub fn play(&mut self, ms: Option<i64>, cx: &mut Context<Self>) {
        let Some(pid) = self.project_id.clone() else { return };
        if let Some((id, pl)) = &self.player {
            if *id == pid {
                match ms {
                    Some(ms) => {
                        pl.seek(ms);
                        pl.set_playing(true);
                    }
                    None => pl.set_playing(!pl.is_playing()),
                }
                cx.notify();
                return;
            }
        }
        if let Some((id, start)) = &mut self.player_loading {
            if *id == pid {
                *start = ms.unwrap_or(*start);
                return;
            }
        }
        let Some(p) = self.current() else { return };
        let Some(path) = self.audio_source(p) else { return };
        let kind = p.kind;
        self.player = None;
        self.player_loading = Some((pid.clone(), ms.unwrap_or(0)));
        let tx = self.bg_tx.clone();
        spawn("playback", move || {
            let result = match kind {
                Kind::Audio => crate::player::read_recording(&path),
                _ => media::decode_audio(&path, None, |_| {}, || false),
            }
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Bg::Playback { pid, result });
        });
        cx.notify();
    }

    fn playback_loaded(&mut self, pid: String, result: Result<Vec<f32>, String>, cx: &mut Context<Self>) {
        let Some((id, start)) = self.player_loading.take() else { return };
        if id != pid {
            self.player_loading = Some((id, start));
            return;
        }
        match result.and_then(|pcm| crate::player::Player::new(pcm).map_err(|e| format!("{e:#}"))) {
            Ok(pl) => {
                pl.seek(start);
                pl.set_playing(true);
                self.player = Some((pid, pl));
            }
            Err(e) => self.toast(
                format!("Could not play the audio: {}.", e.trim_end_matches('.')),
                ToastKind::Bad,
                None,
                cx,
            ),
        }
        cx.notify();
    }

    pub fn pause_playback(&mut self) {
        if let Some((_, pl)) = &self.player {
            pl.set_playing(false);
        }
    }

    pub fn seek_ratio(&mut self, x: gpui::Pixels, cx: &mut Context<Self>) {
        let b = self.scrub_bounds.get();
        let Some((_, pl)) = &self.player else { return };
        if b.size.width <= gpui::px(0.) {
            return;
        }
        let ratio = ((x - b.origin.x) / b.size.width).clamp(0.0, 1.0);
        pl.seek((ratio as f64 * pl.duration_ms() as f64) as i64);
        cx.notify();
    }

    /// The line the playhead is in, if any.
    pub fn playing_line(&self) -> Option<usize> {
        let (pid, pl) = self.player.as_ref()?;
        let p = self.project(pid)?;
        let ms = pl.position_ms();
        p.segs.iter().position(|s| s.in_ms <= ms && ms < s.out_ms)
    }

    /// Keeps the playing line in view, once per line so scrolling by hand
    /// between lines is not fought.
    fn follow_playback(&mut self) -> bool {
        let Some((_, pl)) = &self.player else { return false };
        if !pl.is_playing() {
            return self.scrubbing;
        }
        let line = self.playing_line();
        if line.is_some() && line != self.play_line && self.editor.editing.is_none() {
            if let Some(ix) = self.visible_rows().iter().position(|(i, _)| Some(*i) == line) {
                self.lines_scroll.scroll_to_item(ix);
            }
        }
        self.play_line = line;
        true
    }

    // ─── engines ───────────────────────────────────────────

    /// Loads the speech model in the background, so the first capture or
    /// transcription does not wait on it.
    pub(crate) fn warm_engine(&self) {
        if self.settings.asr != AsrChoice::Qwen && self.settings.asr_ready() {
            let s = self.settings.clone();
            spawn("warm", move || {
                let _ = Engine::load(&s);
            });
        }
    }

    pub fn open_engines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let s = self.settings.clone();
        let set = |input: &gpui::Entity<gpui_component::input::InputState>, v: String, window: &mut Window, cx: &mut Context<Self>| {
            input.update(cx, |st, cx| st.set_value(v, window, cx));
        };
        set(&self.eng.qwen_url.clone(), s.qwen_url.clone(), window, cx);
        set(&self.eng.qwen_model.clone(), s.qwen_model.clone(), window, cx);
        set(&self.eng.qwen_key.clone(), s.qwen_key.clone(), window, cx);
        set(&self.eng.mt_url.clone(), s.mt_url.clone(), window, cx);
        set(&self.eng.mt_model.clone(), s.mt_model.clone(), window, cx);
        set(&self.eng.mt_key.clone(), s.mt_key.clone(), window, cx);
        self.open_select = None;
        self.dialog = Some(Dialog::Engines {
            asr: s.asr,
            whisper_model: s.whisper_model,
            qwen_local_model: s.qwen_local_model,
            asr_test: None,
            mt_test: None,
            testing_asr: false,
            testing_mt: false,
        });
        cx.notify();
    }

    /// The settings as the Engines dialog currently has them.
    fn engine_draft(&self, cx: &Context<Self>) -> Settings {
        let v = |e: &gpui::Entity<gpui_component::input::InputState>| e.read(cx).value().trim().to_string();
        let mut s = self.settings.clone();
        if let Some(Dialog::Engines { asr, whisper_model, qwen_local_model, .. }) = &self.dialog {
            s.asr = *asr;
            s.whisper_model = whisper_model.clone();
            s.qwen_local_model = qwen_local_model.clone();
        }
        s.qwen_url = v(&self.eng.qwen_url);
        s.qwen_model = v(&self.eng.qwen_model);
        s.qwen_key = v(&self.eng.qwen_key);
        s.mt_url = v(&self.eng.mt_url);
        s.mt_model = v(&self.eng.mt_model);
        s.mt_key = v(&self.eng.mt_key);
        s
    }

    pub fn set_engine_asr(&mut self, choice: AsrChoice, cx: &mut Context<Self>) {
        if let Some(Dialog::Engines { asr, asr_test, .. }) = &mut self.dialog {
            *asr = choice;
            *asr_test = None;
        }
        cx.notify();
    }

    pub fn set_whisper_model(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(Dialog::Engines { whisper_model, asr_test, .. }) = &mut self.dialog {
            *whisper_model = id.to_string();
            *asr_test = None;
        }
        cx.notify();
    }

    pub fn set_qwen_model(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(Dialog::Engines { qwen_local_model, asr_test, .. }) = &mut self.dialog {
            *qwen_local_model = id.to_string();
            *asr_test = None;
        }
        cx.notify();
    }

    pub fn save_engines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let s = self.engine_draft(cx);
        let changed_live = self.live.status != LiveStatus::Idle
            && (s.asr != self.settings.asr
                || s.whisper_model != self.settings.whisper_model
                || s.qwen_local_model != self.settings.qwen_local_model);
        self.settings = s;
        let saved = store::save_settings(&self.settings);
        self.close_dialog(window, cx);
        match saved {
            Err(e) => self.toast(format!("Could not save the engine settings: {e:#}."), ToastKind::Bad, None, cx),
            Ok(()) if changed_live => self.toast(
                "Saved. The capture running now keeps its engine; the next one uses the new one.",
                ToastKind::Ok,
                None,
                cx,
            ),
            Ok(()) => {
                self.warm_engine();
                let warn = match (self.settings.asr_ready(), self.settings.mt_ready() || !self.settings.translate) {
                    (true, true) => None,
                    (false, _) if self.settings.asr == AsrChoice::Whisper => {
                        Some("Saved. Download the chosen Whisper model before captioning or transcribing.")
                    }
                    (false, _) if self.settings.asr == AsrChoice::QwenLocal => {
                        Some("Saved. Download the chosen Qwen3-ASR model before captioning or transcribing.")
                    }
                    (false, _) => Some("Saved. Speech recognition still needs a server address and model."),
                    (true, false) => Some("Saved. Translation still needs an endpoint and model."),
                };
                match warn {
                    Some(w) => self.toast(w, ToastKind::Warn, None, cx),
                    None => self.toast(format!("Engines saved · {}.", self.settings.summary()), ToastKind::Ok, None, cx),
                }
            }
        }
    }

    pub fn test_asr(&mut self, cx: &mut Context<Self>) {
        let s = self.engine_draft(cx);
        if let Some(Dialog::Engines { testing_asr, asr_test, .. }) = &mut self.dialog {
            *testing_asr = true;
            *asr_test = None;
        }
        let tx = self.bg_tx.clone();
        spawn("asr-test", move || {
            let url = s.qwen_url.clone();
            let r = Engine::test(&s).map_err(|e| friendly(&e, &url));
            let _ = tx.send(Bg::AsrTest(r));
        });
        cx.notify();
    }

    pub fn test_mt(&mut self, cx: &mut Context<Self>) {
        let s = self.engine_draft(cx);
        if let Some(Dialog::Engines { testing_mt, mt_test, .. }) = &mut self.dialog {
            *testing_mt = true;
            *mt_test = None;
        }
        let tx = self.bg_tx.clone();
        let to = self.live.to.clone();
        spawn("mt-test", move || {
            let cfg = s.mt();
            let r = mt::test(&cfg, &to).map_err(|e| friendly(&e, &cfg.url));
            let _ = tx.send(Bg::MtTest(r));
        });
        cx.notify();
    }

    /// `id` is a Whisper model id or a Qwen3-ASR download id.
    pub fn download_model(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.downloads.contains_key(id) {
            return;
        }
        let whisper = WhisperModel::find(id);
        let qwen = QwenModel::from_download_id(id);
        let speaker = id == speaker::SPEAKER_DOWNLOAD_ID;
        if whisper.is_none() && qwen.is_none() && !speaker {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.downloads.insert(id.to_string(), Download { pct: 0.0, cancel: cancel.clone() });
        let tx = self.bg_tx.clone();
        let id = id.to_string();
        spawn("download", move || {
            let last = AtomicU32::new(0);
            let progress = |p: f32| {
                let pm = (p * 1000.0) as u32;
                if pm != last.swap(pm, Ordering::Relaxed) {
                    let _ = tx.send(Bg::Download { id: id.clone(), pct: p });
                }
            };
            let result = match (whisper, qwen) {
                (Some(m), _) => m.download(progress, &cancel),
                (_, Some(m)) => m.download(progress, &cancel),
                _ => speaker::download(progress, &cancel),
            }
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Bg::Downloaded { id, result });
        });
        cx.notify();
    }

    // ─── speakers ──────────────────────────────────────────

    /// Tells the voices in the open project apart, fetching the model first
    /// if this is the first time.
    pub fn identify_speakers(&mut self, cx: &mut Context<Self>) {
        let Some(pid) = self.project_id.clone() else { return };
        if self.speaking.is_some() || self.speaker_pending.is_some() {
            return;
        }
        if !speaker::is_downloaded() {
            self.speaker_pending = Some(pid);
            self.download_model(speaker::SPEAKER_DOWNLOAD_ID, cx);
            cx.notify();
            return;
        }
        self.run_speakers(pid, cx);
    }

    pub(crate) fn run_speakers(&mut self, pid: String, cx: &mut Context<Self>) {
        let Some(p) = self.project(&pid) else { return };
        let Some(path) = self.audio_source(p) else { return };
        let kind = p.kind;
        let spans: Vec<(i64, i64)> = p.segs.iter().map(|s| (s.in_ms, s.out_ms)).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        self.speaking = Some(SpeakerJob { pid: pid.clone(), done: 0, total: spans.len(), cancel: cancel.clone() });
        let tx = self.bg_tx.clone();
        spawn("speakers", move || {
            let result = (|| {
                let pcm = match kind {
                    Kind::Audio => crate::player::read_recording(&path),
                    _ => media::decode_audio(&path, None, |_| {}, || cancel.load(Ordering::Relaxed)),
                }?;
                let model = speaker::SpeakerModel::load()?;
                let mut last = 0;
                speaker::identify(&model, &pcm, &spans, |done| {
                    if done - last >= 8 || done == spans.len() {
                        last = done;
                        let _ = tx.send(Bg::SpeakerProgress { pid: pid.clone(), done });
                    }
                }, &cancel)
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Bg::Speakers { pid, result });
        });
        cx.notify();
    }

    pub fn cancel_speakers(&mut self, cx: &mut Context<Self>) {
        if let Some(j) = self.speaking.take() {
            j.cancel.store(true, Ordering::Relaxed);
        }
        if self.speaker_pending.take().is_some() {
            self.cancel_download(speaker::SPEAKER_DOWNLOAD_ID, cx);
        }
        cx.notify();
    }

    fn speakers_found(&mut self, pid: String, result: Result<Vec<usize>, String>, cx: &mut Context<Self>) {
        if self.speaking.as_ref().is_none_or(|j| j.pid != pid) {
            return;
        }
        self.speaking = None;
        let labels = match result {
            Ok(l) => l,
            Err(e) => {
                self.toast(format!("Could not tell the speakers apart: {e}."), ToastKind::Bad, None, cx);
                return;
            }
        };
        let Some(p) = self.project_mut(&pid) else { return };
        if labels.len() != p.segs.len() {
            return;
        }
        for (s, l) in p.segs.iter_mut().zip(&labels) {
            s.speaker = Some((*l).min(u8::MAX as usize) as u8);
        }
        let n = labels.iter().max().map_or(0, |m| m + 1);
        p.speakers.clear();
        self.save_now();
        let text = if n <= 1 {
            "Everyone sounds like one speaker.".to_string()
        } else {
            format!("Found {n} speakers. Click a name to rename it.")
        };
        self.toast(text, ToastKind::Ok, None, cx);
    }

    // ─── summary ───────────────────────────────────────────

    /// Asks which language, model and length to use, then writes the summary.
    pub fn open_summarize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.current() else { return };
        if p.transcript().trim().is_empty() {
            self.toast("There is nothing to summarize yet.", ToastKind::Warn, None, cx);
            return;
        }
        let saved_lang = self.settings.summary_lang.trim();
        let lang = if LANGS.iter().any(|(c, _)| *c == saved_lang) {
            saved_lang.to_string()
        } else if !p.summary_lang.is_empty() {
            p.summary_lang.clone()
        } else {
            p.out_lang().to_string()
        };
        let saved_model = self.settings.summary_model.trim();
        let model = if !saved_model.is_empty() {
            saved_model.to_string()
        } else if !p.summary_model.is_empty() {
            p.summary_model.clone()
        } else {
            self.settings.mt_model.trim().to_string()
        };
        let long = self.settings.summary_long || p.summary_long;
        self.sum_model.update(cx, |s, cx| s.set_value(model.clone(), window, cx));
        self.open_select = None;
        let ready = self.settings.mt_ready();
        self.dialog = Some(Dialog::Summarize {
            lang,
            model: model.clone(),
            long,
            models: vec![model],
            custom: false,
            looking: ready,
            lookup_error: None,
        });
        if ready {
            let cfg = self.settings.mt();
            let tx = self.bg_tx.clone();
            spawn("models", move || {
                let result = mt::list_models(&cfg).map_err(|e| friendly(&e, &cfg.url));
                let _ = tx.send(Bg::SummaryModels(result));
            });
        }
        cx.notify();
    }

    pub fn set_summary_length(&mut self, long: bool, cx: &mut Context<Self>) {
        if let Some(Dialog::Summarize { long: slot, .. }) = &mut self.dialog {
            *slot = long;
        }
        cx.notify();
    }

    pub fn summarize_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.require_endpoint("A summary", cx) {
            return;
        }
        let Some(Dialog::Summarize { lang, model, long, custom, .. }) = &self.dialog else {
            return;
        };
        let (lang, mut model, long, custom) = (lang.clone(), model.clone(), *long, *custom);
        if custom {
            model = self.sum_model.read(cx).value().trim().to_string();
        }
        if model.is_empty() {
            self.toast("Name the model that should write the summary.", ToastKind::Warn, None, cx);
            return;
        }
        self.settings.summary_lang = lang.clone();
        self.settings.summary_model = model.clone();
        self.settings.summary_long = long;
        if let Err(e) = store::save_settings(&self.settings) {
            eprintln!("saving settings: {e:#}");
        }
        self.close_dialog(window, cx);
        self.start_summary(lang, model, long, cx);
    }

    fn start_summary(&mut self, lang: String, model: String, long: bool, cx: &mut Context<Self>) {
        let Some(p) = self.current() else { return };
        let transcript = p.transcript();
        if transcript.trim().is_empty() {
            self.toast("There is nothing to summarize yet.", ToastKind::Warn, None, cx);
            return;
        }
        let pid = p.id.clone();
        let lang_label = lang_name(&lang);
        let terms: Vec<&Term> = active_glossary(&self.glossary, Some(&pid)).collect();
        let rules = mt::glossary_rules(&terms);
        let mut cfg = self.settings.mt();
        cfg.model = model.clone();

        if let Some(old) = self.summary_job.take() {
            old.cancel.store(true, Ordering::Relaxed);
        }
        self.next_id += 1;
        let job = self.next_id;
        let cancel = Arc::new(AtomicBool::new(false));
        self.summary_job = Some(SummaryJob {
            id: job,
            pid: pid.clone(),
            lang: lang.clone(),
            model: model.clone(),
            long,
            done: 0,
            parts: 0,
            cancel: cancel.clone(),
        });
        let tx = self.bg_tx.clone();
        spawn("summary", move || {
            let progress = |done, parts| {
                let _ = tx.send(Bg::SummaryProgress { job, done, parts });
            };
            let result = mt::summarize(&cfg, &lang_label, long, &rules, &transcript, progress, &cancel);
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let result = result.map_err(|e| friendly(&e, &cfg.url));
            let _ = tx.send(Bg::Summary { pid, job, result });
        });
        cx.notify();
    }

    /// Ends the summary being written. Whatever summary the project already
    /// had stays as it was.
    pub fn stop_summary(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = self.summary_job.take() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    fn summary_models(&mut self, result: Result<Vec<String>, String>, cx: &mut Context<Self>) {
        let Some(Dialog::Summarize { model, models, looking, lookup_error, .. }) = &mut self.dialog else {
            return;
        };
        *looking = false;
        match result {
            Ok(mut list) => {
                if !model.is_empty() && !list.iter().any(|m| m == model) {
                    list.insert(0, model.clone());
                }
                *models = list;
                *lookup_error = None;
            }
            Err(e) => *lookup_error = Some(e),
        }
        cx.notify();
    }

    fn summary_arrived(&mut self, pid: String, job: u64, result: Result<String, String>, cx: &mut Context<Self>) {
        if self.summary_job.as_ref().is_none_or(|j| j.id != job) {
            return;
        }
        let Some(job) = self.summary_job.take() else { return };
        match result {
            Ok(text) => {
                let Some(p) = self.project_mut(&pid) else { return };
                p.summary = text;
                p.summary_lang = job.lang;
                p.summary_model = job.model;
                p.summary_long = job.long;
                let name = p.name.clone();
                self.save_now();
                self.toast(format!("Summary written for “{name}”."), ToastKind::Ok, None, cx);
            }
            Err(e) => {
                self.toast(
                    format!("Could not write a summary: {e}."),
                    ToastKind::Bad,
                    Some(("Engines", ToastAction::Engines)),
                    cx,
                );
            }
        }
    }

    // ─── adding a translation later ────────────────────────

    pub fn translate_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.require_mt(cx) {
            return;
        }
        let Some(pid) = self.project_id.clone() else { return };
        self.close_dialog(window, cx);
        if let Some(p) = self.project_mut(&pid) {
            p.translate = true;
            p.status = Status::Working;
        }
        self.translate_project(&pid, None, cx);
        cx.notify();
    }

    pub fn cancel_download(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(d) = self.downloads.remove(id) {
            d.cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }
}

#[allow(clippy::too_many_arguments)]
fn transcribe_file(
    path: &Path,
    stream: usize,
    settings: &Settings,
    lang: &str,
    duration: i64,
    note: &dyn Fn(usize, StageState, String),
    tx: &Sender<Bg>,
    job: u64,
    cancel: &Arc<AtomicBool>,
) -> anyhow::Result<Vec<Segment>> {
    note(1, StageState::Run, "loading model".into());
    let engine = Engine::load(settings)?;
    let mut last = Instant::now();
    let pcm = media::decode_audio(
        path,
        Some(stream),
        |ms| {
            if last.elapsed() > Duration::from_millis(400) {
                last = Instant::now();
                let pct = if duration > 0 { format!("reading {}%", ms * 100 / duration) } else { format!("reading {}", mmss(ms)) };
                note(1, StageState::Run, pct);
            }
        },
        || cancel.load(Ordering::Relaxed),
    )?;
    let audio_ms = (pcm.len() * 1000 / media::SAMPLE_RATE) as i64;
    note(1, StageState::Run, "listening 0%".into());
    let sent = Arc::new(AtomicU32::new(0));
    let progress: crate::engine::asr::Progress = {
        let tx = tx.clone();
        let sent = sent.clone();
        Arc::new(move |p: f32| {
            let pct = (p * 100.0).clamp(0.0, 100.0) as u32;
            if pct != sent.swap(pct, Ordering::Relaxed) {
                let _ = tx.send(Bg::Stage { job, stage: 1, state: StageState::Run, note: format!("listening {pct}%") });
            }
        })
    };
    let flag = cancel.clone();
    let segs = engine.transcribe_long(&pcm, lang, progress, &move || flag.load(Ordering::Relaxed))?;
    note(1, StageState::Done, format!("{} of audio", mmss(audio_ms)));
    Ok(segs
        .into_iter()
        .map(|s: AsrSeg| {
            let end = s.end_ms.min(audio_ms).max(s.start_ms + 300);
            Segment::new(s.start_ms, end, s.text.trim(), "")
        })
        .collect())
}

/// Cleans a track for subtitling: empty cues go, order is restored, and
/// overlapping cues are pulled apart by the minimum gap.
fn retime(segs: &mut Vec<Segment>) {
    segs.retain(|s| !s.orig.trim().is_empty());
    segs.sort_by_key(|s| s.in_ms);
    for i in 0..segs.len() {
        if segs[i].out_ms <= segs[i].in_ms {
            segs[i].out_ms = segs[i].in_ms + 1200;
        }
        if let Some(next_in) = segs.get(i + 1).map(|n| n.in_ms) {
            if segs[i].out_ms > next_in - MIN_GAP_MS {
                segs[i].out_ms = (next_in - MIN_GAP_MS).max(segs[i].in_ms + 200);
            }
        }
    }
}

/// Subtitle files come as UTF-8 (with or without a BOM) or UTF-16.
fn decode_text(b: &[u8]) -> String {
    let utf16 = |le: bool| {
        let units: Vec<u16> = b[2..]
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match b {
        [0xFF, 0xFE, ..] => utf16(true),
        [0xFE, 0xFF, ..] => utf16(false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(b).into_owned(),
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Wraps glossary terms in `[[ ]]` for the tokenizer. In spaced scripts the
/// whole word around a match is wrapped, so punctuation stays attached.
pub fn mark_terms(text: &str, terms: &[String], cjk: bool) -> String {
    let lower = text.to_lowercase();
    let same_len = lower.len() == text.len();
    let hay = if same_len { lower.as_str() } else { text };
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut terms: Vec<&String> = terms.iter().filter(|t| !t.trim().is_empty()).collect();
    terms.sort_by_key(|t| std::cmp::Reverse(t.len()));
    for t in terms {
        let needle = if same_len { t.to_lowercase() } else { t.to_string() };
        for (i, _) in hay.match_indices(&needle) {
            let (mut a, mut b) = (i, i + needle.len());
            if !cjk {
                while a > 0 && !text[..a].ends_with(char::is_whitespace) {
                    a = text[..a].char_indices().last().map(|(k, _)| k).unwrap_or(0);
                }
                while b < text.len() && !text[b..].starts_with(char::is_whitespace) {
                    b += text[b..].chars().next().map(char::len_utf8).unwrap_or(1);
                }
            }
            if ranges.iter().all(|&(x, y)| b <= x || a >= y) {
                ranges.push((a, b));
            }
        }
    }
    ranges.sort();
    let mut out = String::with_capacity(text.len() + ranges.len() * 4);
    let mut last = 0;
    for (a, b) in ranges {
        out.push_str(&text[last..a]);
        out.push_str("[[");
        out.push_str(&text[a..b]);
        out.push_str("]]");
        last = b;
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_whole_words_and_cjk_runs() {
        let t = vec!["Taurus".to_string(), "decoder".to_string()];
        assert_eq!(mark_terms("Ship Taurus, then the decoder.", &t, false), "Ship [[Taurus,]] then the [[decoder.]]");
        let z = vec!["解码器".to_string()];
        assert_eq!(mark_terms("新的解码器很快", &z, true), "新的[[解码器]]很快");
    }

    #[test]
    fn retime_pulls_overlaps_apart() {
        let mut s = vec![Segment::new(1000, 3000, "b", ""), Segment::new(0, 1500, "a", ""), Segment::new(4000, 4000, " ", "")];
        retime(&mut s);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].out_ms, 1000 - MIN_GAP_MS);
    }

    #[test]
    fn reads_utf16_subtitles() {
        let mut b = vec![0xFF, 0xFE];
        for u in "1\n".encode_utf16() {
            b.extend(u.to_le_bytes());
        }
        assert_eq!(decode_text(&b), "1\n");
    }
}
