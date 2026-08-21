//! Pure-Rust implementation of the `vim.uv` engine: handle model mirroring
//! luv's documented API (timers, fs thread pool, process/PTY, tcp/pipe/tty/udp,
//! dns, signals, work queue), with handles registered as mio sources so
//! callbacks enter the editor through ox-loop's MultiQueue.

#![forbid(unsafe_code)]
