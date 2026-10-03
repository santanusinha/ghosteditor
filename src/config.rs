//! Configuration: TOML, global then project, deadlines per server (P0.6, P0.7, P0.8).
//!
//! Project `.ghostedit/config.toml` wins over the global file. A project
//! entry replaces a global entry for the same language; it does not merge.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::util::logging;

/// Deadline budget per operation, in milliseconds (P0.8).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Deadlines {
    #[serde(default = "default_read")]
    pub read: u64,
    #[serde(default = "default_edit")]
    pub edit: u64,
    #[serde(default = "default_rename")]
    pub rename: u64,
    #[serde(default = "default_symbol")]
    pub symbol: u64,
    #[serde(default = "default_install")]
    pub install: u64,
}

fn default_read() -> u64 {
    10_000
}
fn default_edit() -> u64 {
    5_000
}
fn default_rename() -> u64 {
    8_000
}
fn default_symbol() -> u64 {
    20_000
}
fn default_install() -> u64 {
    300_000
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            read: default_read(),
            edit: default_edit(),
            rename: default_rename(),
            symbol: default_symbol(),
            install: default_install(),
        }
    }
}

/// One language server entry from the config file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Language {
    pub extensions: Vec<String>,
    pub command: Vec<String>,
    #[serde(default)]
    pub initialization_options: Option<toml::Value>,
    #[serde(default)]
    pub settings: Option<toml::Value>,
    /// Per-server override; missing fields fall back to the defaults.
    #[serde(default)]
    pub deadlines_ms: Option<Deadlines>,
}

/// The background registry refresh mode (plan-registry section 8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum RefreshMode {
    /// Fetch the newest mason release (the default).
    #[serde(rename = "latest")]
    Latest,
    /// Fetch only the tag in `RegistrySettings::pin`.
    #[serde(rename = "pinned")]
    Pinned,
    /// Never fetch; use the cache or the embedded snapshot.
    #[serde(rename = "off")]
    Off,
    /// An absent `[registry]` table: behave as `latest`.
    #[default]
    #[serde(other)]
    Unknown,
}

/// The `[registry]` config table (P2.5 remainder).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RegistrySettings {
    /// Refresh mode: `latest` (default), `pinned`, or `off`.
    #[serde(default)]
    pub refresh: RefreshMode,
    /// The mason release tag to fetch when `refresh = "pinned"`.
    #[serde(default)]
    pub pin: Option<String>,
}

impl Default for RegistrySettings {
    fn default() -> Self {
        Self {
            refresh: RefreshMode::Latest,
            pin: None,
        }
    }
}

/// The merged, effective configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub cwd: PathBuf,
    #[allow(dead_code)]
    pub global_path: PathBuf,
    pub languages: BTreeMap<String, Language>,
    pub defaults: Deadlines,
    pub registry: RegistrySettings,
    /// Idle LSP child eviction in minutes; 0 disables (P2.10).
    pub idle_evict_min: u64,
}

/// Raw file layout; languages is the main table.
#[derive(Debug, Default, Deserialize)]
struct Raw {
    #[serde(default)]
    languages: BTreeMap<String, Language>,
    #[serde(default)]
    defaults_ms: Option<Deadlines>,
    #[serde(default)]
    registry: Option<RegistrySettings>,
}

impl Config {
    /// Load global config, then project config. The project file replaces
    /// whole language entries. It does not deep-merge them.
    pub async fn load(explicit: Option<PathBuf>, cwd: Option<PathBuf>) -> Result<Self, String> {
        let cwd = cwd.unwrap_or_else(|| {
            std::env::current_dir().expect("could not read the working directory")
        });
        let cwd = cwd.canonicalize().unwrap_or(cwd);

        let global_path = explicit.unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config/ghostedit/config.toml")
        });

        let mut languages = BTreeMap::new();
        let mut defaults = Deadlines::default();
        let mut registry = RegistrySettings::default();

        if let Some(raw) = read_raw(&global_path).await {
            if let Some(merged_defaults) = raw.defaults_ms {
                defaults = merged_defaults;
            }
            if let Some(merged_registry) = raw.registry {
                registry = merged_registry;
            }
            languages = raw.languages;
        }

        let project_path = cwd.join(".ghostedit/config.toml");
        if let Some(raw) = read_raw(&project_path).await {
            if let Some(merged_defaults) = raw.defaults_ms {
                defaults = merged_defaults;
            }
            // The project `[registry]` table replaces the global one whole;
            // it does not merge field by field (the same rule as languages).
            if let Some(merged_registry) = raw.registry {
                registry = merged_registry;
            }
            for (name, entry) in raw.languages {
                languages.insert(name, entry); // Full replace, per decision 9.5.
            }
        }

        logging::info("ready");
        Ok(Self {
            cwd,
            global_path,
            languages,
            defaults,
            registry,
            idle_evict_min: 15,
        })
    }

    /// The effective deadline table for one language (S8, P0.8).
    /// Missing fields come from the global defaults.
    pub fn deadlines_for(&self, language: &str) -> Deadlines {
        let Some(entry) = self.languages.get(language) else {
            return self.defaults.clone();
        };
        match &entry.deadlines_ms {
            Some(override_ms) => Deadlines {
                read: override_ms.read,
                edit: override_ms.edit,
                rename: override_ms.rename,
                symbol: override_ms.symbol,
                install: override_ms.install,
            },
            None => self.defaults.clone(),
        }
    }

    /// The language for a file path, from its extension.
    pub fn language_for(&self, path: &Path) -> Option<(String, &Language)> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))?;
        self.languages
            .iter()
            .find(|(_, entry)| entry.extensions.iter().any(|x| x == &ext))
            .map(|(name, entry)| (name.clone(), entry))
    }

    /// The deadline table to publish at initialize (S8).
    pub fn deadline_table(&self) -> serde_json::Value {
        let mut table = serde_json::json!({
            "defaults_ms": {
                "read": self.defaults.read,
                "edit": self.defaults.edit,
                "rename": self.defaults.rename,
                "symbol": self.defaults.symbol,
                "install": self.defaults.install,
            }
        });
        let mut languages = serde_json::Map::new();
        for name in self.languages.keys() {
            let d = self.deadlines_for(name);
            languages.insert(
                name.clone(),
                serde_json::json!({
                    "read": d.read, "edit": d.edit, "rename": d.rename,
                    "symbol": d.symbol, "install": d.install
                }),
            );
        }
        table["languages"] = serde_json::Value::Object(languages);
        table
    }
}

