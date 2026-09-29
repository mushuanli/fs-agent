//! Transport-independent primitives shared by every layer.
//!
//! Nothing in `core` knows about Axum routes, exports or business rules: it
//! only provides the error vocabulary, identifier policy, request-scoped
//! scheduling and the small codecs the rest of the service builds on.

pub mod cursor;
pub mod error;
pub mod gate;
pub mod ids;
pub mod workers;
