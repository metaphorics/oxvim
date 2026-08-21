use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use mio::Waker;

use crate::{Event, MultiQueue, Owner, Result};

/// Work posted into the loop from its own or another thread.
pub enum Work {
    /// Execute during readiness dispatch, before the deferred safe point.
    Fast(Owner, Event),
    /// Forward to the owner's MultiQueue for safe-point processing.
    Deferred(Owner, Event),
}

#[derive(Default)]
struct Pending {
    channels: BTreeMap<Owner, VecDeque<Work>>,
    order: VecDeque<Owner>,
}

/// Thread-safe producer for deferred loop work.
#[derive(Clone)]
pub struct DeferredScheduler {
    pending: Arc<Mutex<Pending>>,
    waker: Arc<Waker>,
}

impl DeferredScheduler {
    /// Posts deferred work and wakes the reactor.
    pub fn schedule_deferred(&self, owner: Owner, event: Event) -> Result<()> {
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.order.push_back(owner);
            pending
                .channels
                .entry(owner)
                .or_default()
                .push_back(Work::Deferred(owner, event));
        }
        self.waker.wake()?;
        Ok(())
    }
}

/// Per-owner inbound queues. They are transferred only by the loop thread.
pub struct WorkQueues {
    pending: Arc<Mutex<Pending>>,
    waker: Arc<Waker>,
}

impl WorkQueues {
    /// Creates inbound work queues backed by the reactor's waker.
    pub fn new(waker: Arc<Waker>) -> Self {
        Self {
            pending: Arc::new(Mutex::new(Pending::default())),
            waker,
        }
    }

    /// Returns a cloneable, thread-safe deferred producer.
    pub fn scheduler(&self) -> DeferredScheduler {
        DeferredScheduler {
            pending: Arc::clone(&self.pending),
            waker: Arc::clone(&self.waker),
        }
    }

    /// Posts fast work and wakes the reactor.
    pub fn schedule_fast(&self, owner: Owner, event: Event) -> Result<()> {
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.order.push_back(owner);
            pending
                .channels
                .entry(owner)
                .or_default()
                .push_back(Work::Fast(owner, event));
        }
        self.waker.wake()?;
        Ok(())
    }

    /// Runs fast callbacks now and forwards deferred callbacks to MultiQueue.
    /// This mirrors loop.c:212-218 followed by loop.c:105-117.
    pub(crate) fn transfer(&self, events: &mut MultiQueue) -> Result<()> {
        let (mut channels, mut order) = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                std::mem::take(&mut pending.channels),
                std::mem::take(&mut pending.order),
            )
        };
        while let Some(owner) = order.pop_front() {
            let work = channels.get_mut(&owner).and_then(VecDeque::pop_front);
            let Some(work) = work else {
                continue;
            };
            match work {
                Work::Fast(_, event) => event.dispatch(),
                Work::Deferred(_, event) => events.put(owner, event)?,
            }
        }
        Ok(())
    }
}
