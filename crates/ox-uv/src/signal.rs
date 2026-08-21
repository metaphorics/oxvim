use crate::handle::{Callback, HandleKind, SignalState, wrong_kind};
use crate::{CallbackError, Handle, HandleId, Result, UvLoop};

/// Process-signal watcher handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Signal {
    id: HandleId,
}

impl Signal {
    /// Allocates an inactive signal watcher.
    pub fn new(uv_loop: &mut UvLoop) -> Result<Self> {
        let id = uv_loop.allocate(HandleKind::Signal(SignalState {
            active: false,
            signum: None,
            oneshot: false,
            generation: 0,
            callback: None,
        }))?;
        Ok(Self { id })
    }

    /// Starts persistent delivery for `signum`.
    pub fn start<F>(&self, uv_loop: &mut UvLoop, signum: i32, callback: F) -> Result<()>
    where
        F: FnMut(&mut UvLoop, HandleId, i32) -> std::result::Result<(), CallbackError> + 'static,
    {
        self.start_inner(uv_loop, signum, false, callback)
    }

    /// Starts delivery that deactivates before its first callback.
    pub fn start_oneshot<F>(
        &self,
        uv_loop: &mut UvLoop,
        signum: i32,
        callback: F,
    ) -> Result<()>
    where
        F: FnMut(&mut UvLoop, HandleId, i32) -> std::result::Result<(), CallbackError> + 'static,
    {
        self.start_inner(uv_loop, signum, true, callback)
    }

    /// Stops signal delivery without closing the handle.
    pub fn stop(&self, uv_loop: &mut UvLoop) -> Result<()> {
        let state = uv_loop.state_mut(self.id)?;
        let HandleKind::Signal(inner) = &mut state.kind else {
            return Err(wrong_kind(self.id, "signal"));
        };
        inner.active = false;
        inner.generation = inner.generation.wrapping_add(1);
        Ok(())
    }

    fn start_inner<F>(
        &self,
        uv_loop: &mut UvLoop,
        signum: i32,
        oneshot: bool,
        mut callback: F,
    ) -> Result<()>
    where
        F: FnMut(&mut UvLoop, HandleId, i32) -> std::result::Result<(), CallbackError> + 'static,
    {
        let state = uv_loop.state(self.id).ok_or(crate::Error::InvalidHandle(self.id))?;
        if state.closing {
            return Err(crate::Error::ClosingHandle(self.id));
        }
        if !matches!(&state.kind, HandleKind::Signal(_)) {
            return Err(wrong_kind(self.id, "signal"));
        }
        uv_loop.signal_driver().subscribe(signum)?;
        let state = uv_loop.state_mut(self.id)?;
        let HandleKind::Signal(inner) = &mut state.kind else {
            return Err(wrong_kind(self.id, "signal"));
        };
        let wrapped: Callback = Box::new(move |uv_loop, id| callback(uv_loop, id, signum));
        inner.active = true;
        inner.signum = Some(signum);
        inner.oneshot = oneshot;
        inner.generation = inner.generation.wrapping_add(1);
        inner.callback = Some(wrapped);
        Ok(())
    }
}

impl Handle for Signal {
    fn id(&self) -> HandleId {
        self.id
    }
}

#[cfg(unix)]
mod platform {
    use std::cell::RefCell;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use mio::unix::SourceFd;
    use mio::Interest;
    use ox_loop::{DrainState, Loop};
    use signal_hook::iterator::backend::{Handle as SignalHandle, SignalDelivery};
    use signal_hook::iterator::exfiltrator::SignalOnly;

    use crate::{Error, Result};

    pub(crate) struct SignalDriver {
        handle: SignalHandle,
        pending: Arc<Mutex<Vec<i32>>>,
    }

    impl SignalDriver {
        pub(crate) fn new(event_loop: &mut Loop) -> Result<Self> {
            let (read, write) = UnixStream::pair()?;
            let delivery = SignalDelivery::with_pipe(
                read,
                write,
                SignalOnly::default(),
                std::iter::empty::<i32>(),
            )?;
            let handle = delivery.handle();
            let pending = Arc::new(Mutex::new(Vec::new()));
            let delivery = Rc::new(RefCell::new(delivery));

            let fd = delivery.borrow().get_read().as_raw_fd();
            let mut source = SourceFd(&fd);
            // Register on ox-loop's reserved internal token range instead of a
            // public caller token, so a caller-owned source at IO_TOKEN_START
            // never collides with the signal self-pipe.
            let token = event_loop.register_internal(&mut source, Interest::READABLE)?;

            let callback_delivery = Rc::clone(&delivery);
            let callback_pending = Arc::clone(&pending);
            event_loop.on_readiness(token, move |_, _| {
                let signals: Vec<_> = callback_delivery.borrow_mut().pending().collect();
                let mut pending = callback_pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.extend(signals);
                Ok(DrainState::Drained)
            })?;
            Ok(Self { handle, pending })
        }

        pub(crate) fn subscribe(&mut self, signum: i32) -> Result<()> {
            if signum <= 0
                || signum >= 128
                || signal_hook::consts::FORBIDDEN.contains(&signum)
            {
                return Err(Error::InvalidSignal(signum));
            }
            self.handle.add_signal(signum)?;
            Ok(())
        }

        pub(crate) fn drain_pending(&mut self) -> Vec<i32> {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *pending)
        }
    }
}

#[cfg(not(unix))]
mod platform {
    use ox_loop::Loop;

    use crate::{Error, Result};

    pub(crate) struct SignalDriver;

    impl SignalDriver {
        pub(crate) fn new(_event_loop: &mut Loop) -> Result<Self> {
            Ok(Self)
        }

        pub(crate) fn subscribe(&mut self, signum: i32) -> Result<()> {
            Err(Error::InvalidSignal(signum))
        }

        pub(crate) fn drain_pending(&mut self) -> Vec<i32> {
            Vec::new()
        }
    }
}

pub(crate) use platform::SignalDriver;
