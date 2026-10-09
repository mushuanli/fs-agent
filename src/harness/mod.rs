//! Host harness profiles, separate from short-lived sandboxed shell commands.
mod attachments;
mod bridge;
mod claude;
mod codex;
pub mod config;
mod driver;
mod events;
mod history;
mod plugins;
mod presentation;
mod search;
mod service;
pub use driver::HarnessDriver;
pub use plugins::{HarnessPlugin, HarnessPlugins};
pub use service::Harnesses;
