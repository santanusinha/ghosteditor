//! Language-server registry (P2.5).
//!
//! mason-registry owns "how to install": versions, purls, per-platform
//! assets, exe names. We ship its compiled JSON as an embedded snapshot and
//! refresh it in the background. This module owns "how to launch": the
//! overlay (launch args, extension routing) and the probe order.
//!
//! Probe order (plan 4.3, registry plan section 6):
//! 1. project config `[languages.X] command` (full replace)
//! 2. global config `command`
//! 3. registry `bin` on `PATH`
//! 4. registry `bin` in the install dir
//!
//! The registry never overrides explicit user config. It fills gaps.

pub mod install;
pub mod jobs;
pub mod refresh;
pub mod transform;

use serde::{Deserialize, Serialize};

const SNAPSHOT: &str = include_str!("snapshot.json");

/// Cache file written by the background refresh. Same schema as the snapshot.
pub fn cache_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join(".cache/ghostedit/registry/registry.json"))
}

/// Install root for `install_server` (P2.6).
pub fn install_root() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join(".local/share/ghostedit/lsp"))
}

// ---------- model ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub target: String,
    pub file: String,
    pub bin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub kind: String,
    pub package: String,
    #[serde(default)]
    pub extra: Vec<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overlay {
    pub language: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub default_for: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    pub name: String,
    pub version: String,
    pub languages: Vec<String>,
    pub homepage: String,
    pub supported: bool,
    pub source: Source,
    pub bin: Option<String>,
    #[serde(default)]
    pub overlay: Option<Overlay>,
}

#[derive(Debug, Clone, Deserialize)]
struct SnapshotFile {
    #[allow(dead_code)]
    pin: String,
    #[serde(default)]
    recipes: Vec<Recipe>,
}

// ---------- registry ----------

#[derive(Debug, Clone)]
pub struct Registry {
    pub pin: String,
    pub recipes: Vec<Recipe>,
}

