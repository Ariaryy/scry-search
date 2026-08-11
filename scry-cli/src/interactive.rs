use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use scry_client::SearchSession;
use scry_core::protocol::{QueryKind, ResultEntry};

use crate::console::{self, Key};

#[derive(Clone, Copy)]
enum UiStatus {
    Searching,
    Timed(Duration),
    Copied,
    CopyFailed,
}

enum Launch {
    Open(PathBuf),
    Reveal(PathBuf),
}

pub fn run(
    mut session: SearchSession,
    explicit_kind: Option<QueryKind>,
    initial: String,
) -> anyhow::Result<()> {
    let raw = console::RawMode::enable()
        .ok_or_else(|| anyhow::anyhow!("--interactive requires a real console"))?;
    let mut pattern = initial;
    let mut results = Vec::new();
    let mut selected = 0;
    let mut status = UiStatus::Searching;
    let mut query_started = Instant::now();
    let mut launch = None;

    session.submit(crate::infer_query_kind(explicit_kind, &pattern), &pattern)?;
    render(
        &pattern,
        &results,
        selected,
        status,
        (raw.width(), raw.height()),
    );

    'search: loop {
        let mut edited = false;
        let mut dirty = false;
        while let Some(key) = raw.try_read_key() {
            match key {
                Key::Escape => break 'search,
                Key::Enter => {
                    if let Some(result) = results.get(selected) {
                        launch = Some(Launch::Open(PathBuf::from(&result.path)));
                        break 'search;
                    }
                }
                Key::Reveal => {
                    if let Some(result) = results.get(selected) {
                        launch = Some(Launch::Reveal(PathBuf::from(&result.path)));
                        break 'search;
                    }
                }
                Key::Copy => {
                    if let Some(result) = results.get(selected) {
                        status = if console::copy_path(std::path::Path::new(&result.path)).is_ok() {
                            UiStatus::Copied
                        } else {
                            UiStatus::CopyFailed
                        };
                        dirty = true;
                    }
                }
                Key::Backspace => {
                    pattern.pop();
                    edited = true;
                }
                Key::Up => {
                    selected = selected.saturating_sub(1);
                    dirty = true;
                }
                Key::Down => {
                    selected = (selected + 1).min(results.len().saturating_sub(1));
                    dirty = true;
                }
                Key::Character(unit) => {
                    if let Some(Ok(ch)) = char::decode_utf16([unit]).next() {
                        if !ch.is_control() {
                            pattern.push(ch);
                            edited = true;
                        }
                    }
                }
            }
        }

        if edited {
            selected = 0;
            status = UiStatus::Searching;
            query_started = Instant::now();
            session.submit(crate::infer_query_kind(explicit_kind, &pattern), &pattern)?;
            dirty = true;
        }

        if let Some(latest) = session.poll_latest()? {
            results = latest;
            selected = selected.min(results.len().saturating_sub(1));
            status = UiStatus::Timed(query_started.elapsed());
            dirty = true;
        }

        if dirty {
            render(
                &pattern,
                &results,
                selected,
                status,
                (raw.width(), raw.height()),
            );
        }
        std::thread::sleep(Duration::from_millis(8));
    }

    drop(raw);
    if let Some(action) = launch {
        match action {
            Launch::Open(path) => console::open_path(&path)?,
            Launch::Reveal(path) => console::reveal_path(&path)?,
        }
    }
    Ok(())
}

fn render(
    pattern: &str,
    results: &[ResultEntry],
    selected: usize,
    status: UiStatus,
    dimensions: (usize, usize),
) {
    let mut frame = Vec::with_capacity(4_096);
    if render_to(&mut frame, pattern, results, selected, status, dimensions).is_err() {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&frame);
    let _ = out.flush();
}

