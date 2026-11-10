//! Translation, summaries and corrections through any OpenAI-compatible
//! `/chat/completions` endpoint: OpenAI, OpenRouter, vLLM, LM Studio or a
//! local Ollama.

use std::io::{BufRead, BufReader};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{agent, check, untimed_agent};
use crate::data::{Rule, Term, lang_name};

#[derive(Clone, Debug)]
pub struct MtConfig {
    pub url: String,
    pub model: String,
    pub key: String,
}

pub const BATCH: usize = 24;

/// How much a summary or a correction may reason before it answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Think {
    /// The reply is the answer itself.
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl Think {
    pub const ALL: [Think; 4] = [Think::Off, Think::Low, Think::Medium, Think::High];

    pub fn label(self) -> &'static str {
        match self {
            Think::Off => "Off",
            Think::Low => "Low",
            Think::Medium => "Medium",
            Think::High => "High",
        }
    }

    pub fn note(self) -> &'static str {
        match self {
            Think::Off => "Answers directly, without a reasoning pass.",
            Think::Low => "A brief check, then the answer.",
            Think::Medium => "Reasons where it helps, then answers.",
            Think::High => "A careful reasoning pass, then the answer.",
        }
    }

    /// Qwen3's chat template reads this at the start of the user message.
    fn lead(self) -> &'static str {
        match self {
            Think::Off => "/no_think\n",
            _ => "/think\n",
        }
    }

    fn pace(self) -> &'static str {
        match self {
            Think::Off => "Answer directly. Do not reason, explain or write a think block.",
            Think::Low => "Check briefly, then answer. Keep any reasoning short.",
            Think::Medium => "Reason where something is unclear, then answer.",
            Think::High => "Reason carefully, then answer.",
        }
    }

    fn api_name(self) -> Option<&'static str> {
        match self {
            Think::Off => None,
            Think::Low => Some("low"),
            Think::Medium => Some("medium"),
            Think::High => Some("high"),
        }
    }
}

/// Lines per correction request. The reply lists only lines that change, so a
/// batch can be long without the model rewriting every line.
pub const CORRECT_BATCH: usize = 32;

/// The glossary entries that apply to one project, written as instructions.
pub fn glossary_rules(terms: &[&Term]) -> Vec<String> {
    terms
        .iter()
        .filter_map(|g| match g.rule {
            Rule::Prefer if !g.target.is_empty() => {
                Some(format!("Translate \"{}\" as \"{}\".", g.term, g.target))
            }
            Rule::Keep => Some(format!(
                "Keep \"{}\" exactly as written; never translate it.",
                g.term
            )),
            Rule::Forbid if !g.target.is_empty() => Some(format!(
                "Never translate \"{}\" as \"{}\".",
                g.term, g.target
            )),
            _ => None,
        })
        .collect()
}

/// Translates `lines` in order. `before` is earlier source text, sent as
/// context only, so a batch reads as part of the whole scene.
pub fn translate(
    cfg: &MtConfig,
    from: &str,
    to: &str,
    rules: &[String],
    before: &[String],
    lines: &[String],
) -> Result<Vec<String>> {
    if lines.is_empty() {
        return Ok(Vec::new());
    }
    match request(cfg, from, to, rules, before, lines) {
        Ok(out) => Ok(out),
        // Smaller models sometimes merge or drop lines in a batch; one line at
        // a time is slower but always lines up.
        Err(_) if lines.len() > 1 => lines
            .iter()
            .map(|l| {
                request(cfg, from, to, rules, before, std::slice::from_ref(l))
                    .map(|mut v| v.remove(0))
            })
            .collect(),
        Err(e) => Err(e),
    }
}

