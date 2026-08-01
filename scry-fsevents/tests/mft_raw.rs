#![cfg(windows)]

use std::collections::{HashMap, HashSet};

#[test]
fn raw_and_usn_enumeration_agree() {
    let mut raw_frns = HashSet::new();
    let mut sample: HashMap<u64, HashSet<(u64, Vec<u8>)>> = HashMap::new();
    let mut slots = Vec::with_capacity(10_000);
    let mut seen = 0u64;
    let mut random = 0x9e37_79b9_7f4a_7c15u64;
    let raw_started = std::time::Instant::now();
    let report = match scry_fsevents::mft::enumerate_mft_raw_with_names(
        "C:",
        |frn, _, _, _, _, _| {
            raw_frns.insert(frn);
        },
        |frn, names| {
            if names.is_empty() {
                return;
            }
            seen += 1;
            let slot = if slots.len() < 10_000 {
                slots.push(frn);
                Some(slots.len() - 1)
            } else {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let candidate = random % seen;
                (candidate < 10_000).then_some(candidate as usize)
            };
            if let Some(slot) = slot {
                if let Some(replaced) = slots.get_mut(slot) {
                    sample.remove(replaced);
                    *replaced = frn;
                }
                sample.insert(
                    frn,
                    names
                        .iter()
                        .map(|name| (name.parent_frn, name.name.as_bytes().to_vec()))
                        .collect(),
                );
            }
        },
    ) {
        Ok(report) => report,
        Err(error) => {
            println!("skipped raw comparison (needs elevation or supported NTFS): {error}");
            return;
        }
    };
    let raw_elapsed = raw_started.elapsed();

    let mut usn_frns = HashSet::new();
    let mut mismatches = Vec::new();
    let usn_started = std::time::Instant::now();
    let usn_result = scry_fsevents::enumerate_mft_usn("C:", |frn, parent, name, _, _, _| {
        usn_frns.insert(frn);
        if let Some(valid_names) = sample.get(&frn) {
            if !valid_names.contains(&(parent, name.to_vec())) {
                mismatches.push((frn, valid_names.clone(), parent, name.to_vec()));
            }
        }
    });
    if let Err(error) = usn_result {
        println!("skipped USN comparison: {error}");
        return;
    }
    let usn_elapsed = usn_started.elapsed();

    let count_difference = raw_frns.len().abs_diff(usn_frns.len());
    let allowed_difference = usn_frns.len().div_ceil(100);
    let only_raw = raw_frns.difference(&usn_frns).count();
    let only_usn = usn_frns.difference(&raw_frns).count();
    println!(
        "raw={} in {:?}, usn={} in {:?}, only_raw={}, only_usn={}, report={report:?}",
        raw_frns.len(),
        raw_elapsed,
        usn_frns.len(),
        usn_elapsed,
        only_raw,
        only_usn
    );
    // The former single-name oracle failed on records with 43, 38, and 5 hard
    // links. NTFS and FSCTL_ENUM_USN_DATA may select different valid links, so
    // membership in the complete Win32-name set is the strict identity check.
    assert!(
        count_difference <= allowed_difference,
        "entry counts differ by more than 1%"
    );
    assert!(
        mismatches.is_empty(),
        "name/parent mismatches: {mismatches:?}"
    );
}
