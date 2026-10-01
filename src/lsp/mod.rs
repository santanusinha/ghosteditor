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
}

impl LspManager {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            clients: Mutex::new(HashMap::new()),
            spawning: Mutex::new(HashMap::new()),
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
            return Ok(Some(client.clone()));
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
