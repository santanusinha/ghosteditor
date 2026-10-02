//! Tool registry: `open_file`, `read_file`, `edit_lines`, `edit_text` (P0.10-P0.13).
//!
//! LSP tools (goto_definition, hover, references, rename, diagnostics,
//! search, symbols) arrive in Phase 1 and Phase 2.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::config::Config;
use crate::fsops::{write_atomic, WriteOutcome};
use crate::mcp::Server;
use crate::registry::jobs as registry_jobs;
use crate::util::{ErrorKind, ToolError};

/// A tool description visible to the agent.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON schema for the arguments. Kept small on purpose.
    pub input_schema: Value,
    /// The operation class for the deadline budget.
    pub deadline_kind: DeadlineKind,
    /// Run the tool. The watchdog in the server enforces the budget.
    pub run: for<'s> fn(Arc<Server>, Value) -> futures_run::BoxedFuture,
}

/// Future alias so the handler table needs no generics.
pub mod futures_run {
    use super::*;
    pub type BoxedFuture = std::pin::Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send>>;
    use std::future::Future;
}

use futures_run::BoxedFuture;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeadlineKind {
    Read,
    Edit,
    Rename,
    /// Reserved for the Phase 2 symbol tools.
    #[allow(dead_code)]
    Symbol,
    Install,
}

impl Tool {
    /// The effective budget in ms. An agent may lower it, never raise it.
    pub fn deadline_ms(&self, config: &Config, arguments: &Value) -> u64 {
        let defaults = &config.defaults;
        let ceiling = match self.deadline_kind {
            DeadlineKind::Read => defaults.read,
            DeadlineKind::Edit => defaults.edit,
            DeadlineKind::Rename => defaults.rename,
            DeadlineKind::Symbol => defaults.symbol,
            DeadlineKind::Install => defaults.install,
        };
        // Optional per-language override wins over the global default.
        if let Some(path) = arguments.get("path").and_then(|v| v.as_str()) {
            if let Some((language, _)) = config.language_for(Path::new(path)) {
                let override_ms = config.deadlines_for(&language);
                let ceiling = match self.deadline_kind {
                    DeadlineKind::Read => override_ms.read,
                    DeadlineKind::Edit => override_ms.edit,
                    DeadlineKind::Rename => override_ms.rename,
                    DeadlineKind::Symbol => override_ms.symbol,
                    DeadlineKind::Install => override_ms.install,
                };
                return clamp_agent(ceiling, arguments);
            }
        }
        clamp_agent(ceiling, arguments)
    }
}

fn clamp_agent(ceiling: u64, arguments: &Value) -> u64 {
    match arguments.get("deadline_ms").and_then(|v| v.as_u64()) {
        Some(want) => want.min(ceiling),
        None => ceiling,
    }
}

/// The full registry, in agent-facing order.
pub fn tool_list() -> Vec<Value> {
    registry()
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema,
            })
        })
        .collect()
}

pub fn lookup(name: &str) -> Option<&'static Tool> {
    registry().iter().find(|tool| tool.name == name)
}

/// Built once on first use. `json!` is not const-evaluable.
fn registry() -> &'static Vec<Tool> {
    static REGISTRY: std::sync::OnceLock<Vec<Tool>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| {
        vec![
        Tool {
            name: "open_file",
            description: "Open a file. Reports facts: exists, line count, language. Never blocks on a language server.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "deadline_ms": { "type": "integer", "description": "Optional; can only lower the server budget" }
                },
                "required": ["path"]
            }),
            deadline_kind: DeadlineKind::Read,
            run: run_open_file,
        },
        Tool {
            name: "read_file",
            description: "Read numbered lines from a file. Lines are 1-based. Opens the file when needed.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" },
                    "deadline_ms": { "type": "integer" }
                },
                "required": ["path"]
            }),
            deadline_kind: DeadlineKind::Read,
            run: run_read_file,
        },
        Tool {
            name: "edit_lines",
            description: "Replace a line range, or insert before a line with start_line = end_line + 1. Saves atomically and returns diagnostics feedback.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" },
                    "text": { "type": "string" },
                    "save": { "type": "boolean", "default": true },
                    "deadline_ms": { "type": "integer" }
                },
                "required": ["path", "start_line", "end_line", "text"]
            }),
            deadline_kind: DeadlineKind::Edit,
            run: run_edit_lines,
        },
        Tool {
            name: "edit_text",
            description: "Replace exact text once. Tolerates whitespace drift, preserves indentation, refuses an ambiguous match, and hints the closest window on no match.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" },
                    "replace_all": { "type": "boolean", "default": false },
                    "save": { "type": "boolean", "default": true },
                    "deadline_ms": { "type": "integer" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
            deadline_kind: DeadlineKind::Edit,
            run: run_edit_text,
        },
        Tool {
            name: "goto_definition",
            description: "Find where the symbol at a position is defined, through the language server.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "line": { "type": "integer", "description": "1-based" },
                    "column": { "type": "integer", "description": "1-based" },
                    "deadline_ms": { "type": "integer" }
                },
                "required": ["path", "line", "column"]
            }),
            deadline_kind: DeadlineKind::Read,
            run: run_goto_definition,
        },
        Tool {
            name: "hover",
            description: "Type and documentation for the symbol at a position.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "line": { "type": "integer", "description": "1-based" },
                    "column": { "type": "integer", "description": "1-based" },
                    "deadline_ms": { "type": "integer" }
                },
                "required": ["path", "line", "column"]
            }),
            deadline_kind: DeadlineKind::Read,
            run: run_hover,
        },
        Tool {
            name: "references",
            description: "Find every reference to the symbol at a position.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "line": { "type": "integer", "description": "1-based" },
                    "column": { "type": "integer", "description": "1-based" },
                    "deadline_ms": { "type": "integer" }
                },
                "required": ["path", "line", "column"]
            }),
            deadline_kind: DeadlineKind::Read,
            run: run_references,
        },
        Tool {
            name: "diagnostics",
            description: "The latest diagnostics for a file, pushed by its language server. Empty when no server is attached.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "deadline_ms": { "type": "integer" }
                },
                "required": ["path"]
            }),
            deadline_kind: DeadlineKind::Read,
            run: run_diagnostics,
        },
        Tool {
            name: "rename",
            description: "Rename a symbol across the workspace. Applies the edit and saves every touched file atomically.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "line": { "type": "integer", "description": "1-based" },
                    "column": { "type": "integer", "description": "1-based" },
                    "new_name": { "type": "string" },
                    "deadline_ms": { "type": "integer" }
                },
                "required": ["path", "line", "column", "new_name"]
            }),
            deadline_kind: DeadlineKind::Rename,
            run: run_rename,
        },
        Tool {
            name: "install_server",
            description: "Install a language server from the registry by recipe name. Returns the final status, or 'running' when the deadline expires first. Poll by calling again.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Registry recipe name, e.g. pyright" },
                    "version": { "type": "string", "description": "Optional; defaults to the recipe version" },
                    "deadline_ms": { "type": "integer" }
                },
                "required": ["name"]
            }),
            deadline_kind: DeadlineKind::Install,
            run: run_install_server,
        },
        Tool {
            name: "search",
            description: "Search file contents across the workspace with the built-in walker. Literal (case-insensitive) or regex. Skips ignored and binary files. Capped at 200 matches; use next_offset to page.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Substring or regex pattern" },
                    "regex": { "type": "boolean", "default": false, "description": "Treat query as a regex" },
                    "offset": { "type": "integer", "default": 0, "description": "Skip the first N matches; pass next_offset to page" },
                    "deadline_ms": { "type": "integer" }
                },
                "required": ["query"]
            }),
            deadline_kind: DeadlineKind::Read,
            run: run_search,
        },
        Tool {
            name: "symbols",
            description: "Document symbols for one file (outline), or workspace symbol search by name. Uses the language server. Pass path for an outline; pass query to search names workspace-wide.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File for the document outline" },
                    "query": { "type": "string", "description": "Name search across the workspace" },
                    "max_results": { "type": "integer", "default": 200, "description": "Cap on returned symbols" },
                    "deadline_ms": { "type": "integer" }
                },
                "required": []
            }),
            deadline_kind: DeadlineKind::Symbol,
            run: run_symbols,
        },
    ]
    })
}

