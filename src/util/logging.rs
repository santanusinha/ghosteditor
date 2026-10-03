//! Async file logging (P3.9).
//!
//! The file is the main sink: `$XDG_STATE_HOME/ghostedit/ghostedit.log`
//! by default. One dedicated writer thread owns the file, so callers
//! never block on I/O. stderr carries errors only. stdout is never
//! touched: it carries the MCP protocol.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Log severity. Lower is more severe. The filter keeps
/// `level <= max_level`, so the default `Info` drops `Debug` lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    /// Parse a level name. Case-insensitive.
    pub fn parse(text: &str) -> Option<Level> {
        match text.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
        }
    }
}

static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Rotation cap: 10 MiB. One old file (`<name>.log.1`) is kept.
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// One message to the writer thread.
enum Msg {
    Line(Level, String),
    Flush(mpsc::SyncSender<()>),
}

/// The active sink. `Stderr` is the fallback when the file is unusable.
/// `Clone` lets callers copy the sender out under the lock without
/// holding the guard across a send.
#[derive(Clone)]
enum Sink {
    File { sender: mpsc::Sender<Msg> },
    Off,
    Stderr,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Enable file logging at `path`; `None` disables file logging
/// (`--log-file off`). Call once, early in main. Failures fall back to
/// stderr with one visible line.
pub fn init(path: Option<PathBuf>) {
    let sink = match path {
        None => Sink::Off,
        Some(path) => match open_log_file(&path) {
            Ok(file) => {
                let (sender, receiver) = mpsc::channel::<Msg>();
                let spawn = std::thread::Builder::new()
                    .name("ghostedit-log".into())
                    .spawn(move || writer_loop(receiver, path, file, ROTATE_BYTES));
                match spawn {
                    Ok(_) => Sink::File { sender },
                    Err(error) => {
                        eprintln!("[ghostedit] file logging disabled: {error}");
                        drop(sender); // No reader exists; sends would fail.
                        Sink::Stderr
                    }
                }
            }
            Err(error) => {
                eprintln!("[ghostedit] file logging disabled: {error}");
                Sink::Stderr
            }
        },
    };
    *SINK.lock().expect("logging sink poisoned") = Some(sink);
}

/// The state directory: `$XDG_STATE_HOME/ghostedit`, default
/// `$HOME/.local/state/ghostedit`. A relative or empty `XDG_STATE_HOME`
/// is ignored, per the XDG base directory spec.
pub fn state_dir() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("XDG_STATE_HOME") {
        if !value.is_empty() && Path::new(&value).is_absolute() {
            return Some(PathBuf::from(value).join("ghostedit"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/state/ghostedit"))
}

/// The default log file path inside the state directory.
pub fn default_log_file() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("ghostedit.log"))
}

/// Set the maximum level the sinks keep. Lower levels (more severe)
/// always pass.
pub fn set_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Wait until the writer thread has flushed, bounded by `timeout`.
/// True when the flush was confirmed (or no file sink is active).
pub fn flush(timeout: Duration) -> bool {
    let sender = match &*SINK.lock().expect("logging sink poisoned") {
        Some(Sink::File { sender }) => sender.clone(),
        _ => return true,
    };
    let (reply_tx, reply_rx) = mpsc::sync_channel(0);
    if sender.send(Msg::Flush(reply_tx)).is_err() {
        return false;
    }
    reply_rx.recv_timeout(timeout).is_ok()
}

/// Log at info level: file only.
pub fn info(message: impl AsRef<str>) {
    log(Level::Info, message);
}

/// Log at debug level: file only, kept only when the level is debug.
pub fn debug(message: impl AsRef<str>) {
    log(Level::Debug, message);
}

/// Log at warn level: file only.
pub fn warn(message: impl AsRef<str>) {
    log(Level::Warn, message);
}

/// Log at error level: file plus stderr. Errors must stay visible even
/// when the file sink is off or unavailable.
pub fn error(message: impl AsRef<str>) {
    let text = message.as_ref();
    let sink = SINK.lock().expect("logging sink poisoned").clone();
    match sink {
        Some(Sink::File { .. }) | Some(Sink::Off) | None => {
            // The file cannot show it (or is off); stderr must.
            eprintln!("[ghostedit] {text}");
        }
        Some(Sink::Stderr) => {} // log() already prints to stderr.
    }
    log(Level::Error, message);
}

fn log(level: Level, message: impl AsRef<str>) {
    if (level as u8) > MAX_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    let text = message.as_ref();
    let sink = SINK.lock().expect("logging sink poisoned").clone();
    match sink {
        Some(Sink::File { sender }) => {
            let _ = sender.send(Msg::Line(level, text.to_string()));
        }
        Some(Sink::Off) => {}
        Some(Sink::Stderr) | None => eprintln!("{} {}", stderr_prefix(level), text),
    }
}

fn stderr_prefix(level: Level) -> &'static str {
    match level {
        Level::Debug => "[ghostedit][debug]",
        _ => "[ghostedit]",
    }
}

fn open_log_file(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new().append(true).create(true).open(path)
}

/// The writer thread: own the file, drain the queue, flush per batch,
/// rotate when the file passes the cap. Exits when every sender drops.
fn writer_loop(receiver: mpsc::Receiver<Msg>, path: PathBuf, file: File, cap: u64) {
    // Track the logical size: the on-disk size lags the BufWriter buffer,
    // so a metadata() check per line would be stale.
    let mut written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let mut writer = BufWriter::new(file);
    loop {
        let Ok(first) = receiver.recv() else {
            break;
        };
        let mut batch = vec![first];
        while let Ok(msg) = receiver.try_recv() {
            batch.push(msg);
        }
        let mut reply = None;
        for msg in batch {
            match msg {
                Msg::Line(level, text) => {
                    let line = format!("{} {} {}\n", timestamp(), level.label(), text);
                    if writer.write_all(line.as_bytes()).is_err() {
                        return; // The file is gone; the sink is dead.
                    }
                    written += line.len() as u64;
                }
                Msg::Flush(waiter) => reply = Some(waiter),
            }
            if written > cap {
                if rotate(&path, &mut writer).is_err() {
                    return;
                }
                written = 0;
            }
        }
        if writer.flush().is_err() {
            break;
        }
        if let Some(waiter) = reply {
            let _ = waiter.send(());
        }
    }
}

/// Rename the current file to `<name>.1` (replacing any old one) and
/// reopen a fresh file.
fn rotate(path: &Path, writer: &mut BufWriter<File>) -> std::io::Result<()> {
    writer.flush()?;
    let mut target = path.as_os_str().to_os_string();
    target.push(".1");
    let _ = fs::rename(path, target);
    *writer = BufWriter::new(OpenOptions::new().append(true).create(true).open(path)?);
    Ok(())
}

/// UTC ISO-8601 with milliseconds: 2026-10-03T09:07:03.123Z.
fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's
/// civil_from_days algorithm; no date library needed.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (year + if month <= 2 { 1 } else { 0 }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::Mutex;

