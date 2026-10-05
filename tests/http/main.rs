//! HTTP transport tests, one module per route group.

#[path = "../common/mod.rs"]
mod common;

mod auth;
mod capabilities;
mod content;
mod listing;
mod mutations;
mod seq;
