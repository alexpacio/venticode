mod agents;
mod config;
mod mcp;
mod oauth;
mod protocol;
mod runtime;

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
    #[command(subcommand)]
    Mcp(mcp::Mcp),
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Mcp(command) => mcp::manage(&command).await,
        Command::Run => standalone::run().await,
    }
}