// ---------- Phase 2: install handler (P2.6c) ----------

/// `install_server`: register the job, then wait up to the budget minus a
/// second. The job keeps running past the budget; a later call reads the
/// finished status (P2.6b job manager).
fn run_install_server(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let tool = "install_server";
        let name = arg_str(&arguments, "name").ok_or_else(|| invalid(tool, "name is required"))?;
        let version = arg_str(&arguments, "version");

        // Look the recipe up in the registry; clone it out before awaiting.
        let recipe = {
            let registry = server.registry.read().await;
            match registry.by_name(&name) {
                Some(recipe) => std::sync::Arc::new(recipe.clone()),
                None => {
                    return Err(ToolError::new(
                        ErrorKind::Missing {
                            what: format!("no registry recipe named {name}"),
                            next_step: "check the name against the registry snapshot".to_string(),
                        },
                        tool,
                    ))
                }
            }
        };
        if !recipe.supported {
            return Err(ToolError::new(
                ErrorKind::Invalid {
                    hint: format!(
                        "{name} is not supported on this platform; the overlay pins it off"
                    ),
                },
                tool,
            ));
        }

        let status = server.jobs.start(recipe, version).await;

        // Wait for the job, but keep one second of the budget for the reply.
        let budget = deadline_for(&server, Path::new(&name), DeadlineKind::Install, &arguments);
        let wait = budget.saturating_sub(1_000).min(budget);
        let waited = tokio::time::sleep(std::time::Duration::from_millis(wait));
        tokio::pin!(waited);
        loop {
            tokio::select! {
                _ = &mut waited => break,
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    if let Some(current) = server.jobs.status(&name).await {
                        if current != registry_jobs::JobStatus::Running {
                            break;
                        }
                    }
                }
            }
        }
        let final_status = server.jobs.status(&name).await.unwrap_or(status.clone());
        Ok(registry_jobs::status_payload(&name, &final_status))
    })
}

// ---------- Phase 1: LSP handlers (P1.7-P1.11) ----------

fn run_goto_definition(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let tool = "goto_definition";
        let raw = arg_str(&arguments, "path").ok_or_else(|| invalid(tool, "path is required"))?;
        let line =
            arg_usize(&arguments, "line").ok_or_else(|| invalid(tool, "line is required"))?;
        let column =
            arg_usize(&arguments, "column").ok_or_else(|| invalid(tool, "column is required"))?;
        let path = resolve_path(&server.config, &raw);
        lsp_position_call(
            server,
            tool,
            path,
            raw,
            line,
            column,
            "textDocument/definition",
        )
        .await
    })
}

fn run_hover(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let tool = "hover";
        let raw = arg_str(&arguments, "path").ok_or_else(|| invalid(tool, "path is required"))?;
        let line =
            arg_usize(&arguments, "line").ok_or_else(|| invalid(tool, "line is required"))?;
        let column =
            arg_usize(&arguments, "column").ok_or_else(|| invalid(tool, "column is required"))?;
        let path = resolve_path(&server.config, &raw);
        lsp_position_call(server, tool, path, raw, line, column, "textDocument/hover").await
    })
}

