//! Remote command execution.
//!
//! Responsibilities are split so each file answers one question:
//!
//! * [`model`] — the wire shape and the process/registry data structures;
//! * [`policy`] — *may* this request run, and over which directories;
//! * [`sandbox`] — *how* the launcher is assembled (bubblewrap, descriptors);
//! * [`runner`] — *when* the file gate reopens, and what is reported;
//! * [`service`] — the API operations, including epoch and idempotency rules;
//! * [`execution`] — admission state and the descriptor that fences the export.
//!
//! Execution is opt-in: [`enable`] must succeed at startup before any command
//! is admitted, and it fails closed when bubblewrap cannot provide fd mounts
//! and user namespaces.

mod execution;
mod model;
mod policy;
mod runner;
mod sandbox;
mod service;

pub use execution::{enable, Execution};
pub use model::{Process, Registry, Request, Status};
pub use service::{cancel, start, status};