fn request(
    cfg: &MtConfig,
    from: &str,
    to: &str,
    rules: &[String],
    before: &[String],
    lines: &[String],
) -> Result<Vec<String>> {
    let mut system = format!(
        "You are a professional subtitle translator. Translate subtitle lines from {} to {}.\n\
         - Reply with JSON only, shaped {{\"t\": [\"...\"]}}, holding exactly {} strings in the same order as the input lines.\n\
         - Never merge, split, skip or reorder lines. Each output string translates the line at the same position.\n\
         - Keep it natural and short enough to read on screen. Keep a line break only where the source has one.\n\
         - Do not explain, transliterate or add notes.",
        lang_name(from),
        lang_name(to),
        lines.len()
    );
    if !rules.is_empty() {
        system.push_str("\nGlossary, which you must follow:\n");
        for r in rules {
            system.push_str("- ");
            system.push_str(r);
            system.push('\n');
        }
    }
    let mut user = json!({ "lines": lines });
    if !before.is_empty() {
        user["context_before"] = json!(before);
    }
    let body = json!({
        "model": cfg.model,
        "temperature": 0.2,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user.to_string() },
        ],
    });
    let content = chat(cfg, &body)?;
    let out = parse_lines(&content, lines.len())?;
    Ok(out)
}

fn chat(cfg: &MtConfig, body: &Value) -> Result<String> {
    let mut resp = post(agent(Duration::from_secs(180)), cfg, body)?;
    let v: Value = resp.body_mut().read_json().context("reading the reply")?;
    reply_text(&v)
}

/// Streams the reply, so it needs no deadline and can still be stopped:
/// dropping the stream closes the connection, which stops the server too.
fn chat_streamed(cfg: &MtConfig, body: Value, cancel: &AtomicBool) -> Result<String> {
    chat_with(untimed_agent(), cfg, body, cancel)
}

/// A correction request. The body has a deadline, and a read that goes quiet
/// gives up, so one batch cannot hold the count still forever.
fn chat_bounded(
    cfg: &MtConfig,
    body: Value,
    cancel: &AtomicBool,
    budget: Duration,
) -> Result<String> {
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(budget))
        .timeout_per_read(Some(Duration::from_secs(90)))
        .timeout_recv_body(Some(budget))
        .http_status_as_error(false)
        .build()
        .into();
    chat_with(agent, cfg, body, cancel)
}

fn chat_with(
    agent: ureq::Agent,
    cfg: &MtConfig,
    mut body: Value,
    cancel: &AtomicBool,
) -> Result<String> {
    body["stream"] = json!(true);
    let mut resp = post(agent, cfg, &body)?;
    read_reply(BufReader::new(resp.body_mut().as_reader()), cancel)
}

fn post(
    agent: ureq::Agent,
    cfg: &MtConfig,
    body: &Value,
) -> Result<ureq::http::Response<ureq::Body>> {
    let mut req = agent.post(format!("{}/chat/completions", cfg.url));
    if !cfg.key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", cfg.key));
    }
    let resp = req
        .send_json(body)
        .with_context(|| format!("contacting {}", cfg.url))?;
    check(resp)
}

fn reply_text(v: &Value) -> Result<String> {
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .context("the reply had no message")?;
    Ok(strip_thinking(content))
}

/// The text of a streamed reply: server-sent events carrying pieces of the
/// message, or the whole reply at once from a server that does not stream.
fn read_reply(reader: impl BufRead, cancel: &AtomicBool) -> Result<String> {
    let mut text = String::new();
    let mut whole = String::new();
    let mut streamed = false;
    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let line = line.context("reading the reply")?;
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            whole.push_str(&line);
            whole.push('\n');
            continue;
        };
        streamed = true;
        if data == "[DONE]" {
            break;
        }
        let v: Value = serde_json::from_str(data).context("reading the reply")?;
        if let Some(msg) = v["error"]["message"].as_str().or(v["error"].as_str()) {
            bail!("{msg}");
        }
        if let Some(piece) = v["choices"][0]["delta"]["content"].as_str() {
            text.push_str(piece);
        }
    }
    if !streamed {
        return reply_text(&serde_json::from_str(&whole).context("reading the reply")?);
    }
    Ok(strip_thinking(&text))
}

/// Reasoning models (Qwen3, DeepSeek-R1) prefix their answer with a thought block.
fn strip_thinking(s: &str) -> String {
    match s.rfind("</think>") {
        Some(i) => s[i + 8..].trim().to_string(),
        None => s.trim().to_string(),
    }
}

