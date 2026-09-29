//! Configuration: the declarative input and the policy that validates it.
//!
//! * [`model`] is the TOML shape and nothing else.
//! * [`credentials`] decides which single secret is configured and how strong
//!   it must be.
//! * [`exports`] decides aliases, rejects overlapping roots and opens the
//!   directory capabilities.
//! * [`launch`] finds and parses the file.
//!
//! Runtime wiring lives in [`crate::app`], so this module never needs a
//! Tokio runtime, never holds a lock, and can be tested by value.

pub mod launch;
pub mod model;

mod credentials;
mod exports;

pub use model::{Access, Config, ExportConfig};

pub(crate) use credentials::resolve as resolve_credentials;
pub(crate) use exports::open_all as open_exports;
