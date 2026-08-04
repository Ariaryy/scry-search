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

const KEY_EVENT: u16 = 0x0001;

/// Layout of `_KEY_EVENT_RECORD` — matches the real Win32 struct byte for
/// byte (including the union collapsed to its `UnicodeChar` member) so it
/// can sit inside `InputRecord` below.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyEventRecord {
    key_down: Bool,
    repeat_count: u16,
    virtual_key_code: u16,
    virtual_scan_code: u16,
    unicode_char: u16,
    control_key_state: u32,
}

/// Layout of `_INPUT_RECORD`. The real struct's `Event` member is a union of
/// several record types; `KeyEventRecord` is the largest, so reusing it here
/// gives the same size/alignment as the union and is only ever read when
/// `event_type == KEY_EVENT`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputRecord {
    event_type: u16,
    _padding: u16,
    key_event: KeyEventRecord,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetStdHandle(std_handle: Dword) -> Handle;
    fn GetConsoleMode(console_handle: Handle, mode: *mut Dword) -> Bool;
    fn SetConsoleMode(console_handle: Handle, mode: Dword) -> Bool;
    fn GetNumberOfConsoleInputEvents(console_input: Handle, count: *mut Dword) -> Bool;
    fn ReadConsoleInputW(
        console_input: Handle,
        buffer: *mut InputRecord,
        length: Dword,
        events_read: *mut Dword,
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

    /// Non-blocking: true if the console has at least one buffered input
    /// record (key up/down, resize, focus, ...) waiting to be consumed.
    /// Used to drain a burst of keystrokes typed while a search is in
    /// flight without blocking on the next one that hasn't arrived yet.
    pub fn has_pending_input(&self) -> bool {
        let mut count = 0;
        let ok = unsafe { GetNumberOfConsoleInputEvents(self.input, &mut count) };
        ok != 0 && count > 0
    }

    /// Blocks until the console produces the next character, skipping
    /// non-character records (key-up, modifier-only key-down, resize,
    /// focus, ...) along the way.
    pub fn read_char(&self) -> Option<u16> {
        loop {
            let mut record = InputRecord::default();
            let mut read = 0;
            unsafe {
                if ReadConsoleInputW(self.input, &mut record, 1, &mut read) == 0 || read == 0 {
                    return None;
                }
            }
            if record.event_type == KEY_EVENT
                && record.key_event.key_down != 0
                && record.key_event.unicode_char != 0
            {
                return Some(record.key_event.unicode_char);
            }
        }
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
