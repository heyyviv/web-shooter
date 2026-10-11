use std::{cell::RefCell, rc::Rc, sync::LazyLock};

use lol_html::{HtmlRewriter, Settings, element, html_content::ContentType, text};
use regex::Regex;
use serde_json::Value;
use spider::url::Url;
use spider_transformations::transformation::content::{
    ReturnFormat, TransformConfig, TransformInput, transform_content_input,
};

use crate::{
    data,
    links::{self, Link},
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
    /// Every readable link on the page, in document order.
    pub links: Vec<Link>,
    /// The page's `<link rel="canonical">`, resolved, if it has one.
    pub canonical: Option<String>,
}

/// Embedded page data rendered for an empty shell is cut at this size.
const MAX_SHELL_DATA_CHARS: usize = 1 << 20;

/// Turn a response into compact markdown. JSON (an API response) is
/// rendered record by record. HTML becomes its main content plus JSON-LD
/// facts, plus the data embedded in its scripts when the HTML itself is an
/// empty JavaScript shell.
pub fn extract(url: &str, body: &[u8]) -> Extracted {
    let parsed = Url::parse(url).ok();
    if let Some(value) = json_body(body) {
        return Extracted {
            title: None,
            markdown: data::render(&value),
            links: Vec::new(),
            canonical: None,
        };
    }
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
        .scripts
        .iter()
        .filter(|s| s.kind == "application/ld+json")
        .filter_map(|s| serde_json::from_str::<Value>(&s.text).ok())
        .flat_map(|v| json_ld_facts(&v))
        .collect();
    if !facts.is_empty() {
        markdown = format!("{}\n\n{}", facts.join("\n"), markdown);
    }

    if visible_chars(&markdown) < EMPTY_SHELL_CHARS {
        let mut blobs: Vec<String> = Vec::new();
        let mut size = 0;
        for script in embedded.scripts.iter().filter(|s| s.may_hold_data()) {
            for value in data::embedded_json(&script.text) {
                let rendered = data::render(&value);
                size += rendered.len();
                blobs.push(rendered);
            }
            if size > MAX_SHELL_DATA_CHARS {
                break;
            }
        }
        // Content first: pages embed config (fonts, flags, translations)
        // alongside the real data, often earlier in the page. Without a
        // query, the start of the page is what gets returned.
        blobs.sort_by_key(|b| std::cmp::Reverse(prose_chars(b)));
        let page_data = blobs.join("\n\n");
        if !page_data.trim().is_empty() {
            markdown = format!("{markdown}\n\n{}", page_data.trim());
        }
    }

    Extracted {
        title: embedded.title.filter(|t| !t.is_empty()),
        markdown: markdown.trim().to_string(),
        canonical: embedded
            .canonical
            .and_then(|href| links::normalize(parsed.as_ref(), &href)),
        links: links::resolve(
            parsed.as_ref(),
            embedded
                .links
                .into_iter()
                .map(|(href, text, heading)| {
                    (href, decode_entities(&text), decode_entities(&heading))
                })
                .collect(),
        ),
    }
}

/// An inline `<script>`: its `type` (lowercased, empty if absent) and body.
struct Script {
    kind: String,
    text: String,
}

impl Script {
    /// Scripts that can carry page data: JSON and JavaScript. JSON-LD is read
    /// separately; HTML templates and the like are not data.
    fn may_hold_data(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "" | "text/javascript" | "application/javascript" | "module" | "application/json"
        )
    }
}

/// The body as JSON, if the response is a JSON document rather than HTML.
fn json_body(body: &[u8]) -> Option<Value> {
    let first = body.iter().find(|b| !b.is_ascii_whitespace())?;
    if !matches!(first, b'{' | b'[') {
        return None;
    }
    serde_json::from_slice(body).ok()
}

#[derive(Default)]
struct Embedded {
    title: Option<String>,
    /// Inline scripts only; `<script src=…>` is skipped.
    scripts: Vec<Script>,
    /// `(href, anchor text, heading above)` before resolving.
    links: Vec<(String, String, String)>,
    canonical: Option<String>,
}

