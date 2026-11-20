//! xorshift64*, so a whole search is reproducible from one integer.
//!
//! `search.js` uses `Math.random()`, so exact cross-implementation streams are
//! not comparable (V8 seeds itself from entropy). Reproducibility here is
//! per-implementation: same seed, same trace.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, n)`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// Uniform in `[0, 1)`, matching `Math.random()`'s use in `search.js`.
    pub fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) / ((1u64 << 53) as f64)
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}
