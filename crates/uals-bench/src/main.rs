//! Command line of `uals-bench`; see `uals_bench::cli`.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    uals_bench::cli::main().await
}
