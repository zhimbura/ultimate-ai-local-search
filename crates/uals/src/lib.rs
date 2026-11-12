//! The `uals` application: orchestrator, MCP server and CLI commands. Owned by the coordinator
//! (phase 2). Library code lives here so that `uals-bench` can drive the orchestrator in process.

pub mod lazy_caller;
pub mod orchestrator;
pub mod setup;
mod snippets;
