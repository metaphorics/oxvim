use std::collections::{HashMap, HashSet, VecDeque};

use crate::{Error, Result};

/// Identity of an owned event queue.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Owner(u64);

/// Work delivered on the loop thread.
pub enum Event {
    /// A one-shot callback executed by the loop.
    Callback(Box<dyn FnOnce() + Send + 'static>),
    /// A platform signal number delivered through signal-hook.
    Signal(i32),
}

impl Event {
    /// Wraps a one-shot loop-thread callback.
    pub fn callback(callback: impl FnOnce() + Send + 'static) -> Self {
        Self::Callback(Box::new(callback))
    }

    /// Executes callback events; signal events are left for explicit consumers.
    pub fn dispatch(self) {
        if let Self::Callback(callback) = self {
            callback();
        }
    }
}

struct Queue {
    parent: Option<Owner>,
    entries: VecDeque<u64>,
}

/// Owned parent/child queues for selective event processing.
///
/// An event ID is represented in its origin queue and every ancestor. This is
/// the safe-Rust equivalent of Neovim's paired child item and parent link node.
pub struct MultiQueue {
    root: Owner,
    next_owner: u64,
    next_event: u64,
    queues: HashMap<Owner, Queue>,
    events: HashMap<u64, Event>,
    origins: HashMap<u64, Owner>,
}

impl Default for MultiQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiQueue {
    /// Creates a root queue with no children.
    pub fn new() -> Self {
        let root = Owner(0);
        let mut queues = HashMap::new();
        queues.insert(
            root,
            Queue {
                parent: None,
                entries: VecDeque::new(),
            },
        );
        Self {
            root,
            next_owner: 1,
            next_event: 0,
            queues,
            events: HashMap::new(),
            origins: HashMap::new(),
        }
    }

    /// Returns the root owner.
    pub fn root(&self) -> Owner {
        self.root
    }

    /// Creates an empty queue owned by `parent`.
    pub fn child(&mut self, parent: Owner) -> Result<Owner> {
        if !self.queues.contains_key(&parent) {
            return Err(Error::UnknownOwner(parent));
        }
        let owner = Owner(self.next_owner);
        self.next_owner = self.next_owner.wrapping_add(1);
        self.queues.insert(
            owner,
            Queue {
                parent: Some(parent),
                entries: VecDeque::new(),
            },
        );
        Ok(owner)
    }

    /// Enqueues an event and mirrors its position into every ancestor.
    pub fn put(&mut self, owner: Owner, event: Event) -> Result<()> {
        if !self.queues.contains_key(&owner) {
            return Err(Error::UnknownOwner(owner));
        }
        let event_id = self.next_event;
        self.next_event = self.next_event.wrapping_add(1);
        self.events.insert(event_id, event);
        self.origins.insert(event_id, owner);

        // multiqueue.c:235-249: child insertion creates a corresponding parent
        // link. Repeating that operation through the ancestor chain generalizes
        // the upstream root/child representation without changing its behavior.
        let mut queue = Some(owner);
        while let Some(current) = queue {
            let current_queue = self
                .queues
                .get_mut(&current)
                .ok_or(Error::UnknownOwner(current))?;
            current_queue.entries.push_back(event_id);
            queue = current_queue.parent;
        }
        Ok(())
    }

    /// Drains only events owned by `owner` or one of its descendants.
    pub fn process_events(&mut self, owner: Owner) -> Result<Vec<Event>> {
        if !self.queues.contains_key(&owner) {
            return Err(Error::UnknownOwner(owner));
        }
        let mut drained = Vec::new();
        loop {
            let event_id = self
                .queues
                .get_mut(&owner)
                .and_then(|queue| queue.entries.pop_front());
            let Some(event_id) = event_id else {
                break;
            };
            let Some(origin) = self.origins.remove(&event_id) else {
                continue;
            };

            // multiqueue.c:193-218: consuming either a parent link or a child
            // item removes its counterpart. Removing the ID from every queue on
            // the origin-to-root path is the same ownership operation.
            let mut queue = Some(origin);
            while let Some(current) = queue {
                let current_queue = self
                    .queues
                    .get_mut(&current)
                    .ok_or(Error::UnknownOwner(current))?;
                if current != owner {
                    current_queue.entries.retain(|id| *id != event_id);
                }
                queue = current_queue.parent;
            }
            if let Some(event) = self.events.remove(&event_id) {
                drained.push(event);
            }
        }
        Ok(drained)
    }

    /// Removes a child owner, all descendants, and their pending events.
    pub fn remove_owner(&mut self, owner: Owner) -> Result<()> {
        if owner == self.root {
            return Err(Error::RootOwnerRemoval);
        }
        if !self.queues.contains_key(&owner) {
            return Err(Error::UnknownOwner(owner));
        }
        // multiqueue.c:111-125 removes each child item and its paired parent
        // link while freeing a queue. Descendant discovery extends that cleanup
        // to the generalized hierarchy used here.
        let removed_owners: HashSet<_> = self
            .queues
            .keys()
            .copied()
            .filter(|candidate| self.is_descendant_of(*candidate, owner))
            .collect();
        let removed_events: HashSet<_> = removed_owners
            .iter()
            .filter_map(|removed| self.queues.get(removed))
            .flat_map(|queue| queue.entries.iter().copied())
            .collect();
        for event_id in &removed_events {
            self.events.remove(event_id);
            self.origins.remove(event_id);
        }
        for queue in self.queues.values_mut() {
            queue.entries.retain(|event_id| !removed_events.contains(event_id));
        }
        self.queues
            .retain(|candidate, _| !removed_owners.contains(candidate));
        Ok(())
    }

    /// Reports whether an owner has no pending events.
    pub fn is_empty(&self, owner: Owner) -> Result<bool> {
        self.queues
            .get(&owner)
            .map(|queue| queue.entries.is_empty())
            .ok_or(Error::UnknownOwner(owner))
    }

    /// Returns the count of pending events visible to an owner.
    pub fn len(&self, owner: Owner) -> Result<usize> {
        self.queues
            .get(&owner)
            .map(|queue| queue.entries.len())
            .ok_or(Error::UnknownOwner(owner))
    }

    fn is_descendant_of(&self, mut candidate: Owner, ancestor: Owner) -> bool {
        loop {
            if candidate == ancestor {
                return true;
            }
            let Some(parent) = self.queues.get(&candidate).and_then(|queue| queue.parent) else {
                return false;
            };
            candidate = parent;
        }
    }
}
