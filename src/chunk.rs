use std::sync::LazyLock;

use regex::Regex;

use crate::pdf::PAGE_MARKER;

/// A chunk grows until it reaches this size, then a new one starts at the
/// next line. About 200 tokens: big enough to carry context, small enough
/// that a relevant chunk isn't mostly padding.
const TARGET_BYTES: usize = 800;

/// A trailing chunk smaller than this is merged into the one before it.
const MIN_BYTES: usize = 160;

/// A passage of one page, under the headings that contain it.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Index of the page within the tool call.
    pub page: usize,
    /// Position within the page, so neighbouring chunks can be merged.
    pub index: usize,
    /// Headings above this chunk, e.g. `Crawling policy > Selection policy`.
    pub heading_path: String,
    pub text: String,
    /// PDF page the chunk starts on, from the `[p.N]` markers.
    pub pdf_page: Option<u32>,
}

/// Back-matter sections: citation lists that match many queries by accident
/// and cost a lot of tokens. Compared case-insensitively against each heading.
const SKIP_SECTIONS: &[&str] = &[
    "references",
    "notes",
    "citations",
    "footnotes",
    "bibliography",
    "sources",
    "further reading",
    "external links",
];

/// Compared without a leading section number, as PDFs number them (`7 References`).
fn is_back_matter(heading: &str) -> bool {
    let h = heading.to_lowercase();
    let h = h
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.')
        .trim();
    SKIP_SECTIONS.contains(&h)
}

/// A table caption line: `Table 2: …`, `TABLE 3. …`.
static TABLE_CAPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:Table|TABLE)\s+[0-9A-Z]+(?:\.[0-9]+)?[.:]?\s").unwrap());

/// A markdown link; the URL may contain one level of parentheses, as in
/// `https://en.wikipedia.org/wiki/Search_engine_(computing)`.
pub static MD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\((?:[^()]|\([^)]*\))*\)").unwrap());

/// Share of letters and digits that sit outside markdown links (link text and URLs).
pub fn prose_share(markdown: &str) -> f32 {
    let alnum = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).count();
    let total = alnum(markdown);
    if total == 0 {
        return 0.0;
    }
    alnum(&MD_LINK.replace_all(markdown, "")) as f32 / total as f32
}

/// Split a page's markdown into chunks at headings and at about `TARGET_BYTES`.
pub fn chunk(page: usize, markdown: &str) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    // (level, text) of the headings above the current line.
    let mut headings: Vec<(usize, String)> = Vec::new();
    let mut buf = String::new();
    let mut in_code = false;
    // The PDF page of the current line, and of the first line in `buf`.
    let mut pdf_page: Option<u32> = None;
    let mut start_page: Option<u32> = None;
    // The header and separator of the markdown table being read, repeated at
    // the start of a chunk that begins inside it so its rows keep their labels.
    let mut table_header: Option<String> = None;
    let mut prev_line = "";
    let mut repeat_header = false;
    // The last line with text, so a caption followed by a blank line still counts.
    let mut last_text = "";

    let flush = |buf: &mut String,
                 headings: &[(usize, String)],
                 pdf_page: Option<u32>,
                 chunks: &mut Vec<Chunk>| {
        let text = buf.trim();
        if text.is_empty() || headings.iter().any(|(_, h)| is_back_matter(h)) {
            buf.clear();
            return;
        }
        let heading_path = headings
            .iter()
            .map(|(_, h)| h.as_str())
            .collect::<Vec<_>>()
            .join(" > ");
        // Merge a small tail into the previous chunk when both sit under the same headings.
        if text.len() < MIN_BYTES
            && let Some(last) = chunks.last_mut()
            && last.heading_path == heading_path
        {
            last.text.push('\n');
            last.text.push_str(text);
        } else {
            chunks.push(Chunk {
                page,
                index: chunks.len(),
                heading_path,
                text: text.to_string(),
                pdf_page,
            });
        }
        buf.clear();
    };

    for line in markdown.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
        }
        if !in_code && let Some(c) = PAGE_MARKER.captures(line) {
            pdf_page = c[1].parse().ok();
            continue;
        }
        if !in_code && let Some((level, text)) = heading(line) {
            flush(&mut buf, &headings, start_page, &mut chunks);
            headings.retain(|(l, _)| *l < level);
            headings.push((level, text));
            continue;
        }
        if !in_code {
            if line.starts_with("| ---") && prev_line.starts_with('|') {
                table_header = Some(format!("{prev_line}\n{line}\n"));
            } else if !line.starts_with('|') {
                table_header = None;
            }
        }
        if buf.trim().is_empty() {
            start_page = pdf_page;
        }
        if std::mem::take(&mut repeat_header)
            && let Some(header) = &table_header
        {
            buf.push_str(header);
        }
        buf.push_str(line);
        buf.push('\n');
        prev_line = line;
        // Code blocks are kept whole unless they get very long, and a table
        // caption stays with the table after it: its words say what the
        // numbers are.
        if !line.trim().is_empty() {
            last_text = line;
        }
        let caption = !in_code && TABLE_CAPTION.is_match(last_text) && buf.len() < TARGET_BYTES * 2;
        if buf.len() >= TARGET_BYTES && (!in_code || buf.len() >= TARGET_BYTES * 3) && !caption {
            flush(&mut buf, &headings, start_page, &mut chunks);
            repeat_header = table_header.is_some();
        }
    }
    flush(&mut buf, &headings, start_page, &mut chunks);
    chunks
}

