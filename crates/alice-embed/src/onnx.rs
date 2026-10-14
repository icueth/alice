//! multilingual-e5-small (int8) on ONNX Runtime: the compact tokenizer, mean pooling over the
//! attention mask, L2 normalisation, `query: ` / `passage: ` prefixes. Same vectors as the
//! fastembed build used in M1 (`tests/onnx_equivalence.rs`), less memory: no tokenizer trie,
//! no CPU arena (ONNX Runtime would keep its largest call's working memory for good), no
//! memory patterns, the model read from its file by ONNX Runtime instead of a copy in memory.
//!
//! With [`OnnxOptions::idle_unload`] the session is dropped after that long unused. A query
//! that arrives then does not wait for the reload: it fails at once with [`MODEL_LOADING`]
//! (search uses word matches) and starts the reload in the background; passages (the
//! embedding worker) wait for it.

use crate::lazy::Lazy;
use crate::tokenizer::UnigramTokenizer;
use crate::{DIM, MODEL_ID, is_model_present, model_dir};
use alice_core::embed::Embedder;
use alice_core::error::{AliceError, Result};
use ort::session::{Session, SessionInputValue};
use ort::value::Tensor;
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Longest input the model reads, in tokens (including `<s>` and `</s>`).
const MAX_TOKENS: usize = 512;
/// Most texts per inference call.
const MAX_BATCH: usize = 32;
/// Padded tokens (texts × longest) per call: bounds working memory and how long a query
/// waits behind the worker (one call, ~115 ms for two 512-token texts on an M4 Pro).
const BATCH_TOKENS: usize = 1_024;

/// Fills the tail of the shorter texts in a batch: `pad_token_id` of the model's pinned
/// `config.json` (0), as M1's fastembed build padded. Padded positions are masked out of
/// attention but still enter the int8 model's per-call activation quantization (its range is
/// taken over the whole batch), so another id would move the vectors slightly.
const PAD_ID: u32 = 0;

/// Why a query was not embedded: the model was unloaded after an idle period and is loading
/// (defined in `alice_core::embed`, where search recognises it).
pub use alice_core::embed::MODEL_LOADING;

#[derive(Debug, Clone, Default)]
pub struct OnnxOptions {
    /// Unload the model after this long without use (`None` or zero: never).
    pub idle_unload: Option<Duration>,
    /// ONNX Runtime intra-op threads (`None`: [`DEFAULT_INTRA_THREADS`], or fewer cores).
    pub intra_threads: Option<usize>,
}

/// Threads ONNX Runtime uses for one call when nothing asks for more. The daemon runs in the
/// background beside the person's own work, and measured on 14 cores (10 performance, 4
/// efficiency; see docs/benchmarks/m2.md, Task 6) four threads answer a query ~25% faster than
/// all of them (the efficiency cores slow every call down), use ~35% less CPU to embed the same
/// texts, and still embed ~110 passages/s against ~135.
pub const DEFAULT_INTRA_THREADS: usize = 4;

struct Model {
    session: Session,
    token_types: bool,
}

/// The local embedding model. One instance is shared by the embedding worker and queries:
/// calls are serialised, and the session is handed to a waiting caller after each batch.
pub struct OnnxEmbedder {
    tokenizer: UnigramTokenizer,
    model: Arc<Lazy<Model>>,
}

fn ort_err(e: impl std::fmt::Display) -> AliceError {
    AliceError::Other(format!(
        "the embedding model failed: {e}. Run \"alice model fetch\" to repair the model files"
    ))
}

/// The intra-op threads to use: the one asked for, else [`DEFAULT_INTRA_THREADS`] or the number
/// of cores when there are fewer (2 when that cannot be told).
fn threads_for(requested: Option<usize>, cores: Option<usize>) -> usize {
    requested.unwrap_or_else(|| cores.map_or(2, |n| n.min(DEFAULT_INTRA_THREADS)))
}

fn load(onnx: &Path, threads: Option<usize>) -> Result<Model> {
    let threads = threads_for(
        threads,
        std::thread::available_parallelism().ok().map(|n| n.get()),
    );
    let session = Session::builder()
        .map_err(ort_err)?
        .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
        .map_err(ort_err)?
        .with_intra_threads(threads)
        .map_err(ort_err)?
        .with_memory_pattern(false)
        .map_err(ort_err)?
        .with_execution_providers([ort::ep::CPU::default().with_arena_allocator(false).build()])
        .map_err(ort_err)?
        .commit_from_file(onnx)
        .map_err(ort_err)?;
    let token_types = session
        .inputs()
        .iter()
        .any(|i| i.name() == "token_type_ids");
    Ok(Model {
        session,
        token_types,
    })
}

/// Consecutive ranges of `lens` (token counts) with at most `max_batch` texts and at most
/// `budget` padded tokens each; a text longer than the budget gets a range of its own.
pub(crate) fn token_batches(
    lens: &[usize],
    max_batch: usize,
    budget: usize,
) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut longest) = (0, 0);
    for (i, &len) in lens.iter().enumerate() {
        let n = i - start;
        if n > 0 && (n >= max_batch || (n + 1) * longest.max(len) > budget) {
            out.push(start..i);
            (start, longest) = (i, 0);
        }
        longest = longest.max(len);
    }
    if start < lens.len() {
        out.push(start..lens.len());
    }
    out
}

