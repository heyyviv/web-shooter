//! PDF → markdown. Text lines are rebuilt from positioned spans, two-column
//! pages are read column by column, running headers and footers are dropped,
//! headings are recovered from font size, weight and section numbers, and
//! each page's text starts after a `[p.N]` marker line that chunking turns
//! into page numbers.

use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
    time::{Duration, Instant},
};

use pdf_oxide::{PdfDocument, layout::TextSpan};
use regex::Regex;

/// Larger files are refused before parsing.
pub const MAX_BYTES: usize = 25 << 20;

/// Conversion stops at the page reached after this long, keeping what it has.
const TIME_LIMIT: Duration = Duration::from_secs(10);

const MAX_PAGES: usize = 1000;

/// Fewer visible characters than this means there is no text layer.
const MIN_TEXT_CHARS: usize = 200;

pub struct Pdf {
    pub title: Option<String>,
    pub markdown: String,
}

/// A page marker line: `[p.12]`.
pub static PAGE_MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[p\.(\d+)\]$").unwrap());

pub fn is_pdf(body: &[u8]) -> bool {
    body.trim_ascii_start().starts_with(b"%PDF-")
}

pub fn to_markdown(body: &[u8]) -> Result<Pdf, String> {
    if body.len() > MAX_BYTES {
        return Err(format!("PDF is larger than {} MB", MAX_BYTES >> 20));
    }
    // The parser can panic on malformed files; that must not take the server down.
    std::panic::catch_unwind(|| convert(body))
        .unwrap_or_else(|_| Err("could not parse the PDF".to_string()))
}

fn convert(body: &[u8]) -> Result<Pdf, String> {
    let started = Instant::now();
    let doc = PdfDocument::from_bytes(body.to_vec())
        .map_err(|e| format!("could not open the PDF: {e}"))?;
    let count = doc
        .page_count()
        .map_err(|e| format!("could not read the PDF's pages: {e}"))?;

    let mut pages: Vec<Vec<Line>> = Vec::new();
    for p in 0..count.min(MAX_PAGES) {
        if started.elapsed() > TIME_LIMIT {
            break;
        }
        pages.push(lines(&doc.extract_spans(p).unwrap_or_default()));
    }
    let read = pages.len();

    drop_running_lines(&mut pages);
    let style = Style::of(&pages);
    let pages: Vec<Vec<Line>> = pages
        .into_iter()
        .map(|p| reading_order(p.into_iter().filter(|l| !style.is_marker(l)).collect()))
        .collect();
    let (title, mut markdown) = render(&pages, &style);

    if markdown.chars().filter(|c| c.is_alphanumeric()).count() < MIN_TEXT_CHARS {
        return Err("no text layer (probably a scanned PDF); OCR is not supported".to_string());
    }
    if read < count {
        markdown.push_str(&format!(
            "\n\n[stopped after page {read} of {count}: the PDF is too long to read in one call]"
        ));
    }
    Ok(Pdf { title, markdown })
}

/// One span: a run of text in one font.
#[derive(Debug, Clone)]
struct Piece {
    text: String,
    x0: f32,
    x1: f32,
    y: f32,
    size: f32,
    bold: bool,
    mono: bool,
}

impl Piece {
    fn of(s: &TextSpan) -> Self {
        // Some ligature glyphs are reported with no width; estimate one so
        // the gap after them doesn't read as a space.
        let width = if s.bbox.width > 0.01 {
            s.bbox.width
        } else {
            s.text.chars().count() as f32 * s.font_size * 0.5
        };
        Piece {
            text: s.text.clone(),
            x0: s.bbox.x,
            x1: s.bbox.x + width,
            y: s.bbox.y,
            size: s.font_size,
            bold: is_bold(s),
            mono: is_mono(s),
        }
    }
}

/// One line of text, or one column's part of it.
#[derive(Debug, Clone)]
struct Line {
    text: String,
    x0: f32,
    x1: f32,
    y: f32,
    /// Font size covering most of the line's letters.
    size: f32,
    bold: bool,
    mono: bool,
    /// Left and right edges of the column the line sits in (set by `reading_order`).
    left: f32,
    right: f32,
    /// The spans, kept so a line merged across a column gap can be split.
    pieces: Vec<Piece>,
}

