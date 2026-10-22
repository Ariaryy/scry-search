//! An administrator-only stop event for an installed daemon update.
use std::ffi::c_void;

#[repr(C)]
struct SecurityAttributes {
    len: u32,
    descriptor: *mut c_void,
    inherit: i32,
}

#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn CreateEventW(
        sa: *const SecurityAttributes,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, timeout: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetLastError() -> u32;
    fn LocalFree(handle: *mut c_void) -> *mut c_void;
}

pub(super) fn listen() -> std::io::Result<()> {
    let sddl: Vec<u16> = "O:BAG:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)\0"
        .encode_utf16()
        .collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: all pointers refer to live, correctly sized Win32 arguments.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let sa = SecurityAttributes {
        len: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor,
        inherit: 0,
    };
    let name: Vec<u16> = format!("Local\\ScryShutdown-{}\0", std::process::id())
        .encode_utf16()
        .collect();
    let event = unsafe { CreateEventW(&sa, 1, 0, name.as_ptr()) };
    let already_exists = unsafe { GetLastError() } == 183;
    unsafe {
        LocalFree(descriptor);
    }
    if event.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    if already_exists {
        unsafe {
            CloseHandle(event);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "shutdown event already exists",
        ));
    }
    let handle = event as usize;
    std::thread::spawn(move || {
        let event = handle as *mut c_void;
        let result = unsafe { WaitForSingleObject(event, u32::MAX) };
        unsafe {
            CloseHandle(event);
        }
        if result == 0 {
            super::handle_console_ctrl(super::ffi::CTRL_CLOSE_EVENT);
        }
    });
    Ok(())
}