/// Top-level section names (`#` and `##`), so the caller can ask for another section.
pub fn outline(markdown: &str, max: usize) -> Vec<String> {
    let mut in_code = false;
    markdown
        .lines()
        .filter(|line| {
            if line.starts_with("```") {
                in_code = !in_code;
            }
            !in_code
        })
        .filter_map(heading)
        .filter(|(level, _)| *level <= 2)
        .map(|(_, text)| text)
        .take(max)
        .collect()
}

/// `## Some [linked](url) **title**` → `(2, "Some linked title")`.
fn heading(line: &str) -> Option<(usize, String)> {
    let level = line.bytes().take_while(|&b| b == b'#').count();
    if level == 0 || level > 6 || !line[level..].starts_with(' ') {
        return None;
    }
    let text = MD_LINK
        .replace_all(line[level..].trim(), "$1")
        .replace("**", "");
    let text = text.trim();
    (!text.is_empty()).then(|| (level, text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_caption_stays_with_its_table() {
        let mut md = "## Results\n\n".to_string();
        md.push_str(&"Some running text about the experiments. ".repeat(18));
        md.push_str("\n\nTable 2: BLEU scores and training cost.\n\n| Model | BLEU |\n| --- | --- |\n| Big | 28.4 |\n");
        let chunks = chunk(0, &md);
        let with_table = chunks.iter().find(|c| c.text.contains("| Big |")).unwrap();
        assert!(with_table.text.contains("Table 2: BLEU"), "{chunks:#?}");
    }

    #[test]
    fn a_table_split_across_chunks_repeats_its_header() {
        let mut md = String::from("## Results\n\n| Model | BLEU |\n| --- | --- |\n");
        for i in 0..80 {
            md.push_str(&format!("| model number {i:02} | {i}.5 |\n"));
        }
        md.push_str("\nAfter the table.\n");
        let chunks = chunk(0, &md);
        assert!(chunks.len() > 2);
        for c in &chunks[1..] {
            if c.text.contains("| model number") {
                assert!(
                    c.text.starts_with("| Model | BLEU |\n| --- | --- |\n"),
                    "{}",
                    c.text
                );
            }
        }
        // No header is left hanging over text that isn't part of the table.
        assert!(
            chunks
                .iter()
                .all(|c| !c.text.contains("| --- | --- |\n\nAfter"))
        );
        assert!(chunks.iter().any(|c| c.text.contains("After the table.")));
    }

    #[test]
    fn prose_share_ignores_link_text_and_urls() {
        let list = "* [Search](https://w.org/Search_(computing))\n";
        assert_eq!(prose_share(&list.repeat(50)), 0.0);
        assert_eq!(prose_share("Ranks pages."), 1.0);
        assert_eq!(prose_share("abcd [ef](g)"), 4.0 / 7.0);
    }

    #[test]
    fn chunks_carry_heading_path() {
        let md = "Intro line\n# Guide\n## Install\nRun the [installer](https://x.y).\n### Linux\nUse apt.\n## Usage\nCall it.";
        let chunks = chunk(0, md);
        let got: Vec<(&str, &str)> = chunks
            .iter()
            .map(|c| (c.heading_path.as_str(), c.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("", "Intro line"),
                ("Guide > Install", "Run the [installer](https://x.y)."),
                ("Guide > Install > Linux", "Use apt."),
                ("Guide > Usage", "Call it."),
            ]
        );
    }

    #[test]
    fn long_sections_split_and_small_tails_merge() {
        let para = "word ".repeat(100); // 500 bytes per line
        let md = format!("## A\n{para}\n{para}\n{para}\nshort tail");
        let chunks = chunk(0, &md);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[1].text.ends_with("short tail"));
    }

    #[test]
    fn back_matter_sections_are_skipped() {
        let md = "## Policy\nVisit rules.\n## References\n### Books\n1. Cho (2000).\n## See also\nOther crawlers.";
        let paths: Vec<String> = chunk(0, md).into_iter().map(|c| c.heading_path).collect();
        assert_eq!(paths, vec!["Policy", "See also"]);
    }

    #[test]
    fn chunks_carry_the_pdf_page_they_start_on() {
        let md = "[p.1]\n\n## 1 Intro\nFirst page text.\n[p.2]\nSecond page text.\n## 2 Method\nMore.\n## 7 References\n[1] A. Paper.";
        let got: Vec<(String, Option<u32>)> = chunk(0, md)
            .into_iter()
            .map(|c| (c.heading_path, c.pdf_page))
            .collect();
        assert_eq!(
            got,
            vec![("1 Intro".into(), Some(1)), ("2 Method".into(), Some(2))]
        );
        assert!(
            chunk(0, md)[0]
                .text
                .contains("First page text.\nSecond page text.")
        );
    }

    #[test]
    fn hashes_inside_code_are_not_headings() {
        let md = "## Real\n```\n# comment\n```";
        let chunks = chunk(0, md);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path, "Real");
        assert_eq!(outline(md, 10), vec!["Real"]);
    }
}
