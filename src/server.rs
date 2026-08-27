use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::Config;
use crate::errors::GatewayError;
use crate::queue::Gate;
use crate::{auth, chromium, proxy, session};

pub struct AppState {
    pub config: Config,
    pub gate: Gate,
    /// Cancelled as soon as SIGTERM/SIGINT is received: rejects new
    /// requests and wakes queued waiters.
    pub shutdown: CancellationToken,
    /// Cancelled after the shutdown grace period: closes active sessions.
    pub session_cancel: CancellationToken,
    /// Tracks session futures so shutdown can wait for their cleanup.
    /// axum runs upgraded WebSocket callbacks in detached tasks, so without
    /// this the runtime could exit while Chromium teardown is in flight.
    pub sessions: TaskTracker,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let gate = Gate::new(config.max_concurrent_sessions, config.max_queue_length);
        Self {
            config,
            gate,
            shutdown: CancellationToken::new(),
            session_cancel: CancellationToken::new(),
            sessions: TaskTracker::new(),
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(ws_handler))
        .route("/chromium", get(ws_handler))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/status", get(status))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn ready(State(state): State<Arc<AppState>>) -> Response {
    if state.shutdown.is_cancelled() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ready": false, "reason": "shutting_down" })),
        )
            .into_response();
    }
    if !state.gate.has_capacity() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ready": false, "reason": "at_capacity" })),
        )
            .into_response();
    }
    Json(serde_json::json!({ "ready": true })).into_response()
}

async fn status(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query_token = params.get("token").map(String::as_str);
    if !auth::is_authorized(state.config.token.as_deref(), query_token, &headers) {
        return GatewayError::Unauthorized.into_response();
    }
    Json(serde_json::json!({
        "active_sessions": state.gate.active_sessions(),
        "max_concurrent_sessions": state.gate.max_sessions(),
        "queued_sessions": state.gate.queued_sessions(),
        "max_queue_length": state.gate.max_queue(),
    }))
    .into_response()
}

/// Main endpoint. All admission steps (auth, queue, Chromium launch, CDP
/// connect) happen before the WebSocket upgrade so failures map to proper
/// HTTP status codes and no application-level messages ever pollute the
/// CDP stream. Puppeteer and Playwright therefore work unmodified.
async fn ws_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, GatewayError> {
    let query_token = params.get("token").map(String::as_str);
    if !auth::is_authorized(state.config.token.as_deref(), query_token, &headers) {
        warn!("unauthorized connection attempt");
        return Err(GatewayError::Unauthorized);
    }
    if state.shutdown.is_cancelled() {
        return Err(GatewayError::ShuttingDown);
    }
    let launch_options = params
        .get("launch")
        .map(|raw| {
            serde_json::from_str::<chromium::LaunchOptions>(raw)
                .map_err(|error| GatewayError::InvalidLaunchOptions(error.to_string()))
        })
        .transpose()?
        .unwrap_or_default()
        .validate()?;

    let session_id = Uuid::new_v4();

    // If a client disconnects while queued, hyper drops this future and the
    // RAII guards inside the gate remove it from the queue accounting.
    info!(%session_id, state = "queued", "session_queued");
    let queued_at = Instant::now();
    let permit = state
        .gate
        .acquire(state.config.queue_timeout, &state.shutdown)
        .await
        .inspect_err(|e| log_admission_failure(&session_id, e))?;
    let queue_time = queued_at.elapsed();

    info!(%session_id, state = "starting", "session_starting");
    let starting_at = Instant::now();
    let mut chromium = chromium::launch(&state.config, &launch_options)
        .await
        .inspect_err(|e| log_admission_failure(&session_id, e))?;
    let startup_time = starting_at.elapsed();
    info!(
        %session_id,
        startup_ms = startup_time.as_millis() as u64,
        "chromium_started"
    );

    // Connect to the local CDP endpoint before upgrading so that a broken
    // Chromium still yields an HTTP error instead of a dangling socket.
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(proxy::MAX_MESSAGE_SIZE))
        .max_frame_size(Some(proxy::MAX_FRAME_SIZE));
    let upstream = match tokio_tungstenite::connect_async_with_config(
        &chromium.ws_url,
        Some(ws_config),
        false,
    )
    .await
    {
        Ok((stream, _)) => stream,
        Err(e) => {
            chromium.shutdown().await;
            let err = GatewayError::ChromiumUnavailable(format!("CDP connect failed: {e}"));
            log_admission_failure(&session_id, &err);
            return Err(err);
        }
    };

    let timings = session::SessionTimings {
        queue_time,
        startup_time,
    };
    let tracker = state.sessions.clone();
    Ok(ws
        .max_message_size(proxy::MAX_MESSAGE_SIZE)
        .max_frame_size(proxy::MAX_FRAME_SIZE)
        .on_upgrade(move |socket| {
            tracker.track_future(session::run(
                socket, upstream, chromium, permit, state, session_id, timings,
            ))
        }))
}

fn log_admission_failure(session_id: &Uuid, error: &GatewayError) {
    match error {
        GatewayError::QueueFull => warn!(%session_id, "queue_full"),
        GatewayError::QueueTimeout => warn!(%session_id, "queue_timeout"),
        GatewayError::ChromiumStartupTimeout => warn!(%session_id, "chromium_startup_timeout"),
        GatewayError::ChromiumUnavailable(reason) => {
            warn!(%session_id, %reason, "chromium_unavailable")
        }
        GatewayError::ShuttingDown => info!(%session_id, "rejected_during_shutdown"),
        GatewayError::InvalidLaunchOptions(_) | GatewayError::Unauthorized => {}
    }
}
