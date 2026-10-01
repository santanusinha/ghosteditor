//! Atomic writes (P0.14, success criterion S7).
//!
//! Write a temp file, fsync, then rename. A crash mid-write never truncates
//! the target. On filesystems without atomic rename, fall back to a direct
//! write and say so.

use std::path::{Path, PathBuf};

use tokio::fs;
use tokio::io::AsyncWriteExt;

/// The outcome of a write.
#[derive(Debug, PartialEq)]
pub enum WriteOutcome {
    /// Temp file + fsync + rename completed.
    Atomic,
    /// Direct write; the filesystem does not support atomic rename.
    Direct,
}

/// Write `content` to `path` atomically when the filesystem supports it.
///
/// The temp file sits next to the target: a rename must not cross devices.
pub async fn write_atomic(path: &Path, content: &str) -> Result<WriteOutcome, std::io::Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).await?;

    let temp: PathBuf = parent.join(format!(
        ".ghostedit-tmp-{}",
        std::process::id()
    ));

    // Write and fsync the temp file first.
    {
        let mut file = fs::File::create(&temp).await?;
        file.write_all(content.as_bytes()).await?;
        file.flush().await?;
        file.sync_all().await?;
    }

    // Rename is atomic on POSIX. Windows offers ReplaceFile semantics too.
    match fs::rename(&temp, path).await {
        Ok(()) => Ok(WriteOutcome::Atomic),
        Err(error) => {
            // Fall back to a direct write. Say so through the outcome.
            logging_fallback(path, &error).await;
            let mut file = fs::File::create(path).await?;
            file.write_all(content.as_bytes()).await?;
            file.flush().await?;
            file.sync_all().await?;
            let _ = fs::remove_file(&temp).await;
            Ok(WriteOutcome::Direct)
        }
    }
}

/// Note a failed rename on stderr for the user.
async fn logging_fallback(path: &Path, error: &std::io::Error) {
    crate::util::logging::debug(format!(
        "atomic rename failed for {}: {error}; direct write used",
        path.display()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_is_atomic_and_readback_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.py");
        let outcome = write_atomic(&path, "print('hello')\n").await.unwrap();
        assert_eq!(outcome, WriteOutcome::Atomic);
        let text = fs::read_to_string(&path).await.unwrap();
        assert_eq!(text, "print('hello')\n");
        // No temp file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".ghostedit-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[tokio::test]
    async fn overwrite_replaces_old_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.py");
        write_atomic(&path, "one\n").await.unwrap();
        write_atomic(&path, "two\n").await.unwrap();
        let text = fs::read_to_string(&path).await.unwrap();
        assert_eq!(text, "two\n");
    }
}
