//! Borrowed rank/select over an LSB-first bitmap.
//!
//! Rank samples store cumulative popcounts every 256 bits, bounding a rank
//! query to four `u64` popcounts. Select uses binary search over positions
//! backed by rank. A larger select-sampling index could make select
//! constant-time, but would spend persistent space to save nanoseconds on
//! operations that occur only while walking a tree path.
//!
//! The sampling rate is a space decision, not a speed one. A `u32` sample per
//! 16 bits — the original rate — costs two bytes of index per two bytes of
//! bitmap: 200% overhead, or ~680 KB of samples over the 340 KB `dir_bits` of
//! a two-million-record volume. At 256 bits the same index is ~21 KB, and the
//! extra scan work is four popcounts on one or two cache lines that rank was
//! already going to touch.

pub const SUPERBLOCK_BITS: usize = 256;

pub fn build_superblocks(bits: &[u8]) -> Vec<u32> {
    let mut samples = Vec::with_capacity(bits.len().div_ceil(64) + 1);
    let mut cumulative = 0u32;
    for chunk in bits.chunks(SUPERBLOCK_BITS / 8) {
        samples.push(cumulative);
        cumulative += chunk.iter().map(|byte| byte.count_ones()).sum::<u32>();
    }
    samples.push(cumulative);
    samples
}

#[derive(Clone, Copy)]
pub struct RankSelect<'a> {
    bits: &'a [u8],
    superblocks: &'a [u32],
}

impl<'a> RankSelect<'a> {
    pub fn new(bits: &'a [u8], superblocks: &'a [u32]) -> Self {
        Self { bits, superblocks }
    }

    /// Number of set bits strictly before position `i`.
    pub fn rank1(&self, i: usize) -> usize {
        let end = i.min(self.bits.len() * 8);
        let superblock = end / SUPERBLOCK_BITS;
        let mut count = self.superblocks.get(superblock).copied().unwrap_or(0) as usize;
        let start_byte = superblock * (SUPERBLOCK_BITS / 8);
        let full_bytes = end / 8;
        let bytes = &self.bits[start_byte.min(self.bits.len())..full_bytes.min(self.bits.len())];
        // Whole `u64`s first: at a 256-bit sampling rate the span between a
        // sample and the query position is up to 32 bytes, which is four
        // popcounts this way versus thirty-two byte-wise.
        let (words, tail) = bytes.split_at(bytes.len() & !7);
        for word in words.chunks_exact(8) {
            count += u64::from_le_bytes(word.try_into().expect("chunks_exact(8) yields 8 bytes"))
                .count_ones() as usize;
        }
        count += tail
            .iter()
            .map(|byte| byte.count_ones() as usize)
            .sum::<usize>();
        let remainder = end % 8;
        if remainder != 0 && full_bytes < self.bits.len() {
            count += (self.bits[full_bytes] & ((1u8 << remainder) - 1)).count_ones() as usize;
        }
        count
    }

    pub fn rank0(&self, i: usize) -> usize {
        i.min(self.bits.len() * 8) - self.rank1(i)
    }

    pub fn select1(&self, k: usize) -> Option<usize> {
        self.select(k, true)
    }

    pub fn select0(&self, k: usize) -> Option<usize> {
        self.select(k, false)
    }

    fn select(&self, k: usize, one: bool) -> Option<usize> {
        let len = self.bits.len() * 8;
        let total = if one {
            self.rank1(len)
        } else {
            self.rank0(len)
        };
        if k >= total {
            return None;
        }
        let rank = |position| {
            if one {
                self.rank1(position)
            } else {
                self.rank0(position)
            }
        };
        let mut low = 0usize;
        let mut high = len;
        while low < high {
            let middle = low + (high - low) / 2;
            if rank(middle + 1) > k {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        Some(low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated() -> Vec<u8> {
        let mut bits = vec![0u8; 100_000 / 8];
        let mut state = 0x1234_5678u32;
        for position in 0..100_000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if state & 3 != 0 {
                bits[position / 8] |= 1 << (position % 8);
            }
        }
        bits
    }

    #[test]
    fn rank1_matches_naive_count() {
        let bits = generated();
        let samples = build_superblocks(&bits);
        let rs = RankSelect::new(&bits, &samples);
        let positions = [0, 1, 511, 512, 513, 100_000];
        for position in positions
            .into_iter()
            .chain((0..1_000).map(|i| i * 99 % 100_001))
        {
            let naive = (0..position)
                .filter(|&bit| bits[bit / 8] & (1 << (bit % 8)) != 0)
                .count();
            assert_eq!(rs.rank1(position), naive, "position {position}");
        }
    }

    #[test]
    fn select_is_inverse_of_rank() {
        let bits = generated();
        let samples = build_superblocks(&bits);
        let rs = RankSelect::new(&bits, &samples);
        for k in 0..rs.rank1(100_000) {
            assert_eq!(rs.rank1(rs.select1(k).unwrap()), k);
        }
        for k in 0..rs.rank0(100_000) {
            assert_eq!(rs.rank0(rs.select0(k).unwrap()), k);
        }
    }

    #[test]
    fn select_past_end_returns_none() {
        let bits = generated();
        let samples = build_superblocks(&bits);
        let rs = RankSelect::new(&bits, &samples);
        assert_eq!(rs.select1(rs.rank1(100_000)), None);
        assert_eq!(rs.select0(rs.rank0(100_000)), None);
    }

    #[test]
    fn rank_on_empty_bitvector() {
        let samples = build_superblocks(&[]);
        let rs = RankSelect::new(&[], &samples);
        assert_eq!(rs.rank1(0), 0);
        assert_eq!(rs.rank0(0), 0);
        assert_eq!(rs.select1(0), None);
        assert_eq!(rs.select0(0), None);
    }
}
