/// Small deterministic SplitMix64 generator used by every fuzz target.
pub(crate) struct Rng {
    state: u64,
}

impl Rng {
    /// Starts a deterministic stream at `seed`.
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Returns the next 64 random bits.
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    /// Selects one index below `upper`, or zero for an empty range.
    pub(crate) fn index(&mut self, upper: usize) -> usize {
        if upper == 0 {
            0
        } else {
            (self.next_u64() % upper as u64) as usize
        }
    }

    /// Selects an inclusive integer range.
    pub(crate) fn range(&mut self, lower: usize, upper: usize) -> usize {
        lower + self.index(upper.saturating_sub(lower).saturating_add(1))
    }

    /// Returns one deterministic byte.
    pub(crate) fn byte(&mut self) -> u8 {
        self.next_u64() as u8
    }
}
