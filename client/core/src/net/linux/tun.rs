//! A Linux TUN device (IFF_TUN | IFF_NO_PI) opened with one TUNSETIFF
//! ioctl, read and written asynchronously through tokio's `AsyncFd`.
//! Address, MTU and link state are set with `ip` (see `ip.rs`).
//!
//! The only unsafe code is the ioctl, hence the module's scoped allow.
#![allow(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;

use nix::libc;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

// TUNSETIFF reads `struct ifreq` (name + flags) and writes the final name
// back, so the read-write form with `*mut ifreq` is the honest signature.
// libc::TUNSETIFF is `_IOW('T', 202, int)` = 0x400454ca on x86_64.
nix::ioctl_readwrite_bad!(tunsetiff, libc::TUNSETIFF, libc::ifreq);

/// An open TUN device. Dropping the last `Arc<Tun>` closes the fd, and the
/// kernel then deletes the (non-persistent) interface together with its
/// addresses and every route through it.
pub struct Tun {
    fd: AsyncFd<File>,
    name: String,
}

impl Tun {
    /// Opens `/dev/net/tun` and attaches a TUN interface called `name`
    /// (created if missing). Must be called inside a tokio runtime with
    /// IO enabled, because `AsyncFd::new` registers with its reactor.
    ///
    /// Errors (all observed except ENOENT): EPERM (`PermissionDenied`)
    /// from the ioctl without CAP_NET_ADMIN; ENOENT (`NotFound`) if
    /// /dev/net/tun is missing; EBUSY (`ResourceBusy`) if another fd holds
    /// a TUN of that name; EINVAL (`InvalidInput`) if `name` is an existing
    /// non-TUN interface such as a veth.
    pub fn open(name: &str) -> io::Result<Self> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() >= libc::IFNAMSIZ || bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interface name must be 1..=15 bytes without NUL",
            ));
        }
        // std opens with O_CLOEXEC already; AsyncFd needs O_NONBLOCK.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/net/tun")?;

        // SAFETY: ifreq is plain old data; all-zero is a valid value.
        let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
        for (dst, src) in req.ifr_name.iter_mut().zip(bytes) {
            *dst = *src as libc::c_char;
        }
        req.ifr_ifru.ifru_flags = (libc::IFF_TUN | libc::IFF_NO_PI) as libc::c_short;
        // SAFETY: the fd is open, and `req` is a valid, writable ifreq that
        // outlives the call.
        unsafe { tunsetiff(file.as_raw_fd(), &mut req) }.map_err(io::Error::from)?;

        // The kernel wrote the final name back (it differs only if `name`
        // was a pattern such as "pp%d").
        let name = req
            .ifr_name
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8 as char)
            .collect();
        Ok(Self {
            fd: AsyncFd::with_interest(file, Interest::READABLE | Interest::WRITABLE)?,
            name,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reads one IP packet (no packet-information header with IFF_NO_PI).
    /// A buffer shorter than the packet truncates it, so pass >= MTU bytes.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.fd
            .async_io(Interest::READABLE, |file| {
                let mut file: &File = file;
                file.read(buf)
            })
            .await
    }

    /// Writes one IP packet to the kernel, as if it arrived on this
    /// interface. EINVAL: not a valid IPv4/IPv6 packet. EIO: link is down.
    ///
    /// Writes first and waits for writability only on WouldBlock: a TUN
    /// whose link is down never polls writable, so waiting for readiness
    /// first could hang instead of returning EIO.
    pub async fn send(&self, packet: &[u8]) -> io::Result<usize> {
        let mut file: &File = self.fd.get_ref();
        match file.write(packet) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.fd
                    .async_io(Interest::WRITABLE, |file| {
                        let mut file: &File = file;
                        file.write(packet)
                    })
                    .await
            }
            other => other,
        }
    }
}

/// The adapter's packet queues, for `PacketChannels`.
pub struct Pump {
    pub incoming: mpsc::Receiver<Vec<u8>>,
    pub outgoing: mpsc::Sender<Vec<u8>>,
    pub tasks: [JoinHandle<()>; 2],
}

/// Spawns a reader task (TUN -> `incoming`) and a writer task
/// (`outgoing` -> TUN). Full queues drop packets instead of buffering
/// (game traffic is real-time). Abort both tasks to release the fd.
pub fn pump(tun: Arc<Tun>, queue: usize) -> Pump {
    let (to_engine, incoming) = mpsc::channel(queue);
    let (outgoing, mut from_engine) = mpsc::channel::<Vec<u8>>(queue);

    let reader = {
        let tun = tun.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                match tun.recv(&mut buf).await {
                    Ok(n) => {
                        // Full: drop. Closed: the engine is gone.
                        if let Err(mpsc::error::TrySendError::Closed(_)) =
                            to_engine.try_send(buf[..n].to_vec())
                        {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    // Note: deleting the interface under us does NOT wake a
                    // pending read (observed); only abort() ends this task.
                    Err(_) => return,
                }
            }
        })
    };
    let writer = tokio::spawn(async move {
        while let Some(packet) = from_engine.recv().await {
            // EINVAL (malformed) or EIO (link down): drop this packet only.
            let _ = tun.send(&packet).await;
        }
    });
    Pump {
        incoming,
        outgoing,
        tasks: [reader, writer],
    }
}
