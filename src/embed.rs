//! Pure-Rust sentence embeddings (all-MiniLM-L6-v2, mean pooling, L2 normalized).
//!
//! No Python, PyTorch or native runtime library: the same weights sentence-transformers uses
//! are run with `candle`. Model files are taken from, in order: `CONTEXT_BROKER_MODEL_DIR`,
//! an existing Hugging Face cache, the broker's own cache (downloaded once on first use).

use crate::config;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use std::path::{Path, PathBuf};
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

pub const MODEL_NAME: &str = "all-MiniLM-L6-v2";
const MODEL_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";
const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];
const MAX_TOKENS: usize = 256;

pub struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
}

fn complete(dir: &Path) -> bool {
    MODEL_FILES.iter().all(|file| dir.join(file).is_file())
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn model_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var("CONTEXT_BROKER_MODEL_DIR").ok().filter(|v| !v.is_empty()).map(PathBuf::from) {
        return if complete(&dir) { Ok(dir) } else { Err(format!("CONTEXT_BROKER_MODEL_DIR lacks {MODEL_FILES:?}")) };
    }
    let hub = std::env::var("HF_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".cache/huggingface")).join("hub");
    let snapshots = hub.join(format!("models--{}", MODEL_REPO.replace('/', "--"))).join("snapshots");
    if let Some(dir) = std::fs::read_dir(snapshots).into_iter().flatten().flatten().map(|e| e.path()).find(|p| complete(p)) {
        return Ok(dir);
    }
    let dir = config::model_cache_dir().join(MODEL_NAME);
    if !complete(&dir) {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        for file in MODEL_FILES {
            let url = format!("https://huggingface.co/{MODEL_REPO}/resolve/main/{file}");
            eprintln!("[broker] downloading {file} (one-time)");
            let mut response = ureq::get(&url).call().map_err(|e| format!("download {url}: {e}"))?;
            let tmp = dir.join(format!("{file}.part"));
            let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            std::io::copy(&mut response.body_mut().as_reader(), &mut out).map_err(|e| format!("download {url}: {e}"))?;
            std::fs::rename(&tmp, dir.join(file)).map_err(|e| e.to_string())?;
        }
    }
    Ok(dir)
}

impl Embedder {
    pub fn load() -> Result<Self, String> {
        let dir = model_dir()?;
        let err = |what: &str, e: &dyn std::fmt::Display| format!("embedding model {what}: {e}");
        let config: Config = serde_json::from_slice(&std::fs::read(dir.join("config.json")).map_err(|e| err("config", &e))?)
            .map_err(|e| err("config", &e))?;
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| err("tokenizer", &e))?;
        tokenizer.with_padding(Some(PaddingParams::default()));
        tokenizer
            .with_truncation(Some(TruncationParams { max_length: MAX_TOKENS, ..Default::default() }))
            .map_err(|e| err("tokenizer", &e))?;
        // SAFETY: the weights file is not modified while mapped; it is written once, atomically.
        let weights = unsafe { VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DTYPE, &Device::Cpu) }
            .map_err(|e| err("weights", &e))?;
        let model = BertModel::load(weights, &config).map_err(|e| err("weights", &e))?;
        Ok(Self { model, tokenizer })
    }

    /// One normalized vector per input text.
    ///
    /// Batches run on a few worker threads: for a model this small, parallelism across
    /// batches scales far better than parallelism inside each matrix multiply.
    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let batches: Vec<&[String]> = texts.chunks(config::EMBED_BATCH).collect();
        let workers = config::embed_threads().min(batches.len()).max(1);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let mut results: Vec<(usize, Result<Vec<Vec<f32>>, String>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(batch) = batches.get(i) else { break done };
                            done.push((i, self.embed_batch(batch).map_err(|e| format!("embedding: {e}"))));
                        }
                    })
                })
                .collect();
            handles.into_iter().flat_map(|handle| handle.join().expect("embedding worker")).collect()
        });
        results.sort_by_key(|(i, _)| *i);
        let mut vectors = Vec::with_capacity(texts.len());
        for (_, batch) in results {
            vectors.extend(batch?);
        }
        Ok(vectors)
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error + Send + Sync>> {
        let encodings = self.tokenizer.encode_batch(texts.to_vec(), true)?;
        let (rows, width) = (encodings.len(), encodings.first().map_or(0, |e| e.get_ids().len()));
        let flat = |pick: fn(&tokenizers::Encoding) -> &[u32]| -> candle_core::Result<Tensor> {
            let data: Vec<u32> = encodings.iter().flat_map(|e| pick(e).iter().copied()).collect();
            Tensor::from_vec(data, (rows, width), &Device::Cpu)
        };
        let ids = flat(|e| e.get_ids())?;
        let mask = flat(|e| e.get_attention_mask())?;
        let hidden = self.model.forward(&ids, &ids.zeros_like()?, Some(&mask))?;
        // Mean over real (unpadded) tokens, then L2 normalize.
        let weights = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
        let mean = hidden.broadcast_mul(&weights)?.sum(1)?.broadcast_div(&weights.sum(1)?)?;
        let mut vectors: Vec<Vec<f32>> = mean.to_vec2()?;
        for vector in &mut vectors {
            let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                vector.iter_mut().for_each(|x| *x /= norm);
            }
        }
        Ok(vectors)
    }
}
