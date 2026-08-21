//! Main-loop pump ordering.
//!
//! Each iteration maps libuv's phases onto mio as follows: expired timers
//! (timer phase), readiness polling and dispatch (poll phase), then owned
//! MultiQueue processing (check/deferred phase). Timers that become due while
//! poll sleeps are fired before readiness dispatch, preserving timers-before-I/O.
//!
//! # Readiness contract (edge-triggered, drain-until-WouldBlock)
//!
//! mio's `epoll` backend sets `EPOLLET` unconditionally (see
//! `mio::sys::unix::selector::epoll::interests_to_epoll`), so a readiness
//! notification is delivered at most once per state change and is **not**
//! re-reported while buffered data remains. A readiness callback that
//! consumes only part of an available byte stream and returns would leave the
//! remainder stranded until new data arrives.
//!
//! To keep msgpack frame streams from ever starving, every readable source
//! must be drained until `WouldBlock`. This pump enforces that contract: a
//! callback returning [`DrainState::KeepDraining`] is re-invoked
//! synchronously until it returns [`DrainState::Drained`] (i.e. the source
//! reported `WouldBlock`). A partial read therefore advances the buffer that
//! was already available on this edge and cannot strand buffered frames.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mio::Waker as MioWaker;
use mio::event::Source;
use mio::{Events as MioEvents, Interest, Token};

use crate::{
    DeferredScheduler, Error, Event, MultiQueue, Owner, Reactor, Result, SIGNAL_TOKEN, Signals,
    TimerHeap, WAKE_TOKEN, WorkQueues,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Platform-neutral readiness flags captured from a mio event.
pub struct Readiness {
    /// Registered source token.
    pub token: Token,
    /// The source may be read without blocking.
    pub readable: bool,
    /// The source may be written without blocking.
    pub writable: bool,
    /// The source reported an operating-system error.
    pub error: bool,
    /// The source's read side has closed.
    pub read_closed: bool,
    /// The source's write side has closed.
    pub write_closed: bool,
}

/// Terminal state of a selective recursive wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    /// The caller's condition evaluated true.
    ConditionMet,
    /// The supplied timeout elapsed first.
    TimedOut,
    /// The loop was explicitly stopped first.
    Stopped,
}

/// Signal a readiness callback returns to tell the pump whether the source
/// was fully drained on this readiness edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainState {
    /// The source was read (or written) until `WouldBlock`; the OS buffer is
    /// exhausted and no further notification is warranted for this edge.
    Drained,
    /// The callback stopped before observing `WouldBlock` (for example, it
    /// consumed a single buffered message). The pump re-invokes it until it
    /// returns [`DrainState::Drained`], so a partial read can never strand
    /// remaining data under edge-triggered readiness.
    KeepDraining,
}

type ReadinessCallback = Box<dyn FnMut(Readiness, &mut MultiQueue) -> Result<DrainState> + 'static>;

/// Cloneable, thread-safe stop signal for a [`Loop`].
///
/// Obtain one with [`Loop::stop_handle`] before pumping. Calling [`stop`] on
/// any clone sets a shared flag and wakes the reactor, so a loop blocked in
/// [`Loop::run`] returns promptly and a loop that has not started yet returns
/// immediately on entry.
/// [`stop`]: StopHandle::stop
#[derive(Clone)]
pub struct StopHandle {
    stopped: Arc<AtomicBool>,
    waker: Arc<MioWaker>,
}

impl StopHandle {
    /// Requests that the associated loop stop after its current iteration.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        // Wake the reactor so a loop blocked in poll returns and observes the
        // flag instead of waiting out its timeout.
        let _ = self.waker.wake();
    }
}

/// Single-threaded callback pump with thread-safe work ingress.
pub struct Loop {
    reactor: Reactor,
    timers: TimerHeap,
    events: MultiQueue,
    work: WorkQueues,
    signals: Signals,
    callbacks: HashMap<Token, ReadinessCallback>,
    stopped: Arc<AtomicBool>,
}

impl Loop {
    /// Creates a loop without signal subscriptions.
    pub fn new() -> Result<Self> {
        Self::with_signals(&[])
    }

