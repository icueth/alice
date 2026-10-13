//! Embedding provider interface and a deterministic fake for tests.

use crate::error::Result;
use crate::normalize::{nfc, truncate_chars};

pub trait Embedder: Send + Sync {
    /// Identifier stored with each vector, e.g. `multilingual-e5-small`.
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;
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
}

impl FakeEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
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
        "fake"
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
}
