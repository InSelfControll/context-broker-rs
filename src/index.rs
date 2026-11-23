//! Incremental semantic index: one embedding per file, re-embedding only what changed.

use crate::config;
use crate::embed::{Embedder, MODEL_NAME};
use crate::project::{self, relative};
use crate::snippet;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const CACHE_VERSION: u64 = 1;

/// `[mtime_ns, size]`; a file is re-read only when this changes.
type Signature = [u64; 2];

pub struct Hit {
    pub rel_path: String,
    pub snippet: String,
}

pub struct SearchResult {
    pub root: PathBuf,
    pub total_files: usize,
    pub hits: Vec<Hit>,
}

struct Index {
    fingerprint: String,
    paths: Vec<String>,
    signatures: Vec<Signature>,
    /// Row-major `paths.len() x dim`, each row L2-normalized.
    vectors: Vec<f32>,
    dim: usize,
    /// Unreadable or secret-bearing files, skipped again until they change.
    skipped: HashMap<String, Signature>,
}

#[derive(Default)]
pub struct Engine {
    model: Option<Embedder>,
    indexes: HashMap<PathBuf, Index>,
}

fn signature(path: &Path) -> Option<Signature> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos() as u64;
    Some([mtime, meta.len()])
}

fn fingerprint(entries: &[(String, Option<Signature>)]) -> String {
    let mut hasher = Sha256::new();
    for (path, sig) in entries {
        hasher.update(path.as_bytes());
        hasher.update(format!("\0{sig:?}\n").as_bytes());
    }
    hex(&hasher.finalize())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn cache_paths(root: &Path) -> (PathBuf, PathBuf) {
    let dir = root.join(".cache").join("context-broker-rs");
    (dir.join("index.json"), dir.join("index.bin"))
}

/// Largest embedding width a cache may claim (the model's is 384).
const MAX_DIM: usize = 4096;

/// A relative path that stays below its root: no absolute prefix, no `..`, no empty parts.
fn is_plain_relative(path: &str) -> bool {
    !path.is_empty() && Path::new(path).components().all(|part| matches!(part, std::path::Component::Normal(_)))
}

fn load_cache(root: &Path) -> Option<Index> {
    let (meta_path, vectors_path) = cache_paths(root);
    let meta: Value = serde_json::from_slice(&std::fs::read(meta_path).ok()?).ok()?;
    if meta["version"].as_u64()? != CACHE_VERSION || meta["model"].as_str()? != MODEL_NAME {
        return None;
    }
    // The cache sits inside the project, so it is untrusted input: a planted file must not point
    // reads outside the project or make the size check below overflow.
    let dim = usize::try_from(meta["dim"].as_u64()?).ok().filter(|dim| (1..=MAX_DIM).contains(dim))?;
    let sig = |v: &Value| -> Option<Signature> { Some([v.get(0)?.as_u64()?, v.get(1)?.as_u64()?]) };
    let mut paths = Vec::new();
    let mut signatures = Vec::new();
    for entry in meta["files"].as_array()? {
        let path = entry.get(0)?.as_str()?;
        if !is_plain_relative(path) {
            return None;
        }
        paths.push(path.to_string());
        signatures.push(sig(entry.get(1)?)?);
    }
    let skipped = meta["skipped"]
        .as_object()?
        .iter()
        .filter_map(|(path, value)| Some((path.clone(), sig(value)?)))
        .collect();
    let bytes = std::fs::read(vectors_path).ok()?;
    if paths.len().checked_mul(dim).and_then(|floats| floats.checked_mul(4)) != Some(bytes.len()) {
        return None;
    }
    let vectors = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    Some(Index { fingerprint: meta["fingerprint"].as_str()?.to_string(), paths, signatures, vectors, dim, skipped })
}

fn save_cache(root: &Path, index: &Index) {
    let (meta_path, vectors_path) = cache_paths(root);
    let files: Vec<Value> = index.paths.iter().zip(&index.signatures).map(|(p, s)| json!([p, s])).collect();
    let meta = json!({
        "version": CACHE_VERSION, "model": MODEL_NAME, "dim": index.dim,
        "fingerprint": index.fingerprint, "files": files, "skipped": index.skipped,
    });
    let bytes: Vec<u8> = index.vectors.iter().flat_map(|v| v.to_le_bytes()).collect();
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(meta_path.parent().expect("cache dir"))?;
        crate::memory::atomic_write(&vectors_path, &bytes)?;
        crate::memory::atomic_write(&meta_path, meta.to_string().as_bytes())
    };
    if let Err(error) = write() {
        eprintln!("[broker] index cache not saved: {error}");
    }
}

