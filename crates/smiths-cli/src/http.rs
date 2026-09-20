//! `/health` + `/metrics` HTTP endpoint.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context as _;
use axum::{Json, Router, routing::get};
use prometheus_client::registry::Registry;
use smiths_core::{Drain, Metrics};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::replication_service::ReplicationState;

/// Snapshot of startup state + live handles that `/health` reports
/// on. Cloneable; the handler holds it behind axum's `State`.
#[derive(Clone)]
pub(crate) struct HealthState {
    pub started_at: u64,
    pub sip_binds: Vec<String>,
    pub plugins_loaded: Vec<String>,
    pub plugins_failed: Vec<(String, String)>,
    pub metrics: Arc<Metrics>,
    pub drain: Drain,
    pub replication: Option<Arc<ReplicationState>>,
}

/// Router state: the Prometheus registry for `/metrics` and the
/// rich [`HealthState`] for `/health`.
#[derive(Clone)]
struct HttpState {
    registry: Arc<Mutex<Registry>>,
    health: HealthState,
}

pub(crate) async fn serve_health(
    bind: SocketAddr,
    cancel: CancellationToken,
    registry: Arc<Mutex<Registry>>,
    health: HealthState,
) -> anyhow::Result<()> {
    let state = HttpState { registry, health };
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(state);
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding health endpoint on {bind}"))?;
    info!(%bind, "health + metrics endpoint listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .context("health server")?;
    info!("health endpoint stopped");
    Ok(())
}

/// Detailed `/health` payload. Fields are stable — k8s liveness and
/// load-balancer probes rely on this shape.
async fn health_handler(
    axum::extract::State(state): axum::extract::State<HttpState>,
) -> Json<serde_json::Value> {
    let hs = &state.health;
    let uptime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .saturating_sub(hs.started_at);
    let status = if hs.drain.is_draining() {
        "draining"
    } else {
        "ok"
    };
    let failed: Vec<serde_json::Value> = hs
        .plugins_failed
        .iter()
        .map(|(dir, err)| serde_json::json!({ "dir": dir, "error": err }))
        .collect();
    let cluster = hs
        .replication
        .as_ref()
        .map_or(serde_json::json!({ "status": "standalone" }), |r| {
            r.status_json()
        });
    Json(serde_json::json!({
        "status": status,
        "draining": hs.drain.is_draining(),
        "uptime_secs": uptime,
        "sip": { "binds": hs.sip_binds },
        "plugins": {
            "loaded": hs.plugins_loaded,
            "failed": failed,
        },
        "dialogs_active": hs.metrics.dialogs_active.get(),
        "bridges_active": hs.metrics.bridges_active.get(),
        "cluster": cluster,
    }))
}

async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<HttpState>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;
    let mut out = String::new();
    let guard = state.registry.lock().await;
    if let Err(e) = prometheus_client::encoding::text::encode(&mut out, &guard) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("encode: {e}")).into_response();
    }
    (
        [(
            CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        out,
    )
        .into_response()
}
