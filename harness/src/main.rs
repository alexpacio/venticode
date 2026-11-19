mod agents;
mod config;
mod mcp;
mod oauth;
mod protocol;
mod runtime;

#[cfg(target_os = "linux")]
mod managed;
mod standalone;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "Venticode remote execution runner")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(alias = "standalone")]
    Run,
    #[cfg(target_os = "linux")]
    Managed,
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    Worker,
    #[command(subcommand)]
    Mcp(mcp::Mcp),
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Mcp(command) => mcp::manage(&command).await,
        Command::Run => standalone::run().await,
        #[cfg(target_os = "linux")]
        Command::Managed => managed::run().await,
        #[cfg(target_os = "linux")]
        Command::Worker => managed::worker().await,
    }
}
