// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/zram_compression.rs
//! How well the compressed host-RAM tier is compressing.
//!
//! One definition of the kernel's zram `mm_stat` contract and of which swap
//! devices are Tier 1. The stress ramp folds it into a peak for the
//! qualification report; the cascade publishes it so the dashboard can show
//! the operator what compression is actually buying. Two definitions of that
//! kernel format would drift, so both consumers take it from here.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

use crate::cascade::{self, SwapTierRole};

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ZramCompression {
    /// Uncompressed size of the data the device currently holds, in bytes.
    pub orig_data_bytes: u64,
    /// Compressed size of that same data, in bytes.
    pub compr_data_bytes: u64,
    /// Memory the device has actually allocated for it, in bytes.
    ///
    /// This is what the data actually **costs** the host: it includes the
    /// device's own bookkeeping on top of `compr_data_bytes`, which is why it
    /// is the number an operator budgets against and why it is not the same
    /// quantity as the compressed size.
    pub mem_used_bytes: u64,
    /// Incompressible pages the device stored uncompressed. Each one is a
    /// page the compression ratio never got.
    ///
    /// `None` when the kernel did not report the column, and also when a
    /// counted device did not: a sum covering only the devices that happened
    /// to report the column is not a tier total. A device that does not answer
    /// `mm_stat` at all is not counted and blanks the whole sample instead —
    /// see [`zram_compression_from`]. `Some(0)` is a measured zero — every page
    /// compressed. The two are different facts and the dashboard must not print
    /// one as the other.
    pub huge_pages: Option<u64>,
    /// **Same-filled** pages the device stored as a fill reference rather than
    /// as a compressed body.
    ///
    /// Same-filled is an *internal* property of one page: every element of it
    /// holds the same value. It is **not** deduplication against other stored
    /// pages — zram does not do that. Two pages filled with different values
    /// (all zero and all `0xFF`) both land here and are not identical to each
    /// other. The kernel's name for the counter is "same element filled
    /// pages", and `page_same_filled` is the predicate.
    ///
    /// They are already counted in `orig_data_bytes` and contribute **nothing**
    /// to `compr_data_bytes`: the kernel increments `orig` for every stored
    /// page and increments `compr` only on the compressed path, and
    /// `write_same_filled_page` never allocates a compressed body. So the
    /// saving they represent is **already inside**
    /// `orig_data_bytes - compr_data_bytes`. Reading this count as a second
    /// saving and adding `same_pages * PAGE_SIZE` on top of that difference
    /// would count the same pages twice. It is published so a reader can see
    /// how much of the ratio came from the fill references, not so the reader
    /// can add it again.
    ///
    /// `None` when the kernel did not report the column, and also when a
    /// counted device did not: a sum covering only the devices that happened
    /// to report the column is not a tier total. A device that does not answer
    /// `mm_stat` at all is not counted and blanks the whole sample instead —
    /// see [`zram_compression_from`]. `Some(0)` is a measured zero — no page
    /// was same-filled. The two are different facts.
    pub same_pages: Option<u64>,
    /// The compressor the kernel says produced these bytes.
    ///
    /// Read from `comp_algorithm` — the kernel's answer, never the name the
    /// setup walk requested. `None` whenever there is no single name that
    /// describes every counted device: none reported one, counted devices
    /// disagreed, or a counted device left the answer unread while its bytes
    /// sat inside the totals. Publishing any of those would claim the tier is
    /// compressing under a name that does not cover the tier. A tier is
    /// normally one device, where those cases collapse to the same fact.
    pub compressor: Option<String>,
    // `pages_compacted` is deliberately not a field of this sample. It counts
    // movements the compaction allocator made — allocator health, not
    // compression quality — and publishing it beside the ratio would invite
    // reading it as a compression result. `mem_used_bytes` is already the
    // honest answer to "what does this cost the host".
}

/// The one derivation of a compression ratio: uncompressed over compressed.
///
/// `None` when either count is zero, because there is no ratio to report — a
/// `1.0` here would claim a measurement that did not happen. Every consumer
/// that has the two byte counts calls this rather than reading the derived
/// `ratio` the wire carries: a derived field can disagree with the counts it is
/// supposed to describe, and a row whose bar comes from the counts while its
/// number comes from the field can contradict itself on one line.
pub(crate) fn compression_ratio_of(orig_data_bytes: u64, compr_data_bytes: u64) -> Option<f64> {
    if orig_data_bytes == 0 || compr_data_bytes == 0 {
        return None;
    }
    Some(orig_data_bytes as f64 / compr_data_bytes as f64)
}

