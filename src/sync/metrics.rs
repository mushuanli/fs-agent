//! In-process diagnostics; no network API or changes to storage semantics.
use rusqlite::{Connection, Transaction};
use serde_json::{json, Value};
use std::{
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicU64, Ordering},
        MutexGuard,
    },
    time::Instant,
};

#[derive(Default)]
pub(super) struct Sample {
    count: AtomicU64,
    total: AtomicU64,
    max: AtomicU64,
}
impl Sample {
    pub(super) fn record(&self, start: Instant) {
        let nanos = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.total.fetch_add(nanos, Ordering::Relaxed);
        self.max.fetch_max(nanos, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }
    fn value(&self) -> Value {
        json!({"count":self.count.load(Ordering::Relaxed),
            "totalNanos":self.total.load(Ordering::Relaxed),"maxNanos":self.max.load(Ordering::Relaxed)})
    }
}
#[derive(Default)]
pub(super) struct Metrics {
    pub wait: Sample,
    pub hold: Sample,
    pub transaction: Sample,
    pub commit: Sample,
}
impl Metrics {
    pub(super) fn value(&self) -> Value {
        json!({"lockWait":self.wait.value(),"lockHold":self.hold.value(),
            "transaction":self.transaction.value(),"commit":self.commit.value()})
    }
}
pub(super) struct LockedConnection<'a> {
    pub guard: MutexGuard<'a, Connection>,
    pub started: Instant,
    pub sample: &'a Sample,
}
impl Deref for LockedConnection<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.guard
    }
}
impl DerefMut for LockedConnection<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.guard
    }
}
impl Drop for LockedConnection<'_> {
    fn drop(&mut self) {
        self.sample.record(self.started);
    }
}
pub(super) struct TimedTransaction<'a> {
    pub inner: Option<Transaction<'a>>,
    pub started: Instant,
    pub metrics: &'a Metrics,
}
impl Deref for TimedTransaction<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.inner.as_ref().expect("live transaction")
    }
}
impl TimedTransaction<'_> {
    pub fn commit(mut self) -> rusqlite::Result<()> {
        let started = Instant::now();
        let result = self.inner.take().expect("live transaction").commit();
        self.metrics.commit.record(started);
        result
    }
}
impl Drop for TimedTransaction<'_> {
    fn drop(&mut self) {
        // Include rollback on every error path before recording elapsed transaction time.
        drop(self.inner.take());
        self.metrics.transaction.record(self.started);
    }
}