impl Engine {
    /// Free the model and in-memory indexes; the disk cache makes the next search cheap.
    pub fn release(&mut self) -> bool {
        let had = self.model.is_some() || !self.indexes.is_empty();
        self.model = None;
        self.indexes.clear();
        had
    }

    pub fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        if self.model.is_none() {
            self.model = Some(Embedder::load()?);
        }
        self.model.as_ref().expect("model loaded").embed(&texts)
    }

    fn refresh(&mut self, root: &Path) -> Result<(), String> {
        let files = project::collect_files(root);
        let current: Vec<(String, Option<Signature>)> =
            files.iter().map(|path| (relative(root, path), signature(path))).collect();
        let print = fingerprint(&current);
        if self.indexes.get(root).is_some_and(|index| index.fingerprint == print) {
            return Ok(());
        }
        let base = self.indexes.remove(root).or_else(|| load_cache(root));
        let listed: HashSet<&str> = current.iter().map(|(rel, _)| rel.as_str()).collect();
        // A matching fingerprint is trusted only when every cached file is one collected now.
        if let Some(index) = base.as_ref().filter(|index| index.fingerprint == print && index.paths.iter().all(|path| listed.contains(path.as_str()))) {
            eprintln!("[broker] index loaded from disk ({} files)", index.paths.len());
            self.indexes.insert(root.to_path_buf(), base.expect("checked"));
            return Ok(());
        }
        let known: HashMap<&str, (usize, Signature)> = base
            .as_ref()
            .map(|index| index.paths.iter().enumerate().map(|(row, p)| (p.as_str(), (row, index.signatures[row]))).collect())
            .unwrap_or_default();

        let mut next = Index { fingerprint: print, paths: vec![], signatures: vec![], vectors: vec![], dim: 0, skipped: HashMap::new() };
        let mut reused: Vec<Option<usize>> = Vec::new();
        let mut pending: Vec<String> = Vec::new();
        for (path, (rel, sig)) in files.iter().zip(current) {
            let Some(sig) = sig else { continue };
            if let Some((row, _)) = known.get(rel.as_str()).filter(|(_, old)| *old == sig) {
                reused.push(Some(*row));
            } else if base.as_ref().is_some_and(|index| index.skipped.get(&rel) == Some(&sig)) {
                next.skipped.insert(rel, sig);
                continue;
            } else if let Some(content) = project::read_safe(root, path, config::READ_MAX_CHARS) {
                let head: String = content.chars().take(config::EMBED_MAX_CHARS).collect();
                pending.push(format!("File: {rel}\nContent: {head}"));
                reused.push(None);
            } else {
                next.skipped.insert(rel, sig);
                continue;
            }
            next.paths.push(rel);
            next.signatures.push(sig);
        }
        let embedded_count = pending.len();
        let fresh = if pending.is_empty() { Vec::new() } else { self.embed(pending)? };
        next.dim = fresh.first().map(Vec::len).or(base.as_ref().map(|index| index.dim)).unwrap_or(0);
        let mut fresh = fresh.into_iter();
        for row in &reused {
            match (row, base.as_ref()) {
                (Some(row), Some(index)) => next.vectors.extend_from_slice(&index.vectors[row * index.dim..(row + 1) * index.dim]),
                _ => next.vectors.extend(fresh.next().ok_or("embedding count mismatch")?),
            }
        }
        if next.dim == 0 || next.vectors.len() != next.paths.len() * next.dim {
            // Nothing indexable, or the model changed dimension: keep no stale rows.
            next.vectors.clear();
            next.paths.clear();
            next.signatures.clear();
        }
        eprintln!(
            "[broker] index ready: {} files ({} reused, {} embedded, {} skipped)",
            next.paths.len(), next.paths.len().saturating_sub(embedded_count), embedded_count, next.skipped.len()
        );
        save_cache(root, &next);
        if embedded_count > config::EMBED_BATCH {
            trim_memory(); // bulk embedding leaves large freed arenas behind
        }
        self.indexes.insert(root.to_path_buf(), next);
        Ok(())
    }

    /// Top `top_k` files for `query`, each with a query-focused snippet.
    pub fn search(&mut self, root: &Path, query: &str, top_k: usize) -> Result<SearchResult, String> {
        if query.trim().is_empty() {
            return Err("query is required".into());
        }
        self.refresh(root)?;
        let total_files = self.indexes.get(root).map_or(0, |index| index.paths.len());
        if total_files == 0 {
            return Err(format!("no indexable files found in {}", root.display()));
        }
        let query_vector = self.embed(vec![query.to_string()])?.pop().ok_or("empty query embedding")?;
        let index = self.indexes.get(root).expect("index refreshed");
        let mut scored: Vec<(f32, usize)> = index
            .vectors
            .chunks_exact(index.dim)
            .enumerate()
            .map(|(row, vector)| (vector.iter().zip(&query_vector).map(|(a, b)| a * b).sum(), row))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let terms = snippet::query_terms(query);
        let (budget, context) = (config::max_tokens_per_file(), config::snippet_context_lines());
        let hits = scored
            .into_iter()
            .take(top_k.clamp(1, 20))
            .filter_map(|(_, row)| {
                let rel_path = index.paths[row].clone();
                let file = project::inside(root, &rel_path).ok()?;
                let content = project::read_safe(root, &file, config::READ_MAX_CHARS)?;
                let (snippet, _) = snippet::focused(&content, &terms, budget, context);
                (!snippet.is_empty()).then_some(Hit { rel_path, snippet })
            })
            .collect();
        Ok(SearchResult { root: root.to_path_buf(), total_files, hits })
    }
}

