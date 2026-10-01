//! Structured error envelope shared by every tool (P0.15).
//!
//! A timeout is a response, never silence. Every error tells the agent what
//! was asked, which layer failed, what state is safe, and what to do next.

use serde_json::json;

/// The category of a failure. One word, stable, agent-friendly.
#[derive(Debug)]
pub enum ErrorKind {
    /// The call ran past its deadline.
    Timeout { layer: String, deadline_ms: u64 },
    /// The input is wrong (bad range, missing file, ambiguous match).
    Invalid { hint: String },
    /// The tool ran but the target was missing (no server for this file, etc).
    Missing { what: String, next_step: String },
    /// An internal failure. The agent cannot fix it; the user can.
    Internal { reason: String },
}

impl ErrorKind {
    fn code(&self) -> &'static str {
        match self {
            ErrorKind::Timeout { .. } => "timeout",
            ErrorKind::Invalid { .. } => "invalid",
            ErrorKind::Missing { .. } => "missing",
            ErrorKind::Internal { .. } => "internal",
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorKind::Timeout { layer, deadline_ms } => {
                write!(f, "timeout in {layer} after {deadline_ms} ms")
            }
            ErrorKind::Invalid { hint } => write!(f, "invalid input: {hint}"),
            ErrorKind::Missing { what, next_step } => {
                write!(f, "missing {what}; next step: {next_step}")
            }
            ErrorKind::Internal { reason } => write!(f, "internal error: {reason}"),
        }
    }
}

/// A tool error ready to serialize into an MCP response.
#[derive(Debug)]
pub struct ToolError {
    pub kind: ErrorKind,
    pub tool: String,
}

impl ToolError {
    pub fn new(kind: ErrorKind, tool: impl Into<String>) -> Self {
        Self { kind, tool: tool.into() }
    }

    /// Structured payload. Text stays short; details live in the fields.
    pub fn to_payload(&self) -> serde_json::Value {
        let mut details = json!({});
        match &self.kind {
            ErrorKind::Timeout { layer, deadline_ms } => {
                details["layer"] = json!(layer);
                details["deadline_ms"] = json!(deadline_ms);
            }
            ErrorKind::Invalid { hint } => {
                details["hint"] = json!(hint);
            }
            ErrorKind::Missing { what, next_step } => {
                details["what"] = json!(what);
                details["next_step"] = json!(next_step);
            }
            ErrorKind::Internal { reason } => {
                details["reason"] = json!(reason);
            }
        }
        json!({
            "error": {
                "code": self.kind.code(),
                "tool": self.tool,
                "message": self.kind.to_string(),
                "details": details,
            }
        })
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.tool, self.kind)
    }
}

impl std::error::Error for ToolError {}

impl From<std::io::Error> for ToolError {
    fn from(error: std::io::Error) -> Self {
        ToolError::new(
            ErrorKind::Internal {
                reason: error.to_string(),
            },
            "io",
        )
    }
}
