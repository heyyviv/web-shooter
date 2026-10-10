use crate::{embed::Embedder, rank::Models, rerank::Reranker};

/// The ranking models a server owns, each loaded once.
#[derive(Clone, Default)]
pub struct ModelSet {
    embedder: Embedder,
    reranker: Reranker,
}

impl ModelSet {
    /// Start loading the embedding model so the first query doesn't pay for
    /// it. The reranker is opt-in and large, so it loads on first use.
    pub fn preload(&self) {
        self.embedder.preload();
    }

    /// Wait until the models a call needs have loaded or failed to load.
    pub async fn wait(&self, precise: bool) {
        if precise {
            tokio::join!(self.embedder.get(), self.reranker.get());
        } else {
            self.embedder.get().await;
        }
    }

    /// The models to rank with: the reranker only when `precise` is set.
    pub fn ready(&self, precise: bool) -> Models<'_> {
        Models {
            embedder: self.embedder.ready(),
            reranker: self.reranker.ready().filter(|_| precise),
        }
    }
}
