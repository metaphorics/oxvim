//! Portable stat-snapshot filesystem event and poll handles.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::fs::{FsError, FsResult, Stat, lstat, stat};
use crate::pool::{LoopPoster, PostError, UvLoopPoster};
use crate::{CallbackError, Handle, HandleId, UvLoop};

const FS_EVENT_INTERVAL: Duration = Duration::from_secs(1);

/// Options controlling [`FsEvent`] snapshot coverage.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FsEventOptions {
    /// Watch only the directory entry named by the supplied path.
    ///
    /// For a directory this excludes changes to its children. For a file or
    /// symlink this is equivalent to the default behavior.
    pub watch_entry: bool,
    /// Force stat-based polling instead of a platform notification backend.
    ///
    /// This portable implementation always uses stat snapshots, so the
    /// requested guarantee is already active when this option is set.
    pub stat: bool,
    /// Include descendants of a watched directory. Symlink directories are
    /// recorded as entries but are never traversed.
    pub recursive: bool,
}

/// Change classification reported by [`FsEvent`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsEventRecord {
    /// Path of the changed file relative to the watched directory.
    pub filename: PathBuf,
    /// The file's metadata changed.
    pub change: bool,
    /// The file was renamed or its identity changed.
    pub rename: bool,
}

/// Previous and current snapshots reported by [`FsPoll`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsPollRecord {
    /// Previous stat snapshot.
    pub previous: Option<Stat>,
    /// Current stat snapshot.
    pub current: Option<Stat>,
}

