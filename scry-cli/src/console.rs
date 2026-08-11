//! Hand-rolled kernel32 console FFI for interactive mode, matching the style
//! used in `scry-ipc`/`scry-fsevents`: raw keystroke input and ANSI output.

use std::ffi::c_void;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

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
const VK_BACK: u16 = 0x08;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_UP: u16 = 0x26;
const VK_DOWN: u16 = 0x28;
const SW_SHOWNORMAL: i32 = 1;
const FILETIME_UNIX_EPOCH_SECS: u64 = 11_644_473_600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Character(u16),
    Backspace,
    Enter,
    Escape,
    Up,
    Down,
}

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

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Coord {
    x: i16,
    y: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SmallRect {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ConsoleScreenBufferInfo {
    size: Coord,
    cursor_position: Coord,
    attributes: u16,
    window: SmallRect,
    maximum_window_size: Coord,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low: u32,
    high: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
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
    fn GetConsoleScreenBufferInfo(
        console_output: Handle,
        info: *mut ConsoleScreenBufferInfo,
    ) -> Bool;
    fn FileTimeToLocalFileTime(file_time: *const FileTime, local_time: *mut FileTime) -> Bool;
    fn FileTimeToSystemTime(file_time: *const FileTime, system_time: *mut SystemTime) -> Bool;
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        window: Handle,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show_command: i32,
    ) -> isize;
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

            let _ = std::io::stdout().write_all(b"\x1b[?1049h\x1b[H");

            Some(Self {
                input,
                input_original,
                output,
                output_original,
            })
        }
    }

    fn has_pending_input(&self) -> bool {
        let mut count = 0;
        let ok = unsafe { GetNumberOfConsoleInputEvents(self.input, &mut count) };
        ok != 0 && count > 0
    }

    /// Returns a buffered key without waiting for console input.
    pub fn try_read_key(&self) -> Option<Key> {
        while self.has_pending_input() {
            let mut record = InputRecord::default();
            let mut read = 0;
            unsafe {
                if ReadConsoleInputW(self.input, &mut record, 1, &mut read) == 0 || read == 0 {
                    return None;
                }
            }
            if record.event_type == KEY_EVENT && record.key_event.key_down != 0 {
                let key = match record.key_event.virtual_key_code {
                    VK_BACK => Some(Key::Backspace),
                    VK_RETURN => Some(Key::Enter),
                    VK_ESCAPE => Some(Key::Escape),
                    VK_UP => Some(Key::Up),
                    VK_DOWN => Some(Key::Down),
                    _ if record.key_event.unicode_char != 0 => {
                        Some(Key::Character(record.key_event.unicode_char))
                    }
                    _ => None,
                };
                if key.is_some() {
                    return key;
                }
            }
        }
        None
    }

    pub fn width(&self) -> usize {
        self.dimensions().0
    }

    pub fn height(&self) -> usize {
        self.dimensions().1
    }

    fn dimensions(&self) -> (usize, usize) {
        let mut info = ConsoleScreenBufferInfo::default();
        if unsafe { GetConsoleScreenBufferInfo(self.output, &mut info) } == 0 {
            return (80, 24);
        }
        (
            usize::try_from(info.window.right - info.window.left + 1).unwrap_or(80),
            usize::try_from(info.window.bottom - info.window.top + 1).unwrap_or(24),
        )
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.input, self.input_original);
            SetConsoleMode(self.output, self.output_original);
        }
        let _ = std::io::stdout().write_all(b"\x1b[?1049l");
    }
}

/// Opens a file with its registered application or a directory in Explorer.
pub fn open_path(path: &Path) -> std::io::Result<()> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result > 32 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "Windows could not open the selected path (ShellExecuteW code {result})"
        )))
    }
}

pub fn format_local_time(unix_seconds: u32) -> Option<String> {
    if unix_seconds == 0 {
        return None;
    }
    let ticks = (u64::from(unix_seconds) + FILETIME_UNIX_EPOCH_SECS) * 10_000_000;
    let utc = FileTime {
        low: ticks as u32,
        high: (ticks >> 32) as u32,
    };
    let mut local = FileTime::default();
    let mut system = SystemTime::default();
    if unsafe { FileTimeToLocalFileTime(&utc, &mut local) } == 0
        || unsafe { FileTimeToSystemTime(&local, &mut system) } == 0
    {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        system.year, system.month, system.day, system.hour, system.minute
    ))
}
