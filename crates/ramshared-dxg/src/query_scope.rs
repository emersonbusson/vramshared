// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-dxg/src/query_scope.rs
//! Local memory queries belong to one sampling process and one linked-adapter node.
use crate::process_identity::ProcessObservation;
use crate::{AdapterLuid, LinuxProcessIdentity};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetProcess {
    Linux(u32),
    Windows(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemorySegmentGroup {
    Local,
    NonLocal,
}

/// Observation provenance, not authentication of a remote producer.
/// A retained native provider owns its process lifetime; remote mappings must
/// additionally authenticate their session and process generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetQueryScope {
    pub process: BudgetProcess,
    /// Native creator generation, PID namespace and kernel boot. Absence is
    /// invalid scope, never permission to downgrade a required budget guard.
    pub process_identity: Option<LinuxProcessIdentity>,
    pub segment: MemorySegmentGroup,
    pub physical_adapter_index: u32,
}

impl BudgetQueryScope {
    pub fn current_process_local(physical_adapter_index: u32) -> Self {
        Self {
            process: BudgetProcess::Linux(std::process::id()),
            process_identity: LinuxProcessIdentity::current(),
            segment: MemorySegmentGroup::Local,
            physical_adapter_index,
        }
    }

    pub fn belongs_to_current_process(&self) -> bool {
        self.matches_current(ProcessObservation::current())
    }

    fn matches_current(&self, observed: Option<ProcessObservation>) -> bool {
        self.process == BudgetProcess::Linux(std::process::id())
            && self
                .process_identity
                .as_ref()
                .is_some_and(|creator| creator.matches_observation(observed))
    }

    pub fn is_current_process_local(&self) -> bool {
        self.belongs_to_current_process()
            && self.segment == MemorySegmentGroup::Local
            && self.physical_adapter_index < u32::BITS
    }

    /// Data equality does not transfer a dead creator's authority. Validate
    /// current scope once, then require the paired native owner to remain live.
    pub fn matches_current_scope(&self, observed: &Self) -> bool {
        self == observed
            && self.is_current_process_local()
            && observed
                .process_identity
                .as_ref()
                .is_some_and(LinuxProcessIdentity::is_alive)
    }
}

/// Product opening preserves an observed physical node. `None` is not zero;
/// it permits only detection of genuine native unavailability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetRequest {
    pub adapter: Option<AdapterLuid>,
    pub physical_adapter_index: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_creator_or_current_incarnation_never_matches() {
        let scope = BudgetQueryScope::current_process_local(0);
        assert!(scope.matches_current(ProcessObservation::current()));
        assert!(!scope.matches_current(None));
        let absent = BudgetQueryScope {
            process_identity: None,
            ..scope.clone()
        };
        assert!(!absent.matches_current(ProcessObservation::current()));
        assert!(!absent.matches_current(None));
    }

    #[test]
    fn query_scope_refuses_foreign_namespaces_segments_and_node_overflow() {
        let scope = BudgetQueryScope::current_process_local(31);
        assert!(scope.is_current_process_local());
        for invalid in [
            BudgetQueryScope {
                process_identity: None,
                ..scope.clone()
            },
            BudgetQueryScope {
                process: BudgetProcess::Windows(std::process::id()),
                ..scope.clone()
            },
            BudgetQueryScope {
                process: BudgetProcess::Linux(0),
                ..scope.clone()
            },
            BudgetQueryScope {
                segment: MemorySegmentGroup::NonLocal,
                ..scope.clone()
            },
            BudgetQueryScope {
                physical_adapter_index: 32,
                ..scope.clone()
            },
        ] {
            assert!(!invalid.is_current_process_local());
        }
    }
}
