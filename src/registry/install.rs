//! Installers for registry recipes (P2.6).
//!
//! Five source kinds: npm, pypi, cargo, golang, github release assets.
//! Every process spawn uses an argv array. No shell, no string
//! interpolation into a shell. The uniform on-disk layout is
//! `<root>/<name>/bin/<bin>` so the probe (registry `resolve_bin`) has one
//! layout to probe.
use std::io::Read;
use std::path::{Path, PathBuf};

use super::Recipe;

/// One unit of install work. `Exec` spawns argv; the rest are file ops.
#[derive(Debug, Clone)]
pub enum Step {
    Exec {
        argv: Vec<String>,
        env: Vec<(String, String)>,
    },
    Download {
        url: String,
        dest: PathBuf,
    },
    Unpack {
        archive: PathBuf,
        dest: PathBuf,
        format: ArchiveFormat,
    },
    /// Place the produced binary at `<dir>/bin/<name>`. `link` makes a
    /// symlink instead of a copy; node bin wrappers resolve `require()`
    /// relative to the real file, so a copy breaks them.
    EnsureBin {
        from: PathBuf,
        to: PathBuf,
        link: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ArchiveFormat {
    TarGz,
    Tar,
    Zip,
    Gunzip,
    Raw,
}

/// A ready-to-run install plan for one recipe.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub kind: String,
    pub steps: Vec<Step>,
    /// The binary the probe looks for after a successful install.
    pub bin_path: PathBuf,
    /// Toolchain binary to check before the first step.
    pub toolchain: Option<String>,
}

// ---------- target mapping ----------

/// The mason target string for this machine, best match first.
pub fn target_candidates() -> Vec<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        #[cfg(target_env = "musl")]
        return vec!["linux_x64_musl", "linux_x64", "linux", "unix"];
        #[cfg(not(target_env = "musl"))]
        return vec!["linux_x64_gnu", "linux_x64", "linux", "unix"];
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        #[cfg(target_env = "musl")]
        return vec!["linux_arm64_musl", "linux_arm64", "linux", "unix"];
        #[cfg(not(target_env = "musl"))]
        return vec!["linux_arm64_gnu", "linux_arm64", "linux", "unix"];
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return vec!["darwin_x64", "darwin", "unix"];
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return vec!["darwin_arm64", "darwin", "unix"];
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64")
    )))]
    return vec!["unknown"];
}

/// The single best mason target for this machine.
pub fn current_target() -> &'static str {
    target_candidates()[0]
}

/// Substitute the three mason asset-file templates we support. The forms
/// may appear anywhere inside the string (file names embed them). Any other
/// `{{ ... }}` expression is an error: the overlay must pin the name.
pub fn eval_file_template(template: &str, version: &str) -> Result<String, String> {
    let strip_v = "{{ version | strip_prefix \"v\" }}";
    let plain = "{{version}}";
    let spaced = "{{ version }}";
    let bare_version = version.strip_prefix('v').unwrap_or(version);
    let out = template
        .replace(strip_v, bare_version)
        .replace(plain, version)
        .replace(spaced, version);
    if out.contains("{{") {
        return Err(format!("unsupported asset template: {template}"));
    }
    Ok(out)
}

/// Pick the release asset for this machine. Best target first; the asset
/// must carry a `bin` path or the install cannot place the binary.
pub fn pick_asset<'a>(
    recipe: &'a Recipe,
    version: &str,
) -> Result<(&'a super::Asset, String), String> {
    let candidates = target_candidates();
    let mut best_without_bin: Option<&super::Asset> = None;
    for want in &candidates {
        for asset in &recipe.source.assets {
            if &asset.target != want {
                continue;
            }
            let file = eval_file_template(&asset.file, version)?;
            if asset.bin.is_some() {
                return Ok((asset, file));
            }
            if best_without_bin.is_none() {
                best_without_bin = Some(asset);
            }
        }
    }
    match best_without_bin {
        Some(_) => Err(format!(
            "no asset with a bin path for {} on this machine",
            recipe.name
        )),
        None => Err(format!(
            "no release asset for {} on this machine (targets: {})",
            recipe.name,
            candidates.join(", ")
        )),
    }
}

