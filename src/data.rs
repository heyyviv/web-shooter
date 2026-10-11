//! Structured data → markdown: JSON API responses, and the JSON that
//! JavaScript-rendered pages embed in their HTML.

use std::{borrow::Cow, sync::LazyLock};

use regex::Regex;
use serde_json::{Map, Value};

/// String fields up to this long go on the record's one-line summary;
/// longer ones become paragraphs.
const SHORT_FIELD_CHARS: usize = 120;

/// Nested objects deeper than this are skipped (usually internal state).
const MAX_DEPTH: usize = 6;

/// Embedded scripts larger than this are skipped: parsing cost grows with
/// size and such blobs are mostly code or bundled state, not content.
const MAX_BLOB_BYTES: usize = 4 << 20;

/// Keys whose value names a record, in order of preference (compared
/// case-insensitively, ignoring `_` and `-`).
const TITLE_KEYS: &[&str] = &[
    "title",
    "postingtitle",
    "jobtitle",
    "headline",
    "name",
    "label",
];

static HTML_BLOCK_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>|</(p|div|li|h[1-6]|tr|ul|ol|section)>").unwrap());
static HTML_LI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<li[^>]*>").unwrap());
static HTML_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());

/// Render any JSON value as compact markdown. Arrays of objects become one
/// `##` section per record, so chunking and ranking can pick single records.
pub fn render(value: &Value) -> String {
    let mut out = String::new();
    render_into(value, "", 0, &mut out);
    out.trim().to_string()
}

fn render_into(value: &Value, key: &str, depth: usize, out: &mut String) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Some(inner) = json_in_string(value) {
        render_into(&inner, key, depth + 1, out);
        return;
    }
    match value {
        Value::Array(items) if items.iter().any(Value::is_object) => {
            for item in items {
                if let Value::Object(map) = item {
                    render_record(map, depth, out);
                }
            }
        }
        Value::Array(items) if items.iter().any(|i| json_in_string(i).is_some()) => {
            for item in items {
                render_into(item, key, depth + 1, out);
            }
        }
        Value::Array(items) => {
            let scalars: Vec<String> = items.iter().filter_map(scalar).collect();
            if !scalars.is_empty() {
                out.push_str(&format!("{}{}\n", label(key), scalars.join(", ")));
            }
        }
        Value::Object(map) => {
            let (short, long, nested) = split_fields(map);
            if !short.is_empty() {
                out.push_str(&short.join(" · "));
                out.push('\n');
            }
            for (k, text) in long {
                out.push_str(&format!("{}{text}\n", label(k)));
            }
            for (k, v) in nested {
                render_into(&v, k, depth + 1, out);
            }
        }
        other => {
            if let Some(s) = scalar(other) {
                out.push_str(&format!("{}{s}\n", label(key)));
            }
        }
    }
}

/// One record: a heading from its title field, its short fields on one line,
/// then long text fields and nested data.
fn render_record(map: &Map<String, Value>, depth: usize, out: &mut String) {
    let title = map
        .iter()
        .filter_map(|(k, v)| {
            let rank = TITLE_KEYS.iter().position(|t| *t == normalize_key(k))?;
            Some((rank, v.as_str()?.trim()))
        })
        .filter(|(_, t)| !t.is_empty())
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, t)| t.to_string());
    let (short, long, nested) = split_fields(map);
    if short.is_empty() && long.is_empty() && nested.is_empty() {
        return;
    }
    out.push_str(&format!("\n## {}\n", title.as_deref().unwrap_or("Item")));
    if !short.is_empty() {
        out.push_str(&short.join(" · "));
        out.push('\n');
    }
    for (k, text) in long {
        out.push_str(&format!("{}{text}\n", label(k)));
    }
    for (k, v) in nested {
        render_into(&v, k, depth + 1, out);
    }
}

type Fields<'a> = (
    Vec<String>,
    Vec<(&'a str, String)>,
    Vec<(&'a str, Cow<'a, Value>)>,
);

