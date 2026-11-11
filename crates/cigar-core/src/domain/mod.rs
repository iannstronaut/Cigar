// SPDX-License-Identifier: MPL-2.0

//! Canonical domain model.
//!
//! Re-exports the identifiers, enums, and records so callers can depend on a
//! single module path.

pub mod enums;
pub mod ids;
pub mod records;

pub use enums::*;
pub use ids::*;
pub use records::*;