    /// Tests inside one binary share the process-wide sink and level,
    /// so they serialize on this mutex.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Replace the process-wide sink with a test file. The caller must
    /// hold TEST_LOCK and call clear_sink() at the end.
    fn init_test_file(path: &Path, cap: u64) {
        let file = open_log_file(path).expect("open test log");
        let (sender, receiver) = mpsc::channel::<Msg>();
        let owned = path.to_path_buf();
        std::thread::Builder::new()
            .name("ghostedit-log-test".into())
            .spawn(move || writer_loop(receiver, owned, file, cap))
            .expect("spawn writer");
        let mut guard = SINK.lock().expect("sink poisoned");
        *guard = Some(Sink::File { sender });
    }

    fn clear_sink() {
        *SINK.lock().expect("sink poisoned") = None;
    }

    #[test]
    fn writes_timestamped_lines_and_flushes() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ghostedit.log");
        init_test_file(&path, ROTATE_BYTES);
        set_level(Level::Debug);
        info("hello file");
        debug("debug line");
        assert!(flush(Duration::from_secs(2)), "flush timed out");
        let mut text = String::new();
        File::open(&path)
            .expect("open")
            .read_to_string(&mut text)
            .expect("read");
        assert!(text.contains("INFO  hello file"), "got: {text}");
        assert!(text.contains("DEBUG debug line"), "got: {text}");
        let first = text.lines().next().expect("one line");
        let stamp = first.split(' ').next().expect("stamp");
        assert!(
            stamp.len() == 24 && stamp.ends_with('Z') && stamp.contains('T'),
            "stamp: {stamp}"
        );
        clear_sink();
    }

    #[test]
    fn level_filter_drops_debug_by_default() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ghostedit.log");
        init_test_file(&path, ROTATE_BYTES);
        set_level(Level::Info);
        debug("dropped");
        info("kept");
        assert!(flush(Duration::from_secs(2)), "flush timed out");
        let mut text = String::new();
        File::open(&path)
            .expect("open")
            .read_to_string(&mut text)
            .expect("read");
        assert!(!text.contains("dropped"), "got: {text}");
        assert!(text.contains("kept"), "got: {text}");
        clear_sink();
    }

    #[test]
    fn rotation_moves_the_file_aside() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ghostedit.log");
        // One line is ~60 bytes; a 1 KiB cap forces a rotation mid-run.
        init_test_file(&path, 1024);
        set_level(Level::Debug);
        for index in 0..100 {
            info(format!("rotation line {index} with some padding text"));
        }
        assert!(flush(Duration::from_secs(2)), "flush timed out");
        assert!(
            path.with_extension("log.1").exists(),
            "rotated file missing"
        );
        assert!(path.exists(), "fresh file missing");
        let mut text = String::new();
        File::open(&path)
            .expect("open")
            .read_to_string(&mut text)
            .expect("read");
        assert!(
            text.contains("rotation line 99"),
            "newest lines must be in the fresh file: {text}"
        );
        clear_sink();
    }

    #[test]
    fn unwritable_path_falls_back() {
        // A path under a regular file cannot become a directory.
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"x").expect("write blocker");
        let path = blocker.join("nested").join("ghostedit.log");
        assert!(open_log_file(&path).is_err(), "open must fail");
    }

    #[test]
    fn civil_date_math_matches_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_729), (2026, 10, 3)); // session date
        assert_eq!(civil_from_days(11_016), (2000, 2, 29)); // leap day
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn level_parse_is_case_insensitive() {
        assert_eq!(Level::parse("DEBUG"), Some(Level::Debug));
        assert_eq!(Level::parse("warn"), Some(Level::Warn));
        assert_eq!(Level::parse("nope"), None);
    }
}
