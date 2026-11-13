//! The `uals` application: orchestrator, MCP server and CLI commands. Owned by the coordinator
//! (phase 2). Library code lives here so that `uals-bench` can drive the orchestrator in process.

pub mod args;
pub mod cli;
pub mod doctor;
pub mod hook;
pub mod lazy_caller;
pub mod mcp;
pub mod orchestrator;
mod precise;
pub mod report;
pub mod setup;
mod snippets;
