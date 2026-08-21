//! Pure-Rust core of the `vim.uv` engine.
//!
//! This task implements libuv-style loop modes, lifecycle-safe handles, timers,
//! phase watchers, cross-thread async wakeups, signals, and the documented misc
//! subset. Stream, filesystem, process, DNS, and IPC handles are intentionally
//! left for the next layer.

#![forbid(unsafe_code)]

mod async_handle;
mod aux_handles;
mod handle;
pub mod misc;
mod signal;
mod timer;
mod uv_loop;

pub use async_handle::{Async, AsyncSender};
pub use aux_handles::{Check, Idle, Prepare};
pub use handle::{Handle, HandleId};
pub use signal::Signal;
pub use timer::Timer;
pub use uv_loop::{RunMode, UvLoop};

/// Result type for loop and handle operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Operational failures from the safe libuv-compatible layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying ox-loop pump failed.
    #[error(transparent)]
    Loop(#[from] ox_loop::Error),
    /// A platform I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The identity no longer names a live registry entry.
    #[error("invalid or closed handle {0:?}")]
    InvalidHandle(HandleId),
    /// An operation was attempted on a different handle kind.
    #[error("handle {id:?} is not a {expected} handle")]
    WrongHandleKind {
        /// Supplied handle identity.
        id: HandleId,
        /// Required kind name.
        expected: &'static str,
    },
    /// New work cannot be started after close was requested.
    #[error("handle {0:?} is closing")]
    ClosingHandle(HandleId),
    /// Close was requested more than once.
    #[error("handle {0:?} is already closing")]
    AlreadyClosing(HandleId),
    /// `again` requires a timer that has been started before.
    #[error("timer {0:?} has never been started")]
    TimerNeverStarted(HandleId),
    /// `again` requires a nonzero repeat interval.
    #[error("timer {0:?} has no repeat interval")]
    TimerNotRepeating(HandleId),
    /// A duration could not be represented as an `Instant` deadline.
    #[error("timer deadline is outside the monotonic clock range")]
    TimeOverflow,
    /// Wall-clock time preceded the Unix epoch.
    #[error("system clock is before the Unix epoch")]
    ClockBeforeEpoch,
    /// A required platform environment variable is absent.
    #[error("environment variable {0} is not set")]
    MissingEnvironment(&'static str),
    /// The signal cannot be intercepted safely on this platform.
    #[error("signal {0} cannot be registered")]
    InvalidSignal(i32),
    /// The loop cannot be pumped recursively from one of its callbacks.
    #[error("event loop is already running")]
    LoopAlreadyRunning,
    /// All owner-allocated 32-bit handle identities have been consumed.
    #[error("handle identity space is exhausted")]
    HandleLimit,
}

/// Callback failure captured without unwinding through the reactor.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CallbackError {
    /// A callback returned a binding-visible error.
    #[error("{0}")]
    Failed(String),
    /// A callback panicked; the panic was caught at the loop boundary.
    #[error("callback panicked: {0}")]
    Panic(String),
}

impl From<Error> for CallbackError {
    fn from(error: Error) -> Self {
        Self::Failed(error.to_string())
    }
}

impl CallbackError {
    /// Creates a callback failure from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    pub(crate) fn panic(payload: Box<dyn std::any::Any + Send>) -> Self {
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_owned());
        Self::Panic(message)
    }
}

/// Loop phase in which a callback error occurred.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallbackPhase {
    /// Timer phase.
    Timer,
    /// Pending async delivery.
    Async,
    /// Pending signal delivery, carrying its signal number.
    Signal(i32),
    /// Idle phase.
    Idle,
    /// Prepare phase.
    Prepare,
    /// Check phase.
    Check,
    /// Deferred close phase.
    Close,
}

/// Binding-visible callback error event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallbackErrorEvent {
    /// Handle whose callback failed.
    pub id: HandleId,
    /// Phase in which it failed.
    pub phase: CallbackPhase,
    /// Captured returned error or panic.
    pub error: CallbackError,
}

#[cfg(test)]
mod tests;
