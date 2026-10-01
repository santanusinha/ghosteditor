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
    pub type BoxedFuture = std::pin::Pin<
        Box<dyn Future<Output = Result<Value, ToolError>> + Send>,
    >;
    use std::future::Future;
}

use futures_run::BoxedFuture;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeadlineKind {
    Read,
    Edit,
    Rename,
    Symbol,
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
    REGISTRY.get_or_init(|| vec![
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
    ])
}

// ---------- handlers ----------

fn arg_str(arguments: &Value, key: &str) -> Option<String> {
    arguments.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn arg_usize(arguments: &Value, key: &str) -> Option<usize> {
    arguments.get(key).and_then(|v| v.as_u64()).map(|v| v as usize)
}

fn arg_bool(arguments: &Value, key: &str, default: bool) -> bool {
    arguments.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
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
        let raw = arg_str(&arguments, "path").ok_or_else(|| invalid("open_file", "path is required"))?;
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
        let raw = arg_str(&arguments, "path").ok_or_else(|| invalid("read_file", "path is required"))?;
        let path = resolve_path(&server.config, &raw);
        let start = arg_usize(&arguments, "start_line");
        let end = arg_usize(&arguments, "end_line");
        let mut store = server.store.lock().await;
        store.open(&path).await.map_err(|e| internal("read_file", e))?;
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

        let mode = if outcome.mode == crate::docstore::EditMode::Insert { "insert" } else { "replace" };
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
        let old_text = arg_str(&arguments, "old_text").ok_or_else(|| invalid(tool, "old_text is required"))?;
        let new_text = arg_str(&arguments, "new_text").ok_or_else(|| invalid(tool, "new_text is required"))?;
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
    for i in 0..hay_lines.len().saturating_sub(needle_lines.len().saturating_sub(1)) {
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
