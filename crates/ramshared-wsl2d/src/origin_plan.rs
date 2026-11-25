// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/origin_plan.rs
//! Immutable worker requests from the trusted runtime adapter, never an activation grant.
use super::WorkerSpawnParams;
use ramshared_config::resource_profile::MAX_RESOURCE_PROFILE_BYTES;

pub(super) type PlanResult = Result<WorkerPlan, Box<dyn std::error::Error>>;
pub(super) struct WorkerPlan {
    pub(super) chunk_bytes: u64,
    pub(super) members: Vec<WorkerSpawnParams>,
}
pub(super) struct PreparedWorkers {
    chunk_bytes: u64,
    members: Vec<WorkerSpawnParams>,
}
impl PreparedWorkers {
    pub(super) fn into_parts(self) -> (u64, Vec<WorkerSpawnParams>) {
        (self.chunk_bytes, self.members)
    }
}
impl WorkerPlan {
    pub(super) fn automatic(target: u64, chunk: u64, reserve: u64) -> Self {
        Self {
            chunk_bytes: chunk,
            members: vec![WorkerSpawnParams {
                generation: None,
                target_bytes: target,
                chunk_bytes: chunk,
                reserve_floor: reserve,
                expected_adapter: None,
                host_catalog: None,
            }],
        }
    }

    pub(super) fn prepare(mut self, logical: u64, reserve: u64) -> Result<PreparedWorkers, String> {
        if self.members.is_empty() || self.chunk_bytes == 0 {
            return Err("cache worker plan needs members and positive routing geometry".into());
        }
        let automatic = self.members.len() == 1;
        let (mut bytes, mut total) = (0usize, 0u64);
        for member in &mut self.members {
            let lengths = member.expected_adapter.as_ref().map_or([0; 3], |id| {
                [
                    id.backend.len(),
                    id.key.len(),
                    id.luid.as_ref().map_or(0, String::len),
                ]
            });
            let identity_bytes = lengths
                .into_iter()
                .try_fold(0usize, usize::checked_add)
                .ok_or("cache worker identity metadata overflows")?;
            bytes = bytes
                .checked_add(std::mem::size_of::<WorkerSpawnParams>())
                .and_then(|n| n.checked_add(identity_bytes))
                .filter(|n| *n <= MAX_RESOURCE_PROFILE_BYTES)
                .ok_or("cache worker request metadata exceeds the resource input budget")?;
            if member.target_bytes == 0 || member.chunk_bytes == 0 || member.reserve_floor < reserve
            {
                return Err("cache member geometry or enforced reserve is invalid".into());
            }
            total = total
                .checked_add(member.target_bytes)
                .filter(|sum| *sum <= logical)
                .ok_or("cache member targets exceed logical origin capacity")?;
            match member.expected_adapter.as_mut() {
                Some(id) if id.is_stable_driver_identity() => {
                    id.key.make_ascii_lowercase();
                    if let Some(luid) = id.luid.as_mut() {
                        luid.make_ascii_lowercase();
                    }
                }
                None if automatic => {}
                _ => return Err("configured cache members need stable pinned identities".into()),
            }
        }
        for (index, member) in self.members.iter().enumerate() {
            if let Some(id) = &member.expected_adapter
                && self.members[..index].iter().any(|other| {
                    other
                        .expected_adapter
                        .as_ref()
                        .is_none_or(|other| !id.proves_independent_memory(other))
                })
            {
                return Err("cache members do not prove independent physical memory".into());
            }
        }
        self.members
            .sort_by(|a, b| identity_order(a).cmp(&identity_order(b)));
        Ok(PreparedWorkers {
            chunk_bytes: self.chunk_bytes,
            members: self.members,
        })
    }
}

fn identity_order(member: &WorkerSpawnParams) -> (&str, &str) {
    member
        .expected_adapter
        .as_ref()
        .map_or(("", ""), |id| (id.backend.as_str(), id.key.as_str()))
}

pub(super) fn member_key(member: &WorkerSpawnParams) -> String {
    member.expected_adapter.as_ref().map_or_else(
        || "automatic".into(),
        |id| format!("{}:{}", id.backend, id.key),
    )
}

#[cfg(test)]
mod tests;
