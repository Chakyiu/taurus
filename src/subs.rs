//! Reading subtitle files: SubRip, WebVTT and Advanced SSA.

use anyhow::{Result, bail};

use crate::data::{LANGS, Segment};

pub const SUBTITLE_EXTS: &[&str] = &["srt", "vtt", "ass", "ssa"];

pub fn is_subtitle(name: &str) -> bool {
    ext_of(name).is_some_and(|e| SUBTITLE_EXTS.contains(&e.as_str()))
}

fn ext_of(name: &str) -> Option<String> {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())
}

/// Parses by extension, falling back to sniffing the content.
pub fn parse(file_name: &str, text: &str) -> Result<Vec<Segment>> {
    let text = text.trim_start_matches('\u{feff}');
    let ext = ext_of(file_name).unwrap_or_default();
    let segs = if ext == "ass" || ext == "ssa" || text.contains("[Events]") {
        parse_ass(text)
    } else if ext == "vtt" || text.starts_with("WEBVTT") {
        parse_cues(text, true)
    } else {
        parse_cues(text, false)
    };
    if segs.is_empty() {
        bail!("no cues found in {file_name}");
    }
    Ok(segs)
}

/// SRT and VTT share one shape: an optional id line, a timing line, then text.
fn parse_cues(text: &str, vtt: bool) -> Vec<Segment> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let lines: Vec<&str> = block.lines().filter(|l| !l.trim().is_empty()).collect();
        let Some(ti) = lines.iter().position(|l| l.contains("-->")) else {
            continue;
        };
        if vtt && lines.first().is_some_and(|l| l.starts_with("NOTE") || l.starts_with("STYLE")) {
            continue;
        }
        let Some((a, b)) = lines[ti].split_once("-->") else { continue };
        let b = b.split_whitespace().next().unwrap_or("");
        let (Some(in_ms), Some(out_ms)) = (parse_time(a.trim()), parse_time(b)) else {
            continue;
        };
        let body = lines[ti + 1..]
            .iter()
            .map(|l| strip_tags(l))
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if body.is_empty() {
            continue;
        }
        out.push(Segment::new(in_ms, out_ms.max(in_ms + 1), &body, ""));
    }
    out.sort_by_key(|s| s.in_ms);
    out
}

fn parse_ass(text: &str) -> Vec<Segment> {
    let mut in_events = false;
    let mut fields: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Format:") {
            fields = rest.split(',').map(|f| f.trim().to_ascii_lowercase()).collect();
            continue;
        }
        let Some(rest) = line.strip_prefix("Dialogue:") else { continue };
        if fields.is_empty() {
            fields = "layer,start,end,style,name,marginl,marginr,marginv,effect,text"
                .split(',')
                .map(String::from)
                .collect();
        }
        let parts: Vec<&str> = rest.splitn(fields.len(), ',').collect();
        let get = |name: &str| fields.iter().position(|f| f == name).and_then(|i| parts.get(i));
        let (Some(a), Some(b), Some(t)) = (get("start"), get("end"), get("text")) else {
            continue;
        };
        let (Some(in_ms), Some(out_ms)) = (parse_time(a.trim()), parse_time(b.trim())) else {
            continue;
        };
        let body = strip_ass(t);
        if body.is_empty() {
            continue;
        }
        out.push(Segment::new(in_ms, out_ms.max(in_ms + 1), &body, ""));
    }
    out.sort_by_key(|s| s.in_ms);
    out
}

/// Accepts `01:02:03,456`, `01:02:03.456`, `02:03.456` and ASS's `1:02:03.45`.
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim().replace(',', ".");
    let (hms, frac) = match s.split_once('.') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (s.clone(), String::new()),
    };
    let parts: Vec<i64> = hms
        .split(':')
        .map(|p| p.trim().parse::<i64>().ok())
        .collect::<Option<_>>()?;
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (*h, *m, *s),
        [m, s] => (0, *m, *s),
        _ => return None,
    };
    let ms = match frac.len() {
        0 => 0,
        1 => frac.parse::<i64>().ok()? * 100,
        2 => frac.parse::<i64>().ok()? * 10,
        _ => frac[..3].parse::<i64>().ok()?,
    };
    Some(((h * 60 + m) * 60 + sec) * 1000 + ms)
}