impl Line {
    fn new(pieces: Vec<Piece>) -> Self {
        let mut text = String::new();
        let mut prev_end: Option<f32> = None;
        // Letters per font size, to find the line's dominant size.
        let mut sizes: HashMap<u32, usize> = HashMap::new();
        let (mut letters, mut bold, mut mono) = (0, 0, 0);
        for p in &pieces {
            // A span ending in a ligature (`Specifi` + `cations`) is often
            // reported a little short; that gap is not a space.
            let ligature_gap = text.ends_with(['ﬁ', 'ﬂ', 'ﬀ', 'ﬃ', 'ﬄ'])
                || (["fi", "fl", "ff"].iter().any(|l| text.ends_with(l))
                    && p.text.starts_with(|c: char| c.is_lowercase()));
            if let Some(end) = prev_end
                && p.x0 - end > 0.15 * p.size
                && !(ligature_gap && p.x0 - end < 0.6 * p.size)
                && !text.ends_with(char::is_whitespace)
                && !p.text.starts_with(char::is_whitespace)
            {
                text.push(' ');
            }
            text.push_str(&p.text);
            prev_end = Some(p.x1);

            let n = p.text.chars().filter(|c| c.is_alphanumeric()).count();
            *sizes.entry((p.size * 2.0).round() as u32).or_default() += n.max(1);
            letters += n;
            if p.bold {
                bold += n;
            }
            if p.mono {
                mono += n;
            }
        }
        let size = sizes
            .into_iter()
            .max_by_key(|&(size, n)| (n, size))
            .map_or(0.0, |(size, _)| size as f32 / 2.0);
        let x0 = pieces.first().map_or(0.0, |p| p.x0);
        let x1 = pieces.last().map_or(0.0, |p| p.x1);
        Line {
            text: clean_text(&text),
            x0,
            x1,
            y: pieces.iter().map(|p| p.y).fold(f32::MIN, f32::max),
            size,
            // Nearly all letters: a bold run-in label doesn't make its line a heading.
            bold: letters > 0 && bold * 20 >= letters * 19,
            mono: letters > 0 && mono * 5 >= letters * 4,
            left: x0,
            right: x1,
            pieces,
        }
    }

    fn letters(&self) -> usize {
        self.text.chars().filter(|c| c.is_alphanumeric()).count()
    }

    /// Split a line that bridges the column gap `(gap_left, gap_right)` at
    /// the span boundary inside the gap, if there is one.
    fn split_at_gap(self, gap_left: f32, gap_right: f32) -> Result<(Line, Line), Line> {
        let at = (1..self.pieces.len()).find(|&i| {
            self.pieces[i - 1].x1 <= gap_left + 1.0 && self.pieces[i].x0 >= gap_right - 1.0
        });
        match at {
            Some(i) => {
                let mut left = self.pieces;
                let right = left.split_off(i);
                Ok((Line::new(left), Line::new(right)))
            }
            None => Err(self),
        }
    }
}

/// Group a page's spans into lines: spans on one baseline form a row, and a
/// wide horizontal gap splits a row into separate lines (columns, table cells).
fn lines(spans: &[TextSpan]) -> Vec<Line> {
    // Tagged PDFs mark running headers, footers and watermarks as artifacts;
    // rotated text is margin stamps and axis labels.
    let mut pieces: Vec<Piece> = spans
        .iter()
        .filter(|s| !s.text.trim().is_empty() && s.font_size > 0.0)
        .filter(|s| s.artifact_type.is_none())
        .filter(|s| {
            let r = s.rotation_degrees.rem_euclid(360.0);
            !(5.0..=355.0).contains(&r)
        })
        .map(Piece::of)
        .collect();
    pieces.sort_by(|a, b| b.y.total_cmp(&a.y));

    let mut rows: Vec<Vec<Piece>> = Vec::new();
    for p in pieces {
        match rows.last_mut() {
            Some(row) if (row[0].y - p.y).abs() <= 0.5 * row[0].size.max(p.size) => row.push(p),
            _ => rows.push(vec![p]),
        }
    }

    let mut out = Vec::new();
    for mut row in rows {
        row.sort_by(|a, b| a.x0.total_cmp(&b.x0));
        let mut segment: Vec<Piece> = Vec::new();
        for p in row {
            if let Some(prev) = segment.last()
                && p.x0 - prev.x1 > 1.5 * prev.size.max(p.size)
            {
                out.push(Line::new(std::mem::take(&mut segment)));
            }
            segment.push(p);
        }
        if !segment.is_empty() {
            out.push(Line::new(segment));
        }
    }
    out
}

