//! Opening the service's IPC channel: the named pipe `\\.\pipe\pingpath`
//! on Windows, the Unix socket `/run/pingpath/service.sock` on Linux (for
//! development only; docs/decisions/007-client-architecture.md).

use std::io;

use crate::service::Transport;

/// Opens a connection to the service.
pub fn connect() -> io::Result<Box<dyn Transport>> {
    #[cfg(windows)]
    {
        pipe::open(pingpath_client_core::ipc::PIPE_NAME)
    }
    #[cfg(unix)]
    {
        let stream =
            std::os::unix::net::UnixStream::connect(pingpath_client_core::ipc::UNIX_SOCKET)?;
        stream.set_nonblocking(true)?;
        Ok(Box::new(crate::service::NonBlocking::new(stream)))
    }
}

/// The named pipe. The service's security descriptor grants interactive
/// users `0x12019b`: read, write data and attributes, but not
/// `FILE_CREATE_PIPE_INSTANCE`, which `GENERIC_WRITE` (and so tokio's
/// default open) asks for and is refused. So the pipe is opened with
/// `CreateFileW` and exactly the rights the descriptor grants.
#[cfg(windows)]
#[allow(unsafe_code)]
mod pipe {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, OPEN_EXISTING,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    use crate::service::Transport;

    /// A pipe handle in blocking (non-overlapped) mode. Reads go through
    /// `PeekNamedPipe` first so they never wait (see crate::service).
    struct Pipe {
        file: File,
    }

    pub fn open(name: &str) -> io::Result<Box<dyn Transport>> {
        let wide: Vec<u16> = OsStr::new(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives
        // the call; the security attributes and template handle may be
        // null. SECURITY_IDENTIFICATION lets whoever holds the pipe's name
        // learn who we are but never act as us: if the service isn't
        // running, another local user could have created the pipe.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateFileW succeeded, so `handle` is an open handle that
        // nothing else owns; OwnedHandle closes it exactly once.
        let owned = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };
        Ok(Box::new(Pipe {
            file: File::from(owned),
        }))
    }

    impl Transport for Pipe {
        fn read_now(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut available = 0u32;
            // SAFETY: the handle stays open for the call (we hold `file`);
            // with a null buffer of size 0 the call only reports how many
            // bytes are waiting, into `available`, which is valid for the
            // write. The other out-pointers may be null.
            let ok = unsafe {
                PeekNamedPipe(
                    self.file.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                // ERROR_BROKEN_PIPE when the service closed its end.
                return Err(io::Error::last_os_error());
            }
            if available == 0 {
                return Ok(0);
            }
            let len = buf.len().min(available as usize);
            match self.file.read(&mut buf[..len])? {
                0 => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the service closed the pipe",
                )),
                n => Ok(n),
            }
        }

        fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
            self.file.write_all(data)
        }
    }
}
