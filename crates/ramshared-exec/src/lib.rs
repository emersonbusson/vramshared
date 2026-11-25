// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/lib.rs
//! Exact, bounded custody for short-lived production subprocesses.
//!
//! Controller commands and serving daemons share the bounded capture owner.
//! The controller API treats unresolved containment as fatal to its controller;
//! the serving API retains the child and capture, blocks further admission,
//! and leaves storage serving. Both address only their exact owned group.
//! See [`bounded_process`] and [`retained_process`] for their custody contracts.

pub mod bounded_process;
mod capture;
pub mod inherited_lease;
pub mod retained_process;

pub use bounded_process::{
    BoundedOutput, DEFAULT_OUTPUT_LIMIT, ProcessSpawnError, configure_process_group,
    run_capture_command, terminate_group_and_reap, wait_grouped_child,
};
