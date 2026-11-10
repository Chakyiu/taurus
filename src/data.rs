//! The workbench model. One media track is one project: a project owns its
//! source, language pair, segments, review status and export history.

use std::ops::Range;

use serde::{Deserialize, Serialize};

pub const LANGS: &[(&str, &str)] = &[
    ("en", "English"),
    ("zh", "Chinese (Simplified)"),
    ("zh-Hant", "Chinese (Traditional)"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("de", "German"),
    ("es", "Spanish"),
    ("fr", "French"),
];

pub fn lang_name(code: &str) -> String {
    LANGS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| code.to_string())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// This Mac's output and a microphone, mixed.
    Both,
    System,
    Mic,
    Screen,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Both => "This Mac and microphone",
            Source::System => "This Mac",
            Source::Mic => "Microphone",
            Source::Screen => "Screen region",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Source::Both => "both",
            Source::System => "system",
            Source::Mic => "mic",
            Source::Screen => "screen",
        }
    }

    pub fn from_key(key: &str) -> Option<Source> {
        [Source::Both, Source::System, Source::Mic]
            .into_iter()
            .find(|s| s.key() == key)
    }
}

// ─── time formatting ───────────────────────────────────────

pub fn mmss(ms: i64) -> String {
    let t = (ms.max(0)) / 1000;
    format!("{:02}:{:02}", t / 60, t % 60)
}

pub fn srt_time(ms: i64) -> String {
    let t = ms.max(0);
    format!(
        "{:02}:{:02}:{:02},{:03}",
        t / 3_600_000,
        (t / 60_000) % 60,
        (t / 1000) % 60,
        t % 1000
    )
}

pub fn vtt_time(ms: i64) -> String {
    srt_time(ms).replace(',', ".")
}

pub fn ass_time(ms: i64) -> String {
    let t = ms.max(0);
    format!(
        "{}:{:02}:{:02}.{:02}",
        t / 3_600_000,
        (t / 60_000) % 60,
        (t / 1000) % 60,
        (t % 1000) / 10
    )
}

/// Characters as a reader counts them, so CJK and Latin lines measure alike.
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

// ─── content ───────────────────────────────────────────────

#[cfg(test)]
pub struct ScriptLine {
    pub en: &'static str,
    pub zh: &'static str,
}

#[cfg(test)]
pub const LIVE_SCRIPT: &[ScriptLine] = &[
    ScriptLine {
        en: "Good morning, and thanks for staying through the last session of the day.",
        zh: "大家早上好，感谢各位坚持到今天的最后一场。",
    },
    ScriptLine {
        en: "I want to talk about how we cut [[end-to-end latency]] in our captioning pipeline.",
        zh: "我想聊聊我们是怎么把字幕流水线的[[端到端延迟]]降下来的。",
    },
    ScriptLine {
        en: "When we started, one sentence took almost four seconds to reach the viewer.",
        zh: "刚开始的时候，一句话要将近四秒才能送到观众眼前。",
    },
    ScriptLine {
        en: "The bottleneck was never the model. It was everything standing around the model.",
        zh: "瓶颈从来不是模型本身，而是围在模型周围的那一圈东西。",
    },
    ScriptLine {
        en: "So we moved to [[streaming inference]] with a two hundred millisecond window.",
        zh: "于是我们换成了[[流式推理]]，窗口定在两百毫秒。",
    },
    ScriptLine {
        en: "That single change took us from four seconds down to about nine hundred.",
        zh: "仅这一项改动，就把四秒压到了九百毫秒左右。",
    },
    ScriptLine {
        en: "The second problem was terminology: product names kept getting translated.",
        zh: "第二个问题是术语：产品名老是被翻掉。",
    },
    ScriptLine {
        en: "So we pin those terms before the [[decoder]] ever sees them.",
        zh: "所以我们在[[解码器]]看到它们之前，就先把这些词钉住。",
    },
    ScriptLine {
        en: "Reviewers went from rewriting every third line to approving most of the file.",
        zh: "校对从每三行改一行，变成了整份文件基本直接通过。",
    },
    ScriptLine {
        en: "That is the whole talk. The rest is tuning, and I am happy to take questions.",
        zh: "这就是全部内容了，剩下的都是调参，欢迎提问。",
    },
];