    /// Creates a loop subscribed to `signal_numbers`.
    pub fn with_signals(signal_numbers: &[i32]) -> Result<Self> {
        let reactor = Reactor::new()?;
        let work = WorkQueues::new(reactor.waker());
        let mut signals = Signals::new(signal_numbers)?;
        signals.register(&reactor)?;
        Ok(Self {
            reactor,
            timers: TimerHeap::new(),
            events: MultiQueue::new(),
            work,
            signals,
            callbacks: HashMap::new(),
            stopped: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Returns the reactor for source registration.
    pub fn reactor(&self) -> &Reactor {
        &self.reactor
    }

    /// Registers a sibling-runtime (ox-uv) internal source on a token in the
    /// reserved internal range, skipping the token owned by this loop's own
    /// signal self-pipe.
    ///
    /// This is the privileged seam for internal sources that must not consume a
    /// public caller token: it allocates from `2..IO_TOKEN_START` and returns
    /// the chosen token, so a later caller-owned source at `IO_TOKEN_START`
    /// never collides with it. The returned token is dispatched through the
    /// ordinary callbacks map, so pair it with [`Loop::on_readiness`].
    pub fn register_internal<S: Source + ?Sized>(
        &mut self,
        source: &mut S,
        interest: Interest,
    ) -> Result<Token> {
        let token = self.reactor.next_internal_token()?;
        self.reactor.register_internal(source, token, interest)?;
        Ok(token)
    }

    /// Returns the loop's timer heap.
    pub fn timers(&mut self) -> &mut TimerHeap {
        &mut self.timers
    }

    /// Returns the owned event queues.
    pub fn events(&mut self) -> &mut MultiQueue {
        &mut self.events
    }

    /// Returns the root event owner.
    pub fn root(&self) -> Owner {
        self.events.root()
    }

    /// Returns a thread-safe deferred-work producer.
    pub fn scheduler(&self) -> DeferredScheduler {
        self.work.scheduler()
    }

    /// Returns the work queues for loop-thread fast scheduling.
    pub fn work_queues(&self) -> &WorkQueues {
        &self.work
    }

    /// Associates a readiness callback with a caller-owned token.
    ///
    /// The callback returns [`DrainState`] to signal whether the source was
    /// drained until `WouldBlock`; see the crate-module readiness contract.
    pub fn on_readiness(
        &mut self,
        token: Token,
        callback: impl FnMut(Readiness, &mut MultiQueue) -> Result<DrainState> + 'static,
    ) -> Result<()> {
        if token == WAKE_TOKEN || token == SIGNAL_TOKEN {
            return Err(Error::ReservedToken(token));
        }
        if self.callbacks.contains_key(&token) {
            return Err(Error::DuplicateReadiness(token));
        }
        self.callbacks.insert(token, Box::new(callback));
        Ok(())
    }

    /// Removes and drops a readiness callback.
    pub fn remove_readiness(&mut self, token: Token) -> bool {
        self.callbacks.remove(&token).is_some()
    }

    /// Returns a cloneable, thread-safe stop signal backed by this loop's
    /// reactor waker. Obtain it before pumping; once `run` is under way no
    /// other thread can reach the loop's `&mut self` to request a stop.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle {
            stopped: Arc::clone(&self.stopped),
            waker: self.reactor.waker(),
        }
    }

    /// Requests that `run` return after its current iteration.
    ///
    /// Equivalent to [`StopHandle::stop`]; it is a convenience for the loop
    /// thread itself, which already holds `&mut self`.
    pub fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.reactor.waker().wake();
    }

