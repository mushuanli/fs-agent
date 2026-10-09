use super::{config::ProfileConfig, driver::HarnessDriver, HarnessPlugins};
use crate::core::error::Error;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Mutex;

pub struct Harnesses {
    profiles: BTreeMap<String, Arc<Profile>>,
    configs: Vec<ProfileConfig>,
    scoped: Mutex<BTreeMap<String, Arc<Profile>>>,
    epoch: String,
    closed: AtomicBool,
    plugins: HarnessPlugins,
}
struct Profile {
    driver: Box<dyn HarnessDriver>,
    receipts: Mutex<BTreeMap<String, (String, Value)>>,
    mutations: Mutex<()>,
}

impl Harnesses {
    pub fn new(configs: Vec<ProfileConfig>, epoch: String) -> Result<Self, String> {
        Self::with_plugins(configs, epoch, HarnessPlugins::default())
    }
    pub fn with_plugins(
        configs: Vec<ProfileConfig>,
        epoch: String,
        plugins: HarnessPlugins,
    ) -> Result<Self, String> {
        let profiles = configs
            .clone()
            .into_iter()
            .map(|config| {
                Ok((
                    config.id.clone(),
                    Arc::new(Profile {
                        driver: plugins.create(config, None)?,
                        receipts: Mutex::new(BTreeMap::new()),
                        mutations: Mutex::new(()),
                    }),
                ))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        Ok(Self {
            profiles,
            configs,
            scoped: Mutex::new(BTreeMap::new()),
            epoch,
            closed: AtomicBool::new(false),
            plugins,
        })
    }
    pub fn enabled(&self) -> bool {
        !self.profiles.is_empty()
    }
    pub fn private_homes(&self) -> impl Iterator<Item = &std::path::Path> {
        self.configs.iter().map(|profile| profile.home.as_path())
    }
    pub fn profiles(&self) -> Value {
        json!({"epoch":self.epoch,"profiles":self.profiles.values().map(|p| p.driver.descriptor()).collect::<Vec<_>>()})
    }
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        if name == "harness_profiles" {
            return Ok(self.profiles());
        }
        let profile = self
            .profiles
            .get(string(&args, "profileId")?)
            .ok_or_else(|| Error::not_found("ENOENT"))?;
        if name == "harness_operation" {
            self.check_epoch(&args)?;
            return profile.operation(string(&args, "requestId")?).await;
        }
        if is_mutation(name) {
            self.check_epoch(&args)?;
            return profile.mutate(name, args).await;
        }
        profile.driver.read(name, args).await
    }
    pub async fn project_call(
        &self,
        state: &Arc<crate::app::State>,
        identity: usize,
        name: &str,
        args: Value,
    ) -> Result<Value, Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        let mut project = state
            .projects
            .as_ref()
            .ok_or_else(Error::unsupported)?
            .get(identity, string(&args, "projectId")?)?;
        crate::projects::service::authorize(state, identity, &project)?;
        if args["revision"].as_u64() != Some(project.revision) {
            return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
        }
        let config = self
            .configs
            .iter()
            .find(|p| p.id == string(&args, "profileId").unwrap_or("") && p.projects)
            .ok_or_else(Error::unsupported)?;
        let read_only = args["readOnly"].as_bool().unwrap_or(false);
        if read_only {
            project.access = "ro".into();
            for mount in &mut project.mounts {
                mount.access = "ro".into();
            }
        }
        let key = format!(
            "{}:{}:{}:{}",
            project.id, project.revision, config.id, read_only
        );
        let mut scoped = self.scoped.lock().await;
        if !scoped.contains_key(&key) {
            if scoped.len() >= 128 {
                return Err(Error::too_many("PROJECT_HARNESS_LIMIT"));
            }
            let runtime = Arc::new(crate::projects::runtime::ProjectRuntime::new(
                state, identity, project,
            ));
            scoped.insert(
                key.clone(),
                Arc::new(Profile {
                    driver: self
                        .plugins
                        .create(config.clone(), Some(runtime))
                        .map_err(|_| Error::unsupported())?,
                    receipts: Mutex::new(BTreeMap::new()),
                    mutations: Mutex::new(()),
                }),
            );
        }
        let profile = scoped[&key].clone();
        drop(scoped);
        if name == "harness_operation" {
            self.check_epoch(&args)?;
            return profile.operation(string(&args, "requestId")?).await;
        }
        if is_mutation(name) {
            self.check_epoch(&args)?;
            return profile.mutate(name, args).await;
        }
        profile.driver.read(name, args).await
    }
    pub async fn close_project(&self, id: &str) {
        let mut scoped = self.scoped.lock().await;
        let keys = scoped
            .keys()
            .filter(|key| key.starts_with(&format!("{id}:")))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(profile) = scoped.remove(&key) {
                profile.driver.stop();
                profile.driver.drain().await;
            }
        }
    }
    fn check_epoch(&self, args: &Value) -> Result<(), Error> {
        if string(args, "epoch")? != self.epoch {
            return Err(Error::conflict("HARNESS_EPOCH_CHANGED"));
        }
        Ok(())
    }
    pub async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let mut scoped = self.scoped.lock().await;
        for profile in scoped.values() {
            profile.driver.stop();
        }
        for profile in scoped.values() {
            profile.driver.drain().await;
        }
        scoped.clear();
        // Fence every driver before waiting so queued mutations cannot start another child.
        for profile in self.profiles.values() {
            profile.driver.stop();
        }
        for profile in self.profiles.values() {
            profile.driver.drain().await;
        }
    }
}

