use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mio::Waker;

use crate::handle::{AsyncState, Callback, HandleKind};
use crate::{CallbackError, Handle, HandleId, Result, UvLoop};

/// Thread-safe producer for a coalescing async handle.
#[derive(Clone)]
pub struct AsyncSender {
    pending: Arc<AtomicBool>,
    waker: Arc<Waker>,
}

impl AsyncSender {
    /// Marks the callback pending and wakes the loop; repeated pending sends coalesce.
    pub fn send(&self) -> Result<()> {
        self.pending.store(true, Ordering::Release);
        self.waker.wake()?;
        Ok(())
    }
}

/// A handle whose callback always executes on the loop thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Async {
    id: HandleId,
}

impl Async {
    /// Allocates an immediately-active async handle.
    pub fn new<F>(uv_loop: &mut UvLoop, callback: F) -> Result<Self>
    where
        F: FnMut(&mut UvLoop, HandleId) -> std::result::Result<(), CallbackError> + 'static,
    {
        let pending = Arc::new(AtomicBool::new(false));
        let callback: Callback = Box::new(callback);
        let id = uv_loop.allocate(HandleKind::Async(AsyncState {
            active: true,
            pending,
            callback: Some(callback),
        }))?;
        Ok(Self { id })
    }

    /// Returns a cloneable cross-thread sender.
    pub fn sender(&self, uv_loop: &UvLoop) -> Result<AsyncSender> {
        let state = uv_loop.state(self.id).ok_or(crate::Error::InvalidHandle(self.id))?;
        let HandleKind::Async(inner) = &state.kind else {
            return Err(crate::handle::wrong_kind(self.id, "async"));
        };
        Ok(AsyncSender {
            pending: Arc::clone(&inner.pending),
            waker: uv_loop.inner().reactor().waker(),
        })
    }

    /// Sends through this handle from the loop thread.
    pub fn send(&self, uv_loop: &UvLoop) -> Result<()> {
        self.sender(uv_loop)?.send()
    }
}

impl Handle for Async {
    fn id(&self) -> HandleId {
        self.id
    }
}
