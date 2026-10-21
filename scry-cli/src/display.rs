//! Terminal-safe result presentation and compact metadata formatting.

use crate::console;
use scry_client::ResultEntry;
use std::io::IsTerminal;

pub(super) fn print_entry(entry: &ResultEntry) {
    let marker = if entry.is_dir { "/" } else { "" };
    if std::io::stdout().is_terminal() {
        let path = visible_terminal_text(&entry.path);
        let color = if entry.is_dir { "36" } else { "37" };
        println!(
            "  \x1b[1;{color}m{path}{marker}\x1b[0m  \x1b[2m{}  ·  {}\x1b[0m",
            display_size(entry),
            display_mtime(entry.mtime)
        );
    } else {
        println!(
            "{}{marker}\t{}\t{}",
            entry.path,
            display_size(entry),
            display_mtime(entry.mtime)
        );
    }
}

pub(super) fn visible_terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

pub(crate) fn display_size(entry: &ResultEntry) -> String {
    if !(entry.size_exact || entry.is_dir && entry.size > 0) {
        return "—".into();
    }
    let prefix = if entry.size_exact { "" } else { "≥" };
    format!("{prefix}{}", human_size(entry.size))
}

pub(super) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes == 0 {
        return "0 B".into();
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 10.0 || value.fract() < 0.05 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub(crate) fn display_mtime(mtime: u32) -> String {
    console::format_local_time(mtime).unwrap_or_else(|| "unknown date".into())
}
