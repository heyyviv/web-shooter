use std::{cell::RefCell, rc::Rc, sync::LazyLock};

use lol_html::{HtmlRewriter, Settings, element, text};
use regex::Regex;
use serde_json::Value;
use spider::url::Url;
use spider_transformations::transformation::content::{
    ReturnFormat, TransformConfig, TransformInput, transform_content_input,
};

/// Below this many characters of visible text, the page is treated as an
/// empty client-side shell and embedded page data is used instead.
const EMPTY_SHELL_CHARS: usize = 200;

/// JSON-LD fields longer than this are left out: long text (descriptions,
/// article bodies) is already in the markdown, so repeating it wastes tokens.
const MAX_LD_VALUE_CHARS: usize = 200;

/// Schema.org types that describe the site rather than the page's content.
const SKIP_LD_TYPES: &[&str] = &[
    "WebSite",
    "WebPage",
    "BreadcrumbList",
    "Organization",
    "SearchAction",
    "ImageObject",
    "SiteNavigationElement",
    "ListItem",
];

pub struct Extracted {
    pub title: Option<String>,
    pub markdown: String,
}

/// Turn raw HTML into compact markdown: main content, plus JSON-LD facts,
/// plus Next.js page data when the HTML itself is an empty shell.
pub fn extract(url: &str, body: &[u8]) -> Extracted {
    let parsed = Url::parse(url).ok();
    let (embedded, html) = scan(body);

    // `clean_html` empties some pages (Wikipedia, Next.js) and `readability`
    // drops headings that chunking relies on, so both stay off; `tidy` removes
    // the leftover noise instead.
    let config = TransformConfig {
        return_format: ReturnFormat::Markdown,
        main_content: true,
        clean_html: false,
        filter_images: true,
        filter_svg: true,
        readability: false,
    };
    let input = TransformInput {
        url: parsed.as_ref(),
        content: &html,
        screenshot_bytes: None,
        encoding: None,
        selector_config: None,
        ignore_tags: None,
    };
    let mut markdown = tidy(&transform_content_input(input, &config));

    let facts: Vec<String> = embedded
        .json_ld
        .iter()
        .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
        .flat_map(|v| json_ld_facts(&v))
        .collect();
    if !facts.is_empty() {
        markdown = format!("{}\n\n{}", facts.join("\n"), markdown);
    }

    if visible_chars(&markdown) < EMPTY_SHELL_CHARS
        && let Some(text) = embedded.next_data.as_deref().and_then(next_data_text)
    {
        markdown = format!("{markdown}\n\n{text}");
    }

    Extracted {
        title: embedded.title.filter(|t| !t.is_empty()),
        markdown: markdown.trim().to_string(),
    }
}

#[derive(Default)]
struct Embedded {
    title: Option<String>,
    json_ld: Vec<String>,
    next_data: Option<String>,
}

/// One streaming pass that pulls the title and embedded JSON out of the HTML
/// and returns the HTML without MathML. Formulas otherwise convert to one
/// character per line; the LaTeX fallback that sites like Wikipedia include
/// next to the MathML is kept.
fn scan(body: &[u8]) -> (Embedded, Vec<u8>) {
    let found = Rc::new(RefCell::new(Embedded::default()));
    let title = Rc::new(RefCell::new(String::new()));
    let ld_buf = Rc::new(RefCell::new(String::new()));
    let next_buf = Rc::new(RefCell::new(String::new()));
    let mut html = Vec::with_capacity(body.len());

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("math", |el| {
                    el.remove();
                    Ok(())
                }),
                text!("head > title", {
                    let title = title.clone();
                    move |t| {
                        title.borrow_mut().push_str(t.as_str());
                        Ok(())
                    }
                }),
                text!(r#"script[type="application/ld+json"]"#, {
                    let (found, buf) = (found.clone(), ld_buf.clone());
                    move |t| {
                        buf.borrow_mut().push_str(t.as_str());
                        if t.last_in_text_node() {
                            found.borrow_mut().json_ld.push(buf.take());
                        }
                        Ok(())
                    }
                }),
                text!("script#__NEXT_DATA__", {
                    let (found, buf) = (found.clone(), next_buf.clone());
                    move |t| {
                        buf.borrow_mut().push_str(t.as_str());
                        if t.last_in_text_node() {
                            found.borrow_mut().next_data = Some(buf.take());
                        }
                        Ok(())
                    }
                }),
            ],
            ..Settings::new()
        },
        |chunk: &[u8]| html.extend_from_slice(chunk),
    );
    // Malformed HTML yields less data; convert the original instead.
    let ok = rewriter.write(body).is_ok() && rewriter.end().is_ok();
    if !ok {
        html = body.to_vec();
    }

    let mut found = found.take();
    found.title = Some(collapse_ws(&title.borrow()));
    (found, html)
}

