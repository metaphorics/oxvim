//! Blocking worker pool used by filesystem, DNS, and user work requests.

use std::collections::VecDeque;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use mio::Waker;

use crate::UvLoop;

const DEFAULT_POOL_SIZE: usize = 4;
const MAX_POOL_SIZE: usize = 1024;

/// A completion that must be executed by the owning [`UvLoop`] while pumping.
pub type LoopCompletion = Box<dyn FnOnce(&mut UvLoop) + Send + 'static>;

/// Integration seam for posting worker completions to a loop's pending queue.
///
/// The implementation must wake the loop and invoke each completion from its
/// pending phase; it must never execute the completion in `post` itself.
pub trait LoopPoster: Clone + Send + Sync + 'static {
    /// Marks one asynchronous request or watcher active for loop liveness.
    fn begin(&self) -> Result<(), PostError>;

    /// Marks a previously begun request or watcher inactive.
    fn end(&self);

    /// Queues one completion for delivery by the owning loop. See
    /// `luv-thread-pool-work-scheduling` in `runtime/doc/luvref.txt`.
    fn post(&self, completion: LoopCompletion) -> Result<(), PostError>;
}

/// Failure to enqueue a loop completion.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("event loop no longer accepts completions")]
pub struct PostError;

/// Failure to submit or execute pooled work.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PoolError {
    /// The pool has begun shutting down.
    #[error("thread pool is shutting down")]
    Shutdown,
    /// The work function unwound; the worker remained alive.
    #[error("thread pool work panicked")]
    WorkPanicked,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

enum Message {
    Run(Job),
    Shutdown,
}

struct Inner {
    sender: Mutex<Option<mpsc::Sender<Message>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    size: usize,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let sender = self.sender.get_mut().ok().and_then(Option::take);
        if let Some(sender) = sender {
            for _ in 0..self.size {
                let _ = sender.send(Message::Shutdown);
            }
        }
        if let Ok(workers) = self.workers.get_mut() {
            for worker in workers.drain(..) {
                let _ = worker.join();
            }
        }
    }
}

/// Shared libuv-style worker pool.
///
/// The default size is four. `UV_THREADPOOL_SIZE` overrides it when it is a
/// positive integer; values above 1024 are clamped to libuv's current maximum.
/// See `luv-thread-pool-work-scheduling` in `runtime/doc/luvref.txt`.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<Inner>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Pool")
            .field("size", &self.inner.size)
            .finish_non_exhaustive()
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

impl Pool {
    /// Creates a pool using `UV_THREADPOOL_SIZE`, or four workers by default.
    ///
    /// See `luv-thread-pool-work-scheduling` in `runtime/doc/luvref.txt`.
    pub fn new() -> Self {
        let size = std::env::var("UV_THREADPOOL_SIZE")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|size| *size > 0)
            .unwrap_or(DEFAULT_POOL_SIZE)
            .min(MAX_POOL_SIZE);
        Self::with_size(size)
    }

    /// Creates a pool with an explicit positive worker count.
    ///
    /// Zero is normalized to one so accepted work always makes progress. See
    /// `luv-thread-pool-work-scheduling` in `runtime/doc/luvref.txt`.
    pub fn with_size(size: usize) -> Self {
        let size = size.max(1).min(MAX_POOL_SIZE);
        let (sender, receiver) = mpsc::channel::<Message>();
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(size);
        for index in 0..size {
            let receiver = Arc::clone(&receiver);
            let builder = thread::Builder::new().name(format!("ox-uv-worker-{index}"));
            if let Ok(worker) = builder.spawn(move || worker_main(&receiver)) {
                workers.push(worker);
            }
        }
        let actual_size = workers.len();
        Self {
            inner: Arc::new(Inner {
                sender: Mutex::new((actual_size > 0).then_some(sender)),
                workers: Mutex::new(workers),
                size: actual_size,
            }),
        }
    }

    /// Returns the number of workers successfully started.
    ///
    /// See `luv-thread-pool-work-scheduling` in `runtime/doc/luvref.txt`.
    pub fn size(&self) -> usize {
        self.inner.size
    }

    /// Runs blocking work on the pool and posts its completion to `poster`.
    ///
    /// Completion is always deferred to the loop, including panic reporting.
    /// See `luv-thread-pool-work-scheduling` in `runtime/doc/luvref.txt`.
    pub fn submit<P, W, C, T>(&self, poster: P, work: W, complete: C) -> Result<(), PoolError>
    where
        P: LoopPoster,
        W: FnOnce() -> T + Send + 'static,
        C: FnOnce(&mut UvLoop, Result<T, PoolError>) + Send + 'static,
        T: Send + 'static,
    {
        poster.begin().map_err(|_| PoolError::Shutdown)?;
        let finish = poster.clone();
        let finish_for_job = finish.clone();
        let job = Box::new(move || {
            let result = catch_unwind(AssertUnwindSafe(work)).map_err(|_| PoolError::WorkPanicked);
            let finish_in_callback = finish_for_job.clone();
            if poster
                .post(Box::new(move |uv_loop| {
                    let _guard = RequestGuard(finish_in_callback);
                    complete(uv_loop, result);
                }))
                .is_err()
            {
                finish_for_job.end();
            }
        });
        let sender = self
            .inner
            .sender
            .lock()
            .map_err(|_| {
                finish.end();
                PoolError::Shutdown
            })?;
        let send_result = sender
            .as_ref()
            .ok_or(PoolError::Shutdown)
            .and_then(|sender| sender.send(Message::Run(job)).map_err(|_| PoolError::Shutdown));
        if send_result.is_err() {
            finish.end();
        }
        send_result
    }
}