fn run_references(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let tool = "references";
        let raw = arg_str(&arguments, "path").ok_or_else(|| invalid(tool, "path is required"))?;
        let line =
            arg_usize(&arguments, "line").ok_or_else(|| invalid(tool, "line is required"))?;
        let column =
            arg_usize(&arguments, "column").ok_or_else(|| invalid(tool, "column is required"))?;
        let path = resolve_path(&server.config, &raw);
        let (path, raw) = (path, raw);
        ensure_did_open(&server, &path).await?;
        let deadline = deadline_for(&server, &path, DeadlineKind::Read, &arguments);
        let Some((_language, client)) = server.lsp.client_for_path(&path).await? else {
            return missing_server(&server, &raw, &path).await;
        };
        let params = json!({
            "textDocument": { "uri": uri_string(&path) },
            "position": { "line": line - 1, "character": column - 1 },
            "context": { "includeDeclaration": true }
        });
        let result = client
            .request("textDocument/references", params, deadline)
            .await?;
        let items = result.as_array().cloned().unwrap_or_default();
        let lines: Vec<String> = items
            .iter()
            .map(|item| {
                let uri = item["uri"].as_str().unwrap_or("");
                let l = item["range"]["start"]["line"].as_u64().unwrap_or(0) + 1;
                let c = item["range"]["start"]["character"].as_u64().unwrap_or(0) + 1;
                format!("{uri}:{l}:{c}")
            })
            .collect();
        let text = if lines.is_empty() {
            "No references found.".to_string()
        } else {
            lines.join("\n")
        };
        Ok(json!({ "text": text, "count": items.len() }))
    })
}

fn run_diagnostics(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let tool = "diagnostics";
        let raw = arg_str(&arguments, "path").ok_or_else(|| invalid(tool, "path is required"))?;
        let path = resolve_path(&server.config, &raw);
        ensure_did_open(&server, &path).await?;
        let Some((_language, client)) = server.lsp.client_for_path(&path).await? else {
            let mut reply = missing_server(&server, &raw, &path).await?;
            reply["diagnostics"] = json!([]);
            return Ok(reply);
        };
        // Give a fast server a moment to push its first batch (P1.5).
        let settle = 300u64;
        tokio::time::sleep(std::time::Duration::from_millis(settle)).await;
        let pushed = client.diagnostics_for(&path).await;
        let items = pushed.map(|p| p.diagnostics).unwrap_or_default();
        let _names = ["ERROR", "WARN", "INFO", "HINT"];
        let rendered: Vec<String> = items
            .iter()
            .map(|d| {
                format!(
                    "{}:{} {} {}",
                    d.range.start.line + 1,
                    d.range.start.character + 1,
                    severity_name(d.severity),
                    d.message.replace('\n', " ")
                )
            })
            .collect();
        let text = if rendered.is_empty() {
            "No diagnostics.".to_string()
        } else {
            rendered.join("\n")
        };
        Ok(json!({
            "text": text,
            "diagnostics": items.iter().map(|d| json!({
                "line": d.range.start.line + 1,
                "column": d.range.start.character + 1,
                "severity": severity_name(d.severity),
                "message": d.message,
                "source": d.source,
            })).collect::<Vec<_>>()
        }))
    })
}

fn run_rename(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let tool = "rename";
        let raw = arg_str(&arguments, "path").ok_or_else(|| invalid(tool, "path is required"))?;
        let line =
            arg_usize(&arguments, "line").ok_or_else(|| invalid(tool, "line is required"))?;
        let column =
            arg_usize(&arguments, "column").ok_or_else(|| invalid(tool, "column is required"))?;
        let new_name =
            arg_str(&arguments, "new_name").ok_or_else(|| invalid(tool, "new_name is required"))?;
        let path = resolve_path(&server.config, &raw);
        ensure_did_open(&server, &path).await?;
        let deadline = deadline_for(&server, &path, DeadlineKind::Rename, &arguments);
        let Some((_language, client)) = server.lsp.client_for_path(&path).await? else {
            return missing_server(&server, &raw, &path).await;
        };
        let params = json!({
            "textDocument": { "uri": uri_string(&path) },
            "position": { "line": line - 1, "character": column - 1 },
            "newName": new_name
        });
        let mut result = client
            .request("textDocument/rename", params.clone(), deadline)
            .await;
        // Cold-start guard: a fresh server often answers -32602 before it
        // finished analysis. Wait for the first diagnostics push, then
        // retry once (the same pattern as the position tools).
        if is_cold_refusal(&result) {
            let mut waited = 0u64;
            while waited < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                waited += 500;
                if client.diagnostics_for(&path).await.is_some() {
                    break;
                }
            }
            result = client
                .request("textDocument/rename", params, deadline)
                .await;
        }
        let result = result?;
        let workspace_edit = match result.as_object() {
            Some(object) => object.clone(),
            None => {
                return Err(ToolError::new(
                    ErrorKind::Invalid {
                        hint: "the server does not support rename at this position".to_string(),
                    },
                    tool,
                ))
            }
        };
        apply_workspace_edit(&server, &workspace_edit).await
    })
}

/// Open the document in the DocStore and send didOpen to the server (P1.4).
async fn ensure_did_open(server: &Arc<Server>, path: &Path) -> Result<(), ToolError> {
    let text = {
        let mut store = server.store.lock().await;
        store.open(path).await.map_err(|e| internal("open", e))?;
        store.text(path)
    };
    let Some(text) = text else { return Ok(()) };
    if let Some((language, client)) = server.lsp.client_for_path(path).await? {
        client.did_open_once(path, &language, &text).await;
    }
    Ok(())
}