/// Hand freed heap pages back to the OS (glibc keeps them otherwise).
pub fn trim_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases unused heap memory.
    unsafe {
        libc::malloc_trim(0);
    }
    #[cfg(target_env = "musl")]
    // SAFETY: mi_collect only returns unused allocator memory to the OS.
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
}

/// Plain text: root stated once, relative paths, no JSON escaping of code.
pub fn render(result: &SearchResult) -> String {
    let mut out = format!("{} of {} files · root: {}", result.hits.len(), result.total_files, result.root.display());
    for hit in &result.hits {
        out.push_str(&format!("\n### {}\n{}", hit.rel_path, hit.snippet));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project holding only a planted cache (and a `.ignore` that hides every file).
    fn planted(name: &str, dim: u64, files: Value, vectors: usize) -> PathBuf {
        let root = std::env::temp_dir().join(format!("context-broker-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (meta, bin) = cache_paths(&root);
        std::fs::create_dir_all(meta.parent().unwrap()).unwrap();
        std::fs::write(root.join(".ignore"), "*\n").unwrap();
        let meta_json = json!({"version": CACHE_VERSION, "model": MODEL_NAME, "dim": dim,
            "fingerprint": fingerprint(&[]), "files": files, "skipped": {}});
        std::fs::write(meta, meta_json.to_string()).unwrap();
        std::fs::write(bin, vec![0u8; vectors]).unwrap();
        root.canonicalize().unwrap()
    }

    #[test]
    fn cache_dim_cannot_overflow_the_size_check() {
        // 1 path x 2^62 x 4 wraps to 0 bytes, which an empty vectors file would match.
        let root = planted("overflow", 1 << 62, json!([["src/a.rs", [0, 0]]]), 0);
        assert!(load_cache(&root).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_paths_cannot_leave_the_project() {
        for (name, path) in [("absolute", "/etc/hostname"), ("parent", "../../etc/hostname"), ("empty", "")] {
            let root = planted(name, 2, json!([[path, [0, 0]]]), 8);
            assert!(load_cache(&root).is_none(), "{path:?} accepted");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn matching_fingerprint_does_not_trust_unlisted_files() {
        // The fingerprint of an empty file list is predictable; the cache names a file that is
        // not collected now, so it must not be used as-is.
        let root = planted("unlisted", 2, json!([["notes/elsewhere.md", [0, 0]]]), 8);
        assert!(load_cache(&root).is_some(), "the cache itself is well-formed");
        let mut engine = Engine::default();
        engine.refresh(&root).unwrap();
        assert!(engine.indexes[&root].paths.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
