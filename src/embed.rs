use std::{sync::Arc, time::Instant};

use model2vec_rs::model::StaticModel;
use tokio::sync::OnceCell;

/// Default static embedding model. Override with `WEB_SHOOTER_EMBED_MODEL`
/// (a Hugging Face repo id or a local folder). In the eval it ranked as well
/// as `potion-retrieval-32M` with a quarter of the memory (88 vs 300 MB RSS).
pub const DEFAULT_MODEL: &str = "minishlab/potion-base-8M";

/// The embedding model, loaded once in the background.
///
/// Loading starts when the server starts. The first query waits for it;
/// if loading fails (e.g. offline on first run), ranking falls back to BM25.
#[derive(Clone, Default)]
pub struct Embedder {
    model: Arc<OnceCell<Option<StaticModel>>>,
}

impl Embedder {
    /// Start loading in the background so the first query doesn't pay for it.
    pub fn preload(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            this.get().await;
        });
    }

    /// The model, or `None` if it could not be loaded.
    pub async fn get(&self) -> Option<&StaticModel> {
        self.model
            .get_or_init(|| async { tokio::task::spawn_blocking(load).await.ok().flatten() })
            .await
            .as_ref()
    }

    /// The model if loading has finished and succeeded, without waiting.
    pub fn ready(&self) -> Option<&StaticModel> {
        self.model.get().and_then(Option::as_ref)
    }
}

/// Load the model, downloading it into the Hugging Face cache on first use.
pub fn load() -> Option<StaticModel> {
    let name = std::env::var("WEB_SHOOTER_EMBED_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    let started = Instant::now();
    match StaticModel::from_pretrained(&name, None, Some(true), None) {
        Ok(model) => {
            eprintln!(
                "web-shooter: loaded embedding model {name} in {} ms",
                started.elapsed().as_millis()
            );
            Some(model)
        }
        Err(e) => {
            eprintln!("web-shooter: embedding model {name} unavailable, using BM25 only: {e:#}");
            None
        }
    }
}

/// Cosine similarity of the query to each text. Vectors are L2-normalised
/// at load time (`normalize = true`), so the dot product is the cosine.
pub fn similarities(model: &StaticModel, query: &str, texts: &[String]) -> Vec<f32> {
    let q = model.encode_single(query);
    model
        .encode(texts)
        .iter()
        .map(|v| v.iter().zip(&q).map(|(a, b)| a * b).sum())
        .collect()
}
