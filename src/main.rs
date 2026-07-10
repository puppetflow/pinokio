mod auth;
mod chromium;
mod config;
mod errors;
mod proxy;
mod queue;
mod server;
mod session;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    let level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
    let filter = EnvFilter::try_new(&level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    let config = match config::Config::from_env() {
        Ok(config) => config,
        Err(e) => {
            error!("invalid configuration: {e}");
            return ExitCode::FAILURE;
        }
    };

    let addr = SocketAddr::new(config.host, config.port);
    let grace = config.shutdown_grace_period;
    let state = Arc::new(server::AppState::new(config));
    let app = server::router(Arc::clone(&state));

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            error!("failed to bind {addr}: {e}");
            return ExitCode::FAILURE;
        }
    };

    info!(
        %addr,
        max_concurrent_sessions = state.gate.max_sessions(),
        max_queue_length = state.gate.max_queue(),
        auth = state.config.token.is_some(),
        "pinokio listening"
    );

    // Graceful shutdown sequence:
    // 1. SIGTERM/SIGINT cancels `shutdown`: new requests get 503 and queued
    //    waiters are woken with 503 immediately.
    // 2. Active sessions get `grace` to finish on their own.
    // 3. `session_cancel` then closes remaining proxies, which terminates
    //    their Chromium processes and removes their temp dirs.
    // 4. axum's graceful shutdown waits for all connections to drain.
    let shutdown_state = Arc::clone(&state);
    let shutdown_signal = async move {
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                error!("failed to install SIGTERM handler: {e}");
                return;
            }
        };
        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM"),
            _ = tokio::signal::ctrl_c() => info!("received SIGINT"),
        }
        shutdown_state.shutdown.cancel();
        info!(
            grace_ms = grace.as_millis() as u64,
            "shutting down, waiting for active sessions"
        );
        // One-shot supervised by design: it only sleeps then cancels.
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            shutdown_state.session_cancel.cancel();
        });
    };

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await
    {
        error!("server error: {e}");
        return ExitCode::FAILURE;
    }

    // Upgraded WebSocket sessions run in their own tasks, outside of the
    // connections axum waits for. Drain them so every Chromium process and
    // temp dir is cleaned up before exiting. Sessions are cancelled at the
    // end of the grace period, the extra margin covers process teardown.
    state.sessions.close();
    let drain_budget = grace + std::time::Duration::from_secs(10);
    if tokio::time::timeout(drain_budget, state.sessions.wait())
        .await
        .is_err()
    {
        error!("some sessions did not finish cleanup before exit");
        return ExitCode::FAILURE;
    }

    info!("pinokio stopped");
    ExitCode::SUCCESS
}
