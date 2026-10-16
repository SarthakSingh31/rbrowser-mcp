use std::path::PathBuf;

use clap::Parser;
use rmcp::ServiceExt;

mod browser;
mod error;
mod reaper;
mod server;
mod snapshot;

use browser::Mode;
use error::Error;
use server::{BrowserMcp, Config};

#[derive(Debug, Parser)]
#[command(version, about = "An MCP server that lets an agent drive Chrome")]
struct Args {
    /// Open new pages in a visible window instead of headless
    #[arg(long)]
    headed: bool,
    /// Number of open pages above which open_page warns the agent to close some
    #[arg(long, default_value_t = 5)]
    max_pages: usize,
    /// Path to the Chrome/Chromium executable (auto-detected by default)
    #[arg(long)]
    chrome_path: Option<PathBuf>,
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut term = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        let mut hangup = signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hangup.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    reaper::exec_chrome_if_wrapper();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    tracing::info!("Starting MCP server!");

    // The browser is launched lazily on the first open_page.
    let mcp = BrowserMcp::new(Config {
        default_mode: if args.headed {
            Mode::Visible
        } else {
            Mode::Headless
        },
        max_pages: args.max_pages,
        chrome_path: args.chrome_path,
    });

    let service = mcp.clone().serve(rmcp::transport::stdio()).await?;

    tracing::info!("MCP server started!");

    let result = tokio::select! {
        r = service.waiting() => r.map(|_| ()).map_err(anyhow::Error::from),
        _ = shutdown_signal() => {
            tracing::info!("Received shutdown signal");
            Ok(())
        }
    };

    mcp.shutdown().await;

    // Exit explicitly: after a signal, tokio's stdin reader is still blocked in a read and would
    // keep the runtime from shutting down.
    if let Err(e) = result {
        tracing::error!("{e:?}");
        std::process::exit(1);
    }
    std::process::exit(0);
}
