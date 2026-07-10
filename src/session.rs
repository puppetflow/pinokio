use std::time::{Duration, Instant};

use axum::extract::ws::WebSocket;
use tokio::sync::OwnedSemaphorePermit;
use tracing::{info, warn};
use uuid::Uuid;

use crate::chromium::Chromium;
use crate::proxy;
use crate::server::AppState;

/// Lifecycle: queued -> starting -> active -> closing -> closed | failed.
/// The queued/starting/failed phases happen before the WebSocket upgrade
/// (in the HTTP handler); this module covers active -> closing -> closed.
///
/// The guard below guarantees the Chromium process and its temp dir are
/// cleaned up exactly once, even if the session task panics: the happy path
/// takes the process out of the guard, and `Drop` covers the unwind path.
struct ChromiumGuard(Option<Chromium>);

impl Drop for ChromiumGuard {
    fn drop(&mut self) {
        if let Some(mut chromium) = self.0.take() {
            // Only reached on panic or cancellation of the session task.
            // Spawning here is a last-resort supervision escape hatch: the
            // cleanup must run and Drop cannot await.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move { chromium.shutdown().await });
            }
        }
    }
}

/// Timing data gathered before the upgrade, for the session summary log.
pub struct SessionTimings {
    pub queue_time: Duration,
    pub startup_time: Duration,
}

/// Runs an active session: proxies frames until either side closes, a
/// timeout fires or the server shuts down, then tears everything down.
pub async fn run(
    client: WebSocket,
    upstream: proxy::Upstream,
    chromium: Chromium,
    permit: OwnedSemaphorePermit,
    state: std::sync::Arc<AppState>,
    session_id: Uuid,
    timings: SessionTimings,
) {
    let started_at = Instant::now();
    info!(%session_id, state = "active", "session_active");

    let mut guard = ChromiumGuard(Some(chromium));

    let reason = proxy::run(
        client,
        upstream,
        state.config.connection_timeout,
        &state.session_cancel,
    )
    .await;

    info!(%session_id, state = "closing", reason = reason.as_str(), "session_closing");

    if let Some(mut chromium) = guard.0.take() {
        chromium.shutdown().await;
    }

    if reason == proxy::CloseReason::SessionTimeout {
        warn!(%session_id, "session_timeout");
    }
    if reason == proxy::CloseReason::ChromiumError {
        warn!(%session_id, "chromium_crashed");
    }

    info!(
        %session_id,
        state = "closed",
        reason = reason.as_str(),
        duration_ms = started_at.elapsed().as_millis() as u64,
        queue_time_ms = timings.queue_time.as_millis() as u64,
        chromium_startup_ms = timings.startup_time.as_millis() as u64,
        "session_closed"
    );

    // Releases the concurrency slot exactly once; the next queued request
    // (if any) is woken by the semaphore in FIFO order.
    drop(permit);
}
