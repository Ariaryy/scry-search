//! Snapshot and spool locations for normal and isolated benchmark runs.

use crate::BENCHMARK_CACHE;

pub(super) fn snapshot_path(volume: &str) -> std::path::PathBuf {
    let safe: String = volume.chars().filter(|c| c.is_alphanumeric()).collect();
    if let Some(directory) = BENCHMARK_CACHE.get() {
        return directory.join(format!("index-{safe}.rkyv"));
    }
    let app_data = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    app_data.join("scry").join(format!("index-{safe}.rkyv"))
}
/// Scratch directory for one volume's compaction spools, next to that
/// volume's snapshot. Every file created under it is removed automatically
/// when its `Spool`/`ByteSpool` drops; the directory itself is left in place
/// between runs so repeated compactions don't pay repeated `create_dir_all`
/// races, but it holds nothing durable.
pub(super) fn compaction_scratch_dir(volume: &str) -> std::path::PathBuf {
    let safe: String = volume.chars().filter(|c| c.is_alphanumeric()).collect();
    snapshot_path(volume)
        .parent()
        .expect("snapshot path always has a parent")
        .join(format!("compact-{safe}"))
}
