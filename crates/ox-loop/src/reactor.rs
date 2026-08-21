use std::cell::Cell;
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
/// First token allocatable to sibling-runtime internal sources; `SIGNAL_TOKEN`
/// is excluded because ox-loop's own signal self-pipe owns it.
const INTERNAL_TOKEN_FIRST: usize = SIGNAL_TOKEN.0 + 1;

/// Thin owner of the platform reactor and its cross-thread wakeup source.
pub struct Reactor {
    poll: Poll,
    waker: Arc<Waker>,
    next_internal: Cell<usize>,
}

impl Reactor {
    /// Creates a poll registry and reserves the cross-thread wake token.
    pub fn new() -> Result<Self> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE_TOKEN)?);
        Ok(Self {
            poll,
            waker,
            next_internal: Cell::new(INTERNAL_TOKEN_FIRST),
        })
    }

    /// Registers a caller-owned source.
    ///
    /// Mio's epoll backend registers with `EPOLLET` unconditionally
    /// (edge-triggered), so a readiness notification is delivered once per
    /// state change and is not re-reported while buffered data remains.
    /// Callers MUST drain readable sources until `WouldBlock` before
    /// returning; [`crate::Loop`] enforces this by re-invoking callbacks that
    /// report [`crate::DrainState::KeepDraining`].
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
    ///
    /// The same edge-triggered (`EPOLLET`) and drain-until-WouldBlock contract
    /// applies as for [`Reactor::register`].
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

    /// Allocates the next token in the reserved internal range `2..IO_TOKEN_START`
    /// (skipping the signal self-pipe token), advancing the internal cursor.
    ///
    /// Internal tokens are privileged: they are not subject to the public-range
    /// guard on [`Reactor::register`], so sibling runtime crates can claim them
    /// without ever colliding with caller-owned I/O tokens that begin at
    /// `IO_TOKEN_START`.
    pub fn next_internal_token(&self) -> Result<Token> {
        let current = self.next_internal.get();
        if current >= IO_TOKEN_START {
            return Err(Error::ReservedToken(Token(current)));
        }
        self.next_internal.set(current + 1);
        Ok(Token(current))
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