/// Short `key: value` lines from a JSON-LD object (or `@graph` / array of them).
fn json_ld_facts(v: &Value) -> Vec<String> {
    match v {
        Value::Array(items) => items.iter().flat_map(json_ld_facts).collect(),
        Value::Object(map) => {
            if let Some(graph) = map.get("@graph") {
                return json_ld_facts(graph);
            }
            let ty = map.get("@type").map(type_name).unwrap_or_default();
            if ty.is_empty() || SKIP_LD_TYPES.contains(&ty.as_str()) {
                return Vec::new();
            }
            let mut fields = Vec::new();
            flatten_scalars("", v, &mut fields);
            if fields.is_empty() {
                return Vec::new();
            }
            vec![format!("**{ty}**: {}", fields.join(" · "))]
        }
        _ => Vec::new(),
    }
}

fn type_name(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.first().map(type_name).unwrap_or_default(),
        _ => String::new(),
    }
}

fn flatten_scalars(prefix: &str, v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if k.starts_with('@') {
                    continue;
                }
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_scalars(&key, child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                flatten_scalars(prefix, child, out);
            }
        }
        Value::String(s) => {
            let s = collapse_ws(s);
            if !s.is_empty() && s.len() <= MAX_LD_VALUE_CHARS && !s.starts_with("http") {
                out.push(format!("{prefix}: {s}"));
            }
        }
        Value::Number(n) => out.push(format!("{prefix}: {n}")),
        Value::Bool(_) | Value::Null => {}
    }
}

/// Readable strings from Next.js `pageProps`, for pages whose HTML is an empty shell.
fn next_data_text(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let props = v.pointer("/props/pageProps")?;
    let mut lines = Vec::new();
    collect_prose(props, &mut lines);
    let mut seen = std::collections::HashSet::new();
    lines.retain(|l| seen.insert(l.clone()));
    (!lines.is_empty()).then(|| lines.join("\n\n"))
}

/// Strings that look like prose (several words), skipping ids, URLs and slugs.
fn collect_prose(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => map.values().for_each(|c| collect_prose(c, out)),
        Value::Array(items) => items.iter().for_each(|c| collect_prose(c, out)),
        Value::String(s) => {
            let s = collapse_ws(s);
            if s.split(' ').count() >= 4 && !s.starts_with("http") {
                out.push(s);
            }
        }
        _ => {}
    }
}

