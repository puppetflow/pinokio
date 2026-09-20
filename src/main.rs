mod auth;
mod browser_archive;
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
use std::sync::atomic::Ordering;

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

    // The browser is prepared while the server already answers /health, so a
    // first-start browser archive download does not trip liveness checks.
    // Sessions and /ready return 503 until the binary is confirmed.
    tokio::spawn(prepare_browser(Arc::clone(&state)));

    // Graceful shutdown sequence:
    // 1. SIGTERM/SIGINT (or a failed browser preparation) cancels
    //    `shutdown`: new requests get 503 and queued waiters are woken with
    //    503 immediately.
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
            _ = shutdown_state.shutdown.cancelled() => info!("shutdown requested internally"),
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

    if state.startup_failed.load(Ordering::SeqCst) {
        error!("pinokio stopped because the browser could not be prepared");
        return ExitCode::FAILURE;
    }
    info!("pinokio stopped");
    ExitCode::SUCCESS
}

/// Makes the configured browser available, then records its identity so
/// sessions can be admitted. CHROME_PATH may point to any Chromium-based
/// binary mounted into the container (see README); a BROWSER_ARCHIVE_URL is
/// fetched on first start onto the operator's volume.
async fn prepare_browser(state: Arc<server::AppState>) {
    let config = &state.config;
    if let Some(archive) = &config.browser_archive
        && config.browser_engine == config::BrowserEngine::Downloaded
    {
        let installed = browser_archive::ensure_installed(
            &config.browser_archive_root,
            &archive.url,
            archive.sha256.as_deref(),
        )
        .await;
        if let Err(e) = installed {
            error!("browser archive installation failed: {e}");
            state.startup_failed.store(true, Ordering::SeqCst);
            state.shutdown.cancel();
            return;
        }
    }
    if !config.chrome_path.is_file() {
        error!(
            "browser binary {} is missing after preparation",
            config.chrome_path.display()
        );
        state.startup_failed.store(true, Ordering::SeqCst);
        state.shutdown.cancel();
        return;
    }

    let browser = chromium::identify(config).await;
    info!(
        engine = %browser.engine,
        path = %browser.path,
        name = browser.name.as_deref().unwrap_or("unknown"),
        version = browser.version.as_deref().unwrap_or("unknown"),
        sha256 = browser.sha256.as_deref().unwrap_or("unknown"),
        "browser binary"
    );
    // Only this task sets the value, so a failure here cannot happen.
    let _ = state.browser.set(browser);
}