/// Short `key: value` pairs, long text fields (HTML turned into text), and
/// nested values, skipping empty ones. Keys containing `/` are skipped: they
/// are file maps (asset manifests, module registries), never page content,
/// and on some pages they run to megabytes.
fn split_fields(map: &Map<String, Value>) -> Fields<'_> {
    let (mut short, mut long, mut nested) = (Vec::new(), Vec::new(), Vec::new());
    for (k, v) in map.iter().filter(|(k, _)| !k.contains('/')) {
        match v {
            Value::Object(m) if !m.is_empty() => nested.push((k.as_str(), Cow::Borrowed(v))),
            Value::Array(a) if !a.is_empty() => nested.push((k.as_str(), Cow::Borrowed(v))),
            Value::String(_) if let Some(inner) = json_in_string(v) => {
                nested.push((k.as_str(), Cow::Owned(inner)));
            }
            Value::String(s) => {
                let text = html_to_text(s);
                if text.is_empty() {
                    continue;
                }
                if text.chars().count() <= SHORT_FIELD_CHARS && !text.contains('\n') {
                    short.push(format!("{k}: {text}"));
                } else {
                    long.push((k.as_str(), text));
                }
            }
            other => {
                if let Some(s) = scalar(other) {
                    short.push(format!("{k}: {s}"));
                }
            }
        }
    }
    (short, long, nested)
}

/// JSON stored as a string inside JSON (an object or array), parsed.
fn json_in_string(v: &Value) -> Option<Value> {
    let s = v.as_str()?;
    if !matches!(s.trim_start().as_bytes().first(), Some(b'{' | b'[')) {
        return None;
    }
    serde_json::from_str(s).ok()
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = html_to_text(s);
            (!t.is_empty()).then_some(t)
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn label(key: &str) -> String {
    if key.is_empty() {
        String::new()
    } else {
        format!("{key}: ")
    }
}

fn normalize_key(k: &str) -> String {
    k.chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect::<String>()
        .to_lowercase()
}

/// Plain text from a string that may contain HTML: block ends become line
/// breaks, list items get a dash, other tags are dropped.
fn html_to_text(s: &str) -> String {
    if !s.contains('<') {
        return s.trim().to_string();
    }
    let s = HTML_LI.replace_all(s, "\n- ");
    let s = HTML_BLOCK_END.replace_all(&s, "\n");
    let s = HTML_TAG.replace_all(&s, "");
    let s = crate::extract::decode_entities(&s);
    s.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty() && l != "-")
        .collect::<Vec<_>>()
        .join("\n")
}

/// JSON values embedded in an inline `<script>`: the whole body if it is
/// JSON, else every `JSON.parse('…')` string and `= {…}` / `= […]` literal
/// that parses as JSON.
pub fn embedded_json(script: &str) -> Vec<Value> {
    if script.len() > MAX_BLOB_BYTES {
        return Vec::new();
    }
    let trimmed = script.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return vec![v];
    }
    let mut found = Vec::new();
    let bytes = script.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"JSON.parse(") {
            let start = i + "JSON.parse(".len();
            if let Some((literal, end)) = js_string(script, start)
                && let Ok(v) = serde_json::from_str::<Value>(&literal)
            {
                found.push(v);
                i = end;
                continue;
            }
        }
        if bytes[i] == b'=' && !matches!(bytes.get(i + 1), Some(b'=' | b'>')) {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if matches!(bytes.get(j), Some(b'{' | b'['))
                && let Some(end) = balanced_end(bytes, j)
                && let Ok(v) = serde_json::from_str::<Value>(&script[j..end])
            {
                found.push(v);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    found
}

/// The JavaScript string literal starting at `start` (after optional
/// whitespace), unescaped, and the index just past its closing quote.
fn js_string(s: &str, start: usize) -> Option<(String, usize)> {
    let rest = &s[start..];
    let offset = rest.len() - rest.trim_start().len();
    let mut chars = rest.trim_start().char_indices();
    let (_, quote) = chars.next()?;
    if !matches!(quote, '\'' | '"' | '`') {
        return None;
    }
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        match c {
            c if c == quote => return Some((out, start + offset + i + c.len_utf8())),
            '\\' => {
                let (_, e) = chars.next()?;
                match e {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'x' => {
                        let hex: String = (0..2)
                            .filter_map(|_| chars.next().map(|(_, c)| c))
                            .collect();
                        out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                    }
                    'u' => {
                        let hex: String = (0..4)
                            .filter_map(|_| chars.next().map(|(_, c)| c))
                            .collect();
                        let code = u32::from_str_radix(&hex, 16).ok()?;
                        // Lone surrogate halves (emoji split in two) can't be a `char`.
                        out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                    }
                    other => out.push(other),
                }
            }
            c => out.push(c),
        }
    }
    None
}