/// The effective deadline for a call, honoring per-server overrides.
fn deadline_for(server: &Arc<Server>, path: &Path, kind: DeadlineKind, arguments: &Value) -> u64 {
    if let Some((language, _)) = server.config.language_for(path) {
        let d = server.config.deadlines_for(&language);
        let ceiling = match kind {
            DeadlineKind::Read => d.read,
            DeadlineKind::Edit => d.edit,
            DeadlineKind::Rename => d.rename,
            DeadlineKind::Symbol => d.symbol,
            DeadlineKind::Install => d.install,
        };
        return clamp_agent(ceiling, arguments);
    }
    let ceiling = match kind {
        DeadlineKind::Read => server.config.defaults.read,
        DeadlineKind::Edit => server.config.defaults.edit,
        DeadlineKind::Rename => server.config.defaults.rename,
        DeadlineKind::Symbol => server.config.defaults.symbol,
        DeadlineKind::Install => server.config.defaults.install,
    };
    clamp_agent(ceiling, arguments)
}

fn uri_string(path: &Path) -> String {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", absolute.display())
}

/// The reply when no language server resolves for a path (P2.6d). When the
/// registry has install one, When it can, the reply carries the
/// `language_server_missing` envelope with the recipe name and status.
/// Otherwise it keeps the legacy text.
async fn missing_server(server: &Arc<Server>, raw: &str, path: &Path) -> Result<Value, ToolError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"));
    if let Some(ext) = ext {
        // Hold the read guard only for the lookup; clone what we need out.
        let candidate = {
            let registry = server.registry.read().await;
            let recipes = registry.recipes_for_extension(&ext);
            // Prefer the recipe that owns this extension by default.
            let picked = recipes
                .iter()
                .find(|recipe| {
                    recipe.supported
                        && recipe
                            .overlay
                            .as_ref()
                            .is_some_and(|o| o.default_for.iter().any(|e| e == &ext))
                })
                .or_else(|| recipes.iter().find(|recipe| recipe.supported));
            picked.map(|recipe| {
                let language = recipe
                    .overlay
                    .as_ref()
                    .map(|o| o.language.clone())
                    .unwrap_or_default();
                (recipe.name.clone(), registry.status_for(&language))
            })
        };
        if let Some((name, status)) = candidate {
            let hint = match status {
                "not_installed" => format!(
                    "a recipe exists for {name} but it is not installed. Call install_server with name {name}."
                ),
                _ => format!(
                    "a recipe exists for {name} ({status})."
                ),
            };
            return Ok(json!({
                "text": format!("No language server configured for {raw}. {hint}"),
                "language_server_missing": {
                    "path": path.display().to_string(),
                    "extension": ext,
                    "recipe": name,
                    "recipe_status": status,
                    "install_command": format!("install_server {{\"name\": \"{name}\"}}"),
                },
                "locations": [],
            }));
        }
    }
    Ok(json!({
        "text": format!("No language server configured for {raw}."),
        "locations": [],
    }))
}

/// One LSP position call: definition or hover (P1.7, P1.8).
async fn lsp_position_call(
    server: Arc<Server>,
    _tool: &'static str,
    path: PathBuf,
    raw: String,
    line: usize,
    column: usize,
    method: &'static str,
) -> Result<Value, ToolError> {
    ensure_did_open(&server, &path).await?;
    let deadline = deadline_for(&server, &path, DeadlineKind::Read, &Value::Null);
    let Some((_language, client)) = server.lsp.client_for_path(&path).await? else {
        return missing_server(&server, &raw, &path).await;
    };
    let params = json!({
        "textDocument": { "uri": uri_string(&path) },
        "position": { "line": line - 1, "character": column - 1 }
    });
    let mut result = client.request(method, params.clone(), deadline).await?;
    // Cold-start guard: an empty answer often means the server is still
    // analyzing. Wait for its first diagnostics push, then retry once
    // within the same budget (the agent-lsp fuzzy-retry pattern).
    let empty = match method {
        "textDocument/hover" => hover_is_empty(&result),
        _ => definition_is_empty(&result),
    };
    if empty {
        let mut waited = 0u64;
        while waited < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            waited += 500;
            // Any first push (even an empty batch) means the initial
            // analysis pass finished (gopls pushes empty arrays).
            if client.diagnostics_for(&path).await.is_some() {
                break;
            }
        }
        result = client.request(method, params, deadline).await?;
    }
    // Fuzzy position fallback (P1.14): an agent often lands on whitespace
    // next to the identifier. Nudge left/right to an identifier character
    // and retry once.
    let still_empty = match method {
        "textDocument/hover" => hover_is_empty(&result),
        _ => definition_is_empty(&result),
    };
    if still_empty {
        if let Some(nudged) = nudge_position(&server, &path, line, column).await {
            let params = json!({
                "textDocument": { "uri": uri_string(&path) },
                "position": { "line": nudged.0, "character": nudged.1 }
            });
            result = client.request(method, params, deadline).await?;
        }
    }
    match method {
        "textDocument/hover" => {
            let text = hover_text(&result);
            Ok(json!({ "text": text }))
        }
        _ => {
            let items = definition_locations(&result);
            let text = if items.is_empty() {
                "No definition found.".to_string()
            } else {
                items.join("\n")
            };
            Ok(json!({ "text": text, "locations": items }))
        }
    }
}

fn hover_text(result: &Value) -> String {
    let contents = &result["contents"];
    if let Some(value) = contents.as_str() {
        return value.to_string();
    }
    if let Some(value) = contents["value"].as_str() {
        return value.to_string();
    }
    "No hover information.".to_string()
}

fn definition_locations(result: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let items = match result.as_array() {
        Some(items) => items.clone(),
        None => vec![result.clone()],
    };
    for item in items {
        let uri = item["uri"]
            .as_str()
            .or_else(|| item["targetUri"].as_str())
            .unwrap_or("");
        if uri.is_empty() {
            continue;
        }
        let range = &item["range"];
        let l = range["start"]["line"].as_u64().unwrap_or(0) + 1;
        let c = range["start"]["character"].as_u64().unwrap_or(0) + 1;
        out.push(format!("{uri}:{l}:{c}"));
    }
    out
}

