use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use fastembed::{RerankInitOptions, RerankerModel, TextRerank};
use tokio::sync::OnceCell;

/// Default cross-encoder. In the eval it was the only one that didn't lose
/// accuracy; the smaller Jina v1 turbo was faster but ranked worse than
/// embeddings alone. ~1.1 GB, so it loads only when a call asks for it.
pub const DEFAULT_MODEL: RerankerModel = RerankerModel::JINARerankerV2BaseMultilingual;

/// Query + passage tokens the reranker reads; chunks are ~200 tokens, so this fits.
const MAX_LENGTH: usize = 512;

/// The cross-encoder reranker, loaded once on first use.
///
/// Unlike [`crate::embed::Embedder`] it is not preloaded: it is opt-in per
/// call and large. Failure to load means no reranking.
/// `TextRerank::rerank` needs `&mut`, so calls are serialised by a mutex.
#[derive(Clone, Default)]
pub struct Reranker {
    model: Arc<OnceCell<Option<Mutex<TextRerank>>>>,
}

impl Reranker {
    pub async fn get(&self) -> Option<&Mutex<TextRerank>> {
        self.model
            .get_or_init(|| async {
                tokio::task::spawn_blocking(|| load().map(Mutex::new))
                    .await
                    .ok()
                    .flatten()
            })
            .await
            .as_ref()
    }

    /// The model if loading has finished and succeeded, without waiting.
    pub fn ready(&self) -> Option<&Mutex<TextRerank>> {
        self.model.get().and_then(Option::as_ref)
    }
}

/// Load the reranker, downloading it into the Hugging Face cache on first use.
/// `WEB_SHOOTER_RERANK_MODEL` picks another built-in model: `bge-base`,
/// `bge-m3` or `jina-turbo`.
pub fn load() -> Option<TextRerank> {
    let started = Instant::now();
    let name = match std::env::var("WEB_SHOOTER_RERANK_MODEL").as_deref() {
        Ok("bge-base") => RerankerModel::BGERerankerBase,
        Ok("bge-m3") => RerankerModel::BGERerankerV2M3,
        Ok("jina-turbo") => RerankerModel::JINARerankerV1TurboEn,
        _ => DEFAULT_MODEL,
    };
    let options = RerankInitOptions::new(name.clone())
        .with_cache_dir(hf_cache_dir())
        .with_max_length(MAX_LENGTH)
        .with_show_download_progress(false);
    match TextRerank::try_new(options) {
        Ok(model) => {
            eprintln!(
                "web-shooter: loaded reranker {name:?} in {} ms",
                started.elapsed().as_millis()
            );
            Some(model)
        }
        Err(e) => {
            eprintln!("web-shooter: reranker unavailable, ranking without it: {e:#}");
            None
        }
    }
}

/// Relevance of each text to the query, 0..1 (sigmoid of the model's logit).
pub fn scores(model: &Mutex<TextRerank>, query: &str, texts: &[String]) -> Option<Vec<f32>> {
    let mut model = model.lock().ok()?;
    let docs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let results = model.rerank(query, docs, false, None).ok()?;
    let mut out = vec![0.0; texts.len()];
    for r in results {
        out[r.index] = 1.0 / (1.0 + (-r.score).exp());
    }
    Some(out)
}

/// `$HF_HOME/hub`, else `~/.cache/huggingface/hub`: the cache the embedding
/// model also uses. fastembed's own default is relative to the working directory.
fn hf_cache_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HF_HOME") {
        return PathBuf::from(home).join("hub");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/huggingface/hub")
}
