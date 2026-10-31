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
    let gutter = document_gutter(&pages);
    let pages: Vec<Vec<Line>> = pages
        .into_iter()
        .map(|p| p.into_iter().filter(|l| !style.is_marker(l)).collect())
        .map(|p| reading_order(tables(p, gutter)))
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
    /// A whole table, already written as markdown in `text` (see `tables`).
    table: bool,
}

impl Line {
    fn new(pieces: Vec<Piece>) -> Self {
        // Letters per font size, to find the line's dominant size.
        let mut sizes: HashMap<u32, usize> = HashMap::new();
        let (mut letters, mut bold, mut mono) = (0, 0, 0);
        for p in &pieces {
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

        let mut text = String::new();
        let mut prev_end: Option<f32> = None;
        // Baseline of the last full-size span, and the marker of the script
        // being written (`^` or `_`), so `1`+`9` raised gives `^19`, not `^1^9`.
        let mut baseline: Option<f32> = None;
        let mut script: Option<char> = None;
        for p in &pieces {
            if let Some(end) = prev_end
                && is_script(p, size, end, &text, script.is_some())
            {
                // Lowered spans have a lower y; raised ones often report the
                // baseline's y, so anything not lowered counts as raised.
                let marker = match baseline {
                    Some(y) if p.y < y - 0.1 * size => '_',
                    _ => '^',
                };
                if script != Some(marker) {
                    text.push(marker);
                }
                text.push_str(p.text.trim());
                script = Some(marker);
                prev_end = Some(p.x1);
                continue;
            }
            // A span ending in a ligature (`Specifi` + `cations`) is often
            // reported a little short; that gap is not a space.
            let ligature_gap = text.ends_with(['ﬁ', 'ﬂ', 'ﬀ', 'ﬃ', 'ﬄ'])
                || (["fi", "fl", "ff"].iter().any(|l| text.ends_with(l))
                    && p.text.starts_with(|c: char| c.is_lowercase()));
            if let Some(end) = prev_end
                // After a script, a letter or digit starts a new word:
                // `d_k` + `queries` must not read as `d_kqueries`.
                && (p.x0 - end > 0.15 * p.size
                    || (script.is_some() && p.text.starts_with(char::is_alphanumeric)))
                && !(ligature_gap && p.x0 - end < 0.6 * p.size)
                && !text.ends_with(char::is_whitespace)
                && !p.text.starts_with(char::is_whitespace)
            {
                text.push(' ');
            }
            text.push_str(&p.text);
            prev_end = Some(p.x1);
            script = None;
            if p.size > 0.8 * size {
                baseline = Some(p.y);
            }
        }
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
            table: false,
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

/// A superscript or subscript: a short span in a clearly smaller font that
/// touches the end of the text before it, like the `20` of `10²⁰` or the
/// `model` of `d_model`. A sign may start one (`10` + `−` + `9`). Small
/// capitals (`R` + `AFT`) are not scripts.
fn is_script(p: &Piece, line_size: f32, prev_end: f32, text: &str, continuing: bool) -> bool {
    let t = p.text.trim();
    let chars = t.chars().count();
    let small_caps = text.ends_with(|c: char| c.is_uppercase())
        && t.chars().all(|c| c.is_uppercase() || !c.is_alphabetic());
    p.size <= 0.8 * line_size
        && (p.x0 - prev_end).abs() < 0.2 * line_size
        && (1..=8).contains(&chars)
        && !t.contains(char::is_whitespace)
        && (t.chars().any(char::is_alphanumeric) || t.chars().all(|c| "−-+".contains(c)))
        && (continuing || text.ends_with(|c: char| c.is_alphanumeric() || ")]}".contains(c)))
        && !small_caps
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
            if l.x0 < gutter && l.x1 > gutter && !l.table {
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

/// Cells of a table row are separated by a gap wider than this, in ems.
/// Narrower than the gap that splits a row into lines (1.5), since table
/// columns can sit closer than text columns.
const CELL_GAP: f32 = 1.0;

/// A caption line: where it is, and whether it is a figure's.
struct Caption {
    y: f32,
    x0: f32,
    x1: f32,
    figure: bool,
}

/// One row of a page: the lines on one baseline, split into cells.
struct Row {
    y: f32,
    size: f32,
    cells: Vec<Line>,
    /// The lines as they were, given back if the row isn't part of a table.
    lines: Vec<Line>,
    /// A line of running text in a text column: never part of a table.
    prose: bool,
}

/// Find tables among a page's lines and replace each with one line holding
/// it as a markdown table. A table is a run of rows, most of them with
/// several cells, whose cells line up in columns.
///
/// On a two-column page, a line from each column on the same baseline would
/// look like a table row, so tables across the page are found among rows
/// that have no full line of column text, then tables inside each column.
/// The page's own column gap is used, else the document's (`document_gutter`):
/// a page with a table in one column often has too few full lines to show one.
fn tables(lines: Vec<Line>, document: Option<(f32, f32)>) -> Vec<Line> {
    let lo = lines.iter().map(|l| l.x0).fold(f32::MAX, f32::min);
    let hi = lines.iter().map(|l| l.x1).fold(f32::MIN, f32::max);
    let captions: Vec<Caption> = lines
        .iter()
        .filter_map(|l| {
            CAPTION.captures(&l.text).map(|c| Caption {
                y: l.y,
                x0: l.x0,
                x1: l.x1,
                figure: c[1].to_lowercase().starts_with("fig"),
            })
        })
        .collect();
    // A table of two narrow columns can look like two text columns; a page's
    // own gap counts only if the lines beside it are running text.
    let page = find_gutter(&lines, lo, hi).filter(|&(l, r)| beside_is_text(&lines, l, r));
    let gutter = page.or(document);
    let lines: Vec<Line> = match gutter {
        // A line from each column on one baseline can have been read as one
        // line. Split at the gap's middle: a table may reach into the gap.
        Some((gap_left, gap_right)) => {
            let middle = (gap_left + gap_right) / 2.0;
            lines
                .into_iter()
                .flat_map(|l| {
                    if l.x0 < middle && l.x1 > middle {
                        match l.split_at_gap(middle, middle) {
                            Ok((a, b)) => vec![a, b],
                            Err(l) => vec![l],
                        }
                    } else {
                        vec![l]
                    }
                })
                .collect()
        }
        None => lines,
    };

    // A table is set in one font size. Looking at each size apart keeps a
    // figure's small axis labels beside a table out of its rows.
    let mut lines = lines;
    lines.sort_by(|a, b| a.size.total_cmp(&b.size));
    let mut out = Vec::new();
    let mut group: Vec<Line> = Vec::new();
    for l in lines {
        if group.last().is_some_and(|g| l.size > g.size * 1.12) {
            out.extend(tables_of_size(
                std::mem::take(&mut group),
                gutter,
                lo,
                hi,
                &captions,
            ));
        }
        group.push(l);
    }
    out.extend(tables_of_size(group, gutter, lo, hi, &captions));
    out
}

/// `tables` for lines of one font size: on a two-column page, tables across
/// the page, then tables inside each column.
fn tables_of_size(
    lines: Vec<Line>,
    gutter: Option<(f32, f32)>,
    lo: f32,
    hi: f32,
    captions: &[Caption],
) -> Vec<Line> {
    let Some((gap_left, gap_right)) = gutter else {
        return find_tables(lines, |_| false, captions, None);
    };
    let middle = (gap_left + gap_right) / 2.0;
    // One cell as wide as most of a column, on either side.
    let prose = |cells: &[Line]| {
        let side = |keep: &dyn Fn(&Line) -> bool, width: f32| {
            let side: Vec<&Line> = cells.iter().filter(|c| keep(c)).collect();
            side.len() == 1 && side[0].x1 - side[0].x0 >= 0.6 * width
        };
        side(&|c| c.x1 <= middle, gap_left - lo) || side(&|c| c.x0 >= middle, hi - gap_right)
    };
    let found = find_tables(lines, prose, captions, Some(middle));
    let (done, rest): (Vec<Line>, Vec<Line>) = found.into_iter().partition(|l| l.table);
    let (left, rest): (Vec<Line>, Vec<Line>) = rest.into_iter().partition(|l| l.x1 <= middle);
    let (right, across): (Vec<Line>, Vec<Line>) = rest.into_iter().partition(|l| l.x0 >= middle);
    let mut out = done;
    out.extend(find_tables(left, |_| false, captions, None));
    out.extend(find_tables(right, |_| false, captions, None));
    out.extend(across);
    out
}

/// Whether the lines on both sides of a column gap average 25 or more
/// characters, as lines of running text do and table cells mostly don't.
fn beside_is_text(lines: &[Line], gap_left: f32, gap_right: f32) -> bool {
    let average = |side: Vec<&Line>| {
        side.iter().map(|l| l.text.chars().count()).sum::<usize>() / side.len().max(1)
    };
    average(lines.iter().filter(|l| l.x1 <= gap_left + 1.0).collect()) >= 25
        && average(lines.iter().filter(|l| l.x0 >= gap_right - 1.0).collect()) >= 25
}

/// The column gap of a two-column document: the one most pages show, if at
/// least a third of them show one.
fn document_gutter(pages: &[Vec<Line>]) -> Option<(f32, f32)> {
    let mut gutters: Vec<(f32, f32)> = pages
        .iter()
        .filter_map(|lines| {
            let lo = lines.iter().map(|l| l.x0).fold(f32::MAX, f32::min);
            let hi = lines.iter().map(|l| l.x1).fold(f32::MIN, f32::max);
            find_gutter(lines, lo, hi)
        })
        .collect();
    if gutters.is_empty() || gutters.len() * 3 < pages.len() {
        return None;
    }
    gutters.sort_by(|a, b| (a.0 + a.1).total_cmp(&(b.0 + b.1)));
    Some(gutters[gutters.len() / 2])
}

/// `tables` for one set of lines. `prose` tells, from a row's cells, that
/// the row is running text. `captions` are the page's caption lines: a grid
/// whose nearest caption is a figure's is the labels of a diagram, not a
/// table. With `across`, only tables that
/// cross that x are kept: a table inside one column is left for that
/// column's own pass, which sees all of its rows.
fn find_tables(
    mut lines: Vec<Line>,
    prose: impl Fn(&[Line]) -> bool,
    captions: &[Caption],
    across: Option<f32>,
) -> Vec<Line> {
    lines.sort_by(|a, b| b.y.total_cmp(&a.y).then(a.x0.total_cmp(&b.x0)));
    let mut rows: Vec<Row> = Vec::new();
    for l in lines {
        match rows.last_mut() {
            Some(row) if (row.y - l.y).abs() <= 0.5 * row.size.max(l.size) => row.lines.push(l),
            _ => rows.push(Row {
                y: l.y,
                size: l.size,
                cells: Vec::new(),
                lines: vec![l],
                prose: false,
            }),
        }
    }
    for row in &mut rows {
        row.cells = row.lines.iter().flat_map(cells).collect();
        row.cells.sort_by(|a, b| a.x0.total_cmp(&b.x0));
        row.prose = prose(&row.cells);
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let end = table_end(&rows, i);
        // The nearest caption above or below, among those over the same
        // part of the page (a figure in the other column doesn't count).
        let figure = end > i && {
            let (top, bottom) = (rows[i].y, rows[end - 1].y);
            let x0 = rows[i..end]
                .iter()
                .flat_map(|r| &r.cells)
                .map(|c| c.x0)
                .fold(f32::MAX, f32::min);
            let x1 = rows[i..end]
                .iter()
                .flat_map(|r| &r.cells)
                .map(|c| c.x1)
                .fold(f32::MIN, f32::max);
            let distance = |y: f32| {
                if y > top {
                    y - top
                } else if y < bottom {
                    bottom - y
                } else {
                    0.0
                }
            };
            captions
                .iter()
                .filter(|c| c.x0 < x1 && c.x1 > x0)
                .min_by(|a, b| distance(a.y).total_cmp(&distance(b.y)))
                .is_some_and(|c| c.figure)
        };
        let table = (end > i && !figure)
            .then(|| table(&rows[i..end]))
            .flatten()
            .filter(|t| across.is_none_or(|x| t.x0 < x && t.x1 > x));
        if let Some(table) = table {
            out.push(table);
            i = end;
        } else {
            out.append(&mut rows[i].lines);
            i += 1;
        }
    }
    out
}

static NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[-+−±]?[0-9][0-9.,]*%?|[-–—])$").unwrap());

/// A span of several numbers (`76.5 79.0 70.9`), as one piece per number,
/// placed by its position in the text. Some files write a whole row of
/// table values as one span.
fn split_numbers(p: &Piece) -> Vec<Piece> {
    let words: Vec<(usize, &str)> = p
        .text
        .split_whitespace()
        .map(|w| (w.as_ptr() as usize - p.text.as_ptr() as usize, w))
        .collect();
    if words.len() < 2 || !words.iter().all(|(_, w)| NUMBER.is_match(w)) {
        return vec![p.clone()];
    }
    let per_byte = (p.x1 - p.x0) / p.text.len().max(1) as f32;
    words
        .iter()
        .map(|&(at, w)| Piece {
            text: w.to_string(),
            x0: p.x0 + at as f32 * per_byte,
            x1: p.x0 + (at + w.len()) as f32 * per_byte,
            ..p.clone()
        })
        .collect()
}

/// Split a line into cells at gaps wider than `CELL_GAP`.
fn cells(l: &Line) -> Vec<Line> {
    let mut out = Vec::new();
    let mut cell: Vec<Piece> = Vec::new();
    for p in &l.pieces {
        // Numbers split out of one span are each a cell of their own.
        let numbers = split_numbers(p);
        if numbers.len() > 1 {
            if !cell.is_empty() {
                out.push(Line::new(std::mem::take(&mut cell)));
            }
            out.extend(numbers.into_iter().map(|n| Line::new(vec![n])));
            continue;
        }
        if let Some(prev) = cell.last()
            && p.x0 - prev.x1 > CELL_GAP * prev.size.max(p.size)
        {
            out.push(Line::new(std::mem::take(&mut cell)));
        }
        cell.push(p.clone());
    }
    if !cell.is_empty() {
        out.push(Line::new(cell));
    }
    out
}

/// The end (exclusive) of the table starting at row `start`, or `start` if
/// none starts there: at least 3 rows with several cells, rows close
/// together, mostly multi-cell, not ending on a single-cell row.
fn table_end(rows: &[Row], start: usize) -> usize {
    let multi = |r: &Row| r.cells.len() >= 2 && !r.prose;
    let caption = |r: &Row| CAPTION.is_match(&r.cells[0].text);
    if !multi(&rows[start]) || caption(&rows[start]) {
        return start;
    }
    let mut end = start + 1;
    while end < rows.len() {
        let (prev, next) = (&rows[end - 1], &rows[end]);
        let size = prev.size.max(next.size);
        if prev.y - next.y > 2.5 * size {
            break;
        }
        // A caption or a row of running text ends the table.
        if caption(next)
            || next.prose
            || (!multi(next) && next.cells.iter().any(|c| c.text.chars().count() > 90))
        {
            break;
        }
        end += 1;
    }
    while end > start && !multi(&rows[end - 1]) {
        end -= 1;
    }
    let n_multi = rows[start..end].iter().filter(|r| multi(r)).count();
    if n_multi >= 3 && n_multi * 10 >= (end - start) * 6 {
        end
    } else {
        start
    }
}

/// Column bands: the x-ranges of the table's columns, left to right. Cells
/// are placed narrowest first, and a cell that would join two columns (a
/// header over several) is left out, so it doesn't merge them.
fn columns(rows: &[Row]) -> Vec<(f32, f32)> {
    let mut cells: Vec<(f32, f32)> = rows
        .iter()
        .flat_map(|r| r.cells.iter().map(|c| (c.x0, c.x1)))
        .collect();
    cells.sort_by(|a, b| (a.1 - a.0).total_cmp(&(b.1 - b.0)));
    let mut bands: Vec<(f32, f32)> = Vec::new();
    for (x0, x1) in cells {
        let hit: Vec<usize> = (0..bands.len())
            .filter(|&b| x0 < bands[b].1 && x1 > bands[b].0)
            .collect();
        match hit.as_slice() {
            [] => bands.push((x0, x1)),
            [b] => {
                let grown = (bands[*b].0.min(x0), bands[*b].1.max(x1));
                let joins = (0..bands.len())
                    .any(|o| o != *b && grown.0 < bands[o].1 && grown.1 > bands[o].0);
                if !joins {
                    bands[*b] = grown;
                }
            }
            _ => {}
        }
    }
    bands.sort_by(|a, b| a.0.total_cmp(&b.0));
    bands
}

/// The columns a cell covers: those it overlaps, or else the nearest one.
fn covered(c: &Line, bands: &[(f32, f32)]) -> Vec<usize> {
    let hit: Vec<usize> = (0..bands.len())
        .filter(|&b| c.x0 < bands[b].1 && c.x1 > bands[b].0)
        .collect();
    if !hit.is_empty() {
        return hit;
    }
    let mid = (c.x0 + c.x1) / 2.0;
    let dist = |b: &(f32, f32)| (mid - (b.0 + b.1) / 2.0).abs();
    (0..bands.len())
        .min_by(|&a, &b| dist(&bands[a]).total_cmp(&dist(&bands[b])))
        .into_iter()
        .collect()
}

/// A cell covering several columns, split where its spans fall into
/// different columns; unchanged if a span itself crosses a boundary.
fn split_cell(c: &Line, bands: &[(f32, f32)]) -> Vec<Line> {
    let cover = covered(c, bands).len();
    if cover < 2 {
        return vec![c.clone()];
    }
    // One word per column (`boat bottle` over two): a header written as one
    // span. Each word must sit over its own column; a group header such as
    // `BLEU score` is centred across the boundary instead.
    if c.pieces.len() == 1 && c.text.split_whitespace().count() == cover {
        let p = &c.pieces[0];
        let per_byte = (p.x1 - p.x0) / p.text.len().max(1) as f32;
        let words: Vec<Piece> = p
            .text
            .split_whitespace()
            .map(|w| {
                let at = w.as_ptr() as usize - p.text.as_ptr() as usize;
                Piece {
                    text: w.to_string(),
                    x0: p.x0 + at as f32 * per_byte,
                    x1: p.x0 + (at + w.len()) as f32 * per_byte,
                    ..p.clone()
                }
            })
            .collect();
        let own_column = |(i, w): (usize, &Piece)| {
            let mid = (w.x0 + w.x1) / 2.0;
            bands.iter().position(|b| mid > b.0 && mid < b.1) == Some(i)
        };
        let first = covered(c, bands)[0];
        if words
            .iter()
            .enumerate()
            .all(|(i, w)| own_column((first + i, w)))
        {
            return words.into_iter().map(|w| Line::new(vec![w])).collect();
        }
    }
    let band_of = |p: &Piece| -> Option<usize> {
        let hit: Vec<usize> = (0..bands.len())
            .filter(|&b| p.x0 < bands[b].1 && p.x1 > bands[b].0)
            .collect();
        (hit.len() == 1).then(|| hit[0])
    };
    let Some(owners) = c.pieces.iter().map(band_of).collect::<Option<Vec<usize>>>() else {
        return vec![c.clone()];
    };
    let mut out: Vec<Line> = Vec::new();
    let mut group: Vec<Piece> = Vec::new();
    for (i, p) in c.pieces.iter().enumerate() {
        if i > 0 && owners[i] != owners[i - 1] {
            out.push(Line::new(std::mem::take(&mut group)));
        }
        group.push(p.clone());
    }
    out.push(Line::new(group));
    out
}

/// Mostly digits: a value, not a label.
fn numeric(text: &str) -> bool {
    let digits = text.chars().filter(char::is_ascii_digit).count();
    digits > 0 && digits >= text.chars().filter(|c| c.is_alphabetic()).count()
}

/// Write rows as a markdown table, or `None` if they don't form one.
fn table(rows: &[Row]) -> Option<Line> {
    let bands = columns(rows);
    let all: Vec<&Line> = rows.iter().flat_map(|r| &r.cells).collect();
    let mut lengths: Vec<usize> = all.iter().map(|c| c.text.chars().count()).collect();
    lengths.sort_unstable();
    // Two columns of long cells are more likely two columns of prose.
    if bands.len() < 2 || (bands.len() == 2 && lengths[lengths.len() / 2] > 30) {
        return None;
    }

    // Cells that cross a column boundary at a gap between their spans are
    // two cells set closer than `CELL_GAP` (`EN-DE EN-FR`): split them.
    let cells: Vec<Vec<Line>> = rows
        .iter()
        .map(|r| r.cells.iter().flat_map(|c| split_cell(c, &bands)).collect())
        .collect();
    // Cell texts per column. Header cells spanning several columns are
    // repeated in each (`BLEU EN-DE`, `BLEU EN-FR`); body cells go in the first.
    let grid = |r: usize, header: bool| -> Vec<String> {
        let mut cols = vec![String::new(); bands.len()];
        for c in &cells[r] {
            let cover = covered(c, &bands);
            // A header over most of the row is a title, not a column group.
            let group = header && cover.len() <= 3 && cover.len() < bands.len();
            let cover = if group { &cover[..] } else { &cover[..1] };
            for &b in cover {
                if !cols[b].is_empty() {
                    cols[b].push(' ');
                }
                cols[b].push_str(&c.text);
            }
        }
        cols
    };
    // Header: the rows before the first one that is at least half numbers,
    // if the table has numbers (`×10^6` under `params` is a unit, not a
    // value); else the first row.
    let first_value = cells.iter().position(|r| {
        let n = r.iter().filter(|c| numeric(&c.text)).count();
        n > 0 && n * 2 >= r.len()
    });
    // Text wrapped inside a cell: in a table whose rows are spaced apart, a
    // row as close to the one above as the tightest line spacing carries on
    // its cells (`Tier 1:` / `Partial`). Also a row with an empty first
    // column that is closer than rows usually are.
    let gaps: Vec<f32> = rows.windows(2).map(|w| w[0].y - w[1].y).collect();
    let mut sorted = gaps.clone();
    sorted.sort_by(f32::total_cmp);
    let (tightest, usual) = (sorted[0], sorted[sorted.len() / 2]);
    let spaced = sorted[sorted.len() - 1] > 1.3 * tightest;
    let wrapped = |i: usize, first_empty: bool| {
        let gap = gaps[i - 1];
        (spaced && gap < 1.15 * tightest) || (first_empty && gap < 0.85 * usual)
    };
    let append = |into: &mut Vec<String>, cols: Vec<String>| {
        for (l, c) in into.iter_mut().zip(cols) {
            if !c.is_empty() {
                if !l.is_empty() {
                    l.push(' ');
                }
                l.push_str(&c);
            }
        }
    };

    // Header: the rows before the first that is mostly values, or else the
    // first row with the rows wrapped under it.
    let header_rows = match first_value {
        Some(n) if (1..=4).contains(&n) => n,
        _ => (1..rows.len())
            .find(|&i| !wrapped(i, grid(i, false)[0].is_empty()))
            .unwrap_or(rows.len()),
    };
    if header_rows == rows.len() {
        return None;
    }
    let mut header = vec![String::new(); bands.len()];
    for r in 0..header_rows {
        append(&mut header, grid(r, true));
    }

    // A record ends at a dot leader (`Form W-4 … 25`), and a row with a
    // value in a column where the record already has one starts a new record.
    let both_values =
        |a: &[String], b: &[String]| a.iter().zip(b).any(|(a, b)| numeric(a) && numeric(b));
    let mut body: Vec<Vec<String>> = Vec::new();
    for i in header_rows..rows.len() {
        let cols = grid(i, false);
        let closed = grid(i - 1, false).iter().any(|c| c.ends_with('…'));
        match body.last_mut() {
            Some(last)
                if wrapped(i, cols[0].is_empty()) && !closed && !both_values(last, &cols) =>
            {
                append(last, cols)
            }
            _ => body.push(cols),
        }
    }
    // A column of bullets is a list, not a table.
    let first: Vec<&str> = body
        .iter()
        .map(|r| r[0].as_str())
        .filter(|c| !c.is_empty())
        .collect();
    if !first.is_empty()
        && first
            .iter()
            .all(|c| BULLET.replace(c, "").trim().is_empty())
    {
        return None;
    }

    let row_md = |cols: &[String]| {
        let cells: Vec<String> = cols.iter().map(|c| c.replace('|', "\\|")).collect();
        format!("| {} |", cells.join(" | "))
    };
    let mut md = row_md(&header);
    md.push('\n');
    md.push_str(&format!("|{}", " --- |".repeat(bands.len())));
    for cols in &body {
        md.push('\n');
        md.push_str(&row_md(cols));
    }

    let x0 = all.iter().map(|c| c.x0).fold(f32::MAX, f32::min);
    let x1 = all.iter().map(|c| c.x1).fold(f32::MIN, f32::max);
    Some(Line {
        text: md,
        x0,
        x1,
        y: rows[0].y,
        size: rows[0].size,
        bold: false,
        mono: false,
        left: x0,
        right: x1,
        pieces: Vec::new(),
        table: true,
    })
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
            if l.table {
                flush_code(&mut out, &mut code);
                flush_heading(&mut out, &mut heading);
                flush_para(&mut out, &mut para, &mut pending_page);
                out.push_str(&l.text);
                out.push_str("\n\n");
                prev = Some(l);
                new_page = false;
                continue;
            }
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
            // A caption starts its own paragraph, even after a colon.
            let continues = !para.is_empty()
                && bullet.is_none()
                && !CAPTION.is_match(&l.text)
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
    fn superscripts_and_subscripts_are_marked() {
        // `1.0 · 10²⁰`: the exponent reports the baseline's y.
        let power = Line::new(vec![
            piece("1.0 · 10", 100.0, 500.0, 10.0),
            piece("20", 145.0, 500.0, 7.0),
        ]);
        assert_eq!(power.text, "1.0 · 10^20");
        // `10⁻⁹`: a sign starts the exponent.
        let negative = Line::new(vec![
            piece("ϵ = 10", 100.0, 500.0, 10.0),
            piece("−", 135.0, 503.0, 7.0),
            piece("9", 145.5, 503.0, 7.0),
        ]);
        assert_eq!(negative.text, "ϵ = 10^−9");
        // `d_k` sits below the baseline; the word after it is separate.
        let sub = Line::new(vec![
            piece("dimension d", 100.0, 500.0, 10.0),
            piece("k", 155.0, 498.5, 7.0),
            piece("queries", 158.5, 500.0, 10.0),
        ]);
        assert_eq!(sub.text, "dimension d_k queries");
        // Small capitals are not scripts, nor is a gap before a small span.
        let caps = Line::new(vec![
            piece("R", 100.0, 500.0, 10.0),
            piece("AFT", 105.0, 500.0, 7.0),
        ]);
        assert_eq!(caps.text, "RAFT");
        let apart = Line::new(vec![
            piece("Note", 100.0, 500.0, 10.0),
            piece("3", 130.0, 500.0, 7.0),
        ]);
        assert_eq!(apart.text, "Note 3");
    }

    /// A row of cells on one baseline, as `lines` would give it.
    fn row(cells: &[(&str, f32)], y: f32) -> Vec<Line> {
        cells.iter().map(|&(t, x)| line(t, x, y, 10.0)).collect()
    }

    #[test]
    fn tables_become_markdown_tables() {
        let mut lines = vec![line("Table 1: Results on the test set.", 50.0, 720.0, 10.0)];
        // Group headers over two columns each: BLEU over 180..255, cost over 300..375.
        lines.extend(row(
            &[("BLEU score", 195.0), ("Training Cost", 305.0)],
            700.0,
        ));
        let header = [
            ("Model", 50.0),
            ("EN-DE", 180.0),
            ("EN-FR", 230.0),
            ("EN-DE", 300.0),
            ("EN-FR", 350.0),
        ];
        lines.extend(row(&header, 690.0));
        lines.extend(row(&[("ByteNet", 50.0), ("23.75", 180.0)], 678.0));
        lines.extend(row(
            &[
                ("Transformer", 50.0),
                ("28.4", 180.0),
                ("41.8", 230.0),
                ("2.3", 300.0),
            ],
            666.0,
        ));
        lines.extend(row(
            &[
                ("ConvS2S", 50.0),
                ("25.16", 180.0),
                ("40.46", 230.0),
                ("1.5", 350.0),
            ],
            654.0,
        ));
        lines.push(line(
            "The big model outperforms the best previously reported models by a wide margin.",
            50.0,
            620.0,
            10.0,
        ));
        let out = tables(lines, None);
        let table: Vec<&Line> = out.iter().filter(|l| l.table).collect();
        assert_eq!(table.len(), 1);
        assert_eq!(
            table[0].text,
            "| Model | BLEU score EN-DE | BLEU score EN-FR | Training Cost EN-DE | Training Cost EN-FR |\n\
             | --- | --- | --- | --- | --- |\n\
             | ByteNet | 23.75 |  |  |  |\n\
             | Transformer | 28.4 | 41.8 | 2.3 |  |\n\
             | ConvS2S | 25.16 | 40.46 |  | 1.5 |"
        );
        assert!(out.iter().any(|l| l.text.starts_with("Table 1:")));
        assert!(out.iter().any(|l| l.text.starts_with("The big model")));
    }

    #[test]
    fn wrapped_cells_join_their_row() {
        // Rows 18 apart, lines inside a cell 11 apart.
        let mut lines = row(&[("Tier", 50.0), ("Governance", 150.0)], 700.0);
        lines.extend(row(&[("Tier 1:", 50.0), ("Managed in an", 150.0)], 682.0));
        lines.extend(row(&[("Partial", 50.0), ("ad hoc manner.", 150.0)], 671.0));
        lines.extend(row(&[("Tier 2:", 50.0), ("Approved by", 150.0)], 653.0));
        lines.extend(row(&[("Informed", 50.0), ("management.", 150.0)], 642.0));
        let out = tables(lines, None);
        assert_eq!(
            out[0].text,
            "| Tier | Governance |\n| --- | --- |\n\
             | Tier 1: Partial | Managed in an ad hoc manner. |\n\
             | Tier 2: Informed | Approved by management. |"
        );
    }

    #[test]
    fn prose_columns_and_lists_are_not_tables() {
        // Two columns of running text, on a page whose gutter is known.
        let mut lines = Vec::new();
        for i in 0..10 {
            let y = 700.0 - i as f32 * 12.0;
            lines.push(line("left column words words words word", 50.0, y, 10.0));
            lines.push(line("right column words words words wor", 320.0, y, 10.0));
        }
        assert!(tables(lines, None).iter().all(|l| !l.table));
        // A bullet list: bullets set apart from their text.
        let mut lines = Vec::new();
        for i in 0..4 {
            lines.extend(row(
                &[("•", 50.0), ("Section explains the basics", 70.0)],
                700.0 - i as f32 * 12.0,
            ));
        }
        assert!(tables(lines, None).iter().all(|l| !l.table));
    }

    #[test]
    fn a_span_of_numbers_is_split_into_cells() {
        let mut lines = row(
            &[
                ("system", 50.0),
                ("aero", 150.0),
                ("bike", 180.0),
                ("bird", 210.0),
            ],
            700.0,
        );
        lines.extend(row(&[("base", 50.0), ("73.2 76.5 79.0", 150.0)], 688.0));
        lines.extend(row(&[("ours", 50.0), ("76.4 79.8 80.7", 150.0)], 676.0));
        lines.extend(row(&[("best", 50.0), ("77.0 80.1 81.2", 150.0)], 664.0));
        let out = tables(lines, None);
        assert!(out[0].table);
        assert!(
            out[0].text.contains("| base | 73.2 | 76.5 | 79.0 |"),
            "{}",
            out[0].text
        );
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
