// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/mod.rs
//! Native Windows install surface (DT-13..DT-16).
//!
//! The owned root device, the receipt-bearing driver-store publisher and the
//! exclusive install transition live here. The transaction and receipt
//! reconciliation in the customer-installation SPEC move in with DT-14/15;
//! until then `main.rs` keeps the install transaction and calls these owners.

pub mod compensation;
pub mod driver_store;
pub mod receipt;
pub mod root_device;
pub mod transaction;
pub mod transition;
pub mod uninstall;
