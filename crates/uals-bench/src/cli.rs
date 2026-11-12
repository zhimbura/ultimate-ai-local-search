//! The command line: `run`, `compare` and `report`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::compare::{Metric, compare};
use crate::engine::AnyEngine;
use crate::gold::{GoldQuery, GoldUsages, Half, QueryType, parse_jsonl};
use crate::markdown;
use crate::report::Report;
use crate::runner::{RunOptions, run};

/// Measures uals search quality and speed against a gold query set.
#[derive(Debug, Parser)]
#[command(name = "uals-bench", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HalfFilter {
    #[value(name = "A")]
    A,
    #[value(name = "B")]
    B,
    #[value(name = "all")]
    All,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a gold set against an engine and write a report.
    Run {
        /// Queries file (JSONL).
        #[arg(long)]
        gold: PathBuf,
        /// Usages file (JSONL); omit to skip the usages measurement.
        #[arg(long)]
        usages: Option<PathBuf>,
        /// `mock:<responses.json>`.
        #[arg(long)]
        engine: String,
        /// Name of the run, shown in reports.
        #[arg(long)]
        name: String,
        /// Where to write the report JSON; stdout when omitted.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Only the queries of this half.
        #[arg(long, value_enum, default_value = "all")]
        half: HalfFilter,
        /// Only queries of these types; repeat the flag for several.
        #[arg(long = "type", value_enum)]
        types: Vec<QueryType>,
        /// How many extra passes over all queries to time as warm calls.
        #[arg(long, default_value_t = 3)]
        warm_repeats: usize,
        /// Hits requested per query.
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Usages requested per symbol.
        #[arg(long, default_value_t = 500)]
        usages_limit: usize,
    },
    /// Compare two reports on the same queries: paired bootstrap of the difference (B - A).
    Compare {
        a: PathBuf,
        b: PathBuf,
        #[arg(long, value_enum, default_value = "mrr")]
        metric: Metric,
        /// Print JSON instead of a Markdown table.
        #[arg(long)]
        json: bool,
    },
    /// Print the tables of one or more reports.
    Report {
        #[arg(required = true)]
        reports: Vec<PathBuf>,
        /// Markdown tables (the default format).
        #[arg(long, conflicts_with = "json")]
        markdown: bool,
        /// The summaries as JSON instead.
        #[arg(long)]
        json: bool,
    },
}

fn read_gold<T>(path: &Path) -> Result<Vec<T>>
where
    T: crate::gold::Record + for<'de> serde::Deserialize<'de>,
{
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    parse_jsonl(&text, &name)
}

pub async fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Run { gold, usages, engine, name, out, half, types, warm_repeats, limit, usages_limit } => {
            let queries: Vec<GoldQuery> = read_gold(&gold)?;
            let usage_gold: Vec<GoldUsages> = usages.as_deref().map(read_gold).transpose()?.unwrap_or_default();
            let backend = AnyEngine::from_spec(&engine)?;
            let options = RunOptions {
                name,
                engine,
                half: match half {
                    HalfFilter::A => Some(Half::A),
                    HalfFilter::B => Some(Half::B),
                    HalfFilter::All => None,
                },
                types,
                warm_repeats,
                limit,
                usages_limit,
            };
            let report = run(&backend, &queries, &usage_gold, &options).await?;
            let overall = &report.summary.overall;
            eprintln!(
                "{}: {} queries, MRR {:.3}, Hit@1 {:.3}, Hit@10 {:.3}, {} failed",
                report.name,
                overall.n,
                overall.primary.mrr,
                overall.primary.hit_at_1,
                overall.primary.hit_at_10,
                overall.failed
            );
            match out {
                Some(path) => report.save(&path)?,
                None => println!("{}", serde_json::to_string_pretty(&report)?),
            }
        }
        Command::Compare { a, b, metric, json } => {
            let comparison = compare(&Report::load(&a)?, &Report::load(&b)?, metric);
            if json {
                println!("{}", serde_json::to_string_pretty(&comparison)?);
            } else {
                print!("{}", comparison.to_markdown());
            }
        }
        Command::Report { reports, json, markdown: _ } => {
            let loaded: Vec<Report> = reports.iter().map(|p| Report::load(p)).collect::<Result<_>>()?;
            if json {
                let summaries: Vec<_> =
                    loaded.iter().map(|r| serde_json::json!({"name": r.name, "summary": r.summary})).collect();
                println!("{}", serde_json::to_string_pretty(&summaries)?);
            } else {
                print!("{}", markdown::render(&loaded));
            }
        }
    }
    Ok(())
}

pub async fn main() -> Result<()> {
    execute(Cli::parse()).await
}