/// Apply a WorkspaceEdit: edit each file in memory, save atomically (P1.11).
async fn apply_workspace_edit(
    server: &Arc<Server>,
    edit: &serde_json::Map<String, Value>,
) -> Result<Value, ToolError> {
    let mut changed_files: Vec<String> = Vec::new();
    // Servers answer with `changes`, `documentChanges`, or both (LSP spec).
    let mut per_file: Vec<(String, &Value)> = Vec::new();
    if let Some(changes) = edit.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits) in changes {
            per_file.push((uri.clone(), edits));
        }
    }
    if let Some(document_changes) = edit.get("documentChanges").and_then(|c| c.as_array()) {
        for change in document_changes {
            if let Some(uri) = change
                .get("textDocument")
                .and_then(|t| t.get("uri"))
                .and_then(|u| u.as_str())
            {
                if let Some(edits) = change.get("edits") {
                    per_file.push((uri.to_string(), edits));
                }
            }
        }
    }
    for (uri, edits) in per_file {
        let path = uri_to_path(&uri);
        let mut store = server.store.lock().await;
        store.open(&path).await.map_err(|e| internal("rename", e))?;
        let mut text = store.text(&path).unwrap_or_default();
        apply_text_edits(&mut text, edits);
        if let Some(document) = store.document_mut(&path) {
            document.text = text.clone();
            document.version += 1;
        }
        crate::fsops::write_atomic(&path, &text)
            .await
            .map_err(|e| internal("rename", e))?;
        store
            .saved(&path)
            .await
            .map_err(|e| internal("rename", e))?;
        changed_files.push(path.display().to_string());
    }
    let text = if changed_files.is_empty() {
        "No changes produced.".to_string()
    } else {
        format!(
            "Renamed in {} file(s): {}",
            changed_files.len(),
            changed_files.join(", ")
        )
    };
    Ok(json!({ "text": text, "files": changed_files }))
}

/// Apply LSP TextEdits to text. Edits are UTF-16 offsets (P1.11).
fn apply_text_edits(text: &mut String, edits: &Value) {
    let mut ranges: Vec<(usize, usize, String)> = Vec::new();
    if let Some(list) = edits.as_array() {
        for edit in list {
            let start = utf16_to_byte(
                text,
                edit["range"]["start"]["line"].as_u64().unwrap_or(0),
                edit["range"]["start"]["character"].as_u64().unwrap_or(0),
            );
            let end = utf16_to_byte(
                text,
                edit["range"]["end"]["line"].as_u64().unwrap_or(0),
                edit["range"]["end"]["character"].as_u64().unwrap_or(0),
            );
            let new_text = edit["newText"].as_str().unwrap_or("").to_string();
            ranges.push((start, end, new_text));
        }
    }
    // Apply back-to-front so offsets stay valid.
    ranges.sort_by_key(|r| std::cmp::Reverse(r.0));
    for (start, end, new_text) in ranges {
        let start = start.min(text.len());
        let end = end.min(text.len()).max(start);
        text.replace_range(start..end, &new_text);
    }
}

/// Convert a UTF-16 line/character pair to a byte offset in `text`.
fn utf16_to_byte(text: &str, line: u64, character: u64) -> usize {
    let mut current_line = 0u64;
    for (i, ch) in text.char_indices() {
        if current_line == line {
            // Walk this line counting UTF-16 units to the target.
            let mut units = 0u64;
            for (j, c) in text[i..].char_indices() {
                if units >= character || c == '\n' {
                    return i + j;
                }
                units += c.len_utf16() as u64;
            }
            return text.len();
        }
        if ch == '\n' {
            current_line += 1;
        }
    }
    text.len()
}

fn uri_to_path(uri: &str) -> PathBuf {
    let stripped = uri.strip_prefix("file://").unwrap_or(uri);
    PathBuf::from(stripped)
}

// ---------- handlers ----------

fn arg_str(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn arg_usize(arguments: &Value, key: &str) -> Option<usize> {
    arguments
        .get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
}

fn arg_bool(arguments: &Value, key: &str, default: bool) -> bool {
    arguments
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}

fn resolve_path(config: &Config, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config.cwd.join(path)
    }
}

fn box_future<F>(fut: F) -> BoxedFuture
where
    F: Future<Output = Result<Value, ToolError>> + Send + 'static,
{
    Box::pin(fut)
}

use std::future::Future;

fn run_open_file(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let raw =
            arg_str(&arguments, "path").ok_or_else(|| invalid("open_file", "path is required"))?;
        let path = resolve_path(&server.config, &raw);
        let facts = server
            .store
            .lock()
            .await
            .open(&path)
            .await
            .map_err(|e| internal("open_file", e))?;
        let language = server
            .config
            .language_for(&path)
            .map(|(name, _)| name)
            .unwrap_or_else(|| "unknown".to_string());
        Ok(json!({
            "text": format!(
                "Opened {}. exists={} lines={} language={}",
                raw, facts.exists, facts.line_count, language
            ),
            "path": path.display().to_string(),
            "exists": facts.exists,
            "line_count": facts.line_count,
            "reloaded": facts.reloaded,
            "language": language,
        }))
    })
}

