//! Real-daemon latency harness for interactive-query measurements. Not a
//! permanent tool — a thin driver over the same `Client` API any consumer
//! uses, run manually against `scryd` on the real corpus and its numbers
//! copied into `plans/002-measurements.md`.
//!
//! Usage: `measure_queries <keystrokes|pathological|burst> [pipe_name]`

use scry_client::Client;
use scry_core::protocol::QueryKind;
use std::time::{Duration, Instant};

const SEQUENCES: &[&str] = &["annual report", "report", ".pdf", "a"];

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

fn report(label: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let p50 = percentile(&samples, 0.50);
    let p99 = percentile(&samples, 0.99);
    let mean: Duration = samples.iter().sum::<Duration>() / samples.len() as u32;
    println!(
        "{label}: n={} mean={:?} p50={:?} p99={:?}",
        samples.len(),
        mean,
        p50,
        p99
    );
}

fn kind_for(pattern: &str) -> QueryKind {
    if pattern.bytes().any(|b| matches!(b, b'*' | b'?')) {
        QueryKind::Wildcard
    } else {
        QueryKind::PathTerms
    }
}

/// Types each sequence character by character over one pipelined connection
/// (matching how the CLI's `--interactive` mode drives it), recording the
/// wall-clock time from each keystroke's request to its response.
fn run_keystrokes(pipe_name: &str) -> anyhow::Result<()> {
    for sequence in SEQUENCES {
        let mut client = Client::connect_to(pipe_name)?;
        let mut pattern = String::new();
        let mut samples = Vec::new();
        for ch in sequence.chars() {
            pattern.push(ch);
            let start = Instant::now();
            client.send_interactive(kind_for(&pattern), &pattern, 50)?;
            client.recv_interactive()?;
            samples.push(start.elapsed());
        }
        report(&format!("keystrokes {sequence:?}"), samples);
    }
    Ok(())
}

/// The pathological case flagged in the plan: `limit = 50` against a
/// one-character substring query, which (pre-parallelization) scanned and
/// allocated a decoded string for every one of ~1M candidate names.
fn run_pathological(pipe_name: &str) -> anyhow::Result<()> {
    let client = Client::connect_to(pipe_name)?;
    let mut samples = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        client.query(QueryKind::Substring, "e", 50)?;
        samples.push(start.elapsed());
    }
    report("pathological one-char substring, limit=50", samples);
    Ok(())
}

/// Fires 200 as-you-type keystrokes back to back (no think time) to give an
/// external RSS-sampling script a load burst to measure against, then exits
/// so the daemon goes idle for the follow-up 60s sample.
fn run_burst(pipe_name: &str) -> anyhow::Result<()> {
    let mut client = Client::connect_to(pipe_name)?;
    let mut pattern = String::new();
    let mut sent = 0;
    'sequences: loop {
        for sequence in SEQUENCES {
            pattern.clear();
            for ch in sequence.chars() {
                pattern.push(ch);
                client.send_interactive(kind_for(&pattern), &pattern, 50)?;
                sent += 1;
                if sent >= 200 {
                    break 'sequences;
                }
            }
        }
    }
    client.recv_interactive()?;
    println!("burst: sent {sent} pipelined requests");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let pipe_name = args
        .next()
        .unwrap_or_else(|| scry_ipc::PIPE_NAME.to_string());
    match mode.as_str() {
        "keystrokes" => run_keystrokes(&pipe_name),
        "pathological" => run_pathological(&pipe_name),
        "burst" => run_burst(&pipe_name),
        _ => {
            eprintln!("usage: measure_queries <keystrokes|pathological|burst> [pipe_name]");
            std::process::exit(1);
        }
    }
}
