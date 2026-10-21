//! Bounded read-only section cache keyed by view generation.

use scry_core::{ArenaStore, IndexView};
use std::sync::Arc;

/// Cache key for [`shared_section`]. Keying on `Arc::as_ptr(&view.base)` is an
/// ABA hazard: once a generation's base is dropped, the allocator can reuse
/// its address for a later generation, and the cache would then hand out a
/// stale mapping under a key that looks fresh. `generation` is monotonic
/// (`scry_core::view::fresh_generation`) and never reused, so it cannot
/// collide this way.
pub(super) fn shared_section_cache_key(view: &IndexView) -> u64 {
    view.generation
}

/// Bound on the number of generations the section cache keeps entries for.
/// A client mid-request may still hold a reference to the previous
/// generation's shared section, so evicting immediately on publish would
/// race a concurrent reader; keeping 2 covers "one in flight, one current"
/// without letting the cache grow forever across a long-running daemon's
/// lifetime of compactions.
pub(super) const SHARED_SECTION_CACHE_GENERATIONS: usize = 2;

// Each entry holds `base` alongside the section so the section can never
// outlive, or be mismatched with, the memory it maps.
pub(super) type SectionCache =
    std::sync::Mutex<Vec<(u64, Arc<ArenaStore>, Arc<scry_ipc::Section>)>>;

pub(super) fn shared_section_from_cache(
    view: &IndexView,
    cache: &SectionCache,
) -> std::io::Result<Arc<scry_ipc::Section>> {
    let key = shared_section_cache_key(view);
    let mut cache = cache.lock().unwrap();
    if let Some((_, _, section)) = cache.iter().find(|(cached_key, ..)| *cached_key == key) {
        return Ok(section.clone());
    }
    let section = Arc::new(scry_ipc::Section::from_file(view.base.snapshot_file())?);
    cache.push((key, view.base.clone(), section.clone()));
    if cache.len() > SHARED_SECTION_CACHE_GENERATIONS {
        cache.remove(0);
    }
    Ok(section)
}

pub(super) fn shared_section(view: &IndexView) -> std::io::Result<Arc<scry_ipc::Section>> {
    static CACHE: std::sync::OnceLock<SectionCache> = std::sync::OnceLock::new();
    shared_section_from_cache(
        view,
        CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new())),
    )
}