fn run_read_file(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let raw =
            arg_str(&arguments, "path").ok_or_else(|| invalid("read_file", "path is required"))?;
        let path = resolve_path(&server.config, &raw);
        let start = arg_usize(&arguments, "start_line");
        let end = arg_usize(&arguments, "end_line");
        let mut store = server.store.lock().await;
        store
            .open(&path)
            .await
            .map_err(|e| internal("read_file", e))?;
        let slice = store
            .read(&path, start, end)
            .ok_or_else(|| internal("read_file", "document missing after open"))?;
        let numbered: Vec<String> = slice
            .lines
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}", slice.start_line + i, line))
            .collect();
        let mut text = numbered.join("\n");
        if slice.truncated {
            text.push_str(&format!(
                "\n... truncated at line {} of {}",
                slice.end_line, slice.total_lines
            ));
        }
        Ok(json!({
            "text": text,
            "start_line": slice.start_line,
            "end_line": slice.end_line,
            "total_lines": slice.total_lines,
        }))
    })
}

fn run_edit_lines(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let tool = "edit_lines";
        let raw = arg_str(&arguments, "path").ok_or_else(|| invalid(tool, "path is required"))?;
        let start = arg_usize(&arguments, "start_line")
            .ok_or_else(|| invalid(tool, "start_line is required"))?;
        let end = arg_usize(&arguments, "end_line")
            .ok_or_else(|| invalid(tool, "end_line is required"))?;
        let text = arg_str(&arguments, "text").ok_or_else(|| invalid(tool, "text is required"))?;
        let save = arg_bool(&arguments, "save", true);
        let path = resolve_path(&server.config, &raw);

        let mut store = server.store.lock().await;
        store.open(&path).await.map_err(|e| internal(tool, e))?;
        let document = store
            .document_mut(&path)
            .ok_or_else(|| internal(tool, "document missing after open"))?;
        let outcome = document
            .edit_lines(start, end, &text)
            .map_err(|hint| invalid(tool, hint))?;
        let new_text = document.text.clone();

        let saved = if save {
            let outcome_kind = write_atomic(&path, &new_text)
                .await
                .map_err(|e| internal(tool, e))?;
            store.saved(&path).await.map_err(|e| internal(tool, e))?;
            Some(outcome_kind)
        } else {
            None
        };

        let mode = if outcome.mode == crate::docstore::EditMode::Insert {
            "insert"
        } else {
            "replace"
        };
        Ok(json!({
            "text": format!(
                "{} {} lines {}..{}. new_line_count={} saved={}",
                mode, raw, start, end, outcome.new_line_count, save
            ),
            "path": path.display().to_string(),
            "mode": mode,
            "new_line_count": outcome.new_line_count,
            "saved": save,
            "write_outcome": match saved {
                Some(WriteOutcome::Atomic) => "atomic",
                Some(WriteOutcome::Direct) => "direct",
                None => "none",
            },
            "errors": 0,
            "warnings": 0,
        }))
    })
}

fn run_edit_text(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let tool = "edit_text";
        let raw = arg_str(&arguments, "path").ok_or_else(|| invalid(tool, "path is required"))?;
        let old_text =
            arg_str(&arguments, "old_text").ok_or_else(|| invalid(tool, "old_text is required"))?;
        let new_text =
            arg_str(&arguments, "new_text").ok_or_else(|| invalid(tool, "new_text is required"))?;
        let replace_all = arg_bool(&arguments, "replace_all", false);
        let save = arg_bool(&arguments, "save", true);
        let path = resolve_path(&server.config, &raw);

        let mut store = server.store.lock().await;
        store.open(&path).await.map_err(|e| internal(tool, e))?;
        let current = store
            .text(&path)
            .ok_or_else(|| internal(tool, "document missing after open"))?;

        let found = find_matches(&current, &old_text, replace_all);
        match found.len() {
            0 => {
                let (line, window) = closest_window(&current, &old_text);
                Err(ToolError::new(
                    ErrorKind::Invalid {
                        hint: format!("no match. closest window at line {line}:\n{window}"),
                    },
                    tool,
                ))
            }
            1 if !replace_all => {
                let (from, to) = found[0];
                let replaced = format!("{}{}{}", &current[..from], new_text, &current[to..]);
                let document = store
                    .document_mut(&path)
                    .ok_or_else(|| internal(tool, "document missing"))?;
                document.text = replaced;
                document.version += 1;
                if save {
                    write_atomic(&path, &document.text)
                        .await
                        .map_err(|e| internal(tool, e))?;
                    store.saved(&path).await.map_err(|e| internal(tool, e))?;
                }
                let line = 1 + current[..from].matches('\n').count();
                Ok(json!({
                    "text": format!("Replaced 1 match at line {line}. saved={save}"),
                    "path": path.display().to_string(),
                    "replacements": 1,
                    "line": line,
                    "saved": save,
                    "errors": 0,
                    "warnings": 0,
                }))
            }
            count => Err(ToolError::new(
                ErrorKind::Invalid {
                    hint: format!(
                        "{count} matches. add more context to old_text, or set replace_all"
                    ),
                },
                tool,
            )),
        }
    })
}

/// Byte ranges of whole-line matches, whitespace-tolerant (P0.13).
/// Byte ranges of whole-line matches, whitespace-tolerant (P0.13).
fn find_matches(haystack: &str, needle: &str, _all: bool) -> Vec<(usize, usize)> {
    // Tier 1: exact. Tier 2: trailing-whitespace-insensitive per line.
    let hay_lines: Vec<&str> = haystack.split('\n').collect();
    let needle_lines: Vec<&str> = needle.split('\n').collect();
    if needle_lines.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    fn norm(s: &str) -> &str {
        s.trim_end()
    }
    for i in 0..hay_lines.len().saturating_sub(needle_lines.len() - 1) {
        let mut ok = true;
        for (j, needle_line) in needle_lines.iter().enumerate() {
            if norm(hay_lines.get(i + j).copied().unwrap_or("")) != norm(needle_line) {
                ok = false;
                break;
            }
        }
        if ok {
            let from: usize = hay_lines[..i].iter().map(|l| l.len() + 1).sum();
            let to: usize = from
                + hay_lines[i..i + needle_lines.len()]
                    .iter()
                    .map(|l| l.len() + 1)
                    .sum::<usize>()
                - 1;
            out.push((from, to.min(haystack.len())));
        }
    }
    out
}