/// Bold from the font name when it says, since some files report every font
/// as bold; otherwise from the reported weight.
fn is_bold(s: &TextSpan) -> bool {
    let name = s.font_name.to_ascii_lowercase();
    let style = name.rsplit_once('+').map_or(name.as_str(), |(_, n)| n);
    if ["bold", "black", "heavy", "semibold", "demi", "medi"]
        .iter()
        .any(|w| style.contains(w))
    {
        return true;
    }
    if ["regu", "roman", "book", "light", "normal"]
        .iter()
        .any(|w| style.contains(w))
    {
        return false;
    }
    s.font_weight as u16 >= 600
}

fn is_mono(s: &TextSpan) -> bool {
    let name = s.font_name.to_ascii_lowercase();
    s.is_monospace
        || ["mono", "courier", "consol", "menlo", "cmtt", "inconsolata"]
            .iter()
            .any(|w| name.contains(w))
}

/// Expand ligatures, drop soft hyphens and collapse whitespace.
fn clean_text(s: &str) -> String {
    let s = DOT_LEADER.replace_all(s, " … ");
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'ﬀ' => out.push_str("ff"),
            'ﬁ' => out.push_str("fi"),
            'ﬂ' => out.push_str("fl"),
            'ﬃ' => out.push_str("ffi"),
            'ﬄ' => out.push_str("ffl"),
            'ﬅ' | 'ﬆ' => out.push_str("st"),
            '\u{ad}' => {}
            '\u{2010}' | '\u{2011}' => out.push('-'),
            c => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Running headers, footers and page numbers are looked for among this many
/// lines at the top and at the bottom of each page.
const EDGE_LINES: usize = 3;

static PAGE_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(page\s*)?(#+|[ivxlc]+)(\s*(of|/)\s*#+)?$|^-\s*#+\s*-$").unwrap()
});

