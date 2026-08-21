//! A safe, single-writer event loop built on `mio`.
//!
//! The loop owns all callback execution. Other threads can only post work through
//! [`DeferredScheduler`], whose `mio::Waker` wakes the loop thread.

#![forbid(unsafe_code)]

mod events;
mod reactor;
mod r#loop;
mod signal;
mod timer;
mod work;

pub use events::{Event, MultiQueue, Owner};
pub use r#loop::{Loop, Readiness, WaitOutcome};
pub use reactor::{IO_TOKEN_START, Reactor, SIGNAL_TOKEN, WAKE_TOKEN};
pub use signal::Signals;
pub use timer::{TimerEntry, TimerHeap, TimerId};
pub use work::{DeferredScheduler, Work, WorkQueues};

/// Errors produced by the reactor and its queueing layers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An operating-system or mio operation failed.
    #[error("event-loop I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// A caller attempted to claim an internal reactor token.
    #[error("token {0:?} is reserved for an internal event-loop source")]
    ReservedToken(mio::Token),
    /// An event operation named an owner not present in this MultiQueue.
    #[error("unknown event owner {0:?}")]
    UnknownOwner(Owner),
    /// The permanent root queue cannot be removed.
    #[error("the root event owner cannot be removed")]
    RootOwnerRemoval,
    /// A readiness callback already owns this token.
    #[error("readiness callback is already registered for {0:?}")]
    DuplicateReadiness(mio::Token),
    /// The signal number is invalid or cannot be safely intercepted.
    #[error("signal {0} cannot be registered")]
    InvalidSignal(i32),
}

/// Result type shared by ox-loop interfaces.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests;