#[cfg(test)]
fn strip_marks(s: &str) -> String {
    s.replace("[[", "").replace("]]", "")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Segment {
    pub in_ms: i64,
    pub out_ms: i64,
    pub orig: String,
    pub trans: String,
    /// What the engine produced, before any glossary rules or hand edits.
    pub base: String,
    /// Index into the project's speakers, once voices have been identified.
    #[serde(default)]
    pub speaker: Option<u8>,
    /// A correction of `orig` suggested by the language model, waiting to be
    /// accepted or turned down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Segment {
    pub fn new(in_ms: i64, out_ms: i64, orig: &str, trans: &str) -> Self {
        Self {
            in_ms,
            out_ms,
            orig: orig.into(),
            trans: trans.into(),
            base: trans.into(),
            speaker: None,
            fix: None,
        }
    }
}

/// A finished track: every cue translated, inside the 17 cps and 42-character
/// limits, with readable gaps.
#[cfg(test)]
fn srt_segments() -> Vec<Segment> {
    [
        (
            2120,
            6120,
            "凌晨四点，这条街才刚刚开始醒来。",
            "At four in the morning, this street\nis only just waking up.",
        ),
        (
            6400,
            9400,
            "老陈的摊子是第一个亮灯的。",
            "Lao Chen's stall is the first one\nto light up.",
        ),
        (
            9600,
            13000,
            "他的卤水已经养了十九年。",
            "His master stock has been going\nfor nineteen years.",
        ),
        (
            13240,
            16440,
            "「每天只添，不换。」",
            "\"You only ever top it up.\nYou never replace it.\"",
        ),
        (
            16700,
            21200,
            "一锅卤水，就是一本账。",
            "A pot of master stock is a ledger\nof everything that has gone into it.",
        ),
        (
            21500,
            25500,
            "天亮之前，第一笼包子上桌。",
            "Before sunrise, the first basket\nof buns reaches the table.",
        ),
        (
            25700,
            28500,
            "客人大多不说话。",
            "Most of the customers\ndo not say a word.",
        ),
        (
            28740,
            31340,
            "他们只是坐下，然后开始吃。",
            "They simply sit down\nand start eating.",
        ),
        (
            31600,
            35000,
            "老陈说，这就是他想要的声音。",
            "Lao Chen says this is exactly\nthe sound he was after.",
        ),
        (
            35300,
            38900,
            "六点一过，街口排起了长队。",
            "After six, a queue forms\nall the way down the street.",
        ),
        (
            39100,
            42700,
            "队伍里有刚下夜班的护士。",
            "There are nurses in the queue\njust off the night shift.",
        ),
        (
            42900,
            45700,
            "也有赶着上早课的学生。",
            "And students on their way\nto an early class.",
        ),
        (
            45900,
            49300,
            "他们点的东西几乎一样。",
            "They order almost exactly\nthe same thing every time.",
        ),
        (
            49500,
            53100,
            "一碗面，一个蛋，加两块钱的辣。",
            "A bowl of noodles, an egg,\nand two yuan of spicy sauce.",
        ),
    ]
    .iter()
    .map(|(a, b, o, t)| Segment::new(*a, *b, o, t))
    .collect()
}

#[cfg(test)]
fn talk_segments() -> Vec<Segment> {
    LIVE_SCRIPT
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let start = 4200 + i as i64 * 6400;
            let plain = strip_marks(l.en);
            let dur = (char_len(&plain) as f64 * 58.0).round().max(2200.0) as i64;
            Segment::new(start, start + dur, &plain, &strip_marks(l.zh))
        })
        .collect()
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegSource {
    Talk,
    Srt,
}

#[cfg(test)]
impl SegSource {
    pub fn base(self) -> Vec<Segment> {
        match self {
            SegSource::Talk => talk_segments(),
            SegSource::Srt => srt_segments(),
        }
    }
}

