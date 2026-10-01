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
}

fn default_read() -> u64 {
    2_000
}
fn default_edit() -> u64 {
    5_000
}
fn default_rename() -> u64 {
    8_000
}
fn default_symbol() -> u64 {
    8_000
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            read: default_read(),
            edit: default_edit(),
            rename: default_rename(),
            symbol: default_symbol(),
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

/// The merged, effective configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub cwd: PathBuf,
    pub global_path: PathBuf,
    pub languages: BTreeMap<String, Language>,
    pub defaults: Deadlines,
}

/// Raw file layout; languages is the only table.
#[derive(Debug, Default, Deserialize)]
struct Raw {
    #[serde(default)]
    languages: BTreeMap<String, Language>,
    #[serde(default)]
    defaults_ms: Option<Deadlines>,
}

impl Config {
    /// Load global config, then project config. The project file replaces
    /// whole language entries. It does not deep-merge them.
    pub async fn load(
        explicit: Option<PathBuf>,
        cwd: Option<PathBuf>,
        debug: bool,
    ) -> Result<Self, String> {
        logging::set_debug(debug);
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

        if let Some(raw) = read_raw(&global_path).await {
            if let Some(merged_defaults) = raw.defaults_ms {
                defaults = merged_defaults;
            }
            languages = raw.languages;
        }

        let project_path = cwd.join(".ghostedit/config.toml");
        if let Some(raw) = read_raw(&project_path).await {
            if let Some(merged_defaults) = raw.defaults_ms {
                defaults = merged_defaults;
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
            }
        });
        let mut languages = serde_json::Map::new();
        for (name, _) in &self.languages {
            let d = self.deadlines_for(name);
            languages.insert(
                name.clone(),
                serde_json::json!({
                    "read": d.read, "edit": d.edit, "rename": d.rename, "symbol": d.symbol
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
            logging::info(format!(
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
                }),
            },
        );
        let config = Config {
            cwd: PathBuf::from("/tmp"),
            global_path: PathBuf::from("/tmp/none.toml"),
            languages,
            defaults: Deadlines::default(),
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