/// One streaming pass that pulls the title, embedded JSON and links out of
/// the HTML and returns the HTML without MathML or icon-font glyph names. Formulas otherwise convert to one
/// character per line; the LaTeX fallback that sites like Wikipedia include
/// next to the MathML is kept.
fn scan(body: &[u8]) -> (Embedded, Vec<u8>) {
    let found = Rc::new(RefCell::new(Embedded::default()));
    let title = Rc::new(RefCell::new(String::new()));
    // Whether the script being read is inline (text handlers append to the last script).
    let in_inline_script = Rc::new(RefCell::new(false));
    let heading = Rc::new(RefCell::new(String::new()));
    let mut html = Vec::with_capacity(body.len());

    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![
                element!("math", |el| {
                    el.remove();
                    Ok(())
                }),
                // Icon fonts render words as glyphs ("place", "arrow_back");
                // as text they are noise. Icons are marked aria-hidden or use
                // a Material icon class.
                element!(
                    r#"i[aria-hidden="true"], span[aria-hidden="true"], [class*="material-icons"], [class*="material-symbols"]"#,
                    |el| {
                        // A space, so the words on either side don't merge.
                        el.replace(" ", ContentType::Text);
                        Ok(())
                    }
                ),
                element!("h1, h2, h3, h4, h5, h6", {
                    let heading = heading.clone();
                    move |_| {
                        heading.borrow_mut().clear();
                        Ok(())
                    }
                }),
                text!("h1, h2, h3, h4, h5, h6", {
                    let heading = heading.clone();
                    move |t| {
                        push_text(&mut heading.borrow_mut(), t);
                        Ok(())
                    }
                }),
                element!("a[href]", {
                    let (found, heading) = (found.clone(), heading.clone());
                    move |el| {
                        if let Some(href) = el.get_attribute("href") {
                            let heading = heading.borrow().clone();
                            found
                                .borrow_mut()
                                .links
                                .push((href, String::new(), heading));
                        }
                        Ok(())
                    }
                }),
                text!("a[href]", {
                    let found = found.clone();
                    move |t| {
                        if let Some(link) = found.borrow_mut().links.last_mut() {
                            push_text(&mut link.1, t);
                        }
                        Ok(())
                    }
                }),
                element!(r#"link[rel="canonical"][href]"#, {
                    let found = found.clone();
                    move |el| {
                        found.borrow_mut().canonical = el.get_attribute("href");
                        Ok(())
                    }
                }),
                text!("head > title", {
                    let title = title.clone();
                    move |t| {
                        title.borrow_mut().push_str(t.as_str());
                        Ok(())
                    }
                }),
                element!("script", {
                    let (found, inline) = (found.clone(), in_inline_script.clone());
                    move |el| {
                        let is_inline = !el.has_attribute("src");
                        *inline.borrow_mut() = is_inline;
                        if is_inline {
                            let kind = el.get_attribute("type").unwrap_or_default();
                            found.borrow_mut().scripts.push(Script {
                                kind: kind.trim().to_ascii_lowercase(),
                                text: String::new(),
                            });
                        }
                        Ok(())
                    }
                }),
                text!("script", {
                    let (found, inline) = (found.clone(), in_inline_script.clone());
                    move |t| {
                        if *inline.borrow()
                            && let Some(script) = found.borrow_mut().scripts.last_mut()
                        {
                            script.text.push_str(t.as_str());
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
    let s = decode_entities(&s);

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

/// Append a text chunk, separating text nodes with a space: adjacent inline
/// elements (`<span>Title</span><span>London</span>`) would otherwise run together.
fn push_text(buf: &mut String, chunk: &lol_html::html_content::TextChunk) {
    buf.push_str(chunk.as_str());
    if chunk.last_in_text_node() {
        buf.push(' ');
    }
}

pub(crate) fn decode_entities(s: &str) -> String {
    ENTITY
        .replace_all(s, |c: &regex::Captures| {
            decode_entity(&c[1]).unwrap_or_else(|| c[0].to_string())
        })
        .into_owned()
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

/// Letters and digits in sentence-like lines (six or more words): long
/// text, as opposed to config values and labels.
fn prose_chars(text: &str) -> usize {
    text.lines()
        .filter(|l| l.split_whitespace().count() >= 6)
        .map(|l| l.chars().filter(|c| c.is_alphanumeric()).count())
        .sum()
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
    fn canonical_url_is_resolved() {
        let html = br#"<html><head><link rel="canonical" href="/wiki/Web_page"></head><body>x</body></html>"#;
        let e = extract("https://en.wikipedia.org/wiki/Web_pages", html);
        assert_eq!(
            e.canonical.as_deref(),
            Some("https://en.wikipedia.org/wiki/Web_page")
        );
    }

    #[test]
    fn links_carry_anchor_text_and_heading() {
        let html = br#"<html><body><h2>Open <em>roles</em></h2>
            <a href="/jobs/1"><span>Rust</span> engineer<span>Berlin &amp; remote</span></a>
            <h3>Other</h3><a href="https://x.org/">X</a></body></html>"#;
        let links = extract("https://acme.com/careers", html).links;
        let got: Vec<(&str, &str, &str)> = links
            .iter()
            .map(|l| (l.url.as_str(), l.text.as_str(), l.heading.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "https://acme.com/jobs/1",
                    "Rust engineer Berlin & remote",
                    "Open roles"
                ),
                ("https://x.org/", "X", "Other"),
            ]
        );
    }

    #[test]
    fn icon_glyph_names_are_dropped() {
        let html = br#"<html><body><main><p>Google<i class="google-material-icons" aria-hidden='true'>place</i>Bengaluru, Karnataka, India</p>
            <p><span class="material-symbols-outlined">arrow_back</span>Back to jobs</p></main></body></html>"#;
        let md = extract("https://x.org/", html).markdown;
        assert!(md.contains("Google Bengaluru, Karnataka, India"), "{md}");
        assert!(!md.contains("place") && !md.contains("arrow"), "{md}");
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
    fn shell_pages_use_embedded_script_data() {
        let html = br#"<html><head><title>App</title></head><body><div id="root"></div>
            <script id="__NEXT_DATA__" type="application/json">
            {"props":{"pageProps":{"post":{"body":"This is the real article text."}}}}
            </script>
            <script>var jobs = JSON.parse('[{\x22Posting_Title\x22:\x22Applied AI Engineer\x22,\x22City\x22:\x22Bangalore\x22}]');</script>
            <script type="text/html"><p>template, not data</p></script>
            </body></html>"#;
        let e = extract("https://app.example/", html);
        assert_eq!(e.title.as_deref(), Some("App"));
        assert!(
            e.markdown.contains("This is the real article text."),
            "{}",
            e.markdown
        );
        assert!(
            e.markdown
                .contains("## Applied AI Engineer\nCity: Bangalore"),
            "{}",
            e.markdown
        );
        assert!(!e.markdown.contains("template"), "{}", e.markdown);
    }

    #[test]
    fn shell_data_puts_prose_before_config() {
        let html = br#"<html><head><title>Job</title></head><body><div id="app"></div>
            <script>var fonts = [{"name": "Montserrat", "family": "Montserrat, sans-serif"}];</script>
            <script>var job = {"title": "Applied AI Engineer", "description": "You will design and build production LLM systems with the team."};</script>
            </body></html>"#;
        let md = extract("https://x.zohorecruit.com/jobs/1", html).markdown;
        let job = md.find("Applied AI Engineer").expect("job rendered");
        let fonts = md.find("Montserrat").expect("fonts rendered");
        assert!(job < fonts, "{md}");
    }

    #[test]
    fn pages_with_content_ignore_script_data() {
        let html = format!(
            "<html><body><main><p>{}</p></main><script>window.s = {{\"secret\": \"config\"}};</script></body></html>",
            "Real visible text. ".repeat(20)
        );
        let e = extract("https://x.org/", html.as_bytes());
        assert!(!e.markdown.contains("config"), "{}", e.markdown);
    }

    #[test]
    fn json_responses_are_rendered_as_records() {
        let body = br#"{"jobs": [{"title": "SDE I", "city": "Bengaluru"}]}"#;
        let e = extract("https://www.amazon.jobs/en/search.json", body);
        assert_eq!(e.markdown, "## SDE I\ncity: Bengaluru · title: SDE I");
    }
}