    /// Pumps until [`StopHandle::stop`] (or [`Loop::stop`]) is requested or an
    /// error occurs.
    ///
    /// A stop requested before `run` is entered is honored: the flag is
    /// observed on entry and never cleared, so `run` returns immediately
    /// without pumping.
    pub fn run(&mut self) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        while !self.stopped.load(Ordering::Acquire) {
            let _ = self.run_once(None)?;
        }
        Ok(())
    }

    /// Pumps one root-queue iteration and returns unhandled signal events.
    pub fn run_once(&mut self, timeout: Option<Duration>) -> Result<Vec<Event>> {
        self.run_once_for(self.events.root(), timeout)
    }

    /// Recursive RPC wait modeled after channel.c:162-166 and
    /// multiqueue.h:25-43. Only the selected channel queue (and descendants)
    /// is drained while sibling events remain represented in the root queue.
    pub fn process_events_until(
        &mut self,
        owner: Owner,
        mut condition: impl FnMut() -> bool,
        timeout: Option<Duration>,
    ) -> Result<WaitOutcome> {
        let started = Instant::now();
        loop {
            if condition() {
                return Ok(WaitOutcome::ConditionMet);
            }
            if self.stopped.load(Ordering::Acquire) {
                return Ok(WaitOutcome::Stopped);
            }
            let remaining = timeout.and_then(|limit| limit.checked_sub(started.elapsed()));
            if timeout.is_some() && remaining.is_none() {
                return Ok(WaitOutcome::TimedOut);
            }
            let events = self.run_once_for(owner, remaining)?;
            for event in events {
                event.dispatch();
            }
            if timeout == Some(Duration::ZERO) && !condition() {
                return Ok(WaitOutcome::TimedOut);
            }
        }
    }

    fn run_once_for(&mut self, owner: Owner, caller_cap: Option<Duration>) -> Result<Vec<Event>> {
        self.fire_expired(Instant::now());

        let now = Instant::now();
        let timer_timeout = self
            .timers
            .next_deadline()
            .map(|deadline| deadline.saturating_duration_since(now));
        let mut timeout = minimum_timeout(timer_timeout, caller_cap);
        if !self.events.is_empty(owner)? {
            timeout = Some(Duration::ZERO);
        }

        let mut ready = MioEvents::with_capacity(128);
        self.reactor.poll(&mut ready, timeout)?;

        // A timer used as the poll deadline belongs to the timer phase of this
        // iteration, ahead of I/O callbacks and the deferred safe point.
        self.fire_expired(Instant::now());

        for event in &ready {
            let token = event.token();
            if token == WAKE_TOKEN {
                self.work.transfer(&mut self.events)?;
                continue;
            }
            if token == SIGNAL_TOKEN {
                let root = self.events.root();
                self.signals.drain(&mut self.events, root)?;
                continue;
            }
            let readiness = Readiness {
                token,
                readable: event.is_readable(),
                writable: event.is_writable(),
                error: event.is_error(),
                read_closed: event.is_read_closed(),
                write_closed: event.is_write_closed(),
            };
            if let Some(mut callback) = self.callbacks.remove(&token) {
                let result = self.dispatch_readiness(&mut callback, readiness);
                self.callbacks.insert(token, callback);
                result?;
            }
        }

        // A readiness callback may run long enough for another timer to become
        // due. Keep it ahead of the check/deferred phase rather than delaying it
        // until a later iteration.
        self.fire_expired(Instant::now());

        let drained = self.events.process_events(owner)?;
        let mut unhandled = Vec::new();
        for event in drained {
            match event {
                Event::Callback(callback) => callback(),
                signal @ Event::Signal(_) => unhandled.push(signal),
            }
        }
        Ok(unhandled)
    }

    /// Invokes `callback` until it reports [`DrainState::Drained`].
    ///
    /// mio's epoll backend is edge-triggered (`EPOLLET`), so a readiness
    /// notification is delivered once per state change. Re-invoking until the
    /// callback observes `WouldBlock` guarantees a partial read advances the
    /// entire buffer available on this edge and never strands buffered
    /// msgpack frames waiting for a readiness report that will not come.
    fn dispatch_readiness(
        &mut self,
        callback: &mut ReadinessCallback,
        readiness: Readiness,
    ) -> Result<()> {
        loop {
            match callback(readiness, &mut self.events)? {
                DrainState::Drained => return Ok(()),
                DrainState::KeepDraining => continue,
            }
        }
    }

    fn fire_expired(&mut self, now: Instant) {
        for mut timer in self.timers.expired(now) {
            timer.fire();
        }
    }
}

fn minimum_timeout(left: Option<Duration>, right: Option<Duration>) -> Option<Duration> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(timeout), None) | (None, Some(timeout)) => Some(timeout),
        (None, None) => None,
    }
}
