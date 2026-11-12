//! `uals-bench`: Hit@k, MRR, latency and output size of uals search against a gold query set
//! (track T8). The repository holds only the public fixture gold set; private gold sets stay
//! outside it.
//!
//! - [`gold`]: gold set files and their parsing errors.
//! - [`engine`]: the `Engine` seam and the `MockEngine` that replays canned responses.
//! - [`real`]: the `RealEngine` that runs the uals orchestrator in process.
//! - [`metrics`]: hit correctness, ranks, Hit@k, MRR, usage recall and precision, percentiles.
//! - [`runner`] and [`report`]: running a gold set and the report it produces.
//! - [`stats`] and [`compare`]: paired bootstrap comparison of two runs.
//! - [`markdown`]: report tables.
//! - [`cli`]: the command line.

pub mod cli;
pub mod compare;
pub mod engine;
pub mod gold;
pub mod markdown;
pub mod metrics;
pub mod real;
pub mod report;
pub mod runner;
pub mod stats;
