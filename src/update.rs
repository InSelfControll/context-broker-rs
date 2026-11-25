//! Update check. Once a day the broker asks GitHub for the latest release and, when it is newer
//! than this build, says so once a day: on stderr, and for a local agent as one extra line on a
//! tool reply. With `CONTEXT_BROKER_UPDATE=auto` it also installs the release binary in place of
//! this one (checksum verified, test-run, previous binary kept as `.bak`); the new version runs
//! from the next start. `CONTEXT_BROKER_UPDATE=off` makes no request at all.

use crate::config;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REPO: &str = "https://github.com/InSelfControll/context-broker-rs";
const LATEST_API: &str = "https://api.github.com/repos/InSelfControll/context-broker-rs/releases/latest";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const HOUR: u64 = 60 * 60;
const DAY: u64 = 24 * HOUR;
const MAX_ARCHIVE_BYTES: u64 = 200_000_000;

/// The release asset built for this platform, when there is one.
const TARGET: Option<&str> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-musl")
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some("aarch64-unknown-linux-musl")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else {
    None
};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Off,
    Notify,
    Auto,
}

fn mode() -> Mode {
    match std::env::var("CONTEXT_BROKER_UPDATE").unwrap_or_default().trim().to_lowercase().as_str() {
        "off" | "0" | "false" | "no" => Mode::Off,
        "auto" => Mode::Auto,
        _ => Mode::Notify,
    }
}

/// A note waiting to be shown to the agent, with the version it is about.
static NOTE: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Check in the background so a slow or absent network never delays a reply. A remote server has
/// no agent of its own to tell, so there the note goes to the log only.
pub fn spawn(remote: bool) {
    let mode = mode();
    if mode == Mode::Off {
        return;
    }
    std::thread::spawn(move || {
        let Some((note, version)) = check(mode) else { return };
        eprintln!("[broker] {note}");
        if remote {
            mark_notified(&version);
        } else {
            *NOTE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((note, version));
        }
    });
}

/// The pending note, handed out once. It is then not repeated for a day, in any process.
pub fn take_note() -> Option<String> {
    let (note, version) = NOTE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()?;
    mark_notified(&version);
    Some(format!("[context-broker] {note} Please tell the user."))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

fn state_path() -> PathBuf {
    config::storage_dir().join("update.json")
}

fn read_state() -> Value {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn write_state(state: &Value) {
    let path = state_path();
    let tmp = path.with_extension("json.part");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&tmp, state.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn mark_notified(version: &str) {
    let mut state = read_state();
    state["notified"] = json!(version);
    state["notified_at"] = json!(now());
    write_state(&state);
}

/// `1.2.3` or `v1.2.3` as numbers; anything else (a pre-release, a stray tag) is not a version.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().trim_start_matches('v').split('.').map(|part| part.parse::<u64>().ok());
    let parsed = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(parsed)
}

fn newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(latest), Some(current)) if latest > current)
}

/// The note to show, with the version it is about; `None` when there is nothing new to say.
fn check(mode: Mode) -> Option<(String, String)> {
    let mut state = read_state();
    if now().saturating_sub(state["checked_at"].as_u64().unwrap_or(0)) >= DAY {
        match latest_release() {
            Ok(tag) => {
                state["latest"] = json!(tag);
                state["checked_at"] = json!(now());
            }
            // Offline is normal: stay quiet and look again in an hour, not at every start.
            Err(_) => state["checked_at"] = json!(now().saturating_sub(DAY - HOUR)),
        }
        write_state(&state);
    }
    let latest = state["latest"].as_str()?.trim_start_matches('v').to_string();
    if !newer(&latest, CURRENT) || state["installed"].as_str() == Some(&latest) {
        return None;
    }
    let told_today = state["notified"].as_str() == Some(&latest) && now().saturating_sub(state["notified_at"].as_u64().unwrap_or(0)) < DAY;
    if told_today {
        return None;
    }
    let available = format!("Version {latest} is available (this is {CURRENT}): {REPO}/releases/latest");
    let note = match mode {
        Mode::Auto => match install(&latest) {
            Ok(backup) => {
                let mut state = read_state();
                state["installed"] = json!(latest);
                write_state(&state);
                format!("Updated from {CURRENT} to {latest}; it takes effect when the server next starts. The previous binary is kept as {backup}.")
            }
            Err(reason) => format!("{available}. Automatic install failed: {reason}."),
        },
        _ => format!("{available}. Set CONTEXT_BROKER_UPDATE=auto to install updates automatically, or =off to stop checking."),
    };
    Some((note, latest))
}

/// Fetch a URL. IPv4 is tried first: on a network whose IPv6 route is broken, a client that
/// starts with the IPv6 addresses waits out a timeout on each one. IPv6-only networks fail the
/// first attempt at once and are served by the second.
fn download(url: &str, limit: u64, seconds: u64) -> Result<Vec<u8>, String> {
    use ureq::config::IpFamily;
    let mut outcome = Err(String::new());
    for family in [IpFamily::Ipv4Only, IpFamily::Any] {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .ip_family(family)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(seconds)))
            .build()
            .into();
        outcome = agent
            .get(url)
            .header("User-Agent", concat!("context-broker/", env!("CARGO_PKG_VERSION")))
            .call()
            .and_then(|mut response| response.body_mut().with_config().limit(limit).read_to_vec())
            .map_err(|e| format!("download {url}: {e}"));
        if outcome.is_ok() {
            break;
        }
    }
    outcome
}

