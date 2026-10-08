//! A project execution boundary. Future VMM launchers implement this port.
use super::{model::Project, service};
use crate::{
    app::State,
    core::error::Error,
    harness::config::ProfileConfig,
    process::{
        policy::Plan,
        sandbox::{self, Prepared},
    },
};
use serde_json::Value;
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::OwnedRwLockWriteGuard;

pub trait ProjectLauncher: Send + Sync {
    fn prepare_process(&self, plan: &Plan, lock: File) -> Result<Prepared, Error>;
    fn prepare_harness(&self, plan: &Plan, lock: File, home: &Path) -> Result<Prepared, Error>;
}
pub struct Bubblewrap {
    pub network: bool,
}
impl ProjectLauncher for Bubblewrap {
    fn prepare_process(&self, plan: &Plan, lock: File) -> Result<Prepared, Error> {
        if !self.network {
            return sandbox::assemble(plan, lock);
        }
        let mut prepared = sandbox::mount_plan(plan, lock)?;
        network(&mut prepared.command);
        prepared
            .command
            .args(["--chdir", &plan.cwd, "--", &plan.command])
            .args(&plan.args);
        Ok(prepared)
    }
    fn prepare_harness(&self, plan: &Plan, lock: File, home: &Path) -> Result<Prepared, Error> {
        let mut prepared = sandbox::mount_plan(plan, lock)?;
        mount_harness_home(&mut prepared, home)?;
        let command = &mut prepared.command;
        if self.network {
            network(command);
        }
        let executable = resolve_command(&plan.command)?;
        let invocation = if !executable.starts_with("/usr") && !executable.starts_with("/bin") {
            command
                .arg("--ro-bind")
                .arg(&executable)
                .arg("/opt/harness");
            Path::new("/opt/harness")
        } else {
            executable.as_path()
        };
        command
            .args(["--chdir", &plan.cwd, "--"])
            .arg(invocation)
            .args(&plan.args);
        sandbox::inherit_fds(command, &prepared.directories);
        Ok(prepared)
    }
}
fn mount_harness_home(prepared: &mut Prepared, native_home: &Path) -> Result<(), Error> {
    use std::os::fd::AsRawFd;
    let home = crate::fs::Export::open(native_home)?.try_clone_root()?;
    let compatibility_home = home.try_clone()?;
    let command = &mut prepared.command;
    // Native indexes retain absolute rollout paths; expose only this harness home.
    command
        .arg("--bind-fd")
        .arg(home.as_raw_fd().to_string())
        .arg(native_home);
    command.args([
        "--bind-fd",
        &compatibility_home.as_raw_fd().to_string(),
        "/harness",
    ]);
    prepared.directories.extend([home, compatibility_home]);
    Ok(())
}

