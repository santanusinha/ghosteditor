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
use ghostedit::registry::Registry;

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
    /// Last use per language, for idle eviction (P2.10).
    last_used: Mutex<HashMap<String, std::time::Instant>>,
    /// Spawn time per language, for the warm-up grace (P3.3).
    spawned_at: Mutex<HashMap<String, std::time::Instant>>,
}

/// Cap: at most 3 restarts per 60 s, then the language stays down with
/// a reason (plan 4.5.7).
const RESTART_CAP: u32 = 3;
const RESTART_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// Warm-up grace: a fresh spawn is never evicted inside this window, even
/// when the idle timeout is shorter, so a cold server is not killed while
/// it answers its first calls (P3.3).
const WARMUP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// True when a client with this idle time and spawn age is an eviction
/// candidate. The warm-up grace keeps a fresh spawn alive (P3.3).
fn eviction_candidate(
    idle: std::time::Duration,
    spawn_age: Option<std::time::Duration>,
    idle_after: std::time::Duration,
) -> bool {
    let warmed = spawn_age.map(|age| age >= WARMUP_GRACE).unwrap_or(true);
    idle > idle_after && warmed
}

impl LspManager {
    pub fn new(config: Config, registry: std::sync::Arc<tokio::sync::RwLock<Registry>>) -> Self {
        Self {
            config,
            registry,
            clients: Mutex::new(HashMap::new()),
            spawning: Mutex::new(HashMap::new()),
            restarts: Mutex::new(HashMap::new()),
            last_used: Mutex::new(HashMap::new()),
            spawned_at: Mutex::new(HashMap::new()),
        }
    }

    /// Record a use of a language client (P2.10).
    pub async fn touch(&self, language: &str) {
        self.last_used
            .lock()
            .await
            .insert(language.to_string(), std::time::Instant::now());
    }

    /// Evict clients idle past the timeout (P2.10). Sends shutdown/exit
    /// best effort and drops the client; the child dies with it. A later
    /// call spawns fresh. Returns the evicted language names.
    pub async fn evict_idle(&self, idle_after: std::time::Duration) -> Vec<String> {
        let now = std::time::Instant::now();
        let mut evicted = Vec::new();
        let candidates: Vec<String> = {
            let used = self.last_used.lock().await;
            let spawned = self.spawned_at.lock().await;
            self.clients
                .lock()
                .await
                .keys()
                .filter(|language| {
                    // Idle time: last use when present, else the spawn
                    // clock; a client with no clock at all stays.
                    let idle = used
                        .get(*language)
                        .map(|last| now.duration_since(*last))
                        .or_else(|| spawned.get(*language).map(|at| now.duration_since(*at)));
                    let age = spawned.get(*language).map(|at| now.duration_since(*at));
                    idle.is_some_and(|idle| eviction_candidate(idle, age, idle_after))
                })
                .cloned()
                .collect()
        };
        for language in candidates {
            let client = self.clients.lock().await.remove(&language);
            if let Some(client) = client {
                client.shutdown().await;
                evicted.push(language.clone());
                self.last_used.lock().await.remove(&language);
                self.spawned_at.lock().await.remove(&language);
                crate::util::logging::info(format!("lsp: evicted idle client for {language}"));
            }
        }
        evicted
    }

    /// The client for a language, spawning it on first use (P1.3).
    /// Returns None when the config has no entry for this language.
    pub async fn client_for(&self, language: &str) -> Result<Option<Arc<LspClient>>, ToolError> {
        let entry = match self.config.languages.get(language) {
            Some(entry) => entry,
            None => return Ok(None),
        };
        self.touch(language).await;
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
        // P3.3: record the spawn clock for the warm-up grace.
        self.spawned_at
            .lock()
            .await
            .insert(language.to_string(), std::time::Instant::now());
        self.touch(language).await;
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
            let is_default = |r: &&ghostedit::registry::Recipe| {
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
            if client.is_some() {
                self.touch(&language).await;
            }
            return client.map(|client| (language, client));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))?;
        let registry = self.registry.read().await;
        let recipes = registry.recipes_for_extension(&ext);
        let is_default = |r: &&ghostedit::registry::Recipe| {
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
        if client.is_some() {
            self.touch(&language).await;
        }
        client.map(|client| (language, client))
    }