/// Closest window hint: the needle-sized slice with most equal lines (4.5.8).
fn closest_window(haystack: &str, needle: &str) -> (usize, String) {
    let hay_lines: Vec<&str> = haystack.split('\n').collect();
    let needle_lines: Vec<&str> = needle.split('\n').collect();
    let mut best = 1;
    let mut best_score = 0usize;
    for i in 0..hay_lines
        .len()
        .saturating_sub(needle_lines.len().saturating_sub(1))
    {
        let score = needle_lines
            .iter()
            .enumerate()
            .filter(|(j, n)| hay_lines.get(i + j).map(|h| h == *n).unwrap_or(false))
            .count();
        if score > best_score {
            best_score = score;
            best = i + 1;
        }
    }
    let window: Vec<&str> = hay_lines
        .iter()
        .skip(best - 1)
        .take(needle_lines.len())
        .copied()
        .collect();
    (best, window.join("\n"))
}

/// Find the nearest identifier character around a position, 0-based LSP
/// coordinates. Searches outward on the same line (P1.14).
async fn nudge_position(
    server: &Arc<Server>,
    path: &Path,
    line: usize,
    column: usize,
) -> Option<(u64, u64)> {
    let text = {
        let mut store = server.store.lock().await;
        store.open(path).await.ok()?;
        store.text(path)
    }?;
    let lines: Vec<&str> = text.split('\n').collect();
    let line_text = lines.get(line - 1)?;
    let char_index = column - 1;
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    // Search outward from the given column.
    for distance in 0..line_text.len() {
        for candidate in [
            char_index as i64 - distance as i64,
            char_index as i64 + distance as i64,
        ] {
            if candidate < 0 {
                continue;
            }
            let at = line_text.char_indices().nth(candidate as usize);
            if let Some((_, c)) = at {
                if is_ident(c) {
                    return Some(((line - 1) as u64, candidate as u64));
                }
            }
        }
    }
    None
}

/// True when the server refused with a no-references style error, which
/// on a cold start usually means "not analyzed yet".
fn is_cold_refusal(result: &Result<Value, ToolError>) -> bool {
    match result {
        Ok(_) => false,
        Err(error) => {
            let message = error.to_string();
            message.contains("No references found") || message.contains("-32602")
        }
    }
}

fn hover_is_empty(result: &Value) -> bool {
    result.get("result").map(|r| r.is_null()).unwrap_or(true)
        || hover_text(result).contains("No hover information")
}

fn definition_is_empty(result: &Value) -> bool {
    match result.as_array() {
        Some(items) => items.is_empty(),
        None => result.get("result").map(|r| r.is_null()).unwrap_or(true),
    }
}

fn severity_name(severity: Option<lsp_types::DiagnosticSeverity>) -> &'static str {
    match severity {
        Some(lsp_types::DiagnosticSeverity::ERROR) => "ERROR",
        Some(lsp_types::DiagnosticSeverity::WARNING) => "WARN",
        Some(lsp_types::DiagnosticSeverity::INFORMATION) => "INFO",
        Some(lsp_types::DiagnosticSeverity::HINT) => "HINT",
        _ => "HINT",
    }
}

fn invalid(tool: &str, hint: impl AsRef<str>) -> ToolError {
    ToolError::new(
        ErrorKind::Invalid {
            hint: hint.as_ref().to_string(),
        },
        tool,
    )
}

fn internal(tool: &str, error: impl std::fmt::Display) -> ToolError {
    ToolError::new(
        ErrorKind::Internal {
            reason: error.to_string(),
        },
        tool,
    )
}

// ---------- Phase 2: search handler (P2.7) ----------

/// `search`: run the walker in a blocking task so the async runtime stays
/// responsive, inside the tool deadline set by the watchdog.
fn run_search(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let tool = "search";
        let query =
            arg_str(&arguments, "query").ok_or_else(|| invalid(tool, "query is required"))?;
        let regex = arguments
            .get("regex")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let offset = arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let root = server.config.cwd.clone();
        let moved_query = query.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::walker::search(&root, &moved_query, regex, offset)
        })
        .await
        .map_err(|e| internal(tool, format!("walker task failed: {e}")))?
        .map_err(|e| invalid(tool, e))?;
        let matches: Vec<Value> = result
            .matches
            .iter()
            .map(|m| json!({ "path": m.path, "line": m.line, "column": m.column, "text": m.text }))
            .collect();
        let next_offset = if result.truncated {
            json!(offset + result.matches.len())
        } else {
            Value::Null
        };
        Ok(json!({
            "matches": matches,
            "count": result.matches.len(),
            "files_scanned": result.files_scanned,
            "truncated": result.truncated,
            "next_offset": next_offset,
            "text": format!(
                "{} matches across {} files{}",
                result.matches.len(),
                result.files_scanned,
                if result.truncated { " (truncated; pass next_offset)" } else { "" }
            ),
        }))
    })
}

// ---------- Phase 2: symbols handler (P2.8) ----------

