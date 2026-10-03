//! Deterministic process exits are available only in explicit test builds.
pub fn point(name: &str) {
    #[cfg(feature = "sync-fault-injection")]
    if std::env::var("FS_AGENT_SYNC_CRASH_AT").ok().as_deref() == Some(name) {
        std::process::exit(86);
    }
    let _ = name;
}

pub fn io(name: &str) -> super::model::Result<()> {
    #[cfg(feature = "sync-fault-injection")]
    if std::env::var("FS_AGENT_SYNC_IO_FAIL_AT").ok().as_deref() == Some(name) {
        return Err(super::model::Error::storage());
    }
    let _ = name;
    Ok(())
}
