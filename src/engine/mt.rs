//! Translation through any OpenAI-compatible `/chat/completions` endpoint:
//! OpenAI, OpenRouter, vLLM, LM Studio or a local Ollama.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use super::{agent, check};
use crate::data::{Rule, Term, lang_name};

#[derive(Clone, Debug)]
pub struct MtConfig {
    pub url: String,
    pub model: String,
    pub key: String,
}

pub const BATCH: usize = 24;

/// The glossary entries that apply to one project, written as instructions.
pub fn glossary_rules(terms: &[&Term]) -> Vec<String> {
    terms
        .iter()
        .filter_map(|g| match g.rule {
            Rule::Prefer if !g.target.is_empty() => {
                Some(format!("Translate \"{}\" as \"{}\".", g.term, g.target))
            }
            Rule::Keep => Some(format!("Keep \"{}\" exactly as written; never translate it.", g.term)),
            Rule::Forbid if !g.target.is_empty() => {
                Some(format!("Never translate \"{}\" as \"{}\".", g.term, g.target))
            }
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
            .map(|l| request(cfg, from, to, rules, before, std::slice::from_ref(l)).map(|mut v| v.remove(0)))
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
    let mut req = agent(Duration::from_secs(180)).post(format!("{}/chat/completions", cfg.url));
    if !cfg.key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", cfg.key));
    }
    let resp = req.send_json(body).with_context(|| format!("contacting {}", cfg.url))?;
    let mut resp = check(resp)?;
    let v: Value = resp.body_mut().read_json().context("reading the reply")?;
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .context("the reply had no message")?;
    Ok(strip_thinking(content))
}

/// Reasoning models (Qwen3, DeepSeek-R1) prefix their answer with a thought block.
fn strip_thinking(s: &str) -> String {
    match s.rfind("</think>") {
        Some(i) => s[i + 8..].trim().to_string(),
        None => s.trim().to_string(),
    }
}

fn parse_lines(content: &str, n: usize) -> Result<Vec<String>> {
    let json_text = match (content.find('{'), content.rfind('}')) {
        (Some(a), Some(b)) if b > a => &content[a..=b],
        _ => content,
    };
    let list: Vec<String> = match serde_json::from_str::<Value>(json_text) {
        Ok(v) => {
            let arr = v["t"]
                .as_array()
                .or_else(|| v["lines"].as_array())
                .or_else(|| v["translations"].as_array())
                .or_else(|| v.as_array())
                .cloned()
                .unwrap_or_default();
            arr.iter().map(|x| x.as_str().unwrap_or_default().to_string()).collect()
        }
        Err(_) if n == 1 => vec![content.trim().trim_matches('"').to_string()],
        Err(e) => bail!("the reply was not the expected JSON ({e})"),
    };
    if list.len() != n {
        bail!("asked for {n} lines and got {}", list.len());
    }
    Ok(list.into_iter().map(|s| s.replace("\\n", "\n").trim().to_string()).collect())
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
    let resp = req.call().with_context(|| format!("contacting {}", cfg.url))?;
    let mut resp = check(resp)?;
    let body = resp.body_mut().read_to_string().context("reading the model list")?;
    let ids = parse_model_ids(&body);
    if ids.is_empty() {
        bail!("the endpoint did not list any models");
    }
    Ok(ids)
}

/// A prose account of a transcript, written in `lang` (a language name).
/// `long` asks for a few paragraphs instead of a few sentences.
pub fn summarize(
    cfg: &MtConfig,
    lang: &str,
    long: bool,
    rules: &[String],
    transcript: &str,
    cancel: &AtomicBool,
) -> Result<String> {
    let transcript = transcript.trim();
    if transcript.is_empty() {
        bail!("nothing to summarize");
    }
    let parts = split_transcript(transcript, CHUNK_CHARS);
    if parts.len() <= 1 {
        return one_summary(cfg, lang, long, rules, transcript, None, cancel);
    }
    let mut partials = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let note = format!(
            "This is part {} of {} of one transcript. Summarize only this part.",
            i + 1,
            parts.len()
        );
        partials.push(one_summary(cfg, lang, long, rules, part, Some(&note), cancel)?);
    }
    let fold = if long {
        "These are summaries of consecutive parts of one transcript. Combine them into one fuller account, without repeating yourself."
    } else {
        "These are summaries of consecutive parts of one transcript. Combine them into one short summary, without repeating yourself."
    };
    one_summary(cfg, lang, long, rules, &partials.join("\n\n"), Some(fold), cancel)
}

fn one_summary(
    cfg: &MtConfig,
    lang: &str,
    long: bool,
    rules: &[String],
    text: &str,
    note: Option<&str>,
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
    let mut system = format!(
        "You summarize a spoken transcript for someone who has not heard it.\n\
         Write in {lang}.\n\
         - {shape}\n\
         - Use only what the transcript says. Do not invent scenes, names or motives.\n\
         - No title, headings, bullets or preamble. Reply with the summary only."
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
            { "role": "user", "content": text },
        ],
    });
    let content = chat(cfg, &body)?;
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
    let out = translate(cfg, "en", to, &[], &[], &["Good evening, and welcome back.".to_string()])?;
    Ok(out.into_iter().next().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replies() {
        assert_eq!(parse_lines("{\"t\": [\"a\", \"b\"]}", 2).unwrap(), vec!["a", "b"]);
        assert_eq!(
            parse_lines("<think>hmm</think>\n```json\n{\"t\":[\"x\"]}\n```", 1).unwrap(),
            vec!["x"]
        );
        assert!(parse_lines("{\"t\": [\"a\"]}", 2).is_err());
        assert_eq!(strip_thinking("<think>a</think> ok"), "ok");
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
}
