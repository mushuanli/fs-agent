//! Structured stderr sink and severity filtering; callers choose safe event metadata.
use chrono::{DateTime, Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Write,
    sync::atomic::{AtomicU8, Ordering},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Level {
    Off = 0,
    Error = 1,
    Warn = 2,
    #[default]
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    pub fn allows(self, event: Level) -> bool {
        event != Level::Off && event as u8 <= self as u8
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
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
    let record = Record::new(Local::now(), level, event, fields);
    // A closed logging sink must not interrupt process supervision or cleanup.
    if let Ok(line) = serde_json::to_string(&record) {
        let _ = writeln!(std::io::stderr().lock(), "{line}");
    }
}

// Struct field order keeps the readable time first without changing JSON-lines consumers.
#[derive(Serialize)]
struct Record<'a> {
    time: String,
    level: Level,
    event: &'a str,
    fields: Value,
    #[serde(rename = "timeMs")]
    time_ms: i64,
}
impl<'a> Record<'a> {
    fn new(time: DateTime<Local>, level: Level, event: &'a str, fields: Value) -> Self {
        Self {
            time: time.to_rfc3339_opts(SecondsFormat::Millis, false),
            level,
            event,
            fields,
            time_ms: time.timestamp_millis(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn severity_thresholds_and_default_are_explicit() {
        assert_eq!(Level::default(), Level::Info);
        assert!(!Level::default().allows(Level::Debug));
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
            Level::Info
        );
        assert_eq!(
            toml::from_str::<Settings>("log_level = 'off'")
                .unwrap()
                .log_level,
            Level::Off
        );
        assert!(toml::from_str::<Settings>("log_level = 'verbose'").is_err());
    }
    #[test]
    fn readable_local_time_is_first_and_matches_the_machine_timestamp() {
        let time = DateTime::parse_from_rfc3339("2026-10-08T13:44:42.034+08:00")
            .unwrap()
            .with_timezone(&Local);
        let line = serde_json::to_string(&Record::new(
            time,
            Level::Info,
            "server.ready",
            serde_json::json!({"address":"127.0.0.1:8787"}),
        ))
        .unwrap();
        assert!(line.starts_with("{\"time\":\""));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let readable = DateTime::parse_from_rfc3339(parsed["time"].as_str().unwrap()).unwrap();
        assert_eq!(readable.timestamp_millis(), 1791438282034);
        assert_eq!(
            parsed["timeMs"].as_i64().unwrap(),
            readable.timestamp_millis()
        );
        assert!(line.find("\"time\"") < line.find("\"level\""));
        assert!(line.find("\"level\"") < line.find("\"event\""));
        assert!(line.find("\"event\"") < line.find("\"fields\""));
    }
}
