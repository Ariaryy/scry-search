//! On-demand updater frontend. The elevated engine is embedded, never loaded
//! from an adjacent writable script. It accepts a release tag, not a command.
use std::io::{BufRead, IsTerminal, Write};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

const RELEASE_SUPPORT: &str = include_str!("../../scripts/updater/release.ps1");
const PROCESS_SUPPORT: &str = include_str!("../../scripts/updater/processes.ps1");
const CHECK: &str = include_str!("../../scripts/updater/check-update.ps1");
const ENGINE: &str = include_str!("../../scripts/updater/coordinator.ps1");

fn installed() -> bool {
    let legacy = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .is_some_and(|p| p.join("Programs/Scry Search/bin/scryd.exe").is_file());
    if legacy {
        return true;
    }
    let Some(sid) = current_sid() else {
        return false;
    };
    let Some(programs) = std::env::var_os("ProgramFiles") else {
        return false;
    };
    let root = std::path::PathBuf::from(programs)
        .join("Scry Search")
        .join(sid);
    root.join("bin/scryd.exe").is_file()
        || std::fs::read_dir(root.join("releases")).is_ok_and(|entries| {
            entries
                .filter_map(Result::ok)
                .take(1024)
                .any(|entry| entry.path().join("scryd.exe").is_file())
        })
}

fn current_sid() -> Option<String> {
    use std::ffi::c_void;
    #[link(name = "advapi32")]
    extern "system" {
        fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
        fn GetTokenInformation(
            token: *mut c_void,
            class: u32,
            buffer: *mut c_void,
            len: u32,
            needed: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, text: *mut *mut u16) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn LocalFree(handle: *mut c_void) -> *mut c_void;
    }
    // SAFETY: Win32 writes into aligned bounded storage, returned handles and
    // allocated SID text are released on every successful allocation path.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), 8, &mut token) == 0 {
            return None;
        }
        let mut needed = 0;
        GetTokenInformation(token, 1, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 || needed > 65536 {
            CloseHandle(token);
            return None;
        }
        let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = GetTokenInformation(token, 1, buffer.as_mut_ptr().cast(), needed, &mut needed);
        CloseHandle(token);
        if ok == 0 {
            return None;
        }
        let mut text = std::ptr::null_mut();
        if ConvertSidToStringSidW(buffer[0] as *mut c_void, &mut text) == 0 {
            return None;
        }
        let mut len = 0;
        while len < 256 && *text.add(len) != 0 {
            len += 1;
        }
        let result =
            (len < 256).then(|| String::from_utf16_lossy(std::slice::from_raw_parts(text, len)));
        LocalFree(text.cast());
        result
    }
}

fn valid_tag(tag: &str) -> bool {
    tag.starts_with('v')
        && tag.len() <= 65
        && tag.len() > 1
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
}

pub(super) fn dispatch() -> anyhow::Result<Option<i32>> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    if command == "check-update" {
        return Ok(Some(check_update(args)?));
    }
    if command != "update" && command != "status" {
        return Ok(None);
    }
    let mut json = false;
    let mut target = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--version" => {
                let tag = args.next().unwrap_or_default();
                anyhow::ensure!(
                    valid_tag(&tag),
                    "--version requires a release tag such as v0.1.0-alpha.4"
                );
                target = Some(tag);
            }
            _ => anyhow::bail!("unknown {command} option: {arg}"),
        }
    }
    if command == "status" {
        return Ok(Some(status(json, target.as_deref())));
    }
    let script = format!(
        "$Release = '{}';\n{}",
        target.as_deref().unwrap_or("latest"),
        ENGINE
            .replace("# RELEASE_SUPPORT", RELEASE_SUPPORT)
            .replace("# PROCESS_SUPPORT", PROCESS_SUPPORT)
    );
    Ok(Some(run_script(&script, json, "Scry Search update")?))
}

fn run_script(script: &str, json: bool, title: &str) -> anyhow::Result<i32> {
    // Feeding the coordinator over stdin avoids a command-line length bound.
    let system =
        std::env::var_os("SystemRoot").ok_or_else(|| anyhow::anyhow!("SystemRoot unavailable"))?;
    let mut child = Command::new(
        std::path::Path::new(&system).join("System32/WindowsPowerShell/v1.0/powershell.exe"),
    )
    .args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "$s=[Console]::In.ReadToEnd(); & ([scriptblock]::Create($s))",
    ])
    .creation_flags(0x08000000)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .spawn()?;
    let mut input = child.stdin.take().unwrap();
    input.write_all(script.as_bytes())?;
    drop(input);
    let output = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(16);
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines() {
            let Ok(line) = line else {
                break;
            };
            if line.len() > 4096 || tx.send(line).is_err() {
                break;
            }
        }
    });
    let animated = !json && std::io::stderr().is_terminal();
    let mut phase = "Preparing update".to_owned();
    let mut tick = 0usize;
    let mut result_code = None;
    if !json {
        eprintln!("\n  {title}\n");
    }
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if event["schema"] != 1 {
                    continue;
                }
                if json {
                    println!("{event}");
                }
                if let Some(message) = event["message"].as_str() {
                    phase = message
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(256)
                        .collect();
                    if !animated && !json {
                        eprintln!("  {phase}");
                    }
                }
                if event["event"] == "result" || event["event"] == "check_update" {
                    result_code = event["exit_code"]
                        .as_i64()
                        .and_then(|n| i32::try_from(n).ok());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if animated {
            eprint!("\r  {} {:<72}", ["|", "/", "-", "\\"][tick % 4], phase);
            let _ = std::io::stderr().flush();
            tick += 1;
        }
    }
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("update output reader failed"))?;
    let code = child.wait()?.code().unwrap_or(1);
    if animated {
        eprintln!("\r  {phase:<76}\n");
    }
    Ok(if result_code == Some(code) { code } else { 1 })
}

