//! Project root resolution and source file collection.

use crate::config::{self, IGNORE_DIRS, PROJECT_MARKERS, SUPPORTED_EXTENSIONS};
use crate::security;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static LOCKED_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Confine every call to `CONTEXT_BROKER_PROJECT_ROOT`. Used when serving remote clients, which
/// must not be able to point the server at other folders of the machine.
pub fn lock_root() -> Result<PathBuf, String> {
    let configured = std::env::var("CONTEXT_BROKER_PROJECT_ROOT").ok().filter(|v| !v.is_empty()).ok_or("CONTEXT_BROKER_PROJECT_ROOT is required when serving over HTTP")?;
    let root = PathBuf::from(&configured).canonicalize().map_err(|e| format!("project root {configured}: {e}"))?;
    Ok(LOCKED_ROOT.get_or_init(|| root).clone())
}

/// The folder every call is confined to when serving remote clients.
pub fn locked_root() -> Option<&'static Path> {
    LOCKED_ROOT.get().map(PathBuf::as_path)
}

/// True when some folder of `path` below `root` is hidden (`.git`, `.ssh`, `.context-broker`, ...).
pub fn in_hidden_folder(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rest| rest.components().any(|part| part.as_os_str().to_string_lossy().starts_with('.')))
}

/// `path` inside `root`, resolved; refused when it leaves the root (`..`, absolute paths, symlinks).
pub fn inside(root: &Path, path: &str) -> Result<PathBuf, String> {
    let resolved = root.join(path.trim()).canonicalize().map_err(|_| format!("not found: {path}"))?;
    if resolved.starts_with(root) { Ok(resolved) } else { Err(format!("outside the project: {path}")) }
}

/// Explicit argument, then `CONTEXT_BROKER_PROJECT_ROOT`, then marker detection, then CWD.
pub fn resolve_root(explicit: &str) -> Result<PathBuf, String> {
    if let Some(locked) = LOCKED_ROOT.get() {
        if explicit.trim().is_empty() {
            return Ok(locked.clone());
        }
        // A sub-folder of the served one, but never a hidden one: as a root, `.git` or `.ssh`
        // would make its own files look like ordinary project files.
        let root = inside(locked, explicit).map_err(|_| format!("project_root must be inside {}", locked.display()))?;
        if in_hidden_folder(locked, &root) {
            return Err(format!("project_root cannot be a hidden folder: {explicit}"));
        }
        return if root.is_dir() { Ok(root) } else { Err(format!("project root is not a directory: {explicit}")) };
    }
    let candidate = if !explicit.trim().is_empty() {
        PathBuf::from(explicit.trim())
    } else if let Some(env_root) = std::env::var("CONTEXT_BROKER_PROJECT_ROOT").ok().filter(|v| !v.is_empty()) {
        PathBuf::from(env_root)
    } else {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        detect_root(&cwd).unwrap_or(cwd)
    };
    let root = candidate.canonicalize().map_err(|e| format!("project root {}: {e}", candidate.display()))?;
    if !root.is_dir() {
        return Err(format!("project root is not a directory: {}", root.display()));
    }
    Ok(root)
}

/// Walk upwards and pick the ancestor with the strongest project markers.
fn detect_root(start: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for dir in start.ancestors().take(12) {
        let score: u32 = PROJECT_MARKERS.iter().filter(|(name, _)| dir.join(name).exists()).map(|(_, s)| s).sum();
        if score > 0 && best.as_ref().is_none_or(|(top, _)| score > *top) {
            best = Some((score, dir.to_path_buf()));
        }
    }
    best.map(|(_, dir)| dir)
}

pub fn project_name(root: &Path) -> String {
    root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "unknown".into())
}

pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// Indexable source files, sorted. Honors `.gitignore`/`.ignore`, never follows symlinks, and
/// always drops ignored directories, unsupported extensions, oversized and secret-named files.
pub fn collect_files(root: &Path) -> Vec<PathBuf> {
    let max_bytes = config::max_file_bytes();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !(entry.file_type().is_some_and(|t| t.is_dir())
                && (IGNORE_DIRS.contains(&name.as_ref()) || name.ends_with(".egg-info")))
        })
        .build();
    let mut files: Vec<PathBuf> = walker
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter(|entry| {
            let path = entry.path();
            let supported = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
            supported
                && !security::is_secret_path(&relative(root, path))
                && entry.metadata().is_ok_and(|m| max_bytes == 0 || m.len() <= max_bytes)
        })
        .map(|entry| entry.into_path())
        .collect();
    files.sort();
    files
}

/// Read at most `max_chars` characters; `None` when unreadable or blocked as secret-bearing.
pub fn read_safe(root: &Path, path: &Path, max_chars: usize) -> Option<String> {
    if security::is_secret_path(&relative(root, path)) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let content: String = text.chars().take(max_chars).collect();
    if security::has_secret_content(&content) {
        return None;
    }
    Some(content)
}
