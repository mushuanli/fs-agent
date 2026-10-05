//! One module per route group.
//!
//! A handler validates transport-level input, calls one domain operation and
//! shapes the JSON response. Anything longer than that belongs in the domain
//! module it delegates to.

pub mod capabilities;
pub mod content;
pub mod entries;
pub mod exports;
pub mod mutations;
pub mod operations;
pub mod processes;
pub mod seq;
pub mod stat;
