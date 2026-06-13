use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crw_browse_camofox::camofox::CamofoxClient;
use crw_browse_camofox::server::CamofoxBrowse;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "crw-browse-camofox",
    about = "MCP server for interactive browser automation over camofox-browser (Camoufox/Firefox)"
)]
struct Cli {
    /// camofox-browser REST base URL.
    #[arg(
        long,
        env = "CRW_CAMOFOX_BASE_URL",
        default_value = "http://localhost:9377"
    )]
    base_url: String,

    /// Optional bearer API key (when camofox runs with auth enabled).
    #[arg(long, env = "CRW_CAMOFOX_API_KEY")]
    api_key: Option<String>,

    /// Per-request timeout to camofox in milliseconds.
    #[arg(long, env = "CRW_CAMOFOX_TIMEOUT_MS", default_value_t = 60_000)]
    timeout_ms: u64,

    /// Default `navigate` readiness wait in milliseconds.
    #[arg(long, env = "CRW_CAMOFOX_WAIT_MS", default_value_t = 15_000)]
    wait_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    let client = Arc::new(CamofoxClient::new(
        cli.base_url.clone(),
        cli.api_key,
        Duration::from_millis(cli.timeout_ms),
    ));

    tracing::info!(base_url = %cli.base_url, "starting crw-browse-camofox");

    let service = CamofoxBrowse::new(client, cli.wait_ms)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serve error: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}
