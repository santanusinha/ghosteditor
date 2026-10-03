//! Background registry refresh (P2.5 remainder).
//!
//! Downloads the mason-registry compiled JSON for a release tag, verifies
//! sha256 against checksums.txt, transforms it with the embedded overlay,
//! writes the cache, and returns the new `Registry`. The caller swaps it
//! into the `RwLock<Registry>` the server holds.
//!
//! Blocking network work runs inside `spawn_blocking`. The pure transform
//! and the cache write are testable without network.

use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use sha2::Digest;

use super::transform::{transform_packages, OverlayEntry, Snapshot};
use super::Registry;

const REPO: &str = "mason-org/mason-registry";

/// The overlay ships inside the binary; refresh never reads the repo copy.
pub const OVERLAY: &str = include_str!("../../registry/overlay.toml");

// ---------- pure parts ----------

#[derive(Deserialize)]
struct OverlayFile {
    #[serde(default)]
    server: Vec<OverlayEntry>,
}

/// Parse the mason packages JSON and build a snapshot with our overlay.
pub fn build_snapshot(packages_text: &str, pin: &str) -> Result<Snapshot, String> {
    let packages: Vec<Value> =
        serde_json::from_str(packages_text).map_err(|e| format!("registry.json parse: {e}"))?;
    let overlay_file: OverlayFile =
        toml::from_str(OVERLAY).map_err(|e| format!("embedded overlay parse: {e}"))?;
    let snapshot = transform_packages(&packages, &overlay_file.server, pin);
    if snapshot.recipes.is_empty() {
        return Err("refresh produced zero recipes; refusing".to_string());
    }
    Ok(snapshot)
}

/// A Registry view of a snapshot.
pub fn registry_from_snapshot(snapshot: &Snapshot) -> Registry {
    Registry {
        pin: snapshot.pin.clone(),
        recipes: snapshot.recipes.clone(),
    }
}

/// Write a snapshot as the cache file at an explicit path.
pub fn write_cache_at(path: &Path, snapshot: &Snapshot) -> Result<(), String> {
    let text = serde_json::to_string_pretty(snapshot).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text + "\n").map_err(|e| format!("write {}: {e}", path.display()))
}

/// Write a snapshot to the default cache location.
pub fn write_cache(snapshot: &Snapshot) -> Result<(), String> {
    let path = super::cache_path().ok_or("no HOME; cannot write the registry cache")?;
    write_cache_at(&path, snapshot)
}

// ---------- blocking network parts ----------

/// Refresh the registry from a release tag. "latest" resolves the newest
/// release first. Runs the blocking work in `spawn_blocking`.
pub async fn refresh(tag: &str) -> Result<Registry, String> {
    let tag = tag.to_string();
    tokio::task::spawn_blocking(move || refresh_blocking(&tag))
        .await
        .map_err(|e| format!("refresh task: {e}"))?
}

fn refresh_blocking(tag: &str) -> Result<Registry, String> {
    // A hard per-call bound. Without it a stalled connection blocks the
    // spawn_blocking thread past the 60s refresh cap.
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let resolved = if tag == "latest" {
        latest_tag(&agent)?
    } else {
        tag.to_string()
    };
    let packages_text = download_release(&agent, &resolved)?;
    let snapshot = build_snapshot(&packages_text, &resolved)?;
    // Cache write failure is not fatal: the in-memory swap still works.
    if let Err(error) = write_cache(&snapshot) {
        // Best effort only; a read-only cache must not block a refresh.
        let _ = error;
    }
    Ok(registry_from_snapshot(&snapshot))
}

/// Resolve the newest release tag from the GitHub API.
fn latest_tag(agent: &ureq::Agent) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let text = agent
        .get(&url)
        .set("User-Agent", "ghostedit-registry-refresh")
        .call()
        .map_err(|e| format!("latest release lookup: {e}"))?
        .into_string()
        .map_err(|e| e.to_string())?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("latest release parse: {e}"))?;
    value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| "latest release has no tag_name".to_string())
}

/// Download and verify the compiled registry for a release tag.
fn download_release(agent: &ureq::Agent, tag: &str) -> Result<String, String> {
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");

    // checksums.txt: "<sha256>  registry.json\n<sha256>  registry.json.zip"
    let checksums = agent
        .get(&format!("{base}/checksums.txt"))
        .call()
        .map_err(|e| format!("checksums fetch: {e}"))?
        .into_string()
        .map_err(|e| e.to_string())?;
    let zip_sha = checksums
        .lines()
        .find(|l| l.trim_end().ends_with("registry.json.zip"))
        .and_then(|l| l.split_whitespace().next())
        .ok_or("checksums.txt has no registry.json.zip entry")?
        .to_string();

    let mut reader = agent
        .get(&format!("{base}/registry.json.zip"))
        .call()
        .map_err(|e| format!("registry.json.zip fetch: {e}"))?
        .into_reader()
        .take(64 * 1024 * 1024);
    // The zip is small (about 75 KB); a bounded read keeps the cap explicit.
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| format!("registry.json.zip read: {e}"))?;

    let hex: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if hex != zip_sha {
        return Err(format!("sha256 mismatch: want {zip_sha}, got {hex}"));
    }

    // The zip holds one file, registry.json.
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|_| "zip open".to_string())?;
    let mut name: Option<String> = None;
    for i in 0..archive.len() {
        let file = archive.by_index(i).map_err(|e| e.to_string())?;
        if file.name().ends_with("registry.json") {
            name = Some(file.name().to_string());
            break;
        }
    }
    let name = name.ok_or("registry.json not found in the zip")?;
    let mut entry = archive.by_name(&name).map_err(|e| e.to_string())?;
    let mut text = String::new();
    entry.read_to_string(&mut text).map_err(|e| e.to_string())?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        serde_json::json!([
            {
                "name": "pyright",
                "homepage": "https://example.com/pyright",
                "languages": ["Python"],
                "categories": ["LSP"],
                "source": { "id": "pkg:npm/pyright@1.1.414" },
                "bin": {
                    "pyright": "npm:pyright",
                    "pyright-langserver": "npm:pyright-langserver"
                }
            },
            {
                "name": "ruff",
                "homepage": "https://example.com/ruff",
                "languages": ["Python"],
                "categories": ["Linter"],
                "source": { "id": "pkg:pypi/ruff@0.1.0" }
            }
        ])
        .to_string()
    }

    #[test]
    fn builds_a_snapshot_with_the_embedded_overlay() {
        let snapshot = build_snapshot(&fixture(), "test-tag").unwrap();
        assert_eq!(snapshot.pin, "test-tag");
        assert_eq!(snapshot.recipes.len(), 1);
        let recipe = snapshot.recipes[0].clone();
        assert_eq!(recipe.name, "pyright");
        let overlay = recipe.overlay.expect("pyright has an overlay entry");
        assert_eq!(overlay.language, "python");
        assert_eq!(overlay.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn empty_registry_is_refused() {
        let error = build_snapshot("[]", "tag").unwrap_err();
        assert!(error.contains("zero recipes"), "was {error}");
    }

    #[test]
    fn cache_round_trips_through_registry_load_format() {
        let snapshot = build_snapshot(&fixture(), "rt-tag").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/registry.json");
        write_cache_at(&path, &snapshot).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // The cache must deserialize into the same shape Registry::load uses.
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["pin"], "rt-tag");
        assert_eq!(parsed["recipes"].as_array().unwrap().len(), 1);
    }
}