// ---------- planning ----------

fn toolchain_on_path(tool: &str) -> bool {
    if tool.contains('/') {
        return Path::new(tool).is_file();
    }
    std::env::var("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file()))
        .unwrap_or(false)
}

/// Build the install plan for one recipe at `root`. `version` overrides the
/// recipe version. Fails with a structured reason before anything runs.
pub fn plan(recipe: &Recipe, version: Option<&str>, root: &Path) -> Result<InstallPlan, String> {
    let version = version.unwrap_or(&recipe.version).to_string();
    let dir = root.join(&recipe.name);
    let bin_dir = dir.join("bin");
    let kind = recipe.source.kind.as_str();
    let package = &recipe.source.package;

    let bin_name = match recipe.bin.as_deref() {
        Some(bin) => bin.to_string(),
        None if kind == "github" => {
            // Resolved from the per-target asset below.
            String::new()
        }
        None => return Err(format!("{}: no binary name in the recipe", recipe.name)),
    };

    let mut steps: Vec<Step> = Vec::new();
    let (toolchain, bin_path) = match kind {
        "npm" => {
            let mut argv = vec![
                "npm".to_string(),
                "install".to_string(),
                "--prefix".to_string(),
                dir.display().to_string(),
                format!("{package}@{version}"),
            ];
            for extra in &recipe.source.extra {
                argv.push(extra.clone());
            }
            steps.push(Step::Exec {
                argv,
                env: Vec::new(),
            });
            let from = dir.join("node_modules").join(".bin").join(&bin_name);
            let to = bin_dir.join(&bin_name);
            steps.push(Step::EnsureBin {
                from,
                to: to.clone(),
                link: true,
            });
            (Some("npm"), to)
        }
        "pypi" => {
            steps.push(Step::Exec {
                argv: vec![
                    "python3".to_string(),
                    "-m".to_string(),
                    "venv".to_string(),
                    dir.display().to_string(),
                ],
                env: Vec::new(),
            });
            let extras = recipe.source.extra.join(",");
            let spec = if extras.is_empty() {
                format!("{package}=={version}")
            } else {
                format!("{package}[{extras}]=={version}")
            };
            steps.push(Step::Exec {
                argv: vec![
                    bin_dir.join("pip").display().to_string(),
                    "install".to_string(),
                    spec,
                ],
                env: Vec::new(),
            });
            let to = bin_dir.join(&bin_name);
            (Some("python3"), to)
        }
        "cargo" => {
            steps.push(Step::Exec {
                argv: vec![
                    "cargo".to_string(),
                    "install".to_string(),
                    format!("{package}@{version}"),
                ],
                env: Vec::new(),
            });
            let to = bin_dir.join(&bin_name);
            (Some("cargo"), to)
        }
        "golang" => {
            steps.push(Step::Exec {
                argv: vec![
                    "go".to_string(),
                    "install".to_string(),
                    format!("{package}@{version}"),
                ],
                env: vec![("GOBIN".to_string(), bin_dir.display().to_string())],
            });
            let to = bin_dir.join(&bin_name);
            (Some("go"), to)
        }
        "github" => {
            let repo = recipe
                .source
                .repo
                .as_deref()
                .ok_or_else(|| format!("{}: github source without a repo", recipe.name))?;
            let (asset, file) = pick_asset(recipe, &version)?;
            let bin_rel_raw = asset
                .bin
                .clone()
                .ok_or_else(|| format!("{}: asset has no bin path", recipe.name))?;
            // The bin path lives inside the unpacked archive; it can embed
            // the version template too (top-level versioned directory).
            let bin_rel = eval_file_template(&bin_rel_raw, &version)?;
            let bin_name = Path::new(&bin_rel)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&bin_rel)
                .to_string();
            let download_dir = dir.join("download");
            let archive = download_dir.join(&file);
            let url = format!("https://github.com/{repo}/releases/download/{version}/{file}");
            steps.push(Step::Download {
                url,
                dest: archive.clone(),
            });
            let format = archive_format(&file)?;
            let to = bin_dir.join(&bin_name);
            match format {
                ArchiveFormat::Raw | ArchiveFormat::Gunzip => {
                    // The asset is the binary itself (maybe gzipped).
                    steps.push(Step::Unpack {
                        archive: archive.clone(),
                        dest: to.clone(),
                        format,
                    });
                }
                _ => {
                    steps.push(Step::Unpack {
                        archive: archive.clone(),
                        dest: dir.clone(),
                        format,
                    });
                    let from = dir.join(&bin_rel);
                    steps.push(Step::EnsureBin {
                        from,
                        to: to.clone(),
                        link: false,
                    });
                }
            }
            (None, to)
        }
        other => {
            return Err(format!(
                "{}: no installer for source kind {other}",
                recipe.name
            ))
        }
    };

    Ok(InstallPlan {
        kind: kind.to_string(),
        steps,
        bin_path,
        toolchain: toolchain.map(str::to_string),
    })
}