static EMPTY_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\s*\]\([^)]*\)").unwrap());
static CITATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[[^\]]*\]\]\([^)]*\)").unwrap());
/// Wiki-style section edit links: `[[edit](…)]` or `[edit](…)`.
static EDIT_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[?\[edit\]\([^)]*\)\]?").unwrap());
static FRAGMENT_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(#[^)]*\)").unwrap());
static ENTITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&(#[0-9]+|#[xX][0-9a-fA-F]+|[a-zA-Z]+);").unwrap());
static JUNK_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\s|\[\]()]*$").unwrap());

/// Remove markdown noise that costs tokens but carries no content: empty
/// links, citation markers, in-page anchors, HTML entities and blank lines.
fn tidy(markdown: &str) -> String {
    let s = EMPTY_LINK.replace_all(markdown, "");
    let s = CITATION.replace_all(&s, "");
    let s = EDIT_LINK.replace_all(&s, "");
    let s = FRAGMENT_LINK.replace_all(&s, "$1");
    let s = ENTITY.replace_all(&s, |c: &regex::Captures| {
        decode_entity(&c[1]).unwrap_or_else(|| c[0].to_string())
    });

    let mut out: Vec<String> = Vec::new();
    let mut in_code = false;
    // A heading marker the converter left on its own line, e.g. "##".
    let mut pending_heading: Option<&str> = None;
    for line in s.lines() {
        let line = line.trim_end();
        if line.starts_with("```") {
            in_code = !in_code;
            out.push(line.to_string());
            continue;
        }
        if in_code {
            out.push(clean_code_line(line));
            continue;
        }
        if JUNK_LINE.is_match(line) {
            continue;
        }
        if !line.is_empty() && line.len() <= 6 && line.chars().all(|c| c == '#') {
            pending_heading = Some(line);
            continue;
        }
        match pending_heading.take() {
            Some(marker) => out.push(format!("{marker} {}", line.trim_start())),
            None => out.push(line.to_string()),
        }
    }
    out.join("\n")
}

/// The converter wraps code lines in inline backticks and escapes markdown
/// punctuation inside fenced blocks; undo both so code reads as written.
fn clean_code_line(line: &str) -> String {
    let line = line.strip_prefix('`').unwrap_or(line);
    let line = line.strip_suffix('`').unwrap_or(line);
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && chars
                .peek()
                .is_some_and(|n| r"\`*_{}[]()#+-.!<>|".contains(*n))
        {
            continue;
        }
        out.push(c);
    }
    out
}

fn decode_entity(name: &str) -> Option<String> {
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "middot" => '·',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "rsquo" => '’',
        "lsquo" => '‘',
        "rdquo" => '”',
        "ldquo" => '“',
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code)?
        }
    };
    Some(c.to_string())
}

fn visible_chars(markdown: &str) -> usize {
    markdown.chars().filter(|c| c.is_alphanumeric()).count()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidy_removes_link_noise() {
        let md = "A [web](https://w.org/Web_(x)) crawler.[[5]](https://w.org/#cite-5)\n[\n](#)\n[Jump](#main) text &amp; more &#x60;x&#96;\n[[edit](https://w.org/x?action=edit)]";
        assert_eq!(
            tidy(md),
            "A [web](https://w.org/Web_(x)) crawler.\nJump text & more `x`"
        );
    }

    #[test]
    fn tidy_joins_split_headings_and_cleans_code() {
        let md = "##\nWhat's new\n```\n`fn f() -\\> Vec\\<i32\\> { a\\_b }`\n```";
        assert_eq!(
            tidy(md),
            "## What's new\n```\nfn f() -> Vec<i32> { a_b }\n```"
        );
    }

    #[test]
    fn json_ld_keeps_short_facts_and_skips_site_types() {
        let v: Value = serde_json::from_str(
            r#"{"@graph":[
                {"@type":"WebSite","name":"Acme"},
                {"@type":"JobPosting","title":"Rust Engineer",
                 "hiringOrganization":{"@type":"Organization","name":"Acme"},
                 "url":"https://acme.com/jobs/1",
                 "description":"long text that is already in the page body and should be skipped because it is over two hundred characters long, which it is once this sentence keeps going for a while longer than you would expect it to."}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            json_ld_facts(&v),
            vec!["**JobPosting**: hiringOrganization.name: Acme · title: Rust Engineer"]
        );
    }

    #[test]
    fn mathml_is_dropped_but_latex_fallback_kept() {
        let html = br#"<html><body><main><p>Freshness is
            <span><math><mi>F</mi><mo>=</mo><mn>1</mn></math>
            <img alt="{\displaystyle F=1}"></span> for fresh pages.</p></main></body></html>"#;
        let md = extract("https://w.org/", html).markdown;
        assert!(!md.contains("\nF\n"), "{md}");
        assert!(md.contains("Freshness is"), "{md}");
    }

    #[test]
    fn next_data_used_only_for_empty_shell() {
        let html = br#"<html><head><title>App</title></head><body><div id="root"></div>
            <script id="__NEXT_DATA__" type="application/json">
            {"props":{"pageProps":{"id":"x1","post":{"body":"This is the real article text."}}}}
            </script></body></html>"#;
        let e = extract("https://app.example/", html);
        assert_eq!(e.title.as_deref(), Some("App"));
        assert!(e.markdown.contains("This is the real article text."));
        assert!(!e.markdown.contains("x1"));
    }
}
