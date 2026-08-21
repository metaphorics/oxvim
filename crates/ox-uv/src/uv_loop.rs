use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use crate::handle::{Callback, HandleKind, HandleState};
use crate::signal::SignalDriver;
use crate::{CallbackError, CallbackErrorEvent, CallbackPhase, Error, HandleId, Result};

/// libuv-compatible run modes over ox-loop's single-threaded pump.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunMode {
    /// Continue until no referenced active or closing handles remain.
    Default,
    /// Perform one iteration, blocking when a live source can wake the poller.
    Once,
    /// Perform one iteration without blocking.
    NoWait,
}

/// `vim.uv` loop state and handle registry.
///
/// Each turn maps libuv's phases onto ox-loop as follows: timers; pending async
/// and signal delivery; idle; prepare; ox-loop's mio poll and I/O callbacks;
/// check; deferred close callbacks. ox-loop also fires its own transport timers
/// around polling, so future I/O handle implementations may observe those
/// internal callbacks within the poll phase. Async and signal events discovered
/// by that poll are delivered immediately before check rather than waiting for
/// the next turn's pending phase.
pub struct UvLoop {
    inner: ox_loop::Loop,
    handles: Vec<(HandleId, HandleState)>,
    locations: HashMap<HandleId, usize>,
    next_handle_id: u64,
    pending_callbacks: VecDeque<(HandleId, CallbackPhase, Option<u64>)>,
    close_next: VecDeque<HandleId>,
    callback_errors: VecDeque<CallbackErrorEvent>,
    stop_requested: bool,
    running_mode: Option<RunMode>,
    now_origin: Instant,
    now_ms: u64,
    signals: SignalDriver,
}

impl UvLoop {
    /// Creates an empty loop and installs the dynamically extensible signal pipe.
    pub fn new() -> Result<Self> {
        let mut inner = ox_loop::Loop::new()?;
        let signals = SignalDriver::new(&mut inner)?;
        let now_origin = Instant::now();
        Ok(Self {
            inner,
            handles: Vec::new(),
            locations: HashMap::new(),
            next_handle_id: 0,
            pending_callbacks: VecDeque::new(),
            close_next: VecDeque::new(),
            callback_errors: VecDeque::new(),
            stop_requested: false,
            running_mode: None,
            now_origin,
            now_ms: 0,
            signals,
        })
    }

    /// Runs with the selected libuv mode and reports whether live work remains.
    pub fn run(&mut self, mode: RunMode) -> Result<bool> {
        if self.running_mode.is_some() {
            return Err(Error::LoopAlreadyRunning);
        }
        self.running_mode = Some(mode);
        let result = match mode {
            RunMode::Default => self.run_default_inner(),
            RunMode::Once => self.run_once_inner(),
            RunMode::NoWait => self.run_nowait_inner(),
        };
        self.running_mode = None;
        result
    }

    /// Returns the active run mode while inside a callback.
    pub fn loop_mode(&self) -> Option<RunMode> {
        self.running_mode
    }

    /// Runs in default mode and reports whether live work remains after stop.
    pub fn run_default(&mut self) -> Result<bool> {
        self.run(RunMode::Default)
    }

    fn run_default_inner(&mut self) -> Result<bool> {
        if self.stop_requested {
            // A stop requested before run must still execute one forced-nowait
            // iteration (luvref.txt:464-471): uv.run() ends no sooner than the
            // next loop iteration, and a pre-`stop` call prevents blocking on
            // I/O but not the turn's due-timer/pending/check callbacks.
            self.stop_requested = false;
            self.run_turn(true)?;
            return Ok(self.loop_alive());
        }
        while self.loop_alive() && !self.stop_requested {
            self.run_turn(false)?;
        }
        self.stop_requested = false;
        Ok(self.loop_alive())
    }

    /// Runs one possibly-blocking iteration and reports whether work remains.
    pub fn run_once(&mut self) -> Result<bool> {
        self.run(RunMode::Once)
    }

    fn run_once_inner(&mut self) -> Result<bool> {
        if self.stop_requested {
            // As for `run_default_inner`, a pending stop forces one non-blocking
            // iteration so due callbacks still run before the loop ends.
            self.stop_requested = false;
            self.run_turn(true)?;
            return Ok(self.loop_alive());
        }
        if self.loop_alive() {
            self.run_turn(false)?;
        }
        self.stop_requested = false;
        Ok(self.loop_alive())
    }

    /// Runs one non-blocking iteration and reports whether work remains.
    pub fn run_nowait(&mut self) -> Result<bool> {
        self.run(RunMode::NoWait)
    }