/// The arXiv identifier printed sideways in the margin of preprints.
static ARXIV_STAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^arXiv:\d{4}\.\d{4,5}(v\d+)?\s*\[").unwrap());

/// Remove running headers and footers: lines at the top or bottom of the page
/// that repeat (digits ignored) on many pages, bare page numbers there, and
/// arXiv stamps.
fn drop_running_lines(pages: &mut [Vec<Line>]) {
    let key = |l: &Line| {
        l.text
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect::<String>()
    };
    // Lines at the edges: a header or footer split into parts (title, date,
    // page) is several lines on the same row.
    let edges = |lines: &[Line]| -> Vec<bool> {
        let mut ys: Vec<f32> = lines.iter().map(|l| l.y).collect();
        ys.sort_by(f32::total_cmp);
        ys.dedup_by(|a, b| (*a - *b).abs() < 2.0);
        let low = ys.get(EDGE_LINES.min(ys.len()).saturating_sub(1)).copied();
        let high = ys
            .len()
            .checked_sub(EDGE_LINES)
            .and_then(|i| ys.get(i))
            .copied();
        lines
            .iter()
            .map(|l| low.is_some_and(|y| l.y <= y + 1.0) || high.is_some_and(|y| l.y >= y - 1.0))
            .collect()
    };

    let mut counts: HashMap<String, usize> = HashMap::new();
    for lines in pages.iter() {
        let edge = edges(lines);
        let keys: HashSet<String> = lines
            .iter()
            .zip(&edge)
            .filter(|(_, e)| **e)
            .map(|(l, _)| key(l))
            .collect();
        for k in keys {
            *counts.entry(k).or_default() += 1;
        }
    }
    let repeated = (pages.len() as f32 * 0.4).ceil().max(3.0) as usize;
    for lines in pages.iter_mut() {
        let edge = edges(lines);
        let mut i = 0;
        lines.retain(|l| {
            let at_edge = edge[i];
            i += 1;
            if ARXIV_STAMP.is_match(&l.text) {
                return false;
            }
            if !at_edge {
                return true;
            }
            let k = key(l);
            counts[&k] < repeated && !PAGE_NUMBER.is_match(&k)
        });
    }
}

/// Order a page's lines for reading. A two-column page is read down the left
/// column, then the right, in bands separated by lines that span both columns
/// (titles, wide figures and tables).
fn reading_order(mut lines: Vec<Line>) -> Vec<Line> {
    lines.sort_by(|a, b| b.y.total_cmp(&a.y).then(a.x0.total_cmp(&b.x0)));
    if lines.is_empty() {
        return lines;
    }
    let lo = lines.iter().map(|l| l.x0).fold(f32::MAX, f32::min);
    let hi = lines.iter().map(|l| l.x1).fold(f32::MIN, f32::max);

    let Some((gap_left, gap_right)) = find_gutter(&lines, lo, hi) else {
        for l in &mut lines {
            (l.left, l.right) = (lo, hi);
        }
        return lines;
    };
    let gutter = (gap_left + gap_right) / 2.0;
    // A line in each column on the same baseline can be read as one line
    // when the gap is narrow; split those back apart.
    let lines: Vec<Line> = lines
        .into_iter()
        .flat_map(|l| {
            if l.x0 < gutter && l.x1 > gutter {
                match l.split_at_gap(gap_left, gap_right) {
                    Ok((a, b)) => vec![a, b],
                    Err(l) => vec![l],
                }
            } else {
                vec![l]
            }
        })
        .collect();

    let mut out = Vec::with_capacity(lines.len());
    let (mut left, mut right): (Vec<Line>, Vec<Line>) = (Vec::new(), Vec::new());
    let flush = |out: &mut Vec<Line>, left: &mut Vec<Line>, right: &mut Vec<Line>| {
        out.append(left);
        out.append(right);
    };
    for mut l in lines {
        if l.x1 <= gutter {
            (l.left, l.right) = (lo, gutter);
            left.push(l);
        } else if l.x0 >= gutter {
            (l.left, l.right) = (gutter, hi);
            right.push(l);
        } else {
            flush(&mut out, &mut left, &mut right);
            (l.left, l.right) = (lo, hi);
            out.push(l);
        }
    }
    flush(&mut out, &mut left, &mut right);
    out
}

/// The gap between two text columns, as (right edge of the left column, left
/// edge of the right one), if the page has two: few lines cross it, and both
/// sides hold many lines as wide as a column of text.
fn find_gutter(lines: &[Line], lo: f32, hi: f32) -> Option<(f32, f32)> {
    let width = hi - lo;
    if width <= 0.0 || lines.len() < 8 {
        return None;
    }
    let n = lines.len();
    let mut best: Option<(usize, f32)> = None;
    let mut x = lo + width * 0.3;
    while x <= lo + width * 0.7 {
        let left = lines.iter().filter(|l| l.x1 <= x).count();
        let right = lines.iter().filter(|l| l.x0 >= x).count();
        let cross = n - left - right;
        let score = left.min(right);
        if cross * 100 <= n * 15 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, x));
        }
        x += 2.0;
    }
    let (score, x) = best?;
    if score < 4 || score * 5 < n {
        return None;
    }
    let left: Vec<&Line> = lines.iter().filter(|l| l.x1 <= x).collect();
    let right: Vec<&Line> = lines.iter().filter(|l| l.x0 >= x).collect();
    let left_edge = left.iter().map(|l| l.x1).fold(f32::MIN, f32::max);
    let right_edge = right.iter().map(|l| l.x0).fold(f32::MAX, f32::min);
    // Table cells also leave a vertical gap; text columns are mostly full width.
    let full = |side: &[&Line], col: f32| {
        let wide = side.iter().filter(|l| l.x1 - l.x0 >= col * 0.6).count();
        wide * 2 >= side.len()
    };
    (full(&left, left_edge - lo) && full(&right, hi - right_edge))
        .then_some((left_edge, right_edge))
}

/// Document-wide font statistics used to recognise headings.
struct Style {
    body_size: f32,
    body_bold: bool,
    body_mono: bool,
    /// Heading styles (size × 2, bold), largest first.
    heading_styles: Vec<(u32, bool)>,
    /// Level of each heading style used with section numbers, so an
    /// unnumbered "Abstract" or "References" sits beside "1 Introduction".
    numbered_levels: HashMap<(u32, bool), usize>,
}

