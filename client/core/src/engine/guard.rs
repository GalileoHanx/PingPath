//! The dead-man switch (docs/decisions/007): a plain OS thread, outside
//! the async runtime, that removes the game routes if the engine's data
//! loop stops beating while routes are in place. A stuck service (a
//! deadlock, a starved runtime, a stalled disk) would otherwise leave game
//! traffic routed into an adapter nobody reads.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::net::{Platform, Route};

const CHECK_EVERY: Duration = Duration::from_millis(100);

pub struct Guard {
    state: Arc<State>,
}

struct State {
    epoch: Instant,
    /// Milliseconds since `epoch` of the data loop's last beat.
    heartbeat: AtomicU64,
    routes: Mutex<Vec<Route>>,
    fired: AtomicBool,
    timeout: Duration,
    stop: AtomicBool,
}

impl Guard {
    /// Starts the guard thread; it removes armed routes through `platform`
    /// when no beat came for `timeout`.
    pub fn start(platform: Arc<dyn Platform>, timeout: Duration) -> Self {
        let state = Arc::new(State {
            epoch: Instant::now(),
            heartbeat: AtomicU64::new(0),
            routes: Mutex::new(Vec::new()),
            fired: AtomicBool::new(false),
            timeout,
            stop: AtomicBool::new(false),
        });
        let watched = state.clone();
        let spawned = thread::Builder::new()
            .name("pingpath-guard".into())
            .spawn(move || {
                while !watched.stop.load(Ordering::Relaxed) {
                    thread::sleep(CHECK_EVERY);
                    let now = watched.epoch.elapsed().as_millis() as u64;
                    let silent = now.saturating_sub(watched.heartbeat.load(Ordering::Relaxed));
                    if silent < watched.timeout.as_millis() as u64 {
                        continue;
                    }
                    let routes = std::mem::take(&mut *watched.routes.lock().expect("guard lock"));
                    if routes.is_empty() {
                        continue;
                    }
                    for route in &routes {
                        if let Err(e) = platform.remove_route(route) {
                            tracing::error!(error = %e, "guard could not remove a game route");
                        }
                    }
                    watched.fired.store(true, Ordering::Relaxed);
                    tracing::error!(
                        silent_ms = silent,
                        routes = routes.len(),
                        "the engine stopped responding: the guard removed the game routes, so the game connects directly"
                    );
                }
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "cannot start the guard thread");
        }
        Self { state }
    }

    pub fn beat(&self) {
        let now = self.state.epoch.elapsed().as_millis() as u64;
        self.state.heartbeat.store(now, Ordering::Relaxed);
    }

    /// Watches `routes` from now on (beats first, so arming isn't late).
    pub fn arm(&self, routes: Vec<Route>) {
        self.beat();
        self.state.fired.store(false, Ordering::Relaxed);
        *self.state.routes.lock().expect("guard lock") = routes;
    }

    /// Stops watching (the routes are being removed normally).
    pub fn disarm(&self) {
        self.state.routes.lock().expect("guard lock").clear();
    }

    /// Whether the guard removed the routes since the last `arm`.
    pub fn fired(&self) -> bool {
        self.state.fired.load(Ordering::Relaxed)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::AdapterConfig;
    use crate::net::fake::FakeNetwork;

    #[tokio::test]
    async fn removes_routes_when_the_beat_stops() {
        let net = FakeNetwork::new();
        let (adapter, _channels) = net
            .create_adapter(&AdapterConfig {
                address: "192.0.0.8".parse().unwrap(),
                mtu: 1380,
            })
            .unwrap();
        let route = Route::Adapter {
            prefix: "198.51.100.0/24".parse().unwrap(),
            interface: adapter.interface(),
        };
        net.add_route(&route).unwrap();
        let guard = Guard::start(Arc::new(net.clone()), Duration::from_millis(300));
        guard.arm(vec![route]);
        for _ in 0..5 {
            thread::sleep(Duration::from_millis(100));
            guard.beat();
        }
        assert_eq!(net.routes(), vec![route], "kept while beating");
        thread::sleep(Duration::from_millis(600));
        assert!(net.routes().is_empty());
        assert!(guard.fired());
    }
}
