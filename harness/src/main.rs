mod agents;
mod config;
mod foreign;
mod mcp;
mod oauth;
mod protocol;
mod runtime;

#[cfg(target_os = "linux")]
mod managed;
mod standalone;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rich_rust::prelude::*;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    version,
    about = "Venticode remote execution runner",
    long_about = "Venticode remote execution runner.\n\nStandalone mode environment variables:\n  VENTICODE_SERVER_URL         Service origin (https://... or loopback http://...)\n  VENTICODE_HARNESS_TOKEN      Shared runner secret (>= 32 chars)\n  VENTICODE_HARNESS_ROOTS      JSON object of root labels to absolute paths\n  VENTICODE_HARNESS_MCP_GRANTS Optional JSON object of approved MCP server IDs to revisions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to a standalone Venticode service over WebSocket
    #[command(alias = "standalone")]
    Run,
    /// Run inside a managed Kubernetes computer pod
    #[cfg(target_os = "linux")]
    Managed,
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    Worker,
    /// Configure and test local MCP servers
    #[command(subcommand)]
    Mcp(mcp::Mcp),
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("venticode_harness=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    #[cfg(target_os = "linux")]
    let is_worker = matches!(cli.command, Command::Worker);
    #[cfg(not(target_os = "linux"))]
    let is_worker = false;
    let use_rich = !matches!(cli.command, Command::Mcp(_)) && !is_worker;
    if !matches!(cli.command, Command::Mcp(_)) && !is_worker {
        init_logging();
    }
    let result: Result<()> = match cli.command {
        Command::Mcp(command) => mcp::manage(&command).await,
        Command::Run => standalone::run().await,
        #[cfg(target_os = "linux")]
        Command::Managed => managed::run().await,
        #[cfg(target_os = "linux")]
        Command::Worker => managed::worker().await,
    };
    if let Err(error) = result {
        if use_rich {
            let console = Console::new();
            let text = format!("{error:#}");
            let panel = Panel::from_text(&text)
                .title("venticode-harness error")
                .border_style(Style::parse("bold red").unwrap_or_default());
            console.print_renderable(&panel);
        } else {
            eprintln!("Error: {error:#}");
        }
        std::process::exit(1);
    }
}
