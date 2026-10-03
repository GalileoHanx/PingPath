//! PingPath control plane: issues the session tokens relays check (PLAN.md
//! rule 10), lists relays for clients, and collects relay-to-relay latency.
//! Crowd data and path recommendations arrive with PLAN.md Phase 6.

pub mod api;
pub mod config;
pub mod metrics;
pub mod state;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use pingpath_games::GameDefinition;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::state::ControlState;

/// When the service started, for uptime.
pub type Started = Instant;

pub struct ControlOptions {
    pub config: Config,
    pub games: Vec<GameDefinition>,
    /// The Ed25519 seed that signs session tokens.
    pub issuer_seed: [u8; 32],
}

/// A running control plane.
pub struct Control {
    state: Arc<ControlState>,
    api_addr: SocketAddr,
    admin_addr: Option<SocketAddr>,
    tasks: Vec<JoinHandle<()>>,
}

impl Control {
    pub async fn start(options: ControlOptions) -> Result<Self, String> {
        let listen = options.config.listen;
        let admin_listen = options.config.admin_listen;
        let state = Arc::new(ControlState::new(
            options.config,
            options.games,
            &options.issuer_seed,
        )?);
        let started = Instant::now();

        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| format!("cannot listen on {listen}: {e}"))?;
        let api_addr = listener.local_addr().map_err(|e| e.to_string())?;
        let app =
            api::router(state.clone(), started).into_make_service_with_connect_info::<SocketAddr>();
        let mut tasks = vec![tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "API server stopped");
            }
        })];

        let admin_addr = match admin_listen {
            Some(addr) => {
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .map_err(|e| format!("cannot listen on {addr}: {e}"))?;
                let bound = listener.local_addr().map_err(|e| e.to_string())?;
                let app = api::admin_router(state.clone(), started);
                tasks.push(tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, app).await {
                        tracing::error!(error = %e, "admin server stopped");
                    }
                }));
                Some(bound)
            }
            None => None,
        };
        tracing::info!(
            api = %api_addr,
            admin = ?admin_addr,
            relays = state.relays().len(),
            issuer_public_key = %pingpath_tunnel::keys::encode(&state.issuer_public_key()),
            "control plane started"
        );
        Ok(Self {
            state,
            api_addr,
            admin_addr,
            tasks,
        })
    }

    pub fn api_addr(&self) -> SocketAddr {
        self.api_addr
    }

    pub fn admin_addr(&self) -> Option<SocketAddr> {
        self.admin_addr
    }

    pub fn state(&self) -> &Arc<ControlState> {
        &self.state
    }

    pub fn shutdown(self) {
        tracing::info!("control plane stopping");
        for task in self.tasks {
            task.abort();
        }
    }
}
