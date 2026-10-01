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

use config::Config;

fn main() {
    // Logs go to stderr. Stdout carries the MCP protocol only.
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("could not start the tokio runtime");

    runtime.block_on(async move {
        let config = match Config::load(args.config.clone(), args.cwd.clone(), args.debug).await {
            Ok(config) => config,
            Err(error) => {
                eprintln!("[ghostedit] config error: {error}");
                std::process::exit(2);
            }
        };
        let server = mcp::Server::new(config);
        server.run().await;
    });
}

/// Command line arguments. Flags win over environment variables.
struct Args {
    config: Option<PathBuf>,
    cwd: Option<PathBuf>,
    debug: bool,
}

impl Args {
    fn parse() -> Self {
        let mut args = Args {
            config: None,
            cwd: None,
            debug: false,
        };
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--config" => args.config = iter.next().map(PathBuf::from),
                "--cwd" => args.cwd = iter.next().map(PathBuf::from),
                "--debug" => args.debug = true,
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
        if args.config.is_none() {
            if let Ok(value) = std::env::var("GE_CONFIG") {
                args.config = Some(PathBuf::from(value));
            }
        }
        if args.cwd.is_none() {
            if let Ok(value) = std::env::var("GE_CWD") {
                args.cwd = Some(PathBuf::from(value));
            }
        }
        if let Ok(value) = std::env::var("GE_DEBUG") {
            args.debug = value != "0" && !value.is_empty();
        }
        args
    }
}
