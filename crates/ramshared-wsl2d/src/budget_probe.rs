// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/budget_probe.rs
//! Context-free metadata query in a separately owned, lease-bound native process.
use ramshared_ipc::budget_observation::{LEASE_PATH, MAX_REPORT_BYTES, Purpose, Report};
use ramshared_vram::{GpuBudgetSnapshot, GpuBudgetTelemetry};
use std::io::Write;
use std::os::fd::AsFd;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(super) fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    emit(
        args,
        rustix::process::geteuid().as_raw(),
        || {
            inherit(
                std::io::stdin().as_fd(),
                std::path::Path::new(LEASE_PATH),
                0,
            )
        },
        || ramshared_block::platform::probe_platform().platform,
        discover,
        std::io::stdout(),
    )
}
fn inherit(
    input: std::os::fd::BorrowedFd<'_>,
    path: &std::path::Path,
    owner: u32,
) -> Result<std::fs::File, Box<dyn std::error::Error>> {
    let lease = std::fs::File::from(rustix::io::dup(input)?);
    ramshared_exec::inherited_lease::validate(&lease, path, owner)?;
    Ok(lease)
}
fn emit(
    args: &[String],
    uid: u32,
    acquire: impl FnOnce() -> Result<std::fs::File, Box<dyn std::error::Error>>,
    platform: impl FnOnce() -> Option<ramshared_block::platform::PlatformKind>,
    discover: impl FnOnce(u64) -> Result<Vec<GpuBudgetTelemetry>, String>,
    mut output: impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    if !args.is_empty() || uid != 0 {
        return Err("budget probe requires its root service owner and no arguments".into());
    }
    let lease = acquire()?;
    let started = Instant::now();
    let stamp = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
    let mut report = Report {
        schema_version: 1,
        purpose: Purpose::NativeLinuxPlanningObservation,
        pid: std::process::id(),
        started_unix_ms: stamp,
        elapsed_ms: 0,
        budgets: vec![],
        unavailable_reason: None,
    };
    if platform() != Some(ramshared_block::platform::PlatformKind::NativeLinux) {
        report.unavailable_reason = Some("native Off observations require measured native Linux; WSL allocator mapping is unavailable".into());
    } else {
        report.budgets = discover(stamp)?;
        if report.budgets.is_empty() {
            report.unavailable_reason = Some("no fresh scoped driver budget is available".into());
        }
    }
    report.elapsed_ms = u64::try_from(started.elapsed().as_millis())?;
    let bytes = serde_json::to_vec(&report)?;
    if bytes.len() > MAX_REPORT_BYTES {
        return Err("native budget report exceeds byte bound".into());
    }
    report.validate(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)?;
    output.write_all(&bytes)?;
    drop(lease);
    Ok(())
}
trait MetadataInventory {
    fn count(&self) -> Option<u32>;
    fn budget(&self, ordinal: u32) -> Option<GpuBudgetSnapshot>;
}
impl MetadataInventory for ramshared_cuda::Cuda {
    fn count(&self) -> Option<u32> {
        self.device_count().ok()?.try_into().ok()
    }
    fn budget(&self, ordinal: u32) -> Option<GpuBudgetSnapshot> {
        self.device_budget_snapshot(&self.device(ordinal.try_into().ok()?).ok()?)
            .ok()
    }
}
impl MetadataInventory for ramshared_vulkan::VulkanInventory {
    fn count(&self) -> Option<u32> {
        Some(self.device_count())
    }
    fn budget(&self, ordinal: u32) -> Option<GpuBudgetSnapshot> {
        let observation = self.observe(ordinal).ok()?;
        observation.hardware_gpu.then_some(observation.budget)
    }
}
struct Discovery {
    stamp: u64,
    encoded_bytes: usize,
    budgets: Vec<GpuBudgetTelemetry>,
}
impl Discovery {
    fn new(stamp: u64) -> Self {
        Self {
            stamp,
            encoded_bytes: 2,
            budgets: Vec::new(),
        }
    }
    fn collect<I, B, F>(&mut self, inventory: Option<I>, mut open: F) -> Result<(), String>
    where
        I: MetadataInventory,
        B: ramshared_dxg::GpuBudgetProvider,
        F: FnMut(ramshared_dxg::BudgetRequest) -> Result<B, ramshared_dxg::DxgError>,
    {
        let Some(inventory) = inventory else {
            return Ok(());
        };
        let Some(count) = inventory.count() else {
            return Ok(());
        };
        for ordinal in 0..count {
            let Some(snapshot) = inventory.budget(ordinal) else {
                continue;
            };
            let Ok(snapshot) = super::gpu_worker_selection::observation_budget(snapshot, &mut open)
            else {
                continue;
            };
            let budget = GpuBudgetTelemetry::from_snapshot(&snapshot, self.stamp);
            let size = serde_json::to_vec(&budget)
                .map_err(|e| e.to_string())?
                .len();
            let next = self
                .encoded_bytes
                .checked_add(size)
                .and_then(|n| n.checked_add(1))
                .filter(|n| *n <= MAX_REPORT_BYTES)
                .ok_or("native budget inventory exceeds byte bound")?;
            self.budgets.push(budget);
            self.encoded_bytes = next;
        }
        Ok(())
    }
}
fn discover(stamp: u64) -> Result<Vec<GpuBudgetTelemetry>, String> {
    discover_with(
        stamp,
        || ramshared_cuda::Cuda::load().ok(),
        || ramshared_vulkan::VulkanInventory::open().ok(),
        ramshared_dxg::DxgBudgetProvider::open_request,
    )
}
fn discover_with<C, V, B, F>(
    stamp: u64,
    cuda: impl FnOnce() -> Option<C>,
    vulkan: impl FnOnce() -> Option<V>,
    mut open: F,
) -> Result<Vec<GpuBudgetTelemetry>, String>
where
    C: MetadataInventory,
    V: MetadataInventory,
    B: ramshared_dxg::GpuBudgetProvider,
    F: FnMut(ramshared_dxg::BudgetRequest) -> Result<B, ramshared_dxg::DxgError>,
{
    let mut discovery = Discovery::new(stamp);
    discovery.collect(cuda(), &mut open)?;
    discovery.collect(vulkan(), &mut open)?;
    Ok(discovery.budgets)
}

#[cfg(test)]
mod tests;