    /// Any usable client for a request that carries no path (query-only
    /// workspace symbol search). Preference order: a live spawned client
    /// (lowest language name, deterministic), then the first config
    /// language (spawns it). None when nothing is up and the config has
    /// no languages. The guard is never held across an await.
    pub async fn client_for_any(&self) -> Result<Option<(String, Arc<LspClient>)>, ToolError> {
        let reused = {
            let clients = self.clients.lock().await;
            clients
                .keys()
                .min()
                .cloned()
                .and_then(|language| clients.get(&language).map(|c| (language, c.clone())))
        };
        if let Some((language, client)) = reused {
            if !client.is_dead() {
                self.touch(&language).await;
                return Ok(Some((language, client)));
            }
        }
        match self.config.languages.keys().min().cloned() {
            Some(language) => {
                let client = self.client_for(&language).await?;
                Ok(client.map(|c| (language, c)))
            }
            None => Ok(None),
        }
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
        // P3.3: a fresh client is never evicted before its warm-up grace,
        // so a short idle window does not kill a server mid-analysis.
        self.spawned_at
            .lock()
            .await
            .insert(language.to_string(), std::time::Instant::now());
        Ok(Some((language.to_string(), client)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Idle eviction bookkeeping: touch refreshes last_used, so a touched
    /// language is never idle; an untouched entry is a candidate (P2.10).
    #[tokio::test]
    async fn touch_refreshes_idle_clock() {
        let config = Config {
            cwd: std::path::PathBuf::from("/tmp"),
            global_path: std::path::PathBuf::from("/tmp/none.toml"),
            languages: Default::default(),
            defaults: Default::default(),
            registry: crate::config::RegistrySettings::default(),
            idle_evict_min: 15,
        };
        let registry = std::sync::Arc::new(tokio::sync::RwLock::new(Registry::load()));
        let manager = LspManager::new(config, registry);
        manager.touch("rust").await;
        // A touched language with no spawned client evicts nothing; the
        // clock only gates clients that exist.
        let evicted = manager.evict_idle(std::time::Duration::from_secs(0)).await;
        assert!(evicted.is_empty());
    }

    /// P3.3: the warm-up grace keeps a fresh spawn alive inside 30 s, even
    /// with a zero idle window; a warmed-up idle client is a candidate.
    #[test]
    fn warmup_grace_gates_eviction() {
        let zero = std::time::Duration::from_secs(0);
        // Zero idle window would evict at once; the grace holds.
        assert!(!eviction_candidate(
            std::time::Duration::from_secs(1),
            Some(std::time::Duration::from_secs(1)),
            zero
        ));
        // Idle past the window and warmed up: evicted.
        assert!(eviction_candidate(
            std::time::Duration::from_secs(45),
            Some(std::time::Duration::from_secs(45)),
            std::time::Duration::from_secs(30)
        ));
        // Idle inside the window: kept.
        assert!(!eviction_candidate(
            std::time::Duration::from_secs(10),
            Some(std::time::Duration::from_secs(40)),
            std::time::Duration::from_secs(30)
        ));
        // Idle past a short window but still inside the grace: kept.
        assert!(!eviction_candidate(
            std::time::Duration::from_secs(20),
            Some(std::time::Duration::from_secs(20)),
            std::time::Duration::from_secs(10)
        ));
        // No spawn clock recorded: the idle rule alone decides.
        assert!(eviction_candidate(
            std::time::Duration::from_secs(40),
            None,
            std::time::Duration::from_secs(30)
        ));
    }
}
