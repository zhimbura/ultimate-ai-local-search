//! `uals`: one code search for AI agents across every repository in a folder.

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use uals::cli::{Cli, run};

/// The variable with the log filter, e.g. `UALS_LOG=debug`. Logs go to stderr: stdout carries the
/// output, and the MCP protocol under `uals mcp`.
const LOG_VAR: &str = "UALS_LOG";

/// How long the process waits for blocking work after the command ends. Under `uals mcp`, SIGTERM
/// can come while a read of stdin is blocked; tokio cannot cancel that read, so without a bound the
/// process would wait for the client's next message.
const SHUTDOWN_BOUND: Duration = Duration::from_millis(500);

fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = EnvFilter::try_from_env(LOG_VAR).unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("uals: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(cli));
    // Drops every task, which stops the backend servers, then waits for blocking work.
    runtime.shutdown_timeout(SHUTDOWN_BOUND);
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("uals: {error:#}");
            ExitCode::FAILURE
        }
    }
}
