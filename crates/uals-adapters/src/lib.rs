//! Search backends for `uals`. Each adapter implements the traits in `uals_core::backend` for one
//! repository at a time; the orchestrator in the `uals` crate runs them in parallel.
//!
//! Ownership during parallel development: each module belongs to one track; this file and
//! `Cargo.toml` belong to the coordinator.
//! - `ast_index`: track T2.
//! - `lexical`: track T3.
//! - `mcp_child` and `semble`: track T4.
//! - `claude_context`: track T5.

pub mod ast_index;
pub mod claude_context;
pub mod lexical;
pub mod mcp_child;
mod path_filter;
pub mod semble;