// ─── projects ──────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Kind {
    Video,
    Audio,
    Subtitle,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Video => "Video",
            Kind::Audio => "Captured audio",
            Kind::Subtitle => "Imported subtitles",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Status {
    Done,
    Review,
    Working,
    Failed,
    Live,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Format {
    Srt,
    Vtt,
    Ass,
    Txt,
    /// A new video with the cues burned into the picture.
    Video,
}

impl Format {
    pub const ALL: [Format; 5] = [
        Format::Srt,
        Format::Vtt,
        Format::Ass,
        Format::Txt,
        Format::Video,
    ];

    pub fn ext(self) -> &'static str {
        match self {
            Format::Srt => "srt",
            Format::Vtt => "vtt",
            Format::Ass => "ass",
            Format::Txt => "txt",
            Format::Video => "mp4",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Layout {
    Stacked,
    OrigFirst,
    Trans,
    Orig,
}

impl Layout {
    pub const ALL: [Layout; 4] = [
        Layout::Stacked,
        Layout::OrigFirst,
        Layout::Trans,
        Layout::Orig,
    ];

    pub fn description(self) -> &'static str {
        match self {
            Layout::Stacked => {
                "Translation first, original underneath, the usual choice for learners."
            }
            Layout::OrigFirst => "Keeps the spoken line in the reading position.",
            Layout::Trans => "One line per cue. Smallest file, cleanest screen.",
            Layout::Orig => "Ships the transcript without the translation.",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportRecord {
    pub at_ms: i64,
    pub format: Format,
    pub layout: Layout,
    pub cues: usize,
    pub copied: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub created_ms: i64,
    /// What the project was made from, as shown in the list: a file name or a capture source.
    pub source: String,
    /// The file on disk the project was read from, when there is one.
    #[serde(default)]
    pub source_path: Option<String>,
    /// A screen recording made alongside a live caption.
    #[serde(default)]
    pub video_path: Option<String>,
    pub status: Status,
    pub from: String,
    pub to: String,
    pub progress: u32,
    pub error: Option<String>,
    pub segs: Vec<Segment>,
    pub exports: Vec<ExportRecord>,
    /// Projects saved before translation became optional were all translated.
    #[serde(default = "yes")]
    pub translate: bool,
    /// Names given to identified speakers; empty means "Speaker n".
    #[serde(default)]
    pub speakers: Vec<String>,
    /// A short account of the lines, written through the translation endpoint.
    #[serde(default)]
    pub summary: String,
    /// Language the summary was written in, as a language code.
    #[serde(default)]
    pub summary_lang: String,
    /// Model that wrote the summary.
    #[serde(default)]
    pub summary_model: String,
    /// The summary was asked for as a fuller account.
    #[serde(default)]
    pub summary_long: bool,
    /// What the recording is about, in the user's words, for correcting it.
    #[serde(default)]
    pub about: String,
}

fn yes() -> bool {
    true
}

pub fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// "Just now", "Today, 09:42", "Yesterday, 17:30", "Monday, 10:05", "12 Oct, 08:15".
pub fn when_label(ms: i64) -> String {
    use chrono::{Datelike, Local, TimeZone};
    let Some(t) = Local.timestamp_millis_opt(ms).single() else {
        return "–".into();
    };
    let now = Local::now();
    if (now.timestamp_millis() - ms).abs() < 60_000 {
        return "Just now".into();
    }
    let hm = t.format("%H:%M");
    match (now.date_naive() - t.date_naive()).num_days() {
        0 => format!("Today, {hm}"),
        1 => format!("Yesterday, {hm}"),
        2..=6 => format!("{}, {hm}", t.format("%A")),
        _ if t.year() == now.year() => format!("{}, {hm}", t.format("%-d %b")),
        _ => t.format("%-d %b %Y").to_string(),
    }
}

/// "night-market_kitchen-e03.zh.srt" → "night market kitchen e03".
pub fn title_from_file(file_name: &str) -> String {
    let mut stem = file_name;
    for _ in 0..2 {
        match stem.rfind('.') {
            Some(i) if i > 0 => {
                let ext = &stem[i + 1..];
                let is_lang = LANGS.iter().any(|(c, _)| c.eq_ignore_ascii_case(ext));
                let is_media = ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric());
                if stem.len() == file_name.len() && is_media || is_lang {
                    stem = &stem[..i];
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    let name = stem
        .split(|c| c == '-' || c == '_' || c == '.')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() {
        file_name.to_string()
    } else {
        name
    }
}

impl Project {
    pub fn when(&self) -> String {
        when_label(self.created_ms)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        kind: Kind,
        name: String,
        source: String,
        source_path: Option<String>,
        status: Status,
        from: &str,
        to: &str,
        segs: Vec<Segment>,
    ) -> Self {
        Self {
            id,
            kind,
            name,
            created_ms: now_ms(),
            source,
            source_path,
            video_path: None,
            status,
            from: from.into(),
            to: to.into(),
            progress: 0,
            error: None,
            segs,
            exports: Vec::new(),
            translate: true,
            speakers: Vec::new(),
            summary: String::new(),
            summary_lang: String::new(),
            summary_model: String::new(),
            summary_long: false,
            about: String::new(),
        }
    }

    /// The spoken lines, labelled by speaker when voices have been named,
    /// for a summary to read.
    pub fn transcript(&self) -> String {
        let mut lines = Vec::new();
        for s in &self.segs {
            let text = s.orig.trim();
            if text.is_empty() {
                continue;
            }
            let line = match s.speaker {
                Some(n) => format!("{}: {text}", self.speaker_name(n)),
                None => text.to_string(),
            };
            lines.push(line);
        }
        lines.join("\n")
    }

    /// The language lines are checked and exported in.
    pub fn out_lang(&self) -> &str {
        if self.translate { &self.to } else { &self.from }
    }

    pub fn has_speakers(&self) -> bool {
        self.segs.iter().any(|s| s.speaker.is_some())
    }

    pub fn speaker_name(&self, n: u8) -> String {
        match self.speakers.get(n as usize) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => format!("Speaker {}", n + 1),
        }
    }

    /// Share of segments that already carry a translation.
    pub fn translated_pct(&self) -> u32 {
        if self.segs.is_empty() {
            return 0;
        }
        let done = self
            .segs
            .iter()
            .filter(|s| !s.trans.trim().is_empty())
            .count();
        (done as f64 / self.segs.len() as f64 * 100.0).round() as u32
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn fixture(
        id: &str,
        kind: Kind,
        name: &str,
        when: &str,
        source: &str,
        status: Status,
        from: &str,
        to: &str,
        src: SegSource,
    ) -> Self {
        let _ = when;
        let mut p = Self::new(
            id.into(),
            kind,
            name.into(),
            source.into(),
            None,
            status,
            from,
            to,
            if status == Status::Failed {
                Vec::new()
            } else {
                src.base()
            },
        );
        p.created_ms = 0;
        p
    }

    /// Each project's own outstanding review work, applied to its copy of the
    /// fixture once. Review completion is then a real count.
    #[cfg(test)]
    fn open_review_work(mut self) -> Self {
        if self.status == Status::Working {
            let reached = (self.segs.len() as f64 * self.progress as f64 / 100.0).round() as usize;
            for s in self.segs.iter_mut().skip(reached) {
                s.trans.clear();
            }
            return self;
        }
        if self.id == "p2" {
            let blank = |s: &mut Segment| s.trans.clear();
            if let Some(s) = self.segs.get_mut(5) {
                blank(s);
            }
            if let Some(s) = self.segs.get_mut(6) {
                s.out_ms += 200;
            }
            if let Some(s) = self.segs.get_mut(9) {
                blank(s);
            }
            if let Some(s) = self.segs.get_mut(12) {
                s.trans = s.trans.replace('\n', " ");
            }
            if let Some(s) = self.segs.get_mut(13) {
                s.out_ms -= 1400;
            }
        }
        self
    }
}

#[cfg(test)]
pub fn fixture_projects() -> Vec<Project> {
    let mut p1 = Project::fixture(
        "p1",
        Kind::Audio,
        "Streaming at Scale, closing keynote",
        "Today, 09:42",
        "This Mac · all system output",
        Status::Done,
        "en",
        "zh",
        SegSource::Talk,
    );
    p1.exports.push(ExportRecord {
        at_ms: 0,
        format: Format::Srt,
        layout: Layout::Stacked,
        cues: 10,
        copied: false,
    });
    let p2 = Project::fixture(
        "p2",
        Kind::Subtitle,
        "Night Market Kitchen, episode 3",
        "Today, 08:15",
        "night-market-kitchen-e03.zh.srt",
        Status::Review,
        "zh",
        "en",
        SegSource::Srt,
    );
    let mut p3 = Project::fixture(
        "p3",
        Kind::Video,
        "Quarterly product demo",
        "Yesterday, 17:30",
        "quarterly-product-demo.mp4",
        Status::Working,
        "en",
        "zh",
        SegSource::Talk,
    );
    p3.progress = 60;
    let mut p4 = Project::fixture(
        "p4",
        Kind::Subtitle,
        "Rail safety briefing",
        "Yesterday, 11:02",
        "rail-safety-briefing.de.vtt",
        Status::Done,
        "de",
        "en",
        SegSource::Srt,
    );
    p4.exports.push(ExportRecord {
        at_ms: 0,
        format: Format::Vtt,
        layout: Layout::Trans,
        cues: 14,
        copied: false,
    });
    let mut p5 = Project::fixture(
        "p5",
        Kind::Audio,
        "Team standup",
        "Monday, 10:05",
        "AirPods Pro",
        Status::Failed,
        "en",
        "zh",
        SegSource::Talk,
    );
    p5.error = Some("Microphone disconnected 2 minutes in".into());
    let p6 = Project::fixture(
        "p6",
        Kind::Subtitle,
        "Kitchen Tour, episode 12",
        "Saturday, 20:41",
        "kitchen-tour-e12.ja.ass",
        Status::Done,
        "ja",
        "zh",
        SegSource::Srt,
    );
    vec![p1, p2, p3, p4, p5, p6]
        .into_iter()
        .map(Project::open_review_work)
        .collect()
}

// ─── glossary ──────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Rule {
    Prefer,
    Keep,
    Forbid,
}

impl Rule {
    pub const ALL: [Rule; 3] = [Rule::Prefer, Rule::Keep, Rule::Forbid];

    pub fn option_label(self) -> &'static str {
        match self {
            Rule::Prefer => "Prefer this translation",
            Rule::Keep => "Keep as-is, never translate",
            Rule::Forbid => "Forbid this translation",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Rule::Prefer => {
                "The translator is nudged toward your wording before the decoder commits."
            }
            Rule::Keep => "The term is copied through untouched: product names, people, places.",
            Rule::Forbid => {
                "This translation is rejected; the preferred entry for the same term is used instead."
            }
        }
    }
}

/// A term either applies to every project or is tied to one by its id.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Scope {
    All,
    Project(String),
}

impl Scope {
    pub fn covers(&self, project_id: Option<&str>) -> bool {
        match self {
            Scope::All => true,
            Scope::Project(id) => Some(id.as_str()) == project_id,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Term {
    pub id: String,
    pub term: String,
    pub target: String,
    pub rule: Rule,
    pub scope: Scope,
    pub hits: u32,
}

#[cfg(test)]
pub fn fixture_glossary() -> Vec<Term> {
    let t = |id: &str, term: &str, target: &str, rule, scope: Scope, hits| Term {
        id: id.into(),
        term: term.into(),
        target: target.into(),
        rule,
        scope,
        hits,
    };
    let p = |id: &str| Scope::Project(id.into());
    vec![
        t(
            "g1",
            "end-to-end latency",
            "端到端延迟",
            Rule::Prefer,
            Scope::All,
            34,
        ),
        t(
            "g2",
            "streaming inference",
            "流式推理",
            Rule::Prefer,
            Scope::All,
            21,
        ),
        t("g3", "Taurus", "", Rule::Keep, Scope::All, 58),
        t("g4", "decoder", "解码器", Rule::Prefer, p("p1"), 12),
        t("g5", "latency", "延时", Rule::Forbid, p("p1"), 7),
        t("g6", "老陈", "Lao Chen", Rule::Prefer, p("p2"), 19),
        t("g7", "卤水", "master stock", Rule::Prefer, p("p2"), 11),
        t("g8", "辣", "chilli", Rule::Prefer, p("p2"), 6),
        t("g9", "辣", "spicy", Rule::Forbid, p("p2"), 3),
    ]
}

pub fn active_glossary<'a>(
    glossary: &'a [Term],
    project_id: Option<&'a str>,
) -> impl Iterator<Item = &'a Term> {
    glossary.iter().filter(move |g| g.scope.covers(project_id))
}

pub fn terms_in(glossary: &[Term], text: &str, project_id: Option<&str>) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut list: Vec<String> = Vec::new();
    for g in active_glossary(glossary, project_id) {
        if g.rule == Rule::Forbid {
            continue;
        }
        if lower.contains(&g.term.to_lowercase()) && !list.contains(&g.term) {
            list.push(g.term.clone());
        }
    }
    list
}

/// Swap forbidden renderings for the preferred entry of the same term.
pub fn apply_rules(glossary: &[Term], text: &str, project_id: Option<&str>) -> (String, usize) {
    let mut out = text.to_string();
    let mut changed = 0;
    let active: Vec<&Term> = active_glossary(glossary, project_id).collect();
    for g in active
        .iter()
        .filter(|g| g.rule == Rule::Forbid && !g.target.is_empty())
    {
        let prefer = active
            .iter()
            .filter(|p| p.rule == Rule::Prefer && p.term == g.term && !p.target.is_empty())
            .map(|p| p.target.clone())
            .last();
        let Some(prefer) = prefer else { continue };
        let replaced = replace_ci(&out, &g.target, &prefer);
        if replaced != out {
            out = replaced;
            changed += 1;
        }
    }
    (out, changed)
}

fn replace_ci(haystack: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let lower_h = haystack.to_lowercase();
    let lower_n = needle.to_lowercase();
    if lower_h.len() != haystack.len() {
        return haystack.replace(needle, with);
    }
    let mut out = String::new();
    let mut last = 0;
    for (i, _) in lower_h.match_indices(&lower_n) {
        out.push_str(&haystack[last..i]);
        out.push_str(with);
        last = i + needle.len();
    }
    out.push_str(&haystack[last..]);
    out
}

// ─── subtitle checks ───────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlagKind {
    Ok,
    Fast,
    Long,
    Gap,
    Empty,
    Term,
    /// A suggested correction waits for a decision.
    Fix,
}

#[derive(Clone, Debug)]
pub struct Flag {
    pub kind: FlagKind,
    pub label: String,
}

pub const MAX_CPS: f64 = 17.0;
pub const MAX_LINE: usize = 42;
pub const MIN_GAP_MS: i64 = 80;

pub fn longest_line(text: &str) -> usize {
    text.split('\n').map(char_len).max().unwrap_or(0)
}

pub fn flags_for(glossary: &[Term], p: &Project, i: usize) -> Vec<Flag> {
    let segs = &p.segs;
    let project_id = Some(p.id.as_str());
    let s = &segs[i];
    let dur = ((s.out_ms - s.in_ms) as f64 / 1000.0).max(0.1);
    let text = if p.translate {
        s.trans.trim()
    } else {
        s.orig.trim()
    };
    let mut out = Vec::new();
    let flag = |kind, label: String| Flag { kind, label };
    if text.is_empty() {
        out.push(flag(FlagKind::Empty, "empty".into()));
    } else {
        let cps = char_len(text) as f64 / dur;
        if cps > MAX_CPS {
            out.push(flag(FlagKind::Fast, format!("{} cps", cps.round() as i64)));
        }
        let longest = longest_line(text);
        if longest > MAX_LINE {
            out.push(flag(FlagKind::Long, format!("{longest} ch")));
        }
    }
    if let Some(next) = segs.get(i + 1) {
        let gap = next.in_ms - s.out_ms;
        if gap < MIN_GAP_MS {
            out.push(flag(FlagKind::Gap, format!("{gap} ms gap")));
        }
    }
    let t = if p.translate {
        terms_in(glossary, &s.orig, project_id)
    } else {
        Vec::new()
    };
    if !t.is_empty() {
        let label = if t.len() == 1 {
            "1 term".to_string()
        } else {
            format!("{} terms", t.len())
        };
        out.push(flag(FlagKind::Term, label));
    }
    if s.fix.is_some() {
        out.push(flag(FlagKind::Fix, "suggested fix".into()));
    }
    if out.is_empty() {
        out.push(flag(FlagKind::Ok, "clean".into()));
    }
    out
}

/// A line that would read badly on screen.
pub fn is_flagged(flags: &[Flag]) -> bool {
    flags.iter().any(|f| {
        matches!(
            f.kind,
            FlagKind::Fast | FlagKind::Long | FlagKind::Gap | FlagKind::Empty
        )
    })
}

/// A line to look at: one that reads badly, or one with a fix to decide on.
pub fn to_check(flags: &[Flag]) -> bool {
    is_flagged(flags) || flags.iter().any(|f| f.kind == FlagKind::Fix)
}

pub struct Stats {
    pub count: usize,
    pub flagged: usize,
}

pub fn project_stats(glossary: &[Term], p: &Project) -> Stats {
    let flagged = (0..p.segs.len())
        .filter(|&i| to_check(&flags_for(glossary, p, i)))
        .count();
    Stats {
        count: p.segs.len(),
        flagged,
    }
}

/// Needs review and Done are two readings of one number, so the pill, the
/// Projects list and the context bar can never disagree.
pub fn settle_status(glossary: &[Term], p: &mut Project) {
    // An empty Working project is a video still being extracted.
    if matches!(p.status, Status::Failed | Status::Live) || p.segs.is_empty() {
        return;
    }
    let flagged = project_stats(glossary, p).flagged;
    if p.status == Status::Working {
        if flagged == 0 {
            p.status = Status::Done;
        }
        return;
    }
    p.status = if flagged > 0 {
        Status::Review
    } else {
        Status::Done
    };
}

// ─── corrections ───────────────────────────────────────────

/// Whether a suggested correction changes more than the spacing of a line.
pub fn is_real_fix(orig: &str, fixed: &str) -> bool {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    !fixed.trim().is_empty() && squash(orig) != squash(fixed)
}

/// What became of a piece of a line when it was corrected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Same,
    Gone,
    Added,
}

/// `old` and `new` compared word by word, or character by character in
/// Chinese, Japanese and Korean, as runs of text. Where a word was replaced,
/// the old one comes first.
pub fn word_diff(old: &str, new: &str) -> Vec<(Change, String)> {
    let (a, b) = (diff_tokens(old), diff_tokens(new));
    let (n, m) = (a.len(), b.len());
    if n * m > 250_000 {
        return vec![
            (Change::Gone, old.to_string()),
            (Change::Added, new.to_string()),
        ];
    }
    // lcs[i][j]: the longest common run of tokens in a[i..] and b[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out: Vec<(Change, String)> = Vec::new();
    let mut push = |kind: Change, text: &str| match out.last_mut() {
        Some((k, run)) if *k == kind => run.push_str(text),
        _ => out.push((kind, text.to_string())),
    };
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            push(Change::Same, a[i]);
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            push(Change::Gone, a[i]);
            i += 1;
        } else {
            push(Change::Added, b[j]);
            j += 1;
        }
    }
    out
}

/// Runs of letters and digits, and every other character on its own,
/// including each Chinese, Japanese or Korean character.
fn diff_tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut word: Option<usize> = None;
    for (i, c) in s.char_indices() {
        let in_word = c.is_alphanumeric() && !is_cjk_char(c);
        if in_word {
            word.get_or_insert(i);
            continue;
        }
        if let Some(start) = word.take() {
            out.push(&s[start..i]);
        }
        out.push(&s[i..i + c.len_utf8()]);
    }
    if let Some(start) = word {
        out.push(&s[start..]);
    }
    out
}

