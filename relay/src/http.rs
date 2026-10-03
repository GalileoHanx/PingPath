//! Health and metrics over HTTP. Bind it to localhost or a private network:
//! it is for the operator and Prometheus, not for clients.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use serde_json::json;
use tokio::task::JoinHandle;

use crate::relay::Shared;

pub(crate) async fn serve(
    shared: Arc<Shared>,
    addr: SocketAddr,
) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    let app = Router::new()
        .route("/healthz", get(health))
        .route("/metrics", get(metrics))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "health and metrics server stopped");
        }
    });
    Ok((bound, task))
}

async fn health(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    let m = &shared.metrics;
    Json(json!({
        "status": "ok",
        "id": shared.config.id,
        "name": shared.config.name,
        "roles": shared.config.roles.iter().map(|r| format!("{r:?}").to_lowercase()).collect::<Vec<_>>(),
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": shared.uptime().as_secs(),
        "entry_sessions": m.entry_sessions.load(Ordering::Relaxed),
        "exit_sessions": m.exit_sessions.load(Ordering::Relaxed),
        "flows": m.flows.load(Ordering::Relaxed),
        "peers": m.peer_latency().iter().map(|p| json!({"name": p.peer, "rtt_ms": p.rtt_ms, "loss_pct": p.loss_pct})).collect::<Vec<_>>(),
    }))
}

async fn metrics(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        shared.metrics.render(),
    )
}