/// The tag of the latest release. `CONTEXT_BROKER_UPDATE_URL` replaces the address asked (used by
/// the tests); downloads always come from the project's own releases.
fn latest_release() -> Result<String, String> {
    let url = config::env_text("CONTEXT_BROKER_UPDATE_URL").unwrap_or_else(|| LATEST_API.to_string());
    let body = download(&url, 1_000_000, 10)?;
    let release: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    release["tag_name"].as_str().map(str::to_string).ok_or_else(|| "no tag_name in the release".to_string())
}

/// The `context-broker` file inside a release `.tar.gz`.
fn unpack(archive: &[u8]) -> Result<Vec<u8>, String> {
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(archive)
        .take(MAX_ARCHIVE_BYTES)
        .read_to_end(&mut tar)
        .map_err(|e| format!("unreadable archive: {e}"))?;
    let field = |bytes: &[u8]| String::from_utf8_lossy(bytes).split('\0').next().unwrap_or("").trim().to_string();
    let mut at = 0;
    while let Some(header) = tar.get(at..at + 512) {
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let size = usize::from_str_radix(&field(&header[124..136]), 8).map_err(|_| "unreadable archive".to_string())?;
        let body = at + 512;
        let regular_file = matches!(header[156], b'0' | 0);
        if regular_file && field(&header[..100]).trim_start_matches("./") == "context-broker" {
            return tar.get(body..body + size).map(<[u8]>::to_vec).ok_or_else(|| "truncated archive".to_string());
        }
        at = body + size.div_ceil(512) * 512;
    }
    Err("the archive holds no context-broker binary".into())
}

fn checksum(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A release binary must answer a ping before it replaces this one.
fn answers_ping(binary: &Path) -> bool {
    let child = std::process::Command::new(binary)
        .env("CONTEXT_BROKER_UPDATE", "off")
        .env_remove("CONTEXT_BROKER_HTTP_ADDR")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n");
    }
    child.wait_with_output().is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("\"result\""))
}

/// Replace the running binary with the release for this platform; returns where the old one is kept.
fn install(latest: &str) -> Result<String, String> {
    let target = TARGET.ok_or("there is no release binary for this platform")?;
    let exe = std::env::current_exe().and_then(|path| path.canonicalize()).map_err(|e| format!("cannot locate the running binary: {e}"))?;
    let dir = exe.parent().ok_or("cannot locate the running binary")?;
    let name = exe.file_name().ok_or("cannot locate the running binary")?.to_string_lossy();
    let staged = dir.join(format!(".{name}.new"));
    // Fail before downloading when the folder is read-only (a hardened service, a Nix store path).
    std::fs::write(&staged, b"").map_err(|e| format!("cannot write to {}: {e}", dir.display()))?;
    let installed = (|| {
        let url = format!("{REPO}/releases/download/v{latest}/context-broker-{target}.tar.gz");
        let archive = download(&url, MAX_ARCHIVE_BYTES, 300)?;
        let published = String::from_utf8_lossy(&download(&format!("{url}.sha256"), 10_000, 30)?).to_string();
        let expected = published.split_whitespace().next().unwrap_or("");
        if expected.len() != 64 || !expected.eq_ignore_ascii_case(&checksum(&archive)) {
            return Err("the download does not match its published checksum".to_string());
        }
        std::fs::write(&staged, unpack(&archive)?).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        }
        if !answers_ping(&staged) {
            return Err("the downloaded binary does not run here".to_string());
        }
        let backup = dir.join(format!("{name}.bak"));
        std::fs::copy(&exe, &backup).map_err(|e| e.to_string())?;
        // A rename swaps the file in one step; this process keeps running the old one.
        std::fs::rename(&staged, &exe).map_err(|e| e.to_string())?;
        Ok(backup.display().to_string())
    })();
    if installed.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    installed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("v0.10.0", "0.9.9") && newer("1.0.0", "0.99.99"));
        assert!(!newer("0.5.0", "0.5.0") && !newer("0.4.9", "0.5.0"));
        assert!(!newer("v1.0.0-rc1", "0.5.0") && !newer("nightly", "0.5.0") && !newer("1.0", "0.5.0"));
    }

    /// A tar entry: 512-byte header (name, octal size, type) and the content padded to 512.
    fn entry(name: &str, kind: u8, content: &[u8]) -> Vec<u8> {
        let mut block = vec![0u8; 512];
        block[..name.len()].copy_from_slice(name.as_bytes());
        block[124..135].copy_from_slice(format!("{:011o}", content.len()).as_bytes());
        block[156] = kind;
        block.extend_from_slice(content);
        block.resize(512 + content.len().div_ceil(512) * 512, 0);
        block
    }

    fn gzip(tar: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(tar).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn unpack_finds_the_binary_among_other_entries() {
        let mut tar = entry("./PaxHeader", b'x', b"18 path=whatever\n");
        tar.extend(entry("README.md", b'0', b"not it"));
        tar.extend(entry("./context-broker", b'0', &[7u8; 700]));
        tar.extend(vec![0u8; 1024]);
        assert_eq!(unpack(&gzip(&tar)).unwrap(), vec![7u8; 700]);

        let mut other = entry("context-broker", b'5', b"");
        other.extend(vec![0u8; 1024]);
        assert!(unpack(&gzip(&other)).is_err(), "a folder of that name is not the binary");
        assert!(unpack(b"not gzip").is_err());
    }

    #[test]
    fn checksum_is_lowercase_sha256() {
        assert_eq!(checksum(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