/// Like [`json_lines`], but a single line may also come back as bare text.
fn parse_lines(content: &str, n: usize) -> Result<Vec<String>> {
    if n == 1 && serde_json::from_str::<Value>(json_part(content)).is_err() {
        return Ok(vec![content.trim().trim_matches('"').to_string()]);
    }
    json_lines(content, n)
}

/// The braces of a reply, without any prose or code fence around them.
fn json_part(content: &str) -> &str {
    match (content.find('{'), content.rfind('}')) {
        (Some(a), Some(b)) if b > a => &content[a..=b],
        _ => content,
    }
}

/// Exactly `n` strings from a reply shaped `{"t": [...]}`.
fn json_lines(content: &str, n: usize) -> Result<Vec<String>> {
    let v: Value = serde_json::from_str(json_part(content))
        .map_err(|e| anyhow::anyhow!("the reply was not the expected JSON ({e})"))?;
    let list: Vec<String> = v["t"]
        .as_array()
        .or_else(|| v["lines"].as_array())
        .or_else(|| v["translations"].as_array())
        .or_else(|| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|x| x.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    if list.len() != n {
        bail!("asked for {n} lines and got {}", list.len());
    }
    Ok(list
        .into_iter()
        .map(|s| s.replace("\\n", "\n").trim().to_string())
        .collect())
}

/// Neighbouring lines sent along with a batch, so it reads in context.
const CONTEXT_LINES: usize = 3;

/// Fixes what a speech engine misheard in `lines[range]`, spoken in `lang` (a
/// language name). The lines around the range go along as context. `about`
/// says what the recording is about, and `terms` are names and words spelled
/// the right way. Every line comes back, unchanged when nothing was wrong.
///
/// `on_span` is called as soon as a piece is settled, including when a batch
/// had to be split, so the count can move before the whole range returns.
/// `plain` is set once a server rejects a vendor field, and later requests
/// use a plain OpenAI body.
pub fn correct(
    cfg: &MtConfig,
    lang: &str,
    about: &str,
    terms: &[String],
    lines: &[String],
    range: Range<usize>,
    think: Think,
    cancel: &AtomicBool,
    plain: &AtomicBool,
    on_span: &mut dyn FnMut(usize, &[String]),
) -> Result<Vec<String>> {
    if range.is_empty() {
        return Ok(Vec::new());
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    let n = range.len();
    let pace = think.pace();
    let mut system = format!(
        "You proofread a transcript made by automatic speech recognition. It is in {lang}, split into subtitle lines.\n\
         - Fix only what the recognizer misheard: wrong words that sound like the right ones, misspelled names and terms, wrong characters.\n\
         - Keep everything else exactly as it is: the wording, the word order, filler words, line breaks and punctuation.\n\
         - Never translate, rephrase, summarize, merge, split or reorder lines.\n\
         - {pace}\n\
         - Reply with JSON only, shaped {{\"f\":[{{\"i\":0,\"t\":\"the corrected line\"}}]}}.\n\
         - i is the index in lines, starting at 0. Include a line only when you change it.\n\
         - When every line is already right, reply {{\"f\":[]}}."
    );
    if !about.trim().is_empty() {
        system.push_str("\nThe recording is about: ");
        system.push_str(about.trim());
    }
    if !terms.is_empty() {
        system.push_str("\nThese names and terms are spelled correctly; use them where the transcript means them:\n");
        for t in terms {
            system.push_str("- ");
            system.push_str(t);
            system.push('\n');
        }
    }
    let user = json!({
        "lines": &lines[range.clone()],
        "context_before": &lines[range.start.saturating_sub(CONTEXT_LINES)..range.start],
        "context_after": &lines[range.end..(range.end + CONTEXT_LINES).min(lines.len())],
    });
    // `/no_think` and `/think` are read by Qwen3's chat template. Other models
    // see the word as a note and are told to ignore it.
    let user = format!("{}{user}", think.lead());
    let mut body = json!({
        "model": cfg.model,
        "temperature": 0,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
    });
    // Off stops after the corrected lines. A reasoning level needs the room
    // to think before the JSON, so it has no token cap.
    if think == Think::Off {
        let max_tokens = (n as u64 * 80).clamp(256, 4096);
        body["max_tokens"] = json!(max_tokens);
    }
    let fetched = deliver(
        cfg,
        body,
        cancel,
        plain,
        think,
        Some(correct_budget(think, n)),
    );
    let content = match fetched {
        Ok(content) => content,
        Err(e) if cancel.load(Ordering::Relaxed) => return Err(e),
        Err(e) if n > 1 && slow_reply(&e) => {
            return correct_halves(
                cfg, lang, about, terms, lines, range, think, cancel, plain, on_span,
            );
        }
        Err(e) if n == 1 && slow_reply(&e) => {
            let out = lines[range.clone()].to_vec();
            on_span(range.start, &out);
            return Ok(out);
        }
        Err(e) => return Err(e),
    };
    match corrected_lines(&content, &lines[range.clone()]) {
        Ok(out) => {
            on_span(range.start, &out);
            Ok(out)
        }
        // Smaller models sometimes merge or drop a line; halves line up more often.
        Err(_) if n > 1 => correct_halves(
            cfg, lang, about, terms, lines, range, think, cancel, plain, on_span,
        ),
        Err(_) => {
            let out = lines[range.clone()].to_vec();
            on_span(range.start, &out);
            Ok(out)
        }
    }
}

fn correct_halves(
    cfg: &MtConfig,
    lang: &str,
    about: &str,
    terms: &[String],
    lines: &[String],
    range: Range<usize>,
    think: Think,
    cancel: &AtomicBool,
    plain: &AtomicBool,
    on_span: &mut dyn FnMut(usize, &[String]),
) -> Result<Vec<String>> {
    let mid = range.start + range.len() / 2;
    let mut out = correct(
        cfg,
        lang,
        about,
        terms,
        lines,
        range.start..mid,
        think,
        cancel,
        plain,
        on_span,
    )?;
    out.extend(correct(
        cfg,
        lang,
        about,
        terms,
        lines,
        mid..range.end,
        think,
        cancel,
        plain,
        on_span,
    )?);
    Ok(out)
}

/// Drop fields a strict server rejects. Ollama accepts them and uses them to
/// turn reasoning off.
fn strip_vendor(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.remove("think");
        obj.remove("chat_template_kwargs");
        obj.remove("reasoning_effort");
        obj.remove("max_tokens");
    }
}

/// Asks the server for `think`. A level is a string where the server supports
/// one, and a boolean where it only knows on or off.
fn apply_think(body: &mut Value, think: Think) {
    match think.api_name() {
        None => {
            body["think"] = json!(false);
            body["chat_template_kwargs"] = json!({ "enable_thinking": false });
            body.as_object_mut().unwrap().remove("reasoning_effort");
        }
        Some(level) => {
            body["think"] = json!(level);
            body["reasoning_effort"] = json!(level);
            body["chat_template_kwargs"] = json!({ "enable_thinking": true });
        }
    }
}

fn apply_think_on(body: &mut Value) {
    strip_vendor(body);
    body["think"] = json!(true);
    body["chat_template_kwargs"] = json!({ "enable_thinking": true });
}

/// Sends `body` with the thinking fields. A server that rejects them is
/// retried with thinking simply on, then with a plain body. `plain` remembers
/// that last outcome for the rest of the job. `budget` is the correction
/// deadline; a summary has none.
fn deliver(
    cfg: &MtConfig,
    mut body: Value,
    cancel: &AtomicBool,
    plain: &AtomicBool,
    think: Think,
    budget: Option<Duration>,
) -> Result<String> {
    let send = |body: Value| match budget {
        Some(limit) => chat_bounded(cfg, body, cancel, limit),
        None => chat_streamed(cfg, body, cancel),
    };
    if plain.load(Ordering::Relaxed) {
        strip_vendor(&mut body);
        return send(body);
    }
    apply_think(&mut body, think);
    match send(body.clone()) {
        Err(e) if rejected_body(&e) && think != Think::Off => {
            apply_think_on(&mut body);
            match send(body.clone()) {
                Err(e) if rejected_body(&e) => {
                    plain.store(true, Ordering::Relaxed);
                    strip_vendor(&mut body);
                    send(body)
                }
                other => other,
            }
        }
        Err(e) if rejected_body(&e) => {
            plain.store(true, Ordering::Relaxed);
            strip_vendor(&mut body);
            send(body)
        }
        other => other,
    }
}

fn correct_budget(think: Think, n: usize) -> Duration {
    let n = n as u64;
    let secs = match think {
        Think::Off => 40 + n * 3,
        Think::Low => 50 + n * 6,
        Think::Medium => 70 + n * 10,
        Think::High => 90 + n * 16,
    };
    Duration::from_secs(secs)
}

/// The server refused the request body, usually an unknown field.
fn rejected_body(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("400")
}

/// Lines after proofreading. A reply shaped `{"f":[{"i","t"}]}` lists only the
/// lines that changed; a full `{"t":[...]}` of the right length is accepted too.
fn corrected_lines(content: &str, originals: &[String]) -> Result<Vec<String>> {
    let v: Value = serde_json::from_str(json_part(content))
        .map_err(|e| anyhow::anyhow!("the reply was not the expected JSON ({e})"))?;
    if let Some(arr) = v.get("t").and_then(Value::as_array) {
        if arr.len() == originals.len() {
            return json_lines(content, originals.len());
        }
        bail!("asked for {} lines and got {}", originals.len(), arr.len());
    }
    let Some(fixes) = v.get("f").and_then(Value::as_array) else {
        bail!("the reply was not the expected JSON");
    };
    let mut out = originals.to_vec();
    for item in fixes {
        let Some(i) = item.get("i").and_then(json_index) else {
            continue;
        };
        let Some(text) = item.get("t").and_then(Value::as_str) else {
            continue;
        };
        let text = text.replace("\\n", "\n");
        let text = text.trim();
        if i < out.len() && !text.is_empty() {
            out[i] = text.to_string();
        }
    }
    Ok(out)
}

fn json_index(v: &Value) -> Option<usize> {
    v.as_u64()
        .map(|n| n as usize)
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// The reply stalled or arrived incomplete. A smaller piece may still land.
/// A server that never answered is not this: that error stops the job.
fn slow_reply(err: &anyhow::Error) -> bool {
    let s = format!("{err:#}").to_ascii_lowercase();
    s.contains("timeout") || s.contains("reading the reply")
}

/// How much of a transcript one request should carry. Longer transcripts are
/// summarized in parts, then those parts are folded into one account.
const CHUNK_CHARS: usize = 6_000;

/// Model ids from an OpenAI-style `/models` reply, or Ollama's native list.
pub fn parse_model_ids(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(arr) = v["data"].as_array().or_else(|| v["models"].as_array()) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for item in arr {
        let id = item["id"]
            .as_str()
            .or_else(|| item["name"].as_str())
            .or_else(|| item["model"].as_str())
            .unwrap_or("")
            .trim();
        if !id.is_empty() && !ids.iter().any(|e: &String| e == id) {
            ids.push(id.to_string());
        }
    }
    ids.sort();
    ids
}

/// Asks the endpoint which chat models it has.
pub fn list_models(cfg: &MtConfig) -> Result<Vec<String>> {
    let mut req = agent(Duration::from_secs(8)).get(format!("{}/models", cfg.url));
    if !cfg.key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", cfg.key));
    }
    let resp = req
        .call()
        .with_context(|| format!("contacting {}", cfg.url))?;
    let mut resp = check(resp)?;
    let body = resp
        .body_mut()
        .read_to_string()
        .context("reading the model list")?;
    let ids = parse_model_ids(&body);
    if ids.is_empty() {
        bail!("the endpoint did not list any models");
    }
    Ok(ids)
}

/// A prose account of a transcript, written in `lang` (a language name).
/// `long` asks for a few paragraphs instead of a few sentences. A transcript
/// too long for one request is summarized in parts, then the parts are
/// joined; `progress` hears how many parts are done, out of how many.
pub fn summarize(
    cfg: &MtConfig,
    lang: &str,
    long: bool,
    rules: &[String],
    transcript: &str,
    think: Think,
    progress: impl Fn(usize, usize),
    cancel: &AtomicBool,
) -> Result<String> {
    let transcript = transcript.trim();
    if transcript.is_empty() {
        bail!("nothing to summarize");
    }
    let plain = AtomicBool::new(false);
    let parts = split_transcript(transcript, CHUNK_CHARS);
    if parts.len() <= 1 {
        return one_summary(
            cfg, lang, long, rules, transcript, None, think, &plain, cancel,
        );
    }
    let mut partials = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        progress(i, parts.len());
        let note = format!(
            "This is part {} of {} of one transcript. Summarize only this part.",
            i + 1,
            parts.len()
        );
        partials.push(one_summary(
            cfg,
            lang,
            long,
            rules,
            part,
            Some(&note),
            think,
            &plain,
            cancel,
        )?);
    }
    progress(parts.len(), parts.len());
    let fold = if long {
        "These are summaries of consecutive parts of one transcript. Combine them into one fuller account, without repeating yourself."
    } else {
        "These are summaries of consecutive parts of one transcript. Combine them into one short summary, without repeating yourself."
    };
    one_summary(
        cfg,
        lang,
        long,
        rules,
        &partials.join("\n\n"),
        Some(fold),
        think,
        &plain,
        cancel,
    )
}

