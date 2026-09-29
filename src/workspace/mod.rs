//! Durable workspace fencing.
//!
//! Two layers, on purpose:
//!
//! * [`lease`] is policy — when a handle is trusted, when a generation is
//!   stale, who may revoke and who may only observe.
//! * [`journal`] is mechanism — an atomically persisted document behind an
//!   exclusive advisory lock.
//!
//! This module is intentionally **not wired to any route**. The Harness MVP
//! does not require workspace leases; it is kept as an independent, tested
//! mechanism so a future workspace API can build on it without redesigning
//! the request path.

pub mod lease;

mod journal;
mod model;