    fn run_nowait_inner(&mut self) -> Result<bool> {
        if !self.stop_requested && self.loop_alive() {
            self.run_turn(true)?;
        }
        self.stop_requested = false;
        Ok(self.loop_alive())
    }

    /// Requests that the current or next run call return after its iteration.
    pub fn stop(&mut self) {
        self.stop_requested = true;
    }

    /// Reports referenced active handles and all closing handles.
    pub fn loop_alive(&self) -> bool {
        self.handles.iter().any(|(_, state)| {
            state.closing || (state.referenced && state.is_active())
        })
    }

    /// Visits a stable snapshot of every allocated handle identity.
    pub fn walk(&mut self, mut callback: impl FnMut(&mut Self, HandleId)) {
        let ids: Vec<_> = self
            .handles
            .iter()
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if self.state(id).is_some() {
                callback(self, id);
            }
        }
    }

    /// Returns the cached monotonic loop time in milliseconds.
    pub fn now(&self) -> u64 {
        self.now_ms
    }

    /// Refreshes the cached monotonic loop time.
    pub fn update_time(&mut self) {
        self.now_ms = u64::try_from(self.now_origin.elapsed().as_millis()).unwrap_or(u64::MAX);
    }

    /// Removes and returns the oldest callback failure or caught panic.
    pub fn pop_callback_error(&mut self) -> Option<CallbackErrorEvent> {
        self.callback_errors.pop_front()
    }

    /// Returns the underlying ox-loop for Task 7b source registration.
    pub(crate) fn inner(&self) -> &ox_loop::Loop {
        &self.inner
    }

    /// Returns mutable access to ox-loop for Task 7b source registration.
    pub fn inner_mut(&mut self) -> &mut ox_loop::Loop {
        &mut self.inner
    }

    pub(crate) fn allocate(&mut self, kind: HandleKind) -> Result<HandleId> {
        let raw = u32::try_from(self.next_handle_id).map_err(|_| Error::HandleLimit)?;
        self.next_handle_id += 1;
        let id = HandleId(raw);
        let slot = self.handles.len();
        self.handles.push((id, HandleState::new(kind)));
        self.locations.insert(id, slot);
        Ok(id)
    }

    pub(crate) fn state(&self, id: HandleId) -> Option<&HandleState> {
        let slot = *self.locations.get(&id)?;
        self.handles.get(slot).map(|(_, state)| state)
    }

    pub(crate) fn state_mut(&mut self, id: HandleId) -> Result<&mut HandleState> {
        let slot = *self.locations.get(&id).ok_or(Error::InvalidHandle(id))?;
        self.handles
            .get_mut(slot)
            .map(|(_, state)| state)
            .ok_or(Error::InvalidHandle(id))
    }

    pub(crate) fn is_active(&self, id: HandleId) -> bool {
        self.state(id).is_some_and(HandleState::is_active)
    }

    pub(crate) fn is_closing(&self, id: HandleId) -> bool {
        self.state(id).is_some_and(|state| state.closing)
    }

    pub(crate) fn has_ref(&self, id: HandleId) -> bool {
        self.state(id).is_some_and(|state| state.referenced)
    }

    pub(crate) fn set_referenced(&mut self, id: HandleId, referenced: bool) -> Result<()> {
        self.state_mut(id)?.referenced = referenced;
        Ok(())
    }

    pub(crate) fn close<F>(&mut self, id: HandleId, callback: Option<F>) -> Result<()>
    where
        F: FnOnce(&mut Self, HandleId) -> std::result::Result<(), CallbackError> + 'static,
    {
        let state = self.state_mut(id)?;
        if state.closing {
            return Err(Error::AlreadyClosing(id));
        }
        state.closing = true;
        state.kind.deactivate();
        state.close_callback = callback.map(|callback| Box::new(callback) as _);
        self.close_next.push_back(id);
        Ok(())
    }

    pub(crate) fn signal_driver(&mut self) -> &mut SignalDriver {
        &mut self.signals
    }

    fn run_turn(&mut self, force_nowait: bool) -> Result<()> {
        self.update_time();
        let mut close_due = std::mem::take(&mut self.close_next);

        self.fire_due_timers();
        self.dispatch_pending_sources();
        self.fire_phase(CallbackPhase::Idle);
        self.fire_phase(CallbackPhase::Prepare);

        let timeout = self.poll_timeout(force_nowait);
        let _ = self.inner.run_once(timeout)?;

        self.dispatch_pending_sources();
        self.fire_phase(CallbackPhase::Check);
        self.fire_closes(&mut close_due);
        Ok(())
    }

    fn poll_timeout(&self, force_nowait: bool) -> Option<Duration> {
        if force_nowait
            || self.stop_requested
            || !self.loop_alive()
            || !self.close_next.is_empty()
            || self.handles.iter().any(|(_, state)| state.closing)
        {
            return Some(Duration::ZERO);
        }
        let mut deadline = None;
        let mut idle_active = false;
        for (_, state) in self.handles.iter().filter(|(_, state)| !state.closing) {
            match &state.kind {
                HandleKind::Timer(timer) if timer.active => {
                    if let Some(candidate) = timer.deadline {
                        deadline = Some(deadline.map_or(candidate, |current: Instant| current.min(candidate)));
                    }
                }
                HandleKind::Idle(phase) if phase.active => {
                    idle_active = true;
                }
                _ => {}
            }
        }
        if idle_active {
            return Some(Duration::ZERO);
        }
        deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    fn fire_due_timers(&mut self) {
        let now = Instant::now();
        let due: Vec<_> = self
            .handles
            .iter()
            .filter_map(|(id, state)| {
                let HandleKind::Timer(timer) = &state.kind else {
                    return None;
                };
                (timer.active && timer.deadline.is_some_and(|deadline| deadline <= now))
                    .then_some((*id, timer.generation))
            })
            .collect();
        for (id, generation) in due {
            let still_due = self.state(id).is_some_and(|state| {
                matches!(
                    &state.kind,
                    HandleKind::Timer(timer) if timer.active && timer.generation == generation
                )
            });
            if !still_due {
                continue;
            }
            let callback = match self.take_callback(id, CallbackPhase::Timer, false) {
                Some(callback) => callback,
                None => continue,
            };
            self.invoke_callback(id, CallbackPhase::Timer, callback);
            let mut overflow = false;
            if let Some(state) = self.state_mut_if_present(id) {
                if let HandleKind::Timer(timer) = &mut state.kind {
                    if !state.closing && timer.active && timer.generation == generation {
                        if timer.repeat.is_zero() {
                            timer.active = false;
                            timer.deadline = None;
                        } else {
                            timer.deadline = now.checked_add(timer.repeat);
                            if timer.deadline.is_none() {
                                timer.active = false;
                                overflow = true;
                            }
                        }
                    }
                }
            }
            if overflow {
                self.callback_errors.push_back(CallbackErrorEvent {
                    id,
                    phase: CallbackPhase::Timer,
                    error: CallbackError::from(Error::TimeOverflow),
                });
            }
        }
    }

    fn fire_phase(&mut self, phase: CallbackPhase) {
        let ids: Vec<_> = self
            .handles
            .iter()
            .filter_map(|(id, state)| {
                let generation = match (&state.kind, phase) {
                    (HandleKind::Idle(inner), CallbackPhase::Idle)
                    | (HandleKind::Prepare(inner), CallbackPhase::Prepare)
                    | (HandleKind::Check(inner), CallbackPhase::Check)
                        if inner.active => Some(inner.generation),
                    _ => None,
                };
                generation.map(|generation| (*id, generation))
            })
            .collect();
        for (id, generation) in ids {
            let current = self.state(id).and_then(|state| match (&state.kind, phase) {
                (HandleKind::Idle(inner), CallbackPhase::Idle)
                | (HandleKind::Prepare(inner), CallbackPhase::Prepare)
                | (HandleKind::Check(inner), CallbackPhase::Check)
                    if inner.active => Some(inner.generation),
                _ => None,
            });
            if current == Some(generation) {
                if let Some(callback) = self.take_callback(id, phase, false) {
                    self.invoke_callback(id, phase, callback);
                }
            }
        }
    }

    fn dispatch_pending_sources(&mut self) {
        let async_ids: Vec<_> = self
            .handles
            .iter()
            .filter_map(|(id, state)| {
                let HandleKind::Async(inner) = &state.kind else {
                    return None;
                };
                (inner.active
                    && inner
                        .pending
                        .swap(false, std::sync::atomic::Ordering::AcqRel))
                .then_some(*id)
            })
            .collect();
        for id in async_ids {
            self.pending_callbacks
                .push_back((id, CallbackPhase::Async, None));
        }

        for signum in self.signals.drain_pending() {
            let ids: Vec<_> = self
                .handles
                .iter()
                .filter_map(|(id, state)| {
                    let HandleKind::Signal(inner) = &state.kind else {
                        return None;
                    };
                    (inner.active && inner.signum == Some(signum))
                        .then_some((*id, inner.generation))
                })
                .collect();
            for (id, generation) in ids {
                self.pending_callbacks.push_back((
                    id,
                    CallbackPhase::Signal(signum),
                    Some(generation),
                ));
            }
        }

        while let Some((id, phase, generation)) = self.pending_callbacks.pop_front() {
            let mut allow_inactive = false;
            let deliver = match phase {
                CallbackPhase::Async => self.state(id).is_some_and(|state| {
                    matches!(&state.kind, HandleKind::Async(inner) if inner.active)
                }),
                CallbackPhase::Signal(_) => {
                    self.state_mut_if_present(id).is_some_and(|state| {
                        let HandleKind::Signal(inner) = &mut state.kind else {
                            return false;
                        };
                        if !inner.active || Some(inner.generation) != generation {
                            return false;
                        }
                        if inner.oneshot {
                            inner.active = false;
                            allow_inactive = true;
                        }
                        true
                    })
                }
                _ => false,
            };
            if deliver {
                if let Some(callback) = self.take_callback(id, phase, allow_inactive) {
                    self.invoke_callback(id, phase, callback);
                }
            }
        }
    }

    fn take_callback(
        &mut self,
        id: HandleId,
        phase: CallbackPhase,
        allow_inactive: bool,
    ) -> Option<Callback> {
        let state = self.state_mut_if_present(id)?;
        if state.closing || (!allow_inactive && !state.is_active()) {
            return None;
        }
        match (&mut state.kind, phase) {
            (HandleKind::Timer(inner), CallbackPhase::Timer) => inner.callback.take(),
            (HandleKind::Idle(inner), CallbackPhase::Idle)
            | (HandleKind::Prepare(inner), CallbackPhase::Prepare)
            | (HandleKind::Check(inner), CallbackPhase::Check) => inner.callback.take(),
            (HandleKind::Async(inner), CallbackPhase::Async) => inner.callback.take(),
            (HandleKind::Signal(inner), CallbackPhase::Signal(_)) => inner.callback.take(),
            _ => None,
        }
    }

    fn restore_callback(&mut self, id: HandleId, phase: CallbackPhase, callback: Callback) {
        let Some(state) = self.state_mut_if_present(id) else {
            return;
        };
        if state.closing {
            return;
        }
        let slot = match (&mut state.kind, phase) {
            (HandleKind::Timer(inner), CallbackPhase::Timer) => &mut inner.callback,
            (HandleKind::Idle(inner), CallbackPhase::Idle)
            | (HandleKind::Prepare(inner), CallbackPhase::Prepare)
            | (HandleKind::Check(inner), CallbackPhase::Check) => &mut inner.callback,
            (HandleKind::Async(inner), CallbackPhase::Async) => &mut inner.callback,
            (HandleKind::Signal(inner), CallbackPhase::Signal(_)) => &mut inner.callback,
            _ => return,
        };
        if slot.is_none() {
            *slot = Some(callback);
        }
    }

    fn invoke_callback(&mut self, id: HandleId, phase: CallbackPhase, mut callback: Callback) {
        let result = catch_unwind(AssertUnwindSafe(|| callback(self, id)));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.callback_errors.push_back(CallbackErrorEvent { id, phase, error }),
            Err(payload) => self.callback_errors.push_back(CallbackErrorEvent {
                id,
                phase,
                error: CallbackError::panic(payload),
            }),
        }
        self.restore_callback(id, phase, callback);
    }

    fn fire_closes(&mut self, close_due: &mut VecDeque<HandleId>) {
        while let Some(id) = close_due.pop_front() {
            let Some(slot) = self.locations.remove(&id) else {
                continue;
            };
            let (_, mut state) = self.handles.swap_remove(slot);
            if let Some((moved_id, _)) = self.handles.get(slot) {
                self.locations.insert(*moved_id, slot);
            }
            if let Some(callback) = state.close_callback.take() {
                let result = catch_unwind(AssertUnwindSafe(|| callback(self, id)));
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => self.callback_errors.push_back(CallbackErrorEvent {
                        id,
                        phase: CallbackPhase::Close,
                        error,
                    }),
                    Err(payload) => self.callback_errors.push_back(CallbackErrorEvent {
                        id,
                        phase: CallbackPhase::Close,
                        error: CallbackError::panic(payload),
                    }),
                }
            }
        }
    }

    fn state_mut_if_present(&mut self, id: HandleId) -> Option<&mut HandleState> {
        let slot = *self.locations.get(&id)?;
        self.handles.get_mut(slot).map(|(_, state)| state)
    }
}