fn is_cjk_char(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xAC00..=0xD7AF | 0x20000..=0x2FA1F
    )
}

pub fn file_stem(name: &str) -> String {
    let mut out = String::new();
    let mut pending_sep = false;
    for ch in name.trim().chars() {
        let keep =
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '-' || ch as u32 >= 0xC0;
        if keep {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.push(ch);
        } else {
            pending_sep = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "subtitles".into()
    } else {
        trimmed
    }
}

// ─── live transcript tokens ────────────────────────────────

#[derive(Clone, Debug)]
pub struct Token {
    pub text: String,
    pub term: bool,
}

pub fn tokenize(s: &str, cjk: bool) -> Vec<Token> {
    let mut toks = Vec::new();
    let mut rest = s;
    loop {
        let (plain, after) = match rest.find("[[") {
            Some(i) => (&rest[..i], Some(&rest[i + 2..])),
            None => (rest, None),
        };
        if cjk {
            toks.extend(plain.chars().map(|c| Token {
                text: c.to_string(),
                term: false,
            }));
        } else {
            toks.extend(plain.split_whitespace().map(|w| Token {
                text: w.to_string(),
                term: false,
            }));
        }
        let Some(after) = after else { break };
        match after.find("]]") {
            Some(j) => {
                toks.push(Token {
                    text: after[..j].to_string(),
                    term: true,
                });
                rest = &after[j + 2..];
            }
            None => break,
        }
    }
    toks
}

#[derive(Clone, Debug)]
pub struct LiveLine {
    pub orig: Vec<Token>,
    pub orig_cjk: bool,
}

/// Which part of a rendered line a byte range belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    Term,
    Provisional,
    ProvisionalTerm,
}

