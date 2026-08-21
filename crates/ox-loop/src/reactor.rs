use std::sync::Arc;
use std::time::Duration;

use mio::event::Source;
use mio::{Events, Interest, Poll, Token, Waker};

use crate::{Error, Result};

/// Cross-thread work notification. Token zero is never available to clients.
pub const WAKE_TOKEN: Token = Token(0);
/// Signal self-pipe readiness. The remainder of 1..1023 is reserved for future
/// internal sources, so public I/O tokens remain stable as modules are added.
pub const SIGNAL_TOKEN: Token = Token(1);
/// First token available to caller-owned mio sources.
pub const IO_TOKEN_START: usize = 1024;

/// Thin owner of the platform reactor and its cross-thread wakeup source.
pub struct Reactor {
    poll: Poll,
    waker: Arc<Waker>,
}

impl Reactor {
    /// Creates a poll registry and reserves the cross-thread wake token.
    pub fn new() -> Result<Self> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE_TOKEN)?);
        Ok(Self { poll, waker })
    }

    /// Registers a caller-owned source. Mio readiness is level-triggered on the
    /// supported platform backends; callbacks must still drain until WouldBlock.
    pub fn register<S: Source + ?Sized>(
        &self,
        source: &mut S,
        token: Token,
        interest: Interest,
    ) -> Result<()> {
        if token.0 < IO_TOKEN_START {
            return Err(Error::ReservedToken(token));
        }
        self.poll.registry().register(source, token, interest)?;
        Ok(())
    }

    /// Changes a caller-owned source's token or readiness interests.
    pub fn reregister<S: Source + ?Sized>(
        &self,
        source: &mut S,
        token: Token,
        interest: Interest,
    ) -> Result<()> {
        if token.0 < IO_TOKEN_START {
            return Err(Error::ReservedToken(token));
        }
        self.poll.registry().reregister(source, token, interest)?;
        Ok(())
    }

    /// Removes a source from this reactor.
    pub fn deregister<S: Source + ?Sized>(&self, source: &mut S) -> Result<()> {
        self.poll.registry().deregister(source)?;
        Ok(())
    }

    pub(crate) fn register_internal<S: Source + ?Sized>(
        &self,
        source: &mut S,
        token: Token,
        interest: Interest,
    ) -> Result<()> {
        self.poll.registry().register(source, token, interest)?;
        Ok(())
    }

    /// Waits for readiness, bounded by `timeout` when supplied.
    pub fn poll(&mut self, events: &mut Events, timeout: Option<Duration>) -> Result<()> {
        self.poll.poll(events, timeout)?;
        Ok(())
    }

    /// Returns the shared waker used by thread-safe producers.
    pub fn waker(&self) -> Arc<Waker> {
        Arc::clone(&self.waker)
    }
}