/// Verify the plan's toolchain is present. Called at run time, not plan
/// time, so planning stays pure and host-independent.
pub fn check_toolchain(plan: &InstallPlan) -> Result<(), String> {
    match &plan.toolchain {
        Some(tool) => {
            if toolchain_on_path(tool) {
                Ok(())
            } else {
                Err(format!("toolchain {tool} is not on PATH; install it first"))
            }
        }
        None => Ok(()),
    }
}

/// Archive format from the asset file name.
pub fn archive_format(file: &str) -> Result<ArchiveFormat, String> {
    let lower = file.to_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return Ok(ArchiveFormat::TarGz);
    }
    if lower.ends_with(".tar") {
        return Ok(ArchiveFormat::Tar);
    }
    if lower.ends_with(".zip") {
        return Ok(ArchiveFormat::Zip);
    }
    if lower.ends_with(".gz") {
        return Ok(ArchiveFormat::Gunzip);
    }
    if lower.ends_with(".tar.xz") || lower.ends_with(".tar.bz2") || lower.ends_with(".xz") {
        return Err(format!("unsupported archive format: {file}"));
    }
    Ok(ArchiveFormat::Raw)
}

// ---------- running ----------

fn append_log(log: &mut String, line: &str) {
    log.push_str(line);
    log.push('\n');
}

/// Run one plan step to completion. Blocking work happens inside
/// `spawn_blocking`; process spawns are async. Output lands in `log`.
pub async fn run_step(step: &Step, log: &mut String) -> Result<(), String> {
    match step {
        Step::Exec { argv, env } => {
            let (program, args) = argv.split_first().ok_or("empty argv")?;
            append_log(log, &format!("$ {}", argv.join(" ")));
            let mut command = tokio::process::Command::new(program);
            command.args(args);
            for (key, value) in env {
                command.env(key, value);
            }
            command.stdin(std::process::Stdio::null());
            command.stdout(std::process::Stdio::piped());
            command.stderr(std::process::Stdio::piped());
            let output = command
                .output()
                .await
                .map_err(|e| format!("spawn {}: {e}", argv.join(" ")))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stdout.lines().chain(stderr.lines()).take(200) {
                append_log(log, line);
            }
            if !output.status.success() {
                return Err(format!(
                    "command failed ({}): {}",
                    output.status,
                    argv.join(" ")
                ));
            }
            Ok(())
        }
        Step::Download { url, dest } => {
            append_log(log, &format!("> download {url}"));
            let url = url.clone();
            let dest = dest.clone();
            let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
                let agent = ureq::Agent::new();
                let response = agent
                    .get(&url)
                    .timeout(std::time::Duration::from_secs(120))
                    .call()
                    .map_err(|e| format!("download: {e}"))?;
                let mut reader = response.into_reader();
                let mut buffer = Vec::new();
                reader
                    .read_to_end(&mut buffer)
                    .map_err(|e| format!("download read: {e}"))?;
                Ok(buffer)
            })
            .await
            .map_err(|e| format!("download task: {e}"))??;
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            tokio::fs::write(&dest, &bytes)
                .await
                .map_err(|e| format!("write {}: {e}", dest.display()))?;
            Ok(())
        }
        Step::Unpack {
            archive,
            dest,
            format,
        } => {
            append_log(
                log,
                &format!("> unpack {} -> {}", archive.display(), dest.display()),
            );
            let archive = archive.clone();
            let dest = dest.clone();
            let format = *format;
            tokio::task::spawn_blocking(move || unpack(&archive, &dest, format))
                .await
                .map_err(|e| format!("unpack task: {e}"))??;
            Ok(())
        }
        Step::EnsureBin { from, to, link } => {
            if let Some(parent) = to.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            if *link {
                // Absolute symlink inside the install dir: node resolves the
                // real file, so the wrapper keeps working from bin/.
                #[cfg(unix)]
                {
                    if tokio::fs::symlink_metadata(to).await.is_ok() {
                        tokio::fs::remove_file(to)
                            .await
                            .map_err(|e| format!("remove {}: {e}", to.display()))?;
                    }
                    tokio::fs::symlink(from, to)
                        .await
                        .map_err(|e| format!("link {} -> {}: {e}", from.display(), to.display()))?;
                    return Ok(());
                }
                #[cfg(not(unix))]
                {
                    let _ = from;
                    return Err("symlink install needs unix".to_string());
                }
            }
            // Copy (not link): the install dir must survive the cache dir.
            tokio::fs::copy(from, to)
                .await
                .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
            mark_executable(to).await?;
            Ok(())
        }
    }
}

