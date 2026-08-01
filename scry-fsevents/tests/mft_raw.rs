#![cfg(windows)]

use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
struct Identity {
    parent: u64,
    name: Vec<u8>,
}

#[test]
fn raw_and_usn_enumeration_agree() {
    let mut raw_frns = HashSet::new();
    let mut sample = HashMap::new();
    let raw_started = std::time::Instant::now();
    let report = match scry_fsevents::mft::enumerate_mft_raw("C:", |frn, parent, name, _, _, _| {
        raw_frns.insert(frn);
        if sample.len() < 10_000 {
            sample.insert(
                frn,
                Identity {
                    parent,
                    name: name.to_vec(),
                },
            );
        }
    }) {
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
        if let Some(expected) = sample.get(&frn) {
            if expected.parent != parent || expected.name != name {
                mismatches.push((frn, expected.clone(), parent, name.to_vec()));
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
    assert!(
        count_difference <= allowed_difference,
        "entry counts differ by more than 1%"
    );
    assert!(
        mismatches.is_empty(),
        "name/parent mismatches: {mismatches:?}"
    );
}
