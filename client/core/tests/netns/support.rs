//! Runs async code (relays, servers) inside another named network
//! namespace from the same test process.
//!
//! setns(2) with CLONE_NEWNET switches only the *calling thread*, and a
//! socket belongs to the namespace of the thread that created it, for
//! life. So: a dedicated OS thread joins the namespace first, then builds
//! its own tokio runtime there. Never create sockets for that namespace
//! from the main runtime.

use std::fs::File;
use std::future::Future;
use std::io;
use std::thread::JoinHandle;

use nix::sched::{CloneFlags, setns};
use tokio::sync::oneshot;

/// A future running in another namespace on its own thread and runtime.
/// Dropping it ends it: the runtime is dropped, which aborts its
/// tasks and closes their sockets (an abrupt "crash").
pub struct InNetns {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl InNetns {
    /// Runs `make()` inside `/run/netns/<ns>` (created by `ip netns add`).
    /// `make` is called on the new thread *after* setns, so sockets it binds
    /// live in `ns`. Returns once the future has been started, or the
    /// setns/runtime error. Needs CAP_SYS_ADMIN (root).
    pub fn spawn<F, Fut>(ns: &str, make: F) -> io::Result<Self>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let netns = File::open(format!("/run/netns/{ns}"))?;
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<io::Result<()>>();
        let thread = std::thread::Builder::new()
            .name(format!("netns-{ns}"))
            .spawn(move || {
                if let Err(e) = setns(&netns, CloneFlags::CLONE_NEWNET) {
                    let _ = ready_tx.send(Err(io::Error::from(e)));
                    return;
                }
                drop(netns);
                // current_thread: no worker threads to worry about. (Threads
                // spawned later from this thread, e.g. spawn_blocking, inherit
                // its namespace anyway.)
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    tokio::task::spawn_local(make());
                    let _ = ready_tx.send(Ok(()));
                    let _ = stop_rx.await;
                });
                // Dropping `local` and `rt` here aborts every task.
            })?;
        ready_rx
            .recv()
            .map_err(|_| io::Error::other("netns thread exited early"))??;
        Ok(Self {
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }

    fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for InNetns {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `ip -n <ns> <args>` from the test (e.g. to kill a relay's address).
pub fn ip_in(ns: &str, args: &[&str]) -> io::Result<()> {
    let mut full = vec!["-n", ns];
    full.extend_from_slice(args);
    pingpath_client_core::net::linux::ip::ip(&full).map(drop)
}