/// Index just past the bracket that closes the one at `start`, skipping
/// brackets inside string literals.
fn balanced_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string: Option<u8> = None;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if let Some(q) = in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                in_string = None;
            }
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => in_string = Some(b),
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_become_sections_with_short_fields_inline() {
        let v: Value = serde_json::from_str(
            r#"{"hits": 2, "jobs": [
                {"id": 7, "title": "SDE I", "location": "Bengaluru", "remote": false,
                 "description": "<p>Build <b>services</b>.</p><ul><li>Rust</li><li>AWS</li></ul>"},
                {"Posting_Title": "Applied AI Engineer", "City": "Bangalore", "Salary": null}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            render(&v),
            "hits: 2\n\n\
             ## SDE I\n\
             id: 7 · location: Bengaluru · remote: false · title: SDE I\n\
             description: Build services.\n- Rust\n- AWS\n\n\
             ## Applied AI Engineer\n\
             City: Bangalore · Posting_Title: Applied AI Engineer"
        );
    }

    #[test]
    fn json_inside_strings_is_rendered_as_data() {
        let v: Value = serde_json::from_str(
            r#"{"locations": "[{\"city\":\"Bengaluru\",\"country\":\"IND\"}]"}"#,
        )
        .unwrap();
        assert_eq!(render(&v), "## Item\ncity: Bengaluru · country: IND");
        let in_array: Value =
            serde_json::from_str(r#"{"locations": ["{\"city\":\"Bengaluru\"}"]}"#).unwrap();
        assert_eq!(render(&in_array), "city: Bengaluru");
    }

    #[test]
    fn file_maps_are_skipped() {
        let v: Value = serde_json::from_str(
            r#"{"dist/a.css": "dist/a.1f2e.css", "dist/b.js": "dist/b.9c8d.js", "name": "Careers"}"#,
        )
        .unwrap();
        assert_eq!(render(&v), "name: Careers");
    }

    #[test]
    fn finds_json_parse_strings_with_hex_escapes() {
        let script = r#"var jobs = JSON.parse('[{\x22Posting_Title\x22:\x22Applied AI Engineer\x22,\x22City\x22:\x22Bangalore\x22}]');"#;
        let found = embedded_json(script);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0][0]["City"], "Bangalore");
    }

    #[test]
    fn unicode_escaped_quotes_become_json_syntax() {
        // Escapes as they appear in the page source; JavaScript turns them
        // into real quotes before JSON.parse sees them.
        let script = r#"JSON.parse("{\u0022a\u0022: \u0022caf\u00e9\u0022}")"#;
        assert_eq!(embedded_json(script)[0]["a"], "caf\u{e9}");
    }

    #[test]
    fn multibyte_text_around_data_does_not_panic() {
        let script = "const t = 'café'; window.x = {\"k\": \"naïve\"};";
        assert_eq!(embedded_json(script)[0]["k"], "naïve");
    }

    #[test]
    fn finds_assigned_literals_and_skips_comparisons() {
        let script = r#"if (a == b) {} window.__STATE__ = {"post": {"body": "text with } brace"}}; x => {y}"#;
        let found = embedded_json(script);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["post"]["body"], "text with } brace");
    }

    #[test]
    fn whole_script_json_is_parsed() {
        assert_eq!(embedded_json(r#" {"a": [1, 2]} "#).len(), 1);
        assert!(embedded_json("console.log(1)").is_empty());
    }
}