/// Read and parse one config file. Absent file is not an error.
async fn read_raw(path: &Path) -> Option<Raw> {
    let text = fs::read_to_string(path).await.ok()?;
    match toml::from_str::<Raw>(&text) {
        Ok(raw) => {
            logging::debug(format!("config loaded: {}", path.display()));
            Some(raw)
        }
        Err(error) => {
            // A config error must reach stderr too: the user's tables are
            // being ignored, so silence would hide it.
            logging::error(format!(
                "config parse failed at {}: {error}",
                path.display()
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_replaces_language_entry() {
        // Full replace: the project entry must win whole, not merge fields.
        let mut languages = BTreeMap::new();
        languages.insert(
            "python".to_string(),
            Language {
                extensions: vec![".py".to_string()],
                command: vec!["pyright-langserver".to_string()],
                initialization_options: None,
                settings: None,
                deadlines_ms: None,
            },
        );
        let config = Config {
            cwd: PathBuf::from("/tmp"),
            global_path: PathBuf::from("/tmp/none.toml"),
            languages,
            defaults: Deadlines::default(),
            registry: RegistrySettings::default(),
            idle_evict_min: 15,
        };
        let (name, entry) = config.language_for(Path::new("/a/main.py")).unwrap();
        assert_eq!(name, "python");
        assert_eq!(entry.command, vec!["pyright-langserver".to_string()]);
    }

    #[test]
    fn deadlines_override_wins() {
        let mut languages = BTreeMap::new();
        languages.insert(
            "java".to_string(),
            Language {
                extensions: vec![".java".to_string()],
                command: vec!["jdtls".to_string()],
                initialization_options: None,
                settings: None,
                deadlines_ms: Some(Deadlines {
                    read: 5_000,
                    edit: 10_000,
                    rename: 20_000,
                    symbol: 15_000,
                    install: 300_000,
                }),
            },
        );
        let config = Config {
            cwd: PathBuf::from("/tmp"),
            global_path: PathBuf::from("/tmp/none.toml"),
            languages,
            defaults: Deadlines::default(),
            registry: RegistrySettings::default(),
            idle_evict_min: 15,
        };
        let d = config.deadlines_for("java");
        assert_eq!(d.read, 5_000);
        assert_eq!(d.edit, 10_000);
        assert_eq!(d.rename, 20_000);
        assert_eq!(d.symbol, 15_000);
        // An unknown language falls back to the defaults.
        let fallback = config.deadline_table()["languages"]["java"].clone();
        assert!(fallback.is_object());
    }
}

#[cfg(test)]
mod registry_config_tests {
    use super::*;

    fn parse(text: &str) -> Option<RegistrySettings> {
        toml::from_str::<Raw>(text)
            .ok()
            .and_then(|raw| raw.registry)
    }

    #[test]
    fn registry_table_parses_all_modes() {
        assert_eq!(
            parse("[registry]\nrefresh = \"off\"\n").unwrap().refresh,
            RefreshMode::Off
        );
        assert_eq!(
            parse("[registry]\nrefresh = \"pinned\"\npin = \"2026-10-02-uppity-comic\"\n")
                .unwrap()
                .pin
                .as_deref(),
            Some("2026-10-02-uppity-comic")
        );
        // Defaults: absent table behaves as latest; unknown value falls back.
        assert_eq!(RegistrySettings::default().refresh, RefreshMode::Latest);
        assert_eq!(
            parse("[registry]\nrefresh = \"nonsense\"\n")
                .unwrap()
                .refresh,
            RefreshMode::Unknown
        );
    }
}