impl Registry {
    /// Embedded snapshot, replaced by a valid cache file when present.
    pub fn load() -> Self {
        if let Some(path) = cache_path() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                match serde_json::from_str::<SnapshotFile>(&text) {
                    Ok(snapshot) => {
                        return Self {
                            pin: snapshot.pin,
                            recipes: snapshot.recipes,
                        };
                    }
                    Err(_) => {
                        // Corrupt cache: fall through to the snapshot.
                    }
                }
            }
        }
        let snapshot: SnapshotFile =
            serde_json::from_str(SNAPSHOT).expect("embedded registry snapshot is valid");
        Self {
            pin: snapshot.pin,
            recipes: snapshot.recipes,
        }
    }

    pub fn by_name(&self, name: &str) -> Option<&Recipe> {
        self.recipes.iter().find(|r| r.name == name)
    }

    /// Recipes whose overlay handles this extension, best match first.
    pub fn recipes_for_extension(&self, ext: &str) -> Vec<&Recipe> {
        let mut matches: Vec<&Recipe> = self
            .recipes
            .iter()
            .filter(|r| {
                r.overlay
                    .as_ref()
                    .is_some_and(|o| o.extensions.iter().any(|e| e == ext))
            })
            .collect();
        // Exact-name servers first, then alphabetical for stability.
        matches.sort_by_key(|r| (r.name == ext.trim_start_matches('.'), r.name.clone()));
        matches
    }

    /// The default language key for an extension, from overlay `default_for`.
    pub fn default_language_for(&self, ext: &str) -> Option<String> {
        self.recipes
            .iter()
            .filter(|r| r.supported)
            .find(|r| {
                r.overlay
                    .as_ref()
                    .is_some_and(|o| o.default_for.iter().any(|e| e == ext))
            })
            .map(|r| r.overlay.as_ref().expect("checked above").language.clone())
    }

    /// Recipes routed to a language: overlay language match, else mason
    /// language names.
    pub fn recipes_for_language(&self, language: &str) -> Vec<&Recipe> {
        let lower = language.to_lowercase();
        self.recipes
            .iter()
            .filter(|r| {
                r.overlay.as_ref().is_some_and(|o| o.language == lower)
                    || r.languages.contains(&lower)
            })
            .collect()
    }

    /// Status of one language for the initialize response (P2.4 input).
    pub fn status_for(&self, language: &str) -> &'static str {
        let recipes = self.recipes_for_language(language);
        if recipes.is_empty() {
            return "no_recipe";
        }
        if recipes.iter().any(|r| self.is_probed(r)) {
            return "available";
        }
        "not_installed"
    }

    /// Is the binary present (PATH or install dir)? No spawn, no wait.
    pub fn is_probed(&self, recipe: &Recipe) -> bool {
        self.resolve_bin(recipe).is_some()
    }

    /// Candidate binary names for a recipe. `bin` is the primary name. Two more
    /// sources add candidates: the recipe name itself (toolchains ship the
    /// server under it, e.g. rustup's rust-analyzer) and github asset file
    /// names (ghostedit installs releases under them).
    fn bin_names(recipe: &Recipe) -> Vec<String> {
        let mut names = Vec::new();
        if let Some(bin) = recipe.bin.as_deref() {
            names.push(bin.to_string());
        }
        names.push(recipe.name.clone());
        for asset in &recipe.source.assets {
            if let Some(bin) = asset.bin.as_deref() {
                if let Some(file) = std::path::Path::new(bin)
                    .file_name()
                    .and_then(|f| f.to_str())
                {
                    names.push(file.to_string());
                }
            }
        }
        names.dedup();
        names
    }

    /// Find the server binary: the install dir first (what ghostedit
    /// installed), then PATH. Same dir order for every name candidate.
    pub fn resolve_bin(&self, recipe: &Recipe) -> Option<std::path::PathBuf> {
        let names = Self::bin_names(recipe);
        if names.is_empty() {
            return None;
        }
        // The install dir wins over PATH: a binary ghostedit installed is the
        // known-good one, and PATH can hold broken shims (e.g. a snap rustup
        // shim that never serves LSP).
        if let Some(root) = install_root() {
            for name in &names {
                let installed = root.join(&recipe.name).join("bin").join(name);
                if installed.is_file() {
                    return Some(installed);
                }
            }
        }
        if let Ok(path_env) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_env) {
                for name in &names {
                    let candidate = dir.join(name);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }

    /// Full launch command for a recipe: resolved binary plus overlay args.
    pub fn launch_command(&self, recipe: &Recipe) -> Option<Vec<String>> {
        let bin = self.resolve_bin(recipe)?;
        let mut command = vec![bin.display().to_string()];
        if let Some(overlay) = &recipe.overlay {
            command.extend(overlay.args.iter().cloned());
        }
        Some(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_loads_and_has_core_servers() {
        let registry = Registry::load();
        assert!(!registry.recipes.is_empty());
        let pyright = registry.by_name("pyright").expect("pyright in snapshot");
        assert_eq!(pyright.source.kind, "npm");
        assert_eq!(pyright.bin.as_deref(), Some("pyright-langserver"));
        assert!(pyright.supported);
    }

    #[test]
    fn overlay_routes_python_by_default() {
        let registry = Registry::load();
        assert_eq!(
            registry.default_language_for(".py").as_deref(),
            Some("python")
        );
        assert_eq!(
            registry.default_language_for(".rs").as_deref(),
            Some("rust")
        );
        // No overlay claims .foobar.
        assert_eq!(registry.default_language_for(".foobar"), None);
    }

    #[test]
    fn recipes_for_language_finds_pyright_and_pylsp() {
        let registry = Registry::load();
        let names: Vec<&str> = registry
            .recipes_for_language("python")
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert!(names.contains(&"pyright"));
        assert!(names.contains(&"python-lsp-server"));
    }

    #[test]
    fn jdtls_is_pinned_off() {
        let registry = Registry::load();
        let jdtls = registry.by_name("jdtls").expect("jdtls in snapshot");
        assert!(!jdtls.supported);
    }
}