/// Input ids and attention mask of a batch, row-major `[texts, longest]`, the tail of each
/// shorter text filled with `pad` and masked out.
fn pad_batch(batch: &[Vec<u32>], pad: u32) -> (Vec<i64>, Vec<i64>, [i64; 2]) {
    let b = batch.len();
    let l = batch.iter().map(Vec::len).max().unwrap_or(0);
    let mut ids = vec![i64::from(pad); b * l];
    let mut mask = vec![0i64; b * l];
    for (i, row) in batch.iter().enumerate() {
        for (j, &t) in row.iter().enumerate() {
            ids[i * l + j] = i64::from(t);
            mask[i * l + j] = 1;
        }
    }
    (ids, mask, [b as i64, l as i64])
}

/// Mean of each text's unmasked rows of `hidden` (`[b, l, h]`, row-major), L2-normalised.
fn mean_pool(hidden: &[f32], mask: &[i64], b: usize, l: usize, h: usize) -> Vec<Vec<f32>> {
    (0..b)
        .map(|i| {
            let mut v = vec![0f32; h];
            let mut count = 0f32;
            for j in (0..l).filter(|&j| mask[i * l + j] == 1) {
                let row = &hidden[(i * l + j) * h..(i * l + j + 1) * h];
                v.iter_mut().zip(row).for_each(|(a, x)| *a += x);
                count += 1.0;
            }
            v.iter_mut().for_each(|a| *a /= count.max(1.0));
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
            v.iter_mut().for_each(|a| *a /= norm);
            v
        })
        .collect()
}

fn run(m: &mut Model, batch: &[Vec<u32>], pad: u32) -> Result<Vec<Vec<f32>>> {
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let (ids, mask, shape) = pad_batch(batch, pad);
    let (b, l) = (batch.len(), shape[1] as usize);
    let mut inputs: Vec<(Cow<'static, str>, SessionInputValue<'static>)> = vec![
        (
            "input_ids".into(),
            Tensor::from_array((shape, ids)).map_err(ort_err)?.into(),
        ),
        (
            "attention_mask".into(),
            Tensor::from_array((shape, mask.clone()))
                .map_err(ort_err)?
                .into(),
        ),
    ];
    if m.token_types {
        inputs.push((
            "token_type_ids".into(),
            Tensor::from_array((shape, vec![0i64; b * l]))
                .map_err(ort_err)?
                .into(),
        ));
    }
    let outputs = m.session.run(inputs).map_err(ort_err)?;
    let hidden = match outputs.get("last_hidden_state") {
        Some(v) => v,
        None if outputs.len() > 0 => &outputs[0],
        None => return Err(ort_err("the model returned no output")),
    };
    let (out_shape, data) = hidden.try_extract_tensor::<f32>().map_err(ort_err)?;
    if out_shape.len() != 3
        || out_shape[0] as usize != b
        || out_shape[1] as usize != l
        || data.len() != b * l * out_shape[2] as usize
    {
        return Err(ort_err(format!(
            "unexpected output shape {:?} for {b} texts of {l} tokens",
            &out_shape[..]
        )));
    }
    Ok(mean_pool(data, &mask, b, l, out_shape[2] as usize))
}

impl OnnxEmbedder {
    pub fn new(models_dir: &Path, opts: OnnxOptions) -> Result<Self> {
        if !is_model_present(models_dir) {
            return Err(AliceError::Other(
                "the embedding model is not downloaded; run \"alice model fetch\"".into(),
            ));
        }
        let dir = model_dir(models_dir);
        let tokenizer = UnigramTokenizer::from_file(&dir.join("tokenizer.json"))?;
        let onnx = dir.join("model_quantized.onnx");
        let threads = opts.intra_threads;
        let model = Lazy::new(Box::new(move || load(&onnx, threads)), opts.idle_unload)?;
        Ok(Self { tokenizer, model })
    }

    fn encode_all(&self, prefix: &str, texts: &[String]) -> Vec<Vec<u32>> {
        texts
            .iter()
            .map(|t| self.tokenizer.encode(&format!("{prefix}{t}"), MAX_TOKENS))
            .collect()
    }

    /// Token count of each passage as the model sees it (prefix included, capped at 512).
    #[doc(hidden)]
    pub fn token_counts(&self, texts: &[String]) -> Vec<usize> {
        self.encode_all("passage: ", texts)
            .iter()
            .map(Vec::len)
            .collect()
    }

    /// The batches `embed_passages` runs for `texts`, in order.
    #[doc(hidden)]
    pub fn passage_batches(&self, texts: &[String]) -> Vec<std::ops::Range<usize>> {
        token_batches(&self.token_counts(texts), MAX_BATCH, BATCH_TOKENS)
    }
}

