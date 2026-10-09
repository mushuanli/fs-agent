//! Bounded inotify invalidation over pinned project directory capabilities.
use super::{model::Project, service};
use crate::{app::State, core::error::Error};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::File,
    mem::MaybeUninit,
    os::fd::{AsRawFd, OwnedFd},
    sync::Arc,
    time::{Duration, Instant},
};
mod tree;

pub struct Registry {
    entries: BTreeMap<String, Watch>,
}
struct Watch {
    owner: usize,
    project: String,
    revision: u64,
    touched: Instant,
    roots: Vec<tree::Root>,
    fd: OwnedFd,
    directories: BTreeMap<i32, tree::Directory>,
    version: u64,
    truncated: bool,
    needs_rescan: bool,
}
impl Registry {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
    pub fn remove_project(&mut self, id: &str) {
        self.entries.retain(|_, watch| watch.project != id);
    }
    fn prune(&mut self) {
        self.entries
            .retain(|_, watch| watch.touched.elapsed() < Duration::from_secs(60));
    }
}
fn observe(state: &Arc<State>, owner: usize, name: &str, args: Value) -> Result<Value, Error> {
    let project = authorized(state, owner, &args)?;
    let registry = &state
        .projects
        .as_ref()
        .ok_or_else(Error::unsupported)?
        .watches;
    let mut registry = registry.lock().map_err(|_| Error::internal())?;
    registry.prune();
    if name == "project_unwatch" {
        return release(&mut registry, owner, &project, &args);
    }
    let requested = args["watchId"].as_str();
    let token = match requested {
        Some(id) if registry.entries.contains_key(id) => id.to_owned(),
        _ => create(&mut registry, state, owner, &project)?,
    };
    snapshot(&mut registry, state, owner, &project, &token, requested)
}

fn snapshot(
    registry: &mut Registry,
    state: &State,
    owner: usize,
    project: &Project,
    token: &str,
    requested: Option<&str>,
) -> Result<Value, Error> {
    let watch = registry
        .entries
        .get_mut(token)
        .ok_or_else(Error::internal)?;
    if watch.owner != owner || watch.project != project.id || watch.revision != project.revision {
        return Err(Error::forbidden("EACCES"));
    }
    if let Err(error) = tree::verify(state, project, &watch.roots) {
        registry.entries.remove(token);
        return Err(error);
    }
    let gap = requested.is_some_and(|id| id != token);
    let overflow = poll(watch)?;
    tree::verify(state, project, &watch.roots)?;
    watch.touched = Instant::now();
    Ok(
        json!({"watchId":token,"version":format!("{token}:{}",watch.version),"gap":gap || overflow,"truncated":watch.truncated}),
    )
}
pub async fn call(
    state: &Arc<State>,
    owner: usize,
    name: &str,
    args: Value,
) -> Result<Value, Error> {
    static WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    let permit = WORK
        .try_acquire()
        .map_err(|_| Error::too_many("PROJECT_WATCH_BUSY"))?;
    let state = state.clone();
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        observe(&state, owner, &name, args)
    })
    .await
    .map_err(|_| Error::internal())?
}
fn authorized(state: &State, owner: usize, args: &Value) -> Result<Project, Error> {
    let id = args["projectId"].as_str().ok_or_else(Error::invalid)?;
    let project = state
        .projects
        .as_ref()
        .ok_or_else(Error::unsupported)?
        .get(owner, id)?;
    service::authorize(state, owner, &project)?;
    if args["revision"].as_u64() != Some(project.revision) {
        return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
    }
    Ok(project)
}
fn create(
    registry: &mut Registry,
    state: &State,
    owner: usize,
    project: &Project,
) -> Result<String, Error> {
    if registry.entries.len() >= 16 {
        return Err(Error::too_many("PROJECT_WATCH_LIMIT"));
    }
    let roots = tree::roots(state, project)?;
    let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
    let (directories, truncated) = tree::scan(&fd, &roots)?;
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random).map_err(|_| Error::internal())?;
    let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
    registry.entries.insert(
        token.clone(),
        Watch {
            owner,
            project: project.id.clone(),
            revision: project.revision,
            touched: Instant::now(),
            roots,
            fd,
            directories,
            version: 0,
            truncated,
            needs_rescan: false,
        },
    );
    Ok(token)
}
fn release(
    registry: &mut Registry,
    owner: usize,
    project: &Project,
    args: &Value,
) -> Result<Value, Error> {
    let token = args["watchId"].as_str().ok_or_else(Error::invalid)?;
    if let Some(watch) = registry.entries.get(token) {
        if watch.owner != owner || watch.project != project.id {
            return Err(Error::forbidden("EACCES"));
        }
        registry.entries.remove(token);
    }
    Ok(json!({"released":true}))
}
fn poll(watch: &mut Watch) -> Result<bool, Error> {
    let (changed, rescan, overflow) = drain(watch)?;
    if changed {
        watch.version += 1;
    }
    watch.needs_rescan |= rescan;
    if watch.needs_rescan {
        rebuild(watch)?;
    }
    Ok(overflow)
}
fn drain(watch: &Watch) -> Result<(bool, bool, bool), Error> {
    let mut buffer = [MaybeUninit::uninit(); 65536];
    let mut reader = inotify::Reader::new(&watch.fd, &mut buffer);
    let (mut changed, mut rescan, mut overflow) = (false, false, false);
    for _ in 0..8192 {
        let event = match reader.next() {
            Ok(event) => event,
            Err(rustix::io::Errno::WOULDBLOCK) => break,
            Err(error) => return Err(error.into()),
        };
        if event.events().contains(ReadFlags::QUEUE_OVERFLOW) {
            changed = true;
            rescan = true;
            overflow = true;
            continue;
        }
        let Some(directory) = watch.directories.get(&event.wd()) else {
            continue;
        };
        if !tree::visible(directory, event.file_name()) {
            continue;
        }
        changed = true;
        rescan |= event.events().intersects(
            ReadFlags::ISDIR | ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF | ReadFlags::IGNORED,
        );
    }
    Ok((changed, rescan, overflow))
}
fn rebuild(watch: &mut Watch) -> Result<(), Error> {
    let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
    let (directories, truncated) = tree::scan(&fd, &watch.roots)?;
    watch.fd = fd;
    watch.directories = directories;
    watch.truncated = truncated;
    watch.needs_rescan = false;
    Ok(())
}