/// `symbols`: documentSymbol outline for a path, or workspace/symbol name
/// search. Results are capped; `next_offset` pages the workspace search.
fn run_symbols(server: Arc<Server>, arguments: Value) -> BoxedFuture {
    box_future(async move {
        let tool = "symbols";
        let max_results = arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(crate::walker::MAX_MATCHES as u64)
            .clamp(1, 500) as usize;
        let raw_path = arg_str(&arguments, "path");
        let query = arg_str(&arguments, "query");
        if raw_path.is_none() && query.is_none() {
            return Err(invalid(
                tool,
                "pass path for an outline, or query for name search",
            ));
        }

        // Workspace-wide name search. The path picks the server.
        if let Some(query) = query {
            let raw = raw_path.expect("checked above");
            let path = resolve_path(&server.config, &raw);
            ensure_did_open(&server, &path).await?;
            let Some((language, client)) = server.lsp.client_for_path(&path).await? else {
                return missing_server(&server, &raw, &path).await;
            };
            let deadline = deadline_for(&server, &path, DeadlineKind::Symbol, &arguments);
            let params = json!({ "query": query });
            // Cold-start guard: a fresh server answers an empty list before
            // it indexed the workspace. Poll within the deadline until the
            // reply is non-empty (same trade-off as hover: a cold start can
            // burn the budget; a warm server answers at once).
            let start = std::time::Instant::now();
            let mut result = client
                .request("workspace/symbol", params.clone(), deadline)
                .await;
            loop {
                let cold_empty =
                    matches!(&result, Ok(v) if v.as_array().is_some_and(|a| a.is_empty()));
                if !cold_empty || start.elapsed().as_millis() as u64 >= deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                result = client
                    .request("workspace/symbol", params.clone(), deadline)
                    .await;
            }
            let result = result?;
            let items = symbol_items(&result);
            let total = items.len();
            let page: Vec<Value> = items.into_iter().take(max_results).collect();
            let truncated = total > page.len();
            let next_offset = if truncated {
                json!(max_results)
            } else {
                Value::Null
            };
            let text = if page.is_empty() {
                "No symbols found.".to_string()
            } else {
                page.iter()
                    .map(|s| symbol_entry_text(s, 0))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            return Ok(json!({
                "language": language,
                "symbols": page,
                "count": page.len(),
                "total": total,
                "truncated": truncated,
                "next_offset": next_offset,
                "text": text,
            }));
        }

        // Document outline.
        let raw = raw_path.expect("checked above");
        let path = resolve_path(&server.config, &raw);
        ensure_did_open(&server, &path).await?;
        let deadline = deadline_for(&server, &path, DeadlineKind::Symbol, &arguments);
        let Some((language, client)) = server.lsp.client_for_path(&path).await? else {
            return missing_server(&server, &raw, &path).await;
        };
        let params = json!({
            "textDocument": { "uri": uri_string(&path) }
        });
        let result = client
            .request("textDocument/documentSymbol", params, deadline)
            .await?;
        let items = document_symbol_tree(&result, 0, max_results);
        let text = if items.is_empty() {
            "No symbols found.".to_string()
        } else {
            items
                .iter()
                .map(|s| s["entry"].as_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(json!({
            "language": language,
            "symbols": items,
            "count": items.len(),
            "text": text,
        }))
    })
}

/// Flatten a documentSymbol reply into one list. Servers answer with
/// DocumentSymbol[] (nested children) or SymbolInformation[] (flat, location).
/// Both forms are handled; entries carry a kind name, 1-based position, and a
/// depth for indent rendering.
fn document_symbol_tree(result: &Value, depth: usize, cap: usize) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(nodes) = result.as_array() else {
        return out;
    };
    for node in nodes {
        if out.len() >= cap {
            break;
        }
        let Some(name) = node.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        // documentSymbol form: range at the top level.
        let start = node
            .get("range")
            .and_then(|r| r.get("start"))
            // SymbolInformation form: location.range.start.
            .or_else(|| {
                node.get("location")
                    .and_then(|l| l.get("range"))
                    .and_then(|r| r.get("start"))
            });
        let line = start
            .and_then(|s| s.get("line"))
            .and_then(|l| l.as_u64())
            .unwrap_or(0)
            + 1;
        let column = start
            .and_then(|s| s.get("character"))
            .and_then(|c| c.as_u64())
            .unwrap_or(0)
            + 1;
        let kind = symbol_kind_name(node["kind"].as_u64().unwrap_or(0));
        let entry = format!(
            "{}{} {} {}:{}",
            "  ".repeat(depth),
            kind,
            name,
            line,
            column
        );
        out.push(json!({
            "name": name,
            "kind": kind,
            "line": line,
            "column": column,
            "depth": depth,
            "entry": entry,
        }));
        if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
            let mut nested =
                document_symbol_tree(&Value::Array(children.clone()), depth + 1, cap - out.len());
            out.append(&mut nested);
        }
    }
    out
}

/// One text line for a workspace/symbol item (SymbolInformation or
/// WorkspaceSymbol, both carry name, kind, location).
fn symbol_entry_text(item: &Value, depth: usize) -> String {
    let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let kind = symbol_kind_name(item["kind"].as_u64().unwrap_or(0));
    let start = item
        .get("location")
        .and_then(|l| l.get("range"))
        .and_then(|r| r.get("start"));
    let line = start
        .and_then(|s| s.get("line"))
        .and_then(|l| l.as_u64())
        .unwrap_or(0)
        + 1;
    let file = item
        .get("location")
        .and_then(|l| l.get("uri"))
        .and_then(|u| u.as_str())
        .and_then(|u| u.rsplit('/').next())
        .map(|f| format!(" {f}:"))
        .unwrap_or_default();
    format!("{}{} {} {}{}", "  ".repeat(depth), kind, name, file, line)
}

/// Normalize a workspace/symbol reply to a symbol list. A null reply or a
/// non-array body means no symbols.
fn symbol_items(result: &Value) -> Vec<Value> {
    result.as_array().cloned().unwrap_or_default()
}

/// LSP SymbolKind numbers to short names.
fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum_member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_parameter",
        _ => "symbol",
    }
}
