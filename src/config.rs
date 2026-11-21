//! Settings (environment variables share the Python broker's names) and fixed policy tables.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
/// Where chat memory and saved search results are kept.
pub enum Store {
    Json,
    /// Markdown notes inside this Obsidian vault.
    Obsidian(PathBuf),
    /// Graphify memory docs inside the project's `graphify-out/`.
    Graphify,
}
/// The store named by `CONTEXT_BROKER_STORAGE`; see [`resolve_store`].
pub fn store(root: &Path) -> Result<Store, String> {
    resolve_store(&std::env::var("CONTEXT_BROKER_STORAGE").unwrap_or_default(), root)
}
/// `json` (the default), `obsidian`, `graphify`, or `auto`: Obsidian when a vault is configured or
/// detected, else Graphify when the project or machine has it, else JSON. Naming a vault with
/// `CONTEXT_BROKER_OBSIDIAN_VAULT` alone selects Obsidian.
pub fn resolve_store(kind: &str, root: &Path) -> Result<Store, String> {
    let kind = kind.trim().to_lowercase();
    let configured = env_text("CONTEXT_BROKER_OBSIDIAN_VAULT").map(PathBuf::from);
    let vault = || configured.clone().or_else(detected_vault);
    match kind.as_str() {
        "json" => Ok(Store::Json),
        "graphify" => Ok(Store::Graphify),
        _ if configured.as_ref().is_some_and(|vault| !vault.is_dir()) => {
            Err(format!("Obsidian vault not found: {}", configured.expect("checked").display()))
        }
        "" => Ok(configured.map_or(Store::Json, Store::Obsidian)),
        "obsidian" => vault().map(Store::Obsidian).ok_or_else(|| "no Obsidian vault found; set CONTEXT_BROKER_OBSIDIAN_VAULT to the vault folder".into()),
        "auto" => Ok(match vault() {
            Some(vault) => Store::Obsidian(vault),
            None if graphify_detected(root) => Store::Graphify,
            None => Store::Json,
        }),
        other => Err(format!("unknown store '{other}' (use json, obsidian, graphify or auto)")),
    }
}
/// The vault Obsidian itself lists in its settings: the one open now, else the most recently used.
/// `CONTEXT_BROKER_OBSIDIAN_CONFIG` names the settings file when it is not in a standard place.
pub fn detected_vault() -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let settings_files: Vec<PathBuf> = match env_text("CONTEXT_BROKER_OBSIDIAN_CONFIG") {
                Some(file) => vec![PathBuf::from(file)],
                None => env_text("XDG_CONFIG_HOME")
                    .map(|dir| PathBuf::from(dir).join("obsidian"))
                    .into_iter()
                    .chain(env_text("APPDATA").map(|dir| PathBuf::from(dir).join("obsidian")))
                    .chain(
                        [".config/obsidian", ".var/app/md.obsidian.Obsidian/config/obsidian",
                         "snap/obsidian/current/.config/obsidian", "Library/Application Support/obsidian"]
                            .map(|dir| home().join(dir)),
                    )
                    .map(|dir| dir.join("obsidian.json"))
                    .collect(),
            };
            let mut vaults: Vec<(bool, u64, PathBuf)> = settings_files
                .iter()
                .filter_map(|file| serde_json::from_slice::<Value>(&std::fs::read(file).ok()?).ok())
                .filter_map(|settings| settings.get("vaults")?.as_object().cloned())
                .flat_map(|vaults| vaults.into_iter().map(|(_, vault)| vault))
                .filter_map(|vault| {
                    let path = PathBuf::from(vault.get("path")?.as_str()?);
                    let open = vault.get("open").and_then(Value::as_bool).unwrap_or(false);
                    path.is_dir().then(|| (open, vault.get("ts").and_then(Value::as_u64).unwrap_or(0), path))
                })
                .collect();
            vaults.sort();
            let found = vaults.pop().map(|(_, _, path)| path);
            if let Some(vault) = &found {
                eprintln!("[broker] detected Obsidian vault: {}", vault.display());
            }
            found
        })
        .clone()
}
/// Graphify is in use when the project has its output folder or its CLI is installed.
pub fn graphify_detected(root: &Path) -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    root.join(GRAPHIFY_OUT).is_dir()
        || *INSTALLED.get_or_init(|| {
            std::env::var_os("PATH")
                .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("graphify").is_file() || dir.join("graphify.exe").is_file()))
        })
}
/// A network as `(address, prefix length)`.
pub type Network = (std::net::IpAddr, u8);

