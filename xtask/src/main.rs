//! xtask: registry codegen (P2.5).
//!
//! `cargo run -p xtask -- registry-gen --pin <tag>`
//!   Download the mason-registry compiled JSON for a release tag, verify
//!   sha256, and emit src/registry/snapshot.json via the shared transform.
//!
//! `cargo run -p xtask -- registry-gen --from-file <registry.json> [--pin <tag>]`
//!   Same, from an already-downloaded file (offline bootstrap).

use std::io::Read;
use std::process::ExitCode;

use ghostedit::registry::transform::{transform_packages, OverlayEntry};
use serde::Deserialize;
use serde_json::Value;
use sha2::Digest;

const REPO: &str = "mason-org/mason-registry";
const PIN_FILE: &str = "registry/PIN";
const OVERLAY_FILE: &str = "registry/overlay.toml";
const SNAPSHOT_FILE: &str = "src/registry/snapshot.json";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("registry-gen") => registry_gen(&args[1..]),
        _ => Err("usage: xtask registry-gen [--pin <tag> | --from-file <path>]".into()),
    }
}

fn registry_gen(args: &[String]) -> Result<String, String> {
    let mut pin: Option<String> = std::fs::read_to_string(PIN_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut from_file: Option<String> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--pin" => pin = Some(iter.next().ok_or("--pin needs a value")?.clone()),
            "--from-file" => {
                from_file = Some(iter.next().ok_or("--from-file needs a value")?.clone())
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }

    let (json_text, resolved_pin) = match from_file {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("could not read {path}: {e}"))?;
            let tag = pin.unwrap_or_else(|| "local".to_string());
            (text, tag)
        }
        None => {
            let tag = pin.ok_or("no pin: pass --pin <tag> or commit registry/PIN")?;
            (download_release(&tag)?, tag)
        }
    };

    let packages: Vec<Value> =
        serde_json::from_str(&json_text).map_err(|e| format!("registry.json parse: {e}"))?;
    let overlay: Vec<OverlayEntry> = parse_overlay()?;

    let snapshot = transform_packages(&packages, &overlay, &resolved_pin);
    let out = serde_json::to_string_pretty(&snapshot).expect("snapshot serialize");

    std::fs::create_dir_all("src/registry").map_err(|e| e.to_string())?;
    std::fs::write(SNAPSHOT_FILE, out + "\n").map_err(|e| e.to_string())?;
    std::fs::write(PIN_FILE, format!("{resolved_pin}\n")).map_err(|e| e.to_string())?;

    let total = snapshot.recipes.len();
    let supported = snapshot.recipes.iter().filter(|r| r.supported).count();
    Ok(format!(
        "wrote {SNAPSHOT_FILE}: {total} LSP recipes, {supported} installable, pin {resolved_pin}"
    ))
}

#[derive(Deserialize)]
struct OverlayFile {
    #[serde(default)]
    server: Vec<OverlayEntry>,
}

fn parse_overlay() -> Result<Vec<OverlayEntry>, String> {
    let text = std::fs::read_to_string(OVERLAY_FILE)
        .map_err(|e| format!("could not read {OVERLAY_FILE}: {e}"))?;
    let parsed: OverlayFile =
        toml::from_str(&text).map_err(|e| format!("{OVERLAY_FILE} parse: {e}"))?;
    Ok(parsed.server)
}

// ---------- download ----------

fn download_release(tag: &str) -> Result<String, String> {
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let agent = ureq::Agent::new();

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

    let bytes = agent
        .get(&format!("{base}/registry.json.zip"))
        .call()
        .map_err(|e| format!("registry.json.zip fetch: {e}"))?
        .into_reader()
        .bytes()
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|e| e.to_string())?;

    let hash = sha2::Sha256::digest(&bytes);
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    if hex != zip_sha {
        return Err(format!("sha256 mismatch: want {zip_sha}, got {hex}"));
    }

    // The zip holds one file, registry.json.
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("zip open: {e}"))?;
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
    entry
        .read_to_string(&mut text)
        .map_err(|e| format!("registry.json read: {e}"))?;
    Ok(text)
}
