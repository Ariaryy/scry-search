//! Hand-rolled kernel32 console FFI for interactive mode, matching the style
//! used in `scry-ipc`/`scry-fsevents`: raw keystroke input (no line editing,
//! no echo) plus ANSI escape rendering, restored on drop.

use std::ffi::c_void;
use std::io::Write;

type Handle = *mut c_void;
type Bool = i32;
type Dword = u32;

const STD_INPUT_HANDLE: Dword = 0xFFFF_FFF6; // (-10i32) as u32
const STD_OUTPUT_HANDLE: Dword = 0xFFFF_FFF5; // (-11i32) as u32

const ENABLE_ECHO_INPUT: Dword = 0x0004;
const ENABLE_LINE_INPUT: Dword = 0x0002;
const ENABLE_PROCESSED_INPUT: Dword = 0x0001;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: Dword = 0x0004;

#[link(name = "kernel32")]
extern "system" {
    fn GetStdHandle(std_handle: Dword) -> Handle;
    fn GetConsoleMode(console_handle: Handle, mode: *mut Dword) -> Bool;
    fn SetConsoleMode(console_handle: Handle, mode: Dword) -> Bool;
    fn ReadConsoleW(
        console_input: Handle,
        buffer: *mut u16,
        chars_to_read: Dword,
        chars_read: *mut Dword,
        input_control: *mut c_void,
    ) -> Bool;
}

/// Puts stdin into raw, unbuffered, unechoed mode and stdout into ANSI mode
/// for the lifetime of this guard; both are restored to their prior modes on
/// drop, including on early return via `?`.
pub struct RawMode {
    input: Handle,
    input_original: Dword,
    output: Handle,
    output_original: Dword,
}

impl RawMode {
    pub fn enable() -> Option<Self> {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            if input.is_null() || output.is_null() {
                return None;
            }

            let mut input_original = 0;
            if GetConsoleMode(input, &mut input_original) == 0 {
                return None;
            }
            // Also drop ENABLE_PROCESSED_INPUT: Ctrl+C should reach us as a
            // plain 0x03 byte so this guard's Drop always runs, rather than
            // the console handling it as a termination signal.
            let raw_input =
                input_original & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
            if SetConsoleMode(input, raw_input) == 0 {
                return None;
            }

            let mut output_original = 0;
            if GetConsoleMode(output, &mut output_original) == 0 {
                SetConsoleMode(input, input_original);
                return None;
            }
            SetConsoleMode(output, output_original | ENABLE_VIRTUAL_TERMINAL_PROCESSING);

            Some(Self {
                input,
                input_original,
                output,
                output_original,
            })
        }
    }

    /// Blocks for a single UTF-16 code unit from the console.
    pub fn read_char(&self) -> Option<u16> {
        let mut buffer = [0u16; 1];
        let mut read = 0;
        unsafe {
            if ReadConsoleW(
                self.input,
                buffer.as_mut_ptr(),
                1,
                &mut read,
                std::ptr::null_mut(),
            ) == 0
                || read == 0
            {
                return None;
            }
        }
        Some(buffer[0])
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.input, self.input_original);
            SetConsoleMode(self.output, self.output_original);
        }
        // Leave the cursor on a fresh line rather than mid-render.
        let _ = std::io::stdout().write_all(b"\r\n");
    }
}
