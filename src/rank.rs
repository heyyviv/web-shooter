use std::{collections::HashMap, sync::LazyLock};

use regex::Regex;
use rust_stemmers::{Algorithm, Stemmer};

use crate::chunk::Chunk;

/// BM25 term-frequency saturation and length normalisation (standard values).
const K1: f32 = 1.2;
const B: f32 = 0.75;

static STEMMER: LazyLock<Stemmer> = LazyLock::new(|| Stemmer::create(Algorithm::English));
static LINK_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\]\([^)]*\)").unwrap());

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "do", "does", "for", "from",
    "has", "have", "how", "i", "if", "in", "is", "it", "its", "me", "my", "of", "on", "or", "our",
    "so", "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "to",
    "was", "we", "were", "what", "when", "where", "which", "who", "why", "will", "with", "you",
    "your",
];

/// Lowercased, stemmed terms with stopwords removed. A hyphenated word
/// yields its parts and the joined form, so "re-visit" matches "revisit".
pub fn terms(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for token in text.split(|c: char| !c.is_alphanumeric() && c != '-') {
        let parts: Vec<&str> = token.split('-').filter(|p| !p.is_empty()).collect();
        words.extend(parts.iter().map(|p| p.to_lowercase()));
        if parts.len() > 1 {
            words.push(parts.concat().to_lowercase());
        }
    }
    words
        .into_iter()
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .map(|w| STEMMER.stem(&w).into_owned())
        .collect()
}

/// The text a chunk is scored on: its headings plus its body, without link URLs.
fn scoring_text(chunk: &Chunk) -> String {
    format!(
        "{}\n{}",
        chunk.heading_path,
        LINK_URL.replace_all(&chunk.text, "]")
    )
}

/// BM25 score of every chunk for the query. The chunks of all pages in the
/// call form the corpus, so scores are comparable across pages.
pub fn bm25(query: &str, chunks: &[Chunk]) -> Vec<f32> {
    let mut query_terms = terms(query);
    query_terms.sort();
    query_terms.dedup();
    if query_terms.is_empty() || chunks.is_empty() {
        return vec![0.0; chunks.len()];
    }

    let docs: Vec<Vec<String>> = chunks.iter().map(|c| terms(&scoring_text(c))).collect();
    let n = docs.len() as f32;
    let avg_len = docs.iter().map(Vec::len).sum::<usize>() as f32 / n;

    let idf: HashMap<&str, f32> = query_terms
        .iter()
        .map(|q| {
            let df = docs.iter().filter(|d| d.contains(q)).count() as f32;
            (q.as_str(), ((n - df + 0.5) / (df + 0.5) + 1.0).ln())
        })
        .collect();

    docs.iter()
        .map(|doc| {
            let len_norm = 1.0 - B + B * doc.len() as f32 / avg_len.max(1.0);
            query_terms
                .iter()
                .map(|q| {
                    let tf = doc.iter().filter(|t| *t == q).count() as f32;
                    if tf == 0.0 {
                        return 0.0;
                    }
                    idf[q.as_str()] * tf * (K1 + 1.0) / (tf + K1 * len_norm)
                })
                .sum()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(heading: &str, text: &str) -> Chunk {
        Chunk {
            page: 0,
            index: 0,
            heading_path: heading.into(),
            text: text.into(),
        }
    }

    #[test]
    fn terms_stem_and_drop_stopwords() {
        assert_eq!(
            terms("How do I install the Crawlers?"),
            vec!["instal", "crawler"]
        );
    }

    #[test]
    fn hyphenated_words_also_match_joined() {
        assert_eq!(terms("re-visit"), vec!["re", "visit", "revisit"]);
        assert_eq!(terms("-x- y"), vec!["x", "y"]);
    }

    #[test]
    fn relevant_chunk_scores_highest() {
        let chunks = [
            c("Pricing", "Plans start at ten dollars a month."),
            c("Install", "Install with cargo install web-shooter."),
            c(
                "About",
                "We build crawlers. [Install](https://example.com/install)",
            ),
        ];
        let s = bm25("how to install", &chunks);
        assert!(s[1] > s[2] && s[2] > s[0], "{s:?}");
        assert_eq!(s[0], 0.0);
    }

    #[test]
    fn link_urls_are_not_scored() {
        let chunks = [c("", "[Home](https://example.com/install-guide)")];
        assert_eq!(bm25("install guide", &chunks), vec![0.0]);
    }
}