fn check_update(mut args: impl Iterator<Item = String>) -> anyhow::Result<i32> {
    let mut json = false;
    let mut ipc = None;
    let mut index = None;
    let mut channel = "stable".to_owned();
    let mut current = env!("CARGO_PKG_VERSION").to_owned();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--compatible-ipc" | "--compatible-index" => {
                let value = args.next().unwrap_or_default().parse::<u16>()?;
                anyhow::ensure!(value > 0, "compatibility versions must be positive");
                if arg == "--compatible-ipc" {
                    ipc = Some(value);
                } else {
                    index = Some(value);
                }
            }
            "--channel" => {
                channel = args.next().unwrap_or_default();
                anyhow::ensure!(
                    channel == "stable" || channel == "prerelease",
                    "unknown channel"
                );
            }
            "--current-version" => {
                current = args
                    .next()
                    .unwrap_or_default()
                    .trim_start_matches('v')
                    .to_owned();
                anyhow::ensure!(valid_tag(&format!("v{current}")), "invalid current version");
            }
            _ => anyhow::bail!("unknown check-update option: {arg}"),
        }
    }
    let ipc = ipc.ok_or_else(|| anyhow::anyhow!("--compatible-ipc is required"))?;
    let index = index.ok_or_else(|| anyhow::anyhow!("--compatible-index is required"))?;
    let script = format!("$CompatibleIpc={ipc};$CompatibleIndex={index};$Channel='{channel}';$CurrentVersion='{current}';\n{RELEASE_SUPPORT}\n{CHECK}");
    run_script(&script, json, "Checking for Scry Search updates")
}

fn status(json: bool, target: Option<&str>) -> i32 {
    let (state, code, detail) = match scry_client::Client::daemon_status_with_server(
        Duration::from_secs(3),
    ) {
        Ok((status, server_pid)) => {
            let matches =
                target.is_none_or(|v| v.strip_prefix('v') == Some(status.version.as_str()));
            let state = if !status.rpc_compatible() {
                "incompatible"
            } else if !matches {
                "update_required"
            } else {
                "ready"
            };
            let code = if state == "ready" { 0 } else { 10 };
            (
                state,
                code,
                serde_json::json!({"server_pid":server_pid,"daemon_version":status.version,"ipc_version":status.ipc_version,"index_format":status.index_format,"capabilities":status.capabilities}),
            )
        }
        Err(error) => {
            let io = error
                .chain()
                .find_map(|e| e.downcast_ref::<std::io::Error>());
            let state = if error.is::<scry_client::UnsupportedHandshake>() {
                "unsupported_handshake"
            } else {
                match io.map(std::io::Error::kind) {
                    Some(std::io::ErrorKind::TimedOut) => "timeout",
                    Some(std::io::ErrorKind::PermissionDenied) => "access_denied",
                    Some(std::io::ErrorKind::NotFound) => {
                        if installed() {
                            "stopped"
                        } else {
                            "not_installed"
                        }
                    }
                    Some(std::io::ErrorKind::InvalidData) | None => "invalid_response",
                    _ => "transport_error",
                }
            };
            (
                state,
                if state == "unsupported_handshake" {
                    10
                } else {
                    11
                },
                serde_json::json!({}),
            )
        }
    };
    let event = serde_json::json!({"schema":1,"event":"status","state":state,"exit_code":code,"detail":detail});
    if json {
        println!("{event}");
    } else {
        println!("Scry Search: {state}\n{detail}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_result_and_exit_code_must_agree() {
        assert_eq!(
            run_script(
                "'{\"schema\":1,\"event\":\"result\",\"exit_code\":21}';exit 21",
                true,
                "test"
            )
            .unwrap(),
            21
        );
        assert_eq!(
            run_script(
                "'{\"schema\":1,\"event\":\"result\",\"exit_code\":0}';exit 21",
                true,
                "test"
            )
            .unwrap(),
            1
        );
        assert_eq!(
            run_script(
                "'{\"schema\":2,\"event\":\"result\",\"exit_code\":0}';exit 0",
                true,
                "test"
            )
            .unwrap(),
            1
        );
    }
    #[test]
    fn release_is_data_never_shell_syntax_or_a_path() {
        assert!(valid_tag("v0.1.0-alpha.4"));
        for tag in [
            "",
            "latest",
            "v../evil",
            "v'; exit",
            "v$env:PATH",
            "v\\evil",
            "v\nexit",
        ] {
            assert!(!valid_tag(tag));
        }
    }
}
