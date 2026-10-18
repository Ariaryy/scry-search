//! Exact extension counts with bounded heap and temporary sorted runs. Keys
//! stay private, never appear in reports, and are deleted on every exit path.
use anyhow::{ensure, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub struct ExtensionCounts<'a> {
    dir: &'a Path,
    counts: BTreeMap<Vec<u8>, u64>,
    estimated_bytes: usize,
    budget: usize,
    levels: Vec<Option<PathBuf>>,
    sequence: usize,
}

impl<'a> ExtensionCounts<'a> {
    pub fn new(dir: &'a Path, budget: usize) -> Self {
        Self {
            dir,
            budget,
            counts: BTreeMap::new(),
            estimated_bytes: 0,
            levels: Vec::new(),
            sequence: 0,
        }
    }

    pub fn add(&mut self, extension: &[u8]) -> Result<()> {
        let key: Vec<u8> = extension.iter().map(u8::to_ascii_lowercase).collect();
        if !self.counts.contains_key(&key) {
            // Conservative allowance for Vec capacity, B-tree slots and nodes.
            self.estimated_bytes += key.len() + 256;
        }
        *self.counts.entry(key).or_default() += 1;
        if self.estimated_bytes >= self.budget {
            self.flush()?;
        }
        Ok(())
    }

    fn path(&mut self) -> PathBuf {
        self.sequence += 1;
        self.dir.join(format!("extensions-{}.run", self.sequence))
    }

    fn flush(&mut self) -> Result<()> {
        if self.counts.is_empty() {
            return Ok(());
        }
        let path = self.path();
        let mut out = BufWriter::new(File::create(&path)?);
        for (key, count) in std::mem::take(&mut self.counts) {
            write_entry(&mut out, &key, count)?;
        }
        out.flush()?;
        drop(out);
        self.estimated_bytes = 0;
        self.insert_run(path, 0)
    }

    fn insert_run(&mut self, mut path: PathBuf, mut level: usize) -> Result<()> {
        loop {
            ensure!(level < 64, "too many extension runs");
            if level == self.levels.len() {
                self.levels.push(None);
            }
            let Some(previous) = self.levels[level].take() else {
                self.levels[level] = Some(path);
                return Ok(());
            };
            let merged = self.path();
            merge(&previous, &path, &merged)?;
            std::fs::remove_file(previous)?;
            std::fs::remove_file(path)?;
            path = merged;
            level += 1;
        }
    }

    pub fn finish(mut self) -> Result<(u64, u64)> {
        self.flush()?;
        let mut final_run: Option<PathBuf> = None;
        for level in 0..self.levels.len() {
            let Some(run) = self.levels[level].take() else {
                continue;
            };
            final_run = Some(if let Some(previous) = final_run {
                let merged = self.path();
                merge(&previous, &run, &merged)?;
                std::fs::remove_file(previous)?;
                std::fs::remove_file(run)?;
                merged
            } else {
                run
            });
        }
        let Some(path) = final_run else {
            return Ok((0, 0));
        };
        let mut reader = BufReader::new(File::open(path)?);
        let mut distinct = 0;
        let mut top = [0u64; 10];
        while let Some((_, count)) = read_entry(&mut reader)? {
            distinct += 1;
            if count > top[0] {
                top[0] = count;
                top.sort_unstable();
            }
        }
        Ok((distinct, top.iter().sum()))
    }
}

fn write_entry(out: &mut impl Write, key: &[u8], count: u64) -> Result<()> {
    out.write_all(&(key.len() as u32).to_le_bytes())?;
    out.write_all(key)?;
    out.write_all(&count.to_le_bytes())?;
    Ok(())
}

fn read_entry(input: &mut impl Read) -> Result<Option<(Vec<u8>, u64)>> {
    let mut length = [0u8; 4];
    if input.read(&mut length[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut length[1..])?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(length <= 65_536, "invalid extension run length");
    let mut key = vec![0; length];
    input.read_exact(&mut key)?;
    let mut count = [0; 8];
    input.read_exact(&mut count)?;
    Ok(Some((key, u64::from_le_bytes(count))))
}

fn merge(a: &Path, b: &Path, output: &Path) -> Result<()> {
    let (mut a, mut b) = (
        BufReader::new(File::open(a)?),
        BufReader::new(File::open(b)?),
    );
    let mut out = BufWriter::new(File::create(output)?);
    let (mut left, mut right) = (read_entry(&mut a)?, read_entry(&mut b)?);
    // Each iteration consumes at least one entry from finite input files.
    while left.is_some() || right.is_some() {
        match (&left, &right) {
            (Some((key, count)), Some((other, other_count))) if key == other => {
                write_entry(
                    &mut out,
                    key,
                    count
                        .checked_add(*other_count)
                        .ok_or_else(|| anyhow::anyhow!("extension count overflow"))?,
                )?;
                left = read_entry(&mut a)?;
                right = read_entry(&mut b)?;
            }
            (Some((key, count)), _) if right.as_ref().is_none_or(|(other, _)| key < other) => {
                write_entry(&mut out, key, *count)?;
                left = read_entry(&mut a)?;
            }
            (_, Some((key, count))) => {
                write_entry(&mut out, key, *count)?;
                right = read_entry(&mut b)?;
            }
            _ => unreachable!(),
        }
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spilled_counts_equal_in_memory_counts_and_merge_case_variants() {
        let temp = tempfile::tempdir().unwrap();
        for budget in [1, 500, 4 * 1024 * 1024] {
            let mut counts = ExtensionCounts::new(temp.path(), budget);
            for _ in 0..5 {
                counts.add(b"TXT").unwrap();
                counts.add(b"txt").unwrap();
            }
            for i in 0..20 {
                for _ in 0..i + 1 {
                    counts.add(format!("type{i:02}").as_bytes()).unwrap();
                }
            }
            assert_eq!(counts.finish().unwrap(), (21, (11..=20).sum()));
        }
    }
}
