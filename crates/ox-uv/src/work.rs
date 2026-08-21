//! Type-erased user work scheduled on the shared worker pool.

use std::any::Any;
use std::sync::{Arc, Mutex};

use crate::UvLoop;
use crate::pool::{LoopPoster, Pool, PoolError};

/// Values crossing the pool boundary.
///
/// The `Send` bound is the Rust equivalent of luv serializing thread arguments
/// into an isolated worker state.
pub type WorkData = Box<dyn Any + Send + 'static>;

/// Result delivered to an after-work callback.
pub type WorkResult = Result<WorkData, PoolError>;

type WorkFn = dyn Fn(WorkData) -> WorkData + Send + Sync + 'static;
type AfterFn = dyn FnMut(&mut UvLoop, WorkResult) + Send + 'static;

/// Reusable work context created by [`new_work`].
///
/// Work executes on the pool; after-work always executes through loop pumping.
/// See `uv.new_work()` in `runtime/doc/luvref.txt`.
pub struct Work<P: LoopPoster> {
    pool: Pool,
    poster: P,
    work: Arc<WorkFn>,
    after: Arc<Mutex<Box<AfterFn>>>,
}

impl<P: LoopPoster> Clone for Work<P> {
    fn clone(&self) -> Self {
        Self { pool: self.pool.clone(), poster: self.poster.clone(), work: Arc::clone(&self.work), after: Arc::clone(&self.after) }
    }
}

/// Creates a reusable pool work context.
///
/// Both input and output are `Box<dyn Any + Send>`; bindings are responsible
/// for serialization or concrete downcasts. See `uv.new_work()` in
/// `runtime/doc/luvref.txt`.
pub fn new_work<P, W, A>(pool: Pool, poster: P, work: W, after: A) -> Work<P>
where
    P: LoopPoster,
    W: Fn(WorkData) -> WorkData + Send + Sync + 'static,
    A: FnMut(&mut UvLoop, WorkResult) + Send + 'static,
{
    Work { pool, poster, work: Arc::new(work), after: Arc::new(Mutex::new(Box::new(after))) }
}

impl<P: LoopPoster> Work<P> {
    /// Queues one value for processing.
    ///
    /// The after-work callback is serialized on the loop even when multiple
    /// jobs complete concurrently. See `uv.queue_work()` in
    /// `runtime/doc/luvref.txt`.
    pub fn queue(&self, data: WorkData) -> Result<(), PoolError> {
        let work = Arc::clone(&self.work);
        let after = Arc::clone(&self.after);
        self.pool.submit(self.poster.clone(), move || work(data), move |uv_loop, result| {
            if let Ok(mut after) = after.lock() { after(uv_loop, result); }
        })
    }
}

/// Queues one request through an existing work context.
/// See `uv.queue_work()` in `runtime/doc/luvref.txt`.
pub fn queue_work<P: LoopPoster>(work: &Work<P>, data: WorkData) -> Result<(), PoolError> { work.queue(data) }
