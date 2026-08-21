//! Main-loop pump ordering.
//!
//! Each iteration maps libuv's phases onto mio as follows: expired timers
//! (timer phase), readiness polling and dispatch (poll phase), then owned
//! MultiQueue processing (check/deferred phase). Timers that become due while
//! poll sleeps are fired before readiness dispatch, preserving timers-before-I/O.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use mio::{Events as MioEvents, Token};

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

type ReadinessCallback = Box<dyn FnMut(Readiness, &mut MultiQueue) -> Result<()> + 'static>;

/// Single-threaded callback pump with thread-safe work ingress.
pub struct Loop {
    reactor: Reactor,
    timers: TimerHeap,
    events: MultiQueue,
    work: WorkQueues,
    signals: Signals,
    callbacks: HashMap<Token, ReadinessCallback>,
    stopped: bool,
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
            stopped: false,
        })
    }

    /// Returns the reactor for source registration.
    pub fn reactor(&self) -> &Reactor {
        &self.reactor
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
    pub fn on_readiness(
        &mut self,
        token: Token,
        callback: impl FnMut(Readiness, &mut MultiQueue) -> Result<()> + 'static,
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

    /// Requests that `run` return after its current iteration.
    pub fn stop(&mut self) {
        self.stopped = true;
    }

    /// Pumps until [`Loop::stop`] is requested or an error occurs.
    pub fn run(&mut self) -> Result<()> {
        self.stopped = false;
        while !self.stopped {
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
            if self.stopped {
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
                let result = callback(readiness, &mut self.events);
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
