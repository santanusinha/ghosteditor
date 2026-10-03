//! ghostedit: a headless editor MCP.
//!
//! One process per workspace. File ops plus direct LSP clients.
//! No editor, no plugins. Every tool call runs inside a hard deadline.

mod config;
mod docstore;
mod fsops;
mod lsp;
mod mcp;
mod util;
mod walker;

use std::path::PathBuf;
use std::time::Duration;

use config::Config;
use util::logging;

/// How long the final flush waits for the writer thread.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

fn main() {
    // Stdout carries the MCP protocol only. Logs go to the state file;
    // stderr carries errors only.
    let args = Args::parse();

    // File logging starts before anything else logs. The level and the
    // file are resolved in parse(); see the precedence notes there.
    logging::init(args.log_file.clone());
    logging::set_level(args.level);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("could not start the tokio runtime");

    runtime.block_on(async move {
        let config = match Config::load(args.config.clone(), args.cwd.clone()).await {
            Ok(config) => config,
            Err(error) => {
                // Fatal startup errors show on stderr and land in the file.
                logging::error(format!("config error: {error}"));
                logging::flush(FLUSH_TIMEOUT);
                std::process::exit(2);
            }
        };
        let mut config = config;
        if let Some(minutes) = args.idle_evict_min {
            config.idle_evict_min = minutes;
        }
        let server = mcp::Server::new(config);

        // P0.17: clean shutdown. SIGTERM or Ctrl-C stops the run loop;
        // the loop exits when the signal future resolves instead of stdin.
        let run = server.run();
        tokio::select! {
            _ = run => {}
            _ = tokio::signal::ctrl_c() => {
                logging::info("interrupt received; exiting");
            }
            _ = sigterm() => {
                logging::info("SIGTERM received; exiting");
            }
        }
        // Children die with the process; tokio Child drops kill on exit.
        logging::info("shutdown complete");
    });

    // The writer thread is not a tokio task; drain it before exit.
    logging::flush(FLUSH_TIMEOUT);
    // A blocking registry fetch must not hold the exit open. Wait a
    // moment, then leave; the process ends either way.
    runtime.shutdown_timeout(Duration::from_millis(500));
}

/// Resolves on SIGTERM. On non-unix hosts it never resolves.
#[cfg(unix)]
async fn sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    if let Ok(mut term) = signal(SignalKind::terminate()) {
        term.recv().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(not(unix))]
async fn sigterm() {
    std::future::pending::<()>().await;
}

/// Command line arguments. Flags win over environment variables.
struct Args {
    config: Option<PathBuf>,
    cwd: Option<PathBuf>,
    /// Maximum kept level; more severe levels always pass.
    level: logging::Level,
    idle_evict_min: Option<u64>,
    /// The file sink path; `None` disables file logging.
    log_file: Option<PathBuf>,
}

impl Args {
    fn parse() -> Self {
        let mut config = None;
        let mut cwd = None;
        let mut debug_flag = false;
        let mut ge_debug = false;
        let mut idle_evict_min = None;
        let mut log_file_flag: Option<String> = None;
        let mut log_level_flag: Option<String> = None;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--config" => config = iter.next().map(PathBuf::from),
                "--cwd" => cwd = iter.next().map(PathBuf::from),
                "--debug" => debug_flag = true,
                "--idle-evict-min" => {
                    let value = iter.next();
                    match value.and_then(|v| v.parse().ok()) {
                        Some(minutes) => idle_evict_min = Some(minutes),
                        None => {
                            eprintln!("[ghostedit] --idle-evict-min needs a number of minutes");
                            std::process::exit(2);
                        }
                    }
                }
                "--log-file" => match iter.next() {
                    Some(value) => log_file_flag = Some(value),
                    None => {
                        eprintln!("[ghostedit] --log-file needs a path or 'off'");
                        std::process::exit(2);
                    }
                },
                "--log-level" => match iter.next() {
                    Some(value) => log_level_flag = Some(value),
                    None => {
                        eprintln!("[ghostedit] --log-level needs error|warn|info|debug");
                        std::process::exit(2);
                    }
                },
                "-h" | "--help" => {
                    println!("ghostedit — headless editor MCP. Use --config and --cwd.");
                    std::process::exit(0);
                }
                other => {
                    eprintln!("[ghostedit] unknown argument: {other}");
                    std::process::exit(2);
                }
            }
        }
        if config.is_none() {
            if let Ok(value) = std::env::var("GE_CONFIG") {
                config = Some(PathBuf::from(value));
            }
        }
        if cwd.is_none() {
            if let Ok(value) = std::env::var("GE_CWD") {
                cwd = Some(PathBuf::from(value));
            }
        }
        if idle_evict_min.is_none() {
            if let Ok(value) = std::env::var("GE_IDLE_EVICT_MIN") {
                if let Ok(minutes) = value.parse() {
                    idle_evict_min = Some(minutes);
                }
            }
        }
        let log_level_env = std::env::var("GE_LOG_LEVEL")
            .ok()
            .filter(|value| !value.is_empty());
        if let Ok(value) = std::env::var("GE_DEBUG") {
            ge_debug = value != "0" && !value.is_empty();
        }

        // Level precedence: --log-level > --debug > GE_LOG_LEVEL >
        // GE_DEBUG > info. Each source maps to the debug level or wins
        // outright; the first Some in this chain decides.
        let level_text = log_level_flag
            .or_else(|| debug_flag.then(|| "debug".to_string()))
            .or(log_level_env)
            .or_else(|| ge_debug.then(|| "debug".to_string()))
            .unwrap_or_else(|| "info".to_string());
        let level = match logging::Level::parse(&level_text) {
            Some(level) => level,
            None => {
                eprintln!(
                    "[ghostedit] invalid log level '{level_text}' (use error|warn|info|debug)"
                );
                std::process::exit(2);
            }
        };

        // File precedence: --log-file > GE_LOG_FILE > the state dir
        // default. The value "off" disables the file sink.
        let raw_log_file =
            log_file_flag.or_else(|| std::env::var("GE_LOG_FILE").ok().filter(|v| !v.is_empty()));
        let log_file = match raw_log_file.as_deref() {
            Some("off") => None,
            Some(path) => Some(PathBuf::from(path)),
            None => logging::default_log_file(),
        };

        Self {
            config,
            cwd,
            level,
            idle_evict_min,
            log_file,
        }
    }
}
