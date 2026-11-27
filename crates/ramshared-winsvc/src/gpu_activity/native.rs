// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_activity/native.rs
//! Sample the Windows GPU Engine counter without opening a CUDA context.
use super::counters::summarize;
use ramshared_vram::host_gpu_activity::{HostGpuActivityReport, HostGpuActivitySource};
use std::{
    io, ptr, slice, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
    PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PDH_NO_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};

const MAX_COUNTER_BYTES: usize = 4 * 1024 * 1024;
const MAX_NAME_UNITS: usize = 128;
type CounterItem = PDH_FMT_COUNTERVALUE_ITEM_W;
const _: () = assert!(align_of::<CounterItem>() <= align_of::<u64>());

fn check(status: u32, operation: &str) -> io::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{operation}: PDH status 0x{status:08x}"
        )))
    }
}

struct Query(PDH_HQUERY);
impl Drop for Query {
    fn drop(&mut self) {
        // SAFETY: this object owns the handle returned by PdhOpenQueryW.
        unsafe { PdhCloseQuery(self.0) };
    }
}

impl Query {
    fn open() -> io::Result<Self> {
        let mut query = ptr::null_mut();
        // SAFETY: live writable handle output. A null source selects the local machine.
        check(
            unsafe { PdhOpenQueryW(ptr::null(), 0, &mut query) },
            "open query",
        )?;
        Ok(Self(query))
    }

    fn collect(&self) -> io::Result<()> {
        // SAFETY: self owns a live query.
        check(unsafe { PdhCollectQueryData(self.0) }, "collect GPU Engine")
    }
}

fn counter_rows(counter: PDH_HCOUNTER) -> io::Result<Vec<(String, f64)>> {
    for _ in 0..2 {
        let mut size = 0;
        let mut count = 0;
        // SAFETY: the documented sizing call uses a null buffer and live outputs.
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                ptr::null_mut(),
            )
        };
        if status == PDH_NO_DATA || (status == 0 && size == 0) {
            return Ok(Vec::new());
        }
        if status != PDH_MORE_DATA {
            check(status, "size GPU Engine counters")?;
        }
        let capacity = size as usize;
        if capacity == 0 || capacity > MAX_COUNTER_BYTES {
            return Err(io::Error::other("GPU Engine counters exceed buffer bounds"));
        }
        let mut buffer = vec![0u64; capacity.div_ceil(size_of::<u64>())];
        let items = buffer.as_mut_ptr().cast::<CounterItem>();
        // SAFETY: buffer is writable, aligned and at least size bytes long.
        let status = unsafe {
            PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, items)
        };
        if status == PDH_MORE_DATA {
            continue;
        }
        if status == PDH_NO_DATA {
            return Ok(Vec::new());
        }
        check(status, "read GPU Engine counters")?;
        let used = size as usize;
        let count = count as usize;
        if used > capacity || count > used / size_of::<CounterItem>() {
            return Err(io::Error::other("invalid GPU Engine item buffer"));
        }
        // SAFETY: successful PDH output contains count aligned items inside used bytes.
        let items = unsafe { slice::from_raw_parts(items, count) };
        let strings_from = size_of_val(items);
        let mut rows = Vec::with_capacity(count);
        for item in items {
            let offset = (item.szName as usize)
                .checked_sub(buffer.as_ptr() as usize)
                .filter(|offset| *offset >= strings_from && *offset < used && *offset % 2 == 0)
                .ok_or_else(|| io::Error::other("GPU Engine name is outside its buffer"))?;
            let length = ((used - offset) / 2).min(MAX_NAME_UNITS);
            // SAFETY: derive the aligned pointer from the owned allocation and
            // bound the slice to bytes that PDH reported inside that allocation.
            let units = unsafe {
                slice::from_raw_parts(
                    buffer.as_ptr().cast::<u8>().add(offset).cast::<u16>(),
                    length,
                )
            };
            let end = units
                .iter()
                .position(|unit| *unit == 0)
                .ok_or_else(|| io::Error::other("GPU Engine name has no bounded terminator"))?;
            let name = String::from_utf16(&units[..end]).map_err(io::Error::other)?;
            let value = if matches!(
                item.FmtValue.CStatus,
                PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
            ) {
                // SAFETY: the query requests PDH_FMT_DOUBLE and the item status is valid.
                unsafe { item.FmtValue.Anonymous.doubleValue }
            } else {
                f64::NAN
            };
            rows.push((name, value));
        }
        return Ok(rows);
    }
    Err(io::Error::other(
        "GPU Engine counters changed during both reads",
    ))
}

pub(super) fn sample() -> io::Result<HostGpuActivityReport> {
    let query = Query::open()?;
    let path = r"\GPU Engine(*)\Utilization Percentage"
        .encode_utf16()
        .chain([0])
        .collect::<Vec<_>>();
    let mut counter = ptr::null_mut();
    // SAFETY: query and NUL-terminated path stay live, with one writable output.
    check(
        unsafe { PdhAddEnglishCounterW(query.0, path.as_ptr(), 0, &mut counter) },
        "add GPU Engine utilization counter",
    )?;
    query.collect()?;
    let first = Instant::now();
    thread::sleep(Duration::from_secs(1));
    query.collect()?;
    let interval_ms = u64::try_from(first.elapsed().as_millis())
        .ok()
        .filter(|value| (1..=10_000).contains(value))
        .ok_or_else(|| io::Error::other("invalid GPU Engine sampling interval"))?;
    let sampled_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .filter(|value| *value != 0)
        .ok_or_else(|| io::Error::other("invalid GPU Engine timestamp"))?;
    let rows = counter_rows(counter)?;
    let report = HostGpuActivityReport {
        schema_version: 2,
        source: HostGpuActivitySource::WddmPdh,
        sampled_at_unix_ms,
        interval_ms,
        adapters: summarize(rows.iter().map(|(name, value)| (name.as_str(), *value))),
    };
    report.validate().map_err(io::Error::other)?;
    Ok(report)
}
