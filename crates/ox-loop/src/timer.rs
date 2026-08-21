use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Stable timer identity. Its sequence also orders equal deadlines.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TimerId(u64);

type TimerCallback = Box<dyn FnMut() + Send + 'static>;

/// A callback and its optional repeat interval.
#[derive(Clone)]
pub struct TimerEntry {
    callback: Arc<Mutex<TimerCallback>>,
    repeat: Option<Duration>,
}

impl TimerEntry {
    /// Creates a one-shot timer callback.
    pub fn once(callback: impl FnMut() + Send + 'static) -> Self {
        Self {
            callback: Arc::new(Mutex::new(Box::new(callback))),
            repeat: None,
        }
    }

    /// Creates a timer callback that re-arms at `interval`.
    pub fn repeating(interval: Duration, callback: impl FnMut() + Send + 'static) -> Self {
        Self {
            callback: Arc::new(Mutex::new(Box::new(callback))),
            repeat: Some(interval),
        }
    }

    /// Returns the repeat interval, or `None` for a one-shot timer.
    pub fn repeat(&self) -> Option<Duration> {
        self.repeat
    }

    /// Poisoning cannot make an event loop callback unsafe; retain the captured
    /// state and let the loop continue rather than panicking in library code.
    pub fn fire(&mut self) {
        let mut callback = self
            .callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        callback();
    }
}

/// Deadline-ordered timers. The sequence key prevents equal-deadline overwrite.
#[derive(Default)]
pub struct TimerHeap {
    entries: BTreeMap<(Instant, u64), TimerEntry>,
    next_sequence: u64,
}

impl TimerHeap {
    /// Creates an empty timer heap.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a timer and returns its stable identity.
    pub fn insert(&mut self, deadline: Instant, entry: TimerEntry) -> TimerId {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.entries.insert((deadline, sequence), entry);
        TimerId(sequence)
    }

    /// Removes a pending timer, including a re-armed repeating timer.
    pub fn cancel(&mut self, id: TimerId) -> Option<TimerEntry> {
        let key = self
            .entries
            .keys()
            .find(|(_, sequence)| *sequence == id.0)
            .copied()?;
        self.entries.remove(&key)
    }

    /// Removes timers due at `now` and re-arms repeating timers from their
    /// previous deadline, preserving both cadence and stable identity.
    pub fn expired(&mut self, now: Instant) -> Vec<TimerEntry> {
        let due: Vec<_> = self
            .entries
            .range(..=(now, u64::MAX))
            .map(|(key, _)| *key)
            .collect();
        let mut expired = Vec::with_capacity(due.len());
        for (deadline, sequence) in due {
            if let Some(entry) = self.entries.remove(&(deadline, sequence)) {
                if let Some(interval) = entry.repeat {
                    if let Some(next_deadline) = deadline.checked_add(interval) {
                        self.entries
                            .insert((next_deadline, sequence), entry.clone());
                    }
                }
                expired.push(entry);
            }
        }
        expired
    }

    /// Returns the earliest pending deadline.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries.first_key_value().map(|(key, _)| key.0)
    }

    /// Reports whether no timers are pending.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of pending timer identities.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}