fn one_summary(
    cfg: &MtConfig,
    lang: &str,
    long: bool,
    rules: &[String],
    text: &str,
    note: Option<&str>,
    think: Think,
    plain: &AtomicBool,
    cancel: &AtomicBool,
) -> Result<String> {
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    let shape = if long {
        "Several paragraphs of plain prose: what happens, who is involved, and the points that matter, in the order they come up."
    } else {
        "A few sentences of plain prose: what happens, who is involved, and the points that matter."
    };
    let pace = think.pace();
    let mut system = format!(
        "You summarize a spoken transcript for someone who has not heard it.\n\
         Write in {lang}.\n\
         - {shape}\n\
         - {pace}\n\
         - Use only what the transcript says. Do not invent scenes, names or motives.\n\
         - No title, headings, bullets or preamble. Reply with the summary only.\n\
         - Ignore a leading /think or /no_think; it is an instruction, not part of the transcript."
    );
    if let Some(note) = note {
        system.push('\n');
        system.push_str(note);
    }
    if !rules.is_empty() {
        system.push_str("\nWhen you name things, follow these:\n");
        for r in rules {
            system.push_str("- ");
            system.push_str(r);
            system.push('\n');
        }
    }
    let body = json!({
        "model": cfg.model,
        "temperature": 0.3,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": format!("{}{text}", think.lead()) },
        ],
    });
    let content = deliver(cfg, body, cancel, plain, think, None)?;
    let prose = plain_prose(&content);
    if prose.is_empty() {
        bail!("the reply was empty");
    }
    Ok(prose)
}

