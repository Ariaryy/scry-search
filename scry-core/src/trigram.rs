//! Fixed dense trigram-to-block presence matrix.
//!
//! At roughly 2 MiB per million records, a dense matrix is predictable,
//! zero-copy, and needs no decoding or extra dependency. A compressed bitmap
//! would save little while adding container heuristics and query-time work.

pub const TRIGRAM_BITS: usize = 14;
pub const TRIGRAM_ROWS: usize = 1 << TRIGRAM_BITS;
pub const TRIGRAM_BLOCK: usize = 1024;

/// Maps three ASCII-lowercased bytes into the 14-bit row space.
#[inline]
pub fn trigram_hash(a: u8, b: u8, c: u8) -> u16 {
    let k = (a.to_ascii_lowercase() as u32) << 16
        | (b.to_ascii_lowercase() as u32) << 8
        | c.to_ascii_lowercase() as u32;
    ((k.wrapping_mul(2_654_435_761) >> 18) & (TRIGRAM_ROWS as u32 - 1)) as u16
}

/// Calls `f` with the hash of every 3-byte window in `name`.
pub fn for_each_trigram(name: &[u8], mut f: impl FnMut(u16)) {
    for bytes in name.windows(3) {
        f(trigram_hash(bytes[0], bytes[1], bytes[2]));
    }
}

pub fn row_bytes(blocks: usize) -> usize {
    blocks.div_ceil(8)
}

pub fn num_blocks(records: usize) -> usize {
    records.div_ceil(TRIGRAM_BLOCK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigram_hash_is_case_insensitive() {
        assert_eq!(
            trigram_hash(b'A', b'B', b'C'),
            trigram_hash(b'a', b'b', b'c')
        );
    }

    #[test]
    fn trigram_hash_is_in_range() {
        let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";
        for &a in alphabet {
            for &b in alphabet {
                for &c in alphabet {
                    assert!((trigram_hash(a, b, c) as usize) < TRIGRAM_ROWS);
                }
            }
        }
    }

    #[test]
    fn for_each_trigram_yields_n_minus_2() {
        for (name, expected) in [(b"0123456789".as_slice(), 8), (b"ab", 0), (b"", 0)] {
            let mut count = 0;
            for_each_trigram(name, |_| count += 1);
            assert_eq!(count, expected);
        }
    }

    #[test]
    fn row_bytes_and_num_blocks_round_numbers() {
        assert_eq!(num_blocks(0), 0);
        assert_eq!(num_blocks(1), 1);
        assert_eq!(num_blocks(1024), 1);
        assert_eq!(num_blocks(1025), 2);
        assert_eq!(row_bytes(1), 1);
        assert_eq!(row_bytes(8), 1);
        assert_eq!(row_bytes(9), 2);
    }
}
