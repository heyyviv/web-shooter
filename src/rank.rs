use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
};

use model2vec_rs::model::StaticModel;
use regex::Regex;
use rust_stemmers::{Algorithm, Stemmer};

use crate::{
    chunk::{Chunk, MD_LINK},
    embed, rerank,
};

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

/// The text a chunk is scored on: its headings, its table's caption if it
/// holds table rows, and its body without link URLs.
pub fn scoring_text(chunk: &Chunk) -> String {
    // A table's caption says what its numbers are; every part of a split
    // table is scored with it.
    let caption = chunk.table.as_deref().unwrap_or_default();
    format!(
        "{}\n{caption}\n{}",
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

/// How BM25 and embedding scores are combined.
#[derive(Debug, Clone, Copy)]
pub enum Fusion {
    /// Reciprocal rank fusion: `Σ 1 / (k + rank)` over both rankings.
    Rrf { k: f32 },
    /// Each score min-max normalised over the call, then
    /// `bm25_weight · bm25 + (1 − bm25_weight) · cosine`.
    Weighted { bm25_weight: f32 },
}

/// Ranking settings, tuned with the eval set (`examples/eval.rs`).
#[derive(Debug, Clone, Copy)]
pub struct RankConfig {
    pub fusion: Fusion,
    /// Without reranking: passages scoring below this fraction of the best
    /// one are left out.
    pub min_relative: f32,
    /// How many of the best fused candidates the cross-encoder rescores,
    /// when a reranker is passed in (0 = never rerank).
    pub rerank_top: usize,
    /// With reranking: the cut-off applied to reranker scores instead.
    pub rerank_min_relative: f32,
    /// How much a passage that is all link text is down-weighted (0 = not
    /// at all, 1 = to zero). Link lists such as "other jobs" sidebars match
    /// many queries by their titles without answering them.
    pub link_penalty: f32,
    /// How much a very short passage (`SHORT_CHUNK_WORDS` or fewer) is
    /// down-weighted. Such passages are mostly navigation ("Back to jobs
    /// search"), and BM25's length normalisation over-rewards them.
    pub short_penalty: f32,
}

impl Default for RankConfig {
    /// Fusion: from `eval --sweep` on 15 cases, weighted fusion with the
    /// embedding score dominant ranked the answer first in 14/15 at ~1,070
    /// tokens. RRF ranked as well, but its flat scores make the relative
    /// cut-off ineffective, so replies fill the budget (~1,600–1,760 tokens).
    ///
    /// Reranking (opt-in): Jina v2 on the top 6 with a 0.5 cut-off kept every
    /// answer (top 13/15) at ~760 tokens, 33% fewer, for ~300 ms.
    fn default() -> Self {
        Self {
            fusion: Fusion::Weighted { bm25_weight: 0.3 },
            min_relative: 0.7,
            rerank_top: 6,
            rerank_min_relative: 0.5,
            link_penalty: 0.5,
            short_penalty: 0.5,
        }
    }
}

/// The optional models ranking can use; any of them may be missing.
#[derive(Clone, Copy, Default)]
pub struct Models<'a> {
    pub embedder: Option<&'a StaticModel>,
    pub reranker: Option<&'a Mutex<rerank::Model>>,
}

/// Chunk scores plus the relative cut-off that suits them.
pub struct Ranked {
    pub scores: Vec<f32>,
    pub min_relative: f32,
}

/// Full ranking: BM25 and embeddings fused, then the best candidates
/// rescored by the cross-encoder. Chunks that weren't reranked score 0.
pub fn rank(query: &str, chunks: &[Chunk], models: Models, config: RankConfig) -> Ranked {
    let fused: Vec<f32> = hybrid(query, chunks, models.embedder, config.fusion)
        .into_iter()
        .zip(chunks)
        .map(|(s, c)| s * shape_factor(c, config))
        .collect();
    let fallback = |scores| Ranked {
        scores,
        min_relative: config.min_relative,
    };
    let Some(reranker) = models.reranker.filter(|_| config.rerank_top > 0) else {
        return fallback(fused);
    };

    let mut candidates: Vec<usize> = (0..chunks.len()).filter(|&i| fused[i] > 0.0).collect();
    candidates.sort_by(|&a, &b| fused[b].total_cmp(&fused[a]));
    candidates.truncate(config.rerank_top);
    let texts: Vec<String> = candidates
        .iter()
        .map(|&i| scoring_text(&chunks[i]))
        .collect();
    let Some(rescored) = rerank::scores(reranker, query, &texts) else {
        return fallback(fused);
    };

    let mut scores = vec![0.0; chunks.len()];
    for (&i, s) in candidates.iter().zip(rescored) {
        scores[i] = s * shape_factor(&chunks[i], config);
    }
    Ranked {
        scores,
        min_relative: config.rerank_min_relative,
    }
}

/// Score every chunk for the query: BM25, fused with embedding similarity
/// when a model is available. Higher is better; 0 means unrelated.
pub fn hybrid(
    query: &str,
    chunks: &[Chunk],
    model: Option<&StaticModel>,
    fusion: Fusion,
) -> Vec<f32> {
    let lexical = bm25(query, chunks);
    let Some(model) = model else {
        return lexical;
    };
    let texts: Vec<String> = chunks.iter().map(scoring_text).collect();
    let dense = embed::similarities(model, query, &texts);
    match fusion {
        Fusion::Rrf { k } => {
            let mut fused = vec![0.0; chunks.len()];
            for (scores, skip_zero) in [(&lexical, true), (&dense, false)] {
                let mut order: Vec<usize> = (0..chunks.len())
                    .filter(|&i| !skip_zero || scores[i] > 0.0)
                    .collect();
                order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
                for (rank, i) in order.into_iter().enumerate() {
                    fused[i] += 1.0 / (k + rank as f32 + 1.0);
                }
            }
            fused
        }
        Fusion::Weighted { bm25_weight } => {
            let (l, d) = (min_max(&lexical), min_max(&dense));
            l.iter()
                .zip(&d)
                .map(|(l, d)| bm25_weight * l + (1.0 - bm25_weight) * d)
                .collect()
        }
    }
}

/// Relevance of each text to the query on a fixed 0..1 scale, so scores from
/// different pages can be compared (unlike `hybrid`, which is normalised per
/// call). The share of query terms the text contains, averaged with the
/// embedding cosine when a model is available.
pub fn absolute(query: &str, texts: &[String], model: Option<&StaticModel>) -> Vec<f32> {
    let mut query_terms = terms(query);
    query_terms.sort();
    query_terms.dedup();
    let overlap: Vec<f32> = texts
        .iter()
        .map(|t| {
            if query_terms.is_empty() {
                return 0.0;
            }
            let doc: HashSet<String> = terms(t).into_iter().collect();
            query_terms.iter().filter(|q| doc.contains(*q)).count() as f32
                / query_terms.len() as f32
        })
        .collect();
    let Some(model) = model else {
        return overlap;
    };
    embed::similarities(model, query, texts)
        .into_iter()
        .zip(overlap)
        .map(|(cos, o)| (cos.max(0.0) + o) / 2.0)
        .collect()
}

/// Passages with at most this many words count as very short.
const SHORT_CHUNK_WORDS: usize = 6;

/// Score multiplier for a passage's shape (link list, very short), applied
/// in both ranking modes.
fn shape_factor(chunk: &Chunk, config: RankConfig) -> f32 {
    let short = if chunk.text.split_whitespace().count() <= SHORT_CHUNK_WORDS {
        1.0 - config.short_penalty
    } else {
        1.0
    };
    short * link_list_factor(&chunk.text, config.link_penalty)
}

/// A passage is a link list if it has at least this many links…
const LINK_LIST_MIN_LINKS: usize = 3;
/// …and fewer than this many letters and digits outside them. A real
/// sentence next to a few links (common on landing pages) stays prose.
const LINK_LIST_MAX_PROSE: usize = 100;

/// Score multiplier for a passage: `1 − penalty` for link lists, else 1.
fn link_list_factor(text: &str, penalty: f32) -> f32 {
    let prose = MD_LINK
        .replace_all(text, "")
        .chars()
        .filter(|c| c.is_alphanumeric())
        .count();
    if MD_LINK.find_iter(text).count() >= LINK_LIST_MIN_LINKS && prose < LINK_LIST_MAX_PROSE {
        1.0 - penalty
    } else {
        1.0
    }
}

/// Scale scores to 0..1 over the call. When they are all equal (one chunk,
/// say) there is nothing to rank against: all 1 if they matched at all,
/// else all 0. Zeros there made a one-chunk page return no passages.
fn min_max(scores: &[f32]) -> Vec<f32> {
    let lo = scores.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if hi - lo <= f32::EPSILON {
        let flat = if hi > 0.0 { 1.0 } else { 0.0 };
        return vec![flat; scores.len()];
    }
    scores.iter().map(|s| (s - lo) / (hi - lo)).collect()
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
            pdf_page: None,
            table: None,
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
    fn only_link_lists_are_penalised() {
        let list = "[A](https://x/a)\n[B](https://x/b)\n[C](https://x/c)";
        assert_eq!(link_list_factor(list, 0.5), 0.5);
        let with_prose = format!(
            "{list}\nYou don't have to build your whole page in React. Add React to your existing HTML page, and render interactive React components anywhere on it."
        );
        assert_eq!(link_list_factor(&with_prose, 0.5), 1.0);
    }

    #[test]
    fn min_max_scales_and_handles_flat_scores() {
        assert_eq!(min_max(&[2.0, 4.0, 3.0]), vec![0.0, 1.0, 0.5]);
        assert_eq!(min_max(&[1.0, 1.0]), vec![1.0, 1.0]);
        assert_eq!(min_max(&[0.4]), vec![1.0]);
        assert_eq!(min_max(&[0.0, 0.0]), vec![0.0, 0.0]);
    }

    #[test]
    fn single_chunk_is_ranked_with_embeddings() {
        let Some(model) = embed::load() else {
            eprintln!("skipped: embedding model unavailable");
            return;
        };
        let chunks = [c("", "Rust was first released in 2015 by Mozilla Research.")];
        let scores = hybrid(
            "when was rust released",
            &chunks,
            Some(&model),
            RankConfig::default().fusion,
        );
        assert_eq!(scores, vec![1.0]);
    }

    #[test]
    fn hybrid_without_model_is_bm25() {
        let chunks = [c("", "install guide"), c("", "pricing")];
        let fusion = RankConfig::default().fusion;
        assert_eq!(
            hybrid("install", &chunks, None, fusion),
            bm25("install", &chunks)
        );
    }

    #[test]
    fn absolute_without_model_is_query_term_share() {
        let texts = ["rust engineer jobs".to_string(), "pricing".to_string()];
        assert_eq!(
            absolute("rust jobs in berlin", &texts, None),
            vec![2.0 / 3.0, 0.0]
        );
    }

    #[test]
    fn link_urls_are_not_scored() {
        let chunks = [c("", "[Home](https://example.com/install-guide)")];
        assert_eq!(bm25("install guide", &chunks), vec![0.0]);
    }
}
