//! Host harness profiles, separate from short-lived sandboxed shell commands.
mod bridge;
mod codex;
pub mod config;
mod driver;
mod events;
mod history;
mod plugins;
mod presentation;
mod service;
pub use driver::HarnessDriver;
pub use plugins::{HarnessPlugin, HarnessPlugins};
pub use service::Harnesses;
