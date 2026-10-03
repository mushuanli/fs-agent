//! Durable single-node sync storage, independent of exports and execution.
mod admin;
mod catalog;
mod commands;
mod coordination;
mod fault;
mod manifest;
mod model;
mod operations;
mod policy;
mod retention;
mod service;
mod store;
#[cfg(test)]
mod tests;
pub mod transport;

pub use admin::run as admin;
pub use model::{Config, Error, Result};
pub use service::SyncService;