fn render_to(
    out: &mut impl Write,
    pattern: &str,
    results: &[ResultEntry],
    selected: usize,
    status: UiStatus,
    dimensions: (usize, usize),
) -> std::io::Result<()> {
    let (width, height) = dimensions;
    write!(out, "\x1b[?2026h\x1b[H\x1b[J\x1b[1;36mScry Search\x1b[0m")?;
    write!(out, "\r\n\x1b[36m›\x1b[0m {pattern}\x1b[s")?;

    let status_text = match status {
        UiStatus::Searching => "searching…".to_owned(),
        UiStatus::Timed(duration) => format_duration(duration),
        UiStatus::Copied => "path copied".to_owned(),
        UiStatus::CopyFailed => "copy failed".to_owned(),
    };
    let status_width = status_text.chars().count();
    let status_column = width.saturating_sub(status_width).saturating_add(1);
    let prompt_width = pattern.chars().count() + 2;
    if prompt_width + 2 < status_column {
        write!(out, "\x1b[{status_column}G\x1b[2m{status_text}\x1b[0m")?;
    }
    write!(out, "\r\n\r\n")?;

    if pattern.is_empty() {
        writeln!(out, "  \x1b[2mStart typing to search your files.\x1b[0m")?;
    } else if results.is_empty() && !matches!(status, UiStatus::Searching) {
        writeln!(out, "  \x1b[2mNo matches\x1b[0m")?;
    } else {
        let visible_rows = height.saturating_sub(5).max(1);
        let first = selected.saturating_add(1).saturating_sub(visible_rows);
        for (index, entry) in results.iter().enumerate().skip(first).take(visible_rows) {
            let suffix = if entry.is_dir { "\\" } else { "" };
            let metadata = format!(
                "{}  ·  {}",
                crate::display_size(entry),
                crate::display_mtime(entry.mtime)
            );
            let metadata_width = metadata.chars().count();
            let metadata_column = width.saturating_sub(metadata_width).saturating_add(1);
            let path_width = metadata_column
                .saturating_sub(4 + suffix.chars().count())
                .max(1);
            let safe = visible_path(&entry.path);
            let path = fit_path(&safe, path_width);
            if index == selected {
                write!(out, "\x1b[36m›\x1b[0m \x1b[1m{path}{suffix}\x1b[0m")?;
            } else {
                write!(out, "  {path}{suffix}")?;
            }
            writeln!(out, "\x1b[{metadata_column}G\x1b[2m{metadata}\x1b[0m")?;
        }
    }

    write!(
        out,
        "\x1b[{height};1H\x1b[2K\x1b[2m↑/↓ select  •  Enter open  •  Ctrl+C copy  •  Alt+Enter reveal  •  Esc close\x1b[0m\x1b[u\x1b[?2026l"
    )
}

fn format_duration(duration: Duration) -> String {
    if duration.as_micros() < 1_000 {
        format!("{} µs", duration.as_micros())
    } else {
        format!("{:.1} ms", duration.as_secs_f64() * 1_000.0)
    }
}

fn fit_path(path: &str, width: usize) -> String {
    let count = path.chars().count();
    if count <= width {
        return path.to_owned();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let tail: String = path
        .chars()
        .rev()
        .take(width - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

fn visible_path(path: &str) -> String {
    path.chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(path: &str) -> ResultEntry {
        ResultEntry {
            path: path.into(),
            is_dir: false,
            size: 0,
            mtime: 0,
            size_exact: false,
        }
    }

    #[test]
    fn redraw_erases_everything_below_the_frame() {
        let mut output = Vec::new();
        render_to(
            &mut output,
            "new",
            &[result("volume\\new")],
            0,
            UiStatus::Timed(Duration::from_micros(420)),
            (100, 30),
        )
        .unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.starts_with("\x1b[?2026h\x1b[H\x1b[J"));
        assert!(rendered.contains("volume\\new"));
        assert!(rendered.contains("420 µs"));
        assert!(rendered.contains("\x1b[30;1H\x1b[2K"));
        assert!(rendered.ends_with("\x1b[u\x1b[?2026l"));
    }

    #[test]
    fn selection_is_visually_distinct() {
        let mut output = Vec::new();
        render_to(
            &mut output,
            "item",
            &[result("first"), result("second")],
            1,
            UiStatus::Timed(Duration::from_millis(2)),
            (100, 30),
        )
        .unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("  first"));
        assert!(rendered.contains("\x1b[1msecond\x1b[0m"));
    }

    #[test]
    fn duration_uses_compact_units() {
        assert_eq!(format_duration(Duration::from_micros(99)), "99 µs");
        assert_eq!(format_duration(Duration::from_micros(1_250)), "1.2 ms");
    }

    #[test]
    fn long_paths_keep_the_distinctive_filename_visible() {
        assert_eq!(fit_path("volume\\deep\\report.pdf", 12), "…\\report.pdf");
    }

    #[test]
    fn paths_cannot_inject_terminal_controls() {
        assert_eq!(visible_path("safe\u{1b}[2Jname"), "safe�[2Jname");
    }
}
