use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

mod trace_viewer;

#[derive(Parser)]
#[command(name = "agora-tools", about = "Local Agora developer tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open an interactive sandbox terminal with a live audit timeline
    TraceViewer(TraceViewerArgs),
}

#[derive(Args)]
struct TraceViewerArgs {
    /// Agora Sandbox configuration file
    #[arg(long)]
    config: PathBuf,

    /// Explicit agora-sandbox binary
    #[arg(long)]
    sandbox_bin: Option<PathBuf>,

    /// Print the URL without opening the default browser
    #[arg(long)]
    no_open: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::TraceViewer(args) => {
            trace_viewer::run(trace_viewer::TraceViewerOptions {
                config: args.config,
                sandbox_bin: args.sandbox_bin,
                open_browser: !args.no_open,
            })
            .await
        }
    }
}
