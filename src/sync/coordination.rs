//! Admission and task lifetime are coordinated independently of SQLite.
use super::model::{Error, Result};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
struct State {
    stopped: bool,
    active: usize,
}
#[derive(Default)]
pub(super) struct Coordination {
    state: Mutex<State>,
    changed: Condvar,
}
pub(super) struct Activity(Arc<Coordination>);
impl Coordination {
    pub fn enter(self: &Arc<Self>, cleanup: bool) -> Result<Activity> {
        let mut state = self.state.lock().map_err(|_| Error::storage())?;
        if state.stopped && (!cleanup || state.active == 0) {
            return Err(Error::new("SYNC_RECOVERING", 503));
        }
        state.active += 1;
        self.changed.notify_all();
        Ok(Activity(self.clone()))
    }
    pub fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
        }
    }
    pub fn ensure_accepting(&self) -> Result<()> {
        if self.state.lock().map_err(|_| Error::storage())?.stopped {
            return Err(Error::new("SYNC_RECOVERING", 503));
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn wait_active(&self) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |s| s.active == 0)
            .unwrap();
        assert!(state.active > 0);
    }
    pub fn drain(&self, timeout: Duration) -> Result<()> {
        let state = self.state.lock().map_err(|_| Error::storage())?;
        if !state.stopped {
            return Err(Error::new("SYNC_NOT_STOPPED", 409));
        }
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |s| s.active != 0)
            .map_err(|_| Error::storage())?;
        if state.active != 0 {
            return Err(Error::new("SYNC_DRAIN_TIMEOUT", 503));
        }
        Ok(())
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.active -= 1;
            self.0.changed.notify_all();
        }
    }
}
