//! The command line. `search`, `usages` and `status` take the parameters of the MCP tools and
//! print the same text.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use uals_core::config::Config;
use uals_core::render::{render_search, render_usages};
use uals_core::types::{Repo, Workspace};
use uals_core::workspace;

use crate::args::{FormatArg, SearchArgs, StatusArgs, UsagesArgs, repos_in, workspace_folder};
use crate::report::{any_refresh_failed, now_ms, render_refresh, render_status};
use crate::{hook, mcp, setup};

#[derive(Debug, Parser)]
#[command(
    name = "uals",
    version,
    about = "One code search for AI agents: symbols, text and meaning across every repository in a folder"
)]
pub struct Cli {
    /// Config file. Default: $UALS_CONFIG, then ~/.config/uals/config.toml.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Search code in every repository of a folder.
    Search(SearchArgs),
    /// List every usage of a symbol in every repository of a folder.
    Usages(UsagesArgs),
    /// Show the index state of every backend in every repository of a folder.
    Status(StatusArgs),
    /// Bring the index of every backend up to date; `--full` rebuilds them. Exits with 1 when a
    /// backend failed; an unavailable backend is reported but is not a failure.
    Refresh(RefreshArgs),
    /// Print the repositories uals searches from a folder, as JSON.
    Workspace {
        /// Start folder. Default: $UALS_WORKSPACE, then the current folder.
        #[arg(env = "UALS_WORKSPACE")]
        path: Option<PathBuf>,
    },
    /// Run the MCP server on stdin and stdout.
    Mcp,
    /// Hooks for coding agents.
    #[command(subcommand)]
    Hook(HookCommand),
}

/// The parameters of `refresh`.
#[derive(Debug, Args)]
pub struct RefreshArgs {
    /// The folder whose repositories are refreshed. Default: $UALS_WORKSPACE, then the current
    /// folder.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Names of the repositories to refresh. Default: all of them.
    #[arg(long = "repo", value_name = "NAME")]
    pub repos: Vec<String>,
    /// Rebuild the indexes from scratch instead of updating them.
    #[arg(long)]
    pub full: bool,
    /// The output format; `detailed` is the same as `concise`.
    #[arg(long, value_enum, default_value_t)]
    pub format: FormatArg,
}

#[derive(Debug, Subcommand)]
pub enum HookCommand {
    /// Claude Code `PostToolUse` hook for file edits: reads the hook input on stdin and updates the
    /// ast-index index of the repository that holds the edited file.
    PostEdit,
}

/// Runs the command of `cli` and prints its output.
pub async fn run(cli: Cli) -> Result<ExitCode> {
    let config = Config::load(cli.config.as_deref())?;
    match cli.command {
        Command::Search(args) => {
            let (_, repos) = select(args.workspace.as_deref(), &args.repos, &config)?;
            let orchestrator = setup::orchestrator(config);
            let response = orchestrator.search(&repos, &args.params()).await?;
            println!("{}", render_search(&response, args.format.into(), &orchestrator.config().output));
        }
        Command::Usages(args) => {
            let (_, repos) = select(args.workspace.as_deref(), &args.repos, &config)?;
            let orchestrator = setup::orchestrator(config);
            let response = orchestrator.usages(&repos, &args.params()).await?;
            println!("{}", render_usages(&response, args.format.into(), &orchestrator.config().output));
        }
        Command::Status(args) => {
            let (workspace, repos) = select(args.workspace.as_deref(), &args.repos, &config)?;
            let states = setup::orchestrator(config).status(&repos).await;
            println!("{}", render_status(&workspace.root, &repos, &states, args.format.into(), now_ms()));
        }
        Command::Refresh(args) => {
            let (_, repos) = select(args.workspace.as_deref(), &args.repos, &config)?;
            let results = setup::orchestrator(config).refresh(&repos, args.full).await;
            println!("{}", render_refresh(&repos, &results, args.format.into()));
            if any_refresh_failed(&results) {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Workspace { path } => {
            let start = match path {
                Some(path) => path,
                None => std::env::current_dir().context("cannot read the current folder")?,
            };
            println!("{}", serde_json::to_string_pretty(&workspace::discover(&start, &config)?)?);
        }
        Command::Mcp => mcp::serve(config).await?,
        Command::Hook(HookCommand::PostEdit) => hook::post_edit(&config, std::io::stdin().lock()).await?,
    }
    Ok(ExitCode::SUCCESS)
}

/// The workspace of the `--workspace` folder (see `workspace_folder` for the default) and its
/// repositories named in `names`.
fn select(workspace: Option<&Path>, names: &[String], config: &Config) -> Result<(Workspace, Vec<Repo>)> {
    repos_in(&workspace_folder(workspace, None)?, names, config)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
