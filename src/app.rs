//! The workbench: all state and every action. Rendering lives in `shell.rs`
//! and the per-screen modules under `views/`; background work (translation,
//! extraction, live captioning, model downloads, saving) lives in `work.rs`.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext as _, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    PathPromptOptions, Pixels, RenderImage, ScrollHandle, Subscription, Task, Window,
};
use gpui_component::input::{InputEvent, InputState};

use crate::capture;
use crate::data::*;
use crate::engine::mt::Think;
use crate::engine::{AsrChoice, CaptureProfile, Settings};
use crate::live::LiveSession;
use crate::media::Probe;
use crate::store;
use crate::update::Release;
use crate::work::{Bg, SpeakerJob};

/// Debug builds honour `TAURUS_VIEW=editor|glossary` for screenshots.
fn initial_view() -> View {
    if !cfg!(debug_assertions) {
        return View::Projects;
    }
    match std::env::var("TAURUS_VIEW").as_deref() {
        Ok("editor") => View::Editor,
        Ok("glossary") => View::Glossary,
        _ => View::Projects,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Projects,
    /// One project's lines, including a capture as it records.
    Editor,
    Glossary,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LiveStatus {
    Idle,
    Running,
    Paused,
}

pub struct LiveState {
    pub status: LiveStatus,
    pub source: Source,
    /// Which app's sound to hear, or all of them.
    pub detail: String,
    pub devices: Vec<String>,
    pub mic: String,
    pub mics: Vec<String>,
    /// What to record on screen: `capture::NO_VIDEO`, the whole screen or an app.
    pub video: String,
    pub videos: Vec<String>,
    pub video_quality: capture::VideoQuality,
    pub can_video: bool,
    pub from: String,
    pub to: String,
    pub(crate) offset_ms: i64,
    pub(crate) t0: Option<Instant>,
    pub project_id: Option<String>,
    /// What the recogniser hears right now, before the line settles.
    pub partial: Option<LiveLine>,
    pub level: f32,
    pub starting: bool,
    /// Shown while the recogniser loads, e.g. "Loading Whisper base…".
    pub engine_note: Option<String>,
    pub(crate) session: Option<LiveSession>,
    /// A stopped capture still committing its last words, with its project.
    pub(crate) draining: Option<(String, LiveSession)>,
    pub(crate) last_error: Option<(String, Instant)>,
}

impl LiveState {
    pub fn live_ms(&self) -> i64 {
        match (self.status, self.t0) {
            (LiveStatus::Running, Some(t0)) => self.offset_ms + t0.elapsed().as_millis() as i64,
            _ => self.offset_ms,
        }
    }

    pub(crate) fn reset_session(&mut self) {
        self.offset_ms = 0;
        self.partial = None;
        self.level = 0.0;
    }
}

pub struct EditorState {
    pub active: Option<usize>,
    pub review_only: bool,
    pub loading: BTreeSet<usize>,
    pub editing: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackKey {
    Sub(usize),
    Audio(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StageState {
    Wait,
    Run,
    Done,
    Failed,
}

pub struct Stage {
    pub label: String,
    pub note: String,
    pub state: StageState,
}

pub struct PickedFile {
    pub name: String,
    pub size: String,
    pub path: PathBuf,
}

pub struct TrackRow {
    pub key: Option<TrackKey>,
    pub title: String,
    pub sub: String,
}

pub struct ExtractState {
    pub file: Option<PickedFile>,
    pub probe: Option<Probe>,
    pub scan_error: Option<String>,
    pub track: Option<TrackKey>,
    pub from: String,
    pub to: String,
    pub project_id: Option<String>,
    pub stages: Vec<Stage>,
    pub transcribing: bool,
    pub failed: Option<String>,
    pub(crate) job: u64,
    pub(crate) cancel: Arc<AtomicBool>,
}

impl ExtractState {
    pub fn scanned(&self) -> bool {
        self.probe.is_some() || self.scan_error.is_some()
    }

    pub fn tracks(&self) -> Vec<TrackRow> {
        let Some(p) = self.probe.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for s in &p.subs {
            let lang = s
                .lang
                .as_deref()
                .and_then(crate::subs::lang_from_tag)
                .map(lang_name)
                .or_else(|| s.lang.clone())
                .unwrap_or_else(|| "language not marked".into());
            let mut title = format!("Embedded subtitle track · {lang}");
            if let Some(t) = s.title.as_deref().filter(|t| !t.is_empty()) {
                title.push_str(&format!(" · {t}"));
            }
            let sub = if !s.text {
                format!("{} · image subtitles, cannot be read as text", s.codec)
            } else {
                let cues = s
                    .cues
                    .map(|n| format!("{n} cues"))
                    .unwrap_or_else(|| "unreadable".into());
                let span = s
                    .span_ms
                    .map(|ms| format!(" · {}", &srt_time(ms)[..8]))
                    .unwrap_or_default();
                format!("{} · {cues}{span}", s.codec)
            };
            let usable = s.text && s.cues.is_some_and(|n| n > 0);
            out.push(TrackRow {
                key: usable.then_some(TrackKey::Sub(s.stream)),
                title,
                sub,
            });
        }
        for (i, a) in p.audio.iter().enumerate() {
            let rate = a
                .sample_rate
                .map(|r| format!(" · {} kHz", r / 1000))
                .unwrap_or_default();
            let ch = match a.channels {
                Some(1) => " mono",
                Some(2) => " stereo",
                Some(_) => " surround",
                None => "",
            };
            let title = if p.audio.len() > 1 {
                let lang = a
                    .lang
                    .as_deref()
                    .and_then(crate::subs::lang_from_tag)
                    .map(lang_name);
                format!(
                    "Transcribe audio track {}{}",
                    i + 1,
                    lang.map(|l| format!(" · {l}")).unwrap_or_default()
                )
            } else {
                "Transcribe the audio track".into()
            };
            out.push(TrackRow {
                key: Some(TrackKey::Audio(a.stream)),
                title,
                sub: format!("{}{rate}{ch} · slower, works on anything", a.codec),
            });
        }
        out
    }

    pub fn file_meta(&self) -> String {
        let size = self
            .file
            .as_ref()
            .map(|f| f.size.clone())
            .unwrap_or_default();
        match self.probe.as_ref() {
            Some(p) => {
                let mut parts = vec![size];
                if let Some(v) = &p.video {
                    parts.push(v.clone());
                }
                if p.duration_ms > 0 {
                    parts.push(srt_time(p.duration_ms)[..8].to_string());
                }
                parts.join(" · ")
            }
            None => size,
        }
    }
}

pub struct ExportState {
    pub format: Format,
    pub layout: Layout,
    pub wrap: usize,
    pub merge: bool,
    pub skip_flagged: bool,
    pub bom: bool,
    pub speakers: bool,
    pub wrap_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub dragging_wrap: bool,
}

impl ExportState {
    pub fn options(&self) -> ExportOptions {
        ExportOptions {
            format: self.format,
            layout: self.layout,
            wrap: self.wrap,
            merge: self.merge,
            skip_flagged: self.skip_flagged,
            bom: self.bom,
            speakers: self.speakers,
        }
    }
}

/// Dropdowns. Each one names the piece of state it edits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sel {
    SourceDetail,
    MicDetail,
    VideoDetail,
    VideoQuality,
    Profile,
    LiveFrom,
    LiveTo,
    ExFrom,
    ExTo,
    /// The language an untranslated project is about to be translated into.
    TrTo,
    /// The language a summary is written in.
    SumLang,
    /// The chat model that writes a summary.
    SumModel,
    TermRule,
    TermScope,
}

#[derive(Clone, Debug)]
pub enum ConfirmAction {
    DeleteProject(String),
    DeleteTerm(String),
}

pub enum Dialog {
    Confirm {
        title: String,
        text: String,
        ok_label: String,
        action: ConfirmAction,
    },
    Term {
        editing: Option<String>,
        rule: Rule,
        scope: Scope,
        term_err: Option<String>,
        target_err: Option<String>,
    },
    Rename {
        target: String,
        /// Names one of the project's speakers instead of the project.
        speaker: Option<u8>,
        note: String,
        err: Option<String>,
    },
    /// Adds a translation to a project made without one.
    Translate,
    /// Language, model, length and reasoning before a summary is written.
    Summarize {
        lang: String,
        model: String,
        long: bool,
        think: Think,
        models: Vec<String>,
        custom: bool,
        looking: bool,
        lookup_error: Option<String>,
    },
    /// Where to listen and in which languages, before a capture starts.
    Capture,
    /// Which track of the picked video to use.
    Extract,
    Export,
    Engines {
        asr: AsrChoice,
        whisper_model: String,
        qwen_local_model: String,
        asr_test: Option<Result<String, String>>,
        mt_test: Option<Result<String, String>>,
        testing_asr: bool,
        testing_mt: bool,
    },
    /// The version, and getting a newer one.
    About,
    /// What the recording is about, and how much to reason, before its lines are corrected.
    Correct {
        think: Think,
    },
}

/// How far finding and installing a new version has got.
pub enum UpdateState {
    /// Not looked for in this session.
    Idle,
    Checking,
    UpToDate,
    /// A newer release that is not being installed. `can_install` is false
    /// when Taurus cannot replace itself where it runs.
    Available {
        release: Release,
        can_install: bool,
    },
    Installing {
        release: Release,
        pct: f32,
        cancel: Arc<AtomicBool>,
    },
    /// Installed; Taurus runs it from the next launch.
    Ready(String),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToastKind {
    Ok,
    Warn,
    Bad,
}

#[derive(Clone, Debug)]
pub enum ToastAction {
    Rename(String),
    Open(String),
    Delete(String),
    Engines,
    Restart,
}

pub struct Toast {
    pub id: u64,
    pub msg: String,
    pub kind: ToastKind,
    pub action: Option<(String, ToastAction)>,
}

/// One summary being written for a project.
pub struct SummaryJob {
    pub id: u64,
    pub pid: String,
    pub lang: String,
    pub model: String,
    pub long: bool,
    /// Parts of a long transcript summarized so far, out of `parts`, which
    /// stays 0 for a transcript short enough to go in one piece.
    pub done: usize,
    pub parts: usize,
    pub(crate) cancel: Arc<AtomicBool>,
}

/// One project's lines being read back for misheard words.
pub struct CorrectJob {
    pub id: u64,
    pub pid: String,
    pub done: usize,
    pub total: usize,
    /// The lines in flight, as 1-based indexes into this run, so the strip
    /// can show which batch is being read while `done` waits for it.
    pub span: Option<(usize, usize)>,
    pub(crate) cancel: Arc<AtomicBool>,
}

/// A video being written with this project's cues burned into the picture.
pub struct VideoExport {
    pub pct: f32,
    pub pid: String,
    pub cues: usize,
    pub layout: Layout,
    /// Stop was pressed; the encode is being killed.
    pub stopping: bool,
    pub(crate) cancel: Arc<AtomicBool>,
}

/// A translation run over some or all of one project's segments.
pub struct MtJob {
    pub id: u64,
    pub done: usize,
    pub total: usize,
    /// Set for a retranslate from the editor, which reports what changed.
    pub(crate) retranslate: bool,
    pub(crate) filled: usize,
    pub(crate) changed: usize,
    pub(crate) cancel: Arc<AtomicBool>,
}

pub struct Download {
    pub pct: f32,
    pub(crate) cancel: Arc<AtomicBool>,
}

pub struct EngineInputs {
    pub qwen_url: Entity<InputState>,
    pub qwen_model: Entity<InputState>,
    pub qwen_key: Entity<InputState>,
    pub mt_url: Entity<InputState>,
    pub mt_model: Entity<InputState>,
    pub mt_key: Entity<InputState>,
}

pub struct Workbench {
    pub focus: FocusHandle,
    pub view: View,
    pub projects: Vec<Project>,
    pub glossary: Vec<Term>,
    pub project_id: Option<String>,
    pub settings: Settings,

    pub live: LiveState,
    pub editor: EditorState,
    pub extract: ExtractState,
    pub exp: ExportState,

    pub new_menu: bool,
    /// The open project's audio, once it has been played.
    pub player: Option<(String, crate::player::Player)>,
    /// The project whose audio is being decoded, and where to start it.
    pub player_loading: Option<(String, i64)>,
    /// The open project's picture, when it has a video on disk.
    pub(crate) video: Option<crate::video::VideoDeck>,
    pub(crate) video_seen: u64,
    /// The frame currently on screen. The previous one is freed from the atlas.
    pub(crate) video_frame: Option<Arc<RenderImage>>,
    /// The picture fills the window under the top bar.
    pub(crate) video_app: bool,
    /// The video control put the window into the fullscreen space.
    pub(crate) video_screen: bool,
    pub(crate) retired_video: Vec<Arc<RenderImage>>,
    pub scrub_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub scrubbing: bool,
    /// The line under the playhead, so the list follows it once per line.
    pub play_line: Option<usize>,
    /// Voices being told apart for one project.
    pub speaking: Option<SpeakerJob>,
    /// A project waiting for the speaker model to finish downloading.
    pub speaker_pending: Option<String>,
    pub open_select: Option<Sel>,
    select_closed: Option<(Sel, Instant)>,
    pub dialog: Option<Dialog>,
    pub toasts: Vec<Toast>,
    pub(crate) next_id: u64,

    pub jobs: HashMap<String, MtJob>,
    /// The summary currently being written, if any.
    pub summary_job: Option<SummaryJob>,
    pub correcting: Option<CorrectJob>,
    /// A burned-in video export in flight, if any.
    pub video_export: Option<VideoExport>,
    pub downloads: HashMap<String, Download>,
    pub update: UpdateState,
    /// The app bundle Taurus runs from, which an update replaces. None for a
    /// development build.
    pub app_bundle: Option<PathBuf>,
    pub(crate) bg_tx: Sender<Bg>,
    pub(crate) bg_rx: Receiver<Bg>,
    pub(crate) saved_json: String,
    pub(crate) last_save: Instant,
    pub(crate) save_failed: bool,
    _pump: Option<Task<()>>,

    pub pj_search: Entity<InputState>,
    pub gl_search: Entity<InputState>,
    pub term_input: Entity<InputState>,
    pub target_input: Entity<InputState>,
    pub rename_input: Entity<InputState>,
    /// The name typed for a new capture profile.
    pub profile_input: Entity<InputState>,
    /// The capture dialog is asking for a profile name; holds its error, if any.
    pub naming_profile: Option<Option<String>>,
    pub seg_input: Entity<InputState>,
    /// A model name typed in the summary dialog, when none of the listed ones is wanted.
    pub sum_model: Entity<InputState>,
    /// What the recording is about, typed before correcting it.
    pub correct_about: Entity<InputState>,
    pub eng: EngineInputs,
    pub lines_scroll: ScrollHandle,
    pub summary_scroll: ScrollHandle,
    pub projects_scroll: ScrollHandle,
    pub glossary_scroll: ScrollHandle,
    pub dialog_scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

unsafe extern "C" {
    fn _exit(code: i32) -> !;
}

impl Workbench {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let single =
            |window: &mut Window, cx: &mut Context<Self>| cx.new(|cx| InputState::new(window, cx));
        let pj_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search projects"));
        let gl_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search terms"));
        let term_input = single(window, cx);
        let target_input = single(window, cx);
        let rename_input = single(window, cx);
        let profile_input = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Team call"));
        let sum_model = cx.new(|cx| InputState::new(window, cx).placeholder("qwen3:8b"));
        let correct_about = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. A product meeting with Ann Lee and Bo Chan")
        });
        let seg_input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(1, 10)
                .placeholder("Not translated yet")
        });
        let text = |window: &mut Window, cx: &mut Context<Self>, ph: &str| {
            let ph = ph.to_string();
            cx.new(|cx| InputState::new(window, cx).placeholder(ph))
        };
        let secret = |window: &mut Window, cx: &mut Context<Self>, ph: &str| {
            let ph = ph.to_string();
            cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(ph))
        };
        let eng = EngineInputs {
            qwen_url: text(window, cx, "http://localhost:8000/v1"),
            qwen_model: text(window, cx, "Qwen/Qwen3-ASR-1.7B"),
            qwen_key: secret(window, cx, "Not needed for a local server"),
            mt_url: text(window, cx, "http://localhost:11434/v1"),
            mt_model: text(window, cx, "qwen3:8b"),
            mt_key: secret(window, cx, "Not needed for Ollama or LM Studio"),
        };

        let mut subs = Vec::new();
        subs.push(
            cx.subscribe_in(&pj_search, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        );
        subs.push(
            cx.subscribe_in(&gl_search, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        );
        for (input, is_target) in [(&term_input, false), (&target_input, true)] {
            subs.push(cx.subscribe_in(
                input,
                window,
                move |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { .. } => this.save_term(window, cx),
                    InputEvent::Blur => this.validate_term_field(is_target, cx),
                    _ => {}
                },
            ));
        }
        subs.push(cx.subscribe_in(
            &rename_input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.rename_project(window, cx),
                InputEvent::Blur => {
                    let empty = this.rename_input.read(cx).value().trim().is_empty();
                    if let Some(Dialog::Rename {
                        err, speaker: None, ..
                    }) = &mut this.dialog
                    {
                        *err = empty
                            .then(|| "Name the project so you can find this track again.".into());
                    }
                    cx.notify();
                }
                _ => {}
            },
        ));
        subs.push(cx.subscribe_in(
            &profile_input,
            window,
            |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.save_profile(window, cx);
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &sum_model,
            window,
            |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.summarize_now(window, cx);
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &correct_about,
            window,
            |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.correct_now(window, cx);
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &seg_input,
            window,
            |this, input, ev: &InputEvent, _, cx| match ev {
                InputEvent::Change => {
                    let value = input.read(cx).value().to_string();
                    if let Some(i) = this.editor.editing {
                        if let Some(p) = this.current_mut() {
                            let translated = p.translate;
                            if let Some(s) = p.segs.get_mut(i) {
                                let text = if translated {
                                    &mut s.trans
                                } else {
                                    &mut s.orig
                                };
                                if *text != value {
                                    *text = value;
                                }
                            }
                        }
                    }
                    cx.notify();
                }
                InputEvent::Blur => {
                    this.editor.editing = None;
                    cx.notify();
                }
                _ => {}
            },
        ));
        subs.push(cx.on_app_quit(|this, _| {
            this.end_session();
            this.live.draining = None;
            this.save_now();
            if crate::engine::asr::unload() {
                // A transcription is still running on another thread; its
                // model cannot be freed in time, so skip the C++ teardown.
                unsafe { _exit(0) }
            }
            async {}
        }));

        let lib = store::load_library();
        let settings = store::load_settings();
        let glossary = lib.glossary;
        let mut projects = lib.projects;
        let mut resume = Vec::new();
        for p in projects.iter_mut() {
            // A capture cannot outlive the app; whatever it recorded is kept.
            if p.status == Status::Live {
                p.status = Status::Review;
            }
            // An extraction cannot outlive the app either, and left nothing behind.
            if p.status == Status::Working && p.segs.is_empty() {
                p.status = Status::Failed;
                p.error = Some("Taurus closed before the extraction finished".into());
            }
            if p.status == Status::Working && p.translate {
                resume.push(p.id.clone());
            }
            settle_status(&glossary, p);
        }
        let saved_json = store::library_json(&store::LibraryRef {
            projects: &projects,
            glossary: &glossary,
        });
        let (bg_tx, bg_rx) = channel();
        let project_id = projects
            .iter()
            .find(|p| p.status != Status::Failed)
            .map(|p| p.id.clone());

        let mut this = Self {
            focus: cx.focus_handle(),
            view: initial_view(),
            projects,
            glossary,
            project_id,
            settings,
            live: LiveState {
                status: LiveStatus::Idle,
                source: Source::Both,
                detail: capture::ALL_SYSTEM.into(),
                devices: vec![capture::ALL_SYSTEM.into()],
                mic: capture::DEFAULT_MIC.into(),
                mics: vec![capture::DEFAULT_MIC.into()],
                video: capture::NO_VIDEO.into(),
                videos: vec![capture::NO_VIDEO.into(), capture::WHOLE_SCREEN.into()],
                video_quality: capture::VideoQuality::Standard,
                can_video: capture::can_record_screen(),
                from: "en".into(),
                to: "zh".into(),
                offset_ms: 0,
                t0: None,
                project_id: None,
                partial: None,
                level: 0.0,
                starting: false,
                engine_note: None,
                session: None,
                draining: None,
                last_error: None,
            },
            editor: EditorState {
                active: None,
                review_only: false,
                loading: BTreeSet::new(),
                editing: None,
            },
            extract: ExtractState {
                file: None,
                probe: None,
                scan_error: None,
                track: None,
                from: "en".into(),
                to: "zh".into(),
                project_id: None,
                stages: Vec::new(),
                transcribing: false,
                failed: None,
                job: 0,
                cancel: Arc::new(AtomicBool::new(false)),
            },
            exp: ExportState {
                format: Format::Srt,
                layout: Layout::Stacked,
                wrap: 42,
                merge: false,
                skip_flagged: false,
                bom: true,
                speakers: true,
                wrap_bounds: Rc::new(Cell::new(Bounds::default())),
                dragging_wrap: false,
            },
            new_menu: false,
            player: None,
            player_loading: None,
            video: None,
            video_seen: 0,
            video_frame: None,
            video_app: false,
            video_screen: false,
            retired_video: Vec::new(),
            scrub_bounds: Rc::new(Cell::new(Bounds::default())),
            scrubbing: false,
            play_line: None,
            speaking: None,
            speaker_pending: None,
            open_select: None,
            select_closed: None,
            dialog: None,
            toasts: Vec::new(),
            next_id: 1,
            jobs: HashMap::new(),
            summary_job: None,
            correcting: None,
            video_export: None,
            downloads: HashMap::new(),
            update: UpdateState::Idle,
            app_bundle: cx.app_path().ok().and_then(|p| crate::update::bundle(&p)),
            bg_tx,
            bg_rx,
            saved_json,
            last_save: Instant::now(),
            save_failed: false,
            _pump: None,
            pj_search,
            gl_search,
            term_input,
            target_input,
            rename_input,
            profile_input,
            naming_profile: None,
            seg_input,
            sum_model,
            correct_about,
            eng,
            lines_scroll: ScrollHandle::new(),
            summary_scroll: ScrollHandle::new(),
            projects_scroll: ScrollHandle::new(),
            glossary_scroll: ScrollHandle::new(),
            dialog_scroll: ScrollHandle::new(),
            _subs: subs,
        };
        if let Some(p) = this
            .settings
            .last_capture
            .clone()
            .or_else(|| this.profile(&this.settings.profile).cloned())
        {
            this.apply_profile(&p);
        }
        this._pump = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if this.update(cx, |this, cx| this.pump(cx)).is_err() {
                    break;
                }
            }
        }));
        for pid in resume {
            if this.settings.mt_ready() {
                this.translate_project(&pid, None, cx);
            }
        }
        this.debug_start(window, cx);
        this
    }

    /// Debug builds can open straight into a flow for screenshots:
    /// `TAURUS_IMPORT=a.srt,b.vtt`, `TAURUS_EXTRACT=video.mp4`,
    /// `TAURUS_DIALOG=engines|capture|export|translate|about|correct`, `TAURUS_MENU=1`, `TAURUS_PLAY=<ms>`,
    /// `TAURUS_SPEAKERS=1`, `TAURUS_TRANSLATE=1`, `TAURUS_VIDEO=Entire screen|<app>`.
    fn debug_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !cfg!(debug_assertions) {
            return;
        }
        if let Ok(list) = std::env::var("TAURUS_IMPORT") {
            self.import_paths(list.split(',').map(PathBuf::from).collect(), cx);
        }
        if let Ok(video) = std::env::var("TAURUS_EXTRACT") {
            self.pick_path(PathBuf::from(video), cx);
        }
        match std::env::var("TAURUS_DIALOG").as_deref() {
            Ok("engines") => self.open_engines(window, cx),
            Ok("capture") => self.open_capture(cx),
            Ok("export") => self.open_export(cx),
            Ok("translate") => self.open_translate(cx),
            Ok("about") => self.open_about(cx),
            _ => {}
        }
        if std::env::var("TAURUS_MENU").is_ok() {
            self.new_menu = true;
        }
        if self.view == View::Editor {
            self.ensure_open_project();
            if std::env::var("TAURUS_DIALOG").as_deref() == Ok("correct") {
                self.open_correct(window, cx);
            }
            if std::env::var("TAURUS_TRANSLATE").is_ok() {
                self.translate_now(window, cx);
            }
            if std::env::var("TAURUS_SPEAKERS").is_ok() {
                self.identify_speakers(cx);
            }
            if let Some(ms) = std::env::var("TAURUS_PLAY")
                .ok()
                .and_then(|v| v.parse().ok())
            {
                self.play(Some(ms), cx);
            }
        }
        if let Ok(target) = std::env::var("TAURUS_VIDEO") {
            self.live.video = target;
        }
        if let Ok(src) = std::env::var("TAURUS_LIVE") {
            match src.as_str() {
                "mic" => self.set_source(Source::Mic, cx),
                "system" => self.set_source(Source::System, cx),
                _ => {}
            }
            self.start_live(window, cx);
        }
    }

    // ─── lookups ───────────────────────────────────────────

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub(crate) fn project_mut(&mut self, id: &str) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.id == id)
    }

    pub fn current(&self) -> Option<&Project> {
        self.project_id.as_deref().and_then(|id| self.project(id))
    }

    pub fn current_mut(&mut self) -> Option<&mut Project> {
        let id = self.project_id.clone()?;
        self.project_mut(&id)
    }

    /// The project being recorded into right now, which is not necessarily
    /// the one open in the editor.
    pub fn live_project(&self) -> Option<&Project> {
        self.live
            .project_id
            .as_deref()
            .and_then(|id| self.project(id))
    }

    pub fn track_len(&self, p: &Project) -> String {
        if p.status == Status::Live {
            mmss(self.live.live_ms())
        } else {
            p.segs
                .last()
                .map(|s| mmss(s.out_ms))
                .unwrap_or_else(|| "–".into())
        }
    }

    pub fn scope_label(&self, scope: &Scope) -> String {
        match scope {
            Scope::All => "Every project".into(),
            Scope::Project(id) => self
                .project(id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Project removed".into()),
        }
    }

    pub(crate) fn settle_all(&mut self) {
        let g = &self.glossary;
        let jobs = &self.jobs;
        for p in self.projects.iter_mut() {
            if p.status == Status::Working && jobs.contains_key(&p.id) {
                continue;
            }
            settle_status(g, p);
        }
    }

    pub(crate) fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!(
            "{prefix}-{}-{}",
            chrono::Local::now().timestamp_millis(),
            self.next_id
        )
    }

    // ─── routing ───────────────────────────────────────────

    pub fn show_view(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        self.view = view;
        self.open_select = None;
        self.new_menu = false;
        self.pause_playback();
        self.stop_editing(window, cx);
        self.settle_all();
        if view == View::Editor {
            self.ensure_open_project();
        }
        window.focus(&self.focus);
        cx.notify();
    }

    /// The project switchers skip failed tracks, so the open project falls
    /// back to the first one that has segments to show.
    fn ensure_open_project(&mut self) {
        let ok = self.current().is_some_and(|p| p.status != Status::Failed);
        if !ok {
            self.project_id = self
                .projects
                .iter()
                .find(|p| p.status != Status::Failed)
                .map(|p| p.id.clone());
        }
    }

    pub fn open_project(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.project(id) else { return };
        if p.status == Status::Failed {
            let msg = format!(
                "“{}” failed before any segment was written: {}.",
                p.name,
                p.error.clone().unwrap_or_default().trim_end_matches('.')
            );
            self.toast(msg, ToastKind::Bad, None, cx);
            return;
        }
        self.project_id = Some(id.to_string());
        self.reset_editor_view();
        self.show_view(View::Editor, window, cx);
    }

    pub fn open_export(&mut self, cx: &mut Context<Self>) {
        if self.current().is_none_or(|p| p.segs.is_empty()) {
            return;
        }
        if self.exp.format == Format::Video
            && self.current().is_none_or(|p| self.video_file(p).is_none())
        {
            self.exp.format = Format::Srt;
        }
        self.open_select = None;
        self.dialog = Some(Dialog::Export);
        cx.notify();
    }

    /// Names a speaker, for every line they say.
    pub fn open_speaker_rename(&mut self, n: u8, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.current() else { return };
        let lines = p.segs.iter().filter(|s| s.speaker == Some(n)).count();
        let note = format!(
            "{} in this project. Leave it empty to go back to “Speaker {}”.",
            if lines == 1 {
                "1 line".to_string()
            } else {
                format!("{lines} lines")
            },
            n + 1
        );
        let current = p.speakers.get(n as usize).cloned().unwrap_or_default();
        let target = p.id.clone();
        self.rename_input
            .update(cx, |s, cx| s.set_value(current, window, cx));
        self.dialog = Some(Dialog::Rename {
            target,
            speaker: Some(n),
            note,
            err: None,
        });
        self.rename_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub fn open_translate(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.current_mut() else { return };
        if p.to == p.from {
            p.to = if p.from == "en" {
                "zh".into()
            } else {
                "en".into()
            };
        }
        self.open_select = None;
        self.dialog = Some(Dialog::Translate);
        cx.notify();
    }

    /// The remembered answer to "translate?" for the next capture or video.
    pub fn toggle_translate_default(&mut self, cx: &mut Context<Self>) {
        self.settings.translate = !self.settings.translate;
        if let Err(e) = store::save_settings(&self.settings) {
            eprintln!("saving settings: {e:#}");
        }
        cx.notify();
    }

    // ─── live: profiles ────────────────────────────────────

    pub fn profile(&self, name: &str) -> Option<&CaptureProfile> {
        self.settings.profiles.iter().find(|p| p.name == name)
    }

    /// The dialog's current choices, as a profile called `name`.
    pub fn profile_from_live(&self, name: &str) -> CaptureProfile {
        CaptureProfile {
            name: name.to_string(),
            source: self.live.source.key().into(),
            sound_from: self.live.detail.clone(),
            microphone: self.live.mic.clone(),
            screen: self.live.video.clone(),
            quality: self.live.video_quality.key().into(),
        }
    }

    /// Whether the dialog has moved away from the picked profile.
    pub fn profile_changed(&self) -> bool {
        self.profile(&self.settings.profile)
            .is_some_and(|p| *p != self.profile_from_live(&p.name))
    }

    fn apply_profile(&mut self, p: &CaptureProfile) {
        if let Some(s) = Source::from_key(&p.source) {
            self.live.source = s;
        }
        let set = |slot: &mut String, value: &str| {
            if !value.is_empty() {
                *slot = value.to_string();
            }
        };
        set(&mut self.live.detail, &p.sound_from);
        set(&mut self.live.mic, &p.microphone);
        set(&mut self.live.video, &p.screen);
        if let Some(q) = capture::VideoQuality::from_key(&p.quality) {
            self.live.video_quality = q;
        }
    }

    fn pick_profile(&mut self, name: String) {
        if let Some(p) = self.profile(&name).cloned() {
            self.apply_profile(&p);
        }
        self.settings.profile = name;
        self.save_settings_quietly();
    }

    pub fn begin_profile_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_select = None;
        let name = self.settings.profile.clone();
        self.profile_input.update(cx, |s, cx| {
            s.set_value(name, window, cx);
            s.focus(window, cx);
        });
        self.naming_profile = Some(None);
        cx.notify();
    }

    pub fn cancel_profile_name(&mut self, cx: &mut Context<Self>) {
        self.naming_profile = None;
        cx.notify();
    }

    /// Saves the dialog's choices under the typed name, replacing a profile of that name.
    pub fn save_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.naming_profile.is_none() {
            return;
        }
        let name = self.profile_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.naming_profile = Some(Some(
                "Name the profile so you can pick it next time.".into(),
            ));
            self.profile_input.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
            return;
        }
        let p = self.profile_from_live(&name);
        match self.settings.profiles.iter_mut().find(|q| q.name == name) {
            Some(slot) => *slot = p,
            None => self.settings.profiles.push(p),
        }
        self.settings.profile = name.clone();
        self.naming_profile = None;
        self.save_settings_quietly();
        self.toast(
            format!("Saved the “{name}” profile."),
            ToastKind::Ok,
            None,
            cx,
        );
    }

    pub fn update_profile(&mut self, cx: &mut Context<Self>) {
        let name = self.settings.profile.clone();
        let p = self.profile_from_live(&name);
        if let Some(slot) = self.settings.profiles.iter_mut().find(|q| q.name == name) {
            *slot = p;
            self.save_settings_quietly();
            self.toast(
                format!("Updated the “{name}” profile."),
                ToastKind::Ok,
                None,
                cx,
            );
        }
    }

    pub fn delete_profile(&mut self, cx: &mut Context<Self>) {
        let name = std::mem::take(&mut self.settings.profile);
        self.settings.profiles.retain(|p| p.name != name);
        self.save_settings_quietly();
        self.toast(
            format!("Deleted the “{name}” profile."),
            ToastKind::Ok,
            None,
            cx,
        );
    }

    fn save_settings_quietly(&self) {
        if let Err(e) = store::save_settings(&self.settings) {
            eprintln!("saving settings: {e:#}");
        }
    }

    /// Shows the project's own audio file in Finder.
    pub fn reveal_recording(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.current().and_then(|p| p.source_path.clone()) else {
            return;
        };
        if !std::path::Path::new(&path).exists() {
            self.toast(
                "The audio file for this capture is gone.",
                ToastKind::Warn,
                None,
                cx,
            );
            return;
        }
        if let Err(e) = std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .spawn()
        {
            self.toast(
                format!("Could not open Finder: {e}."),
                ToastKind::Bad,
                None,
                cx,
            );
        }
    }

    pub fn open_video(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.current().and_then(|p| p.video_path.clone()) else {
            return;
        };
        if !std::path::Path::new(&path).exists() {
            self.toast(
                "The screen recording for this capture is gone.",
                ToastKind::Warn,
                None,
                cx,
            );
            return;
        }
        if let Err(e) = std::process::Command::new("open").arg(&path).spawn() {
            self.toast(
                format!("Could not open the video: {e}."),
                ToastKind::Bad,
                None,
                cx,
            );
        }
    }

    pub fn toggle_new_menu(&mut self, cx: &mut Context<Self>) {
        self.new_menu = !self.new_menu;
        self.open_select = None;
        cx.notify();
    }

    pub(crate) fn reset_editor_view(&mut self) {
        self.player = None;
        self.player_loading = None;
        self.play_line = None;
        self.editor.active = None;
        self.editor.loading.clear();
        self.editor.review_only = false;
        self.editor.editing = None;
        self.video = None;
        self.video_seen = 0;
        self.video_app = false;
        self.video_screen = false;
        if let Some(frame) = self.video_frame.take() {
            self.retired_video.push(frame);
        }
    }

    // ─── toasts ────────────────────────────────────────────

    pub fn toast(
        &mut self,
        msg: impl Into<String>,
        kind: ToastKind,
        action: Option<(&str, ToastAction)>,
        cx: &mut Context<Self>,
    ) {
        self.next_id += 1;
        let id = self.next_id;
        let msg = msg.into();
        let life = if action.is_some() || kind == ToastKind::Bad {
            8000
        } else {
            4200
        } + (msg.len() as u64 * 15).min(4000);
        // The same message twice in a row is one toast, not a stack.
        self.toasts.retain(|t| t.msg != msg);
        self.toasts.push(Toast {
            id,
            msg,
            kind,
            action: action.map(|(l, a)| (l.to_string(), a)),
        });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(life))
                .await;
            this.update(cx, |this, cx| {
                this.toasts.retain(|t| t.id != id);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub fn run_toast_action(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pos) = self.toasts.iter().position(|t| t.id == id) else {
            return;
        };
        let toast = self.toasts.remove(pos);
        match toast.action.map(|(_, a)| a) {
            Some(ToastAction::Rename(pid)) => self.open_rename_dialog(Some(pid), window, cx),
            Some(ToastAction::Open(pid)) => self.open_project(&pid, window, cx),
            Some(ToastAction::Delete(pid)) => self.delete_project(&pid, window, cx),
            Some(ToastAction::Engines) => self.open_engines(window, cx),
            Some(ToastAction::Restart) => self.restart_to_update(cx),
            None => {}
        }
        cx.notify();
    }

    // ─── selects ───────────────────────────────────────────

    pub fn toggle_select(&mut self, sel: Sel, cx: &mut Context<Self>) {
        let just_closed = self
            .select_closed
            .is_some_and(|(s, at)| s == sel && at.elapsed() < Duration::from_millis(250));
        self.open_select = if self.open_select == Some(sel) || just_closed {
            None
        } else {
            Some(sel)
        };
        if matches!(
            self.open_select,
            Some(Sel::SourceDetail | Sel::MicDetail | Sel::VideoDetail)
        ) {
            self.refresh_devices();
        }
        self.select_closed = None;
        cx.notify();
    }

    pub fn dismiss_select(&mut self, cx: &mut Context<Self>) {
        if let Some(sel) = self.open_select.take() {
            self.select_closed = Some((sel, Instant::now()));
            cx.notify();
        }
    }

    pub fn select_options(&self, sel: Sel) -> Vec<(String, String)> {
        let langs = || {
            LANGS
                .iter()
                .map(|(c, n)| (c.to_string(), n.to_string()))
                .collect()
        };
        let scopes = || {
            let mut v = vec![("all".to_string(), "Every project".to_string())];
            v.extend(
                self.projects
                    .iter()
                    .filter(|p| p.status != Status::Failed)
                    .map(|p| (p.id.clone(), p.name.clone())),
            );
            v
        };
        match sel {
            Sel::SourceDetail => self
                .live
                .devices
                .iter()
                .map(|s| (s.clone(), s.clone()))
                .collect(),
            Sel::MicDetail => self
                .live
                .mics
                .iter()
                .map(|s| (s.clone(), s.clone()))
                .collect(),
            Sel::VideoDetail => self
                .live
                .videos
                .iter()
                .map(|s| (s.clone(), s.clone()))
                .collect(),
            Sel::Profile => std::iter::once((String::new(), "No profile".to_string()))
                .chain(
                    self.settings
                        .profiles
                        .iter()
                        .map(|p| (p.name.clone(), p.name.clone())),
                )
                .collect(),
            Sel::VideoQuality => capture::VideoQuality::ALL
                .iter()
                .map(|q| (q.key().to_string(), q.label().to_string()))
                .collect(),
            Sel::LiveFrom | Sel::LiveTo | Sel::ExFrom | Sel::ExTo | Sel::TrTo | Sel::SumLang => {
                let mut v: Vec<(String, String)> = langs();
                if let Some(Dialog::Summarize { lang, .. }) = &self.dialog {
                    if !lang.is_empty() && !v.iter().any(|(c, _)| c == lang) {
                        v.insert(0, (lang.clone(), lang_name(lang)));
                    }
                }
                v
            }
            Sel::SumModel => match &self.dialog {
                Some(Dialog::Summarize { models, .. }) => {
                    let mut v: Vec<(String, String)> =
                        models.iter().map(|m| (m.clone(), m.clone())).collect();
                    v.push(("__other__".into(), "Type a model name".into()));
                    v
                }
                _ => Vec::new(),
            },
            Sel::TermScope => scopes(),
            Sel::TermRule => Rule::ALL
                .iter()
                .map(|r| (format!("{r:?}"), r.option_label().to_string()))
                .collect(),
        }
    }

    pub fn select_value(&self, sel: Sel) -> String {
        let scope_key = |s: &Scope| match s {
            Scope::All => "all".to_string(),
            Scope::Project(id) => id.clone(),
        };
        match sel {
            Sel::SourceDetail => self.live.detail.clone(),
            Sel::MicDetail => self.live.mic.clone(),
            Sel::VideoDetail => self.live.video.clone(),
            Sel::VideoQuality => self.live.video_quality.key().to_string(),
            Sel::Profile => self.settings.profile.clone(),
            Sel::LiveFrom => self.live.from.clone(),
            Sel::LiveTo => self.live.to.clone(),
            Sel::ExFrom => self.extract.from.clone(),
            Sel::ExTo => self.extract.to.clone(),
            Sel::TrTo => self.current().map(|p| p.to.clone()).unwrap_or_default(),
            Sel::SumLang => match &self.dialog {
                Some(Dialog::Summarize { lang, .. }) => lang.clone(),
                _ => "en".into(),
            },
            Sel::SumModel => match &self.dialog {
                Some(Dialog::Summarize { custom: true, .. }) => "__other__".into(),
                Some(Dialog::Summarize { model, .. }) => model.clone(),
                _ => String::new(),
            },
            Sel::TermScope => match &self.dialog {
                Some(Dialog::Term { scope, .. }) => scope_key(scope),
                _ => "all".into(),
            },
            Sel::TermRule => match &self.dialog {
                Some(Dialog::Term { rule, .. }) => format!("{rule:?}"),
                _ => "Prefer".into(),
            },
        }
    }

    pub fn pick(&mut self, sel: Sel, key: String, window: &mut Window, cx: &mut Context<Self>) {
        self.open_select = None;
        let to_scope = |k: &str| {
            if k == "all" {
                Scope::All
            } else {
                Scope::Project(k.to_string())
            }
        };
        match sel {
            Sel::SourceDetail => self.live.detail = key,
            Sel::MicDetail => self.live.mic = key,
            Sel::VideoDetail => self.live.video = key,
            Sel::Profile => self.pick_profile(key),
            Sel::VideoQuality => {
                if let Some(q) = capture::VideoQuality::from_key(&key) {
                    self.live.video_quality = q;
                }
            }
            Sel::LiveFrom => {
                self.live.from = key;
                self.live.partial = None;
            }
            Sel::LiveTo => {
                self.live.to = key;
                self.live.partial = None;
            }
            Sel::ExFrom => self.extract.from = key,
            Sel::ExTo => self.extract.to = key,
            Sel::TrTo => {
                if let Some(p) = self.current_mut() {
                    p.to = key;
                }
            }
            Sel::SumLang => {
                if let Some(Dialog::Summarize { lang, .. }) = &mut self.dialog {
                    *lang = key;
                }
            }
            Sel::SumModel => {
                let custom = key == "__other__";
                if let Some(Dialog::Summarize {
                    model,
                    custom: slot,
                    ..
                }) = &mut self.dialog
                {
                    *slot = custom;
                    if !custom {
                        *model = key;
                    }
                }
                if custom {
                    self.sum_model.update(cx, |s, cx| s.focus(window, cx));
                }
            }
            Sel::TermScope => {
                if let Some(Dialog::Term { scope, .. }) = &mut self.dialog {
                    *scope = to_scope(&key);
                }
            }
            Sel::TermRule => {
                let r = Rule::ALL
                    .into_iter()
                    .find(|r| format!("{r:?}") == key)
                    .unwrap_or(Rule::Prefer);
                if let Some(Dialog::Term {
                    rule, target_err, ..
                }) = &mut self.dialog
                {
                    *rule = r;
                    if r == Rule::Keep {
                        *target_err = None;
                    }
                }
            }
        }
        cx.notify();
    }

    // ─── live: source ──────────────────────────────────────

    pub fn set_source(&mut self, source: Source, cx: &mut Context<Self>) {
        if self.live.status != LiveStatus::Idle || source == Source::Screen {
            return;
        }
        self.live.source = source;
        cx.notify();
    }

    fn refresh_devices(&mut self) {
        match self.open_select {
            Some(Sel::VideoDetail) => {
                self.live.videos = capture::list_video_targets();
                if !self.live.videos.contains(&self.live.video) {
                    self.live.videos.push(self.live.video.clone());
                }
            }
            Some(Sel::MicDetail) => {
                self.live.mics = capture::list_mics();
                if !self.live.mics.contains(&self.live.mic) {
                    self.live.mics.push(self.live.mic.clone());
                }
            }
            _ => {
                self.live.devices = capture::list_apps();
                if !self.live.devices.contains(&self.live.detail) {
                    self.live.devices.push(self.live.detail.clone());
                }
            }
        }
    }

    pub fn swap_langs(&mut self, cx: &mut Context<Self>) {
        if self.live.status != LiveStatus::Idle {
            return;
        }
        std::mem::swap(&mut self.live.from, &mut self.live.to);
        self.live.partial = None;
        cx.notify();
    }

    pub fn source_label(&self) -> String {
        let l = &self.live;
        match l.source {
            Source::Mic => format!("{} · {}", l.source.label(), l.mic),
            Source::Both => format!("{} · {} · {}", l.source.label(), l.detail, l.mic),
            _ => format!("{} · {}", l.source.label(), l.detail),
        }
    }

    pub(crate) fn autoscroll(&self) {
        let h = &self.lines_scroll;
        let remaining = h.max_offset().height + h.offset().y;
        if remaining < gpui::px(160.) {
            h.scroll_to_bottom();
        }
    }

    /// Deleting a project cannot quietly delete glossary work, so any term
    /// tied to it widens to every project instead.
    pub(crate) fn remove_project(&mut self, id: &str) -> usize {
        if let Some(job) = self.jobs.remove(id) {
            job.cancel.store(true, Ordering::Relaxed);
        }
        if self.summary_job.as_ref().is_some_and(|j| j.pid == id) {
            if let Some(job) = self.summary_job.take() {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        if self.correcting.as_ref().is_some_and(|j| j.pid == id) {
            if let Some(job) = self.correcting.take() {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        if let Some(p) = self.project(id).filter(|p| p.kind == Kind::Audio) {
            for file in [&p.source_path, &p.video_path].into_iter().flatten() {
                let _ = std::fs::remove_file(file);
            }
        }
        if self
            .live
            .draining
            .as_ref()
            .is_some_and(|(pid, _)| pid == id)
        {
            self.live.draining = None;
        }
        self.projects.retain(|p| p.id != id);
        let mut widened = 0;
        for g in self.glossary.iter_mut() {
            if g.scope == Scope::Project(id.to_string()) {
                g.scope = Scope::All;
                widened += 1;
            }
        }
        if self.project_id.as_deref() == Some(id) {
            self.project_id = self.projects.first().map(|p| p.id.clone());
            self.reset_editor_view();
            if self.view == View::Editor {
                self.view = View::Projects;
            }
        }
        widened
    }

    fn widened_note(widened: usize) -> String {
        match widened {
            0 => String::new(),
            1 => " 1 glossary term now applies to every project.".into(),
            n => format!(" {n} glossary terms now apply to every project."),
        }
    }

    /// Drops the capture outright, without waiting for its last words.
    pub(crate) fn end_session(&mut self) {
        if let Some(s) = self.live.session.take() {
            s.stop();
        }
        self.live.status = LiveStatus::Idle;
        self.live.t0 = None;
        self.live.partial = None;
        self.live.level = 0.0;
        self.live.engine_note = None;
    }

    // ─── rename ────────────────────────────────────────────

    pub fn open_rename_dialog(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let p = match id.as_deref().and_then(|id| self.project(id)) {
            Some(p) => p,
            None => match self.live_project() {
                Some(p) => p,
                None => return,
            },
        };
        let n = p.segs.len();
        let segs = match n {
            0 => "nothing captured yet".to_string(),
            1 => "1 segment".to_string(),
            n => format!("{n} segments"),
        };
        let note = format!(
            "{} · {} · {} · {} → {}.",
            p.kind.label(),
            p.source,
            segs,
            lang_name(&p.from),
            lang_name(&p.to)
        );
        let target = p.id.clone();
        let name = p.name.clone();
        self.rename_input
            .update(cx, |s, cx| s.set_value(name, window, cx));
        self.dialog = Some(Dialog::Rename {
            target,
            speaker: None,
            note,
            err: None,
        });
        self.rename_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub fn rename_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Rename {
            target,
            speaker,
            err,
            ..
        }) = &mut self.dialog
        else {
            return;
        };
        let name = self.rename_input.read(cx).value().trim().to_string();
        if let Some(n) = *speaker {
            let target = target.clone();
            self.close_dialog(window, cx);
            if let Some(p) = self.project_mut(&target) {
                let n = n as usize;
                if p.speakers.len() <= n {
                    p.speakers.resize(n + 1, String::new());
                }
                p.speakers[n] = name;
            }
            cx.notify();
            return;
        }
        if name.is_empty() {
            *err = Some("Name the project so you can find this track again.".into());
            self.rename_input.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
            return;
        }
        let target = target.clone();
        self.close_dialog(window, cx);
        if let Some(p) = self.project_mut(&target) {
            p.name = name.clone();
        }
        self.toast(format!("Renamed to “{name}”."), ToastKind::Ok, None, cx);
    }

    // ─── dialogs ───────────────────────────────────────────

    pub fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.naming_profile = None;
        if matches!(self.dialog.take(), Some(Dialog::Extract)) {
            self.extract_reset(cx);
        }
        self.open_select = None;
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.dialog, Some(Dialog::Engines { .. })) {
            self.save_engines(window, cx);
            return;
        }
        let Some(Dialog::Confirm { action, .. }) = self.dialog.take() else {
            return;
        };
        window.focus(&self.focus);
        match action {
            ConfirmAction::DeleteProject(id) => self.confirm_delete_project(&id, cx),
            ConfirmAction::DeleteTerm(id) => {
                if let Some(pos) = self.glossary.iter().position(|g| g.id == id) {
                    let g = self.glossary.remove(pos);
                    self.settle_all();
                    self.toast(format!("Deleted “{}”.", g.term), ToastKind::Ok, None, cx);
                }
            }
        }
        cx.notify();
    }

    // ─── projects ──────────────────────────────────────────

    pub fn delete_project(&mut self, id: &str, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.project(id) else { return };
        let tied = self
            .glossary
            .iter()
            .filter(|g| g.scope == Scope::Project(id.to_string()))
            .count();
        let recording = self.live.project_id.as_deref() == Some(id);
        let mut text = format!(
            "“{}” loses its {} segments and its export history.",
            p.name,
            p.segs.len()
        );
        if recording {
            text.push_str(" The capture writing into it stops.");
        } else if self.jobs.contains_key(id) {
            text.push_str(" Its translation stops.");
        }
        match tied {
            0 => {}
            1 => text
                .push_str(" The 1 glossary term tied to it will apply to every project instead."),
            n => text.push_str(&format!(
                " The {n} glossary terms tied to it will apply to every project instead."
            )),
        }
        self.dialog = Some(Dialog::Confirm {
            title: "Delete this project?".into(),
            text,
            ok_label: "Delete project".into(),
            action: ConfirmAction::DeleteProject(id.to_string()),
        });
        cx.notify();
    }

    fn confirm_delete_project(&mut self, id: &str, cx: &mut Context<Self>) {
        let name = self.project(id).map(|p| p.name.clone()).unwrap_or_default();
        if self.live.project_id.as_deref() == Some(id) {
            self.end_session();
            self.live.project_id = None;
        }
        if self.extract.project_id.as_deref() == Some(id) {
            self.extract.project_id = None;
        }
        let widened = self.remove_project(id);
        self.toast(
            format!("Deleted “{name}”.{}", Self::widened_note(widened)),
            ToastKind::Ok,
            None,
            cx,
        );
    }

    // ─── editor ────────────────────────────────────────────

    pub fn visible_rows(&self) -> Vec<(usize, Vec<Flag>)> {
        let Some(p) = self.current() else {
            return Vec::new();
        };
        (0..p.segs.len())
            .filter_map(|i| {
                let flags = flags_for(&self.glossary, p, i);
                (!self.editor.review_only || to_check(&flags)).then_some((i, flags))
            })
            .collect()
    }

    pub fn toggle_review_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_editing(window, cx);
        self.editor.review_only = !self.editor.review_only;
        cx.notify();
    }

    pub fn activate_row(&mut self, i: usize, cx: &mut Context<Self>) {
        self.editor.active = Some(i);
        cx.notify();
    }

    pub fn start_editing(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.loading.contains(&i) {
            return;
        }
        let Some(text) = self.current().and_then(|p| {
            p.segs.get(i).map(|s| {
                if p.translate {
                    s.trans.clone()
                } else {
                    s.orig.clone()
                }
            })
        }) else {
            return;
        };
        self.editor.active = Some(i);
        if self.editor.editing != Some(i) {
            self.editor.editing = None;
            self.seg_input
                .update(cx, |s, cx| s.set_value(text, window, cx));
            self.editor.editing = Some(i);
        }
        self.seg_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub fn stop_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.editing.take().is_some() {
            window.focus(&self.focus);
            cx.notify();
        }
    }

    /// Sends the chosen lines back through the translator, then the glossary.
    pub fn retranslate(
        &mut self,
        indices: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if indices.is_empty() {
            return;
        }
        self.stop_editing(window, cx);
        let Some(pid) = self.project_id.clone() else {
            return;
        };
        if !self.require_mt(cx) {
            return;
        }
        if self.jobs.contains_key(&pid) {
            self.toast(
                "This project is still being translated. Try again when it finishes.",
                ToastKind::Warn,
                None,
                cx,
            );
            return;
        }
        self.editor.loading.extend(indices.iter().copied());
        self.translate_project(&pid, Some(indices), cx);
        cx.notify();
    }

    // ─── files ─────────────────────────────────────────────

    pub fn browse_video(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |this, cx| this.pick_path(path, cx)).ok();
                }
            }
        })
        .detach();
    }

    pub fn browse_subtitles(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update(cx, |this, cx| this.import_paths(paths, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Files dropped anywhere: subtitle files become projects straight away,
    /// a video opens the track picker.
    pub fn drop_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let (subs, other): (Vec<PathBuf>, Vec<PathBuf>) = paths
            .into_iter()
            .partition(|p| crate::subs::is_subtitle(&p.to_string_lossy()));
        if !subs.is_empty() {
            self.import_paths(subs, cx);
        }
        if let Some(video) = other.into_iter().next() {
            self.pick_path(video, cx);
        }
    }

    // ─── glossary ──────────────────────────────────────────

    pub fn visible_terms(&self, cx: &App) -> Vec<&Term> {
        let q = self.gl_search.read(cx).value().trim().to_lowercase();
        self.glossary
            .iter()
            .filter(|g| {
                q.is_empty()
                    || format!("{} {}", g.term, g.target)
                        .to_lowercase()
                        .contains(&q)
            })
            .collect()
    }

    pub fn open_term_dialog(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let g = id
            .as_deref()
            .and_then(|id| self.glossary.iter().find(|g| g.id == id))
            .cloned();
        let (term, target) = g
            .as_ref()
            .map(|g| (g.term.clone(), g.target.clone()))
            .unwrap_or_default();
        self.term_input
            .update(cx, |s, cx| s.set_value(term, window, cx));
        self.target_input
            .update(cx, |s, cx| s.set_value(target, window, cx));
        let scope = match &g {
            Some(g) => g.scope.clone(),
            None => Scope::All,
        };
        let scope = match scope {
            Scope::Project(id) if self.project(&id).is_none() => Scope::All,
            s => s,
        };
        self.dialog = Some(Dialog::Term {
            editing: g.as_ref().map(|g| g.id.clone()),
            rule: g.as_ref().map(|g| g.rule).unwrap_or(Rule::Prefer),
            scope,
            term_err: None,
            target_err: None,
        });
        self.term_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn term_errors(&self, cx: &App) -> (Option<String>, Option<String>) {
        let rule = match &self.dialog {
            Some(Dialog::Term { rule, .. }) => *rule,
            _ => return (None, None),
        };
        let term = self.term_input.read(cx).value().trim().is_empty();
        let target = rule != Rule::Keep && self.target_input.read(cx).value().trim().is_empty();
        (
            term.then(|| "Enter the word as it appears in the source.".into()),
            target.then(|| {
                if rule == Rule::Forbid {
                    "Enter the translation you want blocked.".into()
                } else {
                    "Enter the translation to use instead.".into()
                }
            }),
        )
    }

    fn validate_term_field(&mut self, is_target: bool, cx: &mut Context<Self>) {
        let (t, g) = self.term_errors(cx);
        if let Some(Dialog::Term {
            term_err,
            target_err,
            ..
        }) = &mut self.dialog
        {
            if is_target {
                *target_err = g;
            } else {
                *term_err = t;
            }
            cx.notify();
        }
    }

    fn validate_term_fields(&mut self, cx: &mut Context<Self>) -> bool {
        let (t, g) = self.term_errors(cx);
        let ok = t.is_none() && g.is_none();
        if let Some(Dialog::Term {
            term_err,
            target_err,
            ..
        }) = &mut self.dialog
        {
            *term_err = t;
            *target_err = g;
        }
        cx.notify();
        ok
    }

    pub fn save_term(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.validate_term_fields(cx) {
            let term_bad = matches!(
                &self.dialog,
                Some(Dialog::Term {
                    term_err: Some(_),
                    ..
                })
            );
            let input = if term_bad {
                &self.term_input
            } else {
                &self.target_input
            };
            input.update(cx, |s, cx| s.focus(window, cx));
            return;
        }
        let Some(Dialog::Term {
            editing,
            rule,
            scope,
            ..
        }) = &self.dialog
        else {
            return;
        };
        let (editing, rule, scope) = (editing.clone(), *rule, scope.clone());
        let term = self.term_input.read(cx).value().trim().to_string();
        let target = if rule == Rule::Keep {
            String::new()
        } else {
            self.target_input.read(cx).value().trim().to_string()
        };
        let msg = match editing.and_then(|id| self.glossary.iter_mut().find(|g| g.id == id)) {
            Some(g) => {
                g.term = term.clone();
                g.target = target;
                g.rule = rule;
                g.scope = scope;
                format!("Updated “{term}”.")
            }
            None => {
                let id = self.fresh_id("g");
                self.glossary.push(Term {
                    id,
                    term: term.clone(),
                    target,
                    rule,
                    scope,
                    hits: 0,
                });
                format!("Added “{term}” to the glossary.")
            }
        };
        self.close_dialog(window, cx);
        self.settle_all();
        self.toast(msg, ToastKind::Ok, None, cx);
    }

    pub fn ask_delete_term(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(g) = self.glossary.iter().find(|g| g.id == id) else {
            return;
        };
        self.dialog = Some(Dialog::Confirm {
            title: "Delete this term?".into(),
            text: format!(
                "“{}” will stop being enforced. Segments already translated keep their current wording.",
                g.term
            ),
            ok_label: "Delete term".into(),
            action: ConfirmAction::DeleteTerm(id.to_string()),
        });
        cx.notify();
    }

    // ─── export ────────────────────────────────────────────

    pub fn build_export(&self) -> Option<Built> {
        let p = self.current()?;
        let mut o = self.exp.options();
        if !p.translate {
            o.layout = Layout::Orig;
        }
        o.speakers &= p.has_speakers();
        Some(build_export(&self.glossary, p, &o))
    }

    fn record_export(&mut self, cues: usize, copied: bool) {
        let (format, layout) = (self.exp.format, self.exp.layout);
        if let Some(p) = self.current_mut() {
            p.exports.insert(
                0,
                ExportRecord {
                    at_ms: now_ms(),
                    format,
                    layout,
                    cues,
                    copied,
                },
            );
        }
    }

    pub fn copy_summary(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self
            .current()
            .map(|p| p.summary.clone())
            .filter(|s| !s.trim().is_empty())
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.toast("Copied the summary.", ToastKind::Ok, None, cx);
    }

    pub fn copy_export(&mut self, cx: &mut Context<Self>) {
        let Some(built) = self.build_export() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(built.text));
        self.record_export(built.count, true);
        self.toast(
            format!("Copied {} cues as {}.", built.count, built.filename),
            ToastKind::Ok,
            None,
            cx,
        );
    }

    pub fn save_export(&mut self, cx: &mut Context<Self>) {
        let Some(built) = self.build_export() else {
            return;
        };
        if self.exp.format == Format::Video {
            self.start_video_export(built, cx);
            return;
        }
        let dir = self.export_dir();
        let rx = cx.prompt_for_new_path(&dir, Some(&built.filename));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            let result = std::fs::write(&path, built.text.as_bytes());
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.record_export(built.count, false);
                    let file = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or(built.filename);
                    this.toast(
                        format!("Wrote {file} · {} cues.", built.count),
                        ToastKind::Ok,
                        None,
                        cx,
                    );
                }
                Err(e) => this.toast(
                    format!("Could not write the file: {e}."),
                    ToastKind::Bad,
                    None,
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    fn export_dir(&self) -> PathBuf {
        self.current()
            .filter(|p| p.kind != Kind::Audio)
            .and_then(|p| p.source_path.as_deref())
            .and_then(|p| std::path::Path::new(p).parent().map(PathBuf::from))
            .filter(|d| d.is_dir())
            .or_else(|| dirs::download_dir().filter(|d| d.is_dir()))
            .unwrap_or_else(std::env::temp_dir)
    }

    /// Asks where to write, then burns the cues into a new video on a background thread.
    fn start_video_export(&mut self, built: Built, cx: &mut Context<Self>) {
        if self.video_export.is_some() {
            self.toast(
                "A video is already being written.",
                ToastKind::Bad,
                None,
                cx,
            );
            return;
        }
        let Some(p) = self.current() else { return };
        let Some(src) = self.video_file(p) else {
            self.toast(
                "This project has no video to write into.",
                ToastKind::Bad,
                None,
                cx,
            );
            return;
        };
        let pid = p.id.clone();
        let layout = if p.translate {
            self.exp.layout
        } else {
            Layout::Orig
        };
        let dir = self.export_dir();
        let rx = cx.prompt_for_new_path(&dir, Some(&built.filename));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(dest))) = rx.await else { return };
            this.update(cx, |this, cx| {
                this.launch_video_export(pid, src, dest, built.text, built.count, layout, cx);
            })
            .ok();
        })
        .detach();
    }

    pub fn set_wrap_from_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let b = self.exp.wrap_bounds.get();
        if b.size.width <= gpui::px(0.) {
            return;
        }
        let ratio = ((x - b.origin.x) / b.size.width).clamp(0.0, 1.0);
        let steps = ((ratio * 18.0).round() as usize).min(18);
        let wrap = 24 + steps * 2;
        if wrap != self.exp.wrap {
            self.exp.wrap = wrap;
            cx.notify();
        }
    }

    pub fn nudge_wrap(&mut self, delta: i32, cx: &mut Context<Self>) {
        self.exp.wrap = (self.exp.wrap as i32 + delta * 2).clamp(24, 60) as usize;
        cx.notify();
    }
}

impl Focusable for Workbench {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