/// A pair of `mm_stat` byte counts the kernel cannot produce.
///
/// `orig_data_size` is the stored-page count scaled by the page size, and
/// every path that adds to `compr_data_size` also stores a page. A zero `orig`
/// beside a non-zero `compr` therefore describes no device state — it is a
/// tear between two counters or corruption in the body, and there is no
/// reading of it: not a ratio, not "perfect compression", not "holding
/// nothing".
///
/// Only this direction is a tear. The mirror pair, `orig > 0` with
/// `compr == 0`, is every stored page **same-filled** — the device holds fill
/// references and has allocated no compressed body — and is the most
/// compressed state the tier can be in. That pair is a real measurement and
/// must never be refused here.
pub(crate) fn unproducible_pair(orig_data_bytes: u64, compr_data_bytes: u64) -> bool {
    orig_data_bytes == 0 && compr_data_bytes > 0
}

/// Whether "every stored page was same-filled" is a claim this sample supports.
///
/// The byte pair is the primary source and is sufficient on its own under the
/// kernel's accounting. `mm_stat`'s `orig_data_size` is `pages_stored <<
/// PAGE_SHIFT` and a same-filled page increments `pages_stored`;
/// `compr_data_size` is incremented only on the compressed path, and
/// `write_same_filled_page` never touches it. So data held beside an empty
/// compressed form is same-filled by construction — not "probably", and not
/// "almost".
///
/// The `same_pages` counter is consulted when it was **measured**, because a
/// counter that reads zero contradicts that reading: the bytes say every
/// stored page was same-filled and the counter says none was. That sample
/// disagrees with itself and supports no classification. A counter the kernel
/// did not report (`None`) leaves the byte pair's reading standing — `None` is
/// "not measured", never a measured zero, which is this product's rule for
/// every unreported counter.
pub(crate) fn all_stored_pages_same_filled(
    orig_data_bytes: u64,
    compr_data_bytes: u64,
    same_pages: Option<u64>,
) -> bool {
    orig_data_bytes > 0 && compr_data_bytes == 0 && same_pages != Some(0)
}

impl ZramCompression {
    /// Uncompressed over compressed size. `None` when the device holds
    /// nothing, because there is no ratio to report — a `1.0` here would
    /// claim a measurement that did not happen.
    pub fn compression_ratio(&self) -> Option<f64> {
        compression_ratio_of(self.orig_data_bytes, self.compr_data_bytes)
    }

    /// The one published shape of a compression sample.
    ///
    /// Every consumer goes through [`Serialize`], so the stress report, the
    /// cascade status document and this helper cannot drift into three
    /// spellings of the same sample. `ratio` is derived at serialization time
    /// rather than stored: a stored ratio can disagree with the two byte
    /// counts it is supposed to describe.
    ///
    /// What serializes as `null` and what serializes as `0` are different
    /// claims. `ratio`, `huge_pages`, `same_pages` and `compressor` are `null`
    /// when they were not measured — including when a counted device left the
    /// tier total incomplete. The three byte counts are bare `u64` and
    /// read `0` for a device that is holding nothing — a real measurement of
    /// emptiness.
    /// "Not measured at all" is carried by the surrounding
    /// `Option<ZramCompression>`, which is `null` when the tier had no sample.
    pub fn to_status_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "null".to_string())
    }
}

impl Serialize for ZramCompression {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ZramCompression", 7)?;
        state.serialize_field("orig_data_bytes", &self.orig_data_bytes)?;
        state.serialize_field("compr_data_bytes", &self.compr_data_bytes)?;
        state.serialize_field("mem_used_bytes", &self.mem_used_bytes)?;
        state.serialize_field("huge_pages", &self.huge_pages)?;
        state.serialize_field("same_pages", &self.same_pages)?;
        state.serialize_field("compressor", &self.compressor)?;
        state.serialize_field("ratio", &self.compression_ratio())?;
        state.end()
    }
}

/// Sum the `mm_stat` of every Tier 1 swap device on this host.
///
/// A sample only means something while the pressure that created it is still
/// resident: a read taken after the ramp drops its buffers, or after a
/// teardown resets the device, measures nothing.
pub fn read_zram_compression() -> Option<ZramCompression> {
    let swaps = fs::read_to_string("/proc/swaps").ok()?;
    zram_compression_from(Path::new("/sys/block"), &swaps)
}