struct CompletionQueue {
    accepting: AtomicBool,
    outstanding: AtomicUsize,
    pending: Mutex<VecDeque<LoopCompletion>>,
    waker: Arc<Waker>,
}

/// Cloneable thread-safe completion ingress owned by one [`UvLoop`].
#[derive(Clone)]
pub struct UvLoopPoster {
    inner: Arc<CompletionQueue>,
}

impl UvLoopPoster {
    pub(crate) fn new(waker: Arc<Waker>) -> Self {
        Self {
            inner: Arc::new(CompletionQueue {
                accepting: AtomicBool::new(true),
                outstanding: AtomicUsize::new(0),
                pending: Mutex::new(VecDeque::new()),
                waker,
            }),
        }
    }

    pub(crate) fn pop(&self) -> Option<LoopCompletion> {
        self.inner.pending.lock().ok()?.pop_front()
    }

    pub(crate) fn has_outstanding(&self) -> bool {
        self.inner.outstanding.load(Ordering::Acquire) != 0
    }

    pub(crate) fn close(&self) {
        self.inner.accepting.store(false, Ordering::Release);
    }
}

impl LoopPoster for UvLoopPoster {
    fn begin(&self) -> Result<(), PostError> {
        if !self.inner.accepting.load(Ordering::Acquire) {
            return Err(PostError);
        }
        self.inner.outstanding.fetch_add(1, Ordering::AcqRel);
        if self.inner.accepting.load(Ordering::Acquire) {
            Ok(())
        } else {
            self.end();
            Err(PostError)
        }
    }

    fn end(&self) {
        let _ = self.inner.outstanding.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |count| count.checked_sub(1),
        );
        let _ = self.inner.waker.wake();
    }

    fn post(&self, completion: LoopCompletion) -> Result<(), PostError> {
        if !self.inner.accepting.load(Ordering::Acquire) {
            return Err(PostError);
        }
        self.inner.pending.lock().map_err(|_| PostError)?.push_back(completion);
        self.inner.waker.wake().map_err(|_| PostError)
    }
}

struct RequestGuard<P: LoopPoster>(P);

impl<P: LoopPoster> Drop for RequestGuard<P> {
    fn drop(&mut self) {
        self.0.end();
    }
}

fn worker_main(receiver: &Mutex<mpsc::Receiver<Message>>) {
    loop {
        let message = match receiver.lock() {
            Ok(receiver) => receiver.recv(),
            Err(_) => return,
        };
        match message {
            Ok(Message::Run(job)) => {
                let _ = catch_unwind(AssertUnwindSafe(job));
            }
            Ok(Message::Shutdown) | Err(_) => return,
        }
    }
}
