// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/cache_group/routing.rs
//! Immutable ownership of logical cache chunks, independent of live capacity.
pub(super) struct Routing {
    chunk_bytes: u64,
    ends: Vec<u64>,
    total: u64,
}
impl Routing {
    pub(super) fn new(targets: &[u64], chunk_bytes: u64) -> Result<Self, &'static str> {
        if targets.is_empty() || chunk_bytes == 0 || targets.contains(&0) {
            return Err("cache routing requires positive members and chunk geometry");
        }
        let (mut total, mut bytes) = (0u64, 0u64);
        let mut ends = Vec::new();
        ends.try_reserve_exact(targets.len())
            .map_err(|_| "cache routing metadata allocation failed")?;
        for target in targets {
            bytes = bytes
                .checked_add(*target)
                .ok_or("cache target sum overflow")?;
            total = total
                .checked_add((target / chunk_bytes).max(1))
                .ok_or("cache routing weight overflow")?;
            ends.push(total);
        }
        Ok(Self {
            chunk_bytes,
            ends,
            total,
        })
    }
    pub(super) fn locate(&self, offset: u64, remaining: usize) -> (usize, usize) {
        let position = (offset / self.chunk_bytes) % self.total;
        let member = self.ends.partition_point(|end| *end <= position);
        if self.ends.len() == 1 {
            return (member, remaining);
        }
        // The remaining run is at most this member's own weight. Its byte
        // product fits the configured target, or one valid chunk.
        let bytes = (self.ends[member] - position) * self.chunk_bytes - offset % self.chunk_bytes;
        (member, bytes.min(remaining as u64) as usize)
    }
}
