//! The in-memory document store (P0.9).
//!
//! A document is the open copy of one file. It carries the text and a
//! version that grows with every edit. On read, the store compares disk
//! mtime and size; an external change reloads the text.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tokio::fs;

/// Facts about one open document.
#[derive(Debug, Clone)]
pub struct Document {
    pub path: PathBuf,
    /// The text as a string. `\n` line endings, no final newline guarantee.
    pub text: String,
    /// Grows by one with every in-memory edit.
    pub version: u64,
    /// mtime of the disk file at load or last save. External-change guard.
    disk_mtime: Option<SystemTime>,
    disk_size: u64,
}

impl Document {
    pub fn lines(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }

    pub fn line_count(&self) -> usize {
        self.lines().len()
    }

    /// Replace a line range with new text (P0.12).
    /// `start = end + 1` inserts before `start`.
    pub fn edit_lines(
        &mut self,
        start: usize,
        end: usize,
        text: &str,
    ) -> Result<EditOutcome, String> {
        let mut lines = self.lines();
        let total = lines.len();
        if start == 0 || start > total + 1 {
            return Err(format!(
                "start_line {start} is out of range 1..{}",
                total + 1
            ));
        }
        if end + 1 < start {
            return Err(format!(
                "end_line {end} must be start_line - 1 (insert) or >= start_line"
            ));
        }
        let insert = end + 1 == start;
        if !insert && end > total {
            return Err(format!("end_line {end} is out of range 1..{total}"));
        }
        let replacement: Vec<&str> = if text.is_empty() {
            Vec::new()
        } else {
            text.split('\n').collect()
        };

        let (from, to) = if insert {
            (start - 1, start - 1)
        } else {
            (start - 1, end)
        };
        lines.splice(from..to, replacement);
        self.text = lines.join("\n");
        self.version += 1;

        Ok(EditOutcome {
            mode: if insert { EditMode::Insert } else { EditMode::Replace },
            new_line_count: self.line_count(),
        })
    }

    /// The mtime/size pair this document last saw on disk.
    pub fn disk_state(&self) -> (Option<SystemTime>, u64) {
        (self.disk_mtime, self.disk_size)
    }
}

/// What one edit did.
#[derive(Debug, PartialEq)]
pub enum EditMode {
    Insert,
    Replace,
}

#[derive(Debug, PartialEq)]
pub struct EditOutcome {
    pub mode: EditMode,
    pub new_line_count: usize,
}

#[derive(Debug, Default)]
pub struct DocStore {
    documents: HashMap<PathBuf, Document>,
}

impl DocStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a document: read from disk, or reuse the open copy when nothing
    /// changed externally (P0.10).
    pub async fn open(&mut self, path: &Path) -> Result<OpenFacts, std::io::Error> {
        let absolute = path
            .canonicalize()
            .unwrap_or_else(|_| path.to_path_buf());
        let meta = fs::metadata(&absolute).await;

        if meta.is_err() {
            return Ok(OpenFacts {
                exists: false,
                line_count: 0,
                reloaded: false,
                version: 0,
            });
        }
        let meta = meta?;
        let mtime = meta.modified().ok();
        let size = meta.len();

        let reuse = match self.documents.get(&absolute) {
            Some(doc) => doc.disk_mtime == mtime && doc.disk_size == size,
            None => false,
        };
        if reuse {
            let doc = &self.documents[&absolute];
            return Ok(OpenFacts {
                exists: true,
                line_count: doc.line_count(),
                reloaded: false,
                version: doc.version,
            });
        }

        let text = fs::read_to_string(&absolute).await?;
        let version = self.documents.get(&absolute).map(|d| d.version).unwrap_or(0);
        let document = Document {
            path: absolute.clone(),
            text,
            version,
            disk_mtime: mtime,
            disk_size: size,
        };
        let facts = OpenFacts {
            exists: true,
            line_count: document.line_count(),
            reloaded: true,
            version: document.version,
        };
        self.documents.insert(absolute, document);
        Ok(facts)
    }

    /// Read a line range, 1-based, inclusive (P0.11).
    pub fn read(&self, path: &Path, start: Option<usize>, end: Option<usize>) -> Option<ReadSlice> {
        let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let doc = self.documents.get(&absolute)?;
        let lines = doc.lines();
        let total = lines.len();
        let s = start.unwrap_or(1).max(1).min(total.max(1));
        let e = end.unwrap_or(total).min(total).max(s);
        let slice: Vec<String> = lines
            .iter()
            .skip(s - 1)
            .take(e - s + 1)
            .map(|l| l.to_string())
            .collect();
        Some(ReadSlice {
            start_line: s,
            end_line: e,
            total_lines: total,
            truncated: e < total,
            lines: slice,
        })
    }
    /// The text of an open document, if present.
    pub fn text(&self, path: &Path) -> Option<String> {
        let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.documents.get(&absolute).map(|d| d.text.clone())
    }

    /// Get a document for editing. Opening first is the caller's job.
    pub fn document_mut(&mut self, path: &Path) -> Option<&mut Document> {
        let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.documents.get_mut(&absolute)
    }

    /// Record a save: the disk state now matches the buffer.
    pub async fn saved(&mut self, path: &Path) -> Result<(), std::io::Error> {
        let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(doc) = self.documents.get_mut(&absolute) {
            let meta = fs::metadata(&absolute).await?;
            doc.disk_mtime = meta.modified().ok();
            doc.disk_size = meta.len();
        }
        Ok(())
    }
}

/// Facts returned by `open` (P0.10).
#[derive(Debug)]
pub struct OpenFacts {
    pub exists: bool,
    pub line_count: usize,
    pub reloaded: bool,
    pub version: u64,
}

/// A numbered-line read result (P0.11).
#[derive(Debug)]
pub struct ReadSlice {
    pub start_line: usize,
    pub end_line: usize,
    pub total_lines: usize,
    pub truncated: bool,
    pub lines: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        Document {
            path: PathBuf::from("/tmp/x.py"),
            text: text.to_string(),
            version: 0,
            disk_mtime: None,
            disk_size: 0,
        }
    }

    #[test]
    fn edit_lines_replaces_range() {
        let mut d = doc("a\nb\nc\n");
        let outcome = d.edit_lines(2, 2, "B").unwrap();
        assert_eq!(outcome.mode, EditMode::Replace);
        assert_eq!(d.text, "a\nB\nc\n");
        assert_eq!(d.version, 1);
    }

    #[test]
    fn edit_lines_inserts_before_line() {
        // start = end + 1 means insert.
        let mut d = doc("a\nb\n");
        d.edit_lines(2, 1, "X").unwrap();
        assert_eq!(d.text, "a\nX\nb\n");
    }

    #[test]
    fn edit_lines_rejects_bad_range() {
        let mut d = doc("a\n");
        assert!(d.edit_lines(0, 1, "x").is_err());
        assert!(d.edit_lines(5, 5, "x").is_err());
        assert!(d.edit_lines(2, 0, "x").is_err()); // end < start - 1
    }

    #[test]
    fn multi_line_splice() {
        let mut d = doc("one\ntwo\nthree\nfour\n");
        d.edit_lines(2, 3, "2\n3\n2.5").unwrap();
        assert_eq!(d.text, "one\n2\n3\n2.5\nfour\n");
    }
}