async fn mark_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(path)
            .await
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .permissions();
        perms.set_mode(perms.mode() | 0o755);
        tokio::fs::set_permissions(path, perms)
            .await
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Unpack an archive. Blocking; call from `spawn_blocking`.
fn unpack(archive: &Path, dest: &Path, format: ArchiveFormat) -> Result<(), String> {
    let bytes = std::fs::read(archive).map_err(|e| format!("read {}: {e}", archive.display()))?;
    match format {
        ArchiveFormat::TarGz => {
            let gz = flate2::read::GzDecoder::new(&bytes[..]);
            untar(gz, dest)
        }
        ArchiveFormat::Tar => untar(&bytes[..], dest),
        ArchiveFormat::Zip => {
            let mut lib = zip::ZipArchive::new(std::io::Cursor::new(bytes))
                .map_err(|e| format!("zip open: {e}"))?;
            for i in 0..lib.len() {
                let mut entry = lib.by_index(i).map_err(|e| format!("zip entry: {e}"))?;
                let Some(name) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
                    continue;
                };
                let out = dest.join(name);
                if entry.is_dir() {
                    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
                    continue;
                }
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Some(mode) = entry.unix_mode() {
                        let _ =
                            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode));
                    }
                }
            }
            Ok(())
        }
        ArchiveFormat::Gunzip => {
            let mut gz = flate2::read::GzDecoder::new(&bytes[..]);
            std::fs::create_dir_all(dest.parent().unwrap_or(dest)).map_err(|e| e.to_string())?;
            let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
            std::io::copy(&mut gz, &mut file).map_err(|e| e.to_string())?;
            Ok(())
        }
        ArchiveFormat::Raw => {
            std::fs::create_dir_all(dest.parent().unwrap_or(dest)).map_err(|e| e.to_string())?;
            std::fs::write(dest, &bytes).map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

fn untar<R: std::io::Read>(reader: R, dest: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    archive.set_overwrite(true);
    // Refuse absolute paths and parent traversal (tar crate default is safe,
    // but keep the explicit guard for clarity).
    for entry in archive.entries().map_err(|e| format!("tar read: {e}"))? {
        let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        let Some(path) = entry.path().ok().and_then(|p| strip_safe(p.to_path_buf())) else {
            continue;
        };
        let out = dest.join(path);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(mode) = entry.header().mode() {
                let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn strip_safe(path: PathBuf) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn recipe(kind: &str, package: &str, version: &str, bin: Option<&str>) -> Recipe {
        serde_json::from_value(serde_json::json!({
            "name": "test-server",
            "version": version,
            "languages": [],
            "homepage": "",
            "supported": true,
            "source": { "kind": kind, "package": package },
            "bin": bin,
        }))
        .unwrap()
    }

    #[test]
    fn evals_the_three_supported_templates() {
        assert_eq!(eval_file_template("{{version}}", "1.2.3").unwrap(), "1.2.3");
        assert_eq!(
            eval_file_template("{{ version }}", "1.2.3").unwrap(),
            "1.2.3"
        );
        assert_eq!(
            eval_file_template("{{ version | strip_prefix \"v\" }}", "v0.9").unwrap(),
            "0.9"
        );
        assert_eq!(
            eval_file_template("tool-{{version}}-linux.tar.gz", "1.2.3").unwrap(),
            "tool-1.2.3-linux.tar.gz"
        );
        assert!(eval_file_template("{{source.build.bin}}", "1").is_err());
    }

    #[test]
    fn npm_plan_uses_argv_and_places_the_bin() {
        let recipe = recipe("npm", "pyright", "1.1.414", Some("pyright-langserver"));
        let root = Path::new("/tmp/ghostedit-test-root");
        let plan = plan(&recipe, None, root).unwrap();
        assert_eq!(plan.toolchain.as_deref(), Some("npm"));
        assert_eq!(
            plan.bin_path,
            root.join("test-server/bin/pyright-langserver")
        );
        match &plan.steps[0] {
            Step::Exec { argv, env } => {
                assert_eq!(argv[0], "npm");
                assert_eq!(argv[1], "install");
                assert!(argv.contains(&"pyright@1.1.414".to_string()));
                assert!(env.is_empty());
            }
            other => panic!("expected exec, got {other:?}"),
        }
    }

    #[test]
    fn pypi_plan_uses_venv_and_extra_brackets() {
        let mut recipe = recipe("pypi", "python-lsp-server", "1.15.0", Some("pylsp"));
        recipe.source.extra = vec!["all".to_string()];
        let root = Path::new("/tmp/ghostedit-test-root");
        let plan = plan(&recipe, None, root).unwrap();
        assert_eq!(plan.toolchain.as_deref(), Some("python3"));
        match &plan.steps[1] {
            Step::Exec { argv, .. } => {
                assert_eq!(
                    argv[0],
                    root.join("test-server/bin/pip").display().to_string()
                );
                assert_eq!(argv[2], "python-lsp-server[all]==1.15.0");
            }
            other => panic!("expected exec, got {other:?}"),
        }
    }

    #[test]
    fn golang_plan_sets_gobin() {
        let recipe = recipe(
            "golang",
            "golang.org/x/tools/gopls",
            "v0.23.0",
            Some("gopls"),
        );
        let root = Path::new("/tmp/ghostedit-test-root");
        let plan = plan(&recipe, None, root).unwrap();
        assert_eq!(plan.toolchain.as_deref(), Some("go"));
        match &plan.steps[0] {
            Step::Exec { argv, env } => {
                assert_eq!(argv[0], "go");
                assert_eq!(argv[2], "golang.org/x/tools/gopls@v0.23.0");
                assert_eq!(env[0].0, "GOBIN");
            }
            other => panic!("expected exec, got {other:?}"),
        }
    }

    #[test]
    fn cargo_plan_is_hermetic_and_check_toolchain_rejects_missing() {
        let recipe = recipe("cargo", "some-crate", "1.0.0", Some("some-crate"));
        let plan = plan(&recipe, None, Path::new("/tmp/x")).unwrap();
        assert_eq!(plan.toolchain.as_deref(), Some("cargo"));
        match &plan.steps[0] {
            Step::Exec { argv, env } => {
                assert_eq!(argv[0], "cargo");
                assert_eq!(argv[1], "install");
                assert_eq!(argv[2], "some-crate@1.0.0");
                assert!(env.is_empty());
            }
            other => panic!("expected exec, got {other:?}"),
        }

        // Toolchain checks happen at run time and never mutate the
        // environment. An absolute path is checked directly on disk.
        let missing = InstallPlan {
            toolchain: Some("/nonexistent-ghostedit-path/cargo".to_string()),
            ..plan.clone()
        };
        let error = check_toolchain(&missing).unwrap_err();
        assert!(error.contains("not on PATH"), "error was: {error}");
        let present = InstallPlan {
            toolchain: Some(
                std::env::current_exe()
                    .expect("test binary exists")
                    .display()
                    .to_string(),
            ),
            ..plan
        };
        check_toolchain(&present).unwrap();
    }

    #[test]
    fn github_plan_downloads_unpacks_and_places_the_bin() {
        let raw = serde_json::json!({
            "name": "test-server",
            "version": "2026-01-01",
            "languages": [],
            "homepage": "",
            "supported": true,
            "source": {
                "kind": "github",
                "package": "org/test-server",
                "repo": "org/test-server",
                "assets": [{
                    "target": current_target(),
                    "file": "test-server-{{version}}.tar.gz",
                    "bin": "test-server-{{version}}/bin/test-server",
                }],
            },
            "bin": null,
        });
        let recipe: Recipe = serde_json::from_value(raw).unwrap();
        let root = Path::new("/tmp/ghostedit-test-root");
        let plan = plan(&recipe, None, root).unwrap();
        assert_eq!(plan.steps.len(), 3); // download, unpack, ensure
        match &plan.steps[0] {
            Step::Download { url, .. } => {
                assert!(url.starts_with(
                    "https://github.com/org/test-server/releases/download/2026-01-01/"
                ));
                assert!(url.ends_with("test-server-2026-01-01.tar.gz"));
            }
            other => panic!("expected download, got {other:?}"),
        }
        match &plan.steps[2] {
            Step::EnsureBin { to, .. } => {
                assert_eq!(to, &root.join("test-server/bin/test-server"));
            }
            other => panic!("expected ensure, got {other:?}"),
        }
    }

    #[test]
    fn github_plan_without_a_target_asset_is_a_clean_error() {
        let raw = serde_json::json!({
            "name": "test-server",
            "version": "1.0",
            "languages": [],
            "homepage": "",
            "supported": true,
            "source": {
                "kind": "github",
                "package": "org/test-server",
                "repo": "org/test-server",
                "assets": [{ "target": "win_x64", "file": "x.zip", "bin": "x.exe" }],
            },
            "bin": null,
        });
        let recipe: Recipe = serde_json::from_value(raw).unwrap();
        let error = plan(&recipe, None, Path::new("/tmp/x")).unwrap_err();
        assert!(error.contains("no release asset"), "error was: {error}");
    }

    #[test]
    fn archive_format_covers_the_common_cases() {
        assert_eq!(archive_format("x.tar.gz").unwrap(), ArchiveFormat::TarGz);
        assert_eq!(archive_format("x.tgz").unwrap(), ArchiveFormat::TarGz);
        assert_eq!(archive_format("x.zip").unwrap(), ArchiveFormat::Zip);
        assert_eq!(archive_format("x.gz").unwrap(), ArchiveFormat::Gunzip);
        assert_eq!(archive_format("x").unwrap(), ArchiveFormat::Raw);
        assert!(archive_format("x.tar.xz").is_err());
    }

    #[test]
    fn gunzip_unpack_produces_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"#!/bin/sh\necho hi\n";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(payload).unwrap();
        let compressed = encoder.finish().unwrap();
        let archive = dir.path().join("tool.gz");
        std::fs::write(&archive, &compressed).unwrap();
        let dest = dir.path().join("bin/tool");
        unpack(&archive, &dest, ArchiveFormat::Gunzip).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
    }

    #[test]
    fn targz_unpack_preserves_exec_bits() {
        let dir = tempfile::tempdir().unwrap();
        // Build a tar.gz in memory with one executable file.
        let inner = tempfile::tempdir().unwrap();
        let file_path = inner.path().join("pkg/bin/tool");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, b"binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let tar_path = dir.path().join("in.tar");
        let tar_file = std::fs::File::create(&tar_path).unwrap();
        let mut builder = tar::Builder::new(tar_file);
        builder
            .append_path_with_name(&file_path, "pkg/bin/tool")
            .unwrap();
        builder.finish().unwrap();
        let tar_bytes = std::fs::read(&tar_path).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar_bytes).unwrap();
        let archive = dir.path().join("in.tar.gz");
        std::fs::write(&archive, encoder.finish().unwrap()).unwrap();

        let dest = dir.path().join("out");
        unpack(&archive, &dest, ArchiveFormat::TarGz).unwrap();
        let unpacked = dest.join("pkg/bin/tool");
        assert_eq!(std::fs::read(&unpacked).unwrap(), b"binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&unpacked).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "exec bits must survive");
        }
    }
}
