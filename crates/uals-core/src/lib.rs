//! Core contract of `uals`: data types, backend traits, workspace discovery,
//! configuration, and the pure search pipeline (classify → fuse → merge → render).
//!
//! Ownership during parallel development:
//! - `types`, `backend`, `workspace` and `config` are frozen: only the coordinator changes them.
//!   A track that needs a change asks the coordinator instead of editing them.
//! - `classify`, `fuse`, `merge` and `render` are implemented by track T6 against the
//!   golden cases in `tests/golden/`. Their signatures and documented rules are the contract.

pub mod backend;
pub mod classify;
pub mod config;
pub mod fuse;
pub mod merge;
pub mod render;
pub mod types;
pub mod workspace;
