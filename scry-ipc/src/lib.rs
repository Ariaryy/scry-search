//! Minimal length-prefixed framing over a Windows named pipe. Shared by
//! scry-daemon (server side) and scry-client (client side) so the read/write
//! loop only exists once.

mod ffi;
mod section;

pub use section::{Section, SectionView};

use std::ffi::c_void;
use std::io;

/// Default pipe name scryd listens on and scry-client connects to.
pub const PIPE_NAME: &str = r"\\.\pipe\scry";

/// A connected pipe end, either the server's per-client instance or the
/// client's connection to the daemon. Framing (`read_frame`/`write_frame`)
/// is identical on both sides.
pub struct Pipe(ffi::Handle);

// Safety: Win32 HANDLEs are safe to transfer between threads (there's no
// thread-affinity for file/pipe handles). `Pipe` is also `Sync`, but only
// `pending_bytes` is safe to call concurrently with another thread's
// `read_frame`/`write_frame`: on a synchronous (non-overlapped) handle, the
// kernel serializes a thread's blocking `ReadFile` against another thread's
// `WriteFile` on the same handle through the file object's synchronous-I/O
// event, and if the read is blocked waiting for bytes the write itself needs
// to unblock (the read/respond cycle this crate implements), the two calls
// deadlock. `PeekNamedPipe` is a non-blocking query, not a positional I/O
// op, so it never joins that queue — that's what lets one thread do the
// blocking read+write cycle while another polls `pending_bytes` for a
// signal that a new request already arrived.
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

impl Pipe {
    /// Non-blocking check for unread bytes already sitting in the pipe's
    /// receive buffer — see the `Sync` safety note above for why this, and
    /// not a second `read_frame`, is the only concurrency-safe way to detect
    /// a newer request without blocking the response for the current one.
    pub fn pending_bytes(&self) -> io::Result<u32> {
        let mut avail: u32 = 0;
        let ok = unsafe {
            ffi::PeekNamedPipe(
                self.0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut avail,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(avail)
    }
}

impl Pipe {
    pub fn client_process_id(&self) -> io::Result<u32> {
        let mut process_id = 0;
        let ok = unsafe { ffi::GetNamedPipeClientProcessId(self.0, &mut process_id) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(process_id)
    }
    pub fn read_frame(&self) -> io::Result<Vec<u8>> {
        let mut len_buf = [0u8; 4];
        self.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        self.read_exact(&mut buf)?;
        Ok(buf)
    }

    pub fn write_frame(&self, data: &[u8]) -> io::Result<()> {
        let len = (data.len() as u32).to_le_bytes();
        self.write_all(&len)?;
        self.write_all(data)
    }

    fn read_exact(&self, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let mut n: u32 = 0;
            let ok = unsafe {
                ffi::ReadFile(
                    self.0,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "pipe closed"));
            }
            buf = &mut buf[n as usize..];
        }
        Ok(())
    }

    fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let mut n: u32 = 0;
            let ok = unsafe {
                ffi::WriteFile(
                    self.0,
                    buf.as_ptr() as *const c_void,
                    buf.len() as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            buf = &buf[n as usize..];
        }
        Ok(())
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            ffi::CloseHandle(self.0);
        }
    }
}

/// Server-side listener. Win32 named pipes don't have a single "listening
/// socket" — each `accept()` creates a fresh pipe instance and waits for one
/// client to connect to it, which is what lets multiple worker threads call
/// `accept()` concurrently for concurrent client handling.
pub struct PipeServer {
    name: Vec<u16>,
    /// Explicit DACL granting local read/write to everyone, built once and
    /// reused for every pipe instance `accept()` creates. Needed because a
    /// pipe created by an elevated scryd otherwise inherits a default DACL
    /// that only other elevated processes can open — defeating the point of
    /// unelevated apps talking to it over the SDK.
    security: SecurityDescriptor,
}

/// Owns the memory returned by ConvertStringSecurityDescriptorToSecurityDescriptorW
/// for as long as the server needs to hand out a pointer to it.
struct SecurityDescriptor(*mut c_void);
unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                ffi::LocalFree(self.0);
            }
        }
    }
}

fn everyone_read_write_sd() -> io::Result<SecurityDescriptor> {
    // D: (DACL) A (Allow) GA (Generic All) WD (World / Everyone). Local named
    // pipes (`\\.\pipe\...`) aren't network-reachable, so "everyone" here
    // means "any local process/user", not remote.
    let sddl = to_wide("D:(A;;GA;;;WD)");
    let mut sd: *mut c_void = std::ptr::null_mut();
    let ok = unsafe {
        ffi::ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1, // SDDL_REVISION_1
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SecurityDescriptor(sd))
}

impl PipeServer {
    pub fn new(name: &str) -> io::Result<Self> {
        Ok(Self {
            name: to_wide(name),
            security: everyone_read_write_sd()?,
        })
    }

    pub fn accept(&self) -> io::Result<Pipe> {
        let mut sa = ffi::SecurityAttributes {
            n_length: std::mem::size_of::<ffi::SecurityAttributes>() as u32,
            lp_security_descriptor: self.security.0,
            b_inherit_handle: 0,
        };
        let handle = unsafe {
            ffi::CreateNamedPipeW(
                self.name.as_ptr(),
                ffi::PIPE_ACCESS_DUPLEX,
                ffi::PIPE_TYPE_BYTE | ffi::PIPE_READMODE_BYTE | ffi::PIPE_WAIT,
                ffi::PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &mut sa as *mut _ as *mut c_void,
            )
        };
        if handle == ffi::INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let ok = unsafe { ffi::ConnectNamedPipe(handle, std::ptr::null_mut()) };
        if ok == 0 {
            let err = unsafe { ffi::GetLastError() };
            // A client racing in between CreateNamedPipeW and ConnectNamedPipe
            // is reported as ERROR_PIPE_CONNECTED, not a real failure.
            if err != ffi::ERROR_PIPE_CONNECTED {
                unsafe { ffi::CloseHandle(handle) };
                return Err(io::Error::from_raw_os_error(err as i32));
            }
        }
        Ok(Pipe(handle))
    }
}

/// Connects to a running server on `name`, retrying briefly if every pipe
/// instance is currently busy (ERROR_PIPE_BUSY) rather than failing
/// immediately under concurrent client load.
pub fn connect_client(name: &str) -> io::Result<Pipe> {
    let wide = to_wide(name);
    loop {
        let handle = unsafe {
            ffi::CreateFileW(
                wide.as_ptr(),
                ffi::GENERIC_READ | ffi::GENERIC_WRITE,
                0,
                std::ptr::null_mut(),
                ffi::OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle != ffi::INVALID_HANDLE_VALUE {
            return Ok(Pipe(handle));
        }
        let err = unsafe { ffi::GetLastError() };
        if err != ffi::ERROR_PIPE_BUSY {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        let waited = unsafe { ffi::WaitNamedPipeW(wide.as_ptr(), 2000) };
        if waited == 0 {
            return Err(io::Error::last_os_error());
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
