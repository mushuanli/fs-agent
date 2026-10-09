//! Compiled-in plugins own native protocol semantics and driver construction.
use super::{codex::Codex, config::ProfileConfig, driver::HarnessDriver};
use crate::projects::runtime::ProjectRuntime;
use std::{collections::BTreeMap, sync::Arc};

/// Drivers return the common harness session, event and interaction envelopes.
/// Native storage, process setup and protocol translation remain plugin-owned.
pub trait HarnessPlugin: Send + Sync {
    fn kind(&self) -> &'static str;
    fn validate(&self, profile: &ProfileConfig) -> Result<(), String>;
    fn create(
        &self,
        profile: ProfileConfig,
        runtime: Option<Arc<ProjectRuntime>>,
    ) -> Result<Box<dyn HarnessDriver>, String>;
}

#[derive(Clone)]
pub struct HarnessPlugins {
    entries: BTreeMap<String, Arc<dyn HarnessPlugin>>,
}

impl Default for HarnessPlugins {
    fn default() -> Self {
        let mut plugins = Self::empty();
        plugins
            .entries
            .insert("codex".into(), Arc::new(CodexPlugin));
        plugins
            .entries
            .insert("claude".into(), Arc::new(ClaudePlugin));
        plugins
    }
}

impl HarnessPlugins {
    pub fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
    pub fn register(&mut self, plugin: Arc<dyn HarnessPlugin>) -> Result<(), String> {
        let kind = plugin.kind();
        if !crate::core::ids::is_identifier(kind) || self.entries.contains_key(kind) {
            return Err("Invalid or duplicate harness plugin kind".into());
        }
        self.entries.insert(kind.into(), plugin);
        Ok(())
    }
    pub fn validate(&self, profile: &ProfileConfig) -> Result<(), String> {
        self.plugin(&profile.kind)?.validate(profile)
    }
    pub fn create(
        &self,
        profile: ProfileConfig,
        runtime: Option<Arc<ProjectRuntime>>,
    ) -> Result<Box<dyn HarnessDriver>, String> {
        let plugin = self.plugin(&profile.kind)?;
        plugin.validate(&profile)?;
        plugin.create(profile, runtime)
    }
    fn plugin(&self, kind: &str) -> Result<&Arc<dyn HarnessPlugin>, String> {
        self.entries
            .get(kind)
            .ok_or_else(|| format!("Harness plugin is not registered: {kind}"))
    }
}

struct CodexPlugin;
impl HarnessPlugin for CodexPlugin {
    fn kind(&self) -> &'static str {
        "codex"
    }
    fn validate(&self, _profile: &ProfileConfig) -> Result<(), String> {
        Ok(())
    }
    fn create(
        &self,
        profile: ProfileConfig,
        runtime: Option<Arc<ProjectRuntime>>,
    ) -> Result<Box<dyn HarnessDriver>, String> {
        Ok(match runtime {
            Some(runtime) => Box::new(Codex::project(profile, runtime)),
            None => Box::new(Codex::new(profile)),
        })
    }
}

struct ClaudePlugin;
impl HarnessPlugin for ClaudePlugin {
    fn kind(&self) -> &'static str {
        "claude"
    }
    fn validate(&self, profile: &ProfileConfig) -> Result<(), String> {
        if profile.command == "codex" {
            return Err("Claude profiles require an explicit Claude CLI command".into());
        }
        Ok(())
    }
    fn create(
        &self,
        profile: ProfileConfig,
        runtime: Option<Arc<ProjectRuntime>>,
    ) -> Result<Box<dyn HarnessDriver>, String> {
        Ok(Box::new(super::claude::Claude::new(profile, runtime)))
    }
}
