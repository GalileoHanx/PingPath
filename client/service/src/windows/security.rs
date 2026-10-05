//! Who may do what: the named pipe's and the data folder's permissions,
//! and whether this process runs elevated (docs/decisions/007).
//!
//! The service runs as SYSTEM and trusts `%ProgramData%\PingPath` (its
//! settings, the route ledger, extra game definitions), so only SYSTEM and
//! Administrators may write there. `C:\ProgramData` lets any user create
//! folders, so a folder that is already there when PingPath installs may
//! belong to someone else: one that isn't owned by SYSTEM or
//! Administrators, or is a link, is moved aside, never trusted, and a new
//! folder is created with the right permissions from the start.

#![allow(unsafe_code)]

use std::ffi::{OsStr, c_void};
use std::fs;
use std::io;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::paths::Paths;

/// The pipe: SYSTEM and Administrators in full; signed-in (interactive)
/// users read and write, but not FILE_CREATE_PIPE_INSTANCE (part of
/// GENERIC_WRITE), so no user can start a rogue instance of the pipe and
/// answer the UI in the service's place. The UI must open the pipe asking
/// for no more than FILE_GENERIC_READ | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES.
pub const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019b;;;IU)";
/// The data folder: SYSTEM and Administrators only, inherited by
/// everything inside, nothing inherited from `C:\ProgramData`.
pub const DATA_SDDL: &str = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
/// The logs: Users may read them too, for the app's diagnostic report.
pub const LOGS_SDDL: &str = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FR;;;BU)";

/// Owners a folder the service trusts may have: SYSTEM, Administrators,
/// TrustedInstaller.
const TRUSTED_OWNERS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(once(0)).collect()
}

/// A security descriptor parsed from SDDL, freed on drop.
pub struct SecurityDescriptor {
    sd: PSECURITY_DESCRIPTOR,
}

// SAFETY: the descriptor is never changed after creation; Windows only
// reads it, from any thread.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let sddl = wide(OsStr::new(sddl));
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; `sd` is a valid out-pointer; the
        // size out-pointer may be null.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { sd })
    }

    /// Attributes pointing at this descriptor; valid while `self` lives.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd,
            bInheritHandle: 0,
        }
    }

    fn dacl(&self) -> io::Result<*mut ACL> {
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl: *mut ACL = ptr::null_mut();
        // SAFETY: `self.sd` is a valid descriptor; the out-pointers are valid.
        let ok =
            unsafe { GetSecurityDescriptorDacl(self.sd, &mut present, &mut dacl, &mut defaulted) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if present == 0 || dacl.is_null() {
            return Err(io::Error::other("the security descriptor has no DACL"));
        }
        Ok(dacl)
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW,
        // freed once.
        unsafe { LocalFree(self.sd as HLOCAL) };
    }
}

/// Creates or repairs the data folder, its logs and state folders, with
/// the permissions above. Run as SYSTEM or an elevated administrator.
pub fn prepare_data_dir(paths: &Paths) -> io::Result<()> {
    secure_dir(&paths.data, DATA_SDDL)?;
    let logs = paths.logs();
    if logs.starts_with(&paths.data) {
        secure_dir(&logs, LOGS_SDDL)?;
    }
    fs::create_dir_all(paths.state())
}

fn secure_dir(path: &Path, sddl: &str) -> io::Result<()> {
    let sd = SecurityDescriptor::from_sddl(sddl)?;
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            let is_link = meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            if !meta.is_dir() || is_link || !owner_is_trusted(path)? {
                let aside = path.with_file_name(format!(
                    "{}.untrusted-{}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                ));
                tracing::warn!(path = %path.display(), aside = %aside.display(), "a folder PingPath trusts was created by someone else; moving it aside");
                fs::rename(path, &aside)?;
                create_dir(path, &sd)
            } else {
                set_dacl(path, &sd)
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_dir(path, &sd),
        Err(e) => Err(e),
    }
}

fn create_dir(path: &Path, sd: &SecurityDescriptor) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = wide(path.as_os_str());
    let attributes = sd.attributes();
    // SAFETY: `name` is NUL-terminated; `attributes` points at a valid
    // descriptor that outlives the call.
    if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Replaces the folder's permissions; children inherit them.
fn set_dacl(path: &Path, sd: &SecurityDescriptor) -> io::Result<()> {
    let name = wide(path.as_os_str());
    let dacl = sd.dacl()?;
    // SAFETY: `name` is NUL-terminated; `dacl` points into `sd`, alive for
    // the call; owner, group and SACL are not set.
    let error = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    Ok(())
}

fn owner_is_trusted(path: &Path) -> io::Result<bool> {
    let name = wide(path.as_os_str());
    let mut owner: PSID = ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `name` is NUL-terminated; `owner` and `sd` are valid
    // out-pointers; `owner` points into `sd`, freed below.
    let error = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut sd,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let sid = sid_string(owner);
    // SAFETY: allocated by GetNamedSecurityInfoW, freed once, after the
    // last use of `owner`.
    unsafe { LocalFree(sd as HLOCAL) };
    let sid = sid?;
    Ok(TRUSTED_OWNERS.contains(&sid.as_str()))
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is a valid SID; `text` is a valid out-pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `text` is a NUL-terminated string allocated by the call.
    let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
    // SAFETY: `len` u16s before the terminator are initialised.
    let result = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: allocated by ConvertSidToStringSidW, freed once.
    unsafe { LocalFree(text as HLOCAL) };
    Ok(result)
}

/// Whether this process has an administrator's full token (or is SYSTEM).
pub fn is_elevated() -> bool {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no
    // closing; `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: the buffer is a TOKEN_ELEVATION of the size passed.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            ptr::from_mut(&mut elevation).cast::<c_void>(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: opened above, closed once.
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// Commands that change the system need an elevated prompt.
pub fn require_elevation() -> Result<(), String> {
    if is_elevated() {
        Ok(())
    } else {
        Err(
            "run this as administrator (right-click the terminal, then Run as administrator)"
                .into(),
        )
    }
}