static SECTION_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(\d{1,2}(?:\.\d{1,2})*)\.?|([A-Z](?:\.\d{1,2})+)\.?|Appendix\s+[A-Z0-9]+[.:]?)\s+\S",
    )
    .unwrap()
});

static CAPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(figure|fig\.|table|algorithm|listing)\s*[0-9A-Z]").unwrap()
});

/// Dot leaders in a table of contents: `Wages ........ 18`.
static DOT_LEADER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\s*\.){3,}\s*").unwrap());

static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[•◦▪‣●○■□·∙–—*]\s*").unwrap());

impl Style {
    fn of(pages: &[Vec<Line>]) -> Self {
        let mut sizes: HashMap<u32, usize> = HashMap::new();
        let (mut total, mut bold, mut mono) = (0, 0, 0);
        for l in pages.iter().flatten() {
            let n = l.letters();
            *sizes.entry((l.size * 2.0).round() as u32).or_default() += n;
            total += n;
            if l.bold {
                bold += n;
            }
            if l.mono {
                mono += n;
            }
        }
        let body_size = sizes
            .iter()
            .max_by_key(|&(size, n)| (n, size))
            .map_or(10.0, |(&size, _)| size as f32 / 2.0);
        let mut style = Style {
            body_size,
            body_bold: bold * 2 > total,
            body_mono: mono * 2 > total,
            heading_styles: Vec::new(),
            numbered_levels: HashMap::new(),
        };

        // A style used for much of the text is body text, not headings.
        let mut letters: HashMap<(u32, bool), usize> = HashMap::new();
        for l in pages
            .iter()
            .flatten()
            .filter(|l| style.looks_like_heading(l))
        {
            *letters.entry(style.key(l)).or_default() += l.letters();
        }
        let mut styles: Vec<(u32, bool)> = letters
            .into_iter()
            .filter(|&(_, n)| n * 100 <= total * 15)
            .map(|(k, _)| k)
            .collect();
        styles.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        style.heading_styles = styles;

        let mut numbered_levels = HashMap::new();
        for l in pages.iter().flatten().filter(|l| style.is_heading(l)) {
            if let Some(level) = numbered_level(&l.text) {
                numbered_levels.entry(style.key(l)).or_insert(level);
            }
        }
        style.numbered_levels = numbered_levels;
        style
    }

    fn key(&self, l: &Line) -> (u32, bool) {
        ((l.size * 2.0).round() as u32, l.bold)
    }

    fn larger(&self, l: &Line) -> bool {
        l.size >= self.body_size * 1.12 && l.size - self.body_size >= 1.0
    }

    /// Short, capitalised, and set larger than body text or in bold.
    fn looks_like_heading(&self, l: &Line) -> bool {
        let words = l.text.split_whitespace().count();
        let letters = l.text.chars().filter(|c| c.is_alphabetic()).count();
        if words == 0
            || words > 14
            || letters < 3
            || l.text.len() > 150
            || (l.mono && !self.body_mono)
            || l.text.ends_with([',', ';', '-'])
            || l.text.starts_with(|c: char| c.is_lowercase())
            || CAPTION.is_match(&l.text)
            || BULLET.is_match(&l.text)
            || l.text.contains(" … ")
        {
            return false;
        }
        let bold_only = l.bold && !self.body_bold && l.size >= self.body_size * 0.95;
        self.larger(l)
            || (bold_only
                && (SECTION_NUMBER.is_match(&l.text) || (words <= 10 && !l.text.ends_with('.'))))
    }

    fn is_heading(&self, l: &Line) -> bool {
        self.looks_like_heading(l) && self.heading_styles.contains(&self.key(l))
    }

    /// Markdown level: from the section number when there is one, else from
    /// how large the style is among the heading styles.
    fn level(&self, l: &Line) -> usize {
        if let Some(level) = numbered_level(&l.text) {
            return level;
        }
        if let Some(&level) = self.numbered_levels.get(&self.key(l)) {
            return level;
        }
        let rank = self
            .heading_styles
            .iter()
            .position(|&k| k == self.key(l))
            .unwrap_or(0);
        (2 + rank).min(6)
    }

    /// Footnote and citation markers raised above the line: a few digits or
    /// symbols in a small font.
    fn is_marker(&self, l: &Line) -> bool {
        l.size < self.body_size * 0.85
            && l.text.chars().count() <= 4
            && l.text
                .chars()
                .all(|c| c.is_ascii_digit() || ",*†‡§¶ ".contains(c))
    }
}