impl Embedder for OnnxEmbedder {
    fn model_id(&self) -> &str {
        MODEL_ID
    }
    fn dim(&self) -> usize {
        DIM
    }
    fn is_loaded(&self) -> bool {
        self.model.is_loaded()
    }
    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let encoded = self.encode_all("passage: ", texts);
        let lens: Vec<usize> = encoded.iter().map(Vec::len).collect();
        let mut out = Vec::with_capacity(texts.len());
        for range in token_batches(&lens, MAX_BATCH, BATCH_TOKENS) {
            // The session is handed to a waiting query after each batch.
            out.extend(
                self.model
                    .with_blocking(|m| run(m, &encoded[range], PAD_ID))??,
            );
        }
        Ok(out)
    }
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let ids = self.encode_all("query: ", std::slice::from_ref(&text.to_string()));
        match self.model.with_loaded(|m| run(m, &ids, PAD_ID)) {
            Some(r) => r?
                .pop()
                .ok_or_else(|| AliceError::Other("embedding returned nothing".into())),
            // A failed (re)load is named, so the notice is not "loading" forever; the next
            // query tries again.
            None => Err(AliceError::Other(match self.model.last_error() {
                Some(why) => format!("{MODEL_LOADING} ({why})"),
                None => MODEL_LOADING.into(),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::token_batches;
    use super::*;

    /// The daemon runs in the background: at most four threads unless asked for more.
    #[test]
    fn the_default_thread_count_is_capped_for_a_background_daemon() {
        assert_eq!(threads_for(None, Some(14)), DEFAULT_INTRA_THREADS);
        assert_eq!(threads_for(None, Some(8)), 4);
        assert_eq!(threads_for(None, Some(2)), 2, "never more than the cores");
        assert_eq!(threads_for(None, Some(1)), 1);
        assert_eq!(threads_for(None, None), 2, "cores unknown");
        assert_eq!(threads_for(Some(8), Some(14)), 8, "an explicit count wins");
        assert_eq!(threads_for(Some(1), Some(2)), 1);
    }

    #[test]
    fn passages_are_batched_by_padded_tokens_in_order() {
        // Short texts (14 tokens each): up to 32 per batch.
        let short = vec![14; 70];
        assert_eq!(
            token_batches(&short, 32, 1_024),
            vec![0..32, 32..64, 64..70]
        );
        // Texts at the 512-token cap: two per batch of 1,024.
        let long = vec![512; 5];
        assert_eq!(token_batches(&long, 32, 1_024), vec![0..2, 2..4, 4..5]);
        // One long text after short ones closes the batch: padding would exceed the budget.
        let mut mixed = vec![24; 10];
        mixed.extend([512, 24]);
        assert_eq!(token_batches(&mixed, 32, 1_024), vec![0..10, 10..12]);
        // A text over the budget on its own still gets a batch; nothing is lost or reordered.
        assert_eq!(
            token_batches(&long, 32, 100),
            vec![0..1, 1..2, 2..3, 3..4, 4..5]
        );
        assert!(token_batches(&[], 32, 1_024).is_empty());
        for counts in [&short, &long, &mixed] {
            let ranges = token_batches(counts, 32, 1_024);
            assert_eq!(ranges.first().map(|r| r.start), Some(0));
            assert_eq!(ranges.last().map(|r| r.end), Some(counts.len()));
            assert!(ranges.windows(2).all(|w| w[0].end == w[1].start));
            for r in &ranges {
                let longest = counts[r.clone()].iter().max().copied().unwrap();
                assert!(r.len() == 1 || r.len() * longest <= 1_024, "{r:?}");
            }
        }
    }

    #[test]
    fn loading_without_the_model_says_how_to_fetch_it() {
        let d = tempfile::tempdir().unwrap();
        let Err(e) = OnnxEmbedder::new(d.path(), OnnxOptions::default()) else {
            panic!("loaded a model that is not there")
        };
        assert!(e.to_string().contains("alice model fetch"), "{e}");
    }

    #[test]
    fn a_batch_is_padded_with_the_pad_id_and_masked() {
        let (ids, mask, shape) = pad_batch(&[vec![0, 5, 2], vec![0, 7, 8, 9, 2]], 1);
        assert_eq!(shape, [2, 5]);
        assert_eq!(ids, vec![0, 5, 2, 1, 1, 0, 7, 8, 9, 2]);
        assert_eq!(mask, vec![1, 1, 1, 0, 0, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn mean_pooling_skips_padding_and_normalises() {
        // Two texts, three positions, hidden size 2; the second text's last position is padding.
        let hidden = [
            1.0, 0.0, 3.0, 0.0, 9.0, 9.0, 0.0, 2.0, 0.0, 4.0, 100.0, -100.0,
        ];
        let mask = [1, 1, 1, 1, 1, 0];
        let v = mean_pool(&hidden, &mask, 2, 3, 2);
        // Text 1: mean (13/3, 3) normalised; text 2: mean (0, 3) normalised.
        let n = (13.0f32 / 3.0).hypot(3.0);
        assert!((v[0][0] - 13.0 / 3.0 / n).abs() < 1e-6 && (v[0][1] - 3.0 / n).abs() < 1e-6);
        assert_eq!(v[1], vec![0.0, 1.0]);
    }
}
