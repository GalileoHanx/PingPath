//! The HTTP API (docs/decisions/006-control-plane.md):
//!
//! | Method and path            | Who      | What                                   |
//! |----------------------------|----------|----------------------------------------|
//! | `POST /v1/sessions`        | clients  | a session token for a game and region  |
//! | `POST /v1/sessions/renew`  | clients  | a fresh token for the same session     |
//! | `GET /v1/relays`           | clients  | every relay, to measure paths          |
//! | `GET /v1/latency`          | clients  | relay-to-relay latency                 |
//! | `POST /v1/relay-latency`   | relays   | latency measurements (API key)         |
//! | `GET /healthz`             | anyone   | liveness                               |
//!
//! Health details and `/metrics` are on the separate admin address.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use pingpath_tunnel::token::now_unix;
use serde_json::json;

use crate::Started;
use crate::state::{
    CloseRequest, ControlState, LatencyReport, Refusal, RenewRequest, SessionRequest,
};

/// Requests are small JSON objects; anything bigger is refused unread.
const MAX_BODY: usize = 16 * 1024;

type AppState = (Arc<ControlState>, Started);

pub fn router(state: Arc<ControlState>, started: Started) -> Router {
    Router::new()
        .route(
            "/healthz",
            get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .route("/v1/relays", get(relays))
        .route("/v1/latency", get(latency))
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/renew", post(renew_session))
        .route("/v1/sessions/close", post(close_session))
        .route("/v1/relay-latency", post(relay_latency))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state((state, started))
}

pub fn admin_router(state: Arc<ControlState>, started: Started) -> Router {
    Router::new()
        .route("/healthz", get(admin_health))
        .route("/metrics", get(metrics))
        .with_state((state, started))
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (
            status,
            Json(json!({ "error": self.code, "message": self.message })),
        )
            .into_response();
        if let Some(after) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(after));
        }
        response
    }
}

fn invalid(state: &ControlState, rejection: JsonRejection) -> Refusal {
    state.metrics.refused("invalid_request");
    Refusal::new(
        rejection.status().as_u16(),
        "invalid_request",
        rejection.body_text(),
    )
}

/// The client's address: the socket's peer, or the configured proxy
/// header's last entry. Used only to count sessions per network.
fn client_ip(state: &ControlState, headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    state
        .config
        .client_ip_header
        .as_deref()
        .and_then(|name| headers.get(name))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .and_then(|last| last.trim().parse().ok())
        .unwrap_or(peer.ip())
}

async fn create_session(
    State((state, _)): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<SessionRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return invalid(&state, rejection).into_response(),
    };
    let from = client_ip(&state, &headers, peer);
    match state.issue(&request, from, now_unix()) {
        Ok(grant) => (StatusCode::CREATED, Json(grant)).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn renew_session(
    State((state, _)): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<RenewRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return invalid(&state, rejection).into_response(),
    };
    let from = client_ip(&state, &headers, peer);
    match state.renew(&request, from, now_unix()) {
        Ok(grant) => Json(grant).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn close_session(
    State((state, _)): State<AppState>,
    body: Result<Json<CloseRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return invalid(&state, rejection).into_response(),
    };
    match state.close(&request) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn relay_latency(
    State((state, _)): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<LatencyReport>, JsonRejection>,
) -> Response {
    let Some(key) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        state.metrics.refused("unauthorized");
        return Refusal::new(
            401,
            "unauthorized",
            "send the relay's API key as a bearer token",
        )
        .into_response();
    };
    let report = match body {
        Ok(Json(report)) => report,
        Err(rejection) => return invalid(&state, rejection).into_response(),
    };
    match state.report_latency(key, &report, now_unix()) {
        Ok(ignored) => Json(json!({ "ignored": ignored })).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn relays(State((state, _)): State<AppState>) -> impl IntoResponse {
    Json(json!({ "relays": state.relays() }))
}

async fn latency(State((state, _)): State<AppState>) -> impl IntoResponse {
    let now = now_unix();
    Json(json!({ "latency": state.latency(now), "regions": state.region_latency(now) }))
}

async fn admin_health(State((state, started)): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": started.elapsed().as_secs(),
        "active_sessions": state.active_sessions(),
        "relays": state.relays().len(),
    }))
}

async fn metrics(State((state, started)): State<AppState>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state
            .metrics
            .render(state.active_sessions(), started.elapsed().as_secs()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proxy_header_is_used_only_when_configured() {
        let seed = [1u8; 32];
        let mut config: crate::config::Config = toml::from_str(
            "issuer_seed_file = \"s\"\n[[relays]]\nid = 1\nname = \"a\"\naddress = \"192.0.2.1:51820\"\nroles = [\"entry\"]\n",
        )
        .unwrap();
        let peer: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.1, 203.0.113.5"),
        );

        let direct = ControlState::new(config.clone(), vec![], &seed).unwrap();
        assert_eq!(client_ip(&direct, &headers, peer), peer.ip());

        config.client_ip_header = Some("X-Forwarded-For".into());
        let proxied = ControlState::new(config, vec![], &seed).unwrap();
        assert_eq!(
            client_ip(&proxied, &headers, peer),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
        assert_eq!(client_ip(&proxied, &HeaderMap::new(), peer), peer.ip());
    }
}
