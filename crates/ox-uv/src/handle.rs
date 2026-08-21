use crate::{CallbackError, Error, Result, UvLoop};

/// Stable owner-allocated handle identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HandleId(pub(crate) u32);

/// Shared lifecycle operations implemented by every handle kind.
pub trait Handle {
    /// Returns this handle's stable registry identity.
    fn id(&self) -> HandleId;

    /// Reports whether the handle currently produces events.
    fn is_active(&self, uv_loop: &UvLoop) -> bool {
        uv_loop.is_active(self.id())
    }

    /// Reports whether close has been requested but not yet delivered.
    fn is_closing(&self, uv_loop: &UvLoop) -> bool {
        uv_loop.is_closing(self.id())
    }

    /// Requests deferred destruction without a close callback.
    fn close(&self, uv_loop: &mut UvLoop) -> Result<()> {
        uv_loop.close(
            self.id(),
            None::<fn(&mut UvLoop, HandleId) -> std::result::Result<(), CallbackError>>,
        )
    }

    /// Requests deferred destruction with a next-turn callback.
    fn close_with<F>(&self, uv_loop: &mut UvLoop, callback: F) -> Result<()>
    where
        F: FnOnce(&mut UvLoop, HandleId) -> std::result::Result<(), CallbackError> + 'static,
    {
        uv_loop.close(self.id(), Some(callback))
    }

    /// Makes an active handle contribute to loop liveness.
    fn ref_(&self, uv_loop: &mut UvLoop) -> Result<()> {
        uv_loop.set_referenced(self.id(), true)
    }

    /// Prevents this handle alone from keeping the loop alive.
    fn unref(&self, uv_loop: &mut UvLoop) -> Result<()> {
        uv_loop.set_referenced(self.id(), false)
    }

    /// Reports the handle's reference flag.
    fn has_ref(&self, uv_loop: &UvLoop) -> bool {
        uv_loop.has_ref(self.id())
    }
}

pub(crate) type Callback = Box<
    dyn FnMut(&mut UvLoop, HandleId) -> std::result::Result<(), CallbackError> + 'static,
>;
pub(crate) type CloseCallback = Box<
    dyn FnOnce(&mut UvLoop, HandleId) -> std::result::Result<(), CallbackError> + 'static,
>;

pub(crate) struct HandleState {
    pub(crate) referenced: bool,
    pub(crate) closing: bool,
    pub(crate) kind: HandleKind,
    pub(crate) close_callback: Option<CloseCallback>,
}

impl HandleState {
    pub(crate) fn new(kind: HandleKind) -> Self {
        Self {
            referenced: true,
            closing: false,
            kind,
            close_callback: None,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.closing && self.kind.is_active()
    }
}

pub(crate) enum HandleKind {
    Timer(TimerState),
    Idle(PhaseState),
    Prepare(PhaseState),
    Check(PhaseState),
    Async(AsyncState),
    Signal(SignalState),
}

impl HandleKind {
    fn is_active(&self) -> bool {
        match self {
            Self::Timer(state) => state.active,
            Self::Idle(state) | Self::Prepare(state) | Self::Check(state) => state.active,
            Self::Async(state) => state.active,
            Self::Signal(state) => state.active,
        }
    }

    pub(crate) fn deactivate(&mut self) {
        match self {
            Self::Timer(state) => state.active = false,
            Self::Idle(state) | Self::Prepare(state) | Self::Check(state) => {
                state.active = false;
            }
            Self::Async(state) => state.active = false,
            Self::Signal(state) => state.active = false,
        }
    }
}

pub(crate) struct TimerState {
    pub(crate) active: bool,
    pub(crate) started: bool,
    pub(crate) deadline: Option<std::time::Instant>,
    pub(crate) repeat: std::time::Duration,
    pub(crate) generation: u64,
    pub(crate) callback: Option<Callback>,
}

pub(crate) struct PhaseState {
    pub(crate) active: bool,
    pub(crate) generation: u64,
    pub(crate) callback: Option<Callback>,
}

pub(crate) struct AsyncState {
    pub(crate) active: bool,
    pub(crate) pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(crate) callback: Option<Callback>,
}

pub(crate) struct SignalState {
    pub(crate) active: bool,
    pub(crate) signum: Option<i32>,
    pub(crate) oneshot: bool,
    pub(crate) generation: u64,
    pub(crate) callback: Option<Callback>,
}

pub(crate) fn wrong_kind(id: HandleId, expected: &'static str) -> Error {
    Error::WrongHandleKind { id, expected }
}