/// The first `n` tokens, joined, plus the next token marked provisional.
pub fn join_tokens(
    toks: &[Token],
    n: usize,
    cjk: bool,
    provisional: bool,
) -> (String, Vec<(Range<usize>, Mark)>) {
    let sep = if cjk { "" } else { " " };
    let mut s = String::new();
    let mut marks = Vec::new();
    let take = n.min(toks.len());
    let mut push = |s: &mut String, t: &Token, prov: bool| {
        if !s.is_empty() {
            s.push_str(sep);
        }
        let start = s.len();
        s.push_str(&t.text);
        let mark = match (t.term, prov) {
            (true, true) => Some(Mark::ProvisionalTerm),
            (true, false) => Some(Mark::Term),
            (false, true) => Some(Mark::Provisional),
            _ => None,
        };
        if let Some(m) = mark {
            marks.push((start..s.len(), m));
        }
    };
    for t in &toks[..take] {
        push(&mut s, t, false);
    }
    if provisional && n < toks.len() {
        push(&mut s, &toks[n], true);
    }
    (s, marks)
}

// ─── export ────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Idx,
    Tc,
    A,
    B,
    Raw,
}

pub struct ExportOptions {
    pub format: Format,
    pub layout: Layout,
    pub wrap: usize,
    pub merge: bool,
    pub skip_flagged: bool,
    pub bom: bool,
    pub speakers: bool,
}