/// `CONTEXT_BROKER_HTTP_ALLOW_IPS`: addresses or CIDR ranges (comma separated) that may use the
/// HTTP transport. Empty means no restriction by address.
pub fn http_allowed_networks() -> Result<Vec<Network>, String> {
    env_text("CONTEXT_BROKER_HTTP_ALLOW_IPS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (address, prefix) = entry.split_once('/').unwrap_or((entry, ""));
            let address: std::net::IpAddr = address.parse().map_err(|_| format!("CONTEXT_BROKER_HTTP_ALLOW_IPS: bad address '{entry}'"))?;
            let widest = if address.is_ipv4() { 32 } else { 128 };
            let prefix = if prefix.is_empty() { widest } else { prefix.parse::<u8>().ok().filter(|bits| *bits <= widest).ok_or_else(|| format!("CONTEXT_BROKER_HTTP_ALLOW_IPS: bad prefix in '{entry}'"))? };
            Ok((address, prefix))
        })
        .collect()
}

/// True when `client` lies inside `network` (same address family, same leading `prefix` bits).
pub fn network_contains(network: &Network, client: std::net::IpAddr) -> bool {
    use std::net::IpAddr::{V4, V6};
    let same_prefix = |a: u128, b: u128, width: u32, prefix: u8| prefix == 0 || (a ^ b) >> (width - u32::from(prefix)) == 0;
    match (network.0, client) {
        (V4(net), V4(ip)) => same_prefix(u32::from(net).into(), u32::from(ip).into(), 32, network.1),
        (V6(net), V6(ip)) => same_prefix(u128::from(net), u128::from(ip), 128, network.1),
        _ => false,
    }
}

fn env_text(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}
/// True when notes follow the vault's existing AI-chat convention (`AI Chats/Chats/<platform>/`,
/// `type: ai-chat` notes, Source and Project index notes) instead of the broker's own folder.
/// `CONTEXT_BROKER_OBSIDIAN_LAYOUT` forces `ai-chats` or `broker`; otherwise the vault decides.
pub fn obsidian_ai_chats(vault: &Path) -> bool {
    match env_text("CONTEXT_BROKER_OBSIDIAN_LAYOUT").unwrap_or_default().trim().to_lowercase().as_str() {
        "ai-chats" => true,
        "broker" => false,
        _ => vault.join(AI_CHATS).join("Chats").is_dir(),
    }
}
/// Where the vault puts new notes: the folder chosen in Obsidian's "Default location for new
/// notes" setting, else the vault itself.
pub fn obsidian_new_notes(vault: &Path) -> PathBuf {
    let settings: Option<Value> = std::fs::read(vault.join(".obsidian").join("app.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let folder = settings.as_ref().filter(|app| app["newFileLocation"] == "folder").and_then(|app| app["newFileFolderPath"].as_str());
    match folder.map(|folder| folder.trim_matches('/')).filter(|folder| !folder.is_empty() && !folder.contains("..")) {
        Some(folder) => vault.join(folder),
        None => vault.to_path_buf(),
    }
}
/// Folder inside the vault that holds the broker's notes.
pub fn obsidian_folder() -> String {
    std::env::var("CONTEXT_BROKER_OBSIDIAN_FOLDER").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "Context Broker".into())
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
/// Graphify's output folder inside a project (graph, report, memory docs).
pub const GRAPHIFY_OUT: &str = "graphify-out";
/// Root folder of the AI-chat convention in an Obsidian vault.
pub const AI_CHATS: &str = "AI Chats";

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
    "result", "result-bin", "result-dev", "result-man", "result-lib", ".cache", ".context-broker", "graphify-out", ".obsidian",
    "coverage", "tmp", "temp", "logs",
];

pub const PROJECT_MARKERS: &[(&str, u32)] = &[
    (".git", 100), ("pyproject.toml", 50), ("package.json", 50), ("Cargo.toml", 50), ("go.mod", 50),
    ("pom.xml", 40), ("build.gradle", 40), ("CMakeLists.txt", 40), ("setup.py", 30),
    ("requirements.txt", 30), ("Makefile", 20), ("Dockerfile", 20), ("docker-compose.yml", 20),
    (".gitignore", 10), ("README.md", 10), ("LICENSE", 10),
];
