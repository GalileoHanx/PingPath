//! The UI channel on Linux: a Unix socket at
//! [`UNIX_SOCKET`](pingpath_client_core::ipc::UNIX_SOCKET). Linux runs the
//! client only for development and the network-namespace tests, so any
//! local user may connect, as any signed-in user may on Windows; the
//! caller is told apart by their user ID.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;

use pingpath_client_core::ipc::Message;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use crate::server::{Caller, Service};

/// Accepts UI connections on `path` until `stop`.
pub async fn listen(
    path: PathBuf,
    service: Arc<Service>,
    mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    match fs::symlink_metadata(&path) {
        // Left over from a service that didn't stop cleanly.
        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(&path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(&path)?;
    // Removed however this ends, including when the service drops this
    // future to stop.
    let _socket = RemoveOnDrop(path.clone());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666))?;
    tracing::info!(path = %path.display(), "listening for the UI");

    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => return Err(e),
            },
            _ = stop.wait_for(|stop| *stop) => return Ok(()),
        };
        let caller = match stream.peer_cred() {
            Ok(cred) => Caller::Uid(cred.uid()),
            Err(error) => {
                tracing::warn!(%error, "cannot tell who connected; closing");
                continue;
            }
        };
        let service = service.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let slot = service
                .try_slot()
                .ok_or_else(Service::full)
                .and_then(|permit| service.claim(caller, permit));
            match slot {
                Ok(slot) => {
                    let _ = service.serve(stream, slot, stop).await;
                }
                Err(error) => refuse(stream, error).await,
            }
        });
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

async fn refuse(mut stream: UnixStream, error: pingpath_client_core::ipc::ErrorBody) {
    let _ = stream
        .write_all(Message::err(0, error).to_line().as_bytes())
        .await;
    let _ = stream.shutdown().await;
}