/// Watcher lifecycle failure.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] FsError),
    /// Completion could not be posted to the loop.
    #[error(transparent)]
    Post(#[from] PostError),
    /// The polling thread could not be started.
    #[error("watcher thread could not be started: {0}")]
    Spawn(String),
    /// The watcher has already stopped.
    #[error("watcher has already stopped")]
    Stopped,
    /// A requested watcher mode is unavailable on this target.
    #[error("filesystem watcher mode is unsupported: {0}")]
    Unsupported(&'static str),
    /// The owning loop rejected a handle lifecycle operation.
    #[error(transparent)]
    Loop(#[from] crate::Error),
}

struct WatchThread {
    id: HandleId,
    active: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
    poster: UvLoopPoster,
    accounted: AtomicBool,
    deactivated: AtomicBool,
}

impl WatchThread {
    fn stop_once(&self) -> bool {
        let was_active = self.active.swap(false, Ordering::AcqRel);
        let thread = {
            let mut thread = match self.thread.lock() {
                Ok(thread) => thread,
                Err(poisoned) => poisoned.into_inner(),
            };
            thread.take()
        };
        if let Some(thread) = thread {
            let _ = thread.join();
        }
        if self.accounted.swap(false, Ordering::AcqRel) {
            self.poster.end();
        }
        was_active
    }

    fn deactivate(&self, uv_loop: &mut UvLoop) -> crate::Result<()> {
        uv_loop.set_external_active(self.id, false)?;
        self.deactivated.store(true, Ordering::Release);
        Ok(())
    }

    fn stop(&self, uv_loop: &mut UvLoop) -> Result<(), WatchError> {
        let stopped = self.stop_once();
        self.deactivate(uv_loop)?;
        if stopped { Ok(()) } else { Err(WatchError::Stopped) }
    }
}

impl Drop for WatchThread {
    fn drop(&mut self) {
        self.stop_once();
        if !self.deactivated.swap(true, Ordering::AcqRel) {
            let id = self.id;
            let _ = self.poster.post(Box::new(move |uv_loop| {
                let _ = uv_loop.set_external_active(id, false);
            }));
        }
    }
}

/// Portable filesystem event watcher backed by one-second stat snapshots.
///
/// Snapshot polling is deliberately the portable backend seam; a future native
/// backend may replace it without changing callback delivery. See
/// `luv-fs-event-handle` in `runtime/doc/luvref.txt`.
pub struct FsEvent {
    id: HandleId,
    path: PathBuf,
    options: FsEventOptions,
    watch: WatchThread,
}

impl FsEvent {
    /// Starts watching `path`; callbacks are posted to the loop pending queue.
    ///
    /// Existence or identity transitions are `rename`; other metadata changes
    /// are `change`. Directory events carry paths relative to the watched
    /// directory. See `uv.fs_event_start()` in `runtime/doc/luvref.txt`.
    pub fn start<C>(
        uv_loop: &mut UvLoop,
        path: impl Into<PathBuf>,
        options: FsEventOptions,
        callback: C,
    ) -> Result<Self, WatchError>
    where
        C: FnMut(&mut UvLoop, FsResult<FsEventRecord>) + Send + 'static,
    {
        if options.watch_entry && options.recursive {
            return Err(WatchError::Unsupported(
                "watch_entry cannot be combined with recursive traversal",
            ));
        }
        let path = path.into();
        let poster = uv_loop.completion_poster();
        poster.begin()?;
        let id = match uv_loop.allocate_external(true) {
            Ok(id) => id,
            Err(error) => {
                poster.end();
                return Err(error.into());
            }
        };
        let worker_poster = poster.clone();
        let active = Arc::new(AtomicBool::new(true));
        let callback = Arc::new(Mutex::new(callback));
        let thread_active = Arc::clone(&active);
        let thread_callback = Arc::clone(&callback);
        let thread_path = path.clone();
        let thread = match thread::Builder::new()
            .name("ox-uv-fs-event".into())
            .spawn(move || {
                let mut previous = event_snapshot(&thread_path, options).ok();
                while sleep_while_active(&thread_active, FS_EVENT_INTERVAL) {
                    match event_snapshot(&thread_path, options) {
                        Ok(current) => {
                            if let Some(previous) = previous.as_ref() {
                                for record in event_changes(previous, &current) {
                                    post_if_active(
                                        &worker_poster,
                                        id,
                                        &thread_active,
                                        &thread_callback,
                                        Ok(record),
                                    );
                                }
                            } else {
                                post_if_active(
                                    &worker_poster,
                                    id,
                                    &thread_active,
                                    &thread_callback,
                                    Ok(FsEventRecord {
                                        filename: watched_filename(&thread_path),
                                        change: false,
                                        rename: true,
                                    }),
                                );
                            }
                            previous = Some(current);
                        }
                        Err(error) => {
                            if previous.take().is_some() {
                                post_if_active(
                                    &worker_poster,
                                    id,
                                    &thread_active,
                                    &thread_callback,
                                    Err(error),
                                );
                            }
                        }
                    }
                }
            }) {
            Ok(thread) => thread,
            Err(error) => {
                abandon_start(uv_loop, id, &poster);
                return Err(WatchError::Spawn(error.to_string()));
            }
        };
        Ok(Self {
            id,
            path,
            options,
            watch: WatchThread {
                id,
                active,
                thread: Mutex::new(Some(thread)),
                poster,
                accounted: AtomicBool::new(true),
                deactivated: AtomicBool::new(false),
            },
        })
    }

    /// Stops delivery, joins the polling thread, and removes its liveness.
    /// See `uv.fs_event_stop()` in `runtime/doc/luvref.txt`.
    pub fn stop(&self, uv_loop: &mut UvLoop) -> Result<(), WatchError> {
        self.watch.stop(uv_loop)
    }

    /// Returns the monitored path. See `uv.fs_event_getpath()` in `runtime/doc/luvref.txt`.
    pub fn path(&self) -> &Path { &self.path }

    /// Returns the options governing this watcher.
    pub fn options(&self) -> FsEventOptions { self.options }
}

impl Handle for FsEvent {
    fn id(&self) -> HandleId { self.id }

    fn close(&self, uv_loop: &mut UvLoop) -> crate::Result<()> {
        if uv_loop.is_closing(self.id) {
            return uv_loop.close(
                self.id,
                None::<fn(&mut UvLoop, HandleId) -> Result<(), CallbackError>>,
            );
        }
        self.watch.stop_once();
        self.watch.deactivate(uv_loop)?;
        uv_loop.close(
            self.id,
            None::<fn(&mut UvLoop, HandleId) -> Result<(), CallbackError>>,
        )
    }

    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> crate::Result<()>
    where
        F: FnOnce(&mut UvLoop, HandleId) -> Result<(), CallbackError> + 'static,
    {
        if uv_loop.is_closing(self.id) {
            return uv_loop.close(self.id, Some(callback));
        }
        self.watch.stop_once();
        self.watch.deactivate(uv_loop)?;
        uv_loop.close(self.id, Some(callback))
    }
}

/// Interval-based stat watcher.
/// See `luv-fs-poll-handle` in `runtime/doc/luvref.txt`.
pub struct FsPoll {
    id: HandleId,
    path: PathBuf,
    watch: WatchThread,
}

impl FsPoll {
    /// Starts polling at `interval`; zero milliseconds is normalized to one.
    ///
    /// See `uv.fs_poll_start()` in `runtime/doc/luvref.txt`.
    pub fn start<C>(
        uv_loop: &mut UvLoop,
        path: impl Into<PathBuf>,
        interval: Duration,
        callback: C,
    ) -> Result<Self, WatchError>
    where
        C: FnMut(&mut UvLoop, FsResult<FsPollRecord>) + Send + 'static,
    {
        let path = path.into();
        let poster = uv_loop.completion_poster();
        poster.begin()?;
        let id = match uv_loop.allocate_external(true) {
            Ok(id) => id,
            Err(error) => {
                poster.end();
                return Err(error.into());
            }
        };
        let worker_poster = poster.clone();
        let interval = interval.max(Duration::from_millis(1));
        let active = Arc::new(AtomicBool::new(true));
        let callback = Arc::new(Mutex::new(callback));
        let thread_active = Arc::clone(&active);
        let thread_callback = Arc::clone(&callback);
        let thread_path = path.clone();
        let thread = match thread::Builder::new()
            .name("ox-uv-fs-poll".into())
            .spawn(move || {
                let mut previous = snapshot_follow(&thread_path).ok();
                while sleep_while_active(&thread_active, interval) {
                    let current_result = snapshot_follow(&thread_path);
                    let current = current_result.as_ref().ok().cloned();
                    if current != previous {
                        let result = match current_result {
                            Ok(_) => Ok(FsPollRecord {
                                previous: previous.clone(),
                                current: current.clone(),
                            }),
                            Err(error) => Err(error),
                        };
                        post_if_active(
                            &worker_poster,
                            id,
                            &thread_active,
                            &thread_callback,
                            result,
                        );
                    }
                    previous = current;
                }
            }) {
            Ok(thread) => thread,
            Err(error) => {
                abandon_start(uv_loop, id, &poster);
                return Err(WatchError::Spawn(error.to_string()));
            }
        };
        Ok(Self {
            id,
            path,
            watch: WatchThread {
                id,
                active,
                thread: Mutex::new(Some(thread)),
                poster,
                accounted: AtomicBool::new(true),
                deactivated: AtomicBool::new(false),
            },
        })
    }

    /// Stops delivery, joins the polling thread, and removes its liveness.
    /// See `uv.fs_poll_stop()` in `runtime/doc/luvref.txt`.
    pub fn stop(&self, uv_loop: &mut UvLoop) -> Result<(), WatchError> {
        self.watch.stop(uv_loop)
    }

    /// Returns the monitored path. See `uv.fs_poll_getpath()` in `runtime/doc/luvref.txt`.
    pub fn path(&self) -> &Path { &self.path }
}

impl Handle for FsPoll {
    fn id(&self) -> HandleId { self.id }

    fn close(&self, uv_loop: &mut UvLoop) -> crate::Result<()> {
        if uv_loop.is_closing(self.id) {
            return uv_loop.close(
                self.id,
                None::<fn(&mut UvLoop, HandleId) -> Result<(), CallbackError>>,
            );
        }
        self.watch.stop_once();
        self.watch.deactivate(uv_loop)?;
        uv_loop.close(
            self.id,
            None::<fn(&mut UvLoop, HandleId) -> Result<(), CallbackError>>,
        )
    }

    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> crate::Result<()>
    where
        F: FnOnce(&mut UvLoop, HandleId) -> Result<(), CallbackError> + 'static,
    {
        if uv_loop.is_closing(self.id) {
            return uv_loop.close(self.id, Some(callback));
        }
        self.watch.stop_once();
        self.watch.deactivate(uv_loop)?;
        uv_loop.close(self.id, Some(callback))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EventSnapshot {
    entries: BTreeMap<PathBuf, Stat>,
}

fn event_snapshot(path: &Path, options: FsEventOptions) -> FsResult<EventSnapshot> {
    let metadata = fs::symlink_metadata(path).map_err(FsError::from)?;
    let root_stat = snapshot_link(path)?;
    let mut entries = BTreeMap::new();
    if options.watch_entry || !metadata.file_type().is_dir() {
        entries.insert(watched_filename(path), root_stat);
        return Ok(EventSnapshot { entries });
    }

    let mut directories = vec![path.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut children = fs::read_dir(&directory)
            .map_err(FsError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(FsError::from)?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let child_path = child.path();
            let child_metadata = match fs::symlink_metadata(&child_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(FsError::from(error)),
            };
            let child_stat = match snapshot_link(&child_path) {
                Ok(snapshot) => snapshot,
                Err(error) if error.name == "ENOENT" => continue,
                Err(error) => return Err(error),
            };
            let relative = match child_path.strip_prefix(path) {
                Ok(relative) => relative.to_path_buf(),
                Err(_) => child_path.clone(),
            };
            entries.insert(relative, child_stat);
            if options.recursive && child_metadata.file_type().is_dir() {
                directories.push(child_path);
            }
        }
        if !options.recursive {
            break;
        }
    }
    Ok(EventSnapshot { entries })
}

fn event_changes(previous: &EventSnapshot, current: &EventSnapshot) -> Vec<FsEventRecord> {
    let paths: BTreeSet<_> = previous
        .entries
        .keys()
        .chain(current.entries.keys())
        .cloned()
        .collect();
    paths
        .into_iter()
        .filter_map(|filename| {
            let old = previous.entries.get(&filename);
            let new = current.entries.get(&filename);
            if old == new {
                return None;
            }
            let rename = identity(old) != identity(new);
            Some(FsEventRecord { filename, change: !rename, rename })
        })
        .collect()
}

fn snapshot_link(path: &Path) -> FsResult<Stat> { lstat(path) }
fn snapshot_follow(path: &Path) -> FsResult<Stat> { stat(path) }
fn identity(snapshot: Option<&Stat>) -> Option<(u64, u64)> {
    snapshot.map(|value| (value.dev, value.ino))
}

fn watched_filename(path: &Path) -> PathBuf {
    match path.file_name() {
        Some(filename) => PathBuf::from(filename),
        None => path.to_path_buf(),
    }
}

fn abandon_start(uv_loop: &mut UvLoop, id: HandleId, poster: &UvLoopPoster) {
    poster.end();
    let _ = uv_loop.set_external_active(id, false);
    let _ = uv_loop.close(
        id,
        None::<fn(&mut UvLoop, HandleId) -> Result<(), CallbackError>>,
    );
}

fn sleep_while_active(active: &AtomicBool, duration: Duration) -> bool {
    const QUANTUM: Duration = Duration::from_millis(50);
    let mut remaining = duration;
    while active.load(Ordering::Acquire) && !remaining.is_zero() {
        let step = remaining.min(QUANTUM);
        thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    active.load(Ordering::Acquire)
}

fn post_if_active<C, T>(
    poster: &UvLoopPoster,
    id: HandleId,
    active: &Arc<AtomicBool>,
    callback: &Arc<Mutex<C>>,
    result: FsResult<T>,
)
where
    C: FnMut(&mut UvLoop, FsResult<T>) + Send + 'static,
    T: Send + 'static,
{
    let active = Arc::clone(active);
    let callback = Arc::clone(callback);
    let _ = poster.post(Box::new(move |uv_loop| {
        if active.load(Ordering::Acquire) && uv_loop.is_active(id) {
            if let Ok(mut callback) = callback.lock() {
                callback(uv_loop, result);
            }
        }
    }));
}
