//! Settings (environment variables share the Python broker's names) and fixed policy tables.

use std::path::PathBuf;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// Token budget for each file snippet in a search reply.
pub fn max_tokens_per_file() -> usize {
    env_usize("CONTEXT_BROKER_RESULT_MAX_TOKENS_PER_FILE", 240).max(1)
}
/// Context lines kept on each side of a query-matching line.
pub fn snippet_context_lines() -> usize {
    env_usize("CONTEXT_BROKER_RESULT_SNIPPET_CONTEXT_LINES", 2)
}
/// CPU threads the embedding model may use.
pub fn embed_threads() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    env_usize("CONTEXT_BROKER_EMBED_THREADS", (cores / 2).clamp(1, 4)).max(1)
}
/// Drop the model and in-memory indexes after this many idle seconds (0 disables).
pub fn idle_timeout_secs() -> u64 {
    env_usize("CONTEXT_BROKER_IDLE_RESOURCE_TIMEOUT_SECONDS", 900) as u64
}
/// Skip files larger than this during collection.
pub fn max_file_bytes() -> u64 {
    env_usize("CONTEXT_BROKER_INDEX_MAX_FILE_BYTES", 2_000_000) as u64
}
/// Global storage for chat ledgers and saved results (same default as the Python broker).
pub fn storage_dir() -> PathBuf {
    match std::env::var("CONTEXT_BROKER_STORAGE_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(".context-broker"),
    }
}
/// Where the ONNX embedding model is downloaded once and reused.
pub fn model_cache_dir() -> PathBuf {
    match std::env::var("CONTEXT_BROKER_MODEL_CACHE_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(".cache").join("context-broker").join("models"),
    }
}
fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into()))
}

/// Characters read per file for secret scanning and snippets.
pub const READ_MAX_CHARS: usize = 40_000;
/// Characters of each file that are embedded.
pub const EMBED_MAX_CHARS: usize = 3_000;
/// Texts per inference batch (small batches run in parallel worker threads).
pub const EMBED_BATCH: usize = 8;
pub const IN_PROJECT_FOLDER: &str = ".context-broker";

pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "md", "json", "toml", "yaml", "yml", "xml", "properties", "gradle", "go", "py", "ts", "js", "rs",
    "java", "nix", "html", "css", "scss", "sass", "less", "sh", "bash", "zsh", "fish", "ps1", "sql",
    "graphql", "prisma",
];

/// Hard-coded: files that must never be indexed or returned, regardless of ignore files.
pub const SECRET_FILE_PATTERNS: &[&str] = &[
    ".env", ".env.*", "*.env", "*.env.*", ".aws/credentials", ".aws/config", "id_rsa", "id_dsa",
    "id_ecdsa", "id_ed25519", "*.pem", "*.key", "*.p12", "*.pfx", "*.kubeconfig", "kubeconfig",
    ".docker/config.json", ".pypirc", ".git-credentials", "*.tfstate", "*.tfstate.*", "*.dump",
    "*.sql.dump", "secrets.*", "secret.*", "*.secrets", "*.secret", "credentials.*", "*.credentials",
    "*.token", "*.tokens", "api_key*", "apikey*", "private_key*", "privatekey*",
];
/// Hard-coded: `KEY=value` style names whose presence marks a file as secret-bearing.
pub const SECRET_KEY_SIGNATURES: &[&str] = &[
    "PRIVATE_KEY", "SECRET_KEY", "API_KEY", "ACCESS_KEY", "AUTH_TOKEN", "PASSWORD", "SECRET",
    "CREDENTIAL", "TOKEN", "KEY",
];
/// Hard-coded: substrings whose presence anywhere on a line marks a file as secret-bearing.
pub const SECRET_VALUE_SIGNATURES: &[&str] = &[
    "-----BEGIN PRIVATE KEY-----", "-----BEGIN ENCRYPTED PRIVATE KEY-----",
    "-----BEGIN RSA PRIVATE KEY-----", "-----BEGIN EC PRIVATE KEY-----",
    "-----BEGIN DSA PRIVATE KEY-----", "-----BEGIN OPENSSH PRIVATE KEY-----", "_authToken", "_auth",
    "Bearer ",
];

pub const IGNORE_DIRS: &[&str] = &[
    "__pycache__", ".venv", ".uv", ".tox", ".pytest_cache", ".mypy_cache", ".coverage", "htmlcov",
    ".eggs", "venv", "env", ".env", "node_modules", ".next", ".nuxt", "dist", "build", ".output",
    ".git", ".svn", ".hg", "target", "bin", "out", ".gradle", ".idea", ".vscode", ".vs", ".settings",
    "result", "result-bin", "result-dev", "result-man", "result-lib", ".cache", ".context-broker",
    "coverage", "tmp", "temp", "logs",
];

pub const PROJECT_MARKERS: &[(&str, u32)] = &[
    (".git", 100), ("pyproject.toml", 50), ("package.json", 50), ("Cargo.toml", 50), ("go.mod", 50),
    ("pom.xml", 40), ("build.gradle", 40), ("CMakeLists.txt", 40), ("setup.py", 30),
    ("requirements.txt", 30), ("Makefile", 20), ("Dockerfile", 20), ("docker-compose.yml", 20),
    (".gitignore", 10), ("README.md", 10), ("LICENSE", 10),
];
