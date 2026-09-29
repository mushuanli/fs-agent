//! Weak-revision bookkeeping for writable exports.
//!
//! A revision identifies a file *identity* (`dev`,`ino`) within one server
//! lifetime. It is intentionally not a content hash: the guarantee is that a
//! conditional write fails after the file was replaced or after a command ran,
//! not that equal bytes have equal revisions.

use crate::core::error::Error;
use std::{collections::HashMap, fs::File, os::unix::fs::MetadataExt};

/// Upper bound on tracked files; exceeding it drops every validator.
const CAPACITY: usize = 100_000;
/// Random prefix that makes a stale validator from a previous run unusable.
const NONCE_BYTES: usize = 16;

pub struct Revisions {
    nonce: String,
    next: u64,
    entries: HashMap<(u64, u64), u64>,
}

impl Revisions {
    pub fn new() -> Result<Self, Error> {
        let mut bytes = [0u8; NONCE_BYTES];
        getrandom::getrandom(&mut bytes).map_err(|_| Error::internal())?;
        Ok(Self {
            nonce: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
            next: 0,
            entries: HashMap::new(),
        })
    }

    /// Return the current validator for `file`, minting one if needed.
    pub fn get(&mut self, file: &File) -> Result<String, Error> {
        let key = identity(file)?;
        if !self.entries.contains_key(&key) {
            // Evicting conservatively invalidates old validators; generations
            // are never reused, so an old revision cannot become current again.
            if self.entries.len() >= CAPACITY {
                self.entries.clear();
            }
            self.next = self.next.checked_add(1).ok_or_else(Error::internal)?;
            self.entries.insert(key, self.next);
        }
        Ok(format!("\"{}:{}\"", self.nonce, self.entries[&key]))
    }

    /// Forget one file, e.g. immediately before it is replaced.
    pub fn retire(&mut self, file: &File) -> Result<(), Error> {
        self.entries.remove(&identity(file)?);
        Ok(())
    }

    /// Forget every file, e.g. after a command may have rewritten the export.
    pub fn invalidate(&mut self) {
        self.entries.clear();
    }
}

fn identity(file: &File) -> Result<(u64, u64), Error> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}
