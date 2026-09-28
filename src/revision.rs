use crate::error::{invalid, Error};
use std::{collections::HashMap, fs::File, os::unix::fs::MetadataExt};

pub struct Revisions {
    epoch: String,
    next: u64,
    entries: HashMap<(u64, u64), u64>,
}
impl Revisions {
    pub fn new() -> Result<Self, Error> {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).map_err(|_| invalid())?;
        Ok(Self {
            epoch: bytes.iter().map(|v| format!("{v:02x}")).collect(),
            next: 0,
            entries: HashMap::new(),
        })
    }
    pub fn get(&mut self, file: &File) -> Result<String, Error> {
        let meta = file.metadata()?;
        let key = (meta.dev(), meta.ino());
        if !self.entries.contains_key(&key) {
            // Eviction invalidates old validators conservatively; generations are never reused.
            if self.entries.len() >= 100_000 {
                self.entries.clear();
            }
            self.next = self.next.checked_add(1).ok_or_else(invalid)?;
            self.entries.insert(key, self.next);
        }
        Ok(format!("\"{}:{}\"", self.epoch, self.entries[&key]))
    }
    pub fn retire(&mut self, file: &File) -> Result<(), Error> {
        let meta = file.metadata()?;
        self.entries.remove(&(meta.dev(), meta.ino()));
        Ok(())
    }
    pub fn invalidate(&mut self) {
        self.entries.clear();
    }
}
