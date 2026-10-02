//! Direct LSP clients (Phase 1). One child per (language, root).

pub mod client;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::lsp::client::LspClient;
use crate::registry::Registry;
use crate::util::ToolError;

/// Manages one client per language. Spawn on first use.
pub struct LspManager {
    config: Config,
    /// Registry fallback when the config has no entry for a path (P2.6f).
    registry: std::sync::Arc<tokio::sync::RwLock<Registry>>,
    clients: Mutex<HashMap<String, Arc<LspClient>>>,
    /// Spawn-in-progress guards so two tools do not spawn twice.
    spawning: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Restart bookkeeping per language: (count, window start) (P1.13).
    restarts: Mutex<HashMap<String, (u32, std::time::Instant)>>,
}

/// Cap: at most 3 restarts per 60 s, then the language stays down with
/// a reason (plan 4.5.7).
const RESTART_CAP: u32 = 3;
const RESTART_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

impl LspManager {
    pub fn new(config: Config, registry: std::sync::Arc<tokio::sync::RwLock<Registry>>) -> Self {
        Self {
            config,
            registry,
            clients: Mutex::new(HashMap::new()),
            spawning: Mutex::new(HashMap::new()),
            restarts: Mutex::new(HashMap::new()),
        }
    }

    /// The client for a language, spawning it on first use (P1.3).
    /// Returns None when the config has no entry for this language.
    pub async fn client_for(&self, language: &str) -> Result<Option<Arc<LspClient>>, ToolError> {
        let entry = match self.config.languages.get(language) {
            Some(entry) => entry,
            None => return Ok(None),
        };
        if let Some(client) = self.clients.lock().await.get(language) {
            if client.is_dead() {
                // Dead child: try a restart under the cap (P1.13).
                let restartable = {
                    let mut restarts = self.restarts.lock().await;
                    let entry = restarts
                        .entry(language.to_string())
                        .or_insert((0, std::time::Instant::now()));
                    if entry.1.elapsed() > RESTART_WINDOW {
                        *entry = (0, std::time::Instant::now());
                    }
                    if entry.0 < RESTART_CAP {
                        entry.0 += 1;
                        crate::util::logging::info(format!(
                            "lsp: restarting {language} (attempt {}/{})",
                            entry.0, RESTART_CAP
                        ));
                        true
                    } else {
                        crate::util::logging::info(format!(
                            "lsp: {language} stays down: restart cap reached"
                        ));
                        false
                    }
                };
                if !restartable {
                    self.clients.lock().await.remove(language);
                    return Err(ToolError::new(
                        crate::util::ErrorKind::Missing {
                            what: format!(
                                "language server for {language}: crashed 3 times in 60 s"
                            ),
                            next_step:
                                "file operations still work; fix or restart the server manually"
                                    .to_string(),
                        },
                        "lsp",
                    ));
                }
                self.clients.lock().await.remove(language);
            } else {
                return Ok(Some(client.clone()));
            }
        }

        // One spawner at a time per language.
        let guard_key = language.to_string();
        let guard = {
            let mut guards = self.spawning.lock().await;
            guards
                .entry(guard_key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _lock = guard.lock().await;

        // Another call may have finished the spawn while we waited.
        if let Some(client) = self.clients.lock().await.get(language) {
            return Ok(Some(client.clone()));
        }

        let options = entry
            .initialization_options
            .as_ref()
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null));
        let client = LspClient::spawn(&entry.command, &self.config.cwd, options).await?;
        let client = Arc::new(client);
        self.clients
            .lock()
            .await
            .insert(language.to_string(), client.clone());
        Ok(Some(client))
    }

    /// The client for a path. Probe order (P2.6f):
    /// 1. config entry for the language the config maps the extension to
    /// 2. registry recipe for the extension: `bin` on PATH, then in the
    ///    install dir (launch args from the overlay)
    ///
    /// The registry fills gaps; it never overrides explicit config.
    pub async fn client_for_path(
        &self,
        path: &Path,
    ) -> Result<Option<(String, Arc<LspClient>)>, ToolError> {
        // 1. Explicit config.
        if let Some((language, _)) = self.config.language_for(path) {
            return match self.client_for(&language).await? {
                Some(client) => Ok(Some((language, client))),
                None => Ok(None),
            };
        }
        // 2. Registry fallback.
        let Some(ext) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
        else {
            return Ok(None);
        };
        // Read guard only for the lookup; clone the recipe and its launch
        // command out in the same block. Nothing is awaited while held.
        let resolved = {
            let registry = self.registry.read().await;
            let recipes = registry.recipes_for_extension(&ext);
            let is_default = |r: &&crate::registry::Recipe| {
                r.supported
                    && r.overlay
                        .as_ref()
                        .is_some_and(|o| o.default_for.iter().any(|e| e == &ext))
            };
            let index = recipes
                .iter()
                .position(is_default)
                .or_else(|| recipes.iter().position(|r| r.supported));
            index.map(|i| {
                let recipe = recipes[i].clone();
                let command = registry.launch_command(recipes[i]);
                (recipe, command)
            })
        };
        let Some((recipe, Some(command))) = resolved else {
            return Ok(None);
        };
        let language = recipe
            .overlay
            .as_ref()
            .map(|o| o.language.clone())
            .unwrap_or_else(|| recipe.name.clone());
        self.client_for_command(&language, command).await
    }

    /// The existing client for a path when already spawned; None otherwise.
    /// Never spawns (P2.9). Mirrors the probe order of client_for_path.
    pub async fn try_client_for_path(&self, path: &Path) -> Option<(String, Arc<LspClient>)> {
        if let Some((language, _)) = self.config.language_for(path) {
            let client = self.clients.lock().await.get(&language).cloned();
            return client.map(|client| (language, client));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))?;
        let registry = self.registry.read().await;
        let recipes = registry.recipes_for_extension(&ext);
        let is_default = |r: &&crate::registry::Recipe| {
            r.supported
                && r.overlay
                    .as_ref()
                    .is_some_and(|o| o.default_for.iter().any(|e| e == &ext))
        };
        let language = recipes
            .iter()
            .position(is_default)
            .or_else(|| recipes.iter().position(|r| r.supported))
            .and_then(|i| recipes[i].overlay.as_ref().map(|o| o.language.clone()))?;
        let client = self.clients.lock().await.get(&language).cloned();
        client.map(|client| (language, client))
    }

    /// Spawn (or reuse) a client for a language with an explicit command.
    /// Mirrors `client_for`, but the command comes from the registry.
    async fn client_for_command(
        &self,
        language: &str,
        command: Vec<String>,
    ) -> Result<Option<(String, Arc<LspClient>)>, ToolError> {
        if let Some(client) = self.clients.lock().await.get(language) {
            if !client.is_dead() {
                return Ok(Some((language.to_string(), client.clone())));
            }
            self.clients.lock().await.remove(language);
        }
        // One spawner at a time per language.
        let guard_key = language.to_string();
        let guard = {
            let mut guards = self.spawning.lock().await;
            guards
                .entry(guard_key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _lock = guard.lock().await;
        if let Some(client) = self.clients.lock().await.get(language) {
            return Ok(Some((language.to_string(), client.clone())));
        }
        let client = LspClient::spawn(&command, &self.config.cwd, None).await?;
        let client = Arc::new(client);
        self.clients
            .lock()
            .await
            .insert(language.to_string(), client.clone());
        Ok(Some((language.to_string(), client)))
    }
}