/// `3.2 Attention` → 3: `##` for top-level sections, one more per number part.
fn numbered_level(text: &str) -> Option<usize> {
    let c = SECTION_NUMBER.captures(text)?;
    Some(match c.get(1).or_else(|| c.get(2)) {
        Some(num) => (1 + num.as_str().split('.').count()).min(6),
        None => 2,
    })
}

/// Write the ordered lines as markdown: headings, paragraphs joined across
/// line, column and page breaks, bullet lists, monospace code blocks, and a
/// `[p.N]` marker before the first paragraph that starts on each page.
fn render(pages: &[Vec<Line>], style: &Style) -> (Option<String>, String) {
    let title_size = title_size(pages, style);
    let title = title_size.and_then(|size| {
        let text = pages[0]
            .iter()
            .filter(|l| is_title_line(l, size))
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        (!text.is_empty() && text.len() <= 250).then_some(text)
    });
    // The title goes in the page's header line, not the markdown, so it
    // doesn't prefix every heading path.
    let mut out = String::new();
    let compounds = compounds(pages);
    let mut para = String::new();
    let mut code: Vec<&Line> = Vec::new();
    let mut prev: Option<&Line> = None;
    let mut pending_page: Option<usize>;
    let mut heading: Option<(usize, String, &Line)> = None;

    let flush_para = |out: &mut String, para: &mut String, pending: &mut Option<usize>| {
        if !para.is_empty() {
            out.push_str(para);
            out.push_str("\n\n");
            para.clear();
        }
        if let Some(p) = pending.take() {
            out.push_str(&format!("[p.{p}]\n\n"));
        }
    };
    let flush_code = |out: &mut String, code: &mut Vec<&Line>| {
        if code.is_empty() {
            return;
        }
        let min_x = code.iter().map(|l| l.x0).fold(f32::MAX, f32::min);
        out.push_str("```\n");
        for l in code.iter() {
            let indent = ((l.x0 - min_x) / (l.size * 0.6)).round().max(0.0) as usize;
            out.push_str(&" ".repeat(indent.min(40)));
            out.push_str(&l.text);
            out.push('\n');
        }
        out.push_str("```\n\n");
        code.clear();
    };
    let flush_heading = |out: &mut String, heading: &mut Option<(usize, String, &Line)>| {
        if let Some((level, text, _)) = heading.take() {
            out.push_str(&format!("{} {text}\n\n", "#".repeat(level)));
        }
    };

    for (p, lines) in pages.iter().enumerate() {
        pending_page = Some(p + 1);
        let mut new_page = true;
        for l in lines {
            if p == 0 && title.is_some() && title_size.is_some_and(|size| is_title_line(l, size)) {
                continue;
            }
            if l.mono && !style.body_mono {
                flush_heading(&mut out, &mut heading);
                flush_para(&mut out, &mut para, &mut pending_page);
                code.push(l);
                prev = Some(l);
                new_page = false;
                continue;
            }
            flush_code(&mut out, &mut code);

            if style.is_heading(l) {
                // A heading that wraps onto a second line continues the first.
                if let Some((_, text, last)) = heading.as_mut()
                    && style.key(last) == style.key(l)
                    && !SECTION_NUMBER.is_match(&l.text)
                    && last.y - l.y <= 2.0 * l.size
                    && !new_page
                {
                    text.push(' ');
                    text.push_str(&l.text);
                    *last = l;
                } else {
                    flush_heading(&mut out, &mut heading);
                    flush_para(&mut out, &mut para, &mut pending_page);
                    heading = Some((style.level(l), l.text.clone(), l));
                }
                prev = Some(l);
                new_page = false;
                continue;
            }
            flush_heading(&mut out, &mut heading);

            let bullet = BULLET.find(&l.text);
            let continues = !para.is_empty()
                && bullet.is_none()
                && prev.is_some_and(|pl| continues_paragraph(pl, l, &para, new_page));
            if continues {
                join_line(&mut para, &l.text, &compounds);
            } else {
                flush_para(&mut out, &mut para, &mut pending_page);
                match bullet {
                    Some(m) => {
                        para.push_str("- ");
                        para.push_str(&l.text[m.end()..]);
                    }
                    None => para.push_str(&l.text),
                }
            }
            prev = Some(l);
            new_page = false;
        }
    }
    flush_code(&mut out, &mut code);
    flush_heading(&mut out, &mut heading);
    pending_page = None;
    flush_para(&mut out, &mut para, &mut pending_page);
    (title, out.trim_end().to_string())
}

