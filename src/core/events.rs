//! Structured stderr sink and severity filtering; callers choose safe event metadata.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::Write,
    sync::atomic::{AtomicU8, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Level {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    #[default]
    Debug = 4,
    Trace = 5,
}

impl Level {
    pub fn allows(self, event: Level) -> bool {
        event != Level::Off && event as u8 <= self as u8
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Debug as u8);
pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}
pub fn enabled(level: Level) -> bool {
    level != Level::Off && level as u8 <= LEVEL.load(Ordering::Relaxed)
}

pub fn emit(level: Level, event: &str, fields: Value) {
    if !enabled(level) {
        return;
    }
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let record = json!({"timeMs": time, "level": level, "event": event, "fields": fields});
    // A closed logging sink must not interrupt process supervision or cleanup.
    let _ = writeln!(std::io::stderr().lock(), "{record}");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn severity_thresholds_and_default_are_explicit() {
        assert_eq!(Level::default(), Level::Debug);
        assert!(Level::Debug.allows(Level::Error));
        assert!(Level::Debug.allows(Level::Debug));
        assert!(!Level::Debug.allows(Level::Trace));
        assert!(!Level::Off.allows(Level::Error));
        assert!(!Level::Error.allows(Level::Warn));
        assert!(Level::Trace.allows(Level::Debug));
    }
    #[test]
    fn configuration_accepts_only_supported_levels() {
        #[derive(Deserialize)]
        struct Settings {
            #[serde(default)]
            log_level: Level,
        }
        assert_eq!(
            toml::from_str::<Settings>("").unwrap().log_level,
            Level::Debug
        );
        assert_eq!(
            toml::from_str::<Settings>("log_level = 'off'")
                .unwrap()
                .log_level,
            Level::Off
        );
        assert!(toml::from_str::<Settings>("log_level = 'verbose'").is_err());
    }
}
