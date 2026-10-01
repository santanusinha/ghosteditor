//! Direct LSP clients (Phase 1). One child per (language, root).

pub mod client;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::lsp::client::LspClient;
use crate::util::ToolError;

/// Manages one client per language. Spawn on first use.
pub struct LspManager {
    config: Config,
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
    pub fn new(config: Config) -> Self {
        Self {
            config,
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

    /// The language for a path, then the client for it.
    pub async fn client_for_path(
        &self,
        path: &Path,
    ) -> Result<Option<(String, Arc<LspClient>)>, ToolError> {
        let language = match self.config.language_for(path) {
            Some((name, _)) => name,
            None => return Ok(None),
        };
        match self.client_for(&language).await? {
            Some(client) => Ok(Some((language, client))),
            None => Ok(None),
        }
    }
}