fn plain_prose(s: &str) -> String {
    let mut s = s.trim();
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
        s = rest.trim().trim_end_matches("```").trim();
    }
    for prefix in ["/no_think", "/think"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim();
            break;
        }
    }
    for prefix in ["Summary:", "Summary：", "摘要:", "摘要："] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim();
            break;
        }
    }
    s.to_string()
}

/// Splits on line breaks so each piece stays near `max_chars`.
fn split_transcript(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut n = 0usize;
    for line in text.split_inclusive('\n') {
        let len = line.chars().count();
        if n > 0 && n + len > max_chars {
            out.push(std::mem::take(&mut buf));
            n = 0;
        }
        buf.push_str(line);
        n += len;
    }
    if !buf.trim().is_empty() {
        out.push(buf);
    }
    out
}

/// A one-line round trip, so the settings dialog can prove the endpoint works.
pub fn test(cfg: &MtConfig, to: &str) -> Result<String> {
    let out = translate(
        cfg,
        "en",
        to,
        &[],
        &[],
        &["Good evening, and welcome back.".to_string()],
    )?;
    Ok(out.into_iter().next().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replies() {
        assert_eq!(
            parse_lines("{\"t\": [\"a\", \"b\"]}", 2).unwrap(),
            vec!["a", "b"]
        );
        assert_eq!(
            parse_lines("<think>hmm</think>\n```json\n{\"t\":[\"x\"]}\n```", 1).unwrap(),
            vec!["x"]
        );
        assert!(parse_lines("{\"t\": [\"a\"]}", 2).is_err());
        assert_eq!(
            parse_lines("Just the line.", 1).unwrap(),
            vec!["Just the line."]
        );
        assert!(json_lines("Just the line.", 1).is_err());
        assert_eq!(
            json_lines("Sure: {\"t\": [\"a\\\\nb\"]}", 1).unwrap(),
            vec!["a\nb"]
        );
        assert_eq!(strip_thinking("<think>a</think> ok"), "ok");
        let lines = vec!["service".into(), "fine".into(), "天汽".into()];
        assert_eq!(
            corrected_lines(
                r#"{"f":[{"i":0,"t":"servers"},{"i":2,"t":"天气"}]}"#,
                &lines
            )
            .unwrap(),
            vec!["servers", "fine", "天气"]
        );
        assert_eq!(corrected_lines(r#"{"f":[]}"#, &lines).unwrap(), lines);
        assert_eq!(
            corrected_lines(
                r#"{"f":[{"i":"1","t":"ok"},{"i":9,"t":"no"},{"i":0,"t":""}]}"#,
                &lines
            )
            .unwrap(),
            vec!["service", "ok", "天汽"]
        );
        assert_eq!(
            corrected_lines(r#"{"t":["a","b","c"]}"#, &lines).unwrap(),
            vec!["a", "b", "c"]
        );
        assert!(corrected_lines(r#"{"t":["only one"]}"#, &lines).is_err());

        let mut high = json!({});
        apply_think(&mut high, Think::High);
        assert_eq!(high["think"], json!("high"));
        assert_eq!(high["reasoning_effort"], json!("high"));
        assert_eq!(high["chat_template_kwargs"]["enable_thinking"], json!(true));

        let mut off = json!({});
        apply_think(&mut off, Think::Off);
        assert_eq!(off["think"], json!(false));
        assert_eq!(off["chat_template_kwargs"]["enable_thinking"], json!(false));
        assert!(off.get("reasoning_effort").is_none());
        assert_eq!(plain_prose("```markdown\nSummary: Hello.\n```"), "Hello.");
        assert_eq!(plain_prose("摘要：晚饭谈完了。"), "晚饭谈完了。");
        let long = "one\n".repeat(20);
        let parts = split_transcript(&long, 12);
        assert!(parts.len() > 1);
        assert_eq!(parts.join(""), long);
        assert!(split_transcript("short", 100).len() == 1);
        assert_eq!(
            parse_model_ids(r#"{"data":[{"id":"qwen3:8b"},{"id":"llama3"},{"id":"qwen3:8b"}]}"#),
            vec!["llama3", "qwen3:8b"]
        );
        assert_eq!(
            parse_model_ids(r#"{"models":[{"name":"mistral"}]}"#),
            vec!["mistral"]
        );
    }

    #[test]
    fn reads_streamed_replies() {
        let go = AtomicBool::new(false);
        let sse = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
                   : keep-alive\n\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"<think>hmm</think>Hel\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"reasoning\":\"an aside\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"lo.\"},\"finish_reason\":\"stop\"}]}\n\n\
                   data: [DONE]\n\n";
        assert_eq!(read_reply(sse.as_bytes(), &go).unwrap(), "Hello.");
        let whole = "{\"choices\":[{\"message\":{\"content\":\"In one piece.\"}}]}";
        assert_eq!(read_reply(whole.as_bytes(), &go).unwrap(), "In one piece.");
        let failed = "data: {\"error\":{\"message\":\"model not found\"}}\n\n";
        assert_eq!(
            read_reply(failed.as_bytes(), &go).unwrap_err().to_string(),
            "model not found"
        );
        assert!(read_reply(sse.as_bytes(), &AtomicBool::new(true)).is_err());
    }
}
