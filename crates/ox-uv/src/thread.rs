//! Safe thread creation with one independent [`UvLoop`] per thread.

use std::fmt;
use std::thread::{self, JoinHandle, ThreadId};

use crate::UvLoop;

/// Stable process-local thread identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ThreadIdentity(ThreadId);

/// Thread creation or lifecycle failure.
#[derive(Debug, thiserror::Error)]
pub enum ThreadError {
    /// Thread spawn failed.
    #[error("thread could not be started: {0}")]
    Spawn(String),
    /// Loop creation failed.
    #[error("thread event loop could not be created: {0}")]
    Loop(String),
    /// Thread entry panicked.
    #[error("thread entry panicked")]
    Panicked,
    /// Thread was detached.
    #[error("thread was detached")]
    Detached,
    /// Thread has already been joined.
    #[error("thread has already been joined")]
    AlreadyJoined,
}

/// Joinable thread created by [`new_thread`].
pub struct Thread<T> {
    identity: ThreadIdentity,
    join: Option<JoinHandle<Result<T, ThreadError>>>,
    detached: bool,
}

impl<T> fmt::Debug for Thread<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Thread").field("identity", &self.identity).field("detached", &self.detached).finish_non_exhaustive()
    }
}

impl<T> Thread<T> {
    /// Returns this thread's process-local identity.
    /// See `uv.thread_equal()` in `runtime/doc/luvref.txt`.
    pub fn identity(&self) -> ThreadIdentity { self.identity }

    /// Waits for the thread and returns its entry value.
    /// See `uv.thread_join()` in `runtime/doc/luvref.txt`.
    pub fn join(&mut self) -> Result<T, ThreadError> {
        if self.detached { return Err(ThreadError::Detached); }
        let join = self.join.take().ok_or(ThreadError::AlreadyJoined)?;
        join.join().map_err(|_| ThreadError::Panicked)?
    }

    /// Detaches the thread by releasing its standard join handle.
    /// See `uv.thread_detach()` in `runtime/doc/luvref.txt`.
    pub fn detach(&mut self) -> Result<(), ThreadError> {
        if self.detached { return Err(ThreadError::Detached); }
        self.join.take().ok_or(ThreadError::AlreadyJoined)?;
        self.detached = true;
        Ok(())
    }

    /// Reports whether the thread was explicitly detached.
    /// See `uv.thread_detach()` in `runtime/doc/luvref.txt`.
    pub fn is_detached(&self) -> bool { self.detached }
}

/// Starts an isolated thread with a newly-created event loop.
///
/// Lua-state isolation remains the binding layer's responsibility. `stack_size`
/// maps directly to `std::thread::Builder`. See `uv.new_thread()` in
/// `runtime/doc/luvref.txt`.
pub fn new_thread<F, T>(stack_size: Option<usize>, entry: F) -> Result<Thread<T>, ThreadError>
where
    F: FnOnce(&mut UvLoop) -> T + Send + 'static,
    T: Send + 'static,
{
    let mut builder = thread::Builder::new().name("ox-uv-thread".into());
    if let Some(stack_size) = stack_size { builder = builder.stack_size(stack_size); }
    let join = builder.spawn(move || {
        let mut uv_loop = UvLoop::new().map_err(|error| ThreadError::Loop(error.to_string()))?;
        Ok(entry(&mut uv_loop))
    }).map_err(|error| ThreadError::Spawn(error.to_string()))?;
    let identity = ThreadIdentity(join.thread().id());
    Ok(Thread { identity, join: Some(join), detached: false })
}

/// Returns the calling thread's identity.
/// See `uv.thread_self()` in `runtime/doc/luvref.txt`.
pub fn thread_self() -> ThreadIdentity { ThreadIdentity(thread::current().id()) }

/// Compares two process-local thread identities.
/// See `uv.thread_equal()` in `runtime/doc/luvref.txt`.
pub fn thread_equal(left: ThreadIdentity, right: ThreadIdentity) -> bool { left == right }
