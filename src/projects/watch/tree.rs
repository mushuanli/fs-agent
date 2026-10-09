//! A watch is installed through an open descriptor, never an unchecked pathname.
use super::*;
use std::{ffi::CStr, path::PathBuf};
pub(super) struct Root {
    pub export: Arc<crate::fs::Export>,
    pub path: String,
    pub at: String,
    pub identity: (u64, u64),
    pub excluded: Vec<PathBuf>,
}
struct View {
    relative: String,
    shadowed: Vec<String>,
}
pub(super) struct Directory {
    _file: File,
    views: Vec<View>,
}
fn identity(file: &File) -> Result<(u64, u64), Error> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}
pub(super) fn roots(state: &State, project: &Project) -> Result<Vec<Root>, Error> {
    std::iter::once((project.alias.as_str(), project.path.as_str(), "/workspace"))
        .chain(
            project
                .mounts
                .iter()
                .map(|m| (m.alias.as_str(), m.path.as_str(), m.at.as_str())),
        )
        .map(|(alias, path, at)| root(state, alias, path, at))
        .collect()
}
fn root(state: &State, alias: &str, path: &str, at: &str) -> Result<Root, Error> {
    let export = state.exports.get(alias).ok_or_else(Error::invalid)?.clone();
    let file = export.open_dir(path)?;
    let physical = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    if state
        .harness
        .private_homes()
        .any(|home| physical.starts_with(home))
    {
        return Err(Error::forbidden("WATCH_PRIVATE_ROOT"));
    }
    let excluded = state
        .harness
        .private_homes()
        .filter(|home| home.starts_with(&physical))
        .map(|home| home.to_path_buf())
        .collect();
    Ok(Root {
        export,
        path: path.into(),
        at: at.into(),
        identity: identity(&file)?,
        excluded,
    })
}
pub(super) fn verify(state: &State, project: &Project, roots: &[Root]) -> Result<(), Error> {
    let current = state
        .projects
        .as_ref()
        .ok_or_else(Error::unsupported)?
        .get(project.owner, &project.id)?;
    if current.revision != project.revision {
        return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
    }
    service::authorize(state, project.owner, &current)?;
    for root in roots {
        if identity(&root.export.open_dir(&root.path)?)? != root.identity {
            return Err(Error::conflict("PROJECT_DIRECTORY_CHANGED"));
        }
    }
    Ok(())
}
struct Scan {
    directories: BTreeMap<i32, Directory>,
    pending: Vec<(usize, String, String)>,
    entries: usize,
    started: Instant,
}
impl Scan {
    fn full(&self) -> bool {
        self.directories.len() >= 2048
            || self.entries >= 50000
            || self.started.elapsed() > Duration::from_secs(2)
    }
    fn directory(
        &mut self,
        fd: &OwnedFd,
        roots: &[Root],
        index: usize,
        path: String,
        relative: String,
    ) -> Result<(), Error> {
        let root = &roots[index];
        let file = match root.export.open_dir(&path) {
            Ok(file) => file,
            Err(error) if error.is_not_found() => return Ok(()),
            Err(error) => return Err(error),
        };
        let proc = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let physical = std::fs::read_link(&proc)?;
        if root.excluded.iter().any(|home| physical.starts_with(home)) {
            return Ok(());
        }
        let view = View {
            relative,
            shadowed: roots
                .iter()
                .filter_map(|other| other.at.strip_prefix(&(root.at.clone() + "/")))
                .map(str::to_owned)
                .collect(),
        };
        let wd = inotify::add_watch(fd, &proc, flags())?;
        self.children(&proc, index, &path, &view)?;
        self.insert(wd, file, view);
        Ok(())
    }
    fn insert(&mut self, wd: i32, file: File, view: View) {
        match self.directories.get_mut(&wd) {
            Some(directory) => directory.views.push(view),
            None => {
                self.directories.insert(
                    wd,
                    Directory {
                        _file: file,
                        views: vec![view],
                    },
                );
            }
        }
    }
    fn children(
        &mut self,
        proc: &PathBuf,
        index: usize,
        path: &str,
        view: &View,
    ) -> Result<(), Error> {
        for entry in std::fs::read_dir(proc)? {
            if self.full() {
                break;
            }
            self.entries += 1;
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !visible_name(view, name) {
                continue;
            }
            if entry.file_type()?.is_dir() {
                let child = join(&view.relative, name);
                self.pending.push((index, join(path, name), child));
            }
        }
        Ok(())
    }
}
fn flags() -> WatchFlags {
    WatchFlags::MODIFY
        | WatchFlags::CLOSE_WRITE
        | WatchFlags::ATTRIB
        | WatchFlags::CREATE
        | WatchFlags::DELETE
        | WatchFlags::MOVED_FROM
        | WatchFlags::MOVED_TO
        | WatchFlags::DELETE_SELF
        | WatchFlags::MOVE_SELF
        | WatchFlags::ONLYDIR
}
pub(super) fn scan(
    fd: &OwnedFd,
    roots: &[Root],
) -> Result<(BTreeMap<i32, Directory>, bool), Error> {
    let pending = roots
        .iter()
        .enumerate()
        .map(|(i, root)| (i, root.path.clone(), String::new()))
        .collect();
    let mut scan = Scan {
        directories: BTreeMap::new(),
        pending,
        entries: 0,
        started: Instant::now(),
    };
    while let Some((index, path, relative)) = scan.pending.pop() {
        if scan.full() {
            return Ok((scan.directories, true));
        }
        scan.directory(fd, roots, index, path, relative)?;
    }
    let truncated = scan.full();
    Ok((scan.directories, truncated))
}
fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}
fn visible_name(view: &View, name: &str) -> bool {
    let relative = join(&view.relative, name);
    !name.starts_with('.')
        && !view
            .shadowed
            .iter()
            .any(|at| relative == *at || relative.starts_with(&(at.clone() + "/")))
}
pub(super) fn visible(directory: &Directory, name: Option<&CStr>) -> bool {
    let Some(name) = name else {
        return true;
    };
    let Ok(name) = std::str::from_utf8(name.to_bytes()) else {
        return false;
    };
    directory.views.iter().any(|view| visible_name(view, name))
}
