//! Embedding provider interface and a deterministic fake for tests.

use crate::error::Result;
use crate::normalize::{nfc, truncate_chars};

/// Why a query was not embedded: a local model unloaded after an idle period is loading again
/// (search then uses word matches). Embedders put it at the start of that error's text.
pub const MODEL_LOADING: &str =
    "the embedding model is loading after an idle period; this search used word matches only";

pub trait Embedder: Send + Sync {
    /// Identifier stored with each vector, e.g. `multilingual-e5-small`.
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;
    /// False while a model that was unloaded after an idle period is being loaded again.
    fn is_loaded(&self) -> bool {
        true
    }
    /// True when texts leave this machine to be embedded (an HTTP provider whose endpoint is
    /// not the local host). The daemon then sends a text only if the cloud policy allows its
    /// project (`SettingsHandle::cloud_allowed`); a local embedder never asks.
    fn is_remote(&self) -> bool {
        false
    }
    /// Why this embedder cannot work right now, if it cannot (a stand-in for a provider that
    /// was unreachable at start); shown in the daemon's health.
    fn problem(&self) -> Option<String> {
        None
    }
}

/// Stands in for the embedder of a model whose provider could not be reached when the daemon
/// started: the model's vectors and index stay in use for search, every embedding fails as
/// "unavailable" (texts stay queued, queries use word matches), and the daemon replaces it as
/// soon as the real embedder can be built.
pub struct UnavailableEmbedder {
    id: String,
    dim: usize,
    remote: bool,
    reason: String,
}

impl UnavailableEmbedder {
    pub fn new(id: impl Into<String>, dim: usize, remote: bool, reason: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            dim,
            remote,
            reason: reason.into(),
        }
    }
}

impl Embedder for UnavailableEmbedder {
    fn model_id(&self) -> &str {
        &self.id
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed_passages(&self, _: &[String]) -> Result<Vec<Vec<f32>>> {
        Err(crate::error::AliceError::Unavailable(self.reason.clone()))
    }
    fn embed_query(&self, _: &str) -> Result<Vec<f32>> {
        Err(crate::error::AliceError::Unavailable(format!(
            "{}; this search used word matches only",
            self.reason
        )))
    }
    fn is_remote(&self) -> bool {
        self.remote
    }
    fn problem(&self) -> Option<String> {
        Some(self.reason.clone())
    }
}

/// Text embedded for a memory: title and body, bounded so the model's 512-token window is not exceeded badly.
pub fn memory_embed_text(title: &str, body: &str) -> String {
    truncate_chars(&format!("{title}\n{body}"), 2_000)
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Hashes words and character trigrams into a fixed-size, L2-normalised vector.
/// Texts that share many trigrams get high cosine similarity. No model download.
pub struct FakeEmbedder {
    dim: usize,
    id: String,
}

impl FakeEmbedder {
    pub fn new(dim: usize) -> Self {
        Self::named("fake", dim)
    }

    /// A fake whose vectors are stored under the model id `id` (tests of a model switch).
    pub fn named(id: impl Into<String>, dim: usize) -> Self {
        Self { dim, id: id.into() }
    }

    fn vector(&self, text: &str) -> Vec<f32> {
        fn fnv1a(bytes: &[u8]) -> u64 {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        }
        let mut v = vec![0f32; self.dim];
        let text = nfc(text).to_lowercase();
        let mut add = |feature: &str, weight: f32| {
            let h = fnv1a(feature.as_bytes());
            let idx = (h % self.dim as u64) as usize;
            let sign = if (h >> 63) == 0 { 1.0 } else { -1.0 };
            v[idx] += sign * weight;
        };
        for word in text.split_whitespace() {
            add(word, 1.0);
        }
        let chars: Vec<char> = format!(" {text} ").chars().collect();
        for w in chars.windows(3) {
            add(&w.iter().collect::<String>(), 0.5);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }
}

impl Embedder for FakeEmbedder {
    fn model_id(&self) -> &str {
        &self.id
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.vector(t)).collect())
    }
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.vector(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_embedder_is_deterministic_normalised_and_similarity_aware() {
        let e = FakeEmbedder::new(384);
        let a = e.embed_query("sqlite writer lease timeout").unwrap();
        let b = e
            .embed_passages(&["sqlite writer lease timeout".to_string()])
            .unwrap()
            .remove(0);
        let c = e.embed_query("sqlite writer lease timed out").unwrap();
        let d = e.embed_query("ภาษาไทย ความจำ").unwrap();
        assert_eq!(a.len(), 384);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-5);
        assert!(
            (cosine(&a, &b) - 1.0).abs() < 1e-5,
            "query and passage of same text match"
        );
        assert!(
            cosine(&a, &c) > 0.6,
            "near text is near: {}",
            cosine(&a, &c)
        );
        assert!(
            cosine(&a, &d) < 0.3,
            "unrelated text is far: {}",
            cosine(&a, &d)
        );
    }

    #[test]
    fn embed_text_is_bounded() {
        let t = memory_embed_text("title", &"x".repeat(10_000));
        assert!(t.chars().count() <= 2_001);
        assert!(t.starts_with("title\n"));
    }
    #[test]
    fn a_named_fake_stores_its_vectors_under_its_own_id() {
        let e = FakeEmbedder::named("ollama:nomic-embed-text", 64);
        assert_eq!((e.model_id(), e.dim()), ("ollama:nomic-embed-text", 64));
        assert_eq!(FakeEmbedder::new(8).model_id(), "fake");
        assert!(!e.is_remote() && e.problem().is_none());
    }

    #[test]
    fn a_stand_in_embedder_fails_as_unavailable_and_says_why() {
        let e = UnavailableEmbedder::new("openai:m", 3, true, "cannot reach the provider");
        assert_eq!(
            (e.model_id(), e.dim(), e.is_remote()),
            ("openai:m", 3, true)
        );
        assert_eq!(e.problem().as_deref(), Some("cannot reach the provider"));
        use crate::error::AliceError;
        let err = e.embed_passages(&["x".into()]).unwrap_err();
        assert!(matches!(&err, AliceError::Unavailable(m) if m.contains("cannot reach")));
        let err = e.embed_query("x").unwrap_err();
        assert!(matches!(&err, AliceError::Unavailable(m) if m.contains("word matches only")));
    }
}