/// The reader's pure core: resolve Tier 1 swap devices, sum their `mm_stat`.
///
/// The sum is over **every** Tier 1 device the swap table names, or it does not
/// exist. A named device whose `mm_stat` will not read, whose body does not
/// parse, or whose body parses to a pair the kernel cannot produce has not
/// answered, and a total over the devices that did is the partial sum the
/// counter columns already refuse under the same name. That is why the silence
/// path is `None` for the whole sample rather than a skip: the three byte
/// counts are bare `u64` and cannot be demoted field by field, and none of the
/// other fields would be complete either.
fn zram_compression_from(sys_block: &Path, swaps: &str) -> Option<ZramCompression> {
    let mut total = ZramCompression::default();
    let mut seen = false;
    // `Some(name)` once a device reported one, `None` before any did, and the
    // held name is dropped to `None` on a disagreement — see `compressor`.
    let mut compressor: Option<String> = None;
    let mut compressor_agrees = true;
    // A counted device that left the algorithm unread cannot be described by
    // whatever name its peers reported: its bytes are inside the totals. Same
    // rule as the counter columns, and the incomplete mark is not cleared by a
    // later device that did report.
    let mut compressor_incomplete = false;
    // A counted device that omitted `huge_pages` or `same_pages` makes that
    // tier total unmeasurable. `None` is the product's "not measured", never a
    // measured zero and never the sum of whichever devices happened to answer.
    let mut huge_pages_incomplete = false;
    let mut same_pages_incomplete = false;
    for device in zram_swap_devices(swaps) {
        // The two byte counts come from one `mm_stat` read and cannot disagree
        // with each other. The compressor name is a second file, so it is the
        // one fact this reader can still tear: a zram reset between the two
        // reads would leave the bytes describing one incarnation of the device
        // and the name describing another. Read the name before and after
        // `mm_stat` and keep it only when both reads agree — see
        // [`agreed_compressor`].
        let algorithm_before = read_active_zram_algorithm(sys_block, &device);
        // A Tier 1 device the swap table names and whose `mm_stat` will not
        // read has not answered. Skipping it would publish the survivors'
        // bytes as the tier total — the same partial sum the incomplete marks
        // below refuse — and would even let a column the silent device never
        // reported stop demoting, by ceasing to count as a counted device.
        // A body that does not parse is not an answer either.
        let Ok(text) = fs::read_to_string(sys_block.join(&device).join("mm_stat")) else {
            return None;
        };
        let mut sample = parse_zram_mm_stat(&text)?;
        // A body that parses to a pair the kernel cannot produce has not
        // answered either. Its bytes cannot enter a total without inventing a
        // reading the tear does not have, and the display layer already
        // refuses the same pair as `not measured` — see [`unproducible_pair`].
        // Summing it would publish a plausible-looking ratio built on one
        // device that describes no state beside another that does.
        if unproducible_pair(sample.orig_data_bytes, sample.compr_data_bytes) {
            return None;
        }
        // The kernel's answer to "which compressor", not the name the setup
        // walk asked for. Absent here is `None` — a sample that does not know
        // its compressor is not a sample under `lzo-rle` by default, and a
        // name that changed under the read is not a name for the bytes held.
        sample.compressor = agreed_compressor(
            algorithm_before,
            read_active_zram_algorithm(sys_block, &device),
        );
        total.orig_data_bytes = total.orig_data_bytes.saturating_add(sample.orig_data_bytes);
        total.compr_data_bytes = total
            .compr_data_bytes
            .saturating_add(sample.compr_data_bytes);
        total.mem_used_bytes = total.mem_used_bytes.saturating_add(sample.mem_used_bytes);
        // A counted device that omitted the column leaves the tier total
        // unmeasurable. Summing only the devices that did report would publish
        // a partial count under a name that claims the whole tier, and the
        // incomplete mark is not cleared by a later device that did report.
        if let Some(more) = sample.huge_pages {
            total.huge_pages = Some(total.huge_pages.unwrap_or(0).saturating_add(more));
        } else {
            huge_pages_incomplete = true;
        }
        if let Some(more) = sample.same_pages {
            total.same_pages = Some(total.same_pages.unwrap_or(0).saturating_add(more));
        } else {
            same_pages_incomplete = true;
        }
        match sample.compressor {
            Some(name) => match &compressor {
                None => compressor = Some(name),
                Some(held) if *held == name => {}
                // Two counted devices are compressing under different
                // algorithms. There is no single name that describes the tier,
                // and picking one would say the other device is using it too.
                Some(_) => compressor_agrees = false,
            },
            None => compressor_incomplete = true,
        }
        seen = true;
    }
    if !seen {
        return None;
    }
    if huge_pages_incomplete {
        total.huge_pages = None;
    }
    if same_pages_incomplete {
        total.same_pages = None;
    }
    // A name is published only when it describes every counted device: the
    // devices agreed, and none of them left the answer unread. A name covering
    // only the devices that happened to report is not a tier name.
    total.compressor = if compressor_agrees && !compressor_incomplete {
        compressor
    } else {
        None
    };
    Some(total)
}