fn network(command: &mut tokio::process::Command) {
    command.arg("--share-net");
    for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/ssl/certs"] {
        command.args(["--ro-bind-try", path, path]);
    }
}
fn resolve_command(command: &str) -> Result<PathBuf, Error> {
    if Path::new(command).is_absolute() {
        return Path::new(command).canonicalize().map_err(Error::from);
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(command))
        .find(|p| p.is_file())
        .ok_or_else(Error::unsupported)?
        .canonicalize()
        .map_err(Error::from)
}
struct Active {
    thread: String,
    _guard: OwnedRwLockWriteGuard<()>,
}
pub struct ProjectRuntime {
    state: Weak<State>,
    pub project: Project,
    identity: usize,
    active: Mutex<Option<Active>>,
}
impl ProjectRuntime {
    pub fn new(state: &Arc<State>, identity: usize, project: Project) -> Self {
        Self {
            state: Arc::downgrade(state),
            identity,
            project,
            active: Mutex::new(None),
        }
    }
    pub fn cwd(&self) -> String {
        format!("/projects/{}", self.project.id)
    }
    pub fn resolve_cwd(&self, cwd: &str) -> Result<String, Error> {
        let root = self.cwd();
        if crate::process::policy::virtual_path(cwd).is_ok()
            && (cwd == root || cwd.starts_with(&(root.clone() + "/")))
        {
            return Ok(cwd.into());
        }
        let state = self.state()?;
        let export = state
            .exports
            .get(&self.project.alias)
            .ok_or_else(Error::invalid)?;
        let directory = export.open_dir(&self.project.path)?;
        use std::os::fd::AsRawFd;
        let physical = std::fs::read_link(format!("/proc/self/fd/{}", directory.as_raw_fd()))?;
        let path = std::path::Path::new(cwd).canonicalize()?;
        let relative = path
            .strip_prefix(physical)
            .map_err(|_| Error::forbidden("EACCES"))?
            .to_str()
            .ok_or_else(Error::invalid)?;
        crate::fs::path::validate(relative)?;
        let source = [self.project.path.as_str(), relative]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        export.open_dir(&source)?;
        Ok(if relative.is_empty() {
            root
        } else {
            format!("{root}/{relative}")
        })
    }
    pub fn state(&self) -> Result<Arc<State>, Error> {
        let state = self.state.upgrade().ok_or_else(Error::unavailable)?;
        let current = state
            .projects
            .as_ref()
            .ok_or_else(Error::unsupported)?
            .get(self.identity, &self.project.id)?;
        if current.revision != self.project.revision {
            return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
        }
        service::authorize(&state, self.identity, &current)?;
        Ok(state)
    }
    pub fn prepare(&self, profile: &ProfileConfig, args: Vec<String>) -> Result<Prepared, Error> {
        let state = self.state()?;
        if !state.execution.ready() {
            return Err(Error::unsupported());
        }
        let request = service::request(
            &state,
            &self.project,
            profile.command.clone(),
            args,
            300_000,
        );
        let mut plan = crate::process::policy::plan(&state, self.identity, &request)?;
        service::pin(&mut plan, &self.project)?;
        let cwd = self.cwd();
        for mount in &mut plan.mounts {
            mount.at = format!(
                "{}{}",
                cwd,
                mount
                    .at
                    .strip_prefix("/workspace")
                    .ok_or_else(Error::invalid)?
            );
        }
        plan.cwd = cwd;
        let lock = match plan.writer() {
            Some(export) => export.try_clone_root()?,
            None => state.execution.lock_handle()?,
        };
        state
            .projects
            .as_ref()
            .ok_or_else(Error::unsupported)?
            .launcher
            .prepare_harness(&plan, lock, &profile.home)
    }
    pub fn begin(&self, thread: &str) -> Result<(), Error> {
        let state = self.state()?;
        let mut active = self.active.lock().map_err(|_| Error::internal())?;
        if active.is_some() {
            return Err(Error::busy());
        }
        *active = Some(Active {
            thread: thread.into(),
            _guard: state.files.exclusive()?,
        });
        Ok(())
    }
    pub fn finish(&self, thread: &str) {
        let mut active = self.active.lock().unwrap();
        if active.as_ref().is_some_and(|a| a.thread == thread) {
            if let Some(state) = self.state.upgrade() {
                for (_, export) in state.exports.iter() {
                    export.invalidate_revisions();
                }
            }
            *active = None;
        }
    }
    pub fn event(&self, message: &Value) {
        if message["method"] == "turn/completed" {
            if let Some(thread) = message["params"]["threadId"].as_str() {
                self.finish(thread);
            }
        }
    }
    pub fn stopped(&self, clean: bool) {
        if !clean && self.active.lock().unwrap().is_some() {
            if let Some(state) = self.state.upgrade() {
                state.files.poison();
            }
        }
        if clean {
            let thread = self
                .active
                .lock()
                .unwrap()
                .as_ref()
                .map(|a| a.thread.clone());
            if let Some(thread) = thread {
                self.finish(&thread);
            }
        }
    }
}