/// Whether `next` carries on the paragraph that `prev` is in.
fn continues_paragraph(prev: &Line, next: &Line, para: &str, new_page: bool) -> bool {
    if (prev.size - next.size).abs() >= 1.0 {
        return false;
    }
    let ends_sentence = para.ends_with(['.', '!', '?', ':']);
    // Same row: another cell of a table row, or a split line.
    if !new_page && (prev.y - next.y).abs() < 0.5 * prev.size {
        return true;
    }
    // Moving to a new column or page: carry on mid-sentence only.
    if new_page || next.y > prev.y || next.left != prev.left {
        return !ends_sentence;
    }
    let gap = prev.y - next.y;
    let short_prev = prev.x1 < prev.right - 3.0 * prev.size;
    let indented = next.x0 - next.left > 0.8 * next.size && next.x0 > prev.x0 + 0.8 * next.size;
    gap <= 1.75 * prev.size && !(short_prev && ends_sentence) && !indented
}

/// Hyphenated words written within a line somewhere in the document
/// (`task-specific`), lowercased, to tell them from words split at a line end.
fn compounds(pages: &[Vec<Line>]) -> HashSet<String> {
    pages
        .iter()
        .flatten()
        .flat_map(|l| l.text.split_whitespace())
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| {
            w.split_once('-').is_some_and(|(a, b)| {
                a.ends_with(char::is_alphabetic) && b.starts_with(char::is_alphabetic)
            })
        })
        .map(str::to_lowercase)
        .collect()
}

/// Append a line to a paragraph, rejoining words hyphenated at the line break.
fn join_line(para: &mut String, text: &str, compounds: &HashSet<String>) {
    let next_lower = text.starts_with(|c: char| c.is_lowercase());
    if next_lower && para.ends_with('-') {
        let word = para.rsplit(' ').next().unwrap_or("");
        let stem = &word[..word.len() - 1];
        if stem.ends_with(char::is_alphabetic) {
            // `representa-` + `tion` loses its hyphen. `state-of-the-` + `art`
            // keeps it, as does `task-` + `specific` when the document writes
            // `task-specific` elsewhere.
            let next_word: String = text.chars().take_while(|c| c.is_alphabetic()).collect();
            let joined = format!("{stem}-{next_word}").to_lowercase();
            let joined = joined.trim_start_matches(|c: char| !c.is_alphanumeric());
            // `English-` + `to-German` is a compound too.
            let next_compound = text
                .split_whitespace()
                .next()
                .is_some_and(|w| w.contains('-'));
            if !stem.contains('-') && !next_compound && !compounds.contains(joined) {
                para.pop();
            }
            para.push_str(text);
            return;
        }
    }
    // A URL broken after a slash carries on without a space.
    if !para.ends_with('/') {
        para.push(' ');
    }
    para.push_str(text);
}

/// The title's font size: the largest text on the first page, if it is
/// clearly larger than body text.
fn title_size(pages: &[Vec<Line>], style: &Style) -> Option<f32> {
    let max = pages
        .first()?
        .iter()
        .filter(|l| l.letters() >= 3)
        .map(|l| l.size)
        .fold(0.0_f32, f32::max);
    (max >= style.body_size * 1.3).then_some(max)
}