/// Basenames of the swap devices [`cascade::swap_tier_role`] classifies as
/// Tier 1, each one exactly once.
///
/// The basename is the `/sys/block` key the fold reads `mm_stat` through, so
/// it is the device's identity here — not the `/proc/swaps` path that named
/// it. Two rows can share it: the same device listed twice, or two spellings
/// of one name (`/dev/zram0` beside the bare `zram0` the path predicates also
/// accept). Reading `mm_stat` once per row would sum one device's bytes twice,
/// doubling every published figure while the ratio still looks plausible,
/// because both sides of the ratio double together. A set is what makes that
/// unrepresentable.
///
/// Columns are read from the end, the way the cascade's strict
/// `parse_proc_swaps` does, so a filename containing spaces (a ghost
/// `(deleted)` spelling) does not shift the name. Malformed rows are skipped
/// rather than aborting the fold: this is a measurement, not a safety proof.
///
/// A ghost row is skipped before its basename can become a `/sys/block` key.
/// The role predicates classify `/dev/zram0 (deleted)` as Tier 1 because they
/// strip the ghost spelling to find the device class, which is right for role
/// attribution and wrong for a sysfs lookup: `zram0 (deleted)` is not a
/// directory the kernel publishes. Skipping here states that rather than
/// relying on the `mm_stat` open to fail later.
fn zram_swap_devices(swaps: &str) -> BTreeSet<String> {
    swaps
        .lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let n = cols.len();
            if n < 5 {
                return None;
            }
            let name = cols[..n - 4].join(" ");
            if name.is_empty()
                || cascade::is_ghost_swap_filename(&name)
                || cascade::swap_tier_role(&name) != SwapTierRole::Zram
            {
                return None;
            }
            Path::new(&name)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .collect()
}

/// The compressor the kernel reports as active on one device, or `None` when
/// the device leaves the answer unreadable or unparsable.
///
/// This is a read of `comp_algorithm` alone. The decision to keep what it
/// returns is [`agreed_compressor`], which needs two of these.
fn read_active_zram_algorithm(sys_block: &Path, device: &str) -> Option<String> {
    let text = fs::read_to_string(sys_block.join(device).join("comp_algorithm")).ok()?;
    cascade::parse_active_zram_algorithm(&text).map(str::to_string)
}

/// Keep a compressor name only when two reads of the same device agreed.
///
/// `mm_stat` and `comp_algorithm` are two files, so they are the one pair this
/// reader can tear: a zram reset between the reads leaves the byte counts
/// describing one incarnation and the name describing another. Both reads
/// disagreeing is therefore the same refusal as either one being unreadable —
/// the device is counted, its bytes are inside the totals, and no name
/// describes them. `None` never becomes a default compressor.
fn agreed_compressor(before: Option<String>, after: Option<String>) -> Option<String> {
    match (before, after) {
        (Some(before), Some(after)) if before == after => Some(before),
        _ => None,
    }
}

/// Parse one kernel `mm_stat` line.
///
/// Field order is the kernel's and is long-stable:
/// `orig_data_size compr_data_size mem_used_total mem_limit mem_used_max
/// same_pages pages_compacted huge_pages huge_pages_since`.
/// The three leading columns are what the ratio needs and are required; the
/// rest are tolerated so a kernel that adds or omits a trailing column still
/// parses. `same_pages` and `huge_pages` are each `None` when their column is
/// absent or unreadable — an unavailable counter is not a measured zero, and
/// neither feeds the ratio. `pages_compacted` is read past and dropped.
fn parse_zram_mm_stat(text: &str) -> Option<ZramCompression> {
    let mut fields = text.split_whitespace();
    let orig_data_bytes = fields.next()?.parse::<u64>().ok()?;
    let compr_data_bytes = fields.next()?.parse::<u64>().ok()?;
    let mem_used_bytes = fields.next()?.parse::<u64>().ok()?;
    // mem_limit and mem_used_max are allocator ceilings, not compression
    // results. `same_pages` is the sixth column; `pages_compacted` is the
    // seventh and is skipped on purpose — see the field note on the struct.
    let same_pages = fields.nth(2).and_then(|value| value.parse::<u64>().ok());
    let huge_pages = fields.nth(1).and_then(|value| value.parse::<u64>().ok());
    Some(ZramCompression {
        orig_data_bytes,
        compr_data_bytes,
        mem_used_bytes,
        huge_pages,
        same_pages,
        // `mm_stat` does not name the compressor; the caller fills this from
        // `comp_algorithm`. Leaving it `None` here is what keeps the two reads
        // independent rather than one format pretending to carry both.
        compressor: None,
    })
}

#[cfg(test)]
mod tests;
