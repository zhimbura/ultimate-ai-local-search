//! `uals`: one code search for AI agents across every repository in a folder.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use uals_core::config::Config;
use uals_core::workspace;

#[derive(Parser)]
#[command(
    name = "uals",
    version,
    about = "One code search for AI agents: symbols, text and meaning across every repository in a folder"
)]
struct Cli {
    /// Config file. Default: $UALS_CONFIG, then ~/.config/uals/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the repositories uals searches from a folder, as JSON.
    Workspace {
        /// Start folder. Default: $UALS_WORKSPACE, then the current folder.
        #[arg(env = "UALS_WORKSPACE")]
        path: Option<PathBuf>,
    },
    /// Search code in all repositories (not implemented yet).
    Search { query: String },
    /// List every usage of a symbol (not implemented yet).
    Usages { symbol: String },
    /// Show index state and backend health (not implemented yet).
    Status,
    /// Run the MCP server on stdio (not implemented yet).
    Mcp,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("uals: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let config = Config::load(cli.config.as_deref())?;
    match cli.command {
        Command::Workspace { path } => {
            let start = match path {
                Some(path) => path,
                None => std::env::current_dir().context("cannot read the current folder")?,
            };
            let found = workspace::discover(&start, &config)?;
            println!("{}", serde_json::to_string_pretty(&found)?);
            Ok(())
        }
        Command::Search { .. } | Command::Usages { .. } | Command::Status | Command::Mcp => {
            bail!("this command is not implemented yet")
        }
    }
}
