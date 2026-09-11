use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use anyhow::Context;
use clap::Parser;
use gitea_api::{GiteaClient, TokenLifecycleClient};
use gitea_server::{config::Settings, server};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(name = "mcp-gitea-rs", version)]
struct Cli {
    #[arg(long)]
    healthcheck: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let settings = match Settings::from_env() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("configuration error: {error}");
            return ExitCode::from(2);
        }
    };
    let filter = EnvFilter::try_new(&settings.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to initialize runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli, settings)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "server failed");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli, settings: Settings) -> anyhow::Result<()> {
    if cli.healthcheck {
        return server::healthcheck(&settings.host, settings.port).await;
    }
    let client = Arc::new(GiteaClient::new(
        &settings.upstream_url,
        &settings.service_token,
        settings.timeout,
    )?);
    let token_client = Arc::new(TokenLifecycleClient::new(
        &settings.upstream_url,
        &settings.token_username,
        &settings.token_password,
        settings.timeout,
    )?);
    let files = settings
        .file_public_origin
        .as_deref()
        .map(|origin| {
            gitea_mcp::files::FilePlane::new(origin, std::time::Duration::from_mins(1))
                .map_err(anyhow::Error::msg)
        })
        .transpose()?;
    let cancellation = CancellationToken::new();
    let app = server::router(&settings, client, token_client, files, &cancellation);
    let address: SocketAddr = format!("{}:{}", settings.host, settings.port)
        .parse()
        .context("invalid bind address")?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            cancellation.cancel();
        })
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = ctrl_c => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = ctrl_c.await;
}