pub struct Built {
    pub text: String,
    pub count: usize,
    pub filename: String,
}

/// The wrap width owns line breaks on export, so authored breaks are reflowed.
pub fn wrap_line(text: &str, max: usize) -> Vec<String> {
    let t = text
        .split('\n')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    if t.is_empty() {
        return Vec::new();
    }
    if char_len(&t) <= max {
        return vec![t];
    }
    if !t.contains(' ') {
        let chars: Vec<char> = t.chars().collect();
        return chars.chunks(max).map(|c| c.iter().collect()).collect();
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for w in t.split(' ') {
        if cur.is_empty() {
            cur = w.to_string();
        } else if char_len(&cur) + 1 + char_len(w) <= max {
            cur.push(' ');
            cur.push_str(w);
        } else {
            out.push(std::mem::take(&mut cur));
            cur = w.to_string();
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

struct Cue {
    speaker: Option<u8>,
    in_ms: i64,
    out_ms: i64,
    orig: String,
    trans: String,
}

pub fn build_export(glossary: &[Term], p: &Project, o: &ExportOptions) -> Built {
    let mut cues: Vec<Cue> = Vec::new();
    for (i, s) in p.segs.iter().enumerate() {
        if o.skip_flagged && is_flagged(&flags_for(glossary, p, i)) {
            continue;
        }
        if o.merge {
            if let Some(last) = cues.last_mut() {
                if s.in_ms - last.out_ms < 120 && (!o.speakers || last.speaker == s.speaker) {
                    last.out_ms = s.out_ms;
                    last.trans = format!("{} {}", last.trans, s.trans).trim().to_string();
                    last.orig = format!("{} {}", last.orig, s.orig).trim().to_string();
                    continue;
                }
            }
        }
        cues.push(Cue {
            speaker: s.speaker,
            in_ms: s.in_ms,
            out_ms: s.out_ms,
            orig: s.orig.clone(),
            trans: s.trans.clone(),
        });
    }

    let groups = |c: &Cue| -> Vec<(Role, Vec<String>)> {
        let named = |t: &str| match c.speaker {
            Some(n) if o.speakers && !t.trim().is_empty() => format!("{}: {t}", p.speaker_name(n)),
            _ => t.to_string(),
        };
        let a = wrap_line(&named(&c.trans), o.wrap);
        let b = wrap_line(&named(&c.orig), o.wrap);
        match o.layout {
            Layout::Trans => vec![(Role::A, a)],
            Layout::Orig => vec![(Role::A, b)],
            Layout::OrigFirst => vec![(Role::A, b), (Role::B, a)],
            Layout::Stacked => vec![(Role::A, a), (Role::B, b)],
        }
    };

    let mut rows: Vec<Vec<(Role, String)>> = Vec::new();
    let one = |r: Role, t: &str| vec![(r, t.to_string())];
    match o.format {
        Format::Vtt => {
            rows.push(one(Role::Idx, "WEBVTT"));
            rows.push(one(Role::Raw, ""));
        }
        Format::Ass | Format::Video => {
            rows.push(one(Role::Idx, "[Script Info]"));
            for l in [
                "ScriptType: v4.00+",
                "PlayResX: 1920",
                "PlayResY: 1080",
                "WrapStyle: 0",
                "",
                "[V4+ Styles]",
                "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding",
                "Style: Default,PingFang SC,58,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,3,0,2,80,80,64,1",
                "",
            ] {
                rows.push(one(Role::Raw, l));
            }
            rows.push(one(Role::Idx, "[Events]"));
            rows.push(one(Role::Raw, "Format: Layer, Start, End, Style, Text"));
        }
        _ => {}
    }

    for (i, c) in cues.iter().enumerate() {
        let g = groups(c);
        match o.format {
            Format::Txt => {
                rows.push(one(Role::Tc, &format!("[{}]", mmss(c.in_ms))));
                for (r, lines) in &g {
                    for l in lines {
                        rows.push(one(*r, l));
                    }
                }
                rows.push(one(Role::Raw, ""));
            }
            Format::Ass | Format::Video => {
                let text = g
                    .iter()
                    .map(|(_, lines)| {
                        lines
                            .iter()
                            .map(|l| ass_escape(l))
                            .collect::<Vec<_>>()
                            .join("\\N")
                    })
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\\N");
                rows.push(vec![
                    (
                        Role::Tc,
                        format!(
                            "Dialogue: 0,{},{},Default,",
                            ass_time(c.in_ms),
                            ass_time(c.out_ms)
                        ),
                    ),
                    (Role::A, text),
                ]);
            }
            Format::Srt | Format::Vtt => {
                if o.format == Format::Srt {
                    rows.push(one(Role::Idx, &(i + 1).to_string()));
                }
                let tc = if o.format == Format::Vtt {
                    format!("{} --> {}", vtt_time(c.in_ms), vtt_time(c.out_ms))
                } else {
                    format!("{} --> {}", srt_time(c.in_ms), srt_time(c.out_ms))
                };
                rows.push(one(Role::Tc, &tc));
                for (r, lines) in &g {
                    for l in lines {
                        rows.push(one(*r, l));
                    }
                }
                rows.push(one(Role::Raw, ""));
            }
        }
    }

    let mut text = rows
        .iter()
        .map(|r| r.iter().map(|(_, t)| t.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    if o.bom && !matches!(o.format, Format::Txt | Format::Video) {
        text.insert(0, '\u{FEFF}');
    }
    let filename = if o.format == Format::Video {
        format!("{}.subtitled.mp4", file_stem(&p.name))
    } else {
        format!("{}.{}.{}", file_stem(&p.name), p.out_lang(), o.format.ext())
    };
    Built {
        text,
        count: cues.len(),
        filename,
    }
}

/// ASS override tags and backslashes are written as themselves, so a cue
/// that contains `{` stays text instead of becoming a style change.
fn ass_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\n' => out.push_str("\\N"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_skips_blanks_and_names_speakers() {
        let mut p = Project::new(
            "t".into(),
            Kind::Subtitle,
            "talk".into(),
            "talk.srt".into(),
            None,
            Status::Review,
            "en",
            "zh",
            vec![
                Segment::new(0, 1000, "Hello.", ""),
                Segment::new(1000, 2000, "  ", ""),
                Segment::new(2000, 3000, "Over here.", ""),
            ],
        );
        p.segs[0].speaker = Some(0);
        p.segs[2].speaker = Some(1);
        p.speakers = vec!["Ada".into()];
        assert_eq!(p.transcript(), "Ada: Hello.\nSpeaker 2: Over here.");
    }

    #[test]
    fn corrections_show_what_changed() {
        use Change::*;
        let runs = |old: &str, new: &str| -> Vec<(Change, String)> { word_diff(old, new) };
        assert_eq!(
            runs(
                "We spent less on service, which helps.",
                "We spent less on servers, which helps."
            ),
            vec![
                (Same, "We spent less on ".into()),
                (Gone, "service".into()),
                (Added, "servers".into()),
                (Same, ", which helps.".into()),
            ]
        );
        assert_eq!(
            runs("今天天汽很好", "今天天气很好"),
            vec![
                (Same, "今天天".into()),
                (Gone, "汽".into()),
                (Added, "气".into()),
                (Same, "很好".into())
            ]
        );
        assert_eq!(
            runs("same words", "same words"),
            vec![(Same, "same words".into())]
        );
        assert_eq!(runs("", "new"), vec![(Added, "new".into())]);

        assert!(is_real_fix("Java Script", "JavaScript"));
        assert!(!is_real_fix("a  b ", "a b"));
        assert!(!is_real_fix("words", "  "));

        let mut p = Project::new(
            "t".into(),
            Kind::Subtitle,
            "talk".into(),
            "talk.srt".into(),
            None,
            Status::Done,
            "en",
            "zh",
            vec![Segment::new(0, 3000, "Fine.", "")],
        );
        p.translate = false;
        assert!(!to_check(&flags_for(&[], &p, 0)));
        p.segs[0].fix = Some("Fine!".into());
        let flags = flags_for(&[], &p, 0);
        assert!(to_check(&flags) && !is_flagged(&flags));
        assert_eq!(project_stats(&[], &p).flagged, 1);
    }

    #[test]
    fn night_market_has_five_open_cues() {
        let g = fixture_glossary();
        let p = fixture_projects()
            .into_iter()
            .find(|p| p.id == "p2")
            .unwrap();
        let s = project_stats(&g, &p);
        assert_eq!(s.count, 14);
        assert_eq!(s.flagged, 5);
    }

    #[test]
    fn finished_tracks_are_clean() {
        let g = fixture_glossary();
        for p in fixture_projects()
            .iter()
            .filter(|p| p.status == Status::Done)
        {
            assert_eq!(project_stats(&g, p).flagged, 0, "{}", p.name);
        }
    }

    #[test]
    fn tokenize_marks_terms() {
        let t = tokenize("we cut [[end-to-end latency]] fast", false);
        assert_eq!(t.len(), 4);
        assert!(t[2].term);
        let z = tokenize("把[[流式推理]]用上", true);
        assert_eq!(z.iter().filter(|t| t.term).count(), 1);
        assert_eq!(z.len(), 4);
    }

    #[test]
    fn wraps_by_words_and_chars() {
        assert_eq!(wrap_line("a b c", 3), vec!["a b", "c"]);
        assert_eq!(wrap_line("一二三四五", 2), vec!["一二", "三四", "五"]);
    }

    #[test]
    fn srt_export_shape() {
        let g = fixture_glossary();
        let p = fixture_projects()
            .into_iter()
            .find(|p| p.id == "p1")
            .unwrap();
        let b = build_export(
            &g,
            &p,
            &ExportOptions {
                format: Format::Srt,
                layout: Layout::Stacked,
                wrap: 42,
                merge: false,
                skip_flagged: false,
                bom: false,
                speakers: false,
            },
        );
        assert_eq!(b.count, 10);
        assert!(b.text.starts_with("1\n00:00:04,200 --> "));
        assert_eq!(b.filename, "Streaming-at-Scale-closing-keynote.zh.srt");
    }

    #[test]
    fn video_export_is_a_burnable_script() {
        let mut p = Project::new(
            "t".into(),
            Kind::Video,
            "Talk".into(),
            "talk.mp4".into(),
            Some("talk.mp4".into()),
            Status::Done,
            "en",
            "zh",
            vec![Segment::new(0, 1500, "a {b} c\\d", "你好")],
        );
        p.translate = true;
        let b = build_export(
            &[],
            &p,
            &ExportOptions {
                format: Format::Video,
                layout: Layout::Stacked,
                wrap: 42,
                merge: false,
                skip_flagged: false,
                bom: true,
                speakers: false,
            },
        );
        assert_eq!(b.count, 1);
        assert_eq!(b.filename, "Talk.subtitled.mp4");
        assert!(b.text.starts_with("[Script Info]"));
        assert!(!b.text.starts_with('\u{FEFF}'));
        assert!(b.text.contains("PingFang SC"));
        assert!(
            b.text
                .contains(r"Dialogue: 0,0:00:00.00,0:00:01.50,Default,你好\Na \{b\} c\\d")
        );
    }

    #[test]
    fn untranslated_lines_are_checked_and_exported_as_spoken() {
        let g = fixture_glossary();
        let mut p = fixture_projects()
            .into_iter()
            .find(|p| p.id == "p1")
            .unwrap();
        p.translate = false;
        for s in &mut p.segs {
            s.trans.clear();
        }
        p.segs[0].speaker = Some(0);
        p.speakers = vec!["Ana".into()];
        assert!(
            flags_for(&g, &p, 0)
                .iter()
                .all(|f| f.kind != FlagKind::Empty)
        );
        let b = build_export(
            &g,
            &p,
            &ExportOptions {
                format: Format::Srt,
                layout: Layout::Orig,
                wrap: 200,
                merge: false,
                skip_flagged: false,
                bom: false,
                speakers: true,
            },
        );
        assert!(
            b.text.lines().nth(2).unwrap().starts_with("Ana: "),
            "{}",
            b.text
        );
        assert!(b.filename.ends_with(".en.srt"), "{}", b.filename);
    }

    #[test]
    fn forbid_rule_is_corrected() {
        let g = fixture_glossary();
        let (out, n) = apply_rules(&g, "two yuan of spicy sauce", Some("p2"));
        assert_eq!(n, 1);
        assert_eq!(out, "two yuan of chilli sauce");
    }
}