fn is_title_line(l: &Line, title_size: f32) -> bool {
    (l.size - title_size).abs() < 0.5 && l.letters() >= 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piece(text: &str, x0: f32, y: f32, size: f32) -> Piece {
        Piece {
            text: text.into(),
            x0,
            x1: x0 + text.len() as f32 * size * 0.5,
            y,
            size,
            bold: false,
            mono: false,
        }
    }

    fn line(text: &str, x0: f32, y: f32, size: f32) -> Line {
        Line::new(vec![piece(text, x0, y, size)])
    }

    #[test]
    fn hyphenated_words_are_rejoined() {
        let compounds: HashSet<String> = ["task-specific".to_string()].into();
        let join = |a: &str, b: &str| {
            let mut p = a.to_string();
            join_line(&mut p, b, &compounds);
            p
        };
        assert_eq!(
            join("a new representa-", "tion model"),
            "a new representation model"
        );
        assert_eq!(
            join("the state-of-the-", "art result"),
            "the state-of-the-art result"
        );
        assert_eq!(
            join("many (task-", "specific) models"),
            "many (task-specific) models"
        );
        assert_eq!(join("see 1999-", "2001"), "see 1999- 2001");
        assert_eq!(
            join("WMT English-", "to-German task"),
            "WMT English-to-German task"
        );
        assert_eq!(
            join("at https://github.com/", "google/bert."),
            "at https://github.com/google/bert."
        );
    }

    #[test]
    fn two_columns_are_read_one_after_the_other() {
        let mut lines = vec![line(
            "A Title Spanning The Whole Page Width Here",
            50.0,
            760.0,
            14.0,
        )];
        for i in 0..10 {
            let y = 700.0 - i as f32 * 12.0;
            lines.push(line(
                &format!("left column line {i:02} text text"),
                50.0,
                y,
                10.0,
            ));
            lines.push(line(
                &format!("right column line {i:02} text tex"),
                320.0,
                y,
                10.0,
            ));
        }
        // A right-column heading on a left-column baseline, merged into one line.
        let mut heading = piece("2 Heading", 320.0, 712.0, 12.0);
        heading.bold = true;
        lines.push(Line::new(vec![
            piece("left column line 99 text text", 50.0, 712.0, 10.0),
            heading,
        ]));
        let ordered = reading_order(lines);
        let texts: Vec<&str> = ordered.iter().map(|l| l.text.as_str()).collect();
        assert!(texts[0].starts_with("A Title"));
        assert!(
            texts[1..12].iter().all(|t| t.starts_with("left")),
            "{texts:?}"
        );
        assert_eq!(texts[12], "2 Heading");
        assert!(texts[13..].iter().all(|t| t.starts_with("right")));
    }

    #[test]
    fn running_headers_and_page_numbers_are_dropped() {
        let mut pages: Vec<Vec<Line>> = (1..=4)
            .map(|p| {
                let mut lines = vec![
                    line("RFC 9110 HTTP Semantics", 50.0, 796.0, 9.0),
                    line("June 2022", 400.0, 796.0, 9.0),
                    line(
                        &format!("Fielding, et al. Standards Track Page {p}"),
                        50.0,
                        108.0,
                        9.0,
                    ),
                ];
                let words = ["alpha", "beta", "gamma", "delta"];
                for i in 0..8 {
                    let text = format!("Body line {i} of page {}.", words[p - 1]);
                    lines.push(line(&text, 50.0, 700.0 - i as f32 * 60.0, 10.0));
                }
                lines
            })
            .collect();
        drop_running_lines(&mut pages);
        assert!(
            pages
                .iter()
                .all(|p| p.len() == 8 && p.iter().all(|l| l.text.starts_with("Body")))
        );
    }

    #[test]
    fn headings_paragraphs_and_page_markers() {
        let mut h1 = line("1 Introduction", 50.0, 700.0, 12.0);
        h1.bold = true;
        let mut h2 = line("1.1 Background", 50.0, 400.0, 10.0);
        h2.bold = true;
        let body = |t: &str, y| {
            let mut l = line(t, 50.0, y, 10.0);
            l.x1 = 550.0;
            l
        };
        let pages = vec![
            vec![
                h1,
                body("Crawlers read pages and the text of this para-", 680.0),
                body("graph runs over two lines without a break", 668.0),
                h2,
                body("Background text carries on to the next page and", 380.0),
            ],
            vec![
                body("ends here.", 700.0),
                body("A new paragraph starts.", 680.0),
            ],
        ];
        let pages: Vec<Vec<Line>> = pages.into_iter().map(reading_order).collect();
        let mut style = Style::of(&pages);
        style.body_size = 10.0;
        let (_, md) = render(&pages, &style);
        assert_eq!(
            md,
            "[p.1]\n\n## 1 Introduction\n\n\
             Crawlers read pages and the text of this paragraph runs over two lines without a break\n\n\
             ### 1.1 Background\n\n\
             Background text carries on to the next page and ends here.\n\n\
             [p.2]\n\nA new paragraph starts."
        );
    }
}