impl Profile {
    async fn mutate(self: &Arc<Self>, name: &str, args: Value) -> Result<Value, Error> {
        let id = string(&args, "requestId")?.to_owned();
        if !crate::core::ids::is_identifier_within(&id, 128) {
            return Err(Error::invalid());
        }
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(json!([name, args]).to_string().as_bytes())
        );
        if let Some(receipt) = self.reserve(&id, &fingerprint).await? {
            return Ok(receipt);
        }
        // Execution outlives an HTTP disconnect; queries can confirm a later result.
        let work =
            tokio::spawn(
                self.clone()
                    .execute(name.to_owned(), args, id.clone(), fingerprint),
            );
        match tokio::time::timeout(std::time::Duration::from_secs(25), work).await {
            Ok(Ok(value)) => Ok(value),
            _ => Ok(json!({"outcome":"unknown","requestId":id})),
        }
    }
    async fn operation(&self, id: &str) -> Result<Value, Error> {
        self.receipts
            .lock()
            .await
            .get(id)
            .map(|(_, receipt)| receipt.clone())
            .ok_or_else(|| Error::not_found("ENOENT"))
    }
    async fn execute(
        self: Arc<Self>,
        name: String,
        args: Value,
        id: String,
        fingerprint: String,
    ) -> Value {
        // Replies and interruption must pass a start/resume waiting on native approval.
        let _serial = if matches!(name.as_str(), "harness_respond" | "harness_interrupt") {
            None
        } else {
            Some(self.mutations.lock().await)
        };
        let result = self.driver.execute(&name, &args).await;
        let receipt = receipt(&id, result);
        self.receipts
            .lock()
            .await
            .insert(id, (fingerprint, receipt.clone()));
        receipt
    }
    async fn reserve(&self, id: &str, fingerprint: &str) -> Result<Option<Value>, Error> {
        let mut receipts = self.receipts.lock().await;
        if let Some((previous, receipt)) = receipts.get(id) {
            if previous != fingerprint {
                return Err(Error::conflict("HARNESS_REQUEST_REUSED"));
            }
            return Ok(Some(receipt.clone()));
        }
        if receipts.len() >= 1024 {
            return Err(Error::too_many("HARNESS_OPERATION_LIMIT"));
        }
        receipts.insert(
            id.into(),
            (
                fingerprint.into(),
                json!({"outcome":"unknown","requestId":id}),
            ),
        );
        Ok(None)
    }
}

fn receipt(id: &str, result: Result<Value, Error>) -> Value {
    match result {
        Ok(result) => json!({"outcome":"committed","requestId":id,"result":result}),
        Err(error) => {
            json!({"outcome":if error.code == "ETIMEDOUT" || error.code == "EIO" {"unknown"} else {"not-committed"},
                "requestId":id,"code":error.code})
        }
    }
}

pub(super) fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128 * 1024)
        .ok_or_else(Error::invalid)
}

fn is_mutation(name: &str) -> bool {
    matches!(
        name,
        "harness_create"
            | "harness_resume"
            | "harness_fork"
            | "harness_rename"
            | "harness_archive"
            | "harness_unarchive"
            | "harness_delete"
            | "harness_turn"
            | "harness_interrupt"
            | "harness_respond"
    )
}
