//! The UI channel on Windows: the named pipe
//! [`PIPE_NAME`](pingpath_client_core::ipc::PIPE_NAME), local clients only,
//! with the permissions in [`security::PIPE_SDDL`]. Every instance gets the
//! same security descriptor (one created without it would get the default
//! permissions), and the first is created with `first_pipe_instance`, so
//! the service fails loudly if another process already holds the name.
//! A listening instance always exists while the service runs, so the name
//! can't be taken between connections.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;
use std::sync::Arc;

use pingpath_client_core::ipc::{ErrorBody, Message, PIPE_NAME};
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
use tokio::sync::watch;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Pipes::GetNamedPipeClientSessionId;

use super::security::{PIPE_SDDL, SecurityDescriptor};
use crate::server::{Caller, Service};

fn create(
    options: &ServerOptions,
    security: &SecurityDescriptor,
    first: bool,
) -> io::Result<NamedPipeServer> {
    let mut options = options.clone();
    options.first_pipe_instance(first);
    let mut attributes = security.attributes();
    // SAFETY: `attributes` points at a valid security descriptor that
    // outlives the call; Windows copies it into the new instance.
    unsafe {
        options.create_with_security_attributes_raw(
            PIPE_NAME,
            ptr::from_mut(&mut attributes).cast::<c_void>(),
        )
    }
}

/// The Windows session of the connected client: each signed-in user has
/// their own.
fn client_session(pipe: &NamedPipeServer) -> io::Result<u32> {
    let mut session = 0u32;
    // SAFETY: the handle is a connected pipe owned by `pipe`; `session` is
    // a valid out-pointer.
    let ok = unsafe { GetNamedPipeClientSessionId(pipe.as_raw_handle() as HANDLE, &mut session) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(session)
}

/// Accepts UI connections until `stop`.
pub async fn listen(service: Arc<Service>, mut stop: watch::Receiver<bool>) -> io::Result<()> {
    let security = SecurityDescriptor::from_sddl(PIPE_SDDL)?;
    let mut options = ServerOptions::new();
    options
        .pipe_mode(PipeMode::Byte)
        .reject_remote_clients(true);
    let mut server = create(&options, &security, true).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot create {PIPE_NAME} (is another copy of the service running?): {e}"),
        )
    })?;
    tracing::info!(pipe = PIPE_NAME, "listening for the UI");

    loop {
        tokio::select! {
            connected = server.connect() => connected?,
            _ = stop.wait_for(|stop| *stop) => return Ok(()),
        }
        // The next instance before anything else, so one always listens.
        let next = create(&options, &security, false)?;
        let pipe = std::mem::replace(&mut server, next);
        let service = service.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let caller = match client_session(&pipe) {
                Ok(session) => Caller::WindowsSession(session),
                Err(error) => {
                    tracing::warn!(%error, "cannot tell who connected; closing");
                    return;
                }
            };
            let slot = service
                .try_slot()
                .ok_or_else(Service::full)
                .and_then(|permit| service.claim(caller, permit));
            match slot {
                Ok(slot) => {
                    let _ = service.serve(pipe, slot, stop).await;
                }
                Err(error) => refuse(pipe, error).await,
            }
        });
    }
}

async fn refuse(mut pipe: NamedPipeServer, error: ErrorBody) {
    let _ = pipe
        .write_all(Message::err(0, error).to_line().as_bytes())
        .await;
    let _ = pipe.flush().await;
    let _ = pipe.disconnect();
}