fn strip_tags(line: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in line.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&nbsp;", " ");
    strip_braces(&out).trim().to_string()
}

fn strip_braces(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn strip_ass(t: &str) -> String {
    strip_braces(t)
        .replace("\\N", "\n")
        .replace("\\n", "\n")
        .replace("\\h", " ")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A language tag in the file name wins (`episode.zh.srt`); otherwise the script decides.
pub fn guess_lang(file_name: &str, segs: &[Segment]) -> String {
    let lower = file_name.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('.').collect();
    parts.pop();
    for p in parts.iter().rev().take(2) {
        if let Some(code) = lang_from_tag(p) {
            return code.to_string();
        }
    }
    let sample: String = segs.iter().take(40).map(|s| s.orig.as_str()).collect();
    script_lang(&sample).unwrap_or("en").to_string()
}

pub fn lang_from_tag(tag: &str) -> Option<&'static str> {
    let tag = tag.trim().to_ascii_lowercase();
    let code = match tag.as_str() {
        "en" | "eng" | "english" => "en",
        "zh" | "chi" | "zho" | "chs" | "zh-cn" | "zh-hans" | "sc" | "cmn" => "zh",
        "zh-hant" | "zh-tw" | "zh-hk" | "cht" | "tc" => "zh-Hant",
        "ja" | "jpn" | "jp" => "ja",
        "ko" | "kor" => "ko",
        "de" | "ger" | "deu" => "de",
        "es" | "spa" => "es",
        "fr" | "fre" | "fra" => "fr",
        _ => return None,
    };
    LANGS.iter().find(|(c, _)| *c == code).map(|(c, _)| *c)
}

pub fn script_lang(text: &str) -> Option<&'static str> {
    let (mut han, mut kana, mut hangul, mut latin) = (0, 0, 0, 0);
    for c in text.chars() {
        match c as u32 {
            0x3040..=0x30FF => kana += 1,
            0xAC00..=0xD7AF => hangul += 1,
            0x4E00..=0x9FFF => han += 1,
            _ if c.is_ascii_alphabetic() => latin += 1,
            _ => {}
        }
    }
    if kana > 0 && kana * 5 >= han {
        Some("ja")
    } else if hangul > han {
        Some("ko")
    } else if han > latin / 3 && han > 0 {
        Some("zh")
    } else if latin > 0 {
        None
    } else {
        None
    }
}

pub fn is_cjk(code: &str) -> bool {
    code.starts_with("zh") || code == "ja"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_srt() {
        let s = "1\n00:00:01,000 --> 00:00:02,500\n<i>Hello</i>\nthere\n\n2\n00:00:03,000 --> 00:00:04,000\nBye\n";
        let segs = parse("a.srt", s).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].in_ms, 1000);
        assert_eq!(segs[0].out_ms, 2500);
        assert_eq!(segs[0].orig, "Hello\nthere");
    }

    #[test]
    fn reads_vtt() {
        let s = "WEBVTT\n\nNOTE hi\n\nc1\n00:01.000 --> 00:02.000 align:start\nOne\n\n00:00:03.000 --> 00:00:04.000\nTwo";
        let segs = parse("a.vtt", s).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].in_ms, 1000);
        assert_eq!(segs[1].orig, "Two");
    }

    #[test]
    fn reads_ass() {
        let s = "[Script Info]\nTitle: x\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,{\\an8}Hi, you\\Nthere\n";
        let segs = parse("a.ass", s).unwrap();
        assert_eq!(segs[0].orig, "Hi, you\nthere");
        assert_eq!(segs[0].out_ms, 2500);
    }

    #[test]
    fn guesses_language() {
        let segs = vec![Segment::new(0, 1, "凌晨四点，这条街", "")];
        assert_eq!(guess_lang("x.srt", &segs), "zh");
        assert_eq!(guess_lang("show.de.srt", &segs), "de");
        assert_eq!(guess_lang("show.eng.srt", &[]), "en");
    }
}
